//! What the GPU can decode and encode, in hardware.
//!
//! The Hub 500 has an Intel HD Graphics 630 -- Kaby Lake GT2, `8086:5912`, on
//! `i915` with a render node at `/dev/dri/renderD128`. It has fixed-function
//! video silicon that nothing on these units was using, and the gap between what
//! it can do and what it was doing is wide enough to be worth stating in code
//! rather than in a comment somewhere.
//!
//! What it reports, through the iHD driver:
//!
//! ```text
//! decode   MPEG-2, H.264 (Main/High/ConstrainedBaseline), JPEG Baseline,
//!          VP8, VP9 (profile 0 and 2), HEVC Main and Main10
//! encode   H.264 (low-power path only), JPEG
//! ```
//!
//! **Note what is missing: there is no VP8 or VP9 encode.** Decode only. That is
//! the fact most likely to cost somebody a day, because WebRTC negotiates VP8 by
//! default and a call that does so will encode its outbound video on four 2.7 GHz
//! cores while a perfectly good H.264 encoder sits idle. It is discoverable here
//! -- `can_encode(Codec::Vp8)` is false -- so a caller can pick a codec on the
//! evidence instead of on an assumption about what an Intel GPU "obviously" does.
//!
//! Read through `vainfo` rather than by linking libva. That is a deliberate
//! trade. libva means bindgen, which means libclang and the VA headers on
//! whatever builds this -- and this crate is built on a control node and shipped
//! to hubs that have no compiler, so a build-time C dependency is a real cost
//! paid on every machine that ever builds it. Parsing one short table from a
//! subprocess with a deadline is the convention everything else here follows for
//! exactly this reason, and this particular table changes only when the driver is
//! replaced.

use crate::util::{run_env, RunError};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

/// vainfo opens the driver and enumerates; it is fast, and a deadline this long
/// is for a machine under load rather than for a GPU that has stopped answering.
const VAINFO_TIMEOUT: Duration = Duration::from_secs(10);

/// The render node. Enumerating through DRM rather than through a display server
/// is what lets this work from an Ansible shell, a systemd unit or a test --
/// none of which have a Wayland socket. `vainfo` with no arguments fails in all
/// three with "XDG_RUNTIME_DIR is invalid or not set".
pub(crate) const RENDER_NODE: &str = "/dev/dri/renderD128";

/// A video codec, as something to ask a question about rather than a string to
/// match. Only the ones this silicon has an opinion on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Mpeg2,
    H264,
    Hevc,
    Vp8,
    Vp9,
    Av1,
    Jpeg,
}

impl Codec {
    /// How this codec appears in a VA profile name. `VAProfileH264High` and
    /// `VAProfileH264Main` are both H.264; the question a caller asks is almost
    /// never about the profile.
    fn marker(self) -> &'static str {
        match self {
            Codec::Mpeg2 => "MPEG2",
            Codec::H264 => "H264",
            Codec::Hevc => "HEVC",
            Codec::Vp8 => "VP8",
            Codec::Vp9 => "VP9",
            Codec::Av1 => "AV1",
            Codec::Jpeg => "JPEG",
        }
    }
}

impl std::fmt::Display for Codec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.marker())
    }
}

/// One line of vainfo's table: a profile and the things it can be used for.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    /// e.g. `VAProfileH264High`.
    pub name: String,
    /// e.g. `VAEntrypointVLD` (decode), `VAEntrypointEncSliceLP` (encode).
    pub entrypoints: Vec<String>,
}

/// What this GPU offers.
#[derive(Debug, Clone, PartialEq)]
pub struct Capabilities {
    /// The driver's own description, e.g. "Intel iHD driver for Intel(R) Gen
    /// Graphics - 25.2.3". Worth carrying: a capability that changes between
    /// units is almost always a different driver rather than different silicon.
    pub driver: String,
    /// VA-API version, e.g. "1.22".
    pub version: String,
    pub profiles: Vec<Profile>,
}

/// Why the question could not be answered. Four cases with four different
/// fixes, which is why a caller gets to tell them apart rather than a string.
#[derive(Debug, Clone, PartialEq)]
pub enum VaError {
    /// vainfo is not installed. The fleet's panel role installs it, along with
    /// the driver it reports on.
    NotInstalled,
    /// No render node. Either there is no GPU, or `i915` did not bind -- this is
    /// hardware missing, not a package missing, and says where it looked.
    NoRenderNode(String),
    /// vainfo did not answer and was killed.
    Timeout(Duration),
    /// vainfo ran and failed. Its own last line, which usually names the driver
    /// it could not open.
    Failed(String),
}

