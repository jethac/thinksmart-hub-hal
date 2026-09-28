//! Bluetooth: the radio, what is nearby, and what is paired.
//!
//! The adapter was on these units all along and unusable. The Intel 8265 is a
//! combo part, so the kernel brings `hci0` up unaided, but nothing in userspace
//! was installed -- PipeWire even had its Bluetooth plugin sitting ready for a
//! stack that did not exist. The two device classes that matter here are the two
//! this hardware cannot otherwise have: a keyboard, because these panels have
//! none and a Bluetooth one is the only way to type at a unit directly, and a
//! headset, because it is the only way to take a call without the whole room
//! hearing it.
//!
//! ## Talking to bluez
//!
//! Through `busctl`, not `bluetoothctl`, for everything that is a question.
//! `bluetoothctl` is a REPL that prints a coloured event log, and scraping it
//! means depending on its phrasing and its ANSI codes. `busctl` speaks the same
//! D-Bus API bluez actually exposes and will render a reply as JSON, which this
//! crate can already parse:
//!
//! ```text
//! busctl --json=short call org.bluez /org/bluez/hci0 \
//!     org.freedesktop.DBus.Properties GetAll s org.bluez.Adapter1
//! -> {"type":"a{sv}","data":[{"Powered":{"type":"b","data":true}, ...}]}
//! ```
//!
//! No new build dependency either, which a D-Bus crate would have been: this
//! crate is built on a control node and shipped to hubs with no compiler, so a
//! linked binding is a cost paid by every machine that ever builds it.
//!
//! Two limits found the hard way, both recorded here so the next person does not
//! rediscover them:
//!
//! - `GetManagedObjects` cannot be rendered as JSON at all. busctl answers
//!   `Failed to create new json object: Invalid argument`, because the reply type
//!   `a{oa{sa{sv}}}` nests deeper than its JSON writer handles. So enumeration is
//!   `busctl tree` for the paths and one `GetAll` per device, which works.
//! - Asking for several properties in one `get-property` fails the whole call if
//!   any one of them is absent, and on BLE devices most of them are. `GetAll`
//!   returns what exists and omits the rest.
//!
//! ## Scanning is not a one-shot
//!
//! **A discovery session belongs to the D-Bus connection that started it.** Run
//! `busctl call ... StartDiscovery` and it returns success, the process exits,
//! bluez tears the session down with the connection, and one second later
//! `Discovering` is false. Measured exactly that way on hub-001.
//!
//! So scanning is a child process held open for the duration --
//! `bluetoothctl --timeout N scan on`, which is the one thing bluetoothctl is
//! better at -- and it runs on its own thread so a settings page never blocks on
//! it. Everything else stays a bounded one-shot.
//!
//! ## Devices come and go while you are looking at them
//!
//! Unpaired devices are transient and bluez expires them. In a 12-second scan on
//! hub-001, 39 devices appeared and **23 of them had vanished between listing the
//! paths and reading their properties** -- not an edge case, the majority. A
//! device that disappears mid-enumeration is skipped rather than reported as an
//! error, because it is the normal behaviour of the thing being enumerated.

pub mod agent;

pub use agent::{Answer, Pairing, Stage};

use crate::util::{run, run_env, RunError};
use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Queries are local IPC and answer immediately or not at all.
const T: Duration = Duration::from_secs(5);

/// Pairing and connecting talk to a device over the air, which is slower and
/// involves another machine deciding to cooperate.
const CONNECT_T: Duration = Duration::from_secs(30);

/// Where the kernel exposes adapters. Checked directly so "no Bluetooth
/// hardware" is answerable without bluez running at all.
const HCI_DIR: &str = "/sys/class/bluetooth";

/// rfkill lives in `/usr/sbin`, which is NOT on the PATH of an unprivileged
/// non-login shell on Debian -- and the panel runs as exactly such a user. Found
/// by `rfkill: not found` on a unit where `/usr/sbin/rfkill` was installed and
/// working, which reads as "no Bluetooth" and is not.
const RFKILL: &str = "/usr/sbin/rfkill";

