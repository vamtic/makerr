//! The shared on-disk tile cache.
//!
//! Key format and layout are byte-compatible with the V1 application:
//! each entry is `md5("{storage}_{canvas}_{date}_{x}_{y}_{ts}") + ".dat"`
//! inside the cache root, so the Rust app and the V1 app can share one cache.
//!
//! An empty timestamp (the preview's "day base" request) normalizes to
//! "base", exactly like `loadCachedTile`/`saveCachedTile` did.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use md5::{Digest, Md5};

mod packed;

/// Length of the packed-cache key (an MD5 digest, matching the legacy
/// `<md5>.dat` file names).
pub const CACHE_KEY_LEN: usize = 16;
/// A packed-cache key.
pub type CacheKey = [u8; CACHE_KEY_LEN];

/// Memoized cache root, keyed by the `TIMELAPSE_CACHE_DIR` override so tests
/// that flip the env var still see a fresh directory. Resolving the path is
/// otherwise a per-tile `create_dir_all` syscall, which showed up in profiles
/// of large runs.
fn dir_memo() -> &'static Mutex<Option<(String, PathBuf)>> {
    static MEMO: OnceLock<Mutex<Option<(String, PathBuf)>>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(None))
}

/// Environment variable that overrides the cache root (used by tests).
pub const CACHE_DIR_ENV: &str = "TIMELAPSE_CACHE_DIR";

/// Default cap on cache growth enforced by [`prune`]: 512 MiB, like the
/// V1 app's `kDefaultCacheMaxBytes`.
pub const DEFAULT_CACHE_MAX_BYTES: u64 = 512 * 1024 * 1024;

static CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static NETWORK_FETCHES: AtomicU64 = AtomicU64::new(0);
static BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);

pub fn cache_hit_count() -> u64 {
    CACHE_HITS.load(Ordering::Relaxed)
}

pub fn network_fetch_count() -> u64 {
    NETWORK_FETCHES.load(Ordering::Relaxed)
}

pub fn bytes_downloaded() -> u64 {
    BYTES_TOTAL.load(Ordering::Relaxed)
}

pub fn record_cache_hit() {
    CACHE_HITS.fetch_add(1, Ordering::Relaxed);
}

/// Counts a whole frame's cache hits with one atomic update.
pub fn record_cache_hits(hits: u64) {
    if hits > 0 {
        CACHE_HITS.fetch_add(hits, Ordering::Relaxed);
    }
}

pub fn record_network_fetch(bytes: u64) {
    NETWORK_FETCHES.fetch_add(1, Ordering::Relaxed);
    BYTES_TOTAL.fetch_add(bytes, Ordering::Relaxed);
}

pub fn reset_stats() {
    CACHE_HITS.store(0, Ordering::Relaxed);
    NETWORK_FETCHES.store(0, Ordering::Relaxed);
    BYTES_TOTAL.store(0, Ordering::Relaxed);
}

/// The cache root directory, honoring `TIMELAPSE_CACHE_DIR`.
///
/// Default: `<system cache>/TimelapseMaker/tilecache` — the same location the
/// V1 app used, so the caches are interchangeable.
pub fn cache_dir() -> PathBuf {
    let override_dir = std::env::var(CACHE_DIR_ENV).unwrap_or_default();
    {
        let memo = dir_memo().lock().unwrap_or_else(|p| p.into_inner());
        if let Some((key, dir)) = memo.as_ref() {
            if key == &override_dir {
                return dir.clone();
            }
        }
    }
    let dir = if !override_dir.is_empty() {
        PathBuf::from(&override_dir)
    } else {
        let base =
            dirs::cache_dir().unwrap_or_else(|| std::env::temp_dir().join("timelapse-cache"));
        base.join("TimelapseMaker").join("tilecache")
    };
    let _ = std::fs::create_dir_all(&dir);
    *dir_memo().lock().unwrap_or_else(|p| p.into_inner()) = Some((override_dir, dir.clone()));
    dir
}

