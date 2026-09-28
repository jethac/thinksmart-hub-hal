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
//! MJPEG is decoded on the GPU where the hardware allows it, by handing the whole
//! job to an `ffmpeg` child instead of `v4l2-ctl` -- decode, scale and colour
//! conversion all happen before anything is copied back to system memory. That is
//! worth 3.4x the CPU at full rate, and the reason it is worth anything at all is
//! the scaling: see [`crate::vaapi::Capabilities::can_post_process`]. Where the
//! hardware cannot, the software path below is used unchanged, chosen at runtime
//! so a unit with a missing or broken driver still shows a picture.
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

    /// Start the watchdog that can stop a wedged child.
    ///
    /// The reader below blocks, and a device with no signal never unblocks it, so
    /// the decision to give up has to be made on another thread -- killing the
    /// child is what makes that read return. Shared by both streaming paths
    /// because the hazard is the same one either way: the process differs, the
    /// kernel read that will not come back does not.
    fn watch(self: &Arc<Self>, pid: i32, target: &Target) -> Arc<std::sync::atomic::AtomicBool> {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = stop.clone();
        let me = self.clone();
        let path = target.path.clone();
        let generation = target.generation;
        // Kills by pid because the supervisor owns the only Child handle, and
        // sharing that through a mutex the reader also wants would be a worse
        // trade than a pid.
        thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                let keep = me.wanted()
                    && me
                        .inner
                        .lock()
                        .unwrap()
                        .target
                        .as_ref()
                        .is_some_and(|t| t.path == path && t.generation == generation);
                if !keep {
                    // SIGTERM rather than SIGKILL: the child releases the device
                    // on the way out, and a UVC device left streaming wants a
                    // replug.
                    unsafe {
                        libc::kill(pid, libc::SIGTERM);
                    }
                    return;
                }
                thread::sleep(Duration::from_millis(150));
            }
        });
        stop
    }

    /// Whether this frame is still the one being asked for.
    fn still_wanted(&self, target: &Target) -> bool {
        self.wanted()
            && self
                .inner
                .lock()
                .unwrap()
                .target
                .as_ref()
                .is_some_and(|t| t.path == target.path && t.generation == target.generation)
    }

    /// Record a decoded frame.
    fn deliver(&self, rgb: Vec<u8>, w: u32, h: u32, seq: &mut u64) {
        *seq += 1;
        // 24 rather than 0: limited-range black is Y=16, and conversion rounding
        // puts it a little either side.
        let dark = rgb.iter().all(|&b| b <= 24);
        let mut inner = self.inner.lock().unwrap();
        inner.state.dark = dark;
        inner.frame = Some(Frame { width: w, height: h, rgb, seq: *seq });
        inner.state.frames = *seq;
        inner.state.no_signal = false;
    }

    /// MJPEG through the GPU: decode, scale and convert before anything crosses
    /// back to system memory.
    ///
    /// Everything the expensive way would be done on the CPU is in the filter
    /// chain. `scale_vaapi` is the part that matters -- without it a full-size
    /// frame has to be downloaded and the transfer costs what the decode saved.
    /// The output is NV12 at preview size, 345 KB rather than 3.1 MB.
    ///
    /// Frames are fixed-size here, which is simpler than the software path: there
    /// is no JPEG framing to do, because ffmpeg has already done it.
    fn stream_accelerated(
        self: &Arc<Self>,
        target: &Target,
        driver: &str,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        let (ow, oh) = preview_size(width, height);
        let filter = format!("scale_vaapi=w={ow}:h={oh},hwdownload,format=nv12");
        let size = format!("{width}x{height}");

        let mut cmd = Command::new("ffmpeg");
        cmd.args([
            "-hide_banner",
            "-loglevel", "error",
            "-hwaccel", "vaapi",
            "-hwaccel_device", crate::vaapi::RENDER_NODE,
            "-hwaccel_output_format", "vaapi",
            "-f", "v4l2",
            "-input_format", "mjpeg",
            "-video_size", &size,
            "-i", &target.path,
            "-vf", &filter,
            "-pix_fmt", "nv12",
            "-f", "rawvideo",
            "-",
        ]);
        if !driver.is_empty() {
            // Per child, never for the whole system: a browser encoding a call
            // later may prefer a different driver, and choosing one here must not
            // choose it everywhere.
            cmd.env("LIBVA_DRIVER_NAME", driver);
        }
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("ffmpeg: {e}"))?;
        let Some(mut out) = child.stdout.take() else {
            let _ = child.kill();
            return Err("ffmpeg produced no stdout".into());
        };

        {
            let mut inner = self.inner.lock().unwrap();
            inner.state.running = true;
            inner.state.no_signal = false;
            inner.state.error = None;
        }
        let stop = self.watch(child.id() as i32, target);

        let frame_bytes = (ow as usize) * (oh as usize) * 3 / 2;
        let mut nv12 = vec![0u8; frame_bytes];
        let mut seq = self.inner.lock().unwrap().state.frames;
        let mut delivered = 0u64;

        // No no_signal timer here, unlike the software path, and its absence is
        // deliberate. That path reads whatever arrives and can tell a partial
        // frame from none; this one reads a fixed-size frame and blocks until it
        // is whole, so there is no in-between state to observe. A source that
        // stops simply blocks, and the watchdog is what ends it.
        while self.still_wanted(target) {
            match out.read_exact(&mut nv12) {
                Ok(()) => {
                    let rgb = nv12_to_rgb(&nv12, ow, oh);
                    self.deliver(rgb, ow, oh, &mut seq);
                    delivered += 1;
                    thread::sleep(FRAME_INTERVAL);
                }
                Err(_) => break,
            }
        }

        stop.store(true, Ordering::Relaxed);
        let _ = child.kill();
        let _ = child.wait();
        self.inner.lock().unwrap().state.running = false;

        // Nothing at all means ffmpeg could not do it -- a driver that probed
        // well but cannot actually stream, a filter that failed to configure.
        // Reported so the caller falls back rather than showing a dead pane.
        if delivered == 0 {
            return Err("ffmpeg delivered no frames".into());
        }
        Ok(())
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

        // Hardware first for MJPEG, when a driver exists that can both decode it
        // and post-process. Decided here rather than at build time: the same
        // binary runs on units whose driver may be missing or broken, and one of
        // those must still show a picture.
        if fourcc == "MJPG" || fourcc == "JPEG" {
            if let Some(accel) = crate::vaapi::accelerator_for(crate::vaapi::Codec::Jpeg) {
                match self.stream_accelerated(target, &accel.driver_name, width, height) {
                    Ok(()) => return,
                    Err(e) => {
                        // Falls through to software. Worth a line: a silent
                        // downgrade to three times the CPU is the kind of thing
                        // that is only ever noticed as a warm room.
                        eprintln!("hub-hal: hardware decode unavailable ({e}); using software");
                    }
                }
            }
        }

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

