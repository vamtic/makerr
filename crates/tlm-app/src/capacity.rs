//! Free memory / free disk probes and the pre-render capacity estimate.
//!
//! The GUI warns before a run when the chosen output mode cannot fit: a direct
//! pipe keeps whole frames in RAM, while a PNG sequence writes one PNG per
//! frame to the output folder.

use std::path::{Path, PathBuf};

/// Timestamps per day the site serves, used when the exact frame count is not
/// known yet. Measured runs land a little below this (about 42/day).
pub const FRAMES_PER_DAY_ESTIMATE: u64 = 48;

/// Bytes of RAM the OS considers free for new allocations.
pub fn available_ram_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let info = std::fs::read_to_string("/proc/meminfo").ok()?;
        let line = info.lines().find(|l| l.starts_with("MemAvailable:"))?;
        let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kib * 1024)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // macOS and the BSDs: free physical pages.
        let pages = unsafe { libc::sysconf(libc::_SC_AVPHYS_PAGES) };
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if pages > 0 && page_size > 0 {
            return Some(pages as u64 * page_size as u64);
        }
        None
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if unsafe { GlobalMemoryStatusEx(&mut status) } != 0 {
            return Some(status.ullAvailPhys);
        }
        None
    }
    #[cfg(not(any(target_os = "linux", unix, windows)))]
    {
        None
    }
}

/// Free bytes on the filesystem backing `path`, walking up to the nearest
/// existing ancestor (the output folder may not exist before the first run).
pub fn free_disk_bytes(path: &Path) -> Option<u64> {
    let existing = nearest_existing(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c_path = std::ffi::CString::new(existing.as_os_str().as_bytes()).ok()?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
            return None;
        }
        let block = if stat.f_frsize > 0 {
            stat.f_frsize as u64
        } else {
            stat.f_bsize as u64
        };
        Some(stat.f_bavail as u64 * block)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let wide: Vec<u16> = existing
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut free: u64 = 0;
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut free,
            )
        };
        if ok != 0 {
            return Some(free);
        }
        None
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = existing;
        None
    }
}

fn nearest_existing(path: &Path) -> Option<PathBuf> {
    let mut current = if path.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        path.to_path_buf()
    };
    loop {
        if current.exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
        if current.as_os_str().is_empty() {
            current.push(".");
        }
    }
}

/// What a run is expected to need, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    pub frames: u64,
    pub frame_bytes: u64,
    pub ram_bytes: u64,
    pub disk_bytes: u64,
}

/// Estimates the peak RAM and disk footprint of one run.
///
/// * `pixels` – region size in pixels.
/// * `frames` – expected frame count.
/// * `streaming` – frames are piped straight into ffmpeg (no frame files).
/// * `save_png` – every frame is also written to the output folder as PNG.
pub fn estimate(pixels: u64, frames: u64, streaming: bool, save_png: bool) -> Estimate {
    // RGBA frame.
    let frame_bytes = pixels.saturating_mul(4);
    // Canvas + day base are always alive; the renderer adds one output frame
    // per sink that keeps a frame alive (encoder hand-off, PNG writer).
    let copies = 2 + u64::from(streaming) + u64::from(save_png);
    let ram_bytes = frame_bytes.saturating_mul(copies);
    // Pixel art PNGs measured ~0.93 B/px at quality 0; 1 B/px is a safe upper
    // bound and keeps the estimate readable.
    let disk_bytes = if save_png {
        pixels.saturating_mul(frames)
    } else {
        0
    };
    Estimate {
        frames,
        frame_bytes,
        ram_bytes,
        disk_bytes,
    }
}

/// Human-readable size using decimal units, e.g. `3.4 GB`.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_only_is_ram_bound() {
        let est = estimate(9026 * 7833, 339, true, false);
        assert_eq!(est.frame_bytes, 9026 * 7833 * 4);
        assert_eq!(est.ram_bytes, est.frame_bytes * 3);
        assert_eq!(est.disk_bytes, 0);
    }

    #[test]
    fn png_sequence_is_disk_bound() {
        let est = estimate(1000 * 1000, 100, true, true);
        assert_eq!(est.ram_bytes, 4 * 4_000_000);
        assert_eq!(est.disk_bytes, 100_000_000);
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1_500), "1.5 kB");
        assert_eq!(format_bytes(24_000_000_000), "24.0 GB");
    }

    #[test]
    fn nearest_existing_walks_up() {
        let dir = std::env::temp_dir().join("tlm-capacity-test/a/b");
        assert!(nearest_existing(&dir).is_some());
    }

    #[test]
    fn probes_do_not_panic() {
        let _ = available_ram_bytes();
        let _ = free_disk_bytes(Path::new("."));
    }
}
