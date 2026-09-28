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
//!
//! # Two layers
//!
//! The free functions below are the primitives: each one runs a `v4l2-ctl` and
//! blocks until it answers. They are correct and they are the wrong thing to
//! call from a UI thread, because "until it answers" is not bounded by anything
//! a caller controls -- enumerating four nodes is four process spawns and the
//! device gets a say in how long each takes.
//!
//! So [`Video`] wraps them the way [`crate::media`], [`crate::mic`] and
//! [`crate::preview`] wrap theirs: a worker thread does the talking, callers get
//! a cheap [`State`] snapshot, and writes are queued rather than performed. That
//! is the shape every other module in this crate already had, and `video` was
//! the one that did not -- which is how a slow camera could stall the first
//! window or freeze a touch.
//!
//! A queued write carries the device path it was issued against, and the worker
//! drops it if the selection has moved on by the time it runs. Applying a
//! brightness meant for the webcam to the capture card instead would be worse
//! than doing nothing.

use crate::util::{last_line, read_trim, run};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
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

/// Which of the two devices a call is about.
///
/// They are addressed by role rather than by path because that is how the panel
/// thinks about them: one is "the camera", whichever camera that currently is,
/// and the other is the HDMI input that every unit has.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
pub enum Target {
    Camera,
    Capture,
}

/// Everything read from one device, as of one moment.
///
/// `path` is not decoration: it is how a caller confirms that what it is holding
/// describes the device it thinks is selected, rather than the one that was
/// selected when the read started.
#[derive(Clone, Default, Serialize)]
pub struct Reading {
    pub path: String,
    /// What the device is set to right now: fourcc, width, height.
    pub format: Option<(String, u32, u32)>,
    /// Which entry of the device's `formats` that corresponds to, if any. V4L2
    /// can be in a mode it does not advertise, so this really can be None while
    /// `format` is Some.
    pub mode: Option<usize>,
    pub controls: Vec<Control>,
}

#[derive(Clone, Default, Serialize)]
pub struct State {
    /// Capture devices that are NOT the built-in HDMI card, in `/dev/videoN`
    /// order. Empty on a unit with no webcam, which is two of the three here.
    pub cameras: Vec<Device>,
    /// Index into `cameras` of the selected one.
    pub camera: Option<usize>,
    /// What was read from the selected camera.
    pub camera_reading: Reading,
    /// The built-in HDMI capture card.
    pub capture: Option<Device>,
    /// What was read from the capture card.
    pub capture_reading: Reading,
    /// Bumped every time a refresh completes. A consumer that rebuilds UI models
    /// from this should do it when the generation changes and not otherwise:
    /// rebuilding a model on a timer resets a control under whoever is touching
    /// it.
    pub generation: u64,
    pub error: Option<String>,
}

enum Job {
    Refresh,
    SelectCamera(usize),
    SetFormat { target: Target, path: String, mode: usize },
    SetControl { target: Target, path: String, name: String, value: i64 },
}

struct Inner {
    state: State,
    /// The camera the worker is tracking, by path rather than by index: indices
    /// move when a device is plugged in or out, and a selection that silently
    /// becomes a different camera is worse than one that is lost.
    selected: Option<String>,
}

pub struct Video {
    inner: Mutex<Inner>,
    tx: Mutex<Option<Sender<Job>>>,
}

