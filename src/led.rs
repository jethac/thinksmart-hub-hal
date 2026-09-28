//! LED ring: a USB telephony HID interface on the audio composite device
//! 17ef:a017 (interface 1.5 on the reference unit).
//!
//! Protocol from ChristopheHD's working implementation, as ported in hubd:
//! three-byte reports `[0x02, mask, 0x00]`, where 0x02 is the report ID. The
//! ring is white/green/red only and the mask picks a firmware state rather than
//! mixing a colour. There is no "off" in the verified protocol - mask 0x00 is
//! static white - and arbitrary masks are deliberately not offered: this
//! endpoint shares a device with the speakers and microphones.

use crate::util::writable;
use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Mutex;

pub const PRESETS: &[(&str, u8)] = &[
    ("white", 0x00),
    ("green", 0x01),
    ("breathing_green", 0x02),
    ("red", 0x04),
    ("breathing_white", 0x08),
    ("rotating_green", 0x40),
];

#[derive(Clone, Default, Serialize)]
pub struct State {
    pub node: Option<String>,
    pub interface: Option<String>,
    pub candidates: Vec<String>,
    pub writable: bool,
    pub preset: Option<String>,
    pub presets: Vec<&'static str>,
    pub error: Option<String>,
}

pub struct Led {
    preset: Mutex<Option<String>>,
    error: Mutex<Option<String>>,
}

/// The USB interface a hidraw node belongs to, e.g. "1.5" from a sysfs path
/// component like "1-4:1.5".
pub fn hidraw_interface(name: &str) -> Option<String> {
    let real = fs::canonicalize(format!("/sys/class/hidraw/{name}/device")).ok()?;
    real.components().rev().find_map(|c| {
        let s = c.as_os_str().to_string_lossy();
        s.split_once(':')
            .filter(|(_, iface)| iface.contains('.') && !iface.contains(':'))
            .map(|(_, iface)| iface.to_string())
    })
}

/// hidraw nodes whose HID_ID matches `vid:pid` (both upper-case hex).
pub fn hidraw_for(vid: &str, pid: &str) -> Vec<String> {
    let needle = format!("0000{vid}:0000{pid}");
    let mut v: Vec<String> = fs::read_dir("/sys/class/hidraw")
        .map(|d| {
            d.flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    let ue = fs::read_to_string(e.path().join("device/uevent")).ok()?;
                    ue.to_uppercase().contains(&needle).then_some(name)
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

impl Led {
    pub fn new() -> Self {
        Self { preset: Mutex::new(None), error: Mutex::new(None) }
    }

    /// Located fresh on every call: hidraw numbering follows USB enumeration
    /// order and moves between boots and replugs.
    fn locate() -> (Option<(String, String)>, Vec<String>) {
        let nodes = hidraw_for("17EF", "A017");
        let tagged: Vec<(String, String)> = nodes
            .iter()
            .map(|n| (n.clone(), hidraw_interface(n).unwrap_or_else(|| "?".into())))
            .collect();
        let candidates = tagged.iter().map(|(n, i)| format!("{n} (interface {i})")).collect();
        let chosen = tagged
            .iter()
            .find(|(_, i)| i.ends_with(".5"))
            .or(if tagged.len() == 1 { tagged.first() } else { None })
            .cloned();
        (chosen, candidates)
    }

    pub fn state(&self) -> State {
        let (chosen, candidates) = Self::locate();
        let node = chosen.as_ref().map(|(n, _)| format!("/dev/{n}"));
        let error = self.error.lock().unwrap().clone().or_else(|| match (&node, candidates.len()) {
            (None, 0) => Some("no hidraw node for 17ef:a017".into()),
            (None, _) => Some("17ef:a017 has hidraw nodes but none on interface .5".into()),
            _ => None,
        });
        State {
            writable: node.as_deref().is_some_and(|n| writable(std::path::Path::new(n))),
            node,
            interface: chosen.map(|(_, i)| i),
            candidates,
            preset: self.preset.lock().unwrap().clone(),
            presets: PRESETS.iter().map(|(n, _)| *n).collect(),
            error,
        }
    }

    pub fn set(&self, preset: &str) -> Result<(), String> {
        let mask = PRESETS
            .iter()
            .find(|(n, _)| *n == preset)
            .map(|(_, m)| *m)
            .ok_or_else(|| format!("unknown preset '{preset}'"))?;
        let (chosen, _) = Self::locate();
        let (node, _) = chosen.ok_or("LED ring hidraw node not found")?;
        let result = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(format!("/dev/{node}"))
            .and_then(|mut f| f.write_all(&[0x02, mask, 0x00]));
        match result {
            Ok(()) => {
                *self.preset.lock().unwrap() = Some(preset.to_string());
                *self.error.lock().unwrap() = None;
                Ok(())
            }
            Err(e) => {
                let msg = format!("write to /dev/{node} failed: {e}");
                *self.error.lock().unwrap() = Some(msg.clone());
                Err(msg)
            }
        }
    }
}
