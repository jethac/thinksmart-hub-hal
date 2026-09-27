//! Speaker output through PipeWire.
//!
//! Hardware volume does nothing on this device: a Harman DSP in the audio path
//! ignores USB Audio Class volume requests, so the output is pinned at maximum.
//! PipeWire software volume on the sink is the only control that exists, and
//! that is what the slider drives.

use crate::util::{last_line, run, wav};
use serde::Serialize;
use std::f32::consts::TAU;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const T: Duration = Duration::from_secs(5);
const SINK: &str = "@DEFAULT_AUDIO_SINK@";
const SOURCE: &str = "@DEFAULT_AUDIO_SOURCE@";

#[derive(Clone, Default, Serialize)]
pub struct State {
    pub volume: Option<u32>,
    pub muted: bool,
    pub sink: Option<String>,
    pub source: Option<String>,
    pub sink_is_hub: bool,
    pub playing: bool,
    pub error: Option<String>,
}

pub struct Audio {
    st: Mutex<State>,
}

impl Audio {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { st: Mutex::new(State::default()) })
    }

    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        thread::spawn(move || loop {
            me.refresh();
            thread::sleep(Duration::from_secs(3));
        });
    }

    pub fn state(&self) -> State {
        self.st.lock().unwrap().clone()
    }

    fn refresh(&self) {
        let vol = run("wpctl", &["get-volume", SINK], T);
        let sink = describe(SINK);
        let source = describe(SOURCE);
        let mut st = self.st.lock().unwrap();
        if !vol.ok {
            st.error = Some(last_line(&vol.text));
            return;
        }
        st.volume = vol
            .text
            .split_whitespace()
            .nth(1)
            .and_then(|v| v.parse::<f32>().ok())
            .map(|v| (v * 100.0).round() as u32);
        st.muted = vol.text.contains("MUTED");
        st.sink_is_hub = sink.as_deref().is_some_and(is_hub);
        st.sink = sink;
        st.source = source;
        st.error = None;
    }

    pub fn set_volume(&self, value: u32) -> Result<(), String> {
        let v = value.min(100);
        let out = run("wpctl", &["set-volume", SINK, &format!("{:.2}", v as f32 / 100.0)], T);
        if !out.ok {
            return Err(last_line(&out.text));
        }
        self.st.lock().unwrap().volume = Some(v);
        Ok(())
    }

    pub fn set_mute(&self, on: bool) -> Result<(), String> {
        let out = run("wpctl", &["set-mute", SINK, if on { "1" } else { "0" }], T);
        if !out.ok {
            return Err(last_line(&out.text));
        }
        self.st.lock().unwrap().muted = on;
        Ok(())
    }

    /// Left channel at 440 Hz, then right at 660 Hz, played by pw-play rather
    /// than the browser - so a failure here and a success in the browser (or the
    /// reverse) points at the layer that is broken.
    pub fn play_test(self: &Arc<Self>) -> Result<(), String> {
        {
            let mut st = self.st.lock().unwrap();
            if st.playing {
                return Err("already playing".into());
            }
            st.playing = true;
        }
        let path: PathBuf = std::env::temp_dir().join("hub-tester-tone.wav");
        if let Err(e) = fs::write(&path, tone()) {
            self.st.lock().unwrap().playing = false;
            return Err(e.to_string());
        }
        let me = self.clone();
        thread::spawn(move || {
            let out = run("pw-play", &[&path.to_string_lossy()], Duration::from_secs(15));
            let mut st = me.st.lock().unwrap();
            st.playing = false;
            if !out.ok {
                st.error = Some(format!("pw-play: {}", last_line(&out.text)));
            }
        });
        Ok(())
    }
}

fn describe(target: &str) -> Option<String> {
    let out = run("wpctl", &["inspect", target], T);
    if !out.ok {
        return None;
    }
    let field = |key: &str| {
        out.text.lines().find_map(|l| {
            let l = l.trim().trim_start_matches('*').trim();
            l.strip_prefix(key)
                .and_then(|r| r.trim().strip_prefix('='))
                .map(|v| v.trim().trim_matches('"').to_string())
        })
    };
    let desc = field("node.description");
    let name = field("node.name");
    match (desc, name) {
        (Some(d), Some(n)) => Some(format!("{d} ({n})")),
        (d, n) => d.or(n),
    }
}

fn is_hub(s: &str) -> bool {
    let s = s.to_lowercase();
    s.contains("17ef") || s.contains("lenovo") || s.contains("thinksmart")
}

fn tone() -> Vec<u8> {
    const RATE: u32 = 48_000;
    let mut pcm = Vec::new();
    let mut seg = |freq: f32, secs: f32, left: bool, right: bool| {
        let n = (RATE as f32 * secs) as usize;
        for i in 0..n {
            let fade = ((i.min(n - i)) as f32 / 480.0).min(1.0);
            let s = ((i as f32 / RATE as f32) * freq * TAU).sin() * 0.3 * fade;
            let s = (s * i16::MAX as f32) as i16;
            pcm.push(if left { s } else { 0 });
            pcm.push(if right { s } else { 0 });
        }
    };
    seg(440.0, 0.8, true, false);
    seg(0.0, 0.25, false, false);
    seg(660.0, 0.8, false, true);
    wav(RATE, 2, &pcm)
}
