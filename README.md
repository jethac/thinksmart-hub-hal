# thinksmart-hub-hal

Hardware support for the **Lenovo ThinkSmart Hub 500** on Debian 13. One Rust
crate, one module per device, no UI and no policy.

These were Teams Rooms appliances and are cheap secondhand: i5-7500T, 11.6"
1920x1080 touchscreen at ~190 PPI, mic array, speakerphone, HDMI capture input,
LED ring. Debian runs fine on them. Much of the hardware does not behave the way
its interfaces suggest.

Every number below was measured on these units. There is no datasheet.

## Modules

| Module | Hardware | Quirk |
|---|---|---|
| `display` | Brightness and power over DDC/CI | No `/sys/class/backlight`. VCP `0x10` brightness, `0xD6` power. Bus number is not fixed; discover it. `ddcutil` is slow, so calls are cached and serialised |
| `prox` | PIR sensor (`17ef:60c0`) | Can block uninterruptibly. See below |
| `led` | LED ring, telephony HID on the audio device | White/green/red firmware presets, not RGB. No "off" |
| `audio` | Speaker via PipeWire | A Harman DSP ignores UAC volume requests. Software volume is the only control |
| `mic` | Dual omni mic array | Unresolved whether Linux sees two capsules or a pre-mixed pair |
| `media` | Audio devices via PipeWire | WirePlumber scores the HDMI capture card's audio like a microphone and may make it the default source. It then answers "the microphone" with silence |
| `video` | Capture devices via V4L2 | Each device presents two `/dev/video*` nodes; the metadata one enumerates no capture formats. Controls carry V4L2's inactive flag, and inactive controls accept writes and ignore them |
| `preview` | Live frames | `v4l2-ctl` writes progress markers to stdout, interleaved with frames, without `--silent` |
| `vaapi` | GPU decode/encode capability | See below |
| `screen` | Screenshots, via `grim` | These have no keyboard; this is how you see the screen over ssh |
| `bluetooth` | The radio, nearby devices, pairing | The adapter is a combo part and comes up on its own, but nothing in userspace is installed by default. A discovery session dies with the D-Bus connection that started it, so scanning cannot be a one-shot |
| `hid` | Raw HID capture | For undocumented devices, notably `17ef:60ce` |
| `inventory` | One-shot hardware description | |
| `util` | Bounded-child-process primitive | Everything external goes through it |

## The proximity sensor

- Reads block **uninterruptibly** in `usbhid_wait_io` for tens of seconds. Such
  a task ignores `SIGKILL`, holds the sysfs attribute so other readers queue
  behind it, and makes its process unkillable. Every access therefore forks:
  `timeout N cat` can be abandoned, a thread cannot.
- **Inert until activated.** Sampling rate is 0 after boot and after reset, and
  at 0 the raw value is a permanent `0`, indistinguishable from nobody there.
  Never trust raw until you have written a rate.
- Costs: writing the rate ~10 s, first read after activation ~21 s, subsequent
  reads ~100 ms.
- Runtime PM must be pinned on the **HID-SENSOR platform device**, not the USB
  device. The callbacks that hang belong to `hid_sensor_trigger`.
- Recover a wedged sensor with a USB reset: write `0` then `1` to the port's
  `authorized`, located by vendor/product rather than a fixed path.

## GPU

**Intel HD Graphics 630** (`8086:5912`, Kaby Lake GT2, `i915`,
`/dev/dri/renderD128`). With `intel-media-va-driver-non-free`:

```
decode   MPEG-2, H.264 Main/High/ConstrainedBaseline, JPEG Baseline,
         VP8, VP9 profile 0 and 2, HEVC Main and Main10
encode   H.264 (EncSlice and EncSliceLP), JPEG
```

**No VP8 or VP9 encode.** Anything encoding video here must be told to use
H.264 explicitly, or it falls back to the CPU while the hardware encoder idles.

### Driver

Debian's free `intel-media-va-driver` has **no video post-processing** on this
chip: 15 entrypoints, no `VAEntrypointVideoProc`. The DFSG repack strips Intel's
Gen9 post-processing kernels (`igvpkrn_g9.c`) as source-less binaries, and VPP
here is entirely shader-based. Decode survives, being fixed-function.

Decode without scaling is worthless. Over 120 frames of 1080p MJPEG:

