//! Process and file helpers shared by every hardware backend.

use std::ffi::CString;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct Output {
    pub ok: bool,
    pub text: String,
}

/// Run a command with a hard deadline. Never panics; a missing binary or a
/// timeout comes back as `ok: false` with the reason in `text`.
pub fn run(cmd: &str, args: &[&str], timeout: Duration) -> Output {
    let mut child = match Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Output { ok: false, text: format!("{cmd}: {e}") },
    };

    // Drain both pipes on their own threads. Waiting first and reading after
    // deadlocks as soon as a command writes more than a pipe buffer.
    let mut so = child.stdout.take().expect("stdout piped");
    let mut se = child.stderr.take().expect("stderr piped");
    let out = thread::spawn(move || {
        let mut s = String::new();
        let _ = so.read_to_string(&mut s);
        s
    });
    let err = thread::spawn(move || {
        let mut s = String::new();
        let _ = se.read_to_string(&mut s);
        s
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(_) => break None,
        }
    };

    let text = out.join().unwrap_or_default() + &err.join().unwrap_or_default();
    match status {
        Some(st) => Output { ok: st.success(), text },
        None => Output {
            ok: false,
            text: format!("{cmd}: no answer in {}s", timeout.as_secs()),
        },
    }
}

/// Whether this process may write `path`, by the kernel's own check.
pub fn writable(path: &Path) -> bool {
    CString::new(path.as_os_str().as_bytes())
        .map(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 })
        .unwrap_or(false)
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Read a small sysfs/procfs attribute that is known not to block.
pub fn read_trim(p: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

pub fn last_line(text: &str) -> String {
    let line = text.trim().lines().last().unwrap_or("failed").trim();
    line.chars().take(200).collect()
}

/// Build a 16-bit PCM WAV file in memory.
pub fn wav(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let block = channels * 2;
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * block as u32).to_le_bytes());
    b.extend_from_slice(&block.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}