impl Video {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner { state: State::default(), selected: None }),
            tx: Mutex::new(None),
        })
    }

    /// Returns immediately. The first enumeration happens on the worker, so
    /// nothing here is on the path to showing the first window.
    pub fn start(self: &Arc<Self>) {
        let (tx, rx) = channel();
        *self.tx.lock().unwrap() = Some(tx);
        let me = self.clone();
        thread::Builder::new()
            .name("hal-video".into())
            .spawn(move || me.work(rx))
            .expect("spawn the video thread");
    }

    pub fn state(&self) -> State {
        self.inner.lock().unwrap().state.clone()
    }

    /// Ask for a fresh enumeration. Cheap and non-blocking; the answer turns up
    /// in [`State`] with a new `generation`.
    pub fn refresh(&self) {
        self.send(Job::Refresh);
    }

    pub fn select_camera(&self, index: usize) {
        self.send(Job::SelectCamera(index));
    }

    /// Ask the device for one of the modes in its `formats`.
    ///
    /// The path is stamped here, on the caller's thread, from the selection as
    /// it stands right now. That is what lets the worker tell a write meant for
    /// this device from one meant for whatever was selected a moment ago.
    pub fn set_format(&self, target: Target, mode: usize) {
        let Some(path) = self.path_of(target) else { return };
        self.send(Job::SetFormat { target, path, mode });
    }

    pub fn set_control(&self, target: Target, name: &str, value: i64) {
        let Some(path) = self.path_of(target) else { return };
        self.send(Job::SetControl { target, path, name: name.to_string(), value });
    }

    fn send(&self, job: Job) {
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            // A closed channel means the worker is gone, which on this panel
            // means the process is going down. Nothing useful to say about it.
            let _ = tx.send(job);
        }
    }

    fn path_of(&self, target: Target) -> Option<String> {
        let inner = self.inner.lock().unwrap();
        let reading = match target {
            Target::Camera => &inner.state.camera_reading,
            Target::Capture => &inner.state.capture_reading,
        };
        (!reading.path.is_empty()).then(|| reading.path.clone())
    }

    fn work(self: Arc<Self>, rx: Receiver<Job>) {
        // Before waiting for anything to ask: the panel wants a device list as
        // soon as there is one, and this is off the UI thread now.
        self.enumerate();
        while let Ok(job) = rx.recv() {
            match job {
                Job::Refresh => self.enumerate(),
                Job::SelectCamera(index) => {
                    {
                        let mut inner = self.inner.lock().unwrap();
                        let path = inner.state.cameras.get(index).map(|c| c.path.clone());
                        if path.is_none() {
                            continue;
                        }
                        inner.selected = path;
                    }
                    self.enumerate();
                }
                Job::SetFormat { target, path, mode } => {
                    if !self.still_selected(target, &path) {
                        continue;
                    }
                    let fmt = self.format_at(target, mode);
                    if let Some(f) = fmt {
                        if let Err(e) = set_format(&path, &f.fourcc, f.width, f.height) {
                            eprintln!("hub-hal: {path} format: {e}");
                        }
                    }
                    self.reread(target, &path);
                }
                Job::SetControl { target, path, name, value } => {
                    if !self.still_selected(target, &path) {
                        continue;
                    }
                    if let Err(e) = set_control(&path, &name, value) {
                        eprintln!("hub-hal: {path} {name}: {e}");
                    }
                    self.reread(target, &path);
                }
            }
        }
    }

    /// Whether the device a queued write was issued against is still the one
    /// that role points at.
    fn still_selected(&self, target: Target, path: &str) -> bool {
        self.path_of(target).as_deref() == Some(path)
    }

    fn format_at(&self, target: Target, mode: usize) -> Option<Format> {
        let inner = self.inner.lock().unwrap();
        let device = match target {
            Target::Camera => inner.state.camera.and_then(|i| inner.state.cameras.get(i)),
            Target::Capture => inner.state.capture.as_ref(),
        }?;
        device.formats.get(mode).cloned()
    }

    /// Re-read one device after writing to it.
    ///
    /// The whole set of controls, not the one that was written: V4L2 controls own
    /// each other, so turning autofocus on makes focus_absolute inactive and the
    /// UI has to follow. The format is re-read for the same reason in reverse --
    /// V4L2 answers a set-format request with something it can do, which is not
    /// always what was asked for.
    fn reread(&self, target: Target, path: &str) {
        let formats = {
            let inner = self.inner.lock().unwrap();
            match target {
                Target::Camera => inner
                    .state
                    .camera
                    .and_then(|i| inner.state.cameras.get(i))
                    .map(|d| d.formats.clone()),
                Target::Capture => inner.state.capture.as_ref().map(|d| d.formats.clone()),
            }
        };
        let Some(formats) = formats else { return };
        let reading = read_device(path, &formats);

        let mut inner = self.inner.lock().unwrap();
        // Selection can have moved while the reads were running. Publishing this
        // would describe one device under another's name.
        let current = match target {
            Target::Camera => &inner.state.camera_reading,
            Target::Capture => &inner.state.capture_reading,
        };
        if current.path != path {
            return;
        }
        match target {
            Target::Camera => inner.state.camera_reading = reading,
            Target::Capture => inner.state.capture_reading = reading,
        }
        inner.state.generation += 1;
    }

    /// One full pass: what is plugged in, and everything about the two devices
    /// that matter.
    fn enumerate(&self) {
        let all = devices();
        let (capture, cameras): (Vec<Device>, Vec<Device>) =
            all.into_iter().partition(|d| d.is_capture_card);
        let capture = capture.into_iter().next();

        // Keep the selection across a re-enumeration where the device is still
        // there. Fall back to the first camera rather than to nothing, so a unit
        // with exactly one webcam never needs anyone to pick it.
        let wanted = self.inner.lock().unwrap().selected.clone();
        let index = wanted
            .as_deref()
            .and_then(|p| cameras.iter().position(|c| c.path == p))
            .or_else(|| (!cameras.is_empty()).then_some(0));

        let camera_reading = match index.and_then(|i| cameras.get(i)) {
            Some(c) => read_device(&c.path, &c.formats),
            None => Reading::default(),
        };
        let capture_reading = match &capture {
            Some(c) => read_device(&c.path, &c.formats),
            None => Reading::default(),
        };

        let mut inner = self.inner.lock().unwrap();
        inner.selected = index.and_then(|i| cameras.get(i)).map(|c| c.path.clone());
        inner.state.cameras = cameras;
        inner.state.camera = index;
        inner.state.camera_reading = camera_reading;
        inner.state.capture = capture;
        inner.state.capture_reading = capture_reading;
        inner.state.generation += 1;
        inner.state.error = None;
    }
}

