//! Audio soundtrack helpers.
//!
//! Muxing itself happens inside the ffmpeg argv (see
//! [`crate::stream::pipe`]); this module owns the policy: which file to use,
//! whether the fade is safe for the clip length, and the exact filter string.

use std::path::{Path, PathBuf};

/// Resolves a configured audio path to an existing file. Empty or missing
/// paths simply mean "silent video" — a missing soundtrack must never fail a
/// render that would otherwise succeed.
pub fn resolve_track(path: &str) -> Option<PathBuf> {
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    let candidate = PathBuf::from(path);
    candidate.is_file().then_some(candidate)
}

/// The ffmpeg `-af` value for a fade-in/fade-out over `duration_secs`.
///
/// The fade is capped at half the clip length so a 1-second video with a
/// 1.5-second configured fade still gets a valid filter instead of a
/// zero/negative fade-out start.
pub fn fade_filter(duration_secs: f64, fade_ms: i32) -> Option<String> {
    if fade_ms <= 0 || !duration_secs.is_finite() || duration_secs <= 0.0 {
        return None;
    }
    let fade = (fade_ms as f64 / 1000.0).min(duration_secs / 2.0);
    if fade <= 0.0 {
        return None;
    }
    // A sub-millisecond fade is pointless and ffmpeg rounds it to zero.
    if fade < 0.001 {
        return None;
    }
    let out_start = (duration_secs - fade).max(0.0);
    Some(format!(
        "afade=t=in:st=0:d={fade:.3},afade=t=out:st={out_start:.3}:d={fade:.3}"
    ))
}

/// True when `path` names a file the audio muxer can read.
pub fn is_readable_track(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_clip_gets_a_full_fade() {
        let f = fade_filter(10.0, 1500).unwrap();
        assert_eq!(f, "afade=t=in:st=0:d=1.500,afade=t=out:st=8.500:d=1.500");
    }

    #[test]
    fn fade_is_capped_for_short_clips() {
        let f = fade_filter(0.2, 1500).unwrap();
        assert_eq!(f, "afade=t=in:st=0:d=0.100,afade=t=out:st=0.100:d=0.100");
    }

    #[test]
    fn disabled_or_invalid_fades_are_skipped() {
        assert_eq!(fade_filter(10.0, 0), None);
        assert_eq!(fade_filter(0.0, 1500), None);
        assert_eq!(fade_filter(f64::NAN, 1500), None);
        assert_eq!(fade_filter(1e-9, 1), None);
    }

    #[test]
    fn missing_audio_paths_are_ignored() {
        assert_eq!(resolve_track(""), None);
        assert_eq!(resolve_track("   "), None);
        assert_eq!(resolve_track("/definitely/not/here.mp3"), None);

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("track.mp3");
        std::fs::write(&file, b"not really audio").unwrap();
        assert_eq!(resolve_track(file.to_str().unwrap()), Some(file.clone()));
        assert!(is_readable_track(&file));
    }
}
