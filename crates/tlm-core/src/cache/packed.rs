//! Embedded packed tile cache (`cache.redb`).
//!
//! Replaces one-file-per-tile caching, which turns a month of history over a
//! large canvas into hundreds of thousands of inodes and makes pruning a
//! filesystem walk. Entries live in a single memory-mapped redb database:
//!
//! * key   = MD5 of the legacy cache key string (16 bytes — byte-compatible
//!   with the legacy `<md5>.dat` file name);
//! * value = one tag byte (`0` payload, `1` definitive 404) followed by the
//!   tile bytes.
//!
//! The legacy loose-file cache is migrated in the background on first open and
//! remains readable during the migration, so upgrading never loses cached
//! tiles or stalls startup. If the database cannot be opened (another process
//! holds the lock, a read-only cache dir, …) every API in this module quietly
//! returns `None` and [`super`] falls back to the loose-file cache.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use redb::{
    Database, DatabaseError, Durability, ReadableDatabase, ReadableTable, StorageError,
    TableDefinition,
};

const TILES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("tiles");
const ACCESS: TableDefinition<&[u8], u64> = TableDefinition::new("access");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const MIGRATED_KEY: &str = "legacy_migrated_v1";
const TAG_DATA: u8 = 0;
const TAG_ABSENT: u8 = 1;

pub(crate) const KEY_LEN: usize = 16;

pub(crate) struct PackedCache {
    db: Database,
    dir: PathBuf,
    legacy_done: AtomicBool,
    saves: AtomicU64,
}

fn open_stores() -> &'static Mutex<HashMap<PathBuf, Arc<PackedCache>>> {
    static STORES: OnceLock<Mutex<HashMap<PathBuf, Arc<PackedCache>>>> = OnceLock::new();
    STORES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Opens (or reuses) the packed cache rooted at `dir`, or `None` when redb
/// cannot be used and the caller should stay on the loose-file cache.
pub(crate) fn store_for(dir: &Path) -> Option<Arc<PackedCache>> {
    let mut stores = open_stores().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(cache) = stores.get(dir) {
        return Some(Arc::clone(cache));
    }
    let db_path = dir.join("cache.redb");
    // Opening the database is cheap; keeping the map lock across it closes a
    // race where two rayon workers both tried to create the same redb file
    // (one of them would fail the file lock and silently fall back to files).
    let db = open_database(&db_path)?;
    let cache = Arc::new(PackedCache {
        db,
        dir: dir.to_path_buf(),
        legacy_done: AtomicBool::new(false),
        saves: AtomicU64::new(0),
    });
    let done = cache.migration_done();
    cache.legacy_done.store(done, Ordering::SeqCst);
    if !done {
        let worker = Arc::clone(&cache);
        let _ = std::thread::Builder::new()
            .name("tlm-cache-migrate".into())
            .spawn(move || worker.migrate_legacy());
    }
    stores.insert(dir.to_path_buf(), Arc::clone(&cache));
    Some(cache)
}

/// Opens (or creates) the packed cache database.
///
/// redb 2 wrote the v2 file format, which redb 3+ refuses to open. The packed
/// cache is disposable — every tile can be re-downloaded — so an old or
/// unreadable `cache.redb` is replaced instead of leaving the app permanently
/// on the slow loose-file path. Databases held by another process fail with
/// `DatabaseAlreadyOpen` and are left alone.
fn open_database(path: &Path) -> Option<Database> {
    match Database::create(path) {
        Ok(db) => Some(db),
        Err(err) if is_disposable_cache_error(&err) => {
            let _ = std::fs::remove_file(path);
            Database::create(path).ok()
        }
        Err(_) => None,
    }
}

/// True for failures that mean the existing `cache.redb` bytes are unusable.
///
/// Only format errors qualify. Permission, locking and I/O errors must fall
/// back to the loose-file cache without deleting anything.
fn is_disposable_cache_error(err: &DatabaseError) -> bool {
    match err {
        DatabaseError::UpgradeRequired(_) => true,
        DatabaseError::Storage(StorageError::Corrupted(_)) => true,
        DatabaseError::Storage(StorageError::Io(err)) => {
            err.kind() == std::io::ErrorKind::InvalidData
        }
        _ => false,
    }
}

impl PackedCache {
    /// True once the one-time legacy migration has finished; while false the
    /// reader must also consult the loose files.
    pub(crate) fn legacy_migration_done(&self) -> bool {
        self.legacy_done.load(Ordering::Acquire)
    }