fn read_device(path: &str, formats: &[Format]) -> Reading {
    let format = current_format(path);
    let mode = format.as_ref().and_then(|f| mode_index(formats, f));
    Reading { path: path.to_string(), format, mode, controls: controls(path) }
}

/// Which entry of `formats` a current format corresponds to.
///
/// Matched on size and fourcc rather than assumed to be the first: V4L2 answers
/// a set-format request with something it can do, which is not always what was
/// asked for, and a picker showing a mode the device is not in would be a lie.
pub fn mode_index(formats: &[Format], current: &(String, u32, u32)) -> Option<usize> {
    let (fourcc, w, h) = current;
    formats
        .iter()
        .position(|f| f.width == *w && f.height == *h && f.fourcc == *fourcc)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(fourcc: &str, width: u32, height: u32) -> Format {
        Format { fourcc: fourcc.into(), width, height, fps: vec![] }
    }

    /// The picker shows a mode only if the device is really in it. A device in a
    /// size it does not advertise has to come back as None rather than as the
    /// first entry, which would put a tick next to a mode nothing is using.
    #[test]
    fn mode_index_matches_size_and_fourcc() {
        let formats = [fmt("MJPG", 1920, 1080), fmt("YUYV", 1920, 1080), fmt("MJPG", 1280, 720)];
        assert_eq!(mode_index(&formats, &("YUYV".into(), 1920, 1080)), Some(1));
        assert_eq!(mode_index(&formats, &("MJPG".into(), 1280, 720)), Some(2));
        // Right size, format the device does not list here.
        assert_eq!(mode_index(&formats, &("NV12".into(), 1920, 1080)), None);
        // Right format, size it does not list.
        assert_eq!(mode_index(&formats, &("MJPG".into(), 640, 480)), None);
    }

    /// Controls own each other, and the flag saying so has to survive parsing --
    /// an inactive control accepts writes and ignores them, so drawing it as live
    /// would be a lie.
    #[test]
    fn parses_an_inactive_control() {
        let line = "     exposure_time_absolute 0x009a0902 (int)    : min=1 max=2500 step=1 default=330 value=330 flags=inactive";
        let c = parse_control(line).expect("a control");
        assert_eq!(c.name, "exposure_time_absolute");
        assert_eq!(c.kind, "int");
        assert_eq!((c.min, c.max, c.value), (1, 2500, 330));
        assert!(!c.active);
    }

    /// bool controls list no min or max. Leaving the range at 0..0 would give a
    /// switch nothing to be.
    #[test]
    fn bool_controls_get_a_range() {
        let line = "        white_balance_automatic 0x0098090c (bool)   : default=1 value=1";
        let c = parse_control(line).expect("a control");
        assert_eq!(c.kind, "bool");
        assert_eq!((c.min, c.max), (0, 1));
        assert!(c.active);
    }
}