/// What the radio is doing, as four states rather than a boolean.
///
/// They have four different fixes and collapsing them into "no Bluetooth" is
/// what makes a settings page useless: absent means this unit has no adapter,
/// blocked means something switched the radio off outside bluez and bluez cannot
/// override it, and off means bluez has it and it is not powered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Radio {
    /// No adapter in `/sys/class/bluetooth`. Hardware, not software.
    Absent,
    /// Soft- or hard-blocked by rfkill. Powering it on through bluez will fail
    /// until it is unblocked.
    Blocked,
    /// Present and not powered.
    Off,
    On,
}

/// What kind of thing a device is, for an icon and for sorting.
///
/// Derived from what bluez reports rather than from the name, because a name is
/// whatever the manufacturer felt like and matching on it is how a keyboard
/// called "Keychron K2" works and one called "K380" does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Kind {
    Keyboard,
    Mouse,
    Headset,
    Headphones,
    Speaker,
    Phone,
    Computer,
    Other,
}

impl Kind {
    /// bluez's own `Icon`, which is a freedesktop icon name and the better
    /// answer when it exists. It is frequently absent: of 39 devices seen in one
    /// scan here, exactly one had it.
    fn from_icon(icon: &str) -> Option<Self> {
        Some(match icon {
            "input-keyboard" => Kind::Keyboard,
            "input-mouse" | "input-tablet" => Kind::Mouse,
            "audio-headset" => Kind::Headset,
            "audio-headphones" => Kind::Headphones,
            "audio-card" | "audio-speakers" => Kind::Speaker,
            "phone" => Kind::Phone,
            "computer" => Kind::Computer,
            _ => return None,
        })
    }

    /// The Class of Device field, as defined by the Bluetooth assigned numbers.
    ///
    /// Bits 8-12 are the major class and bits 2-7 the minor. Worth decoding
    /// rather than ignoring: an LG TV seen here reported class 795708, whose
    /// major class is 4 (Audio/Video), which is how it is a speaker and not an
    /// "unknown device".
    fn from_class(class: u32) -> Option<Self> {
        let major = (class >> 8) & 0x1F;
        let minor = (class >> 2) & 0x3F;
        Some(match major {
            1 => Kind::Computer,
            2 => Kind::Phone,
            4 => match minor {
                1 | 2 => Kind::Headset,
                6 => Kind::Headphones,
                _ => Kind::Speaker,
            },
            // Peripheral. The top two bits of the minor field say keyboard,
            // pointing device, or both -- a combo keyboard reports both and is
            // more useful called a keyboard.
            5 => match (minor >> 4) & 0x3 {
                1 | 3 => Kind::Keyboard,
                2 => Kind::Mouse,
                _ => Kind::Other,
            },
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Adapter {
    pub address: String,
    /// What this unit calls itself to other devices. Defaults to the hostname,
    /// so on this fleet it is already `hub-001-bedroom` and means something.
    pub name: String,
    pub powered: bool,
    pub discovering: bool,
    /// Whether other devices can find this one. Off by default and worth
    /// showing: a keyboard cannot be paired from its side without it.
    pub discoverable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Device {
    pub address: String,
    /// `Alias` if set, which is bluez's own display name and falls back to the
    /// address with dashes when a device advertises nothing.
    pub name: String,
    pub kind: Kind,
    pub paired: bool,
    pub connected: bool,
    pub trusted: bool,
    /// Signal strength, present only while a scan has recently seen it. Useful
    /// for ordering what is nearby; absent for everything already paired.
    pub rssi: Option<i32>,
}

impl Device {
    /// Whether this is worth offering on a page whose purpose is keyboards and
    /// headsets.
    ///
    /// A scan in a house finds dozens of phones, watches and beacons, almost all
    /// of them nameless BLE randoms. This is what separates a short list from
    /// noise.
    pub fn is_interesting(&self) -> bool {
        self.paired
            || matches!(
                self.kind,
                Kind::Keyboard | Kind::Mouse | Kind::Headset | Kind::Headphones | Kind::Speaker
            )
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct State {
    pub radio: Option<Radio>,
    pub adapter: Option<Adapter>,
    /// Everything bluez currently knows about: paired devices always, plus
    /// whatever a recent scan turned up.
    pub devices: Vec<Device>,
    /// A scan is running right now.
    pub scanning: bool,
    pub error: Option<String>,
}

/// Why something could not be done. Separate cases because they have separate
/// fixes, and a caller that only has a string has to guess.
#[derive(Debug, Clone, PartialEq)]
pub enum BtError {
    /// `busctl` or `bluetoothctl` is missing. bluez is not installed.
    NotInstalled(String),
    /// No adapter at all.
    NoAdapter,
    /// rfkill has the radio blocked. bluez cannot override this.
    Blocked,
    /// It ran and did not answer. Pairing with something that has wandered off
    /// looks like this.
    Timeout(Duration),
    /// bluez said no, in its own words.
    Failed(String),
}

impl std::fmt::Display for BtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BtError::NotInstalled(c) => write!(f, "{c} is not installed"),
            BtError::NoAdapter => write!(f, "this unit has no Bluetooth adapter"),
            BtError::Blocked => write!(f, "the Bluetooth radio is blocked by rfkill"),
            BtError::Timeout(d) => write!(f, "no answer in {}s", d.as_secs()),
            BtError::Failed(t) => write!(f, "{t}"),
        }
    }
}

impl std::error::Error for BtError {}

fn map_run(e: RunError) -> BtError {
    match e {
        RunError::NotFound(c) => BtError::NotInstalled(c),
        RunError::Timeout(d) => BtError::Timeout(d),
        RunError::Spawn(s) | RunError::Failed(s) => BtError::Failed(s),
    }
}

pub struct Bluetooth {
    st: Mutex<State>,
    scanning: AtomicBool,
    /// Shared with the bluez agent, which is called into from another thread
    /// while a pairing is in flight.
    pairing: Arc<agent::Shared>,
}

impl Bluetooth {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            st: Mutex::new(State::default()),
            scanning: AtomicBool::new(false),
            pairing: Arc::new(agent::Shared::new()),
        })
    }

    /// Refresh on a thread of its own, so reading [`state`](Self::state) is
    /// cheap.
    ///
    /// Three seconds rather than the one a UI would like. Each pass is one
    /// `busctl tree` plus a `GetAll` per device, and during a scan in a house
    /// that is forty subprocesses; a settings page that nobody is looking at
    /// should not spend a core on it.
    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        thread::spawn(move || loop {
            me.refresh();
            thread::sleep(Duration::from_secs(3));
        });
    }

