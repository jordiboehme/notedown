//! The id registry: `ids.json` (compacted state) plus `ids.jsonl` (append log).
//!
//! Ids are `u64`, start at [`FIRST_ID`] and are never reused. A move keeps the id (a folder move
//! moves every descendant), a copy gets a new id. The log lines are `assign`, `move` and
//! `remove`; replaying a line twice gives the same state, so a crash during compaction (after
//! `ids.json` is written, before the log is removed) is harmless.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::Kind;
use super::path::WirePath;
use crate::{Error, Result, io_err};

/// The first id the store hands out.
pub const FIRST_ID: u64 = 1000;
pub(crate) const IDS_FILE: &str = "ids.json";
pub(crate) const LOG_FILE: &str = "ids.jsonl";

/// One registry row, as stored in `ids.json` (field order is the file order).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub kind: Kind,
    pub path: String,
}

/// `ids.json`: the next id, then the rows keyed by id (as strings, in numeric order).
#[derive(Debug, Serialize, Deserialize)]
struct IdsFile {
    next: u64,
    ids: BTreeMap<u64, Record>,
}

/// One line of `ids.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub(crate) enum Op {
    Assign { id: u64, kind: Kind, path: String },
    Move { id: u64, path: String },
    Remove { id: u64 },
}

#[derive(Debug, Clone)]
pub(crate) struct Registry {
    by_id: BTreeMap<u64, (Kind, WirePath)>,
    by_path: HashMap<WirePath, u64>,
    next: u64,
}

impl Default for Registry {
    fn default() -> Self {
        Registry {
            by_id: BTreeMap::new(),
            by_path: HashMap::new(),
            next: FIRST_ID,
        }
    }
}

fn corrupt(path: &Path, message: String) -> Error {
    Error::Config {
        path: path.to_path_buf(),
        message,
    }
}

fn parse_path(file: &Path, text: &str) -> Result<WirePath> {
    WirePath::parse_named(text).map_err(|e| corrupt(file, format!("bad path {text:?}: {e}")))
}

impl Registry {
    pub fn id_of(&self, path: &WirePath) -> Option<u64> {
        self.by_path.get(path).copied()
    }

    pub fn get(&self, id: u64) -> Option<&(Kind, WirePath)> {
        self.by_id.get(&id)
    }

    pub fn next(&self) -> u64 {
        self.next
    }

    fn insert(&mut self, id: u64, kind: Kind, path: WirePath) {
        if let Some(old) = self.by_path.insert(path.clone(), id)
            && old != id
        {
            self.by_id.remove(&old);
        }
        if let Some((_, old_path)) = self.by_id.insert(id, (kind, path.clone()))
            && old_path != path
            && self.by_path.get(&old_path) == Some(&id)
        {
            self.by_path.remove(&old_path);
        }
        self.next = self.next.max(id + 1);
    }

    fn drop_id(&mut self, id: u64) {
        if let Some((_, path)) = self.by_id.remove(&id)
            && self.by_path.get(&path) == Some(&id)
        {
            self.by_path.remove(&path);
        }
    }

    /// The ids at `path` and below it (segment-aware), `path` itself first.
    fn subtree(&self, path: &WirePath) -> Vec<u64> {
        let mut ids: Vec<u64> = self
            .by_id
            .iter()
            .filter(|(_, (_, p))| p.starts_with(path))
            .map(|(id, _)| *id)
            .collect();
        ids.sort_by_key(|id| self.by_id[id].1.segments().len());
        ids
    }

