//! Raw HID report capture, for working out what the undocumented devices
//! (17ef:60ce, the sensor hub, the telephony interface) actually send.
//!
//! Read-only. Nothing here writes to a HID device.

use crate::util::now_ms;
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::{ErrorKind, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::thread;
use std::time::{Duration, Instant};

const MAX_REPORTS: usize = 500;

pub fn capture(node: &str, seconds: u64) -> Result<Value, String> {
    let valid = node.strip_prefix("hidraw").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    if !valid {
        return Err("node must look like hidrawN".into());
    }
    let seconds = seconds.clamp(1, 30);
    // Non-blocking, so an idle device cannot hold the request past its deadline.
    let mut f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(format!("/dev/{node}"))
        .map_err(|e| format!("/dev/{node}: {e}"))?;

    let start = Instant::now();
    let deadline = start + Duration::from_secs(seconds);
    let mut reports = vec![];
    let mut buf = [0u8; 256];
    while Instant::now() < deadline && reports.len() < MAX_REPORTS {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let hex: Vec<String> = buf[..n].iter().map(|b| format!("{b:02x}")).collect();
                reports.push(json!({"t_ms": start.elapsed().as_millis() as u64, "len": n, "hex": hex.join(" ")}));
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(format!("read failed: {e}")),
        }
    }
    Ok(json!({"node": node, "seconds": seconds, "captured_at": now_ms(), "truncated": reports.len() >= MAX_REPORTS, "reports": reports}))
}