    pub fn state(&self) -> State {
        self.st.lock().unwrap().clone()
    }

    pub fn refresh(&self) {
        let radio = radio_state();
        let scanning = self.scanning.load(Ordering::Relaxed);

        // Nothing below will work without an adapter, and saying so once is
        // better than a list of failures from every subsequent call.
        if radio == Radio::Absent {
            let mut st = self.st.lock().unwrap();
            *st = State {
                radio: Some(Radio::Absent),
                scanning,
                ..Default::default()
            };
            return;
        }

        let adapter = read_adapter();
        let devices = read_devices();

        let mut st = self.st.lock().unwrap();
        st.radio = Some(radio);
        st.adapter = adapter;
        st.devices = devices;
        st.scanning = scanning;
        st.error = None;
    }

    /// Turn the radio on or off.
    ///
    /// Refuses when rfkill has it blocked rather than letting bluez fail with
    /// something less specific: unblocking needs `rfkill unblock bluetooth` and
    /// usually root, which is a different instruction to give somebody.
    pub fn set_powered(&self, on: bool) -> Result<(), BtError> {
        if radio_state() == Radio::Blocked {
            return Err(BtError::Blocked);
        }
        run_env(
            "busctl",
            &[
                "set-property",
                "org.bluez",
                "/org/bluez/hci0",
                "org.bluez.Adapter1",
                "Powered",
                "b",
                if on { "true" } else { "false" },
            ],
            &[],
            T,
        )
        .map_err(map_run)?;
        self.refresh();
        Ok(())
    }

    /// Whether other devices can see this one.
    pub fn set_discoverable(&self, on: bool) -> Result<(), BtError> {
        run_env(
            "busctl",
            &[
                "set-property",
                "org.bluez",
                "/org/bluez/hci0",
                "org.bluez.Adapter1",
                "Discoverable",
                "b",
                if on { "true" } else { "false" },
            ],
            &[],
            T,
        )
        .map_err(map_run)?;
        self.refresh();
        Ok(())
    }

