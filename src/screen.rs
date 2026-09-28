//! A screenshot of what is actually on the panel.
//!
//! This exists because these units are wall-mounted, have no keyboard, and are
//! developed over ssh from another room. "What does it look like" was otherwise
//! unanswerable, and the cost of that was real: the panel ran 1920x1045 inside a
//! 1920x1080 display for a day, with a 35 pixel black band that appeared in no
//! log and was found only by capturing the screen and counting rows.
//!
//! Separate from [`crate::display`] on purpose. That module is the panel as
//! hardware -- brightness and power over DDC/CI, the mode the kernel has it in.
//! This one is the compositor's output, which is a different thing that can be
//! wrong while the hardware is right. That was exactly the case above.
//!
//! `grim` does the work, as a child process with a deadline, for the reason that
//! shapes the rest of this crate: a wedged compositor must not be able to wedge
//! the caller. A Wayland client that never gets a frame callback waits forever,
//! and a thread that does that cannot be recovered. A child can be killed.

use crate::util::{run_env, RunError};
use std::path::Path;
use std::time::Duration;

/// Generous. A 1080p capture is well under a second on this hardware; this is
/// sized for a machine under load, not for a compositor that has stopped
/// answering -- that case is what the deadline is for.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);

/// Why a capture did not happen. Three different problems with three different
/// fixes, which is why a caller gets to tell them apart rather than a string.
#[derive(Debug, Clone, PartialEq)]
pub enum CaptureError {
    /// grim is not installed. The fleet's panel role installs it.
    NotInstalled,
    /// No Wayland socket to talk to: nothing is running a session, or this
    /// process cannot see the session's runtime directory. Says where it looked.
    NoSession(String),
    /// grim did not answer in time and was killed. The compositor is wedged.
    Timeout(Duration),
    /// grim ran and refused. Its own last line, which is usually specific.
    Failed(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::NotInstalled => write!(f, "grim is not installed"),
            CaptureError::NoSession(where_) => {
                write!(f, "no Wayland session found ({where_})")
            }
            CaptureError::Timeout(d) => {
                write!(f, "grim did not answer in {}s; the compositor is wedged", d.as_secs())
            }
            CaptureError::Failed(t) => write!(f, "grim failed: {t}"),
        }
    }
}

impl std::error::Error for CaptureError {}

/// Where the compositor is listening.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// XDG_RUNTIME_DIR, e.g. `/run/user/1000`.
    pub runtime_dir: String,
    /// WAYLAND_DISPLAY, e.g. `wayland-0`.
    pub display: String,
}

/// Find the session to capture from.
///
/// The environment first, because a process started inside the session already
/// has it right and second-guessing that would be worse. Otherwise the running
/// user's own runtime directory, which is where its compositor puts the socket --
/// derived from getuid rather than hardcoded, because this crate is also used
/// from root-owned units and from a developer's shell, and /run/user/1000 is only
/// correct by accident.
pub fn session() -> Result<Session, CaptureError> {
    let runtime_dir = match std::env::var("XDG_RUNTIME_DIR") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => format!("/run/user/{}", unsafe { libc::getuid() }),
    };

    if let Ok(v) = std::env::var("WAYLAND_DISPLAY") {
        if !v.trim().is_empty() {
            return Ok(Session { runtime_dir, display: v });
        }
    }

    let names: Vec<String> = std::fs::read_dir(&runtime_dir)
        .map_err(|e| CaptureError::NoSession(format!("{runtime_dir}: {e}")))?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
        .collect();

    match pick_socket(&names) {
        Some(display) => Ok(Session { runtime_dir, display }),
        None => Err(CaptureError::NoSession(format!(
            "no wayland-* socket in {runtime_dir}"
        ))),
    }
}

/// Choose a Wayland socket from a directory listing.
///
/// `wayland-0` first because that is what a single compositor takes, then the
/// lowest numbered. The `.lock` files sitting beside each socket are excluded --
/// picking one produces a WAYLAND_DISPLAY that looks plausible and connects to
/// nothing.
fn pick_socket(names: &[String]) -> Option<String> {
    let mut sockets: Vec<(u32, &str)> = names
        .iter()
        .filter(|n| !n.ends_with(".lock"))
        .filter_map(|n| {
            let num = n.strip_prefix("wayland-")?;
            Some((num.parse::<u32>().ok()?, n.as_str()))
        })
        .collect();
    sockets.sort();
    sockets.first().map(|(_, n)| n.to_string())
}

/// Capture the screen to `dest`, as a PNG.
///
/// Overwrites. The caller picks the path, because the useful destinations differ
/// by a lot -- a tmpfile to measure, a dated file to keep, somewhere an operator
/// will fetch from.
pub fn capture(dest: impl AsRef<Path>) -> Result<(), CaptureError> {
    capture_in(&session()?, dest)
}

/// Capture from a session found some other way. Worth having separately: a
/// caller that already knows the session should not pay to rediscover it, and a
/// caller debugging a session mismatch wants to state one explicitly.
pub fn capture_in(session: &Session, dest: impl AsRef<Path>) -> Result<(), CaptureError> {
    let dest = dest.as_ref();
    let Some(path) = dest.to_str() else {
        return Err(CaptureError::Failed(format!("path is not UTF-8: {}", dest.display())));
    };

    let env = [
        ("XDG_RUNTIME_DIR", session.runtime_dir.as_str()),
        ("WAYLAND_DISPLAY", session.display.as_str()),
    ];

    match run_env("grim", &[path], &env, CAPTURE_TIMEOUT) {
        Ok(_) => Ok(()),
        Err(RunError::NotFound(_)) => Err(CaptureError::NotInstalled),
        Err(RunError::Timeout(d)) => Err(CaptureError::Timeout(d)),
        Err(RunError::Spawn(e)) => Err(CaptureError::Failed(e)),
        // grim's own message. It is specific about the two common cases -- no
        // compositor on that socket, and a compositor without the screencopy
        // protocol -- so it is passed through rather than summarised.
        Err(RunError::Failed(t)) => Err(CaptureError::Failed(t)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_socket_and_not_its_lock_file() {
        // A real /run/user/1000 listing: the lock sits next to the socket, and
        // choosing it yields a WAYLAND_DISPLAY that connects to nothing.
        let names = vec![
            "bus".to_string(),
            "wayland-0.lock".to_string(),
            "wayland-0".to_string(),
            "pipewire-0".to_string(),
        ];
        assert_eq!(pick_socket(&names), Some("wayland-0".into()));
    }

    #[test]
    fn prefers_the_lowest_numbered_display() {
        let names = vec!["wayland-3".to_string(), "wayland-1".to_string()];
        assert_eq!(pick_socket(&names), Some("wayland-1".into()));
    }

    #[test]
    fn no_socket_is_not_a_socket() {
        let names = vec!["bus".to_string(), "systemd".to_string()];
        assert_eq!(pick_socket(&names), None);
        // `wayland-` with nothing after it is not a display either.
        assert_eq!(pick_socket(&["wayland-".to_string()]), None);
    }

    #[test]
    fn errors_say_what_to_do() {
        assert_eq!(CaptureError::NotInstalled.to_string(), "grim is not installed");
        assert!(CaptureError::Timeout(Duration::from_secs(15))
            .to_string()
            .contains("wedged"));
        assert!(CaptureError::NoSession("/run/user/0: no such directory".into())
            .to_string()
            .contains("/run/user/0"));
    }
}
