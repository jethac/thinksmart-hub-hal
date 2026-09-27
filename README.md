# thinksmart-hub-hal

Hardware support for the **Lenovo ThinkSmart Hub 500** running Debian 13. One
Rust crate, one module per piece of hardware, no UI and no policy.

Extracted from [jethac/thinksmart-hub-tester](https://github.com/jethac/thinksmart-hub-tester)
so that every app on this panel shares one implementation of the quirks instead
of rediscovering them. **The quirks are the reason this crate exists.** The
hardware has several failure modes that are not obvious, and two of them can
hang a task in the kernel that no signal will clear.

Hardware facts and their provenance live in
[jethac/thinksmart-hub-custom](https://github.com/jethac/thinksmart-hub-custom)
(`docs/hardware.md`, `docs/peripherals.md`, `docs/verified.md`). That repo
records what the hardware does; this one turns it into code.

## Modules

| Module | Hardware | The quirk it exists to hide |
|---|---|---|
| `display` | Panel brightness and power over DDC/CI | No `/sys/class/backlight`. VCP `0x10` brightness, `0xD6` power. The bus number is not fixed — discover it, never hardcode `i2c-5`. `ddcutil` is slow and dislikes being hammered, so calls are cached and serialised |
| `prox` | PIR proximity sensor (`17ef:60c0`) | Reads are HID get-feature round trips the device sometimes never answers. Every access is a **child process with a deadline**, because a thread stuck in `usbhid_wait_io` is in uninterruptible sleep and cannot be cancelled |
| `led` | LED ring, a telephony HID interface on the audio device | White/green/red firmware presets only, not RGB. There is no "off" |
| `audio` | Speaker output via PipeWire | Hardware volume does nothing: a Harman DSP ignores USB Audio Class volume requests, so the output is pinned at maximum. Software volume is the only control |
| `mic` | Dual omni microphone array | Whether Linux sees two usable capsules or a pre-mixed pair is still an open question |
| `hid` | Read-only raw HID capture | For the undocumented devices, notably `17ef:60ce` |
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

## Using it

It is not published. Depend on it by git:

```toml
[dependencies]
hub-hal = { git = "ssh://git@github.com/jethac/thinksmart-hub-hal.git", tag = "v0.1.0" }
```

Pin a tag rather than tracking `master`, so a panel app does not change hardware
behaviour underneath itself on an unrelated push.

## Build

```
cargo build --release
cargo test
```

Depends only on glibc at runtime, so building on an older distribution than the
Hub's Debian 13 is fine — which is what `thinksmart-fleet` does, building on the
control node and shipping the binary.

## Consumers

- [thinksmart-hub-tester](https://github.com/jethac/thinksmart-hub-tester) —
  exercises every piece of this hardware from a touch page, so a unit can be
  checked off before anything is built on it.
- [thinksmart-hub-home](https://github.com/jethac/thinksmart-hub-home) — the
  home panel app.
