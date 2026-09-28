//! Live frames from a capture device, for a preview pane.
//!
//! Frames come from a long-lived `v4l2-ctl --stream-mmap --stream-to=-` rather
//! than from V4L2 ioctls in this process, for the reason that shapes the rest of
//! this crate: a capture device with nothing plugged into it can block in the
//! kernel, and a UI thread that does that is a dead panel. A child can be killed;
//! a thread stuck in a driver read cannot.
//!
//! `--silent` is not optional. Without it v4l2-ctl writes progress markers to
//! stdout, interleaved with the frames, which corrupts the stream in a way that
//! looks like a decoder bug.
//!
//! Two pixel formats, because the two devices on this hardware disagree: the
//! built-in HDMI capture card offers YUYV only, and the OBSBOT webcam offers
//! MJPEG only. YUYV is sampled straight to the preview size, so a 1080p frame
//! costs one read per output pixel rather than two million conversions. MJPEG has
//! to be decoded whole before it can be scaled.
//!
//! Like [`crate::mic`], this runs only while something keeps asking. A wall panel
//! must not hold a camera open because someone once opened a settings page.

use crate::util::now_ms;
use serde::Serialize;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// How long a single `keep_alive` is good for. Longer than the UI's poll interval
/// by enough that a slow frame does not tear the preview down.
const KEEPALIVE_MS: u64 = 1_500;
/// Preview frames are for looking at, not for recording. This is the long edge.
const TARGET_WIDTH: u32 = 640;
/// Frame pacing. The pipe provides the backpressure -- v4l2-ctl blocks on write
/// while nothing is reading -- so this sets both the frame rate and the CPU cost.
const FRAME_INTERVAL: Duration = Duration::from_millis(160);
/// How long without a complete frame before saying so. The first open of a device
/// can legitimately produce nothing for a moment, which is why this is not
/// reported immediately.
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(5);
/// A frame that never completes must not grow without bound. Comfortably above a
/// 4K MJPEG frame and far below anything that matters.
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// One preview frame, packed RGB8 at preview size.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
    /// Increments per delivered frame, so a consumer can tell a new frame from
    /// the same one shown again.
    pub seq: u64,
}

#[derive(Clone, Default, Serialize)]
pub struct State {
    /// A child is streaming right now.
    pub running: bool,
    /// Streaming, but nothing has completed a frame for a while.
    pub no_signal: bool,
    /// Every pixel of the last frame was black.
    ///
    /// This is what the HDMI capture card does with nothing plugged into it: it
    /// does not stop delivering frames, it delivers Y=16 black ones, so
    /// `no_signal` never fires and a preview pane just looks broken. Worth
    /// surfacing, and worth surfacing honestly -- a genuinely black source is
    /// indistinguishable from no source, so a caller should say "no signal, or a
    /// black source" rather than pick one.
    pub dark: bool,
    pub frames: u64,
    pub error: Option<String>,
}

#[derive(Clone, Default, PartialEq)]
struct Target {
    path: String,
    /// Bumped to force a restart when the device's format changes underneath us.
    generation: u64,
}

struct Inner {
    target: Option<Target>,
    frame: Option<Frame>,
    state: State,
}

pub struct Preview {
    inner: Mutex<Inner>,
    wanted_until: AtomicU64,
}

