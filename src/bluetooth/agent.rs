//! The bluez pairing agent.
//!
//! Pairing is not one action. bluez asks questions part way through and will not
//! proceed until something answers them, and for the device class this hardware
//! most needs -- a keyboard, because these panels have none -- the question is
//! "show this number to the person, they will type it on the keyboard". Nothing
//! can answer that except a human looking at the panel, so the answer has to
//! travel from bluez, through this crate, onto a screen, and back.
//!
//! ## Why this speaks D-Bus directly
//!
//! The rest of this module talks to bluez through `busctl`, deliberately, and
//! that stops working here. An agent is not a caller; it is a **served object**
//! that bluez makes method calls *to*. `busctl` is a client and cannot serve
//! anything, so the only way to keep it would be to drive `bluetoothctl` as a
//! child and scrape the prompts out of its coloured event log -- which also
//! cannot surface the `entered` counter, and cannot answer a confirmation from
//! what a person has tapped.
//!
//! So this uses `zbus`. The earlier objection to a D-Bus crate was that a linked
//! binding is a cost paid by every machine that builds this crate, which is
//! built on a control node and shipped to hubs with no compiler. `zbus` is pure
//! Rust and speaks the wire protocol over the socket itself: no C library, no
//! development package, nothing new needed to build. Eighteen seconds of compile
//! time, measured.
//!
//! ## What bluez actually asks
//!
//! Which question arrives is decided by Secure Simple Pairing from the two
//! sides' declared input and output capability. This agent registers as
//! `KeyboardDisplay`, the most capable, so that pairing a `KeyboardOnly` device
//! -- which is what a keyboard is -- lands on passkey entry with **this** end
//! displaying and the keyboard typing.
//!
//! | Callback | Who acts | Blocks |
//! |---|---|---|
//! | `DisplayPasskey` | person types it on the device being paired | no |
//! | `DisplayPinCode` | same, for older devices | no |
//! | `RequestConfirmation` | person confirms the numbers match | yes |
//! | `RequestPasskey` / `RequestPinCode` | person types it **here** | yes |
//! | `AuthorizeService` | person allows a profile | yes |
//! | `RequestAuthorization` | person allows the pairing at all | yes |
//!
//! The keyboard case is the one that does **not** block: bluez hands over the
//! passkey and carries on, and the pairing completes when the right digits are
//! typed on the keyboard. `entered` then counts up as each key is pressed, and
//! it is the only feedback that the person is typing on the right keyboard
//! rather than into a void.
//!
//! The blocking ones wait on an answer from the UI with a hard ceiling, so a
//! panel that nobody comes back to cannot leave a bluez call outstanding for
//! ever. While one is outstanding this connection is not dispatching, which is
//! acceptable for a subsystem that pairs one device at a time, and is why the
//! ceiling exists at all.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde::Serialize;

/// Where this agent is served. Namespaced so it cannot collide with another
/// agent on the same bus.
pub const AGENT_PATH: &str = "/net/straylight/hubhal/agent";

/// What to tell bluez this end can do.
///
/// `KeyboardDisplay` is the most capable and is what makes a keyboard pair the
/// way a keyboard should: against a `KeyboardOnly` peer, Secure Simple Pairing
/// selects passkey entry with this end displaying.
pub const CAPABILITY: &str = "KeyboardDisplay";

/// How long a question may wait for a person before it is given up on.
///
/// Long enough to walk to the panel, short enough that a bluez call is never
/// left outstanding indefinitely by somebody who wandered off.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(90);

/// Where a pairing got to, and what it is waiting for.
///
/// This is the whole point of the module: a settings page can render each of
/// these and, where a person has to act, collect the answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Pairing {
    /// Nothing in progress.
    Idle,
    /// Asked bluez to pair; it has not said anything yet.
    Starting { address: String },
    /// **The keyboard case.** Show this and have the person type it on the
    /// device being paired, then Enter. `entered` counts the digits bluez has
    /// seen so far.
    DisplayPasskey {
        address: String,
        passkey: u32,
        entered: u32,
    },
    /// Older devices: show this and have the person type it on the device.
    DisplayPinCode { address: String, pin: String },
    /// Both ends show the same number; the person says whether they match.
    /// Waiting on [`Answer::Confirm`].
    Confirm { address: String, passkey: u32 },
    /// The device wants a PIN typed **here**. Waiting on [`Answer::Pin`].
    RequestPin { address: String },
    /// The device wants a passkey typed **here**. Waiting on
    /// [`Answer::Passkey`].
    RequestPasskey { address: String },
    /// The device wants permission to pair at all. Waiting on
    /// [`Answer::Confirm`].
    Authorize { address: String },
    /// A profile wants permission. Waiting on [`Answer::Confirm`].
    AuthorizeService { address: String, uuid: String },
    Succeeded { address: String },
    Failed {
        address: String,
        stage: Stage,
        reason: String,
    },
}