impl std::fmt::Display for VaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VaError::NotInstalled => write!(f, "vainfo is not installed"),
            VaError::NoRenderNode(p) => write!(f, "no render node at {p}; is there a GPU?"),
            VaError::Timeout(d) => write!(f, "vainfo did not answer in {}s", d.as_secs()),
            VaError::Failed(t) => write!(f, "vainfo failed: {t}"),
        }
    }
}

impl std::error::Error for VaError {}

impl Capabilities {
    /// Whether this GPU can decode `codec` in hardware.
    pub fn can_decode(&self, codec: Codec) -> bool {
        self.supports(codec, |e| e.contains("VLD"))
    }

    /// Whether this GPU can encode `codec` in hardware.
    ///
    /// True for any encode entrypoint, including the low-power one. On this
    /// hardware H.264 encode is `EncSliceLP` only, which is a real encoder and
    /// the only one there is -- treating it as "not encode" would be wrong.
    pub fn can_encode(&self, codec: Codec) -> bool {
        self.supports(codec, |e| e.contains("Enc"))
    }

    /// Whether this GPU can scale and convert on the video post-processing
    /// engine, rather than only decode.
    ///
    /// This is the question that decides whether hardware decode is worth
    /// anything at all here, and it is separate from decoding. Without VPP a
    /// decoded frame has to be pulled back out of GPU memory at full size --
    /// 3.1 MB for 1080p NV12 -- and scaled on the CPU, and the download costs
    /// almost exactly what the decode saved. Measured on this fleet: software
    /// 3.19 s of CPU for 120 frames, hardware decode without VPP 3.24 s. A wash.
    ///
    /// With VPP the scale happens before the download and only 345 KB comes
    /// back, which is 0.93 s for the same 120 frames.
    ///
    /// It is reported as `VAProfileNone : VAEntrypointVideoProc`, which is not a
    /// codec and so does not fit `can_decode`.
    pub fn can_post_process(&self) -> bool {
        self.profiles
            .iter()
            .filter(|p| p.name.contains("VAProfileNone"))
            .any(|p| p.entrypoints.iter().any(|e| e.contains("VideoProc")))
    }

    fn supports(&self, codec: Codec, want: impl Fn(&str) -> bool) -> bool {
        self.profiles
            .iter()
            .filter(|p| p.name.contains(codec.marker()))
            .any(|p| p.entrypoints.iter().any(|e| want(e)))
    }

    /// Every codec this GPU can decode, for a log line or an inventory.
    pub fn decodes(&self) -> Vec<Codec> {
        ALL.iter().copied().filter(|c| self.can_decode(*c)).collect()
    }

    pub fn encodes(&self) -> Vec<Codec> {
        ALL.iter().copied().filter(|c| self.can_encode(*c)).collect()
    }
}

const ALL: [Codec; 7] = [
    Codec::Mpeg2,
    Codec::H264,
    Codec::Hevc,
    Codec::Vp8,
    Codec::Vp9,
    Codec::Av1,
    Codec::Jpeg,
];

/// Drivers to try, in order of preference, when looking for one that can do a
/// whole hardware path rather than half of one.
///
/// The order is not arbitrary and is the opposite of what the modern advice
/// would be. On this hardware -- Kaby Lake, Debian 13 -- the maintained iHD
/// driver reports 15 entrypoints and **no** `VAEntrypointVideoProc`, while the
/// legacy i965 driver reports 28 and has it -- which is why i965 was tried first
/// for a while, and why the order has since changed.
///
/// The gap was never about the chip. Debian's DFSG repack strips Intel's Gen9
/// post-processing kernels, which ship as source-less binaries, and VPP on this
/// generation is entirely shader-based, so removing them removes the entrypoint.
/// Decode survives because it is fixed-function. The fleet now installs
/// `intel-media-va-driver-non-free`, which has the kernels: 32 entrypoints,
/// post-processing present, and the full `VAEntrypointEncSlice` H.264 encoder
/// rather than only the low-power path.
///
/// With both drivers working, iHD is tried first on grounds that are not
/// performance. Measured on the same workload the preview runs, 120 frames
/// alternating between them, the times were 251ms, 234ms, 196ms and 306ms: the
/// run-to-run spread is wider than any difference between the drivers, so speed
/// does not choose. What chooses is that i965 is frozen upstream at 2.4.1 and
/// iHD is maintained, and that iHD carries the encoder the call work will need.
/// i965 stays as the fallback rather than being dropped, because a unit whose
/// non-free driver is missing should still get hardware decode.
///
/// An empty name means "whatever libva picks by itself", tried last so that a
/// unit with neither of the named drivers still gets an answer.
const DRIVER_CANDIDATES: [&str; 3] = ["iHD", "i965", ""];