    fn migration_done(&self) -> bool {
        let Ok(read) = self.db.begin_read() else {
            return false;
        };
        let Ok(table) = read.open_table(META) else {
            return false;
        };
        table.get(MIGRATED_KEY).ok().flatten().is_some()
    }

    fn mark_migrated(&self) {
        let Ok(write) = self.db.begin_write() else {
            return;
        };
        {
            let Ok(mut table) = write.open_table(META) else {
                return;
            };
            let _ = table.insert(MIGRATED_KEY, 1u64);
        }
        let _ = write.commit();
    }

    /// Loads an entry. While the legacy migration is still running the loose
    /// file is consulted first, which closes the insert/delete race: a reader
    /// that misses the file always finds the row the migration committed
    /// before deleting that file.
    pub(crate) fn load(&self, key: &[u8; KEY_LEN]) -> Option<Vec<u8>> {
        if !self.legacy_done.load(Ordering::Acquire) {
            if let Some(value) = self.load_legacy_file(key) {
                return Some(value);
            }
        }
        let read = self.db.begin_read().ok()?;
        let table = read.open_table(TILES).ok()?;
        let guard = table.get(key.as_slice()).ok().flatten()?;
        Some(guard.value().to_vec())
    }

    /// Loads many keys through a single read transaction.
    ///
    /// One frame is one key batch, and the pipeline used to open a read
    /// transaction (and the table handle behind it) per tile. While the legacy
    /// migration is running the loose files are consulted as well, with the
    /// same precedence as [`Self::load`].
    pub(crate) fn load_batch(&self, keys: &[CacheKey]) -> Vec<Option<Vec<u8>>> {
        let mut out: Vec<Option<Vec<u8>>> = vec![None; keys.len()];
        if let Ok(read) = self.db.begin_read() {
            if let Ok(table) = read.open_table(TILES) {
                for (slot, key) in out.iter_mut().zip(keys) {
                    if let Ok(Some(guard)) = table.get(key.as_slice()) {
                        *slot = Some(guard.value().to_vec());
                    }
                }
            }
        }
        if !self.legacy_done.load(Ordering::Acquire) {
            for (slot, key) in out.iter_mut().zip(keys) {
                if slot.is_none() {
                    *slot = self.load_legacy_file(key);
                }
            }
        }
        out
    }

    fn load_legacy_file(&self, key: &[u8; KEY_LEN]) -> Option<Vec<u8>> {
        let dat = self.legacy_path(key).with_extension("dat");
        if let Ok(data) = std::fs::read(&dat) {
            if !data.is_empty() {
                return Some(tagged(TAG_DATA, &data));
            }
        }
        if dat.with_extension("404").is_file() {
            return Some(vec![TAG_ABSENT]);
        }
        None
    }

    fn legacy_path(&self, key: &[u8; KEY_LEN]) -> PathBuf {
        self.dir.join(format!("{}.dat", hex(key)))
    }

    pub(crate) fn save(&self, key: &[u8; KEY_LEN], data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let mut value = Vec::with_capacity(data.len() + 1);
        value.push(TAG_DATA);
        value.extend_from_slice(data);
        if self.save_rows(&[(*key, value.as_slice())]) {
            self.remove_legacy_files(key);
        }
    }

    pub(crate) fn save_batch(&self, entries: &[(CacheKey, Vec<u8>)]) {
        if entries.is_empty() {
            return;
        }
        let mut rows: Vec<(CacheKey, Vec<u8>)> = Vec::with_capacity(entries.len());
        for (key, data) in entries {
            if data.is_empty() {
                continue;
            }
            let mut value = Vec::with_capacity(data.len() + 1);
            value.push(TAG_DATA);
            value.extend_from_slice(data);
            rows.push((*key, value));
        }
        let refs: Vec<(CacheKey, &[u8])> = rows
            .iter()
            .map(|(key, value)| (*key, value.as_slice()))
            .collect();
        if self.save_rows(&refs) {
            for (key, _) in entries {
                self.remove_legacy_files(key);
            }
        }
    }