impl Pairing {
    /// Whether a person has to do something before this can go any further.
    pub fn needs_a_person(&self) -> bool {
        matches!(
            self,
            Pairing::Confirm { .. }
                | Pairing::RequestPin { .. }
                | Pairing::RequestPasskey { .. }
                | Pairing::Authorize { .. }
                | Pairing::AuthorizeService { .. }
        )
    }

    /// Whether this is over, one way or the other.
    pub fn finished(&self) -> bool {
        matches!(self, Pairing::Succeeded { .. } | Pairing::Failed { .. })
    }

    pub fn address(&self) -> Option<&str> {
        match self {
            Pairing::Idle => None,
            Pairing::Starting { address }
            | Pairing::DisplayPasskey { address, .. }
            | Pairing::DisplayPinCode { address, .. }
            | Pairing::Confirm { address, .. }
            | Pairing::RequestPin { address }
            | Pairing::RequestPasskey { address }
            | Pairing::Authorize { address }
            | Pairing::AuthorizeService { address, .. }
            | Pairing::Succeeded { address }
            | Pairing::Failed { address, .. } => Some(address),
        }
    }
}

/// Which part of pairing failed.
///
/// Worth distinguishing because they need different things done about them: a
/// device that will not register an agent is a broken installation, a device
/// that never answers is out of range or asleep, and a refusal is the other end
/// saying no.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Stage {
    /// Could not register with bluez as an agent at all.
    Registering,
    /// The device is not known to bluez, so there is nothing to pair with.
    NoSuchDevice,
    /// bluez refused or abandoned the pairing.
    Pairing,
    /// A question was asked and nobody answered it in time.
    Unanswered,
    /// Paired, but marking it trusted afterwards failed.
    Trusting,
}

/// What a person said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Yes or no, for confirmations and authorisations.
    Confirm(bool),
    Pin(String),
    Passkey(u32),
}

/// State shared between the agent bluez calls into and whatever is watching.
pub struct Shared {
    state: Mutex<Pairing>,
    answer: Mutex<Option<Answer>>,
    /// Signalled when an answer arrives or the pairing is abandoned.
    bell: Condvar,
    /// Raised to make an outstanding question give up immediately.
    abandoned: Mutex<bool>,
}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

impl Shared {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(Pairing::Idle),
            answer: Mutex::new(None),
            bell: Condvar::new(),
            abandoned: Mutex::new(false),
        }
    }

    pub fn get(&self) -> Pairing {
        self.state.lock().unwrap().clone()
    }

    pub fn set(&self, p: Pairing) {
        *self.state.lock().unwrap() = p;
    }

    /// Start a fresh pairing, discarding anything left over from the last one.
    pub fn begin(&self, address: &str) {
        *self.abandoned.lock().unwrap() = false;
        *self.answer.lock().unwrap() = None;
        self.set(Pairing::Starting {
            address: address.to_string(),
        });
    }

    /// Provide what a person said, and wake whatever is waiting for it.
    pub fn answer(&self, a: Answer) {
        *self.answer.lock().unwrap() = Some(a);
        self.bell.notify_all();
    }

    /// Give up on whatever is outstanding.
    pub fn abandon(&self) {
        *self.abandoned.lock().unwrap() = true;
        self.bell.notify_all();
    }

    pub fn is_abandoned(&self) -> bool {
        *self.abandoned.lock().unwrap()
    }

    /// Wait for an answer, or give up.
    ///
    /// Bounded, because this is called from inside a bluez method call: an
    /// unbounded wait would leave that call outstanding for as long as nobody
    /// walked past the panel.
    fn wait_for_answer(&self) -> Option<Answer> {
        let mut answer = self.answer.lock().unwrap();
        let deadline = std::time::Instant::now() + ANSWER_TIMEOUT;
        loop {
            if let Some(a) = answer.take() {
                return Some(a);
            }
            if self.is_abandoned() {
                return None;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return None;
            }
            let (guard, timeout) = self.bell.wait_timeout(answer, left).unwrap();
            answer = guard;
            if timeout.timed_out() && answer.is_none() {
                return None;
            }
        }
    }
}

