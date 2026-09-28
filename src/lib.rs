//! Hardware support for the Lenovo ThinkSmart Hub 500 running Debian.
//!
//! One module per piece of hardware, extracted from `jethac/thinksmart-hub-tester`
//! so that every app on this panel shares one implementation of the quirks
//! rather than rediscovering them. The quirks are the point: this hardware has
//! several failure modes that are not obvious and are expensive to relearn.
//!
//! - [`display`] Panel brightness and power over DDC/CI. There is no
//!   `/sys/class/backlight`; the bus number is not fixed and must be discovered.
//! - [`prox`] The PIR sensor. Reads are HID round trips that sometimes never
//!   answer, leaving a task in uninterruptible sleep that cannot be killed. Every
//!   access here is a child process with a deadline for exactly that reason.
//! - [`led`] The LED ring, a telephony HID interface on the audio device.
//! - [`audio`] Speaker output. Hardware volume does nothing -- a Harman DSP
//!   ignores it -- so PipeWire software volume is the only control.
//! - [`mic`] The microphone array.
//! - [`media`] Which audio devices exist and which one is in use. The default
//!   source is not always the microphone -- on most of these units WirePlumber
//!   had picked the HDMI capture card instead.
//! - [`video`] Capture devices through V4L2: the built-in HDMI card, and a USB
//!   webcam where one is fitted. Enumeration and writes go through a worker, so
//!   a slow camera cannot stall whoever asked.
//! - [`preview`] Live frames from one of those, for a preview pane. YUYV and
//!   MJPEG, because the two devices here each offer only one of them.
//! - [`hid`] Read-only raw HID capture, for the undocumented devices.
//! - [`inventory`] A one-shot description of the hardware.
//! - [`util`] Process and file helpers every backend shares, including the
//!   bounded-child-process primitive the sensor code depends on.
//!
//! Hardware facts and their provenance live in
//! `jethac/thinksmart-hub-custom` (`docs/hardware.md`, `docs/peripherals.md`,
//! `docs/verified.md`). This crate turns them into code.

pub mod audio;
pub mod display;
pub mod hid;
pub mod inventory;
pub mod led;
pub mod media;
pub mod mic;
pub mod preview;
pub mod prox;
pub mod video;
pub mod util;
