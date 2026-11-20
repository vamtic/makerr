//! `ln`-style frame linking that behaves the same on Linux, macOS and Windows.
//!
//! Timelapse runs are dominated by *unchanged* frames: a quiet canvas produces
//! hundreds of bit-identical snapshots. Instead of paying a PNG encode (and a
//! full frame's worth of disk) for each of them, the pipeline links each
//! unchanged frame to its predecessor:
//!
//! * [`LinkMode::Hard`] — `ln` (hardlink): one inode, two names, zero extra
//!   bytes. Works on Unix filesystems and on NTFS/ReFS on Windows. Both names
//!   stay valid even if the directory is moved or one name is deleted.
//! * [`LinkMode::Soft`] — `ln -s` (symlink): a tiny file that points at the
//!   target. Works on Unix, and on Windows once Developer Mode (or the
//!   `SeCreateSymbolicLinkPrivilege`) is enabled. Survives on filesystems
//!   without hardlink support (exFAT/FAT32, some network shares).
//! * [`LinkMode::Copy`] — a plain byte-for-byte copy. Always possible, costs
//!   disk, but still far cheaper than re-encoding a PNG.
//! * [`LinkMode::Auto`] (default) — hardlink, then symlink, then copy: the
//!   cheapest mechanism the destination filesystem actually supports.
//!
//! The pipeline reports which mechanism was used so "linking" is never silently
//! a copy.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How an unchanged frame is attached to its predecessor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LinkMode {
    /// Hardlink → symlink → copy, whichever the filesystem supports.
    #[default]
    Auto,
    /// Hardlink only (`ln`).
    Hard,
    /// Symlink only (`ln -s`).
    Soft,
    /// Plain copy only.
    Copy,
}

impl LinkMode {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkMode::Auto => "auto",
            LinkMode::Hard => "hard",
            LinkMode::Soft => "soft",
            LinkMode::Copy => "copy",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LinkMode::Auto => "Auto (hardlink, then symlink, then copy)",
            LinkMode::Hard => "Hardlink (ln)",
            LinkMode::Soft => "Symlink (ln -s)",
            LinkMode::Copy => "Copy",
        }
    }

    /// Parses a CLI/config value; unknown values fall back to `auto`.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "hard" | "hardlink" | "ln" => LinkMode::Hard,
            "soft" | "symlink" | "ln-s" | "ln -s" | "symbolic" => LinkMode::Soft,
            "copy" | "none" | "off" => LinkMode::Copy,
            _ => LinkMode::Auto,
        }
    }

    pub fn all() -> &'static [LinkMode] {
        &[
            LinkMode::Auto,
            LinkMode::Hard,
            LinkMode::Soft,
            LinkMode::Copy,
        ]
    }
}

/// What `dst` actually ended up being.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    HardLink,
    SymLink,
    Copy,
}

impl LinkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkKind::HardLink => "hardlink",
            LinkKind::SymLink => "symlink",
            LinkKind::Copy => "copy",
        }
    }
}

/// Replaces `dst` with a link to `src` according to `mode`, falling back to the
/// next-cheapest mechanism in [`LinkMode::Auto`]. Returns what `dst` became.
///
/// `src` must already exist. An existing `dst` (stale frame from an earlier
/// run, or a symlink) is removed first, so the result is deterministic.
pub fn link_or_copy(src: &Path, dst: &Path, mode: LinkMode) -> io::Result<LinkKind> {
    if !src.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("link source missing: {}", src.display()),
        ));
    }
    remove_existing(dst)?;

    match mode {
        LinkMode::Hard => hard_link(src, dst).map(|()| LinkKind::HardLink),
        LinkMode::Soft => symlink(src, dst).map(|()| LinkKind::SymLink),
        LinkMode::Copy => copy_file(src, dst).map(|()| LinkKind::Copy),
        LinkMode::Auto => {
            if hard_link(src, dst).is_ok() {
                return Ok(LinkKind::HardLink);
            }
            if symlink(src, dst).is_ok() {
                return Ok(LinkKind::SymLink);
            }
            // Last resort on filesystems that support neither (exFAT, some
            // SMB/NFS mounts): a cheap byte copy beats re-encoding a PNG.
            copy_file(src, dst).map(|()| LinkKind::Copy)
        }
    }
}