| | |
|---|---|
| software | 3.19 s |
| hardware decode, no GPU scaling | 3.24 s |
| hardware decode, GPU scaling | 0.93 s |

The win is scaling before the copy back: 345 KB instead of 3.1 MB. In-crate
that is 7.39 ms/frame against 37.67. At rate, 22% of a core for 30 fps instead
of 113%.

Use `intel-media-va-driver-non-free` (needs `non-free`; it **conflicts** with
the free package, so it is a swap) or the legacy `i965-va-driver`, which also
has VPP. On this workload they are indistinguishable: 251, 234, 196, 306 ms
alternating, so run-to-run spread exceeds the difference. This crate tries iHD
first because i965 is frozen upstream at 2.4.1, and falls back to i965.

`LIBVA_DRIVER_NAME` is set per child process, never system-wide.

## Bluetooth

The Intel 8265 is a combo Wi-Fi/Bluetooth part, so the kernel brings up `hci0`
unaided. Debian installs nothing to use it with: no `bluez`, no `bluetoothctl`,
`bluetoothd` inactive. PipeWire's `libspa-0.2-bluetooth` may already be present,
waiting for a stack that does not exist.

Read through `busctl` rather than `bluetoothctl`. The latter is a REPL that
prints a coloured event log; the former speaks the D-Bus API bluez actually
exposes and renders replies as JSON. Two limits, both found the hard way:

- `GetManagedObjects` cannot be rendered as JSON: busctl answers `Failed to
  create new json object: Invalid argument`, because the reply type nests deeper
  than its JSON writer handles. Use `busctl tree` plus one
  `Properties.GetAll` per device.
- A multi-property `get-property` fails entirely if any one property is absent,
  and on BLE devices most are. `GetAll` returns what exists.

**A discovery session belongs to the D-Bus connection that started it.**
`busctl call ... StartDiscovery` returns success, the process exits, and
`Discovering` is false a second later. Scanning has to hold a connection open
for its duration: `bluetoothctl --timeout N scan on` as a held child.

**Unpaired devices are transient.** In one 12-second scan here, 39 devices
appeared and 23 had vanished between listing the paths and reading their
properties. Skip them; that is the normal behaviour, not an error.

Pairing needs an agent to answer bluez's prompts, which `bluetoothctl`
registers and a raw `Device1.Pair` call does not.

Most of what a scan finds in a house is nameless BLE randoms with no `Icon` and
no `Class` — 16 of 16 on one run, 37 of 39 on another. Filter to paired devices
plus recognised kinds or the list is unusable.

## Other traps

- **`rfkill` is in `/usr/sbin`, which is not on an unprivileged non-login
  `PATH`.** It reports `rfkill: not found` on a machine where it is installed
  and working, which reads as "no Bluetooth" and is not.

- `pgrep` cannot find a process whose name is 16+ characters: the kernel
  truncates `comm` to 15. `ps -C` and `pgrep -f` work.
- `/run/user/N` contains `wayland-0` **and** `wayland-0.lock`. The latter gives
  a plausible `WAYLAND_DISPLAY` that connects to nothing.
- `/sys/class/drm/*/modes` lists this panel's mode twice. Take the first line.
- `cage` without `-d` lets the client draw decorations: the panel runs
  1920x1045 inside 1920x1080, a titlebar reserved and never painted. Invisible
  in logs.
- The HDMI capture card never stops sending frames. With nothing connected it
  sends Y=16 black, so "no signal" never fires.

## Using it

Not published to crates.io.

```toml
[dependencies]
hub-hal = { git = "https://github.com/jethac/thinksmart-hub-hal.git", tag = "v0.9.0" }
```

Pin a tag. Tracking `master` means hardware behaviour changing under you on an
unrelated push.

```
cargo build --release
cargo test
```

Runtime deps are glibc plus the tools it shells out to: `ddcutil`, `v4l2-ctl`,
`wpctl`, `pw-dump`, `vainfo`, `ffmpeg`, `grim`. Building on an older
distribution than the Hub's Debian 13 is fine.

## Scope

One machine, Debian 13. Not a cross-platform abstraction.

Everything external is a subprocess with a deadline rather than a binding,
because several of these drivers can block a thread uninterruptibly. A child
process can be killed.

## License

MIT or Apache-2.0, at your option.