/// MJPEG to RGB at preview size, in software.
///
/// In software deliberately, and this is the record of why so nobody re-runs the
/// experiment. The GPU here has a JPEG Baseline decoder -- [`crate::vaapi`] will
/// tell you so -- and using it for this is not worth what it costs.
///
/// Measured on hub-002 with a real 1080p frame from the OBSBOT, 40 iterations:
///
/// ```text
/// decode to RGB then downscale (this)      12.17 ms   of which decode is 10.91
/// decode to YCbCr, convert only kept px    14.27 ms   +17%, worse
/// jpeg-decoder at 1/2 scale (960x540)      11.21 ms   -3%
/// jpeg-decoder at 1/4 scale (480x270)       8.82 ms   -23%, but below preview size
/// ```
///
/// Two things fall out of that. The downscale costs 1.26 ms, so restructuring
/// around it cannot help -- 90% of the time is the decode. And decoding at
/// reduced DCT scale, which should have been the obvious win, is worth 3% at a
/// size we can actually use, because jpeg-decoder is a slower decoder to begin
/// with and its scaling gain pays for that rather than for us. Converting only
/// the pixels the preview keeps is slower still: zune's colour conversion is
/// vectorised and a scalar loop over the sampled pixels is not.
///
/// So software has no headroom, and hardware is the only real lever. At preview
/// size it is not a lever worth pulling: 5 frames a second at 12 ms is 6% of one
/// core of four, and the optimistic hardware figure saves maybe 8 ms of that --
/// about 1% of the machine. Against that, the `libva` crate cannot be built on
/// the control node (bindgen wants libclang and the VA headers, neither is
/// installed and installing them needs a password nobody types), so it would mean
/// hand-written unsafe FFI through `dlopen` against a C ABI with no headers
/// available to check the struct layouts against, on units that hang on walls and
/// are recovered by walking to them.
///
/// Where it does become worth it is rate, not size. The same 12 ms is 36% of a
/// core at 30 fps and 73% at 60, and a call needs the encoder as well -- which on
/// this chip is H.264 only, since it has no VP8 or VP9 encode at all. That work
/// wants `libva-dev` and `libclang` on whatever builds the binary; `libva2` and
/// `libva-drm2` are already on the hubs, pulled in by the driver.
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