/// Removes whatever currently sits at `dst`, including dangling symlinks
/// (`Path::exists` follows links, so it cannot be used here).
fn remove_existing(dst: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(dst) {
        Ok(_) => std::fs::remove_file(dst),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn hard_link(src: &Path, dst: &Path) -> io::Result<()> {
    std::fs::hard_link(src, dst)
}

fn copy_file(src: &Path, dst: &Path) -> io::Result<()> {
    std::fs::copy(src, dst).map(|_| ())
}

/// Creates a symlink at `dst` pointing at `src`.
///
/// Inside one directory the link stores just the file name (what `ln -s`
/// does), so the output folder stays relocatable; otherwise the absolute
/// source path is used.
fn symlink(src: &Path, dst: &Path) -> io::Result<()> {
    let target = symlink_target(src, dst);
    platform_symlink(&target, dst)
}

fn symlink_target(src: &Path, dst: &Path) -> PathBuf {
    match (src.parent(), dst.parent()) {
        (Some(sp), Some(dp)) if sp == dp => src
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| src.to_path_buf()),
        _ => src.to_path_buf(),
    }
}

#[cfg(unix)]
fn platform_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn platform_symlink(target: &Path, link: &Path) -> io::Result<()> {
    // Needs Developer Mode or SeCreateSymbolicLinkPrivilege; callers that use
    // `Auto` fall back to a hardlink/copy when this is denied.
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(not(any(unix, windows)))]
fn platform_symlink(_target: &Path, _link: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symlinks are not supported on this platform",
    ))
}

/// Result of [`relink_identical_frames`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RelinkReport {
    /// `t*.png` files inspected.
    pub scanned: usize,
    /// Files replaced by a link to an earlier identical frame.
    pub linked: usize,
    /// Bytes that were on disk before relinking and are now shared.
    pub freed_bytes: u64,
    /// Mechanism used for the links that were created.
    pub kind: Option<LinkKind>,
}

/// Rewrites an existing frame folder so byte-identical `t*.png` frames become
/// links instead of duplicates — the `ln`-style pass for runs that were
/// produced before linking (or on a filesystem where it silently copied).
///
/// Frames are processed in numeric order and each duplicate is linked to the
/// *first* frame with the same content, so `t5.png` points at `t2.png`, exactly
/// like running `ln t2.png t5.png`.
pub fn relink_identical_frames(dir: &Path, mode: LinkMode) -> io::Result<RelinkReport> {
    let mut frames: Vec<(i64, PathBuf)> = std::fs::read_dir(dir)?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            let index = frame_index(name)?;
            Some((index, path))
        })
        .collect();
    frames.sort_by_key(|(index, _)| *index);

    let mut report = RelinkReport::default();

    // Group by file size first: two byte-identical frames are necessarily the
    // same length, so a frame whose size is unique in the folder can never be a
    // duplicate — and the previous version still read and hashed every frame,
    // i.e. the whole output folder (gigabytes) for a single-file answer.
    let mut by_size: std::collections::HashMap<u64, Vec<PathBuf>> =
        std::collections::HashMap::new();
    for (_, path) in frames {
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue; // symlink to a missing frame: leave it alone
        }
        report.scanned += 1;
        by_size.entry(meta.len()).or_default().push(path);
    }

    for (size, group) in by_size {
        if group.len() < 2 {
            continue; // nothing to compare against: skip reading the file
        }
        let mut canonical: std::collections::HashMap<[u8; 16], PathBuf> =
            std::collections::HashMap::new();
        // `frames` was sorted by frame number and the groups preserve that
        // order, so each duplicate is linked to the *first* frame with the same
        // content, exactly like running `ln t2.png t5.png`.
        for path in group {
            let Ok(digest) = file_digest(&path) else {
                continue;
            };
            match canonical.get(&digest) {
                Some(src) => {
                    if same_file(src, &path) {
                        // Already a link to that frame: nothing to reclaim, and
                        // re-running the command must stay a no-op.
                        continue;
                    }
                    if let Ok(kind) = link_or_copy(src, &path, mode) {
                        report.linked += 1;
                        report.freed_bytes += size;
                        report.kind = Some(kind);
                    }
                }
                None => {
                    canonical.insert(digest, path);
                }
            }
        }
    }
    Ok(report)
}