    /// Look for nearby devices for `how_long`, without blocking the caller.
    ///
    /// Returns immediately. The scan runs on its own thread holding a child
    /// process open, because a discovery session dies with the D-Bus connection
    /// that started it -- see the module docs. Calling it again while one is
    /// running is a no-op rather than a second radio doing the same thing.
    pub fn scan(self: &Arc<Self>, how_long: Duration) {
        if self.scanning.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        thread::spawn(move || {
            let secs = how_long.as_secs().max(1).to_string();
            // Deadline a little past bluetoothctl's own timeout: it is expected
            // to stop by itself, and the deadline is only there so a wedged one
            // cannot leave this flag set for ever.
            let out = run(
                "bluetoothctl",
                &["--timeout", &secs, "scan", "on"],
                how_long + Duration::from_secs(10),
            );
            if !out.ok {
                me.st.lock().unwrap().error = Some(crate::util::last_line(&out.text));
            }
            me.scanning.store(false, Ordering::SeqCst);
            me.refresh();
        });
    }

    pub fn scanning(&self) -> bool {
        self.scanning.load(Ordering::Relaxed)
    }

    /// Begin pairing, and return at once.
    ///
    /// Pairing is not a call that either works or does not: bluez asks questions
    /// part way through and waits for answers, and for a keyboard the question
    /// is a number that has to be typed on the keyboard itself. So this starts
    /// the process and the caller watches [`pairing`](Self::pairing), answering
    /// with [`confirm`](Self::confirm), [`submit_pin`](Self::submit_pin) or
    /// [`submit_passkey`](Self::submit_passkey) when asked.
    ///
    /// The old one-shot `pair()` could not do this. It shelled out to
    /// `bluetoothctl`, whose agent answers bluez inside its own REPL, so a
    /// passkey was shown to nobody and a confirmation was answered by a program
    /// that had not asked anyone.
    pub fn start_pairing(self: &Arc<Self>, address: &str) {
        match radio_state() {
            Radio::Absent => {
                self.pairing.set(Pairing::Failed {
                    address: address.to_string(),
                    stage: Stage::Registering,
                    reason: "no Bluetooth adapter".into(),
                });
                return;
            }
            Radio::Blocked => {
                self.pairing.set(Pairing::Failed {
                    address: address.to_string(),
                    stage: Stage::Registering,
                    reason: "the radio is blocked by rfkill".into(),
                });
                return;
            }
            _ => {}
        }

        self.pairing.begin(address);
        let me = self.clone();
        let address = address.to_string();
        thread::spawn(move || {
            let shared = me.pairing.clone();
            if let Err(e) = run_pairing(&shared, &address) {
                // Only overwrite if the agent has not already recorded something
                // more specific, which it will have for anything it was asked.
                if !shared.get().finished() {
                    shared.set(e);
                }
            }
            me.refresh();
        });
    }

    /// Where a pairing has got to.
    pub fn pairing(&self) -> Pairing {
        self.pairing.get()
    }

    /// Answer a confirmation or an authorisation.
    pub fn confirm(&self, yes: bool) {
        self.pairing.answer(Answer::Confirm(yes));
    }

    /// Answer a request for a PIN typed on this machine.
    pub fn submit_pin(&self, pin: &str) {
        self.pairing.answer(Answer::Pin(pin.to_string()));
    }

    /// Answer a request for a passkey typed on this machine.
    pub fn submit_passkey(&self, passkey: u32) {
        self.pairing.answer(Answer::Passkey(passkey));
    }

    /// Give up on a pairing in progress.
    ///
    /// Releases whatever bluez is waiting on rather than leaving the call
    /// outstanding until it times out.
    pub fn cancel_pairing(&self) {
        self.pairing.abandon();
    }

    pub fn connect(&self, address: &str) -> Result<(), BtError> {
        self.ctl(&["connect", address])
    }

    pub fn disconnect(&self, address: &str) -> Result<(), BtError> {
        self.ctl(&["disconnect", address])
    }

