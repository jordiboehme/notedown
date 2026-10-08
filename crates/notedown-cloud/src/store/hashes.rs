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

    /// The last cached size and MD5 of `path`, without touching the disk (they may be stale).
    pub fn cached_with_size(&self, path: &WirePath) -> Option<(u64, &str)> {
        self.rows
            .get(&path.display())
            .map(|r| (r.size, r.md5.as_str()))
    }

    /// The MD5 of the regular file `fs_path` (wire path `key`), from the cache when size, mtime
    /// and ctime match `meta`, else hashed and cached. A file that changes while it is hashed
    /// gives the MD5 of what was read, which may be no version of the file at all: see
    /// [`HashCache::md5_checked`].
    pub fn md5(
        &mut self,
        key: &WirePath,
        fs_path: &Path,
        meta: &std::fs::Metadata,
    ) -> std::io::Result<String> {
        self.hash(key, fs_path, meta).map(|(md5, _)| md5)
    }

    /// [`HashCache::md5`], but `None` when the file changed while it was hashed (its size, mtime
    /// or ctime after the read differs from `meta`). Such a file is not cached, and its old row
    /// stays as it was: it no longer matches the file, and it still holds the MD5 before the
    /// change for the next try.
    pub fn md5_checked(
        &mut self,
        key: &WirePath,
        fs_path: &Path,
        meta: &std::fs::Metadata,
    ) -> std::io::Result<Option<String>> {
        let (md5, stable) = self.hash(key, fs_path, meta)?;
        Ok(stable.then_some(md5))
    }

    /// The MD5 and whether the file stayed the same while it was hashed.
    fn hash(
        &mut self,
        key: &WirePath,
        fs_path: &Path,
        meta: &std::fs::Metadata,
    ) -> std::io::Result<(String, bool)> {
        let key = key.display();
        let (size, mtime, ctime) = (meta.len(), mtime_ns(meta), ctime_ns(meta));
        if let Some(row) = self.rows.get(&key)
            && row.size == size
            && row.mtime_ns == mtime
            && row.ctime_ns == Some(ctime)
        {
            return Ok((row.md5.clone(), true));
        }
        let md5 = md5_file(fs_path)?;
        let after = std::fs::metadata(fs_path)?;
        let stable = after.len() == size && mtime_ns(&after) == mtime && ctime_ns(&after) == ctime;
        if stable {
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
        }
        Ok((md5, stable))
    }

    /// Whether the row of `key` matches `meta`, so [`HashCache::md5`] would not read the file.
    pub fn is_current(&self, key: &WirePath, meta: &std::fs::Metadata) -> bool {
        self.rows.get(&key.display()).is_some_and(|row| {
            row.size == meta.len()
                && row.mtime_ns == mtime_ns(meta)
                && row.ctime_ns == Some(ctime_ns(meta))
        })
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

    /// The row of the file `path` is dropped (rows below it are not looked at).
    pub fn removed_file(&mut self, path: &WirePath) {
        if self.rows.remove(&path.display()).is_some() {
            self.dirty = true;
        }
    }

    /// Rows at each of `folders` and below are dropped, in one pass over the cache.
    pub fn removed_all(&mut self, folders: &[WirePath]) {
        if folders.is_empty() {
            return;
        }
        let tops: std::collections::HashSet<String> =
            folders.iter().map(WirePath::display).collect();
        let before = self.rows.len();
        // A key lies below a folder when the folder is the key itself or one of its prefixes
        // that ends before a `/`.
        self.rows.retain(|key, _| {
            !(tops.contains(key)
                || key
                    .match_indices('/')
                    .any(|(i, _)| tops.contains(&key[..i])))
        });
        if self.rows.len() != before {
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
    fn a_file_that_changes_while_hashed_is_not_cached_and_keeps_its_old_row() {
        let dir = tempfile::tempdir().unwrap();
        let fs = dir.path().join("a.txt");
        std::fs::write(&fs, b"one").unwrap();
        let key = wp("a.txt");
        let mut cache = HashCache::default();
        let first = md5_of(&mut cache, &key, &fs);
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&fs, b"two").unwrap();
        // The stat the walk took, then a write before the hash ends.
        let meta = std::fs::metadata(&fs).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&fs, b"three").unwrap();
        assert_eq!(cache.md5_checked(&key, &fs, &meta).unwrap(), None);
        assert_eq!(cache.cached(&key), Some(first.as_str()));
        assert!(!cache.is_current(&key, &std::fs::metadata(&fs).unwrap()));
    }

    #[test]
    fn removed_all_drops_folders_by_whole_segments() {
        let mut cache = HashCache::default();
        for k in ["a/b/x", "a/bc/y", "a/b", "c/d", "e"] {
            cache.rows.insert(
                k.into(),
                HashRow {
                    size: 1,
                    mtime_ns: 1,
                    ctime_ns: Some(1),
                    md5: "m".into(),
                },
            );
        }
        cache.removed_all(&[wp("a/b"), wp("c")]);
        let left: Vec<&str> = cache.rows.keys().map(String::as_str).collect();
        assert_eq!(left, vec!["a/bc/y", "e"]);
        cache.removed_file(&wp("e"));
        assert_eq!(cache.cached(&wp("e")), None);
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