/// Whether two names already refer to the same file (so relinking is a no-op).
fn same_file(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ma.dev() == mb.dev() && ma.ino() == mb.ino()
    }
    #[cfg(not(unix))]
    {
        // Hardlinks share the inode, hence the same length and mtime; a byte
        // copy written by `std::fs::copy` gets a fresh mtime, so genuine
        // duplicates are still relinked.
        ma.len() == mb.len() && ma.modified().ok() == mb.modified().ok()
    }
}

/// `t123.png` → `Some(123)`; anything else → `None`.
fn frame_index(name: &str) -> Option<i64> {
    let digits = name.strip_prefix('t')?.strip_suffix(".png")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// MD5 of a file, streamed so multi-megabyte frames never land in memory.
fn file_digest(path: &Path) -> io::Result<[u8; 16]> {
    use md5::{Digest, Md5};
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hasher.finalize().into())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sample(dir: &Path) -> PathBuf {
        let src = dir.join("t1.png");
        std::fs::write(&src, b"frame-bytes").unwrap();
        src
    }

    #[test]
    fn hardlink_shares_the_inode() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let src = sample(dir.path());
        let dst = dir.path().join("t2.png");

        // A stale destination from an earlier run must be replaced.
        std::fs::write(&dst, b"stale").unwrap();

        assert_eq!(
            link_or_copy(&src, &dst, LinkMode::Hard).unwrap(),
            LinkKind::HardLink
        );
        let a = std::fs::metadata(&src).unwrap();
        let b = std::fs::metadata(&dst).unwrap();
        assert_eq!(a.ino(), b.ino());
        assert_eq!(b.nlink(), 2);
        assert_eq!(std::fs::read(&dst).unwrap(), b"frame-bytes");
    }

    #[test]
    fn symlink_is_relative_and_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let src = sample(dir.path());
        let dst = dir.path().join("t2.png");

        assert_eq!(
            link_or_copy(&src, &dst, LinkMode::Soft).unwrap(),
            LinkKind::SymLink
        );
        let meta = std::fs::symlink_metadata(&dst).unwrap();
        assert!(meta.file_type().is_symlink());
        assert_eq!(
            std::fs::read_link(&dst).unwrap(),
            PathBuf::from("t1.png"),
            "links inside one folder must stay relative"
        );
        assert_eq!(std::fs::read(&dst).unwrap(), b"frame-bytes");
    }

    #[test]
    fn auto_prefers_hardlink_and_copy_mode_copies() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let src = sample(dir.path());

        let hard = dir.path().join("t2.png");
        assert_eq!(
            link_or_copy(&src, &hard, LinkMode::Auto).unwrap(),
            LinkKind::HardLink
        );

        let copied = dir.path().join("t3.png");
        assert_eq!(
            link_or_copy(&src, &copied, LinkMode::Copy).unwrap(),
            LinkKind::Copy
        );
        assert_eq!(std::fs::read(&copied).unwrap(), b"frame-bytes");
        assert_eq!(std::fs::metadata(&src).unwrap().nlink(), 2);
    }

    #[test]
    fn missing_source_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = link_or_copy(
            &dir.path().join("nope.png"),
            &dir.path().join("t1.png"),
            LinkMode::Auto,
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    /// A folder of duplicated frames (what an older/pre-linking run left
    /// behind) is converted to real `ln`-style links for identical frames only.
    #[test]
    fn relink_turns_duplicate_frames_into_links() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("t1.png"), b"same").unwrap();
        std::fs::write(dir.path().join("t2.png"), b"same").unwrap();
        std::fs::write(dir.path().join("t3.png"), b"other").unwrap();
        std::fs::write(dir.path().join("t4.png"), b"same").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"same").unwrap();

        let report = relink_identical_frames(dir.path(), LinkMode::Auto).unwrap();
        assert_eq!(report.scanned, 4, "only t*.png frames are considered");
        assert_eq!(report.linked, 2, "t2 and t4 are duplicates of t1");
        assert!(report.freed_bytes >= 8);
        assert_eq!(report.kind, Some(LinkKind::HardLink));

        let first = std::fs::metadata(dir.path().join("t1.png")).unwrap();
        for dup in ["t2.png", "t4.png"] {
            let meta = std::fs::metadata(dir.path().join(dup)).unwrap();
            assert_eq!(meta.ino(), first.ino(), "{dup} must share t1's inode");
        }
        assert_ne!(
            std::fs::metadata(dir.path().join("t3.png")).unwrap().ino(),
            first.ino(),
            "a different frame must never be linked"
        );
        // Non-frame files are untouched.
        assert!(dir.path().join("notes.txt").is_file());

        // Idempotent: a second pass finds nothing new to do.
        let again = relink_identical_frames(dir.path(), LinkMode::Auto).unwrap();
        assert_eq!(again.linked, 0);
    }

    #[test]
    fn relink_supports_symlinks_and_empty_folders() {
        let dir = tempfile::tempdir().unwrap();
        let empty = relink_identical_frames(dir.path(), LinkMode::Auto).unwrap();
        assert_eq!(empty.scanned, 0);

        std::fs::write(dir.path().join("t1.png"), b"f").unwrap();
        std::fs::write(dir.path().join("t2.png"), b"f").unwrap();
        let report = relink_identical_frames(dir.path(), LinkMode::Soft).unwrap();
        assert_eq!(report.kind, Some(LinkKind::SymLink));
        assert!(std::fs::symlink_metadata(dir.path().join("t2.png"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(dir.path().join("t2.png")).unwrap(), b"f");
    }

    #[test]
    fn frame_index_parsing() {
        assert_eq!(frame_index("t1.png"), Some(1));
        assert_eq!(frame_index("t042.png"), Some(42));
        assert_eq!(frame_index("t.png"), None);
        assert_eq!(frame_index("tx.png"), None);
        assert_eq!(frame_index("metadata.json"), None);
    }

    #[test]
    fn mode_parsing_round_trips() {
        assert_eq!(LinkMode::parse("hard"), LinkMode::Hard);
        assert_eq!(LinkMode::parse("ln -s"), LinkMode::Soft);
        assert_eq!(LinkMode::parse("SYMLINK"), LinkMode::Soft);
        assert_eq!(LinkMode::parse("copy"), LinkMode::Copy);
        assert_eq!(LinkMode::parse("whatever"), LinkMode::Auto);
        for m in LinkMode::all() {
            assert_eq!(LinkMode::parse(m.as_str()), *m);
        }
    }
}

#[cfg(all(test, not(unix)))]
mod tests {
    use super::*;

    #[test]
    fn auto_links_or_copies_and_never_loses_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("t1.png");
        std::fs::write(&src, b"frame-bytes").unwrap();

        let dst = dir.path().join("t2.png");
        std::fs::write(&dst, b"stale").unwrap();
        let kind = link_or_copy(&src, &dst, LinkMode::Auto).unwrap();

        // Windows: hardlink on NTFS; symlink only with Developer Mode; both
        // are fine, a copy is the last resort.
        assert!(matches!(
            kind,
            LinkKind::HardLink | LinkKind::SymLink | LinkKind::Copy
        ));
        assert_eq!(std::fs::read(&dst).unwrap(), b"frame-bytes");
    }
}
