//! The panel: brightness and power over DDC/CI, and what mode it is in.
//!
//! There is no /sys/class/backlight on the Hub 500. The panel answers DDC/CI on
//! an I2C bus (i2c-5 on the reference unit): VCP 0x10 is brightness 0-100 and
//! VCP 0xD6 is power mode. ddcutil takes ~200 ms per call and the bus does not
//! like being hammered, so state is cached and refreshed slowly, and every call
//! is serialised through one lock.
//!
//! [`panel`] and [`Panel::shortfall`] are a different thing entirely and are here
//! because they answer a question about the same piece of hardware: how big is it
//! actually. They read /sys/class/drm rather than asking a compositor, because
//! the compositor is frequently the thing that is wrong.

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

/// A connected display, as the kernel sees it.
///
/// Read from /sys/class/drm, which is the mode the hardware is in -- not what a
/// compositor believes, and not what a toolkit has been told. That distinction is
/// the entire reason this exists.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Panel {
    /// DRM connector, e.g. `card0-DP-1`.
    pub connector: String,
    /// The preferred mode, which on a fixed panel is its native resolution.
    pub width: u32,
    pub height: u32,
    /// The Hub 500's own screen rather than something plugged into an HDMI port.
    pub internal: bool,
    /// DPMS says the connector is on. A blanked panel still reports its mode.
    pub powered: bool,
}

/// How far a window falls short of covering the panel.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Shortfall {
    pub connector: String,
    pub panel_width: u32,
    pub panel_height: u32,
    pub window_width: u32,
    pub window_height: u32,
    /// Unused pixels along each edge. Either can be zero.
    pub unused_width: u32,
    pub unused_height: u32,
}

impl std::fmt::Display for Shortfall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "window is {}x{} on {} which is {}x{}: {} px unused across, {} px unused down",
            self.window_width,
            self.window_height,
            self.connector,
            self.panel_width,
            self.panel_height,
            self.unused_width,
            self.unused_height
        )
    }
}

impl Panel {
    /// Whether a window of this physical size covers the whole panel.
    pub fn covers(&self, window_width: u32, window_height: u32) -> bool {
        window_width >= self.width && window_height >= self.height
    }

    /// What is missing, or None when the window covers the panel.
    ///
    /// This is the check that was not there. The panel spent a day running
    /// 1920x1045 inside a 1920x1080 display -- a compositor was letting the
    /// client draw its own decorations, so 35 rows were reserved and never
    /// painted. It was invisible in every log, and was found by taking a
    /// screenshot and counting black rows. Nothing in software could have
    /// noticed, because nothing in software had ever compared the two numbers.
    ///
    /// Sizes are PHYSICAL pixels. A toolkit working in logical pixels has to
    /// multiply by its scale factor before asking, or this will report a
    /// shortfall on a correct window.
    pub fn shortfall(&self, window_width: u32, window_height: u32) -> Option<Shortfall> {
        if self.covers(window_width, window_height) {
            return None;
        }
        Some(Shortfall {
            connector: self.connector.clone(),
            panel_width: self.width,
            panel_height: self.height,
            window_width,
            window_height,
            unused_width: self.width.saturating_sub(window_width),
            unused_height: self.height.saturating_sub(window_height),
        })
    }
}

/// Every connected display, in connector order.
pub fn panels() -> Vec<Panel> {
    let Ok(dir) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    let mut found: Vec<Panel> = dir
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_str()?.to_string();
            // card0-DP-1 and friends. Bare `card0` is the device, not a
            // connector, and has no status to read.
            if !name.contains('-') {
                return None;
            }
            if crate::util::read_trim(path.join("status"))? != "connected" {
                return None;
            }
            let (width, height) = parse_modes(&crate::util::read_trim(path.join("modes"))?)?;
            Some(Panel {
                internal: is_internal(&name),
                connector: name,
                width,
                height,
                powered: crate::util::read_trim(path.join("dpms"))
                    .map(|d| d.eq_ignore_ascii_case("On"))
                    .unwrap_or(false),
            })
        })
        .collect();
    found.sort_by(|a, b| a.connector.cmp(&b.connector));
    found
}

/// The Hub 500's own screen.
///
/// The internal one if it is there, otherwise whatever single display is
/// connected -- a unit on a bench with an HDMI monitor should still be able to
/// answer this rather than returning nothing and looking broken.
pub fn panel() -> Option<Panel> {
    let all = panels();
    all.iter()
        .find(|p| p.internal)
        .cloned()
        .or_else(|| (all.len() == 1).then(|| all[0].clone()))
}

/// The first line of `modes` is the preferred mode, which on a fixed panel is its
/// native resolution. Later lines are fallbacks the panel will also accept and
/// are not what it is running.
fn parse_modes(text: &str) -> Option<(u32, u32)> {
    let first = text.lines().next()?.trim();
    let (w, h) = first.split_once('x')?;
    // Modes can carry a suffix like `1920x1080i` for interlaced.
    let h: String = h.chars().take_while(|c| c.is_ascii_digit()).collect();
    Some((w.trim().parse().ok()?, h.parse().ok()?))
}

/// The Hub 500 drives its own screen over embedded DisplayPort; the two HDMI
/// connectors are outputs for external monitors. Same rule as [`pick_panel`]
/// applies to ddcutil output, kept separate because one reads sysfs and the other
/// parses a tool.
fn is_internal(connector: &str) -> bool {
    let c = connector.to_ascii_uppercase();
    c.contains("-DP-") || c.contains("EDP")
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
    fn reads_the_preferred_mode_and_ignores_the_fallbacks() {
        // What /sys/class/drm/card0-DP-1/modes looks like on a hub: the native
        // mode twice, because the panel advertises it under two timings.
        assert_eq!(parse_modes("1920x1080\n1920x1080\n"), Some((1920, 1080)));
        assert_eq!(parse_modes("1920x1080i\n1280x720\n"), Some((1920, 1080)));
        assert_eq!(parse_modes(""), None);
        assert_eq!(parse_modes("garbage\n"), None);
    }

    #[test]
    fn tells_the_built_in_screen_from_an_hdmi_port() {
        assert!(is_internal("card0-DP-1"));
        assert!(is_internal("card0-eDP-1"));
        assert!(!is_internal("card0-HDMI-A-1"));
        assert!(!is_internal("card0-HDMI-A-2"));
    }

    #[test]
    fn a_window_that_covers_the_panel_reports_nothing() {
        let p = Panel {
            connector: "card0-DP-1".into(),
            width: 1920,
            height: 1080,
            internal: true,
            powered: true,
        };
        assert!(p.covers(1920, 1080));
        assert_eq!(p.shortfall(1920, 1080), None);
    }

    #[test]
    fn the_band_that_was_actually_there_is_reported() {
        // The real numbers from the day this was added: cage was started without
        // -d, winit drew its own decorations, and 35 rows were reserved and never
        // painted.
        let p = Panel {
            connector: "card0-DP-1".into(),
            width: 1920,
            height: 1080,
            internal: true,
            powered: true,
        };
        let s = p.shortfall(1920, 1045).expect("a shortfall");
        assert_eq!(s.unused_height, 35);
        assert_eq!(s.unused_width, 0);
        assert!(s.to_string().contains("35 px unused down"), "{s}");
    }

    #[test]
    fn parses_brief_vcp() {
        assert_eq!(parse_continuous("VCP 10 C 60 100\n"), (Some(60), Some(100)));
        assert_eq!(parse_noncontinuous("VCP D6 SNC x01\n"), Some(1));
    }
}