/// A hardware path that actually works: a driver that can both decode the codec
/// and post-process, which is the pair that makes acceleration worth using.
#[derive(Debug, Clone)]
pub struct Accelerator {
    /// What to put in `LIBVA_DRIVER_NAME` for the child doing the decoding.
    ///
    /// Set per child rather than for the whole system on purpose: a browser doing
    /// its own encoding later may well prefer iHD, and choosing a driver for one
    /// consumer must not choose it for every other.
    pub driver_name: String,
    pub caps: Capabilities,
}

/// The driver to use to hardware-decode `codec`, if any can.
///
/// Requires post-processing as well as decode. A driver that can only decode is
/// reported as no accelerator at all, because using it is measurably not worth
/// the complexity -- the caller should stay on its software path rather than
/// take on a second one for nothing.
pub fn accelerator_for(codec: Codec) -> Option<&'static Accelerator> {
    static CACHE: OnceLock<Option<Accelerator>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            for name in DRIVER_CANDIDATES {
                let Ok(caps) = probe_with(name) else { continue };
                if caps.can_decode(codec) && caps.can_post_process() {
                    return Some(Accelerator { driver_name: name.to_string(), caps });
                }
            }
            None
        })
        .as_ref()
}

/// What this GPU can do, asked once.
///
/// Cached for the life of the process: it cannot change without the driver being
/// replaced, and a caller deciding per frame whether to use hardware must not pay
/// a subprocess for the privilege. The error is cached too -- a missing vainfo
/// stays missing, and retrying it in a decode loop would be the same mistake with
/// extra steps.
pub fn capabilities() -> Result<&'static Capabilities, VaError> {
    static CACHE: OnceLock<Result<Capabilities, VaError>> = OnceLock::new();
    CACHE.get_or_init(probe).as_ref().map_err(Clone::clone)
}

fn probe() -> Result<Capabilities, VaError> {
    probe_with("")
}

/// Ask a named driver what it can do. An empty name lets libva choose.
fn probe_with(driver: &str) -> Result<Capabilities, VaError> {
    if !Path::new(RENDER_NODE).exists() {
        return Err(VaError::NoRenderNode(RENDER_NODE.to_string()));
    }
    let env: Vec<(&str, &str)> = if driver.is_empty() {
        Vec::new()
    } else {
        vec![("LIBVA_DRIVER_NAME", driver)]
    };
    let text = run_env(
        "vainfo",
        &["--display", "drm", "--device", RENDER_NODE],
        &env,
        VAINFO_TIMEOUT,
    )
    .map_err(|e| match e {
        RunError::NotFound(_) => VaError::NotInstalled,
        RunError::Timeout(d) => VaError::Timeout(d),
        RunError::Spawn(s) | RunError::Failed(s) => VaError::Failed(s),
    })?;
    Ok(parse(&text))
}