impl Preview {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                target: None,
                frame: None,
                state: State::default(),
            }),
            wanted_until: AtomicU64::new(0),
        })
    }

    /// Say the preview is being looked at. Call it from whatever draws the pane;
    /// stopping calling it is how the device gets released.
    pub fn keep_alive(&self) {
        self.wanted_until.store(now_ms() + KEEPALIVE_MS, Ordering::Relaxed);
    }

    fn wanted(&self) -> bool {
        now_ms() < self.wanted_until.load(Ordering::Relaxed)
    }

    /// Point the preview at a device. A no-op if it is already the target, so this
    /// is safe to call from a poll.
    pub fn select(&self, path: &str) {
        let mut inner = self.inner.lock().unwrap();
        if inner.target.as_ref().is_some_and(|t| t.path == path) {
            return;
        }
        let generation = inner.target.as_ref().map(|t| t.generation).unwrap_or(0) + 1;
        inner.target = Some(Target { path: path.to_string(), generation });
        inner.frame = None;
    }

    /// Re-open the current device. Needed after a format change: the stream
    /// carries no headers, so frame size is read once at open and a device that
    /// changed resolution mid-stream would produce sheared frames forever.
    pub fn restart(&self) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(t) = inner.target.as_mut() {
            t.generation += 1;
        }
        inner.frame = None;
    }

    pub fn stop(&self) {
        self.wanted_until.store(0, Ordering::Relaxed);
        let mut inner = self.inner.lock().unwrap();
        inner.target = None;
        inner.frame = None;
    }

    pub fn frame(&self) -> Option<Frame> {
        self.inner.lock().unwrap().frame.clone()
    }

    pub fn state(&self) -> State {
        self.inner.lock().unwrap().state.clone()
    }

    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        thread::Builder::new()
            .name("hal-preview".into())
            .spawn(move || me.supervise())
            .expect("spawn the preview thread");
    }

    fn supervise(self: Arc<Self>) {
        loop {
            let target = {
                let inner = self.inner.lock().unwrap();
                inner.target.clone()
            };
            let Some(target) = target.filter(|_| self.wanted()) else {
                {
                    let mut inner = self.inner.lock().unwrap();
                    inner.state.running = false;
                }
                thread::sleep(Duration::from_millis(200));
                continue;
            };
            self.stream(&target);
        }
    }

    /// One child's worth of streaming, from open until it stops or the target
    /// changes.
    fn stream(self: &Arc<Self>, target: &Target) {
        // Format is read now rather than passed in: what the device is actually
        // set to is the only thing that describes the bytes about to arrive.
        let Some((fourcc, width, height)) = crate::video::current_format(&target.path) else {
            self.fail("could not read the device format");
            thread::sleep(Duration::from_secs(1));
            return;
        };

        let child = Command::new("v4l2-ctl")
            .args(["-d", &target.path, "--stream-mmap", "--stream-to=-", "--silent"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                self.fail(&format!("v4l2-ctl: {e}"));
                thread::sleep(Duration::from_secs(1));
                return;
            }
        };
        let Some(mut out) = child.stdout.take() else {
            let _ = child.kill();
            self.fail("v4l2-ctl produced no stdout");
            return;
        };

        {
            let mut inner = self.inner.lock().unwrap();
            inner.state.running = true;
            inner.state.no_signal = false;
            inner.state.error = None;
        }

        // The read below blocks, and a device with no signal never unblocks it.
        // So the thing that decides to stop is a separate thread with a clock:
        // killing the child is what makes the read return.
        let pid = child.id() as i32;
        let generation = target.generation;
        let path = target.path.clone();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let stop = stop.clone();
            let me = self.clone();
            let path = path.clone();
            // This is the thread that can actually stop things. The reader below
            // is blocked in read(), and a device with no signal never unblocks it
            // -- so the decision to give up has to be made elsewhere, and killing
            // the child is what makes that read return.
            //
            // It kills by pid because the supervisor owns the only Child handle.
            // Sharing that handle through a mutex the reader also wants would be a
            // worse trade than a pid.
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let keep = me.wanted()
                        && me
                            .inner
                            .lock()
                            .unwrap()
                            .target
                            .as_ref()
                            .is_some_and(|t| t.path == path && t.generation == generation);
                    if !keep {
                        // SIGTERM rather than SIGKILL: v4l2-ctl releases the
                        // device on the way out, and a UVC device left streaming
                        // wants a replug.
                        unsafe {
                            libc::kill(pid, libc::SIGTERM);
                        }
                        return;
                    }
                    thread::sleep(Duration::from_millis(150));
                }
            });
        }

        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = vec![0u8; 256 * 1024];
        let frame_bytes = (width as usize) * (height as usize) * 2;
        let mut last_frame = Instant::now();
        let mut seq = self.inner.lock().unwrap().state.frames;

        loop {
            // A second check, for the case where reads are completing: it exits
            // promptly instead of waiting on the watchdog's next tick. The
            // watchdog is what handles the case where they are not.
            let keep = self.wanted()
                && self
                    .inner
                    .lock()
                    .unwrap()
                    .target
                    .as_ref()
                    .is_some_and(|t| t.path == path && t.generation == generation);
            if !keep {
                break;
            }

            match out.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(_) => break,
            }

            let decoded = if fourcc == "MJPG" || fourcc == "JPEG" {
                take_jpeg(&mut buf).and_then(|jpeg| decode_jpeg(&jpeg))
            } else if fourcc == "YUYV" {
                take_raw(&mut buf, frame_bytes).map(|raw| yuyv_to_rgb(&raw, width, height))
            } else {
                // An unexpected format is worth saying rather than showing a
                // scrambled pane.
                self.fail(&format!("{fourcc} preview is not supported"));
                break;
            };

            if let Some((rgb, w, h)) = decoded {
                seq += 1;
                // 24 rather than 0: limited-range black is Y=16, and conversion
                // rounding puts it a little either side.
                let dark = rgb.iter().all(|&b| b <= 24);
                let mut inner = self.inner.lock().unwrap();
                inner.state.dark = dark;
                inner.frame = Some(Frame { width: w, height: h, rgb, seq });
                inner.state.frames = seq;
                inner.state.no_signal = false;
                last_frame = Instant::now();
                drop(inner);
                // Paced here rather than by asking the device for a lower rate:
                // the rate belongs to whatever the user picked, and a preview
                // should not change it.
                thread::sleep(FRAME_INTERVAL);
            } else if last_frame.elapsed() > SIGNAL_TIMEOUT {
                self.inner.lock().unwrap().state.no_signal = true;
            }

            if buf.len() > MAX_FRAME_BYTES {
                // Nothing in this buffer is going to resynchronise. Start over.
                buf.clear();
            }
        }

        stop.store(true, Ordering::Relaxed);
        let _ = child.kill();
        let _ = child.wait();
        let mut inner = self.inner.lock().unwrap();
        inner.state.running = false;
    }

    fn fail(&self, msg: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.state.running = false;
        inner.state.error = Some(msg.to_string());
    }
}

