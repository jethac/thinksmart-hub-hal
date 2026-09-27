//! Panel brightness and power over DDC/CI.
//!
//! There is no /sys/class/backlight on the Hub 500. The panel answers DDC/CI on
//! an I2C bus (i2c-5 on the reference unit): VCP 0x10 is brightness 0-100 and
//! VCP 0xD6 is power mode. ddcutil takes ~200 ms per call and the bus does not
//! like being hammered, so state is cached and refreshed slowly, and every call
//! is serialised through one lock.

use crate::util::{last_line, now_ms, run};
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const DDC_TIMEOUT: Duration = Duration::from_secs(10);
const REFRESH: Duration = Duration::from_secs(30);

#[derive(Clone, Default, Serialize)]
pub struct State {
    pub bus: Option<u32>,
    pub connector: Option<String>,
    pub brightness: Option<u32>,
    pub brightness_max: Option<u32>,
    pub power: Option<String>,
    pub blanked_until: Option<u64>,
    pub last_op_ms: Option<u64>,
    pub detect: String,
    pub error: Option<String>,
}

pub struct Display {
    st: Mutex<State>,
    bus_lock: Mutex<()>,
}

impl Display {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { st: Mutex::new(State::default()), bus_lock: Mutex::new(()) })
    }

    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        thread::spawn(move || {
            me.detect();
            loop {
                me.refresh();
                thread::sleep(REFRESH);
            }
        });
    }

    pub fn state(&self) -> State {
        self.st.lock().unwrap().clone()
    }

    fn ddc(&self, args: &[&str]) -> Result<String, String> {
        let _bus = self.bus_lock.lock().unwrap();
        let t = Instant::now();
        let out = run("ddcutil", args, DDC_TIMEOUT);
        self.st.lock().unwrap().last_op_ms = Some(t.elapsed().as_millis() as u64);
        if out.ok {
            Ok(out.text)
        } else {
            Err(last_line(&out.text))
        }
    }

    /// Find the panel's bus. Bus numbers are assignment order and are not
    /// promised to stay at 5, so this asks ddcutil instead of assuming.
    /// HUB_I2C_BUS overrides it.
    pub fn detect(&self) {
        let forced: Option<u32> = std::env::var("HUB_I2C_BUS").ok().and_then(|s| s.parse().ok());
        let result = self.ddc(&["detect"]);
        let mut st = self.st.lock().unwrap();
        match result {
            Ok(text) => {
                let (bus, conn) = match forced {
                    Some(b) => (Some(b), Some("forced by HUB_I2C_BUS".into())),
                    None => pick_panel(&text),
                };
                st.detect = text;
                st.bus = bus;
                st.connector = conn;
                st.error = bus.is_none().then(|| "ddcutil found no DDC/CI display".into());
            }
            Err(e) => {
                st.bus = forced;
                st.error = Some(e);
            }
        }
    }

    fn bus(&self) -> Option<String> {
        self.st.lock().unwrap().bus.map(|b| b.to_string())
    }

    fn blanked(&self) -> bool {
        self.st.lock().unwrap().blanked_until.is_some()
    }

    pub fn refresh(&self) {
        let Some(bus) = self.bus() else { return };
        // Reading while blanked can wake some panels; leave it alone.
        if self.blanked() {
            return;
        }
        match self.ddc(&["--bus", &bus, "--brief", "getvcp", "10"]) {
            Ok(text) => {
                let (cur, max) = parse_continuous(&text);
                let mut st = self.st.lock().unwrap();
                st.brightness = cur;
                st.brightness_max = max;
                st.error = None;
            }
            Err(e) => {
                self.st.lock().unwrap().error = Some(e);
                return;
            }
        }
        if let Ok(text) = self.ddc(&["--bus", &bus, "--brief", "getvcp", "d6"]) {
            self.st.lock().unwrap().power = parse_noncontinuous(&text).map(power_name);
        }
    }

    pub fn set_brightness(&self, value: u32) -> Result<(), String> {
        let bus = self.bus().ok_or("no DDC/CI bus detected")?;
        let value = value.min(100);
        self.ddc(&["--bus", &bus, "setvcp", "10", &value.to_string()])?;
        let mut st = self.st.lock().unwrap();
        st.brightness = Some(value);
        st.error = None;
        Ok(())
    }

    /// Switch the panel off for `seconds`, then back on. The restore is done by
    /// the daemon, not the page: with the panel dark there is nothing to tap.
    pub fn blank(self: &Arc<Self>, seconds: u64) -> Result<(), String> {
        let bus = self.bus().ok_or("no DDC/CI bus detected")?;
        if self.blanked() {
            return Err("already blanked".into());
        }
        let seconds = seconds.clamp(1, 30);
        self.ddc(&["--bus", &bus, "setvcp", "d6", "4"])?;
        self.st.lock().unwrap().blanked_until = Some(now_ms() + seconds * 1000);

        let me = self.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(seconds));
            let mut last = Ok(String::new());
            for _ in 0..3 {
                last = me.ddc(&["--bus", &bus, "setvcp", "d6", "1"]);
                if last.is_ok() {
                    break;
                }
                thread::sleep(Duration::from_secs(1));
            }
            {
                let mut st = me.st.lock().unwrap();
                st.blanked_until = None;
                if let Err(e) = last {
                    st.error = Some(format!("panel did not come back on: {e}"));
                }
            }
            me.refresh();
        });
        Ok(())
    }
}