/// The object bluez calls into.
pub struct Agent {
    pub shared: Arc<Shared>,
}

/// An address as bluez spells it in an object path: the last six groups of a
/// path like `/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF`.
pub fn address_from_path(path: &str) -> String {
    path.rsplit('/')
        .next()
        .and_then(|leaf| leaf.strip_prefix("dev_"))
        .map(|a| a.replace('_', ":"))
        .unwrap_or_default()
}

/// The object path bluez uses for a device on a given adapter.
pub fn path_for(adapter: &str, address: &str) -> String {
    format!("/org/bluez/{adapter}/dev_{}", address.replace(':', "_"))
}

/// A passkey as it must be shown.
///
/// Six digits, zero padded. bluez sends an integer, so a passkey of 1234 is
/// `001234` on the other device's screen and typing `1234` will not pair it.
pub fn format_passkey(passkey: u32) -> String {
    format!("{passkey:06}")
}

#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    /// bluez is done with this agent.
    fn release(&self) {}

    /// Older devices: a PIN typed on this machine.
    async fn request_pin_code(
        &self,
        device: zbus::zvariant::OwnedObjectPath,
    ) -> zbus::fdo::Result<String> {
        let address = address_from_path(device.as_str());
        self.shared.set(Pairing::RequestPin {
            address: address.clone(),
        });
        match self.shared.wait_for_answer() {
            Some(Answer::Pin(pin)) => Ok(pin),
            _ => {
                self.fail(&address, Stage::Unanswered, "nobody entered a PIN");
                Err(zbus::fdo::Error::Failed("cancelled".into()))
            }
        }
    }

    /// Older devices: a PIN to be typed on the device.
    fn display_pin_code(
        &self,
        device: zbus::zvariant::OwnedObjectPath,
        pincode: String,
    ) -> zbus::fdo::Result<()> {
        self.shared.set(Pairing::DisplayPinCode {
            address: address_from_path(device.as_str()),
            pin: pincode,
        });
        Ok(())
    }

    /// A passkey typed on this machine.
    async fn request_passkey(
        &self,
        device: zbus::zvariant::OwnedObjectPath,
    ) -> zbus::fdo::Result<u32> {
        let address = address_from_path(device.as_str());
        self.shared.set(Pairing::RequestPasskey {
            address: address.clone(),
        });
        match self.shared.wait_for_answer() {
            Some(Answer::Passkey(p)) => Ok(p),
            _ => {
                self.fail(&address, Stage::Unanswered, "nobody entered a passkey");
                Err(zbus::fdo::Error::Failed("cancelled".into()))
            }
        }
    }

    /// **The keyboard case.** Show this; the person types it on the keyboard.
    ///
    /// Returns immediately. bluez keeps calling this with a rising `entered` as
    /// keys are pressed, which is the only sign that the right keyboard is being
    /// typed on.
    fn display_passkey(
        &self,
        device: zbus::zvariant::OwnedObjectPath,
        passkey: u32,
        entered: u16,
    ) {
        self.shared.set(Pairing::DisplayPasskey {
            address: address_from_path(device.as_str()),
            passkey,
            entered: entered as u32,
        });
    }

    /// Both ends show the same number.
    async fn request_confirmation(
        &self,
        device: zbus::zvariant::OwnedObjectPath,
        passkey: u32,
    ) -> zbus::fdo::Result<()> {
        let address = address_from_path(device.as_str());
        self.shared.set(Pairing::Confirm {
            address: address.clone(),
            passkey,
        });
        match self.shared.wait_for_answer() {
            Some(Answer::Confirm(true)) => Ok(()),
            Some(Answer::Confirm(false)) => {
                self.fail(&address, Stage::Pairing, "the codes did not match");
                Err(zbus::fdo::Error::Failed("rejected".into()))
            }
            _ => {
                self.fail(&address, Stage::Unanswered, "nobody confirmed the code");
                Err(zbus::fdo::Error::Failed("cancelled".into()))
            }
        }
    }

    /// The device wants to pair and has no way to show anything.
    async fn request_authorization(
        &self,
        device: zbus::zvariant::OwnedObjectPath,
    ) -> zbus::fdo::Result<()> {
        let address = address_from_path(device.as_str());
        self.shared.set(Pairing::Authorize {
            address: address.clone(),
        });
        match self.shared.wait_for_answer() {
            Some(Answer::Confirm(true)) => Ok(()),
            _ => {
                self.fail(&address, Stage::Unanswered, "nobody allowed the pairing");
                Err(zbus::fdo::Error::Failed("cancelled".into()))
            }
        }
    }

    /// A profile on an already-paired device wants permission.
    async fn authorize_service(
        &self,
        device: zbus::zvariant::OwnedObjectPath,
        uuid: String,
    ) -> zbus::fdo::Result<()> {
        let address = address_from_path(device.as_str());
        self.shared.set(Pairing::AuthorizeService {
            address: address.clone(),
            uuid,
        });
        match self.shared.wait_for_answer() {
            Some(Answer::Confirm(true)) => Ok(()),
            _ => {
                self.fail(&address, Stage::Unanswered, "nobody allowed the service");
                Err(zbus::fdo::Error::Failed("cancelled".into()))
            }
        }
    }

    /// bluez gave up on whatever it was asking.
    fn cancel(&self) {
        self.shared.abandon();
    }
}

