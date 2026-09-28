//! PIR proximity sensor, exposed by `hid-sensor-prox` as an IIO device named
//! `prox`, on the USB sensor hub 17ef:60c0.
//!
//! Every read of this sensor is a HID get-feature round trip, and the device
//! sometimes never answers. The reading task then sleeps in `usbhid_wait_io`
//! in uninterruptible (D) state: it cannot be signalled, it holds the sysfs
//! attribute so every other reader queues behind it, and only a USB reset of
//! the device frees it (issue #1, measured on hub-01). So:
//!
//! * No thread of this process ever touches the sensor's attributes. Every read
//!   and write is a child process (`cat`, `sh -c printf`) that can be
//!   abandoned. A thread in D state would make the whole daemon unkillable; a
//!   child in D state is just a stuck child.
//! * At most one child is ever outstanding. While one is stuck the sensor is
//!   reported as wedged and nothing else is attempted until it is reaped.
//! * The sampling rate is never read. Reading it while it is 0 hangs, and it is
//!   0 after every boot and every reset. The rate shown is the one this process
//!   last wrote; until then it is unknown, and unknown is treated as 0.
//! * `in_proximity_raw` is never read while the rate is 0 or unknown, and
//!   nothing is read at startup.
//! * Recovery is a USB reset via the device's `authorized` attribute, which the
//!   udev rule opens to the `input` group.
//!
//! The udev rule also pins the device's runtime PM to `on`. Without it, reading
//! raw powers the sensor up, USB autosuspend then powers it down, and *that*
//! get-feature can wedge a kworker in the shared PM workqueue - outside this
//! process entirely.

use crate::util::{last_line, now_ms, writable};
use serde::Serialize;
use std::collections::VecDeque;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const IIO: &str = "/sys/bus/iio/devices";
const USB: &str = "/sys/bus/usb/devices";
const RATE: &str = "in_proximity_sampling_frequency";
const RAW: &str = "in_proximity_raw";
/// The first raw read after activation is expensive and every one after it is
/// free: 21 s then 0 s, measured on hub-01 with runtime PM pinned on for the
/// HID-SENSOR device. 15 s was not enough to ever see the first value.
const RAW_TIMEOUT: Duration = Duration::from_secs(30);
/// Writing the sampling rate is the same HID set-feature round trip as reading
/// one, so it costs about the same: 10 s measured on hub-01, with the read-back
/// at 11 s. The old shared 5 s deadline could never win, so the rate was never
/// set, raw was never read, and the card could not leave `inactive` (#2).
const RATE_WRITE_TIMEOUT: Duration = Duration::from_secs(20);
/// Writing `authorized` is a plain sysfs write with no HID round trip. Kept
/// short deliberately: a USB reset that cannot write should say so promptly
/// rather than sitting for 20 s first.
const AUTH_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const HISTORY: usize = 240;

#[derive(Clone, Default, Serialize)]
pub struct State {
    pub status: &'static str,
    pub path: Option<String>,
    pub usb_port: Option<String>,
    pub reset_writable: bool,
    pub reset_command: Option<String>,
    pub rate_hz: Option<f64>,
    pub raw: Option<i64>,
    pub interval_s: u64,
    pub reads: u64,
    pub stalls: u64,
    pub stuck_pid: Option<u32>,
    pub stuck_since: Option<u64>,
    pub last_read_ms: Option<u64>,
    pub last_change: Option<u64>,
    pub history: VecDeque<(u64, i64)>,
    pub error: Option<String>,
}

pub struct Prox {
    st: Mutex<State>,
    stuck: Mutex<Option<Child>>,
    interval_s: AtomicU64,
    poll_lock: Mutex<()>,
}

enum Outcome {
    Done(String),
    Failed(String),
    Stuck(Child),
}

/// Run a child with a deadline. On timeout it is sent SIGKILL - which takes
/// effect only if it ever leaves D state - and handed back to be reaped later.
fn run_child(mut cmd: Command, timeout: Duration) -> Outcome {
    let spawned = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return Outcome::Failed(e.to_string()),
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Output is a few bytes, so it cannot have filled the pipe.
                let mut out = String::new();
                let mut err = String::new();
                if let Some(mut o) = child.stdout.take() {
                    let _ = o.read_to_string(&mut out);
                }
                if let Some(mut e) = child.stderr.take() {
                    let _ = e.read_to_string(&mut err);
                }
                return if status.success() {
                    Outcome::Done(out.trim().to_string())
                } else {
                    Outcome::Failed(last_line(&err))
                };
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                return Outcome::Stuck(child);
            }
            Ok(None) => thread::sleep(Duration::from_millis(25)),
            Err(e) => return Outcome::Failed(e.to_string()),
        }
    }
}

fn write_cmd(path: PathBuf, value: &str) -> Command {
    let mut c = Command::new("sh");
    c.args(["-c", "printf '%s' \"$1\" > \"$2\"", "sh", value]).arg(path);
    c
}