/// Choose the internal panel from `ddcutil detect` output. It is on embedded
/// DisplayPort (card0-DP-1); the two HDMI outputs are external monitors, which
/// may also speak DDC/CI and must not be mistaken for the panel.
fn pick_panel(text: &str) -> (Option<u32>, Option<String>) {
    let mut first = None;
    let mut cur: Option<u32> = None;
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("I2C bus:") {
            cur = rest.trim().strip_prefix("/dev/i2c-").and_then(|n| n.trim().parse().ok());
            first = first.or(cur);
        } else if let Some(rest) = line.strip_prefix("DRM connector:") {
            let c = rest.trim();
            if (c.contains("-DP-") || c.contains("eDP")) && cur.is_some() {
                return (cur, Some(c.to_string()));
            }
        }
    }
    (first, None)
}

/// `VCP 10 C 60 100` -> (60, 100)
fn parse_continuous(text: &str) -> (Option<u32>, Option<u32>) {
    for line in text.lines() {
        let t: Vec<&str> = line.split_whitespace().collect();
        if t.len() >= 5 && t[0] == "VCP" && t[2] == "C" {
            return (t[3].parse().ok(), t[4].parse().ok());
        }
    }
    (None, None)
}

/// `VCP D6 SNC x01` -> 1
fn parse_noncontinuous(text: &str) -> Option<u32> {
    for line in text.lines() {
        let t: Vec<&str> = line.split_whitespace().collect();
        if t.len() >= 4 && t[0] == "VCP" {
            return u32::from_str_radix(t[3].trim_start_matches('x'), 16).ok();
        }
    }
    None
}

fn power_name(v: u32) -> String {
    match v {
        1 => "on".into(),
        2 => "standby".into(),
        3 => "suspend".into(),
        4 => "off".into(),
        5 => "off (power button)".into(),
        n => format!("0x{n:02x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_internal_panel_over_hdmi() {
        let text = "Display 1\n   I2C bus:  /dev/i2c-3\n   DRM connector:  card0-HDMI-A-1\n\
                    Display 2\n   I2C bus:  /dev/i2c-5\n   DRM connector:  card0-DP-1\n";
        assert_eq!(pick_panel(text), (Some(5), Some("card0-DP-1".into())));
    }

    #[test]
    fn parses_brief_vcp() {
        assert_eq!(parse_continuous("VCP 10 C 60 100\n"), (Some(60), Some(100)));
        assert_eq!(parse_noncontinuous("VCP D6 SNC x01\n"), Some(1));
    }
}