impl Agent {
    fn fail(&self, address: &str, stage: Stage, reason: &str) {
        self.shared.set(Pairing::Failed {
            address: address.to_string(),
            stage,
            reason: reason.to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_passkey_is_six_digits() {
        // bluez sends an integer. A passkey of 1234 is 001234 on the other
        // device, and typing 1234 will not pair it.
        assert_eq!(format_passkey(1234), "001234");
        assert_eq!(format_passkey(418299), "418299");
        assert_eq!(format_passkey(0), "000000");
    }

    #[test]
    fn addresses_survive_the_round_trip_through_a_path() {
        let path = path_for("hci0", "AA:BB:CC:DD:EE:FF");
        assert_eq!(path, "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF");
        assert_eq!(address_from_path(&path), "AA:BB:CC:DD:EE:FF");
    }

    #[test]
    fn a_path_that_is_not_a_device_yields_nothing_rather_than_rubbish() {
        assert_eq!(address_from_path("/org/bluez/hci0"), "");
        assert_eq!(address_from_path(""), "");
    }

    #[test]
    fn only_some_states_need_a_person() {
        assert!(Pairing::Confirm {
            address: "x".into(),
            passkey: 1
        }
        .needs_a_person());
        assert!(Pairing::RequestPin { address: "x".into() }.needs_a_person());
        // The keyboard case does NOT block: bluez hands over the passkey and
        // carries on while the person types it on the keyboard.
        assert!(!Pairing::DisplayPasskey {
            address: "x".into(),
            passkey: 1,
            entered: 0
        }
        .needs_a_person());
        assert!(!Pairing::Idle.needs_a_person());
    }

    #[test]
    fn an_answer_wakes_a_waiter() {
        let shared = Arc::new(Shared::new());
        let s = shared.clone();
        let waiter = std::thread::spawn(move || s.wait_for_answer());
        // Give the waiter a moment to park on the condvar.
        std::thread::sleep(Duration::from_millis(50));
        shared.answer(Answer::Confirm(true));
        assert_eq!(waiter.join().unwrap(), Some(Answer::Confirm(true)));
    }

    #[test]
    fn abandoning_releases_a_waiter_without_an_answer() {
        // Cancel from bluez, or a person walking away, must not leave a method
        // call outstanding.
        let shared = Arc::new(Shared::new());
        let s = shared.clone();
        let waiter = std::thread::spawn(move || s.wait_for_answer());
        std::thread::sleep(Duration::from_millis(50));
        shared.abandon();
        assert_eq!(waiter.join().unwrap(), None);
    }

    #[test]
    fn beginning_a_pairing_clears_the_last_one() {
        let shared = Shared::new();
        shared.abandon();
        shared.answer(Answer::Confirm(false));
        shared.begin("AA:BB:CC:DD:EE:FF");
        assert!(!shared.is_abandoned());
        assert_eq!(
            shared.get(),
            Pairing::Starting {
                address: "AA:BB:CC:DD:EE:FF".into()
            }
        );
    }
}
