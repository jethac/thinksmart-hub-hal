# thinksmart-hub-hal

Hardware support for the **Lenovo ThinkSmart Hub 500** running Debian 13. One
Rust crate, one module per piece of hardware, no UI and no policy.

Extracted from a test harness for the same machine so that every app on this
panel shares one implementation of the quirks instead of rediscovering them. **The quirks are the reason this crate exists.** The
hardware has several failure modes that are not obvious, and two of them can
hang a task in the kernel that no signal will clear.

Every hardware fact below was measured on these units rather than taken from a
datasheet, because there is no datasheet. Where a number appears — a timing, an
entrypoint count, a USB id — it came off one of these machines.

## Modules

| Module | Hardware | The quirk it exists to hide |
|---|---|---|
| `display` | Panel brightness and power over DDC/CI | No `/sys/class/backlight`. VCP `0x10` brightness, `0xD6` power. The bus number is not fixed — discover it, never hardcode `i2c-5`. `ddcutil` is slow and dislikes being hammered, so calls are cached and serialised |
| `prox` | PIR proximity sensor (`17ef:60c0`) | Reads are HID get-feature round trips the device sometimes never answers. Every access is a **child process with a deadline**, because a thread stuck in `usbhid_wait_io` is in uninterruptible sleep and cannot be cancelled |
| `led` | LED ring, a telephony HID interface on the audio device | White/green/red firmware presets only, not RGB. There is no "off" |
| `audio` | Speaker output via PipeWire | Hardware volume does nothing: a Harman DSP ignores USB Audio Class volume requests, so the output is pinned at maximum. Software volume is the only control |
| `mic` | Dual omni microphone array | Whether Linux sees two usable capsules or a pre-mixed pair is still an open question |
| `hid` | Read-only raw HID capture | For the undocumented devices, notably `17ef:60ce` |
| `media` | Audio devices through PipeWire | WirePlumber scores the HDMI capture card's audio like an ordinary microphone and will make it the default **source**. On two of three units here it had. Every app asking for "the microphone" then gets the HDMI input, which answers — with silence |
| `video` | Capture devices through V4L2 | Every device presents two `/dev/video*` nodes; the metadata one enumerates no capture formats, which is how to tell them apart. Controls carry V4L2's inactive flag, and an inactive control accepts writes and ignores them |
| `preview` | Live frames from a capture device | See *Video decode* below. Also: `v4l2-ctl` writes progress markers to **stdout**, interleaved with the frames, unless given `--silent` |
| `vaapi` | What the GPU can decode and encode | See *Video decode* below |
| `screen` | Screenshots of the panel itself | These units have no keyboard and are worked on over ssh. "What does it actually look like" is otherwise unanswerable |
| `inventory` | One-shot hardware description | Cheap to regenerate; never polled |
| `util` | Shared process and file helpers | Includes the bounded-child-process primitive everything above depends on |

## The proximity sensor, specifically

If you use one thing from this crate, it is `prox`, and it is worth knowing why
it is shaped the way it is.

- Reads can block **uninterruptibly** in `usbhid_wait_io` for tens of seconds.
  A task in that state ignores `SIGKILL`, holds the sysfs attribute so every
  other reader queues behind it, and makes its whole process unkillable.
- So every access forks. `timeout N cat` can be abandoned; a thread cannot.
- The sensor is **inert until activated**: the sampling rate is 0 after every
  boot and every reset, and at 0 the raw value is a permanent `0` —
  indistinguishable from "nobody there". Never believe raw until you have
  written a rate yourself.
- The costs are not small. Measured on hub-001: writing the rate ~10 s, the
  first raw read after activation ~21 s, every read after that ~100 ms.
- Runtime PM must be pinned **on the HID-SENSOR platform device**, not the USB
  device. The callbacks that hang belong to `hid_sensor_trigger`, so pinning
  `17ef:60c0` alone is not enough. `jethac/thinksmart-fleet` ships the udev rule.
- Recovering a wedged sensor needs a USB reset: write `0` then `1` to the port's
  `authorized`, found by vendor/product rather than a fixed path.

## Video decode, and a driver trap

The GPU is an **Intel HD Graphics 630** (`8086:5912`, Kaby Lake GT2, `i915`,
`/dev/dri/renderD128`). With `intel-media-va-driver-non-free` it reports:

```
decode   MPEG-2, H.264 Main/High/ConstrainedBaseline, JPEG Baseline,
         VP8, VP9 profile 0 and 2, HEVC Main and Main10
encode   H.264 (EncSlice and the low-power EncSliceLP), JPEG
```

**There is no VP8 or VP9 encode.** Decode for both, encode for neither. Anything
that encodes video here has to be told to use H.264, and told explicitly: a
stack that negotiates VP8 will fall back to encoding on four 2.7 GHz cores while
the hardware encoder sits idle, and the symptom is a machine that runs hot
rather than an error anybody can act on.