/// The on-disk path for one cached tile.
///
/// The storage base is part of the key so tiles from different sites that
/// happen to share a numeric canvas id (e.g. PixMap's "0") can never collide.
pub fn cache_path(
    storage_base: &str,
    canvas_id: &str,
    date: &str,
    tile_x: i64,
    tile_y: i64,
    timestamp: &str,
) -> PathBuf {
    cache_path_in(
        &cache_dir(),
        storage_base,
        canvas_id,
        date,
        tile_x,
        tile_y,
        timestamp,
    )
}

/// Same as [`cache_path`] but with a pre-resolved cache root, so a frame with
/// hundreds of tiles resolves the directory once instead of per tile.
pub fn cache_path_in(
    dir: &Path,
    storage_base: &str,
    canvas_id: &str,
    date: &str,
    tile_x: i64,
    tile_y: i64,
    timestamp: &str,
) -> PathBuf {
    let ts = normalize_ts(timestamp);
    let key = cache_key(storage_base, canvas_id, date, tile_x, tile_y, ts);
    dir.join(format!("{}.dat", hex_key(&key)))
}

/// The packed-cache key for one tile. `timestamp` is normalized exactly like
/// the legacy cache key, so both caches map one-to-one.
pub fn cache_key(
    storage_base: &str,
    canvas_id: &str,
    date: &str,
    tile_x: i64,
    tile_y: i64,
    timestamp: &str,
) -> CacheKey {
    let ts = normalize_ts(timestamp);
    let key = format!("{storage_base}_{canvas_id}_{date}_{tile_x}_{tile_y}_{ts}");
    let mut hash = Md5::new();
    hash.update(key.as_bytes());
    hash.finalize().into()
}

fn hex_key(key: &CacheKey) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut name = String::with_capacity(CACHE_KEY_LEN * 2);
    for byte in key {
        name.push(HEX[(byte >> 4) as usize] as char);
        name.push(HEX[(byte & 0x0f) as usize] as char);
    }
    name
}

/// An empty timestamp maps to the downloader's "base" key so the preview and
/// the pipeline share entries.
pub fn normalize_ts(timestamp: &str) -> &str {
    if timestamp.is_empty() {
        "base"
    } else {
        timestamp
    }
}

/// Loads one cached tile, or `None` when absent/empty.
pub fn load_tile(
    storage_base: &str,
    canvas_id: &str,
    date: &str,
    tile_x: i64,
    tile_y: i64,
    timestamp: &str,
) -> Option<Vec<u8>> {
    load_file(&cache_path(
        storage_base,
        canvas_id,
        date,
        tile_x,
        tile_y,
        timestamp,
    ))
}

pub fn load_file(path: &Path) -> Option<Vec<u8>> {
    match load_status(path) {
        CacheStatus::Data(data) => Some(data),
        CacheStatus::Absent | CacheStatus::Missing => None,
    }
}

// ----------------------------------------------------------------------------
// Packed cache entry API (used by the pipeline and the preview fetch path)
// ----------------------------------------------------------------------------

fn packed_store() -> Option<std::sync::Arc<packed::PackedCache>> {
    packed::store_for(&cache_dir())
}

fn legacy_path_for_key(key: &CacheKey) -> PathBuf {
    cache_dir().join(format!("{}.dat", hex_key(key)))
}

/// Loads one tile by key from the packed cache, falling back to the legacy
/// loose-file cache while its background migration is still running.
pub fn load_entry(key: &CacheKey) -> CacheStatus {
    if let Some(store) = packed_store() {
        if let Some(value) = store.load(key) {
            return match value.first() {
                Some(&0) => CacheStatus::Data(value[1..].to_vec()),
                Some(&1) => CacheStatus::Absent,
                _ => CacheStatus::Missing,
            };
        }
        if store.legacy_migration_done() {
            return CacheStatus::Missing;
        }
    }
    load_status(&legacy_path_for_key(key))
}

/// Loads a whole frame's tiles with one cache read transaction instead of one
/// per tile, and accounts for the hits in a single atomic update.
pub fn load_entries_batch(keys: &[CacheKey]) -> Vec<CacheStatus> {
    if keys.is_empty() {
        return Vec::new();
    }
    let Some(store) = packed_store() else {
        return keys.iter().map(load_entry).collect();
    };
    let mut statuses = Vec::with_capacity(keys.len());
    for value in store.load_batch(keys) {
        statuses.push(match value {
            Some(v) => match v.first() {
                Some(&0) => CacheStatus::Data(v[1..].to_vec()),
                Some(&1) => CacheStatus::Absent,
                _ => CacheStatus::Missing,
            },
            // The store consulted the legacy loose files itself while their
            // migration is still running, so a miss here is a real miss.
            None => CacheStatus::Missing,
        });
    }
    statuses
}