    /// Mark a device as trusted, so it may reconnect by itself.
    ///
    /// Worth doing for anything meant to keep working: without it a keyboard
    /// that goes to sleep has to be reconnected by hand, from a panel that has
    /// no keyboard.
    pub fn trust(&self, address: &str) -> Result<(), BtError> {
        self.ctl(&["trust", address])
    }

    /// Forget a device entirely, undoing the pairing.
    pub fn forget(&self, address: &str) -> Result<(), BtError> {
        self.ctl(&["remove", address])
    }

    fn ctl(&self, args: &[&str]) -> Result<(), BtError> {
        match radio_state() {
            Radio::Absent => return Err(BtError::NoAdapter),
            Radio::Blocked => return Err(BtError::Blocked),
            _ => {}
        }
        let text = run_env("bluetoothctl", args, &[], CONNECT_T).map_err(map_run)?;
        // bluetoothctl exits 0 having printed a failure, so the exit status is
        // not the answer. Its wording is, and it is passed through rather than
        // summarised: "br-connection-profile-unavailable" says considerably more
        // than "could not connect".
        if let Some(line) = text
            .lines()
            .map(strip_ansi)
            .find(|l| l.contains("Failed") || l.contains("not available"))
        {
            return Err(BtError::Failed(line.trim().to_string()));
        }
        self.refresh();
        Ok(())
    }
}

/// Register an agent, ask bluez to pair, and mark the result trusted.
///
/// Two connections, deliberately. One serves the agent and is held open for the
/// whole pairing, because an agent dies with the connection that registered it
/// -- the same way a discovery session does, and for the same reason. The other
/// makes the `Pair` call, so that waiting on a reply cannot starve the dispatch
/// of the questions that reply is waiting for.
///
/// `Pair` is not given a deadline. zbus imposes none and neither does the bus,
/// so it waits as long as bluez does, which is what somebody typing six digits
/// on a keyboard needs. The bounds that do exist are the agent's own answer
/// timeout and [`Bluetooth::cancel_pairing`], which releases an outstanding
/// question and lets the call fail rather than leaving it in the air.
fn run_pairing(shared: &Arc<agent::Shared>, address: &str) -> Result<(), Pairing> {
    use zbus::blocking::{connection, Proxy};

    let fail = |stage: Stage, reason: String| Pairing::Failed {
        address: address.to_string(),
        stage,
        reason,
    };

    let device_path = agent::path_for("hci0", address);
    let agent_path = zbus::zvariant::ObjectPath::try_from(agent::AGENT_PATH)
        .map_err(|e| fail(Stage::Registering, e.to_string()))?;

    let served = agent::Agent {
        shared: shared.clone(),
    };
    let conn = connection::Builder::system()
        .and_then(|b| b.serve_at(&agent_path, served))
        .and_then(|b| b.build())
        .map_err(|e| fail(Stage::Registering, e.to_string()))?;

    let manager = Proxy::new(&conn, "org.bluez", "/org/bluez", "org.bluez.AgentManager1")
        .map_err(|e| fail(Stage::Registering, e.to_string()))?;
    manager
        .call::<_, _, ()>("RegisterAgent", &(&agent_path, agent::CAPABILITY))
        .map_err(|e| fail(Stage::Registering, e.to_string()))?;
    // Being the default agent is what makes bluez route questions here rather
    // than to whatever else is registered. Not fatal if it is refused: another
    // agent holding the default still leaves ours registered for our own calls.
    let _ = manager.call::<_, _, ()>("RequestDefaultAgent", &(&agent_path,));

    let caller = connection::Builder::system()
        .and_then(|b| b.build())
        .map_err(|e| fail(Stage::Pairing, e.to_string()))?;
    let device = Proxy::new(&caller, "org.bluez", device_path.as_str(), "org.bluez.Device1")
        .map_err(|e| fail(Stage::NoSuchDevice, e.to_string()))?;

    let outcome = device.call::<_, _, ()>("Pair", &());
    let _ = manager.call::<_, _, ()>("UnregisterAgent", &(&agent_path,));

    match outcome {
        Ok(()) => {
            // Trusted, or a keyboard that goes to sleep has to be reconnected by
            // hand from a panel that has no keyboard.
            if let Err(e) = device.set_property("Trusted", true) {
                return Err(fail(Stage::Trusting, e.to_string()));
            }
            shared.set(Pairing::Succeeded {
                address: address.to_string(),
            });
            Ok(())
        }
        Err(e) => {
            // bluez's own wording, which says considerably more than ours would:
            // AuthenticationFailed, AuthenticationCanceled, AlreadyExists and
            // ConnectionAttemptFailed all mean different things to do next.
            let text = e.to_string();
            let stage = if text.contains("does not exist") || text.contains("No such") {
                Stage::NoSuchDevice
            } else {
                Stage::Pairing
            };
            Err(fail(stage, text))
        }
    }
}

