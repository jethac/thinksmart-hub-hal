//! Microphone array (dual omni, on 17ef:a017) through PipeWire.
//!
//! The level meter holds one long-lived `pw-record` and reads it continuously,
//! but only while the page is being looked at: it stops ten seconds after the
//! last poll, so the tester is not holding the microphones open forever.
//! Levels are per channel, because whether Linux sees two usable capsules or a
//! pre-mixed pair is one of the open questions about this hardware.

use crate::util::{last_line, now_ms, run};
use serde::Serialize;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const RATE: u32 = 16_000;
const CHANNELS: usize = 2;
const CHUNK_FRAMES: usize = 1600; // 0.1 s
const FLOOR_DB: f64 = -90.0;
const KEEPALIVE_MS: u64 = 10_000;

#[derive(Clone, Default, Serialize)]
pub struct Level {
    pub dbfs: f64,
    pub peak_dbfs: f64,
}

#[derive(Clone, Default, Serialize)]
pub struct State {
    pub monitoring: bool,
    pub channels: Vec<Level>,
    pub recording: bool,
    pub playing: bool,
    pub recorded_s: Option<u64>,
    pub error: Option<String>,
}

pub struct Mic {
    st: Mutex<State>,
    wanted_until: AtomicU64,
}

pub fn recording_path() -> PathBuf {
    std::env::temp_dir().join("hub-tester-recording.wav")
}

fn db(x: f64) -> f64 {
    if x <= 0.0 {
        FLOOR_DB
    } else {
        (20.0 * (x / 32768.0).log10()).max(FLOOR_DB)
    }
}

impl Mic {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { st: Mutex::new(State::default()), wanted_until: AtomicU64::new(0) })
    }

    pub fn keep_alive(&self) {
        self.wanted_until.store(now_ms() + KEEPALIVE_MS, Ordering::Relaxed);
    }

    fn wanted(&self) -> bool {
        now_ms() < self.wanted_until.load(Ordering::Relaxed)
    }

    pub fn state(&self) -> State {
        self.st.lock().unwrap().clone()
    }

    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        thread::spawn(move || loop {
            if !me.wanted() {
                me.st.lock().unwrap().monitoring = false;
                thread::sleep(Duration::from_millis(250));
                continue;
            }
            me.monitor();
        });
    }

    fn monitor(&self) {
        let spawned = Command::new("pw-record")
            .args([&format!("--rate={RATE}"), &format!("--channels={CHANNELS}"), "--format=s16", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                self.st.lock().unwrap().error = Some(format!("pw-record: {e}"));
                thread::sleep(Duration::from_secs(3));
                return;
            }
        };
        let mut out = child.stdout.take().expect("stdout piped");
        self.st.lock().unwrap().monitoring = true;

        let started = Instant::now();
        let mut buf = vec![0u8; CHUNK_FRAMES * CHANNELS * 2];
        while self.wanted() {
            if out.read_exact(&mut buf).is_err() {
                break;
            }
            let mut sum = [0f64; CHANNELS];
            let mut peak = [0f64; CHANNELS];
            for (i, pair) in buf.chunks_exact(2).enumerate() {
                let s = i16::from_le_bytes([pair[0], pair[1]]) as f64;
                let ch = i % CHANNELS;
                sum[ch] += s * s;
                peak[ch] = peak[ch].max(s.abs());
            }
            let levels = (0..CHANNELS)
                .map(|c| Level {
                    dbfs: (db((sum[c] / CHUNK_FRAMES as f64).sqrt()) * 10.0).round() / 10.0,
                    peak_dbfs: (db(peak[c]) * 10.0).round() / 10.0,
                })
                .collect();
            let mut st = self.st.lock().unwrap();
            st.channels = levels;
            st.error = None;
        }
        let _ = child.kill();
        let _ = child.wait();
        let mut st = self.st.lock().unwrap();
        st.monitoring = false;
        if self.wanted() && started.elapsed() < Duration::from_secs(2) {
            // Died immediately while still wanted: no source, or PipeWire down.
            st.error = Some("pw-record exited straight away - is there a capture device?".into());
            drop(st);
            thread::sleep(Duration::from_secs(3));
        }
    }

    pub fn record(self: &Arc<Self>, seconds: u64) -> Result<(), String> {
        let seconds = seconds.clamp(1, 15);
        {
            let mut st = self.st.lock().unwrap();
            if st.recording || st.playing {
                return Err("busy".into());
            }
            st.recording = true;
        }
        let path = recording_path();
        let _ = std::fs::remove_file(&path);
        let child = Command::new("pw-record")
            .args(["--rate=48000", "--channels=2"])
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                self.st.lock().unwrap().recording = false;
                return Err(format!("pw-record: {e}"));
            }
        };
        let me = self.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(seconds));
            // SIGINT, not SIGKILL, so pw-record finalises the WAV header.
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = child.try_wait() {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
            let mut st = me.st.lock().unwrap();
            st.recording = false;
            st.recorded_s = recording_path().exists().then_some(seconds);
        });
        Ok(())
    }

    pub fn play(self: &Arc<Self>) -> Result<(), String> {
        let path = recording_path();
        if !path.exists() {
            return Err("nothing recorded yet".into());
        }
        {
            let mut st = self.st.lock().unwrap();
            if st.recording || st.playing {
                return Err("busy".into());
            }
            st.playing = true;
        }
        let me = self.clone();
        thread::spawn(move || {
            let out = run("pw-play", &[&path.to_string_lossy()], Duration::from_secs(20));
            let mut st = me.st.lock().unwrap();
            st.playing = false;
            if !out.ok {
                st.error = Some(format!("pw-play: {}", last_line(&out.text)));
            }
        });
        Ok(())
    }
}