/// One fixed-size frame, removed from the front of the buffer.
fn take_raw(buf: &mut Vec<u8>, frame_bytes: usize) -> Option<Vec<u8>> {
    if frame_bytes == 0 || buf.len() < frame_bytes {
        return None;
    }
    let frame: Vec<u8> = buf.drain(..frame_bytes).collect();
    Some(frame)
}

/// One complete JPEG, removed from the front of the buffer.
///
/// The stream has no framing of its own, so frames are found by their markers:
/// SOI `FF D8` to EOI `FF D9`. Anything before the first SOI is junk from a
/// mid-frame start and is dropped.
fn take_jpeg(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    let soi = find(buf, &[0xFF, 0xD8])?;
    if soi > 0 {
        buf.drain(..soi);
    }
    // Search from 2 so the SOI itself cannot be read as an EOI.
    let eoi = find(&buf[2..], &[0xFF, 0xD9])? + 2;
    let end = eoi + 2;
    let frame: Vec<u8> = buf.drain(..end).collect();
    Some(frame)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn decode_jpeg(jpeg: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let mut decoder = zune_jpeg::JpegDecoder::new(jpeg);
    let pixels = decoder.decode().ok()?;
    let (w, h) = decoder.dimensions()?;
    let (w, h) = (w as u32, h as u32);
    Some(scale_rgb(&pixels, w, h))
}

/// YUYV 4:2:2 straight to RGB at preview size.
///
/// Sampled rather than converted: the output is a few hundred pixels across, so
/// this reads the pixels it needs instead of converting two million and throwing
/// most away. BT.709 coefficients, which is what the capture card reports
/// (Colorspace sRGB, Transfer Function Rec. 709).
fn yuyv_to_rgb(raw: &[u8], width: u32, height: u32) -> (Vec<u8>, u32, u32) {
    let (ow, oh) = preview_size(width, height);
    let stride = width as usize * 2;
    let mut rgb = vec![0u8; (ow * oh * 3) as usize];

    for oy in 0..oh {
        let sy = (oy as u64 * height as u64 / oh as u64) as usize;
        let row = sy * stride;
        for ox in 0..ow {
            let sx = (ox as u64 * width as u64 / ow as u64) as usize;
            // Each four bytes carry two pixels: Y0 U Y1 V.
            let pair = row + (sx / 2) * 4;
            if pair + 3 >= raw.len() {
                continue;
            }
            let y = raw[pair + if sx % 2 == 0 { 0 } else { 2 }] as f32;
            let u = raw[pair + 1] as f32 - 128.0;
            let v = raw[pair + 3] as f32 - 128.0;
            let o = ((oy * ow + ox) * 3) as usize;
            rgb[o] = clamp8(y + 1.5748 * v);
            rgb[o + 1] = clamp8(y - 0.1873 * u - 0.4681 * v);
            rgb[o + 2] = clamp8(y + 1.8556 * u);
        }
    }
    (rgb, ow, oh)
}

/// Nearest-neighbour downscale of packed RGB. Good enough for a preview and cheap
/// enough to do on the same thread as the read.
fn scale_rgb(src: &[u8], width: u32, height: u32) -> (Vec<u8>, u32, u32) {
    let (ow, oh) = preview_size(width, height);
    if (ow, oh) == (width, height) {
        return (src.to_vec(), width, height);
    }
    let mut out = vec![0u8; (ow * oh * 3) as usize];
    for oy in 0..oh {
        let sy = (oy as u64 * height as u64 / oh as u64) as usize;
        for ox in 0..ow {
            let sx = (ox as u64 * width as u64 / ow as u64) as usize;
            let s = (sy * width as usize + sx) * 3;
            let o = ((oy * ow + ox) * 3) as usize;
            if s + 2 < src.len() {
                out[o] = src[s];
                out[o + 1] = src[s + 1];
                out[o + 2] = src[s + 2];
            }
        }
    }
    (out, ow, oh)
}

fn preview_size(width: u32, height: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (1, 1);
    }
    if width <= TARGET_WIDTH {
        return (width, height);
    }
    let ow = TARGET_WIDTH;
    let oh = ((height as u64 * ow as u64) / width as u64).max(1) as u32;
    (ow, oh)
}