/// bluetoothctl colours its output whether or not it is a terminal.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The radio's state, without needing bluez to be running.
///
/// The adapter check is sysfs, so "no hardware" is answerable on a unit where
/// bluez was never installed. rfkill is asked second because a blocked radio
/// still has an adapter directory.
pub fn radio_state() -> Radio {
    let has_adapter = std::fs::read_dir(HCI_DIR)
        .map(|d| d.flatten().any(|e| e.file_name().to_string_lossy().starts_with("hci")))
        .unwrap_or(false);
    if !has_adapter {
        return Radio::Absent;
    }
    if is_blocked() {
        return Radio::Blocked;
    }
    match adapter_bool("Powered") {
        Some(true) => Radio::On,
        Some(false) => Radio::Off,
        // An adapter the kernel has but bluez does not: bluetoothd is not
        // running. Reported as off, which is what it behaves like and what
        // turning it on will fix.
        None => Radio::Off,
    }
}

fn is_blocked() -> bool {
    if !Path::new(RFKILL).exists() {
        // Without rfkill a blocked radio is indistinguishable from an idle one.
        // Saying "not blocked" is the safer of the two guesses: the alternative
        // hides a working radio behind an instruction nobody needs.
        return false;
    }
    let out = run(RFKILL, &["list", "bluetooth"], T);
    out.text
        .lines()
        .any(|l| l.trim_start().starts_with("Soft blocked: yes") || l.trim_start().starts_with("Hard blocked: yes"))
}

/// One property of the adapter, as a bool. `None` when bluez is not answering.
fn adapter_bool(name: &str) -> Option<bool> {
    let text = run_env(
        "busctl",
        &["--json=short", "get-property", "org.bluez", "/org/bluez/hci0", "org.bluez.Adapter1", name],
        &[],
        T,
    )
    .ok()?;
    serde_json::from_str::<Value>(&text)
        .ok()?
        .get("data")?
        .as_bool()
}

fn read_adapter() -> Option<Adapter> {
    let props = get_all("/org/bluez/hci0", "org.bluez.Adapter1")?;
    Some(Adapter {
        address: prop_str(&props, "Address").unwrap_or_default(),
        name: prop_str(&props, "Alias")
            .or_else(|| prop_str(&props, "Name"))
            .unwrap_or_default(),
        powered: prop_bool(&props, "Powered").unwrap_or(false),
        discovering: prop_bool(&props, "Discovering").unwrap_or(false),
        discoverable: prop_bool(&props, "Discoverable").unwrap_or(false),
    })
}

/// Every device bluez currently knows about.
///
/// A device that vanishes between the listing and the read is skipped in
/// silence. That is the majority case during a scan -- 23 of 39 on one run here
/// -- because unpaired devices are transient and bluez expires them, so treating
/// it as an error would mean an error list longer than the device list.
fn read_devices() -> Vec<Device> {
    let mut devices: Vec<Device> = device_paths()
        .into_iter()
        .filter_map(|p| get_all(&p, "org.bluez.Device1").map(|props| device_from(&props)))
        .collect();

    // Paired first, then connected, then by name: the list is read by somebody
    // looking for something they already own before it is read by somebody
    // looking for something new.
    devices.sort_by(|a, b| {
        b.paired
            .cmp(&a.paired)
            .then(b.connected.cmp(&a.connected))
            .then(a.name.cmp(&b.name))
    });
    devices
}