/// The IIO directory. Re-found on every call: a USB reset re-enumerates the
/// device and it can come back as a different iio:deviceN. Reading `name`
/// is answered by the IIO core, not the device, so it cannot hang.
fn find() -> Option<PathBuf> {
    fs::read_dir(IIO).ok()?.flatten().map(|e| e.path()).find(|p| {
        fs::read_to_string(p.join("name")).map(|n| n.trim() == "prox").unwrap_or(false)
    })
}

/// The sensor hub's USB device directory, by vendor/product. Port names such
/// as 1-11.1 differ between units.
fn usb_port() -> Option<PathBuf> {
    fs::read_dir(USB).ok()?.flatten().map(|e| e.path()).find(|p| {
        let id = |f: &str| fs::read_to_string(p.join(f)).map(|s| s.trim().to_string()).unwrap_or_default();
        id("idVendor") == "17ef" && id("idProduct") == "60c0"
    })
}

impl Prox {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            st: Mutex::new(State { status: "starting", interval_s: 10, ..Default::default() }),
            stuck: Mutex::new(None),
            interval_s: AtomicU64::new(10),
            poll_lock: Mutex::new(()),
        })
    }

    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        thread::spawn(move || loop {
            me.poll();
            let started = Instant::now();
            while started.elapsed().as_secs() < me.interval_s.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(250));
            }
        });
    }

    pub fn state(&self) -> State {
        self.st.lock().unwrap().clone()
    }

    /// True while an abandoned child is still stuck in the kernel.
    fn still_stuck(&self) -> bool {
        let mut slot = self.stuck.lock().unwrap();
        if let Some(c) = slot.as_mut() {
            if matches!(c.try_wait(), Ok(None)) {
                return true;
            }
        }
        if slot.take().is_some() {
            let mut st = self.st.lock().unwrap();
            st.stuck_pid = None;
            st.stuck_since = None;
        }
        false
    }

    fn park(&self, child: Child, what: &str, timeout: Duration) {
        let mut st = self.st.lock().unwrap();
        st.stalls += 1;
        st.status = "wedged";
        st.stuck_pid = Some(child.id());
        st.stuck_since = Some(now_ms());
        st.error = Some(format!("{what} did not answer in {}s; the reader is stuck in the kernel", timeout.as_secs()));
        *self.stuck.lock().unwrap() = Some(child);
    }

    fn refresh_port(&self) {
        let port = usb_port();
        let mut st = self.st.lock().unwrap();
        st.usb_port = port.as_ref().map(|p| p.file_name().unwrap_or_default().to_string_lossy().to_string());
        st.reset_writable = port.as_ref().is_some_and(|p| writable(&p.join("authorized")));
        st.reset_command = port.map(|p| {
            let a = p.join("authorized");
            format!("echo 0 | sudo tee {a} && sleep 3 && echo 1 | sudo tee {a}", a = a.display())
        });
    }

    pub fn poll(&self) {
        let _one_at_a_time = self.poll_lock.lock().unwrap();
        self.refresh_port();
        if self.st.lock().unwrap().status == "resetting" {
            return;
        }
        if self.still_stuck() {
            self.st.lock().unwrap().status = "wedged";
            return;
        }
        let Some(dir) = find() else {
            let mut st = self.st.lock().unwrap();
            st.status = "unavailable";
            st.path = None;
            st.error = Some("no IIO device named 'prox'".into());
            return;
        };
        {
            let mut st = self.st.lock().unwrap();
            st.path = Some(dir.display().to_string());
            if st.rate_hz.unwrap_or(0.0) <= 0.0 {
                st.status = "inactive";
                st.raw = None;
                st.error = None;
                return;
            }
            if st.raw.is_none() {
                st.status = "reading";
            }
        }

        let mut cat = Command::new("cat");
        cat.arg(dir.join(RAW));
        let t = Instant::now();
        match run_child(cat, RAW_TIMEOUT) {
            Outcome::Done(s) => {
                let mut st = self.st.lock().unwrap();
                st.reads += 1;
                st.last_read_ms = Some(t.elapsed().as_millis() as u64);
                st.status = "active";
                st.error = None;
                if let Ok(v) = s.parse::<i64>() {
                    if st.raw.is_some_and(|old| old != v) {
                        st.last_change = Some(now_ms());
                    }
                    st.raw = Some(v);
                    st.history.push_back((now_ms(), v));
                    while st.history.len() > HISTORY {
                        st.history.pop_front();
                    }
                }
            }
            Outcome::Failed(e) => {
                let mut st = self.st.lock().unwrap();
                st.status = "error";
                st.error = Some(format!("{RAW}: {e}"));
            }
            Outcome::Stuck(child) => self.park(child, RAW, RAW_TIMEOUT),
        }
    }

    pub fn set_rate(self: &Arc<Self>, hz: f64) -> Result<(), String> {
        if !(0.0..=1000.0).contains(&hz) {
            return Err("rate must be between 0 and 1000 Hz".into());
        }
        if self.still_stuck() {
            return Err("sensor is wedged - reset it first".into());
        }
        let dir = find().ok_or("no IIO device named 'prox'")?;
        match run_child(write_cmd(dir.join(RATE), &format!("{hz}")), RATE_WRITE_TIMEOUT) {
            Outcome::Done(_) => {}
            Outcome::Failed(e) if e.contains("Permission denied") => {
                return Err("permission denied - is the udev rule installed and are you in 'input'?".into())
            }
            Outcome::Failed(e) => return Err(e),
            Outcome::Stuck(child) => {
                self.park(child, RATE, RATE_WRITE_TIMEOUT);
                return Err("the write did not return; sensor is wedged".into());
            }
        }
        {
            let mut st = self.st.lock().unwrap();
            st.rate_hz = Some(hz);
            if hz <= 0.0 {
                st.raw = None;
            }
        }
        let me = self.clone();
        thread::spawn(move || me.poll());
        Ok(())
    }

    pub fn set_interval(&self, seconds: u64) {
        let s = seconds.clamp(1, 60);
        self.interval_s.store(s, Ordering::Relaxed);
        self.st.lock().unwrap().interval_s = s;
    }

    /// Deauthorise and reauthorise the sensor hub. Confirmed on hub-01 to free
    /// a reader stuck in D state. The device comes back at 0 Hz.
    pub fn reset(self: &Arc<Self>) -> Result<(), String> {
        self.refresh_port();
        let (writable, command) = {
            let st = self.st.lock().unwrap();
            (st.reset_writable, st.reset_command.clone())
        };
        let port = usb_port().ok_or("sensor hub 17ef:60c0 not found on USB")?;
        if !writable {
            return Err(format!("no permission to reset; run: {}", command.unwrap_or_default()));
        }
        {
            let mut st = self.st.lock().unwrap();
            if st.status == "resetting" {
                return Err("already resetting".into());
            }
            st.status = "resetting";
        }
        let me = self.clone();
        thread::spawn(move || {
            let auth = port.join("authorized");
            let mut err = None;
            for (value, pause) in [("0", 3), ("1", 3)] {
                match run_child(write_cmd(auth.clone(), value), AUTH_WRITE_TIMEOUT) {
                    Outcome::Done(_) => {}
                    Outcome::Failed(e) => err = Some(e),
                    Outcome::Stuck(_) => err = Some(format!("writing {value} to authorized did not return")),
                }
                thread::sleep(Duration::from_secs(pause));
            }
            {
                let mut st = me.st.lock().unwrap();
                st.status = "starting";
                st.rate_hz = None;
                st.raw = None;
                st.history.clear();
                st.error = err;
            }
            if me.still_stuck() {
                me.st.lock().unwrap().error = Some("reset done, but the stuck reader has not exited".into());
            }
            me.poll();
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    #[test]
    fn child_answers() {
        assert!(matches!(run_child(sh("echo 42"), Duration::from_secs(5)), Outcome::Done(s) if s == "42"));
    }

    #[test]
    fn child_fails_with_its_stderr() {
        let out = run_child(sh("echo 'Permission denied' >&2; exit 1"), Duration::from_secs(5));
        assert!(matches!(out, Outcome::Failed(e) if e.contains("Permission denied")));
    }

    /// Guards #2: the rate write is a HID round trip measured at 10 s on
    /// hub-01, so its deadline has to clear that with margin. Sharing the short
    /// `authorized` deadline meant set_rate could never succeed and the card
    /// could never leave `inactive`. Keep them separate.
    #[test]
    fn rate_write_deadline_clears_the_measured_cost() {
        assert!(
            RATE_WRITE_TIMEOUT >= Duration::from_secs(15),
            "rate writes take ~10s on real hardware; {:?} is too tight",
            RATE_WRITE_TIMEOUT
        );
        assert!(
            AUTH_WRITE_TIMEOUT < RATE_WRITE_TIMEOUT,
            "a plain sysfs write should fail fast, not wait for the HID deadline"
        );
        assert!(
            RAW_TIMEOUT >= Duration::from_secs(25),
            "the first raw read after activation took 21s on hub-01; {:?} is too tight",
            RAW_TIMEOUT
        );
    }

    #[test]
    fn silent_child_is_abandoned_not_waited_for() {
        let t = Instant::now();
        let Outcome::Stuck(mut child) = run_child(sh("sleep 30"), Duration::from_millis(300)) else {
            panic!("expected Stuck");
        };
        assert!(t.elapsed() < Duration::from_secs(2), "caller must not block past the deadline");
        // A sleeping child is killable, so the SIGKILL from run_child lands and
        // it can be reaped. A child in D state would stay until a USB reset.
        thread::sleep(Duration::from_millis(200));
        assert!(matches!(child.try_wait(), Ok(Some(_))));
    }

    #[test]
    fn writes_value_to_path() {
        let p = std::env::temp_dir().join(format!("hub-tester-write-{}", std::process::id()));
        assert!(matches!(run_child(write_cmd(p.clone(), "10"), Duration::from_secs(5)), Outcome::Done(_)));
        assert_eq!(fs::read_to_string(&p).unwrap(), "10");
        let _ = fs::remove_file(p);
    }
}
