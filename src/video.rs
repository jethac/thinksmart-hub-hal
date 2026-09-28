//! Video capture devices, through V4L2.
//!
//! Two kinds of device turn up on a Hub 500. Every unit has the built-in HDMI
//! capture card (`17ef:7219`), which takes YUYV at up to 1920x1080p60 and has no
//! camera controls beyond picture adjustment. Some units also have a USB webcam
//! -- the OBSBOT on the living room hub offers MJPEG to 4K plus pan, tilt, zoom,
//! autofocus and auto-exposure.
//!
//! Everything goes through `v4l2-ctl` rather than raw ioctls. The reason is the
//! same one that shapes [`crate::prox`]: a subprocess with a deadline cannot
//! wedge this process. A capture device with no signal on it can block in the
//! kernel, and a UI thread that does that is a dead panel.
//!
//! Each device presents two `/dev/video*` nodes, one for frames and one for
//! metadata. Only the first is useful, and the way to tell them apart is that
//! the metadata node enumerates no capture formats -- which is why
//! [`devices`] asks for formats rather than trusting the node's index.

use crate::util::{last_line, read_trim, run};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

const T: Duration = Duration::from_secs(5);

#[derive(Clone, Default, Serialize)]
pub struct Device {
    /// `/dev/videoN`.
    pub path: String,
    /// The card name V4L2 reports, e.g. "LENOVO USB 3.0 UVC UAC: Lenovo".
    pub card: String,
    /// True for the built-in HDMI capture card (`17ef:7219`).
    pub is_capture_card: bool,
    pub formats: Vec<Format>,
}

#[derive(Clone, Default, Serialize)]
pub struct Format {
    /// Four-character code, e.g. `YUYV` or `MJPG`.
    pub fourcc: String,
    pub width: u32,
    pub height: u32,
    /// Frame rates offered at this size, highest first.
    pub fps: Vec<f32>,
}

#[derive(Clone, Default, Serialize)]
pub struct Control {
    pub name: String,
    /// `int`, `bool`, `menu` -- what the UI should draw.
    pub kind: String,
    pub min: i64,
    pub max: i64,
    pub step: i64,
    pub default: i64,
    pub value: i64,
    /// V4L2 marks a control inactive when another control owns it: exposure time
    /// while auto-exposure is on, white balance while it is automatic. Showing
    /// those as editable would be a lie -- writes are accepted and ignored.
    pub active: bool,
}

/// Every node that can actually deliver frames, in `/dev/videoN` order.
pub fn devices() -> Vec<Device> {
    let mut nodes: Vec<PathBuf> = std::fs::read_dir("/sys/class/video4linux")
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("video"))
                })
                .collect()
        })
        .unwrap_or_default();
    // Numeric order, not lexical: video10 must not sort before video2.
    nodes.sort_by_key(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.trim_start_matches("video").parse::<u32>().ok())
            .unwrap_or(u32::MAX)
    });

    let mut out = Vec::new();
    for sys in nodes {
        let Some(node) = sys.file_name().and_then(|n| n.to_str()) else { continue };
        let path = format!("/dev/{node}");
        let formats = formats(&path);
        // No capture formats means this is the metadata node, not the camera.
        if formats.is_empty() {
            continue;
        }
        let card = read_trim(sys.join("name")).unwrap_or_else(|| node.to_string());
        out.push(Device {
            is_capture_card: is_capture_card(&sys, &card),
            path,
            card,
            formats,
        });
    }
    out
}

/// `17ef:7219` by its USB ids where sysfs exposes them, by name where it does
/// not. The ids are the truth and the name is the fallback, in that order.
fn is_capture_card(sys: &Path, card: &str) -> bool {
    // /sys/class/video4linux/videoN/device is the USB *interface*; the ids live
    // on its parent, the device.
    let usb = sys.join("device").join("..");
    let vid = read_trim(usb.join("idVendor"));
    let pid = read_trim(usb.join("idProduct"));
    if let (Some(v), Some(p)) = (vid, pid) {
        return v.eq_ignore_ascii_case("17ef") && p.eq_ignore_ascii_case("7219");
    }
    card.to_lowercase().contains("uvc uac")
}