    fn save_rows(&self, rows: &[(CacheKey, &[u8])]) -> bool {
        let Ok(mut write) = self.db.begin_write() else {
            return false;
        };
        // A tile cache must not pay a disk flush per committed frame: the
        // pipeline commits once per frame (thousands of commits per run) and
        // waits for the commit. Non-durable commits keep the transaction
        // atomic and visible to readers, but redb only persists them when a
        // durable commit follows. Flushing every 256th batch bounds what an
        // unclean shutdown can lose while keeping the fsync cost negligible.
        // The migration and prune paths keep the default (immediate)
        // durability because they delete the files they replace.
        let durability = if self
            .saves
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(256)
        {
            Durability::Immediate
        } else {
            Durability::None
        };
        let _ = write.set_durability(durability);

        {
            let Ok(mut tiles) = write.open_table(TILES) else {
                return false;
            };
            let Ok(mut access) = write.open_table(ACCESS) else {
                return false;
            };
            let now = now_unix();
            for (key, value) in rows {
                if tiles.insert(key.as_slice(), *value).is_err() {
                    return false;
                }
                let _ = access.insert(key.as_slice(), now);
            }
        }
        write.commit().is_ok()
    }

    pub(crate) fn mark_absent(&self, key: &[u8; KEY_LEN]) {
        let _ = self.mark_absent_rows(&[*key]);
    }

    pub(crate) fn mark_absent_batch(&self, keys: &[CacheKey]) {
        let _ = self.mark_absent_rows(keys);
    }

    fn mark_absent_rows(&self, keys: &[CacheKey]) -> bool {
        let Ok(mut write) = self.db.begin_write() else {
            return false;
        };
        // Same trade-off as `save_rows`: negative markers are a cache hint.
        let _ = write.set_durability(Durability::None);

        {
            let Ok(mut tiles) = write.open_table(TILES) else {
                return false;
            };
            let Ok(mut access) = write.open_table(ACCESS) else {
                return false;
            };
            let now = now_unix();
            for key in keys {
                if tiles
                    .insert(key.as_slice(), [TAG_ABSENT].as_slice())
                    .is_err()
                {
                    return false;
                }
                let _ = access.insert(key.as_slice(), now);
            }
        }
        if write.commit().is_err() {
            return false;
        }
        for key in keys {
            self.remove_legacy_files(key);
        }
        true
    }

    fn remove_legacy_files(&self, key: &[u8; KEY_LEN]) {
        let dat = self.legacy_path(key);
        let _ = std::fs::remove_file(&dat);
        let _ = std::fs::remove_file(dat.with_extension("404"));
    }

    /// `(payload bytes, payload entries)` from the B-tree, without touching the
    /// filesystem.
    pub(crate) fn stats(&self) -> (u64, usize) {
        let Ok(read) = self.db.begin_read() else {
            return (0, 0);
        };
        let Ok(table) = read.open_table(TILES) else {
            return (0, 0);
        };
        let mut bytes = 0u64;
        let mut count = 0usize;
        let Ok(range) = table.iter() else {
            return (0, 0);
        };
        for item in range.into_iter().flatten() {
            let (_, value) = item;
            let value = value.value();
            if value.first() == Some(&TAG_DATA) {
                bytes += (value.len() as u64).saturating_sub(1);
                count += 1;
            }
        }
        (bytes, count)
    }

    /// Drops entries older than `max_age_days` then the oldest entries until
    /// `max_bytes` is satisfied. Runs as B-tree range operations, not a
    /// directory walk.
    pub(crate) fn prune(&self, max_age_days: i32, max_bytes: u64) -> usize {
        let Ok(read) = self.db.begin_read() else {
            return 0;
        };
        let Ok(tiles) = read.open_table(TILES) else {
            return 0;
        };
        let access = read.open_table(ACCESS).ok();

        let mut entries: Vec<(Vec<u8>, u64, u64)> = Vec::new();
        let Ok(range) = tiles.iter() else {
            return 0;
        };
        for item in range.into_iter().flatten() {
            let (key, value) = item;
            let key = key.value().to_vec();
            let size = value.value().len() as u64;
            let ts = access
                .as_ref()
                .and_then(|a| a.get(key.as_slice()).ok().flatten())
                .map(|g| g.value())
                .unwrap_or(0);
            entries.push((key, size, ts));
        }
        drop(access);
        drop(tiles);
        drop(read);

        let mut doomed: Vec<Vec<u8>> = Vec::new();
        if max_age_days > 0 {
            let cutoff = now_unix().saturating_sub(max_age_days as u64 * 24 * 3600);
            for (key, _, ts) in &entries {
                if *ts < cutoff {
                    doomed.push(key.clone());
                }
            }
            let doomed_set: std::collections::HashSet<&[u8]> =
                doomed.iter().map(|k| k.as_slice()).collect();
            entries.retain(|(key, _, _)| !doomed_set.contains(key.as_slice()));
        }

        if max_bytes > 0 {
            entries.sort_by_key(|(_, _, ts)| *ts);
            let mut total: u64 = entries.iter().map(|(_, size, _)| *size).sum();
            for (key, size, _) in &entries {
                if total <= max_bytes {
                    break;
                }
                doomed.push(key.clone());
                total = total.saturating_sub(*size);
            }
        }

        if doomed.is_empty() {
            return 0;
        }
        let Ok(write) = self.db.begin_write() else {
            return 0;
        };
        {
            let Ok(mut tiles) = write.open_table(TILES) else {
                return 0;
            };
            let Ok(mut access) = write.open_table(ACCESS) else {
                return 0;
            };
            for key in &doomed {
                let _ = tiles.remove(key.as_slice());
                let _ = access.remove(key.as_slice());
            }
        }
        if write.commit().is_err() {
            return 0;
        }
        doomed.len()
    }