/// NV12 to packed RGB, at preview size.
///
/// Cheap because it is already small: the GPU scaled before the download, so this
/// is a few hundred pixels across rather than two million.
///
/// BT.601 with full-range luma, which is what this webcam reports -- ffmpeg shows
/// the stream as `csp:bt470bg range:pc`. Deliberately different from the YUYV path
/// above, which uses BT.709 because the capture card reports Rec. 709. Two devices,
/// two colorimetries, and using one matrix for both would tint one of them.
fn nv12_to_rgb(nv12: &[u8], width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let y_plane = w * h;
    let mut rgb = vec![0u8; y_plane * 3];
    if nv12.len() < y_plane + y_plane / 2 {
        return rgb;
    }
    for y in 0..h {
        for x in 0..w {
            let luma = nv12[y * w + x] as f32;
            // One chroma pair per 2x2 block of luma, interleaved U then V.
            let c = y_plane + (y / 2) * w + (x / 2) * 2;
            let u = nv12[c] as f32 - 128.0;
            let v = nv12[c + 1] as f32 - 128.0;
            let o = (y * w + x) * 3;
            rgb[o] = clamp8(luma + 1.402 * v);
            rgb[o + 1] = clamp8(luma - 0.344136 * u - 0.714136 * v);
            rgb[o + 2] = clamp8(luma + 1.772 * u);
        }
    }
    rgb
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

    /// The NV12 path cannot be exercised without a camera and a GPU, so the
    /// conversion is checked against hand-built planes the same way YUYV is.
    #[test]
    fn nv12_converts_known_pixels() {
        // 2x2, one chroma pair. Full-range BT.601: neutral chroma is 128.
        let white = [255u8, 255, 255, 255, 128, 128];
        let rgb = nv12_to_rgb(&white, 2, 2);
        assert!(rgb[0] > 250 && rgb[1] > 250 && rgb[2] > 250, "white -> {:?}", &rgb[..3]);

        let black = [0u8, 0, 0, 0, 128, 128];
        let rgb = nv12_to_rgb(&black, 2, 2);
        assert!(rgb[0] < 5 && rgb[1] < 5 && rgb[2] < 5, "black -> {:?}", &rgb[..3]);

        // V high: red dominates.
        let red = [128u8, 128, 128, 128, 128, 240];
        let rgb = nv12_to_rgb(&red, 2, 2);
        assert!(rgb[0] > rgb[1] && rgb[0] > rgb[2], "red -> {:?}", &rgb[..3]);
    }

    #[test]
    fn a_short_nv12_frame_does_not_panic() {
        // A truncated read must produce a black frame rather than an index out
        // of bounds on a wall-mounted device.
        let rgb = nv12_to_rgb(&[0u8; 3], 4, 4);
        assert_eq!(rgb.len(), 4 * 4 * 3);
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