/// Sizes and rates this device offers, biggest first.
pub fn formats(path: &str) -> Vec<Format> {
    let out = run("v4l2-ctl", &["-d", path, "--list-formats-ext"], T);
    if !out.ok {
        return Vec::new();
    }
    let mut formats: Vec<Format> = Vec::new();
    let mut fourcc = String::new();
    for line in out.text.lines() {
        let t = line.trim();
        if let Some(rest) = t.split_once(": '").map(|(_, r)| r) {
            // "[0]: 'YUYV' (YUYV 4:2:2)"
            if let Some((cc, _)) = rest.split_once('\'') {
                fourcc = cc.to_string();
            }
        } else if let Some(rest) = t.strip_prefix("Size: Discrete ") {
            if let Some((w, h)) = rest.split_once('x') {
                if let (Ok(w), Ok(h)) = (w.trim().parse(), h.trim().parse()) {
                    formats.push(Format { fourcc: fourcc.clone(), width: w, height: h, fps: vec![] });
                }
            }
        } else if t.starts_with("Interval: Discrete ") {
            // "Interval: Discrete 0.017s (60.000 fps)"
            if let Some(fps) = t
                .split_once('(')
                .and_then(|(_, r)| r.split_once(" fps"))
                .and_then(|(v, _)| v.parse::<f32>().ok())
            {
                if let Some(f) = formats.last_mut() {
                    f.fps.push(fps);
                }
            }
        }
    }
    for f in &mut formats {
        f.fps.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        f.fps.dedup();
    }
    formats.sort_by_key(|f| std::cmp::Reverse(f.width as u64 * f.height as u64));
    formats
}

/// What the device is set to right now: fourcc, width, height.
pub fn current_format(path: &str) -> Option<(String, u32, u32)> {
    let out = run("v4l2-ctl", &["-d", path, "--get-fmt-video"], T);
    if !out.ok {
        return None;
    }
    let mut size: Option<(u32, u32)> = None;
    let mut fourcc = None;
    for line in out.text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("Width/Height") {
            if let Some((w, h)) = rest.trim_start_matches([' ', ':']).split_once('/') {
                if let (Ok(w), Ok(h)) = (w.trim().parse(), h.trim().parse()) {
                    size = Some((w, h));
                }
            }
        } else if t.starts_with("Pixel Format") {
            fourcc = t.split('\'').nth(1).map(|s| s.to_string());
        }
    }
    size.map(|(w, h)| (fourcc.unwrap_or_default(), w, h))
}

/// Ask the device for a size and rate. V4L2 may answer with something near what
/// was asked rather than exactly it, so the caller should re-read
/// [`current_format`] instead of assuming this took.
pub fn set_format(path: &str, fourcc: &str, width: u32, height: u32) -> Result<(), String> {
    let spec = format!("width={width},height={height},pixelformat={fourcc}");
    let out = run("v4l2-ctl", &["-d", path, "--set-fmt-video", &spec], T);
    if out.ok {
        Ok(())
    } else {
        Err(last_line(&out.text))
    }
}

/// Every control the device exposes, in the order V4L2 lists them -- which
/// groups picture controls before camera controls, and is the order a person
/// expects to read them in.
pub fn controls(path: &str) -> Vec<Control> {
    let out = run("v4l2-ctl", &["-d", path, "--list-ctrls"], T);
    if !out.ok {
        return Vec::new();
    }
    out.text.lines().filter_map(parse_control).collect()
}

/// One line of `--list-ctrls`, e.g.
///
/// ```text
///   brightness 0x00980900 (int)  : min=0 max=100 step=1 default=50 value=50
///   exposure_time_absolute 0x009a0902 (int) : min=1 max=2500 ... flags=inactive
/// ```
fn parse_control(line: &str) -> Option<Control> {
    let (head, tail) = line.split_once(" : ")?;
    let mut words = head.split_whitespace();
    let name = words.next()?.to_string();
    // Skip the hex id; the kind is the parenthesised word after it.
    let kind = words
        .find(|w| w.starts_with('('))
        .map(|w| w.trim_matches(['(', ')']).to_string())?;
    // Types this cannot usefully draw. Skipped rather than shown broken.
    if !matches!(kind.as_str(), "int" | "bool" | "menu") {
        return None;
    }

    let mut c = Control { name, kind, step: 1, active: true, ..Default::default() };
    for pair in tail.split_whitespace() {
        let Some((k, v)) = pair.split_once('=') else { continue };
        match k {
            "min" => c.min = v.parse().unwrap_or(0),
            "max" => c.max = v.parse().unwrap_or(0),
            "step" => c.step = v.parse().unwrap_or(1),
            "default" => c.default = v.parse().unwrap_or(0),
            "value" => c.value = v.parse().unwrap_or(0),
            "flags" => c.active = !v.contains("inactive"),
            _ => {}
        }
    }
    // bool controls list no min/max. Say so explicitly rather than leaving the
    // range at 0..0 for a caller to trip over.
    if c.kind == "bool" {
        c.min = 0;
        c.max = 1;
    }
    Some(c)
}

pub fn set_control(path: &str, name: &str, value: i64) -> Result<(), String> {
    let spec = format!("{name}={value}");
    let out = run("v4l2-ctl", &["-d", path, "--set-ctrl", &spec], T);
    if out.ok {
        Ok(())
    } else {
        Err(last_line(&out.text))
    }
}