/// Stores one tile payload in the packed cache (or the legacy file cache when
/// redb is unavailable).
pub fn save_entry(key: &CacheKey, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    if let Some(store) = packed_store() {
        store.save(key, data);
    } else {
        save_file(&legacy_path_for_key(key), data);
    }
}

/// Stores many fetched tiles in a single redb transaction. Falls back to
/// parallel-friendly loose files when the packed cache is unavailable.
pub fn save_entries_batch(entries: &[(CacheKey, Vec<u8>)]) {
    if entries.is_empty() {
        return;
    }
    if let Some(store) = packed_store() {
        store.save_batch(entries);
    } else {
        for (key, data) in entries {
            save_file(&legacy_path_for_key(key), data);
        }
    }
}

/// Remembers a definitive 404 for one tile.
pub fn mark_absent_entry(key: &CacheKey) {
    if let Some(store) = packed_store() {
        store.mark_absent(key);
    } else {
        let path = legacy_path_for_key(key);
        if !path.is_file() {
            let _ = std::fs::File::create(path.with_extension("404"));
        }
    }
}

/// Remembers definitive 404s for a whole frame in one transaction.
pub fn mark_absent_entries_batch(keys: &[CacheKey]) {
    if keys.is_empty() {
        return;
    }
    if let Some(store) = packed_store() {
        store.mark_absent_batch(keys);
    } else {
        for key in keys {
            let path = legacy_path_for_key(key);
            if !path.is_file() {
                let _ = std::fs::File::create(path.with_extension("404"));
            }
        }
    }
}

/// What the cache knows about one tile.
#[derive(Debug, Clone)]
pub enum CacheStatus {
    /// The payload is on disk.
    Data(Vec<u8>),
    /// A previous request got a definitive 404/410. Remembering it avoids
    /// re-requesting a tile that will never exist for that date/spelling.
    Absent,
    /// Never fetched (or the entry was pruned).
    Missing,
}

/// True when a `404` sidecar marker exists for this cache entry.
pub fn is_absent(path: &Path) -> bool {
    absent_path(path).is_file()
}

/// Marks a cache entry as definitively absent. The marker is a separate
/// `.404` file so the V1 app can keep sharing the `.dat` cache format (it
/// simply ignores unknown files).
pub fn mark_absent(path: &Path) {
    // Never shadow a real payload: if both exist, the data is authoritative.
    if path.is_file() {
        return;
    }
    let _ = std::fs::File::create(absent_path(path));
}

fn absent_path(path: &Path) -> PathBuf {
    path.with_extension("404")
}

/// Loads one cache entry together with the negative-cache state.
pub fn load_status(path: &Path) -> CacheStatus {
    match std::fs::read(path) {
        Ok(data) if !data.is_empty() => CacheStatus::Data(data),
        _ if is_absent(path) => CacheStatus::Absent,
        _ => CacheStatus::Missing,
    }
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique temp path for an atomic cache write.
///
/// A fixed `entry.dat.tmp` name meant two threads (or the V1 app and the Rust
/// app sharing one cache) could interleave rename/delete on the same temp file:
/// one write would vanish, and the other could rename a half-written file.
fn temp_path(path: &Path) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("cache-entry");
    path.with_file_name(format!(".{name}.{}.{n}.tmp", std::process::id()))
}

/// Stores one tile. Empty payloads are never written (404s must not poison
/// the cache). Writes through a temp file + rename so a crash mid-write can
/// never leave a truncated entry behind.
///
/// Deliberately **no `fsync`**: the cache is disposable (a lost entry only
/// costs one re-download), while `sync_all` per tile forced a disk flush for
/// every single tile of every frame and dominated download time on spinning
/// disks and SD cards. Frame PNGs are still written atomically by the
/// pipeline's own save path.
pub fn save_tile(
    storage_base: &str,
    canvas_id: &str,
    date: &str,
    tile_x: i64,
    tile_y: i64,
    timestamp: &str,
    data: &[u8],
) {
    save_file(
        &cache_path(storage_base, canvas_id, date, tile_x, tile_y, timestamp),
        data,
    );
}

