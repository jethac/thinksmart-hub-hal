//! Audio devices: which ones exist, which one is in use, and their levels.
//!
//! [`crate::audio`] drives `@DEFAULT_AUDIO_SINK@` and `@DEFAULT_AUDIO_SOURCE@`,
//! which is the right thing when there is one obvious device on each side. On
//! this hardware there is not. Every Hub 500 carries an HDMI capture card
//! (`17ef:7219`) that presents an audio source of its own, and on two of three
//! units WirePlumber had picked THAT as the default source rather than the
//! microphone array -- so the "microphone" control was adjusting the capture
//! card's HDMI input and the array was untouched. Nothing about that is visible
//! if the only thing on screen is a slider labelled Microphone.
//!
//! So this module enumerates, and lets the user choose. Enumeration goes through
//! `pw-dump` because it is JSON: `wpctl status` is a tree drawn for humans, and
//! parsing it means depending on its indentation.

use crate::util::{last_line, run};
use serde::Serialize;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const T: Duration = Duration::from_secs(5);

/// Which side of the pipeline a device sits on.
#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Kind {
    Source,
    Sink,
}

#[derive(Clone, Default, Serialize)]
pub struct Device {
    /// PipeWire node id. Volume and default-selection are addressed by id --
    /// names are stable across reboots but ids are what wpctl takes.
    pub id: u32,
    /// `node.name`, e.g. `alsa_input.usb-...`. Stable across reboots, so this is
    /// what a persisted choice is stored as.
    pub name: String,
    /// `node.description`, e.g. "USB Audio Analog Stereo". What a person reads.
    pub description: String,
    pub default: bool,
    pub volume: Option<u32>,
    pub muted: bool,
    /// True for the Hub 500's own microphone array (`17ef:a017`).
    pub is_mic_array: bool,
    /// True for the HDMI capture card's audio (`17ef:7219`). Worth marking: it
    /// is a plausible-looking default that is almost never the one wanted for
    /// voice.
    pub is_capture_card: bool,
}

#[derive(Clone, Default, Serialize)]
pub struct State {
    pub sources: Vec<Device>,
    pub sinks: Vec<Device>,
    pub error: Option<String>,
}

pub struct Media {
    st: Mutex<State>,
}