    pub fn apply(&mut self, op: &Op) {
        match op {
            Op::Assign { id, kind, path } => match WirePath::parse_named(path) {
                Ok(path) => self.insert(*id, *kind, path),
                Err(_) => self.next = self.next.max(id + 1),
            },
            Op::Move { id, path } => {
                let Ok(to) = WirePath::parse_named(path) else {
                    return;
                };
                let Some((kind, from)) = self.by_id.get(id).cloned() else {
                    return;
                };
                if from == to {
                    return;
                }
                if kind == Kind::Folder {
                    for moved in self.subtree(&from) {
                        let (k, p) = self.by_id[&moved].clone();
                        if let Some(new) = p.rebase(&from, &to) {
                            self.drop_id(moved);
                            self.insert(moved, k, new);
                        }
                    }
                } else {
                    self.drop_id(*id);
                    self.insert(*id, kind, to);
                }
            }
            Op::Remove { id } => {
                let Some((kind, path)) = self.by_id.get(id).cloned() else {
                    return;
                };
                if kind == Kind::Folder {
                    for gone in self.subtree(&path) {
                        self.drop_id(gone);
                    }
                } else {
                    self.drop_id(*id);
                }
            }
        }
    }

    /// Read `ids.json` and replay `ids.jsonl`. An unterminated last line (a torn append) is
    /// ignored; with `repair` (only under the store lock) it is also cut from the file. A corrupt
    /// complete line is an error: guessing could reuse or lose ids.
    pub fn load(dir: &Path, repair: bool) -> Result<Registry> {
        let mut reg = Registry::default();
        let ids_path = dir.join(IDS_FILE);
        match std::fs::read(&ids_path) {
            Ok(bytes) => {
                let file: IdsFile = serde_json::from_slice(&bytes)
                    .map_err(|e| corrupt(&ids_path, e.to_string()))?;
                for (id, rec) in file.ids {
                    let path = parse_path(&ids_path, &rec.path)?;
                    reg.insert(id, rec.kind, path);
                }
                reg.next = reg.next.max(file.next);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&ids_path)(e)),
        }
        let log_path = dir.join(LOG_FILE);
        let bytes = match std::fs::read(&log_path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(reg),
            Err(e) => return Err(io_err(&log_path)(e)),
        };
        let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        for (n, line) in bytes[..complete].split(|b| *b == b'\n').enumerate() {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let op: Op = serde_json::from_slice(line)
                .map_err(|e| corrupt(&log_path, format!("line {}: {e}", n + 1)))?;
            reg.apply(&op);
        }
        if repair && complete < bytes.len() {
            let f = std::fs::OpenOptions::new()
                .write(true)
                .open(&log_path)
                .map_err(io_err(&log_path))?;
            f.set_len(complete as u64)
                .and_then(|()| f.sync_all())
                .map_err(io_err(&log_path))?;
        }
        Ok(reg)
    }

    /// Append `ops` to `ids.jsonl` (one write, one fsync), then apply them.
    pub fn append(&mut self, dir: &Path, ops: &[Op]) -> Result<()> {
        if ops.is_empty() {
            return Ok(());
        }
        let log_path = dir.join(LOG_FILE);
        let mut text = Vec::new();
        for op in ops {
            serde_json::to_writer(&mut text, op).expect("plain enums serialize");
            text.push(b'\n');
        }
        let existed = log_path.exists();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&log_path)
            .map_err(io_err(&log_path))?;
        f.write_all(&text)
            .and_then(|()| f.sync_all())
            .map_err(io_err(&log_path))?;
        if !existed {
            crate::fsutil::fsync_dir(dir)?;
        }
        for op in ops {
            self.apply(op);
        }
        Ok(())
    }

    /// Write the state to `ids.json` and remove the log (in that order).
    pub fn compact(&self, dir: &Path) -> Result<()> {
        let file = IdsFile {
            next: self.next,
            ids: self
                .by_id
                .iter()
                .map(|(id, (kind, path))| {
                    (
                        *id,
                        Record {
                            kind: *kind,
                            path: path.display(),
                        },
                    )
                })
                .collect(),
        };
        super::write_atomic_short(&dir.join(IDS_FILE), &crate::fsutil::json_bytes(&file))?;
        let log_path = dir.join(LOG_FILE);
        match std::fs::remove_file(&log_path) {
            Ok(()) => crate::fsutil::fsync_dir(dir),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err(&log_path)(e)),
        }
    }

    /// True if the log holds anything to compact.
    pub fn log_is_empty(dir: &Path) -> bool {
        std::fs::metadata(dir.join(LOG_FILE)).map_or(true, |m| m.len() == 0)
    }
}