### The driver trap

Debian's **free** `intel-media-va-driver` has no video post-processing on this
chip: 15 entrypoints, no `VAEntrypointVideoProc`. The DFSG repack strips Intel's
Gen9 post-processing kernels (`igvpkrn_g9.c` and its ISA twin) because they are
source-less binaries, and VPP on this generation is entirely shader-based, so
removing them removes the entrypoint. Decode survives, being fixed-function.

That leaves **decode without scaling, which is the worst combination available**,
because GPU decode alone is worth nothing here. Measured over 120 frames of
1080p MJPEG: software 3.19 s, hardware-decode-without-scaling 3.24 s. The entire
win is scaling *before* the frame is copied back — 345 KB instead of 3.1 MB —
and that needs VPP. With it: **7.39 ms a frame against 37.67**, 5.1x, which at
rate is the difference between needing 113% of a core to hold 30 fps and needing
22%.

So either install `intel-media-va-driver-non-free` (needs `non-free` in your apt
sources; the two intel-media packages **conflict**, so it is a swap) or keep the
legacy `i965-va-driver`, which has VPP and is what this crate used first. On the
same workload the two are indistinguishable — 251, 234, 196 and 306 ms for 120
frames, alternating — so the run-to-run spread is wider than the gap. This crate
now tries iHD first because i965 is frozen upstream at 2.4.1, and keeps i965 as
the fallback.

`LIBVA_DRIVER_NAME` is set **per child process**, never system-wide. Two
consumers on the same machine can want different drivers, and choosing one for
the whole system chooses it for all of them.

## Things this hardware will do to you

A list for anyone else who has bought one of these.

- **The PIR sensor can hang a task nothing can kill.** See above. This is the
  one that actually costs you a reboot.
- **`pgrep` will not find a process named 15+ characters.** The kernel truncates
  `comm` to 15, so a sixteen-character binary name is never found. `ps -C` and
  `pgrep -f` work. It fails silently and confidently.
- **The Wayland socket has a `.lock` beside it.** Picking `wayland-0.lock` gives
  you a `WAYLAND_DISPLAY` that looks entirely plausible and connects to nothing.
- **`/sys/class/drm/*/modes` lists this panel's mode twice.** Take the first
  line; it is the preferred one.
- **The compositor will let a client decorate itself.** Under `cage`, without
  `-d`, the panel runs 1920x1045 inside a 1920x1080 display: a titlebar's worth
  of height reserved and never painted. Invisible in every log; obvious in one
  screenshot.
- **The HDMI capture card never stops sending frames.** With nothing plugged in
  it sends Y=16 black, so "no signal" never fires and a preview pane just looks
  broken.
- **A capture device with no signal can block in the kernel.** Which is why
  everything external here is a child process with a deadline.

## Using it

It is not published. Depend on it by git:

```toml
[dependencies]
hub-hal = { git = "https://github.com/jethac/thinksmart-hub-hal.git", tag = "v0.9.0" }
```

Pin a tag rather than tracking `master`, so an app does not change hardware
behaviour underneath itself on an unrelated push.

## Build

```
cargo build --release
cargo test
```

Depends only on glibc at runtime, so building on an older distribution than the
Hub's Debian 13 is fine — which is what `thinksmart-fleet` does, building on the
control node and shipping the binary.

## Who this is for

Anyone else who has one of these.

The ThinkSmart Hub 500 was a Microsoft Teams Rooms appliance, which means a lot
of them are now on the secondhand market for very little: an i5-7500T, 11.6" of
1920x1080 touchscreen at roughly 190 PPI, a microphone array, a speakerphone, an
HDMI capture input and an LED ring, in one unit designed to sit on a desk and be
left on. It runs Debian perfectly well once you know where the bodies are
buried.

Knowing where they are buried is the whole value here. Most of this hardware
does not work the way its interfaces suggest: there is no backlight class, the
speaker ignores hardware volume, the proximity sensor can hang a task nothing
can kill, and the GPU driver that sounds newer is the one that cannot scale. None
of that is documented anywhere, and all of it was found by losing time to it.

So this crate is published in the hope that the next person to buy one does not
have to lose the same time. It is deliberately **only hardware** — no UI, no
policy, no opinion about what you build on top — so it should be usable whatever
you have in mind for yours.

It is not a general-purpose library and makes no attempt to be. It targets one
machine on Debian 13, it shells out to `ddcutil`, `v4l2-ctl`, `wpctl`, `vainfo`,
`ffmpeg` and `grim` rather than linking anything, and it will not build you a
cross-platform abstraction. On this hardware, a child process with a deadline is
the only thing that survives a driver that stops answering, and that shapes
everything else.

## License

MIT or Apache-2.0, at your option.
