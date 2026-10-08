//! The MD5 cache `hashes.json`: wire path -> `{size, mtime_ns, ctime_ns, md5}`. A file is hashed
//! again when its size, mtime or ctime differs from the cached row.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::time::UNIX_EPOCH;

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};

use super::path::WirePath;
use crate::{Result, io_err};

pub(crate) const HASHES_FILE: &str = "hashes.json";

/// One cache row (field order is the file order).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HashRow {
    pub size: u64,
    pub mtime_ns: u64,
    /// Missing in rows written before the ctime was checked: such a row never matches.
    #[serde(default)]
    pub ctime_ns: Option<u64>,
    pub md5: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct HashCache {
    rows: BTreeMap<String, HashRow>,
    dirty: bool,
}

/// Nanoseconds since the epoch (0 before it).
pub(crate) fn mtime_ns(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// Nanoseconds since the epoch of the last inode change (0 before it, and 0 off unix). Unlike
/// the mtime, a tool can't set it back.
#[cfg(unix)]
pub(crate) fn ctime_ns(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let (secs, nanos) = (meta.ctime(), meta.ctime_nsec());
    u64::try_from(secs)
        .ok()
        .zip(u64::try_from(nanos).ok())
        .and_then(|(s, n)| s.checked_mul(1_000_000_000)?.checked_add(n))
        .unwrap_or(0)
}

#[cfg(not(unix))]
pub(crate) fn ctime_ns(_meta: &std::fs::Metadata) -> u64 {
    0
}

/// Milliseconds since the epoch (0 before it), the wire's `lastUpdateTime`.
pub(crate) fn mtime_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .unwrap_or(UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Streamed MD5 of a file, lowercase hex.
pub(crate) fn md5_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(const_hex::encode(hasher.finalize()))
}

impl HashCache {
    pub fn load(dir: &Path) -> Result<HashCache> {
        let path = dir.join(HASHES_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(rows) => Ok(HashCache { rows, dirty: false }),
                Err(e) => {
                    // Only a cache: start empty (every file is hashed again) and rewrite it at
                    // the next flush, instead of making the account unusable.
                    eprintln!(
                        "notedown cloud: {} is corrupt ({e}); starting with an empty hash cache",
                        path.display()
                    );
                    Ok(HashCache {
                        rows: Default::default(),
                        dirty: true,
                    })
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashCache::default()),
            Err(e) => Err(io_err(&path)(e)),
        }
    }

    /// The last cached MD5 of `path`, without touching the disk (it may be stale).
    pub fn cached(&self, path: &WirePath) -> Option<&str> {
        self.rows.get(&path.display()).map(|r| r.md5.as_str())
    }

    /// The MD5 of the regular file `fs_path` (wire path `key`), from the cache when size, mtime
    /// and ctime match `meta`, else hashed and cached. A file that changes while it is hashed is
    /// not cached.
    pub fn md5(
        &mut self,
        key: &WirePath,
        fs_path: &Path,
        meta: &std::fs::Metadata,
    ) -> std::io::Result<String> {
        let key = key.display();
        let (size, mtime, ctime) = (meta.len(), mtime_ns(meta), ctime_ns(meta));
        if let Some(row) = self.rows.get(&key)
            && row.size == size
            && row.mtime_ns == mtime
            && row.ctime_ns == Some(ctime)
        {
            return Ok(row.md5.clone());
        }
        let md5 = md5_file(fs_path)?;
        let after = std::fs::metadata(fs_path)?;
        if after.len() == size && mtime_ns(&after) == mtime && ctime_ns(&after) == ctime {
            self.rows.insert(
                key,
                HashRow {
                    size,
                    mtime_ns: mtime,
                    ctime_ns: Some(ctime),
                    md5: md5.clone(),
                },
            );
            self.dirty = true;
        } else if self.rows.remove(&key).is_some() {
            self.dirty = true;
        }
        Ok(md5)
    }

    /// Rows at `from` and below move to `to` (a rename keeps size and mtime).
    pub fn moved(&mut self, from: &WirePath, to: &WirePath) {
        let keys: Vec<String> = self.keys_under(from);
        for key in keys {
            let row = self.rows.remove(&key).expect("key listed");
            if let Some(new) = WirePath::parse(&key).ok().and_then(|p| p.rebase(from, to)) {
                self.rows.insert(new.display(), row);
            }
            self.dirty = true;
        }
    }

    /// Rows at `path` and below are dropped.
    pub fn removed(&mut self, path: &WirePath) {
        for key in self.keys_under(path) {
            self.rows.remove(&key);
            self.dirty = true;
        }
    }

    /// Keep only the rows of `seen` (after a full listing).
    pub fn retain(&mut self, seen: &std::collections::HashSet<String>) {
        let before = self.rows.len();
        self.rows.retain(|k, _| seen.contains(k));
        if self.rows.len() != before {
            self.dirty = true;
        }
    }

    fn keys_under(&self, path: &WirePath) -> Vec<String> {
        self.rows
            .keys()
            .filter(|k| WirePath::parse(k).is_ok_and(|p| p.starts_with(path)))
            .cloned()
            .collect()
    }

    /// Write `hashes.json` if anything changed.
    pub fn flush(&mut self, dir: &Path) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        super::write_atomic_short(
            &dir.join(HASHES_FILE),
            &crate::fsutil::json_bytes(&self.rows),
        )?;
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn wp(s: &str) -> WirePath {
        WirePath::parse(s).unwrap()
    }

    fn md5_of(cache: &mut HashCache, key: &WirePath, fs: &Path) -> String {
        let meta = std::fs::metadata(fs).unwrap();
        cache.md5(key, fs, &meta).unwrap()
    }

    #[test]
    fn row_is_reused_when_size_mtime_and_ctime_match() {
        let dir = tempfile::tempdir().unwrap();
        let fs = dir.path().join("a.txt");
        std::fs::write(&fs, b"one").unwrap();
        let key = wp("a.txt");
        let mut cache = HashCache::default();
        let first = md5_of(&mut cache, &key, &fs);
        // Plant a wrong MD5 in the row: a reuse returns it, a re-hash would not.
        cache.rows.get_mut("a.txt").unwrap().md5 = "planted".into();
        assert_eq!(md5_of(&mut cache, &key, &fs), "planted");
        assert_ne!(first, "planted");
    }

    #[test]
    fn same_size_rewrite_with_old_mtime_is_hashed_again() {
        let dir = tempfile::tempdir().unwrap();
        let fs = dir.path().join("a.txt");
        std::fs::write(&fs, b"one").unwrap();
        let old = std::fs::metadata(&fs).unwrap().modified().unwrap();
        let key = wp("a.txt");
        let mut cache = HashCache::default();
        let first = md5_of(&mut cache, &key, &fs);
        // The ctime moves with the write even when the mtime is set back (`touch -r`).
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&fs, b"two").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&fs)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let meta = std::fs::metadata(&fs).unwrap();
        assert_eq!(mtime_ns(&meta), cache.rows["a.txt"].mtime_ns);
        let second = md5_of(&mut cache, &key, &fs);
        assert_ne!(first, second);
        assert_eq!(second, md5_file(&fs).unwrap());
    }

    #[test]
    fn row_without_ctime_is_hashed_once_then_reused() {
        let dir = tempfile::tempdir().unwrap();
        let fs = dir.path().join("a.txt");
        std::fs::write(&fs, b"one").unwrap();
        let meta = std::fs::metadata(&fs).unwrap();
        let old = format!(
            r#"{{"a.txt":{{"size":{},"mtime_ns":{},"md5":"planted"}}}}"#,
            meta.len(),
            mtime_ns(&meta)
        );
        std::fs::write(dir.path().join(HASHES_FILE), old).unwrap();
        let mut cache = HashCache::load(dir.path()).unwrap();
        let key = wp("a.txt");
        let fresh = md5_of(&mut cache, &key, &fs);
        assert_eq!(fresh, md5_file(&fs).unwrap());
        assert!(cache.dirty);
        cache.rows.get_mut("a.txt").unwrap().md5 = "planted".into();
        assert_eq!(md5_of(&mut cache, &key, &fs), "planted");
    }

    #[test]
    fn cached_returns_the_stored_md5() {
        let dir = tempfile::tempdir().unwrap();
        let fs = dir.path().join("a.txt");
        std::fs::write(&fs, b"one").unwrap();
        let key = wp("a.txt");
        let mut cache = HashCache::default();
        assert_eq!(cache.cached(&key), None);
        let md5 = md5_of(&mut cache, &key, &fs);
        assert_eq!(cache.cached(&key), Some(md5.as_str()));
        assert_eq!(cache.cached(&wp("other.txt")), None);
    }
}