fn clamp8(v: f32) -> u8 {
    v.clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The YUYV path cannot be exercised from the webcam -- the OBSBOT offers
    /// MJPEG only -- and the capture card needs something plugged into it. So the
    /// conversion is checked against hand-built pixels instead: pure white, pure
    /// black, and a saturated red.
    #[test]
    fn yuyv_converts_known_pixels() {
        // Two pixels per four bytes: Y0 U Y1 V.
        let white = [235u8, 128, 235, 128];
        let (rgb, w, h) = yuyv_to_rgb(&white, 2, 1);
        assert_eq!((w, h), (2, 1));
        assert!(rgb[0] > 230 && rgb[1] > 230 && rgb[2] > 230, "white -> {rgb:?}");

        let black = [16u8, 128, 16, 128];
        let (rgb, _, _) = yuyv_to_rgb(&black, 2, 1);
        assert!(rgb[0] < 30 && rgb[1] < 30 && rgb[2] < 30, "black -> {rgb:?}");

        // Y mid, V high: red should dominate.
        let red = [128u8, 128, 128, 240];
        let (rgb, _, _) = yuyv_to_rgb(&red, 2, 1);
        assert!(rgb[0] > rgb[1] && rgb[0] > rgb[2], "red -> {rgb:?}");
    }

    #[test]
    fn jpeg_framing_finds_one_frame_and_drops_junk() {
        let mut buf = vec![0x00, 0x11, 0xFF, 0xD8, 0x01, 0x02, 0xFF, 0xD9, 0xFF, 0xD8, 0x03];
        let frame = take_jpeg(&mut buf).expect("a complete frame");
        assert_eq!(frame, vec![0xFF, 0xD8, 0x01, 0x02, 0xFF, 0xD9]);
        // What is left is the start of the next frame, not junk.
        assert_eq!(buf, vec![0xFF, 0xD8, 0x03]);
        assert!(take_jpeg(&mut buf).is_none(), "an incomplete frame must not be taken");
    }

    #[test]
    fn raw_framing_waits_for_a_whole_frame() {
        let mut buf = vec![1u8; 10];
        assert!(take_raw(&mut buf, 16).is_none());
        buf.extend_from_slice(&[2u8; 10]);
        let f = take_raw(&mut buf, 16).expect("a whole frame");
        assert_eq!(f.len(), 16);
        assert_eq!(buf.len(), 4);
    }
}