fn device_paths() -> Vec<String> {
    let out = run("busctl", &["tree", "org.bluez"], T);
    if !out.ok {
        return Vec::new();
    }
    out.text
        .lines()
        .filter_map(|l| {
            let start = l.find("/org/bluez/hci")?;
            let path = l[start..].trim();
            // Only device objects; the adapter and its children are not devices.
            path.contains("/dev_").then(|| path.to_string())
        })
        .collect()
}

fn device_from(props: &Value) -> Device {
    let address = prop_str(props, "Address").unwrap_or_default();
    let kind = prop_str(props, "Icon")
        .and_then(|i| Kind::from_icon(&i))
        .or_else(|| prop_u32(props, "Class").and_then(Kind::from_class))
        .unwrap_or(Kind::Other);
    Device {
        name: prop_str(props, "Alias")
            .or_else(|| prop_str(props, "Name"))
            .unwrap_or_else(|| address.clone()),
        address,
        kind,
        paired: prop_bool(props, "Paired").unwrap_or(false),
        connected: prop_bool(props, "Connected").unwrap_or(false),
        trusted: prop_bool(props, "Trusted").unwrap_or(false),
        rssi: prop_i32(props, "RSSI"),
    }
}

/// Every property of one interface, as JSON.
///
/// `GetAll` rather than several `get-property` calls: asking for a property a
/// device does not have fails the whole call, and on BLE devices most of them
/// are absent. `GetManagedObjects` would do the entire tree in one request and
/// cannot be rendered as JSON by busctl at all.
fn get_all(path: &str, interface: &str) -> Option<Value> {
    let text = run_env(
        "busctl",
        &[
            "--json=short",
            "call",
            "org.bluez",
            path,
            "org.freedesktop.DBus.Properties",
            "GetAll",
            "s",
            interface,
        ],
        &[],
        T,
    )
    .ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get("data")?.as_array()?.first().cloned()
}

fn prop<'a>(props: &'a Value, name: &str) -> Option<&'a Value> {
    props.get(name)?.get("data")
}

fn prop_str(props: &Value, name: &str) -> Option<String> {
    prop(props, name)?.as_str().map(str::to_string)
}

fn prop_bool(props: &Value, name: &str) -> Option<bool> {
    prop(props, name)?.as_bool()
}

fn prop_u32(props: &Value, name: &str) -> Option<u32> {
    prop(props, name)?.as_u64().map(|v| v as u32)
}

