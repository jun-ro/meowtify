use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::SystemTime;

const MAGIC: &[u8; 4] = b"MWC1";
const HEADER: usize = 4 + 8 + 8; // magic, payload length, checksum
const EXT: &str = "img";
const MAX_KEY_LEN: usize = 100;

fn checksum(data: &[u8]) -> u64 {
    // FNV-1a 64: detects truncation and bit rot, not an adversary.
    let mut h = 0xcbf29ce484222325u64;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

struct Meta {
    size: u64,
    tick: u64,
    /// Changes every time the file of this key is replaced. A reader that found a bad file
    /// removes it only if this is still the version it read.
    version: u64,
}

struct Index {
    files: HashMap<String, Meta>, // file stem -> meta
    bytes: u64,
    tick: u64,
    next_version: u64,
}

/// Keyed byte store in a folder with a total on-disk byte limit and LRU eviction.
/// Files carry a header (length + checksum); bad files are a miss and are deleted.
/// Writes go to a temp file and are renamed into place.
pub struct DiskCache {
    dir: PathBuf,
    limit: u64,
    index: Mutex<Index>,
}

fn stem_for(key: &str) -> io::Result<String> {
    if key.is_empty() || key.len() > MAX_KEY_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "cache key empty or too long"));
    }
    Ok(key.bytes().map(|b| format!("{b:02x}")).collect())
}