    pub(crate) fn clear(&self) -> usize {
        let (_, count) = self.stats();
        let Ok(write) = self.db.begin_write() else {
            return 0;
        };
        let _ = write.delete_table(TILES);
        let _ = write.delete_table(ACCESS);
        let _ = write.delete_table(META);
        if write.commit().is_err() {
            return 0;
        }
        // Everything the legacy cache held has either been migrated (and is now
        // gone with the tables) or is still on disk; the caller clears files.
        self.legacy_done.store(true, Ordering::SeqCst);
        count
    }

    fn migrate_legacy(&self) {
        let Ok(read) = std::fs::read_dir(&self.dir) else {
            self.mark_migrated();
            self.legacy_done.store(true, Ordering::SeqCst);
            return;
        };
        let mut batch: Vec<(CacheKey, Vec<u8>, PathBuf, u64)> = Vec::new();
        for entry in read.flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str());
            if !matches!(ext, Some("dat") | Some("404")) {
                continue;
            }
            let Some(key) = key_from_file_name(&path) else {
                continue;
            };
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if ext == Some("404") {
                batch.push((key, vec![TAG_ABSENT], path, mtime));
            } else if let Ok(data) = std::fs::read(&path) {
                if data.is_empty() {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                let mut value = Vec::with_capacity(data.len() + 1);
                value.push(TAG_DATA);
                value.extend_from_slice(&data);
                batch.push((key, value, path, mtime));
            }
            if batch.len() >= 512 && !self.migrate_batch(&mut batch) {
                // Database became unwritable: keep the loose files and stay on
                // the legacy read path instead of losing cached tiles.
                return;
            }
        }
        if !self.migrate_batch(&mut batch) {
            return;
        }
        self.mark_migrated();
        self.legacy_done.store(true, Ordering::SeqCst);
    }

    fn migrate_batch(&self, batch: &mut Vec<(CacheKey, Vec<u8>, PathBuf, u64)>) -> bool {
        if batch.is_empty() {
            return true;
        }
        let Ok(write) = self.db.begin_write() else {
            return false;
        };
        {
            let Ok(mut tiles) = write.open_table(TILES) else {
                return false;
            };
            let Ok(mut access) = write.open_table(ACCESS) else {
                return false;
            };
            for (key, value, _, mtime) in batch.iter() {
                // Never overwrite a fresher row written by a live render.
                if tiles.get(key.as_slice()).ok().flatten().is_some() {
                    continue;
                }
                if tiles.insert(key.as_slice(), value.as_slice()).is_err() {
                    return false;
                }
                let _ = access.insert(key.as_slice(), *mtime);
            }
        }
        if write.commit().is_err() {
            return false;
        }
        for (_, _, path, _) in batch.iter() {
            let _ = std::fs::remove_file(path);
        }
        batch.clear();
        true
    }
}

/// The key type is a fixed-size array so it can be copied around freely.
type CacheKey = [u8; KEY_LEN];

fn tagged(tag: u8, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 1);
    out.push(tag);
    out.extend_from_slice(data);
    out
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn hex(key: &[u8; KEY_LEN]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(KEY_LEN * 2);
    for byte in key {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn key_from_file_name(path: &Path) -> Option<[u8; KEY_LEN]> {
    let stem = path.file_stem()?.to_str()?;
    if stem.len() != KEY_LEN * 2 || !stem.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut key = [0u8; KEY_LEN];
    for (i, chunk) in stem.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let hi = (chunk[0] as char).to_digit(16)? as u8;
        let lo = (chunk[1] as char).to_digit(16)? as u8;
        key[i] = (hi << 4) | lo;
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreadable_database_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.redb");
        std::fs::write(&path, b"not a redb database").unwrap();

        let db = open_database(&path).expect("corrupt cache should be recreated");
        drop(db);

        assert!(std::fs::metadata(&path).unwrap().len() > 16);
    }
}