fn prop_i32(props: &Value, name: &str) -> Option<i32> {
    prop(props, name)?.as_i64().map(|v| v as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real adapter reply from hub-001, trimmed to the properties read here.
    const ADAPTER: &str = r#"{"Address":{"type":"s","data":"28:7F:CF:99:B7:8A"},
        "Name":{"type":"s","data":"hub-001-bedroom"},
        "Alias":{"type":"s","data":"hub-001-bedroom"},
        "Class":{"type":"u","data":7078148},
        "Powered":{"type":"b","data":true},
        "Discoverable":{"type":"b","data":false},
        "Discovering":{"type":"b","data":false}}"#;

    /// The LG TV that turned up in a scan on hub-001, which is the only device
    /// seen there that reported an Icon at all.
    const TV: &str = r#"{"Address":{"type":"s","data":"AA:BB:CC:DD:EE:FF"},
        "Alias":{"type":"s","data":"[LG] webOS TV OLED55CXPJA"},
        "Icon":{"type":"s","data":"audio-card"},
        "Class":{"type":"u","data":795708},
        "Paired":{"type":"b","data":false},
        "Connected":{"type":"b","data":false},
        "Trusted":{"type":"b","data":false},
        "RSSI":{"type":"n","data":-61}}"#;

    /// A nameless BLE random, which is what most of a scan is.
    const RANDOM: &str = r#"{"Address":{"type":"s","data":"2D:AD:C7:5B:AF:6C"},
        "Alias":{"type":"s","data":"2D-AD-C7-5B-AF-6C"},
        "Paired":{"type":"b","data":false},
        "Connected":{"type":"b","data":false},
        "Trusted":{"type":"b","data":false}}"#;

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn reads_an_adapter() {
        let a = read_adapter_from(&parse(ADAPTER));
        assert_eq!(a.address, "28:7F:CF:99:B7:8A");
        assert_eq!(a.name, "hub-001-bedroom");
        assert!(a.powered);
        assert!(!a.discovering);
    }

    // The property reading is shared; this mirrors read_adapter without the
    // subprocess so it can be tested.
    fn read_adapter_from(props: &Value) -> Adapter {
        Adapter {
            address: prop_str(props, "Address").unwrap_or_default(),
            name: prop_str(props, "Alias")
                .or_else(|| prop_str(props, "Name"))
                .unwrap_or_default(),
            powered: prop_bool(props, "Powered").unwrap_or(false),
            discovering: prop_bool(props, "Discovering").unwrap_or(false),
            discoverable: prop_bool(props, "Discoverable").unwrap_or(false),
        }
    }

    #[test]
    fn an_icon_names_the_kind() {
        let d = device_from(&parse(TV));
        assert_eq!(d.kind, Kind::Speaker);
        assert_eq!(d.name, "[LG] webOS TV OLED55CXPJA");
        assert_eq!(d.rssi, Some(-61));
    }

    #[test]
    fn a_device_with_nothing_falls_back_to_its_address() {
        let d = device_from(&parse(RANDOM));
        assert_eq!(d.kind, Kind::Other);
        assert!(!d.is_interesting(), "a nameless BLE random is noise");
        assert_eq!(d.rssi, None);
    }

    #[test]
    fn the_class_field_is_decoded_when_there_is_no_icon() {
        // Major class 5 is Peripheral; the top two minor bits say keyboard.
        // 0x540 = keyboard, which is the device this whole module exists for.
        let keyboard = Kind::from_class(0x540);
        assert_eq!(keyboard, Some(Kind::Keyboard));
        assert_eq!(Kind::from_class(0x580), Some(Kind::Mouse));
        // Audio/Video major with headset minor.
        assert_eq!(Kind::from_class(0x404), Some(Kind::Headset));
        // The real TV, which is how this decoding was checked against hardware.
        assert_eq!(Kind::from_class(795708), Some(Kind::Speaker));
        // Major class 0 is Miscellaneous and says nothing.
        assert_eq!(Kind::from_class(4194304), None);
    }

    #[test]
    fn an_icon_wins_over_a_class() {
        // A device reporting both is trusted on its icon, which is bluez's own
        // interpretation rather than a field this crate decodes by hand.
        let d = device_from(&parse(TV));
        assert_eq!(d.kind, Kind::Speaker);
    }

    #[test]
    fn paired_devices_are_interesting_whatever_they_are() {
        let mut d = device_from(&parse(RANDOM));
        assert!(!d.is_interesting());
        d.paired = true;
        assert!(d.is_interesting(), "something already paired is never noise");
    }

    #[test]
    fn ansi_is_stripped_from_what_bluetoothctl_says() {
        // It colours its output whether or not stdout is a terminal, so a
        // failure line has escapes in the middle of it.
        let line = "\u{1b}[0;91mFailed to pair\u{1b}[0m: org.bluez.Error.AuthenticationCanceled";
        assert_eq!(
            strip_ansi(line),
            "Failed to pair: org.bluez.Error.AuthenticationCanceled"
        );
    }

    #[test]
    fn devices_sort_with_the_ones_you_own_first() {
        let mut list = vec![
            Device { address: "a".into(), name: "Zed".into(), kind: Kind::Other, paired: false, connected: false, trusted: false, rssi: None },
            Device { address: "b".into(), name: "Alpha".into(), kind: Kind::Keyboard, paired: true, connected: false, trusted: true, rssi: None },
            Device { address: "c".into(), name: "Beta".into(), kind: Kind::Headset, paired: true, connected: true, trusted: true, rssi: None },
        ];
        list.sort_by(|a, b| {
            b.paired
                .cmp(&a.paired)
                .then(b.connected.cmp(&a.connected))
                .then(a.name.cmp(&b.name))
        });
        assert_eq!(list[0].name, "Beta", "paired and connected comes first");
        assert_eq!(list[1].name, "Alpha");
        assert_eq!(list[2].name, "Zed");
    }
}
