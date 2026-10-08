//! The id registry: `ids.json` (compacted state) plus `ids.jsonl` (append log).
//!
//! Ids are `u64`, start at [`FIRST_ID`] and are never reused. A move keeps the id (a folder move
//! moves every descendant), a copy gets a new id. The log lines are `assign`, `move`, `remove`
//! and `reserve` (raises the next id without a row, so ids a journal append used before their
//! rows were written are never handed out again); replaying a line twice gives the same state, so a crash during compaction (after
//! `ids.json` is written, before the log is removed) is harmless.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::ops::Bound;
use std::path::Path;
use std::sync::OnceLock;

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
    Assign {
        id: u64,
        kind: Kind,
        path: String,
    },
    Move {
        id: u64,
        path: String,
    },
    Remove {
        id: u64,
    },
    /// The next id is at least `next`; no row changes.
    Reserve {
        next: u64,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct Registry {
    by_id: BTreeMap<u64, (Kind, WirePath)>,
    by_path: HashMap<WirePath, u64>,
    /// `by_path` in path order, so a subtree is one range (see [`Registry::scope_rows`]). Built
    /// at the first scope query, then kept up to date; a load or a whole-tree scan never pays
    /// for it. Once built it holds every path key a second time in memory.
    ordered: OnceLock<BTreeMap<WirePath, u64>>,
    /// The number of file rows.
    files: usize,
    next: u64,
}

impl Default for Registry {
    fn default() -> Self {
        Registry {
            by_id: BTreeMap::new(),
            by_path: HashMap::new(),
            ordered: OnceLock::new(),
            files: 0,
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

    /// Every row: id, kind and path, by id.
    pub fn rows(&self) -> impl Iterator<Item = (u64, Kind, &WirePath)> {
        self.by_id
            .iter()
            .map(|(id, (kind, path))| (*id, *kind, path))
    }

    /// The number of file rows, without a walk.
    pub fn files(&self) -> usize {
        self.files
    }

    /// The path index in path order, built on first use.
    fn ordered(&self) -> &BTreeMap<WirePath, u64> {
        self.ordered.get_or_init(|| {
            self.by_path
                .iter()
                .map(|(path, id)| (path.clone(), *id))
                .collect()
        })
    }

    fn index_path(&mut self, path: &WirePath, id: u64) {
        self.by_path.insert(path.clone(), id);
        if let Some(ordered) = self.ordered.get_mut() {
            ordered.insert(path.clone(), id);
        }
    }

    fn unindex_path(&mut self, path: &WirePath) {
        self.by_path.remove(path);
        if let Some(ordered) = self.ordered.get_mut() {
            ordered.remove(path);
        }
    }

    /// The row of `path` from the path index.
    fn row_at<'a>(&'a self, path: &'a WirePath, id: u64) -> Option<(u64, Kind, &'a WirePath)> {
        self.by_id.get(&id).map(|(kind, _)| (id, *kind, path))
    }

    /// The rows at `path` and below it (segment-aware), by path. Reads only that range.
    pub fn subtree_rows<'a>(
        &'a self,
        path: &'a WirePath,
    ) -> impl Iterator<Item = (u64, Kind, &'a WirePath)> + 'a {
        self.ordered()
            .range::<WirePath, _>((Bound::Included(path), Bound::Unbounded))
            .take_while(move |(p, _)| p.starts_with(path))
            .filter_map(|(p, id)| self.row_at(p, *id))
    }

    /// The rows of the direct children of `folder`, by path. The subtree of each child is
    /// skipped, so the cost follows the number of children, not of descendants.
    fn child_rows<'a>(&'a self, folder: &WirePath) -> Vec<(u64, Kind, &'a WirePath)> {
        let depth = folder.segments().len() + 1;
        let mut out = Vec::new();
        let mut from = Bound::Excluded(folder.clone());
        while let Some((p, id)) = self
            .ordered()
            .range::<WirePath, _>((from.as_ref(), Bound::Unbounded))
            .next()
        {
            if !p.starts_with(folder) {
                break;
            }
            if p.segments().len() == depth {
                out.extend(self.row_at(p, *id));
            }
            from = Bound::Included(p.subtree_end(depth));
        }
        out
    }

    /// The rows of a scan scope: the scope itself (never the root) and every row below it, or
    /// only its direct children when not `recursive`. The same rows as a filter of
    /// [`Registry::rows`] by scope, but read from the path index.
    pub fn scope_rows<'a>(
        &'a self,
        scope: &'a WirePath,
        recursive: bool,
    ) -> Vec<(u64, Kind, &'a WirePath)> {
        if recursive {
            return self.subtree_rows(scope).collect();
        }
        let mut out: Vec<(u64, Kind, &WirePath)> = self
            .ordered()
            .get_key_value(scope)
            .and_then(|(p, id)| self.row_at(p, *id))
            .into_iter()
            .collect();
        out.extend(self.child_rows(scope));
        out
    }

    fn count(&mut self, kind: Kind, add: bool) {
        if kind == Kind::File {
            if add {
                self.files += 1;
            } else {
                // A drifted count would give a wrong hold decision: loud in tests, never a
                // panic in a release build.
                debug_assert!(self.files > 0, "file count below zero");
                self.files = self.files.saturating_sub(1);
            }
        }
    }

    fn insert(&mut self, id: u64, kind: Kind, path: WirePath) {
        let old = self.by_path.get(&path).copied();
        self.index_path(&path, id);
        if let Some(old) = old
            && old != id
            && let Some((old_kind, _)) = self.by_id.remove(&old)
        {
            self.count(old_kind, false);
        }
        if let Some((old_kind, old_path)) = self.by_id.insert(id, (kind, path.clone())) {
            self.count(old_kind, false);
            if old_path != path && self.by_path.get(&old_path) == Some(&id) {
                self.unindex_path(&old_path);
            }
        }
        self.count(kind, true);
        self.next = self.next.max(id.saturating_add(1));
    }

    fn drop_id(&mut self, id: u64) {
        if let Some((kind, path)) = self.by_id.remove(&id) {
            self.count(kind, false);
            if self.by_path.get(&path) == Some(&id) {
                self.unindex_path(&path);
            }
        }
    }

    /// The ids at `path` and below it (segment-aware), shallowest first. From the ordered index
    /// once it is built, else from a walk of every row (a load replays its log without it).
    fn subtree(&self, path: &WirePath) -> Vec<u64> {
        let mut ids: Vec<(usize, u64)> = if self.ordered.get().is_some() {
            self.subtree_rows(path)
                .map(|(id, _, p)| (p.segments().len(), id))
                .collect()
        } else {
            self.rows()
                .filter(|(_, _, p)| p.starts_with(path))
                .map(|(id, _, p)| (p.segments().len(), id))
                .collect()
        };
        ids.sort();
        ids.into_iter().map(|(_, id)| id).collect()
    }

    /// Why a move of `id` to `to` is not allowed, if it is not: an unknown id, a folder moved
    /// into its own subtree, or a target that holds another id.
    pub fn check_move(&self, id: u64, to: &WirePath) -> Option<String> {
        let Some((kind, from)) = self.by_id.get(&id) else {
            return Some(format!("id {id} is not known"));
        };
        if *kind == Kind::Folder && to != from && to.starts_with(from) {
            return Some(format!("cannot move folder {from} into itself ({to})"));
        }
        match self.by_path.get(to) {
            Some(other) if *other != id => Some(format!("{to} is taken by id {other}")),
            _ => None,
        }
    }

    pub fn apply(&mut self, op: &Op) {
        match op {
            Op::Assign { id, kind, path } => match WirePath::parse_named(path) {
                Ok(path) => self.insert(*id, *kind, path),
                Err(_) => self.next = self.next.max(id.saturating_add(1)),
            },
            Op::Move { id, path } => {
                // Total: a line that `check_move` would refuse (written by an older build or by
                // hand) is skipped, never a panic, so a bad line cannot brick an open.
                let Ok(to) = WirePath::parse_named(path) else {
                    return;
                };
                let Some((kind, from)) = self.by_id.get(id).cloned() else {
                    return;
                };
                if from == to || (kind == Kind::Folder && to.starts_with(&from)) {
                    return;
                }
                let moves: Vec<(u64, Kind, WirePath)> = if kind == Kind::Folder {
                    self.subtree(&from)
                        .into_iter()
                        .filter_map(|m| {
                            let (k, p) = self.by_id.get(&m)?;
                            Some((m, *k, p.rebase(&from, &to)?))
                        })
                        .collect()
                } else {
                    vec![(*id, kind, to)]
                };
                for (m, _, _) in &moves {
                    self.drop_id(*m);
                }
                for (m, k, p) in moves {
                    self.insert(m, k, p);
                }
            }
            Op::Reserve { next } => self.next = self.next.max(*next),
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A small xorshift, so the op sequences are the same on every run.
    pub(crate) struct Rng(pub u64);

    impl Rng {
        pub fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            &items[(self.next() % items.len() as u64) as usize]
        }
    }

    /// Paths over few names, so ops often hit the same path, a path of the other kind, a name
    /// that is a prefix of another (`a`, `a b`, `ab`) or a folder above a file.
    pub(crate) fn random_path(rng: &mut Rng) -> WirePath {
        const NAMES: [&str; 5] = ["a", "a b", "ab", "b", "a.txt"];
        let depth = 1 + rng.next() % 4;
        let segments: Vec<&str> = (0..depth).map(|_| *rng.pick(&NAMES)).collect();
        WirePath::from_segments(segments).unwrap()
    }

    /// A registry built from a random sequence of every kind of op.
    pub(crate) fn random_registry(seed: u64, ops: usize) -> Registry {
        let mut rng = Rng(seed);
        let mut reg = Registry::default();
        for n in 0..ops {
            if n == ops / 2 {
                // Build the ordered index halfway: the ops after it keep it up to date.
                reg.ordered();
            }
            let known: Vec<u64> = reg.by_id.keys().copied().collect();
            let op = match rng.next() % 10 {
                0..=4 => Op::Assign {
                    id: reg.next() + rng.next() % 2,
                    kind: if rng.next().is_multiple_of(2) {
                        Kind::File
                    } else {
                        Kind::Folder
                    },
                    path: random_path(&mut rng).display(),
                },
                5 if !known.is_empty() => Op::Assign {
                    // An existing id assigned again, maybe with the other kind.
                    id: *rng.pick(&known),
                    kind: if rng.next().is_multiple_of(2) {
                        Kind::File
                    } else {
                        Kind::Folder
                    },
                    path: random_path(&mut rng).display(),
                },
                6 | 7 if !known.is_empty() => Op::Move {
                    id: *rng.pick(&known),
                    path: random_path(&mut rng).display(),
                },
                8 if !known.is_empty() => Op::Remove {
                    id: *rng.pick(&known),
                },
                _ => Op::Reserve {
                    next: reg.next() + rng.next() % 3,
                },
            };
            reg.apply(&op);
        }
        reg
    }

    fn brute_scope(reg: &Registry, scope: &WirePath, recursive: bool) -> Vec<u64> {
        let mut ids: Vec<u64> = reg
            .rows()
            .filter(|(_, _, p)| {
                !p.is_root()
                    && p.starts_with(scope)
                    && (recursive || p.segments().len() <= scope.segments().len() + 1)
            })
            .map(|(id, _, _)| id)
            .collect();
        ids.sort_unstable();
        ids
    }

    #[test]
    fn the_file_count_and_the_scope_queries_match_a_walk_of_every_row() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for seed in 1..=200 {
            let reg = random_registry(seed, 60);
            // The two indexes hold the same rows.
            assert_eq!(reg.by_id.len(), reg.by_path.len(), "seed {seed}");
            let ordered: Vec<(&WirePath, &u64)> = reg.ordered().iter().collect();
            let mut by_path: Vec<(&WirePath, &u64)> = reg.by_path.iter().collect();
            by_path.sort();
            assert_eq!(ordered, by_path, "seed {seed}");
            for (id, _, path) in reg.rows() {
                assert_eq!(reg.id_of(path), Some(id), "seed {seed}");
            }
            let files = reg.rows().filter(|r| r.1 == Kind::File).count();
            assert_eq!(reg.files(), files, "seed {seed}");
            let mut scopes = vec![WirePath::root()];
            scopes.extend((0..8).map(|_| random_path(&mut rng)));
            for scope in &scopes {
                for recursive in [true, false] {
                    let mut got: Vec<u64> = reg
                        .scope_rows(scope, recursive)
                        .iter()
                        .map(|r| r.0)
                        .collect();
                    got.sort_unstable();
                    assert_eq!(
                        got,
                        brute_scope(&reg, scope, recursive),
                        "seed {seed}, scope {scope:?}, recursive {recursive}"
                    );
                }
            }
        }
    }

    #[test]
    fn direct_children_skip_the_subtrees_between_them() {
        let mut reg = Registry::default();
        for (id, kind, path) in [
            (1000, Kind::Folder, "a"),
            (1001, Kind::File, "a/x"),
            (1002, Kind::Folder, "a/b"),
            (1003, Kind::File, "a/b/deep"),
            (1004, Kind::File, "a/b c"),
            (1005, Kind::File, "a/b/c/d"),
            (1006, Kind::File, "ab"),
            (1007, Kind::File, "a b"),
            // A child folder without a row of its own: only its subtree is there.
            (1008, Kind::File, "a/ghost/y"),
        ] {
            reg.apply(&Op::Assign {
                id,
                kind,
                path: path.into(),
            });
        }
        let names = |rows: Vec<(u64, Kind, &WirePath)>| -> Vec<String> {
            rows.iter().map(|r| r.2.display()).collect()
        };
        let a = WirePath::parse("a").unwrap();
        assert_eq!(
            names(reg.scope_rows(&a, false)),
            vec!["a", "a/b", "a/b c", "a/x"]
        );
        assert_eq!(
            names(reg.scope_rows(&WirePath::root(), false)),
            vec!["a", "a b", "ab"]
        );
        assert_eq!(
            names(reg.scope_rows(&a, true)),
            vec![
                "a",
                "a/b",
                "a/b/c/d",
                "a/b/deep",
                "a/b c",
                "a/ghost/y",
                "a/x"
            ]
        );
        assert_eq!(reg.files(), 7);
        reg.apply(&Op::Remove { id: 1002 });
        assert_eq!(reg.files(), 5);
        // A file replaced by a folder at the same path.
        reg.apply(&Op::Assign {
            id: 1010,
            kind: Kind::Folder,
            path: "a/x".into(),
        });
        assert_eq!(reg.files(), 4);
        assert_eq!(reg.id_of(&WirePath::parse("a/x").unwrap()), Some(1010));
    }
}