impl DiskCache {
    /// Open (creating) `dir`, index existing files, drop stale temp files, and
    /// evict down to `max_bytes` if needed.
    pub fn open(dir: impl Into<PathBuf>, max_bytes: u64) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let mut found: Vec<(String, u64, SystemTime)> = Vec::new();
        for ent in fs::read_dir(&dir)? {
            let ent = ent?;
            let path = ent.path();
            let Ok(md) = ent.metadata() else { continue };
            if !md.is_file() {
                continue;
            }
            match path.extension().and_then(|e| e.to_str()) {
                Some("tmp") => {
                    let _ = fs::remove_file(&path);
                }
                Some(EXT) => {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        let m = md.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                        found.push((stem.to_owned(), md.len(), m));
                    }
                }
                _ => {}
            }
        }
        found.sort_by_key(|f| f.2);
        let mut idx = Index { files: HashMap::new(), bytes: 0, tick: 0, next_version: 0 };
        for (stem, size, _) in found {
            idx.tick += 1;
            idx.bytes += size;
            idx.next_version += 1;
            let version = idx.next_version;
            idx.files.insert(stem, Meta { size, tick: idx.tick, version });
        }
        let cache = Self { dir, limit: max_bytes, index: Mutex::new(idx) };
        {
            let mut g = cache.lock();
            cache.evict(&mut g);
        }
        Ok(cache)
    }

    fn lock(&self) -> MutexGuard<'_, Index> {
        self.index.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn path(&self, stem: &str) -> PathBuf {
        self.dir.join(format!("{stem}.{EXT}"))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Total bytes of stored files (including headers).
    pub fn used_bytes(&self) -> u64 {
        self.lock().bytes
    }

    pub fn len(&self) -> usize {
        self.lock().files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Removes the file of `stem`. With `only_version`, only if the file is still the one
    /// that was read: a newer file written meanwhile stays. The file is removed while the
    /// index is locked, so no writer can slip in between.
    fn forget(&self, stem: &str, only_version: Option<u64>) {
        let mut g = self.lock();
        if only_version.is_some_and(|v| g.files.get(stem).is_none_or(|m| m.version != v)) {
            return;
        }
        if let Some(m) = g.files.remove(stem) {
            g.bytes -= m.size;
        }
        let _ = fs::remove_file(self.path(stem));
    }

    /// Read the bytes for `key`. Missing, partial or corrupt files return `None`
    /// (corrupt ones are removed).
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        let stem = stem_for(key).ok()?;
        let version = self.lock().files.get(&stem)?.version;
        let data = match fs::read(self.path(&stem)) {
            Ok(d) => d,
            Err(_) => {
                self.forget(&stem, Some(version));
                return None;
            }
        };
        match Self::decode(data) {
            Some(payload) => {
                let mut g = self.lock();
                g.tick += 1;
                let t = g.tick;
                if let Some(m) = g.files.get_mut(&stem) {
                    m.tick = t;
                }
                Some(payload)
            }
            None => {
                self.forget(&stem, Some(version));
                None
            }
        }
    }

    fn decode(mut data: Vec<u8>) -> Option<Vec<u8>> {
        if data.len() < HEADER || &data[..4] != MAGIC {
            return None;
        }
        let len = u64::from_le_bytes(data[4..12].try_into().ok()?);
        let sum = u64::from_le_bytes(data[12..20].try_into().ok()?);
        if data.len() as u64 != HEADER as u64 + len {
            return None;
        }
        data.drain(..HEADER);
        (checksum(&data) == sum).then_some(data)
    }

    /// Atomically store `bytes` under `key`, then evict LRU files to respect the limit.
    /// Payloads that cannot fit in the limit are not stored (returns `Ok(false)`).
    pub fn put(&self, key: &str, bytes: &[u8]) -> io::Result<bool> {
        let stem = stem_for(key)?;
        let total = (HEADER + bytes.len()) as u64;
        if total > self.limit {
            return Ok(false);
        }
        let final_path = self.path(&stem);
        // Unique temp name per writer so concurrent puts never share a file.
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let tmp = self.dir.join(format!("{stem}.{}.{n}.tmp", std::process::id()));
        let res = (|| {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(MAGIC)?;
            f.write_all(&(bytes.len() as u64).to_le_bytes())?;
            f.write_all(&checksum(bytes).to_le_bytes())?;
            f.write_all(bytes)?;
            f.sync_all()
        })();
        if let Err(e) = res {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        // Moving the file in and updating the index happen under one lock, so the index
        // always describes the file that is on disk.
        let mut g = self.lock();
        if let Err(e) = fs::rename(&tmp, &final_path) {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        if let Some(old) = g.files.remove(&stem) {
            g.bytes -= old.size;
        }
        g.tick += 1;
        g.next_version += 1;
        let (t, version) = (g.tick, g.next_version);
        g.bytes += total;
        g.files.insert(stem, Meta { size: total, tick: t, version });
        self.evict(&mut g);
        Ok(true)
    }

    pub fn remove(&self, key: &str) {
        if let Ok(stem) = stem_for(key) {
            self.forget(&stem, None);
        }
    }

    fn evict(&self, g: &mut Index) {
        while g.bytes > self.limit {
            let Some(victim) = g.files.iter().min_by_key(|(_, m)| m.tick).map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(m) = g.files.remove(&victim) {
                g.bytes -= m.size;
            }
            let _ = fs::remove_file(self.path(&victim));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_bytes(p: &Path) -> u64 {
        fs::read_dir(p).unwrap().map(|e| e.unwrap().metadata().unwrap().len()).sum()
    }

    #[test]
    fn roundtrip_and_lru_limit() {
        let td = tempfile::tempdir().unwrap();
        let limit = 3 * (HEADER as u64 + 100);
        let c = DiskCache::open(td.path(), limit).unwrap();
        for k in ["a", "b", "c"] {
            assert!(c.put(k, &[7u8; 100]).unwrap());
        }
        assert_eq!(c.get("a").unwrap(), vec![7u8; 100]); // a newest now
        c.put("d", &[1u8; 100]).unwrap(); // evicts b
        assert!(c.get("b").is_none());
        assert!(c.get("a").is_some() && c.get("c").is_some() && c.get("d").is_some());
        for i in 0..20 {
            c.put(&format!("k{i}"), &[0u8; 100]).unwrap();
            assert!(c.used_bytes() <= limit);
            assert!(dir_bytes(td.path()) <= limit);
        }
        assert_eq!(c.used_bytes(), dir_bytes(td.path()));
        assert!(!c.put("big", &[0u8; 1000]).unwrap());
    }

    #[test]
    fn corrupt_and_partial_files_are_misses_and_removed() {
        let td = tempfile::tempdir().unwrap();
        let c = DiskCache::open(td.path(), 10_000).unwrap();
        c.put("bad", b"hello world").unwrap();
        c.put("short", b"hello world").unwrap();
        let p_bad = c.path(&stem_for("bad").unwrap());
        let p_short = c.path(&stem_for("short").unwrap());
        let mut raw = fs::read(&p_bad).unwrap();
        *raw.last_mut().unwrap() ^= 0xff; // flipped payload byte
        fs::write(&p_bad, raw).unwrap();
        let raw = fs::read(&p_short).unwrap();
        fs::write(&p_short, &raw[..raw.len() - 3]).unwrap(); // truncated
        assert!(c.get("bad").is_none());
        assert!(c.get("short").is_none());
        assert!(!p_bad.exists() && !p_short.exists());
        assert_eq!(c.used_bytes(), 0);
        // after reopen, a garbage file is also a miss
        c.put("g", b"data").unwrap();
        fs::write(c.path(&stem_for("g").unwrap()), b"garbage").unwrap();
        let c2 = DiskCache::open(td.path(), 10_000).unwrap();
        assert!(c2.get("g").is_none());
        assert!(c2.is_empty());
    }

    #[test]
    fn atomic_write_leaves_no_temp_and_survives_overwrite() {
        let td = tempfile::tempdir().unwrap();
        let c = DiskCache::open(td.path(), 10_000).unwrap();
        c.put("k", b"one").unwrap();
        c.put("k", b"two-longer").unwrap();
        assert_eq!(c.get("k").unwrap(), b"two-longer");
        let tmps = fs::read_dir(td.path())
            .unwrap()
            .filter(|e| e.as_ref().unwrap().path().extension().is_some_and(|x| x == "tmp"))
            .count();
        assert_eq!(tmps, 0);
        // a stale temp from a crashed write is cleaned on open and never visible
        fs::write(td.path().join("dead.1.1.tmp"), b"partial").unwrap();
        let c2 = DiskCache::open(td.path(), 10_000).unwrap();
        assert!(!td.path().join("dead.1.1.tmp").exists());
        assert_eq!(c2.get("k").unwrap(), b"two-longer");
        assert!(c2.put("", b"x").is_err());
    }
}