pub fn save_file(path: &Path, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    let tmp = temp_path(path);
    let saved = match std::fs::File::create(&tmp) {
        Ok(mut f) => f.write_all(data).is_ok() && std::fs::rename(&tmp, path).is_ok(),
        Err(_) => false,
    };
    if saved {
        // A successful fetch overrides a stale negative-cache marker.
        let _ = std::fs::remove_file(absent_path(path));
        return;
    }
    let _ = std::fs::remove_file(&tmp);
}

/// One `*.dat` entry discovered on disk.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub path: PathBuf,
    pub size: u64,
    pub modified: std::time::SystemTime,
}

/// Lists every cache entry (one directory scan shared by both prune passes).
pub fn list_entries() -> Vec<CacheEntry> {
    let mut out = Vec::new();
    let Ok(read) = std::fs::read_dir(cache_dir()) else {
        return out;
    };
    for entry in read.flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str());
        // `.dat` payloads plus the negative-cache `.404` markers and stale
        // `.tmp` files left behind by an interrupted write. Pruning/clearing
        // all of them keeps the cache directory healthy.
        if !matches!(ext, Some("dat") | Some("404") | Some("tmp")) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let modified = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        out.push(CacheEntry {
            path,
            size: meta.len(),
            modified,
        });
    }
    out
}

/// Prunes the cache: drops entries older than `max_age_days` (when > 0),
/// then evicts least-recently-modified entries until the total size is at
/// or below `max_bytes` (when > 0). Returns the number of removed files.
pub fn prune(max_age_days: i32, max_bytes: u64) -> usize {
    let packed_removed = packed_store()
        .map(|s| s.prune(max_age_days, max_bytes))
        .unwrap_or(0);

    let mut entries = list_entries();
    let mut removed = 0usize;

    if max_age_days > 0 {
        let cutoff = std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(
                max_age_days as u64 * 24 * 3600,
            ))
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        // Files that fail to remove are kept in the list so the size
        // accounting below still sees them.
        entries.retain(|e| {
            if e.modified < cutoff && std::fs::remove_file(&e.path).is_ok() {
                removed += 1;
                return false;
            }
            true
        });
    }

    if max_bytes > 0 && !entries.is_empty() {
        entries.sort_by_key(|e| e.modified);
        let total: u64 = entries.iter().map(|e| e.size).sum();
        let mut total = total;
        for e in &entries {
            if total <= max_bytes {
                break;
            }
            if std::fs::remove_file(&e.path).is_ok() {
                total -= e.size;
                removed += 1;
            }
        }
    }

    packed_removed + removed
}

/// Removes every cache entry. Returns how many files were deleted.
pub fn clear() -> usize {
    let packed_removed = packed_store().map(|s| s.clear()).unwrap_or(0);
    let mut removed = 0usize;
    for e in list_entries() {
        if std::fs::remove_file(&e.path).is_ok() {
            removed += 1;
        }
    }
    packed_removed + removed
}