/// Parse vainfo's output.
///
/// The table is one profile per line, `VAProfileX : VAEntrypointY`, and a profile
/// appears once per entrypoint rather than once with a list -- H.264 High shows up
/// twice, for decode and for encode. They are folded together here, because
/// "can this encode H.264" is the question and "which line was it on" is not.
fn parse(text: &str) -> Capabilities {
    let mut driver = String::new();
    let mut version = String::new();
    let mut profiles: Vec<Profile> = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("vainfo: Driver version:") {
            driver = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("vainfo: VA-API version:") {
            // "1.22 (libva 2.22.0)" -- the bracket is the library, not the API.
            version = rest.split_whitespace().next().unwrap_or("").to_string();
        } else if line.starts_with("VAProfile") {
            let Some((name, entry)) = line.split_once(':') else { continue };
            let (name, entry) = (name.trim().to_string(), entry.trim().to_string());
            if entry.is_empty() {
                continue;
            }
            match profiles.iter_mut().find(|p| p.name == name) {
                Some(p) => p.entrypoints.push(entry),
                None => profiles.push(Profile { name, entrypoints: vec![entry] }),
            }
        }
    }
    Capabilities { driver, version, profiles }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real output from hub-002, trimmed. Kept verbatim so the parser is tested
    /// against what the hardware actually says rather than against a tidied
    /// version of it.
    const REAL: &str = "\
libva info: VA-API version 1.22.0
libva info: Trying to open /usr/lib/x86_64-linux-gnu/dri/iHD_drv_video.so
libva info: Found init function __vaDriverInit_1_22
libva info: va_openDriver() returns 0
Trying display: drm
vainfo: VA-API version: 1.22 (libva 2.22.0)
vainfo: Driver version: Intel iHD driver for Intel(R) Gen Graphics - 25.2.3 ()
vainfo: Supported profile and entrypoints
      VAProfileMPEG2Simple            :\tVAEntrypointVLD
      VAProfileMPEG2Main              :\tVAEntrypointVLD
      VAProfileH264Main               :\tVAEntrypointVLD
      VAProfileH264Main               :\tVAEntrypointEncSliceLP
      VAProfileH264High               :\tVAEntrypointVLD
      VAProfileH264High               :\tVAEntrypointEncSliceLP
      VAProfileJPEGBaseline           :\tVAEntrypointVLD
      VAProfileJPEGBaseline           :\tVAEntrypointEncPicture
      VAProfileH264ConstrainedBaseline:\tVAEntrypointVLD
      VAProfileH264ConstrainedBaseline:\tVAEntrypointEncSliceLP
      VAProfileVP8Version0_3          :\tVAEntrypointVLD
      VAProfileHEVCMain               :\tVAEntrypointVLD
      VAProfileHEVCMain10             :\tVAEntrypointVLD
      VAProfileVP9Profile0            :\tVAEntrypointVLD
      VAProfileVP9Profile2            :\tVAEntrypointVLD
";

    fn caps() -> Capabilities {
        parse(REAL)
    }

    #[test]
    fn reads_the_driver_and_version() {
        let c = caps();
        assert_eq!(c.version, "1.22");
        assert!(c.driver.starts_with("Intel iHD driver"), "{}", c.driver);
    }

    #[test]
    fn folds_a_profile_that_appears_once_per_entrypoint() {
        let c = caps();
        let h264_high = c.profiles.iter().find(|p| p.name == "VAProfileH264High").unwrap();
        assert_eq!(h264_high.entrypoints.len(), 2, "decode and encode are one profile");
    }

    #[test]
    fn decode_is_what_this_chip_actually_offers() {
        let c = caps();
        for codec in [Codec::Mpeg2, Codec::H264, Codec::Hevc, Codec::Vp8, Codec::Vp9, Codec::Jpeg] {
            assert!(c.can_decode(codec), "{codec} should decode");
        }
        // Kaby Lake predates AV1 entirely.
        assert!(!c.can_decode(Codec::Av1));
    }

    #[test]
    fn there_is_no_vp8_or_vp9_encode_and_that_is_the_point() {
        // The fact most likely to cost somebody a day: WebRTC negotiates VP8 by
        // default, and this chip can only decode it.
        let c = caps();
        assert!(c.can_decode(Codec::Vp8) && !c.can_encode(Codec::Vp8));
        assert!(c.can_decode(Codec::Vp9) && !c.can_encode(Codec::Vp9));
        assert!(c.can_encode(Codec::H264), "H.264 is the encoder a call must use");
        assert!(c.can_encode(Codec::Jpeg));
    }

    #[test]
    fn low_power_encode_still_counts_as_encode() {
        // H.264 here is EncSliceLP only. It is a real encoder and the only one
        // there is; reporting "cannot encode" would send a caller to software.
        let c = caps();
        let p = c.profiles.iter().find(|p| p.name == "VAProfileH264Main").unwrap();
        assert!(p.entrypoints.iter().any(|e| e == "VAEntrypointEncSliceLP"));
        assert!(c.can_encode(Codec::H264));
    }

    #[test]
    fn a_table_with_nothing_in_it_is_not_a_crash() {
        let c = parse("nothing here\n");
        assert!(c.profiles.is_empty());
        assert!(!c.can_decode(Codec::Jpeg));
        assert!(c.decodes().is_empty());
    }

    #[test]
    fn lists_read_as_an_inventory() {
        let c = caps();
        assert_eq!(c.encodes(), vec![Codec::H264, Codec::Jpeg]);
        assert!(c.decodes().contains(&Codec::Vp9));
    }
}