impl Media {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { st: Mutex::new(State::default()) })
    }

    /// Refreshes on its own thread. `pw-dump` on this hardware is a few hundred
    /// kilobytes of JSON and parsing it is not free, so five seconds rather than
    /// the one a UI would like: device lists do not change often, and a hotplug
    /// showing up a moment late is not a problem worth spending CPU on forever.
    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        thread::spawn(move || {
            me.refresh();
            // Correct the default once, before anything reads it.
            if let Err(e) = me.ensure_mic_array_default() {
                eprintln!("hub-hal: could not set the microphone array as default: {e}");
            }
            loop {
                me.refresh();
                thread::sleep(Duration::from_secs(5));
            }
        });
    }

    /// Make the Hub 500's own microphone array the default source, if something
    /// wrong is holding the slot.
    ///
    /// WirePlumber picks a default by scoring devices, and the HDMI capture
    /// card's audio (`17ef:7219`) scores like an ordinary USB microphone -- so on
    /// two of the three hubs here it had won, and every application asking for
    /// "the microphone" got the HDMI input instead. That is never what is wanted
    /// on a device whose job includes voice, and it is invisible: the capture
    /// card answers, it just answers with silence.
    ///
    /// Deliberately narrow. It only acts when the current default IS the capture
    /// card, so a deliberate choice of any other device is left alone, and it
    /// runs once at start rather than on every refresh, so it cannot fight with
    /// the person using the settings screen. WirePlumber persists the result, so
    /// one correction sticks across reboots.
    ///
    /// Returns the description of the device it switched to, or None if there was
    /// nothing to fix.
    pub fn ensure_mic_array_default(&self) -> Result<Option<String>, String> {
        let (current_is_capture_card, array) = {
            let st = self.st.lock().unwrap();
            let current = st.sources.iter().find(|d| d.default);
            (
                current.is_some_and(|d| d.is_capture_card),
                st.sources.iter().find(|d| d.is_mic_array).cloned(),
            )
        };
        if !current_is_capture_card {
            return Ok(None);
        }
        // No array to switch to: leave the capture card alone rather than leave
        // the unit with no input at all.
        let Some(array) = array else { return Ok(None) };

        set_default(array.id)?;
        self.refresh();
        Ok(Some(array.description))
    }

    pub fn state(&self) -> State {
        self.st.lock().unwrap().clone()
    }

    pub fn refresh(&self) {
        let dump = run("pw-dump", &[], T);
        if !dump.ok {
            self.st.lock().unwrap().error = Some(last_line(&dump.text));
            return;
        }
        let json: Value = match serde_json::from_str(&dump.text) {
            Ok(v) => v,
            Err(e) => {
                self.st.lock().unwrap().error = Some(format!("pw-dump: {e}"));
                return;
            }
        };

        let default_source = default_node("@DEFAULT_AUDIO_SOURCE@");
        let default_sink = default_node("@DEFAULT_AUDIO_SINK@");

        let mut sources = Vec::new();
        let mut sinks = Vec::new();

        for node in json.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            if node.get("type").and_then(Value::as_str) != Some("PipeWire:Interface:Node") {
                continue;
            }
            let props = match node.pointer("/info/props") {
                Some(p) => p,
                None => continue,
            };
            let class = props.get("media.class").and_then(Value::as_str).unwrap_or("");
            let kind = match class {
                "Audio/Source" => Kind::Source,
                "Audio/Sink" => Kind::Sink,
                _ => continue,
            };
            let name = props.get("node.name").and_then(Value::as_str).unwrap_or("").to_string();
            // A sink's monitor shows up as a source. It is a loopback of what is
            // playing, never a microphone, and offering it as one would be a trap.
            if name.ends_with(".monitor") {
                continue;
            }
            let id = node.get("id").and_then(Value::as_u64).unwrap_or(0) as u32;
            let description = props
                .get("node.description")
                .or_else(|| props.get("node.nick"))
                .and_then(Value::as_str)
                .unwrap_or(&name)
                .to_string();

            // USB ids where they are exposed, falling back to the description.
            // The ids are the truth; the description is what survives when
            // PipeWire did not record them.
            let vid = props.get("device.vendor.id").and_then(Value::as_str).unwrap_or("");
            let pid = props.get("device.product.id").and_then(Value::as_str).unwrap_or("");
            let hay = format!("{name} {description}").to_lowercase();
            let is_capture_card = (vid.eq_ignore_ascii_case("0x17ef")
                && pid.eq_ignore_ascii_case("0x7219"))
                || hay.contains("uvc uac");
            let is_mic_array = (vid.eq_ignore_ascii_case("0x17ef")
                && pid.eq_ignore_ascii_case("0xa017"))
                || (hay.contains("usb audio") && !is_capture_card);

            let (volume, muted) = volume_of(id);
            let default = match kind {
                Kind::Source => default_source.as_deref() == Some(name.as_str()),
                Kind::Sink => default_sink.as_deref() == Some(name.as_str()),
            };
            let dev = Device {
                id,
                name,
                description,
                default,
                volume,
                muted,
                is_mic_array,
                is_capture_card,
            };
            match kind {
                Kind::Source => sources.push(dev),
                Kind::Sink => sinks.push(dev),
            }
        }

        // Stable order, so the list does not reshuffle under a finger. pw-dump's
        // order follows node ids, which change when a device is replugged.
        sources.sort_by(|a, b| a.description.cmp(&b.description));
        sinks.sort_by(|a, b| a.description.cmp(&b.description));

        let mut st = self.st.lock().unwrap();
        st.sources = sources;
        st.sinks = sinks;
        st.error = None;
    }
}

/// Make this device the default. WirePlumber remembers the choice across
/// reboots, so this is a setting rather than a session tweak.
pub fn set_default(id: u32) -> Result<(), String> {
    let out = run("wpctl", &["set-default", &id.to_string()], T);
    if out.ok {
        Ok(())
    } else {
        Err(last_line(&out.text))
    }
}

pub fn set_volume(id: u32, percent: u32) -> Result<(), String> {
    let v = percent.min(100);
    let out = run(
        "wpctl",
        &["set-volume", &id.to_string(), &format!("{:.2}", v as f32 / 100.0)],
        T,
    );
    if out.ok {
        Ok(())
    } else {
        Err(last_line(&out.text))
    }
}

pub fn set_mute(id: u32, on: bool) -> Result<(), String> {
    let out = run(
        "wpctl",
        &["set-mute", &id.to_string(), if on { "1" } else { "0" }],
        T,
    );
    if out.ok {
        Ok(())
    } else {
        Err(last_line(&out.text))
    }
}

/// `node.name` of whatever `target` currently resolves to.
fn default_node(target: &str) -> Option<String> {
    let out = run("wpctl", &["inspect", target], T);
    if !out.ok {
        return None;
    }
    out.text.lines().find_map(|l| {
        let l = l.trim().trim_start_matches('*').trim();
        l.strip_prefix("node.name")
            .and_then(|r| r.trim().strip_prefix('='))
            .map(|v| v.trim().trim_matches('"').to_string())
    })
}

fn volume_of(id: u32) -> (Option<u32>, bool) {
    let out = run("wpctl", &["get-volume", &id.to_string()], T);
    if !out.ok {
        return (None, false);
    }
    let vol = out
        .text
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse::<f32>().ok())
        .map(|v| (v * 100.0).round() as u32);
    (vol, out.text.contains("MUTED"))
}