/// `(total_bytes, entry_count)` of the cache root, counting payloads only
/// (404 markers are bookkeeping, not cached tiles).
pub fn stats() -> (u64, usize) {
    let (mut bytes, mut count) = packed_store().map(|s| s.stats()).unwrap_or((0, 0));
    let entries = list_entries();
    for e in entries {
        if e.path.extension().and_then(|x| x.to_str()) == Some("dat") {
            bytes += e.size;
            count += 1;
        }
    }
    (bytes, count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};
    use std::time::Duration;

    /// The cache root comes from a process-global env var, so tests that
    /// touch it must run one at a time.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn with_cache_dir(f: impl FnOnce(&Path)) {
        let _guard = env_lock();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(CACHE_DIR_ENV, dir.path());
        f(dir.path());
        std::env::remove_var(CACHE_DIR_ENV);
    }

    fn make_file(path: &Path, age_days: u64) {
        std::fs::write(path, b"x").unwrap();
        stamp_age(path, age_days);
    }

    fn stamp_age(path: &Path, age_days: u64) {
        let old = std::time::SystemTime::now() - Duration::from_secs(age_days * 24 * 3600);
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
    }

    // The memoized cache root must still honor env-var changes (tests swap it
    // per case); a stale memo would silently write into another test's dir.
    #[test]
    fn cache_dir_tracks_env_override() {
        let _guard = env_lock();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();

        std::env::set_var(CACHE_DIR_ENV, a.path());
        assert_eq!(cache_dir(), a.path());
        std::env::set_var(CACHE_DIR_ENV, b.path());
        assert_eq!(cache_dir(), b.path());
        std::env::remove_var(CACHE_DIR_ENV);
        let default = cache_dir();
        assert_ne!(default, b.path());
        assert!(default.ends_with("TimelapseMaker/tilecache"));
    }

    // A pre-resolved directory must hash to exactly the same file name as the
    // per-tile helper (the pipeline relies on this for cache hits).
    #[test]
    fn cache_path_in_matches_cache_path() {
        with_cache_dir(|dir| {
            assert_eq!(
                cache_path_in(dir, "storage.gs-os.com", "0", "20000103", 131, 124, "base"),
                cache_path("storage.gs-os.com", "0", "20000103", 131, 124, "")
            );
        });
    }

    #[test]
    fn prune_keeps_new_and_removes_old() {
        with_cache_dir(|dir| {
            make_file(&dir.join("old1.dat"), 30);
            make_file(&dir.join("new1.dat"), 0);
            make_file(&dir.join("other.dat"), 1);

            prune(7, DEFAULT_CACHE_MAX_BYTES);

            assert!(!dir.join("old1.dat").exists());
            assert!(dir.join("new1.dat").exists());
            assert!(dir.join("other.dat").exists());
        });
    }

    #[test]
    fn zero_ttl_keeps_everything() {
        with_cache_dir(|dir| {
            make_file(&dir.join("keepme.dat"), 100);
            prune(0, DEFAULT_CACHE_MAX_BYTES);
            assert!(dir.join("keepme.dat").exists());
        });
    }

    #[test]
    fn size_cap_evicts_oldest_first() {
        with_cache_dir(|dir| {
            // Write the sizes first, then stamp ages: rewriting after
            // make_file would reset the mtimes and shuffle the eviction
            // order.
            std::fs::write(dir.join("a.dat"), vec![0u8; 100]).unwrap();
            std::fs::write(dir.join("b.dat"), vec![0u8; 100]).unwrap();
            std::fs::write(dir.join("c.dat"), vec![0u8; 100]).unwrap();
            stamp_age(&dir.join("a.dat"), 10);
            stamp_age(&dir.join("b.dat"), 5);
            stamp_age(&dir.join("c.dat"), 0);
            // Cap below the total so exactly the oldest two must go.
            prune(0, 150);
            assert!(!dir.join("a.dat").exists());
            assert!(!dir.join("b.dat").exists());
            assert!(dir.join("c.dat").exists());
        });
    }

    // The GUI preview and the timelapse downloads share one tile cache.
    #[test]
    fn shared_tile_cache_round_trip() {
        with_cache_dir(|_| {
            let payload = b"TILE-BYTES-1".to_vec();
            let pixmap = "pixmap.fun/history/chunk";
            save_tile(pixmap, "0", "20000103", 157, 73, "", &payload);

            // Round trip for the same coordinates.
            assert_eq!(
                load_tile(pixmap, "0", "20000103", 157, 73, ""),
                Some(payload.clone())
            );

            // An empty timestamp (the preview's base frame) and the
            // downloader's "base" spelling must resolve to the same entry.
            assert_eq!(
                load_tile(pixmap, "0", "20000103", 157, 73, "base"),
                Some(payload.clone())
            );

            // No collisions across site / date / tile.
            assert_eq!(
                load_tile("storage.gs-os.com", "0", "20000103", 157, 73, ""),
                None
            );
            assert_eq!(load_tile(pixmap, "0", "20000102", 157, 73, ""), None);
            assert_eq!(load_tile(pixmap, "0", "20000103", 158, 73, ""), None);

            // Empty payloads are never stored (404s must not poison the cache).
            save_tile(pixmap, "0", "20000103", 158, 74, "", &[]);
            assert_eq!(load_tile(pixmap, "0", "20000103", 158, 74, ""), None);
        });
    }

    // A definitive 404 is remembered without poisoning the `.dat` payload
    // cache (the V1 app must keep sharing the same directory).
    #[test]
    fn negative_cache_markers_round_trip_and_clear_on_save() {
        with_cache_dir(|_| {
            let path = cache_path("storage.gs-os.com", "0", "20000103", 1, 2, "base");
            assert!(matches!(load_status(&path), CacheStatus::Missing));
            mark_absent(&path);
            assert!(is_absent(&path));
            assert!(matches!(load_status(&path), CacheStatus::Absent));

            // A later successful fetch overrides the marker.
            save_file(&path, b"data");
            assert!(!is_absent(&path));
            assert_eq!(load_file(&path), Some(b"data".to_vec()));
        });
    }

    /// The packed cache must behave exactly like the loose-file cache for
    /// payloads and definitive 404 markers.
    #[test]
    fn packed_cache_round_trip_and_negative_markers() {
        with_cache_dir(|_| {
            let key = cache_key("storage.example", "0", "20000105", 3, 4, "");
            assert!(matches!(load_entry(&key), CacheStatus::Missing));

            save_entry(&key, b"tile-payload");
            match load_entry(&key) {
                CacheStatus::Data(data) => assert_eq!(data, b"tile-payload"),
                other => panic!("expected data, got {other:?}"),
            }

            // Empty payloads never poison the cache.
            let empty = cache_key("storage.example", "0", "20000105", 3, 5, "");
            save_entry(&empty, b"");
            assert!(matches!(load_entry(&empty), CacheStatus::Missing));

            let missing = cache_key("storage.example", "0", "20000105", 3, 6, "");
            mark_absent_entry(&missing);
            assert!(matches!(load_entry(&missing), CacheStatus::Absent));

            // A later payload overrides the 404 marker.
            save_entry(&missing, b"now-exists");
            match load_entry(&missing) {
                CacheStatus::Data(data) => assert_eq!(data, b"now-exists"),
                other => panic!("expected data, got {other:?}"),
            }

            let (bytes, count) = stats();
            assert_eq!(bytes, 22, "two payloads: 12 + 10 bytes");
            assert_eq!(count, 2);
        });
    }

    /// A legacy loose-file tile is readable immediately and is migrated into
    /// `cache.redb` in the background, after which the loose file disappears.
    #[test]
    fn legacy_files_migrate_into_the_packed_cache() {
        with_cache_dir(|dir| {
            let key = cache_key("legacy.example", "0", "20000106", 7, 8, "1200");
            let legacy_path = cache_path("legacy.example", "0", "20000106", 7, 8, "1200");
            save_file(&legacy_path, b"old-tile");

            match load_entry(&key) {
                CacheStatus::Data(data) => assert_eq!(data, b"old-tile"),
                other => panic!("legacy read failed: {other:?}"),
            }

            // The migration worker runs on its own thread.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while legacy_path.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert!(!legacy_path.exists(), "legacy file should be migrated");
            assert!(dir.join("cache.redb").exists());
            match load_entry(&key) {
                CacheStatus::Data(data) => assert_eq!(data, b"old-tile"),
                other => panic!("packed read failed after migration: {other:?}"),
            }
        });
    }

    // The Rust cache must produce the same file names as the V1 app so the
    // two front-ends can share one cache directory.
    #[test]
    fn cache_key_matches_v1_implementation() {
        with_cache_dir(|_| {
            // Expected values produced by the V1 implementation:
            //   key = "storage.gs-os.com_0_20000103_131_124_base"
            let p = cache_path("storage.gs-os.com", "0", "20000103", 131, 124, "");
            let digest: CacheKey = {
                use md5::Digest;
                let mut h = Md5::new();
                h.update(b"storage.gs-os.com_0_20000103_131_124_base");
                h.finalize().into()
            };
            assert_eq!(
                p.file_name().unwrap().to_str().unwrap(),
                format!("{}.dat", hex_key(&digest))
            );
        });
    }
}
