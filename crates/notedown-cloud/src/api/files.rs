//! The file sync session, listing, queries, download URLs, capacity, the upload apply and
//! finish steps, and folder creation, delete, move and copy of accept mode
//! (`docs/sync-protocol.md` 3.4, 3.5). Every write needs the sync lock: the device's own
//! session, or a free lock that the write takes (Auto Sync sends a changed file without
//! `synchronous/start`). Another device's live lock answers `E0079` (flag `no_sync_lock`).
//!
//! Every route needs the device token. The body's `equipmentNo`, when present, must be the
//! token's device (`E0312` otherwise). Every call of the device that holds the account's sync
//! lock extends it. Store and journal work runs in blocking tasks under
//! [`Store::lock_recovered`](crate::store::Store::lock_recovered), so nothing is listed or
//! answered from an unrecovered registry.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use serde_json::{Map, Value, json};

use super::envelope::{Reply, field, parse_body};
use crate::Error;
use crate::auth::{Account, DEFAULT_QUOTA_BYTES};
use crate::diag::{LogEntry, LogKind};
use crate::journal::Origin;
use crate::server::AppState;
use crate::store::{
    Entry, Kind, PendingOp, PutOptions, ScanOptions, Store, StoreGuard, Stored, Walk, WirePath,
    WriteGuard,
};
use crate::sync::staging;
use crate::sync::urls::{base_url, download_url, oss_url};
use crate::sync::{Purpose, SignedTarget, StartOutcome, WriteLock};

/// Another device holds the sync lock.
pub const E_SYNC_LOCKED: &str = "E0078";
/// The path (or folder id) does not exist.
pub const E_PATH_MISSING: &str = "E0081";
/// The body names another device than the token.
pub const E_DEVICE_MISMATCH: &str = "E0312";
/// The file to download does not exist.
pub const E_NO_FILE: &str = "E0321";

/// A file or folder with the same name (or one that differs only in case or Unicode
/// normalization) exists.
pub const E_NAME_TAKEN: &str = "E0322";
/// The file cannot be uploaded (hash or size mismatch, missing data, too large, bad name).
pub const E_UPLOAD: &str = "E0324";

pub const MSG_NAME_TAKEN: &str = "A file with the same name already exists";
pub const MSG_UPLOAD: &str = "The file cannot be uploaded";
pub const MSG_SYNC_LOCKED: &str = "A device is currently synchronizing";
pub const MSG_PATH_MISSING: &str = "The path does not exist";
pub const MSG_DEVICE_MISMATCH: &str = "The device is not linked to this account";
pub const MSG_NO_FILE: &str = "The file does not exist";

/// Log flag: the store of an imported account is damaged (`files/` missing, a symlink or no
/// folder, or a native folder missing). Every file route answers `E0079` instead of a listing.
pub const FLAG_STORE_DAMAGED: &str = "store_damaged";
/// Log flag: `synchronous/start` with protect off for an account without an `imported` marker
/// and without files, refused (`E0079`): the device would compare against an empty tree.
pub const FLAG_EMPTY_ACCOUNT: &str = "empty_account";
/// Log flag: `upload/finish` named the upload by its file name (as in Ratta's traffic).
pub const FLAG_INNER_NAME_FILE: &str = "inner_name_file_name";
/// Log flag: `upload/finish` named the upload by the internal staging id.
pub const FLAG_INNER_NAME_ID: &str = "inner_name_id";
/// Log flag: `synchronous/start` refused by the protect gate (no import yet).
pub const FLAG_PROTECT_REFUSED: &str = "protect_refused";
/// Log flag: a scan of the account holds (`scan-hold.json`): too many files are gone from the
/// data folder. Every file route answers `E0079` until the files are back or the scan is
/// accepted.
pub const FLAG_SCAN_HOLD: &str = "scan_hold";
/// Log flag: `synchronous/end` with `flag` `Y`.
pub const FLAG_SYNC_END_OK: &str = "sync_end_ok";
/// Log flag: `synchronous/end` with `flag` `N` (the device saw a failure).
pub const FLAG_SYNC_END_FAILED: &str = "sync_end_failed";
/// Log flag: `synchronous/end` with a missing or unknown `flag`.
pub const FLAG_SYNC_END_UNKNOWN: &str = "sync_end_unknown_flag";
/// Log flag: `synchronous/end` from a device that did not hold the lock.
pub const FLAG_SYNC_LOCK_NOT_HELD: &str = "sync_lock_not_held";

/// Log flag: a write while another device holds the live sync lock (`E0079`).
pub const FLAG_NO_SYNC_LOCK: &str = "no_sync_lock";
/// Marker flag: a device wrote without `synchronous/start` and the server took the free sync
/// lock for it.
pub const FLAG_IMPLICIT_LOCK: &str = "implicit_lock";
/// Log flag: the staged bytes do not have the MD5 the device names.
pub const FLAG_HASH_MISMATCH: &str = "hash_mismatch";
/// Log flag: the staged bytes do not have the size the device names.
pub const FLAG_SIZE_MISMATCH: &str = "size_mismatch";
/// Log flag: `upload/finish` names an upload with no complete data (unknown, or parts missing).
pub const FLAG_UPLOAD_MISSING: &str = "upload_missing";
/// Log flag: a file or part above the size limit.
pub const FLAG_UPLOAD_TOO_LARGE: &str = "upload_too_large";
/// Log flag: `upload/finish` names another parent folder than the one signed at apply.
pub const FLAG_PARENT_MISMATCH: &str = "parent_mismatch";
/// Log flag: a target path or name the store does not take.
pub const FLAG_BAD_NAME: &str = "bad_name";
/// Log flag: a path segment matches no entry exactly and two or more without case (`E0322`).
pub const FLAG_CASE_AMBIGUOUS: &str = "case_ambiguous";
/// Log flag: a name longer than [`MAX_NAME_BYTES`] (`E0324`).
pub const FLAG_NAME_TOO_LONG: &str = "name_too_long";

/// The longest name the store takes, in UTF-8 bytes: most file systems allow 255 bytes.
pub const MAX_NAME_BYTES: usize = 255;
/// The message of `E0324` for a name longer than [`MAX_NAME_BYTES`].
pub const MSG_NAME_TOO_LONG: &str = "The name is too long";

/// A wire path with the stored spelling of each existing segment ([`stored`]).
struct Resolved {
    path: WirePath,
    /// The first `existing` segments name entries on disk.
    existing: usize,
}

impl Resolved {
    /// Whether a name that would be new on disk is longer than [`MAX_NAME_BYTES`]. A name that
    /// exists already passes: the file system took it.
    fn new_name_too_long(&self) -> bool {
        self.path.segments()[self.existing..]
            .iter()
            .any(|s| s.len() > MAX_NAME_BYTES)
    }
}

/// `path` with the stored spelling of each existing segment ([`Store::stored_path`]); `None`
/// when a segment matches two entries without case and none exactly.
fn stored(store: &Store, guard: &StoreGuard, path: &WirePath) -> crate::Result<Option<Resolved>> {
    Ok(match store.stored_path(guard, path)? {
        Stored::Path { path, existing } => Some(Resolved { path, existing }),
        Stored::Ambiguous { folder, names } => {
            eprintln!(
                "notedown cloud: account {}: {path} matches {} in /{folder} without case; \
                 rename one of them",
                store.slug(),
                names.join(" and ")
            );
            None
        }
    })
}

/// A checked device request: the token's account and device and the parsed body.
#[derive(Debug)]
pub struct DeviceRequest {
    pub slug: String,
    pub serial: String,
    /// The token's generation (signed into transfer URLs).
    pub generation: u64,
    pub account: Account,
    pub body: Value,
}

impl DeviceRequest {
    /// Success envelope with `equipmentNo` and these payload fields (`fields` must be an
    /// object), attributed to the device in the log.
    pub fn ok(&self, fields: Value) -> Reply {
        let mut m = Map::new();
        m.insert("equipmentNo".into(), json!(self.serial));
        if let Value::Object(f) = fields {
            m.extend(f);
        }
        Reply::ok(Value::Object(m)).device(&self.serial)
    }

    pub fn error(&self, code: &str, msg: &str) -> Reply {
        Reply::error(code, msg).device(&self.serial)
    }
}

/// Authenticate the token, parse the body, check `equipmentNo` against the token's device
/// (`E0312`; a body without it is accepted) and extend the sync lock when this device holds
/// it. Every file route of accept mode starts here.
pub fn device_request(
    state: &AppState,
    headers: &HeaderMap,
    bytes: &[u8],
) -> Result<DeviceRequest, Reply> {
    let (a, _) = super::server::authenticate(state, headers)?;
    let serial = a.claims.equipment_no;
    let generation = a.claims.generation;
    let body = parse_body(bytes).map_err(|r| r.device(&serial))?;
    if let Some(sent) = field(&body, "equipmentNo")
        && sent != serial
    {
        return Err(Reply::error(E_DEVICE_MISMATCH, MSG_DEVICE_MISMATCH).device(&serial));
    }
    let slug = a.account.slug.clone();
    state.sync_locks.touch(&slug, &serial, state.now_ms());
    Ok(DeviceRequest {
        slug,
        serial,
        generation,
        account: a.account,
        body,
    })
}

/// An id argument as the device sends it: a JSON string (`"4711"`) or a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdArg {
    /// Absent or `null`.
    Missing,
    Id(u64),
    /// Any other type, or text that is no unsigned integer.
    Bad,
}

pub fn id_arg(body: &Value, key: &str) -> IdArg {
    match body.get(key) {
        None | Some(Value::Null) => IdArg::Missing,
        Some(Value::String(s)) => s.trim().parse().map_or(IdArg::Bad, IdArg::Id),
        Some(Value::Number(n)) => n.as_u64().map_or(IdArg::Bad, IdArg::Id),
        Some(_) => IdArg::Bad,
    }
}

/// `recursive` as a boolean, or the strings `true` / `Y`. Default false.
fn flag_arg(body: &Value, key: &str) -> bool {
    match body.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => {
            s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("y") || s == "1"
        }
        Some(Value::Number(n)) => n.as_u64() == Some(1),
        _ => false,
    }
}

/// Run blocking store work; an error becomes an internal-error envelope (and a line on stderr).
pub async fn blocking<T, F>(serial: &str, f: F) -> Result<T, Reply>
where
    T: Send + 'static,
    F: FnOnce() -> crate::Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(Error::GateClosed(_))) => Err(protect_refused(serial)),
        Ok(Err(Error::ScanHold(_))) => Err(scan_held(serial)),
        Ok(Err(e @ Error::StoreDamaged { .. })) => Err(store_damaged(serial, &e)),
        Ok(Err(e)) => {
            eprintln!("notedown cloud: {e}");
            Err(Reply::internal().device(serial))
        }
        Err(e) => {
            eprintln!("notedown cloud: blocking task failed: {e}");
            Err(Reply::internal().device(serial))
        }
    }
}

fn entries_value(entries: &[Entry]) -> Value {
    serde_json::to_value(entries).unwrap_or_else(|_| Value::Array(Vec::new()))
}

fn entry_value(entry: Option<&Entry>) -> Value {
    entry
        .and_then(|e| serde_json::to_value(e).ok())
        .unwrap_or(Value::Null)
}

macro_rules! device_request {
    ($state:expr, $headers:expr, $bytes:expr) => {
        match device_request(&$state, &$headers, &$bytes) {
            Ok(r) => r,
            Err(reply) => return reply,
        }
    };
}

/// The protect gate: in accept mode with `protect = true`, an account without an `imported`
/// marker gets `E0079` (flag `protect_refused`). `synchronous/start` checks it, and so does
/// every file route on each request, so a session that started before the marker was cleared
/// (`import --replace`) stops at its next call. `account` is the read of this request.
pub fn protect_gate(state: &AppState, account: &Account, serial: &str) -> Option<Reply> {
    (state.config.protect && account.imported.is_none()).then(|| protect_refused(serial))
}

/// The protect gate again, read fresh under the store lock: a request that passed
/// [`protect_gate`] and then waited for the lock behind an import that cleared the marker (and
/// failed) must not see the half-imported tree. Digest and to-do routes call it right after
/// `lock_recovered`; file routes call [`gate_locked`]. [`blocking`] turns the error into the
/// `E0079` answer.
pub fn protect_locked(state: &AppState, slug: &str) -> crate::Result<()> {
    if !state.config.protect {
        return Ok(());
    }
    match state.auth.accounts().get(slug)? {
        Some(a) if a.imported.is_some() => Ok(()),
        _ => Err(Error::GateClosed(slug.to_string())),
    }
}

/// The gates of a file route under the store lock: [`protect_locked`], then the scan hold. While
/// `scan-hold.json` exists (or can't be read) the route answers `E0079` with `scan_hold`. Call
/// it right after `lock_recovered` in every file route.
pub fn gate_locked(state: &AppState, store: &Store) -> crate::Result<()> {
    protect_locked(state, store.slug())?;
    match store.scan_hold() {
        Ok(None) => Ok(()),
        Ok(Some(_)) => Err(Error::ScanHold(store.slug().to_string())),
        Err(e) => {
            eprintln!(
                "notedown cloud: account {}: can't read the scan hold, device sync waits: {e}",
                store.slug()
            );
            Err(Error::ScanHold(store.slug().to_string()))
        }
    }
}

/// The scan of a device listing (`docs/sync-protocol.md` 8.3): `scope` and what lies below it
/// (only the direct children when not `recursive`), right before the listing under the same
/// guard, so a change made by hand is journaled before the device sees it. A hold is
/// `Error::ScanHold`. Returns the scan's walk, which the listing reuses
/// ([`Store::list_walked`]).
fn scan_scope(
    state: &AppState,
    store: &Store,
    guard: &WriteGuard,
    scope: &WirePath,
    recursive: bool,
) -> crate::Result<Option<Walk>> {
    let opts = ScanOptions {
        scope: scope.clone(),
        recursive,
        dry_run: false,
        accept: false,
        min_files: state.config.scan_min_files,
    };
    state
        .run_scan_walked(store, guard, &opts)
        .map(|(_, walk)| walk)
}

/// [`scan_scope`] for every existing folder on the way to `target` (stored spelling,
/// [`Store::way_scopes`]): the root, then each folder above `target` that is a real folder on
/// disk, direct children only.
fn scan_way(
    state: &AppState,
    store: &Store,
    guard: &WriteGuard,
    target: &WirePath,
) -> crate::Result<()> {
    for scope in store.way_scopes(target)? {
        scan_scope(state, store, guard, &scope, false)?;
    }
    Ok(())
}

/// [`scan_scope`] for the entry at `path`: its parent folder, direct children only.
fn scan_around(
    state: &AppState,
    store: &Store,
    guard: &WriteGuard,
    path: &WirePath,
) -> crate::Result<()> {
    let parent = path.parent().unwrap_or_else(WirePath::root);
    scan_scope(state, store, guard, &parent, false).map(drop)
}

/// The answer for a damaged store (`Error::StoreDamaged`): `E0079` with flag `store_damaged`,
/// the reason in the log entry's `message`, and a warning on stderr. Never a listing.
pub fn store_damaged(serial: &str, e: &Error) -> Reply {
    let text = e.to_string();
    eprintln!("notedown cloud: warning: {text}");
    Reply::sync_busy()
        .flag(FLAG_STORE_DAMAGED)
        .message(&text)
        .device(serial)
}

/// The answer of a closed protect gate: `E0079` with flag `protect_refused`.
pub fn protect_refused(serial: &str) -> Reply {
    Reply::sync_busy().flag(FLAG_PROTECT_REFUSED).device(serial)
}

/// The answer while a scan holds: `E0079` with flag `scan_hold`.
pub fn scan_held(serial: &str) -> Reply {
    Reply::sync_busy().flag(FLAG_SCAN_HOLD).device(serial)
}

/// [`device_request!`] followed by the protect gate, then a lapsed sync lock of this device is
/// taken again.
macro_rules! gated_request {
    ($state:expr, $headers:expr, $bytes:expr) => {{
        let req = device_request!($state, $headers, $bytes);
        if let Some(reply) = protect_gate(&$state, &req.account, &req.serial) {
            return reply;
        }
        // The device that held the sync lock last keeps it across a pause in its session.
        $state
            .sync_locks
            .retake(&req.slug, &req.serial, $state.now_ms());
        req
    }};
}

/// `POST /api/file/2/files/synchronous/start`. Order: device check, the protect gate (accept
/// mode with `protect = true` refuses with `E0079` until the account has an `imported` marker),
/// `E0078` while another device holds the lock (checked before the store lock and again under
/// it, so nothing is scanned under another device's sync), a scan of the whole tree (`E0079`
/// with `scan_hold` when it holds; it also ends a hold when the files are back), the store
/// check (`E0079` with `store_damaged` for a damaged tree, and with `empty_account` for an
/// account without marker and without files), then the lock is taken (`E0078` if another
/// device got it meanwhile). Answers `synType: true`.
pub async fn sync_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = device_request!(state, headers, bytes);
    if let Some(reply) = protect_gate(&state, &req.account, &req.serial) {
        return reply;
    }
    // The store is checked before the lock is taken: a damaged tree (`store_damaged`) or, with
    // protect off, an account that was never imported and holds no file (`empty_account`) gets
    // no sync, so the device never compares against an empty tree.
    // The changes made by hand are journaled first, by a scan of the whole tree under the same
    // store lock. The hold is checked by that scan only, not by `gate_locked`: a scan that sees
    // the files back ends it.
    // The journal position is read after the scan (store lock, then journal lock): the sync
    // lists the tree after it, so pushes for the changes up to it are covered (D7). It is only
    // a hint, so a failure to read it is logged and never fails the sync.
    // Another device's live lock answers E0078 before anything is scanned: its sync must not
    // see the journal and the registry change under it.
    if state
        .sync_locks
        .holder(&req.slug, state.now_ms())
        .is_some_and(|h| h != req.serial)
    {
        return req.error(E_SYNC_LOCKED, MSG_SYNC_LOCKED);
    }
    let (st, slug, serial) = (state.clone(), req.slug.clone(), req.serial.clone());
    let checked = blocking(&req.serial, move || {
        let store = st.store(&slug)?;
        let guard = store.lock_recovered(&st.journal)?;
        protect_locked(&st, store.slug())?;
        // Again under the store lock: another device may have started while this one waited.
        if st
            .sync_locks
            .holder(&slug, st.now_ms())
            .is_some_and(|h| h != serial)
        {
            return Ok(None);
        }
        st.run_scan(&store, &guard, &ScanOptions::all(st.config.scan_min_files))?;
        let last_seq = match st.journal.last_seq() {
            Ok(seq) => Some(seq),
            Err(e) => {
                eprintln!("notedown cloud: account {slug}: journal position for pushes: {e}");
                None
            }
        };
        let imported = st
            .auth
            .accounts()
            .get(store.slug())?
            .is_some_and(|a| a.imported.is_some());
        if imported {
            return Ok(Some((false, last_seq)));
        }
        let entries = store.list(&guard, &WirePath::root(), true)?;
        Ok(Some((
            !entries.iter().any(|e| e.tag == Kind::File),
            last_seq,
        )))
    })
    .await;
    let last_seq = match checked {
        Ok(None) => return req.error(E_SYNC_LOCKED, MSG_SYNC_LOCKED),
        Ok(Some((false, last_seq))) => last_seq,
        Ok(Some((true, _))) => {
            return Reply::sync_busy()
                .flag(FLAG_EMPTY_ACCOUNT)
                .device(&req.serial);
        }
        Err(r) => return r,
    };
    match state
        .sync_locks
        .start(&req.slug, &req.serial, state.now_ms())
    {
        StartOutcome::Busy { .. } => req.error(E_SYNC_LOCKED, MSG_SYNC_LOCKED),
        StartOutcome::Taken | StartOutcome::Renewed => {
            if let Some(seq) = last_seq {
                state
                    .push
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .covered(&req.slug, &req.serial, seq);
            }
            req.ok(json!({"synType": true}))
        }
    }
}

/// `POST /api/file/2/files/synchronous/end` (`flag` `Y` or `N`): releases this device's lock.
/// The flag is logged (`sync_end_ok`, `sync_end_failed`).
pub async fn sync_end(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = device_request!(state, headers, bytes);
    let released = state.sync_locks.end(&req.slug, &req.serial, state.now_ms());
    let mut reply = req.ok(json!({}));
    reply = match field(&req.body, "flag").as_deref() {
        Some("Y") | Some("y") => reply.flag(FLAG_SYNC_END_OK),
        Some("N") | Some("n") => reply.flag(FLAG_SYNC_END_FAILED),
        _ => reply.flag(FLAG_SYNC_END_UNKNOWN),
    };
    if !released {
        reply = reply.flag(FLAG_SYNC_LOCK_NOT_HELD);
    }
    reply
}

/// The folder a listing is for.
enum Folder {
    Path(WirePath),
    /// A folder id, resolved under the recovered store lock.
    Id(u64),
}

/// What a listing found.
enum Listing {
    Entries(Vec<Entry>),
    /// Not a folder (`E0081`).
    Missing,
    /// A segment of the path matches two entries without case (`E0322`).
    Ambiguous,
}

/// The listing of a folder (the root included). A path is looked up with the stored spelling
/// of each segment (3.0). The id or path is resolved after `lock_recovered`, so an unfinished
/// commit is repaired first.
fn list_folder_of(
    state: &AppState,
    slug: &str,
    folder: Folder,
    recursive: bool,
) -> crate::Result<Listing> {
    let store = state.store(slug)?;
    let guard = store.lock_recovered(&state.journal)?;
    gate_locked(state, &store)?;
    let path = match folder {
        Folder::Path(p) => match stored(&store, &guard, &p)? {
            Some(r) => r.path,
            None => return Ok(Listing::Ambiguous),
        },
        Folder::Id(id) => match store.kind_and_path_of(id)? {
            Some((Kind::Folder, p)) => p,
            _ => return Ok(Listing::Missing),
        },
    };
    let walk = scan_scope(state, &store, &guard, &path, recursive)?;
    if !path.is_root() {
        match store.entry(&guard, &path)? {
            Some(e) if e.tag == Kind::Folder => {}
            _ => return Ok(Listing::Missing),
        }
    }
    store
        .list_walked(&guard, &path, recursive, walk)
        .map(Listing::Entries)
}

/// `POST /api/file/2/files/list_folder` `{path, recursive}`: `entries`. `""` or `/` is the
/// root. Names compare without case: a path in another spelling lists the folder, with the
/// stored spelling in the entries. A path that is not a folder answers `E0081` (never an empty
/// list, which the device could read as "everything deleted"); a segment that matches two
/// entries without case and none exactly answers `E0322` (flag `case_ambiguous`).
pub async fn list_folder(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = gated_request!(state, headers, bytes);
    let text = match req.body.get("path") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => return req.error(E_PATH_MISSING, MSG_PATH_MISSING),
    };
    let Ok(path) = WirePath::parse(&text) else {
        return req.error(E_PATH_MISSING, MSG_PATH_MISSING);
    };
    let recursive = flag_arg(&req.body, "recursive");
    let (st, slug) = (state.clone(), req.slug.clone());
    // A first listing after a hash-cache reset hashes the whole tree: keep the lock alive.
    let listed = keep_lock(
        &state,
        &req.slug,
        &req.serial,
        blocking(&req.serial, move || {
            list_folder_of(&st, &slug, Folder::Path(path), recursive)
        }),
    )
    .await;
    match listed {
        Ok(Listing::Entries(entries)) => req.ok(json!({"entries": entries_value(&entries)})),
        Ok(Listing::Missing) => req.error(E_PATH_MISSING, MSG_PATH_MISSING),
        Ok(Listing::Ambiguous) => Refusal::Ambiguous.reply(&req.serial),
        Err(r) => r,
    }
}

/// `POST /api/file/3/files/list_folder_v3` `{id, recursive}`: the entries of the folder with
/// that id; id `0` or null is the root. An unknown id or a file id (also a string or number that
/// is no valid id) answers `E0081`; an id of another JSON type answers `422`.
pub async fn list_folder_v3(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = gated_request!(state, headers, bytes);
    let id = match id_arg(&req.body, "id") {
        IdArg::Missing | IdArg::Id(0) => None,
        IdArg::Id(n) => Some(n),
        IdArg::Bad
            if matches!(
                req.body.get("id"),
                Some(Value::String(_) | Value::Number(_))
            ) =>
        {
            return req.error(E_PATH_MISSING, MSG_PATH_MISSING);
        }
        IdArg::Bad => return Reply::bad_params("id").device(&req.serial),
    };
    let recursive = flag_arg(&req.body, "recursive");
    let (st, slug) = (state.clone(), req.slug.clone());
    let folder = id.map_or(Folder::Path(WirePath::root()), Folder::Id);
    let r = keep_lock(
        &state,
        &req.slug,
        &req.serial,
        blocking(&req.serial, move || {
            list_folder_of(&st, &slug, folder, recursive)
        }),
    )
    .await;
    match r {
        Ok(Listing::Entries(entries)) => req.ok(json!({"entries": entries_value(&entries)})),
        Ok(Listing::Missing) => req.error(E_PATH_MISSING, MSG_PATH_MISSING),
        Ok(Listing::Ambiguous) => Refusal::Ambiguous.reply(&req.serial),
        Err(r) => r,
    }
}

/// The entry with `id`, if the id is known and something listable is at its path. Its parent
/// folder is scanned first ([`scan_around`]).
fn entry_by_id(state: &AppState, slug: &str, id: u64) -> crate::Result<Option<Entry>> {
    let store = state.store(slug)?;
    let guard = store.lock_recovered(&state.journal)?;
    gate_locked(state, &store)?;
    let Some(path) = store.path_of(id)? else {
        return Ok(None);
    };
    scan_around(state, &store, &guard, &path)?;
    // The scan may have dropped the id (the file is gone) or moved it.
    let Some(path) = store.path_of(id)? else {
        return Ok(None);
    };
    Ok(store.entry(&guard, &path)?.filter(|e| e.id == id))
}

/// `POST /api/file/3/files/query_v3` `{id}`: `entriesVO`, or null for an unknown id (or one
/// that is no id at all).
pub async fn query_v3(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = gated_request!(state, headers, bytes);
    let IdArg::Id(id) = id_arg(&req.body, "id") else {
        return req.ok(json!({"entriesVO": null}));
    };
    let (st, slug) = (state.clone(), req.slug.clone());
    match blocking(&req.serial, move || entry_by_id(&st, &slug, id)).await {
        Ok(e) => req.ok(json!({"entriesVO": entry_value(e.as_ref())})),
        Err(r) => r,
    }
}

/// `POST /api/file/3/files/query/by/path_v3` `{path}`: `entriesVO` or null, always HTTP 200
/// (a missing, unparsable or hidden path and the root give null). Names compare without case;
/// the entry has the stored spelling. A segment that matches two entries without case and none
/// exactly answers `E0322` (flag `case_ambiguous`).
pub async fn query_by_path_v3(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = gated_request!(state, headers, bytes);
    let Some(path) = req
        .body
        .get("path")
        .and_then(Value::as_str)
        .and_then(|t| WirePath::parse(t).ok())
        .filter(|p| !p.is_root())
    else {
        return req.ok(json!({"entriesVO": null}));
    };
    let (st, slug) = (state.clone(), req.slug.clone());
    let r = blocking(&req.serial, move || {
        let store = st.store(&slug)?;
        let guard = store.lock_recovered(&st.journal)?;
        gate_locked(&st, &store)?;
        match stored(&store, &guard, &path)? {
            Some(r) => {
                scan_around(&st, &store, &guard, &r.path)?;
                store.entry(&guard, &r.path).map(Some)
            }
            None => Ok(None),
        }
    })
    .await;
    match r {
        Ok(Some(e)) => req.ok(json!({"entriesVO": entry_value(e.as_ref())})),
        Ok(None) => Refusal::Ambiguous.reply(&req.serial),
        Err(r) => r,
    }
}

/// `POST /api/file/3/files/download_v3` `{id}`: a signed download URL (valid 24 h) and the
/// file's entry fields. Unknown id, a folder, a vanished file or an id of another type: `E0321`.
pub async fn download_v3(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = gated_request!(state, headers, bytes);
    let IdArg::Id(id) = id_arg(&req.body, "id") else {
        return req.error(E_NO_FILE, MSG_NO_FILE);
    };
    let (st, slug) = (state.clone(), req.slug.clone());
    let entry = match blocking(&req.serial, move || entry_by_id(&st, &slug, id)).await {
        Ok(Some(e)) if e.tag == Kind::File => e,
        Ok(_) => return req.error(E_NO_FILE, MSG_NO_FILE),
        Err(r) => return r,
    };
    let target = SignedTarget {
        account: req.slug.clone(),
        device: req.serial.clone(),
        generation: req.generation,
        path: entry.path_display.clone(),
        id: Some(entry.id),
        ..SignedTarget::default()
    }
    .encode();
    let signed = state
        .url_signer
        .sign("GET", &target, Purpose::Download, 0, state.now_ms());
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok());
    let base = base_url(state.config.external_base_url.as_deref(), host);
    req.ok(json!({
        "id": entry.id.to_string(),
        "url": download_url(&base, &target, &signed, entry.id),
        "name": entry.name,
        "path_display": entry.path_display,
        "content_hash": entry.content_hash,
        "size": entry.size,
        "is_downloadable": entry.is_downloadable,
    }))
}

/// `POST /api/file/capacity/query`: `usedCapacity` (bytes of every listed file, cached for
/// 60 s) and `totalCapacity` (`quota_bytes` of `account.json`, else 64 GiB).
pub async fn capacity_query(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = device_request!(state, headers, bytes);
    let (st, slug) = (state.clone(), req.slug.clone());
    match blocking(&req.serial, move || st.used_bytes(&slug)).await {
        Ok(used) => req.ok(json!({
            "usedCapacity": used,
            "totalCapacity": req.account.quota_bytes.unwrap_or(DEFAULT_QUOTA_BYTES),
        })),
        Err(r) => r,
    }
}

/// `POST /api/file/2/users/get_space_usage`: `used` and `allocationVO {tag: "individual",
/// allocated}`, the same numbers as `capacity/query`.
pub async fn space_usage(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = device_request!(state, headers, bytes);
    let (st, slug) = (state.clone(), req.slug.clone());
    match blocking(&req.serial, move || st.used_bytes(&slug)).await {
        Ok(used) => req.ok(json!({
            "used": used,
            "allocationVO": {
                "tag": "individual",
                "allocated": req.account.quota_bytes.unwrap_or(DEFAULT_QUOTA_BYTES),
            },
        })),
        Err(r) => r,
    }
}

// ------------------------------------------------------------------ uploads

/// Whether `serial` holds the sync lock of `slug` for a write now. Call it after the protect
/// gate. The device that held the lock last keeps it ([`SyncLocks::retake`]). A device that
/// writes without `synchronous/start` (Auto Sync sends a changed file this way,
/// `docs/sync-protocol.md` 9.3) takes a free or lapsed lock; the protocol log gets a marker with
/// the flag `implicit_lock`. That needs the account's `imported` marker: without it
/// `synchronous/start` refuses an account without files (`empty_account`), and a write
/// without a session must not fill such an account past that check. Another device's live
/// lock answers `E0079`.
///
/// [`SyncLocks::retake`]: crate::sync::SyncLocks::retake
pub fn holds_sync_lock(state: &AppState, slug: &str, serial: &str) -> bool {
    let now = state.now_ms();
    if state.sync_locks.retake(slug, serial, now) {
        return true;
    }
    let imported = match state.auth.accounts().get(slug) {
        Ok(a) => a.is_some_and(|a| a.imported.is_some()),
        Err(e) => {
            eprintln!("notedown cloud: {e}");
            false
        }
    };
    if !imported {
        return false;
    }
    match state.sync_locks.take_for_write(slug, serial, now) {
        WriteLock::Held => true,
        WriteLock::Taken => {
            let mut m = LogEntry::new(LogKind::Marker);
            m.device = Some(serial.to_string());
            m.flags = vec![FLAG_IMPLICIT_LOCK.to_string()];
            m.message = Some(format!(
                "account {slug}: sync lock taken for a write without synchronous/start"
            ));
            state.log.record(m);
            true
        }
        WriteLock::Busy => false,
    }
}

/// How often [`keep_lock`] extends the sync lock (well inside its 30 s TTL).
pub const KEEP_LOCK_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

/// Await `work` (a long blocking step: a merge, hashing, a commit) while extending the device's
/// sync lock every 10 s, so the lock does not lapse inside one request. The keeper task is
/// aborted when `work` ends and also when this future is dropped (a cut connection).
pub async fn keep_lock<F: std::future::Future>(
    state: &Arc<AppState>,
    slug: &str,
    serial: &str,
    work: F,
) -> F::Output {
    keep_lock_every(state, slug, serial, KEEP_LOCK_EVERY, work).await
}

/// [`keep_lock`] with another interval. Test hook.
#[doc(hidden)]
pub async fn keep_lock_every<F: std::future::Future>(
    state: &Arc<AppState>,
    slug: &str,
    serial: &str,
    every: std::time::Duration,
    work: F,
) -> F::Output {
    let (st, slug, serial) = (state.clone(), slug.to_string(), serial.to_string());
    let _keeper = crate::server::AbortOnDrop(tokio::spawn(async move {
        let mut ticks = tokio::time::interval(every);
        loop {
            ticks.tick().await;
            st.sync_locks.touch(&slug, &serial, st.now_ms());
        }
    }));
    work.await
}

/// `E0079` with the log flag `no_sync_lock`.
pub fn no_sync_lock(serial: &str) -> Reply {
    Reply::sync_busy().flag(FLAG_NO_SYNC_LOCK).device(serial)
}

/// A size as a JSON number or a string of digits (`upload/finish` sends a string).
pub fn size_arg(body: &Value, key: &str) -> Option<u64> {
    match body.get(key)? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => {
            let s = s.trim();
            (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                .then(|| s.parse().ok())
                .flatten()
        }
        _ => None,
    }
}

/// The parent folder and the target of an upload, by one rule for `upload/apply` and
/// `upload/finish` (`docs/sync-protocol.md` 7.3):
///
/// - A trailing slash means `path` is the folder (finish sends `/NOTE/Note/Work/`).
/// - Without it, a `path` whose last segment is `fileName` is the full path (apply sends
///   `/NOTE/Note/Work/x.note`); any other `path` is the folder.
///
/// Double slashes are fine. `None` for a path or name the store cannot take. Finish must arrive
/// at the parent signed at apply, so a `/NOTE/Note/x.note/` finish after a
/// `/NOTE/Note/x.note` apply is refused and never writes `x.note/x.note`.
pub fn upload_target(path: &str, file_name: &str) -> Option<(WirePath, WirePath)> {
    let p = WirePath::parse(path).ok()?;
    let folder = path.trim_end().ends_with('/');
    let parent = if !folder && p.name() == Some(file_name) {
        p.parent()?
    } else {
        p
    };
    let target = parent.join(file_name).ok()?;
    Some((parent, target))
}

/// Why an upload target is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// `E0322`: a name collision, or a folder at the target.
    Taken,
    /// `E0322` with the flag `case_ambiguous`: a segment matches two entries without case.
    Ambiguous,
    /// `E0324` with the flag `name_too_long`.
    TooLong,
    /// `E0324` with a log flag.
    Cannot(&'static str),
}

impl Refusal {
    fn reply(self, serial: &str) -> Reply {
        match self {
            Refusal::Taken => Reply::error(E_NAME_TAKEN, MSG_NAME_TAKEN).device(serial),
            Refusal::Ambiguous => Reply::error(E_NAME_TAKEN, MSG_NAME_TAKEN)
                .flag(FLAG_CASE_AMBIGUOUS)
                .device(serial),
            Refusal::TooLong => Reply::error(E_UPLOAD, MSG_NAME_TOO_LONG)
                .flag(FLAG_NAME_TOO_LONG)
                .device(serial),
            Refusal::Cannot(flag) => Reply::error(E_UPLOAD, MSG_UPLOAD).flag(flag).device(serial),
        }
    }
}

/// A store error that refuses the target (and is no fault of the server).
fn refusal_of(e: &Error) -> Option<Refusal> {
    match e {
        Error::NameCollision { .. } => Some(Refusal::Taken),
        Error::Invalid { .. } => Some(Refusal::Cannot(FLAG_BAD_NAME)),
        _ => None,
    }
}

/// Check that `target` (with the stored spelling, see [`stored`]) can take a file: no case or
/// normalization collision on its way (`E0322`), no dot-name and no file in place of a parent
/// folder (`E0324`), no folder at the target (`E0322`).
fn check_target(
    store: &Store,
    guard: &StoreGuard,
    target: &WirePath,
) -> crate::Result<Option<Refusal>> {
    if let Err(e) = store.check_path(guard, target) {
        return refusal_of(&e).map(Some).ok_or(e);
    }
    let mut cur = WirePath::root();
    for segment in target.segments() {
        cur = cur.join(segment)?;
        let Some(e) = store.entry(guard, &cur)? else {
            break;
        };
        let last = &cur == target;
        match (e.tag, last) {
            (Kind::Folder, true) => return Ok(Some(Refusal::Taken)),
            (Kind::File, false) => return Ok(Some(Refusal::Cannot(FLAG_BAD_NAME))),
            _ => {}
        }
    }
    Ok(None)
}

/// `POST /api/file/3/files/upload/apply` `{path (full path with the name), fileName, size}`:
/// the signed upload URLs. Needs the sync lock (`E0079`). The name is checked first: a new name
/// over 255 bytes on the way answers `E0324` (flag `name_too_long`), a dot-name `E0324`, a
/// folder at the target `E0322`. Names compare without case (3.0): a case variant of an existing file
/// or folder on the way is that entry; two such entries and no exact one answer `E0322` (flag
/// `case_ambiguous`). `innerName` is `fileName`, as in Ratta's traffic.
/// The upload gets a fresh internal id (a UUID) that names its staging entry; the URLs sign the
/// account, the device, its token generation, the parent folder, that id and `size`. The apply
/// is recorded in the entry, so `upload/finish` finds it by device, parent and name.
pub async fn upload_apply(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = gated_request!(state, headers, bytes);
    if !holds_sync_lock(&state, &req.slug, &req.serial) {
        return no_sync_lock(&req.serial);
    }
    let Some(file_name) = req
        .body
        .get("fileName")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
    else {
        return Reply::bad_params("fileName").device(&req.serial);
    };
    let path = req.body.get("path").and_then(Value::as_str).unwrap_or("");
    let Some((_, target)) = upload_target(path, &file_name) else {
        return Refusal::Cannot(FLAG_BAD_NAME).reply(&req.serial);
    };
    let size = size_arg(&req.body, "size").unwrap_or(0);
    if size > state.file_max() {
        return Refusal::Cannot(FLAG_UPLOAD_TOO_LARGE).reply(&req.serial);
    }
    // The internal id of the upload: signed into the URLs, names the staging entry.
    let inner = crate::journal::new_op_id();
    let parent = target.parent().unwrap_or_default().display();
    let (st, slug, t) = (state.clone(), req.slug.clone(), target.clone());
    let (serial, inner_c, parent_c, name_c, now) = (
        req.serial.clone(),
        inner.clone(),
        parent.clone(),
        file_name.clone(),
        state.now_ms(),
    );
    let checked = blocking(&req.serial, move || {
        let store = st.store(&slug)?;
        let guard = store.lock_recovered(&st.journal)?;
        gate_locked(&st, &store)?;
        // The staging entry and the URLs keep the device's spelling: finish looks the target
        // up again.
        let Some(t) = stored(&store, &guard, &t)? else {
            return Ok(Some(Refusal::Ambiguous));
        };
        if t.new_name_too_long() {
            return Ok(Some(Refusal::TooLong));
        }
        scan_way(&st, &store, &guard, &t.path)?;
        let refused = check_target(&store, &guard, &t.path)?;
        if refused.is_none() {
            let entry = staging::entry_dir(store.account_dir(), &inner_c)?;
            staging::record_apply(&entry, &serial, &parent_c, &name_c, now)?;
        }
        Ok(refused)
    })
    .await;
    match checked {
        Ok(None) => {}
        Ok(Some(refusal)) => return refusal.reply(&req.serial),
        Err(r) => return r,
    }
    let signed_target = SignedTarget {
        account: req.slug.clone(),
        device: req.serial.clone(),
        generation: req.generation,
        path: parent,
        id: None,
        inner: Some(inner),
        size,
    }
    .encode();
    let signed = state.url_signer.sign(
        "POST",
        &signed_target,
        Purpose::Upload,
        size,
        state.now_ms(),
    );
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok());
    let base = base_url(state.config.external_base_url.as_deref(), host);
    req.ok(json!({
        "bucketName": file_name,
        // As in Ratta's traffic: the file name. The internal id is only in the signed URLs.
        "innerName": file_name,
        "xamzDate": signed.timestamp.to_string(),
        "authorization": "",
        "fullUploadUrl": oss_url(&base, "upload", &signed_target, &signed),
        "partUploadUrl": oss_url(&base, "upload/part", &signed_target, &signed),
    }))
}

/// The outcome of `upload/finish`.
enum Finished {
    Done(Entry),
    Refused(Refusal),
}

/// The parsed `upload/finish` request.
struct FinishArgs {
    slug: String,
    serial: String,
    path: String,
    file_name: String,
    inner: String,
    /// `innerName` is the internal id (not the file name).
    by_id: bool,
    content_hash: String,
    size: u64,
    protect: bool,
    now_ms: u64,
}

/// Verify the staged data, then plan, index and commit it under the store lock (blocking).
/// A size or MD5 mismatch removes the staging entry and changes nothing else.
fn finish_upload(state: &AppState, a: FinishArgs) -> crate::Result<Finished> {
    let cannot = |flag| Ok(Finished::Refused(Refusal::Cannot(flag)));
    let store = state.store(&a.slug)?;
    // By internal id: the entry of that id, if this device applied for it (the entry lives
    // below the account, so the account matches). Another device's id is an unknown upload,
    // and its entry stays for the device that applied.
    let by_id = if a.by_id {
        let entry = staging::entry_dir(store.account_dir(), &a.inner)?;
        if !staging::applied_by(&entry, &a.serial) {
            return cannot(FLAG_UPLOAD_MISSING);
        }
        Some(entry)
    } else {
        None
    };
    let Some((parent, target)) = upload_target(&a.path, &a.file_name) else {
        if let Some(entry) = &by_id {
            staging::remove_entry(entry);
        }
        return cannot(FLAG_BAD_NAME);
    };
    // `innerName` is the internal id, or the file name (then the latest apply of this device
    // for this parent and name).
    let entry = if let Some(entry) = by_id {
        entry
    } else {
        match staging::find_applied(
            store.account_dir(),
            &a.serial,
            &parent.display(),
            &a.file_name,
        )? {
            Some(entry) => entry,
            None => return cannot(FLAG_UPLOAD_MISSING),
        }
    };
    // Take the bytes away from every other request first: from here on only this finish
    // reads them, so what is verified is what is committed.
    let op = crate::journal::new_op_id();
    let data = match state.staging_locks.claim(&entry, &parent.display(), &op)? {
        staging::Claim::Claimed(data) => data,
        // Unknown upload, or parts still missing: the entry stays for a retry of the part.
        staging::Claim::Missing => return cannot(FLAG_UPLOAD_MISSING),
        staging::Claim::ParentMismatch => return cannot(FLAG_PARENT_MISMATCH),
    };
    let r = finish_claimed(state, &store, &a, &entry, &target, &data);
    if r.is_err() {
        // Not committed: give the bytes back so the finish can be retried. An intent that
        // names the claimed file belongs to recovery and is left alone.
        let staged_in_intent = match store.pending() {
            Ok(Some(c)) => matches!(
                &c.op,
                PendingOp::Put { staged, .. } if store.account_dir().join(staged) == data
            ),
            _ => false,
        };
        if !staged_in_intent && let Err(e) = state.staging_locks.release(&entry, &data) {
            eprintln!("notedown cloud: cannot give the upload back: {e}");
        }
    }
    r
}

/// The part of [`finish_upload`] after the claim: verify size and MD5 and commit.
fn finish_claimed(
    state: &AppState,
    store: &Store,
    a: &FinishArgs,
    entry: &std::path::Path,
    target: &WirePath,
    data: &std::path::Path,
) -> crate::Result<Finished> {
    let cannot = |flag| Ok(Finished::Refused(Refusal::Cannot(flag)));
    let len = std::fs::metadata(data).map_err(|e| Error::Io {
        path: data.to_path_buf(),
        source: e,
    })?;
    if len.len() != a.size {
        staging::remove_entry(entry);
        return cannot(FLAG_SIZE_MISMATCH);
    }
    let guard = store.lock_recovered(&state.journal)?;
    gate_locked(state, store)?;
    // A case variant of an existing file is that file: it is replaced and keeps its id and
    // spelling (3.0).
    let Some(target) = stored(store, &guard, target)? else {
        staging::remove_entry(entry);
        return Ok(Finished::Refused(Refusal::Ambiguous));
    };
    if target.new_name_too_long() {
        staging::remove_entry(entry);
        return Ok(Finished::Refused(Refusal::TooLong));
    }
    let target = &target.path;
    scan_way(state, store, &guard, target)?;
    if let Some(refusal) = check_target(store, &guard, target)? {
        staging::remove_entry(entry);
        return Ok(Finished::Refused(refusal));
    }
    let options = PutOptions {
        create_parents: true,
        recycle_at_ms: a.protect.then_some(a.now_ms),
        // The one hash of the staged bytes, under the store lock, before any id is assigned.
        expect_md5: Some(a.content_hash.clone()),
        drop_md5: false,
    };
    let planned = store.plan_put_with(
        &guard,
        target,
        data,
        Origin::Device,
        Some(a.serial.clone()),
        options,
    );
    let mut commit = match planned {
        Ok(c) => c,
        Err(Error::Invalid { what, .. }) if what == crate::store::CONTENT_HASH_MISMATCH => {
            staging::remove_entry(entry);
            return cannot(FLAG_HASH_MISMATCH);
        }
        Err(e) => match refusal_of(&e) {
            Some(refusal) => {
                staging::remove_entry(entry);
                return Ok(Finished::Refused(refusal));
            }
            None => return Err(e),
        },
    };
    crate::pages::plan_page_events(store, &guard, &mut commit);
    store.commit(&guard, commit, &state.journal)?;
    let found = store.entry(&guard, target)?;
    // The file's cache row goes to disk before the answer, as the CLI put does: a row in
    // memory only is lost at a stop or when another process writes the cache, and a later
    // edit by hand would then be taken in without an event.
    store.flush_cache(&guard)?;
    let listed = found
        .filter(|e| e.tag == Kind::File)
        .ok_or_else(|| Error::Invalid {
            what: "upload",
            message: format!("{target} is not a file after the commit"),
        })?;
    drop(guard);
    staging::remove_entry(entry);
    Ok(Finished::Done(listed))
}

/// `POST /api/file/2/files/upload/finish` `{path (parent folder + "/"), fileName,
/// content_hash, innerName, size (string)}`: `innerName` is the file name (the latest apply of
/// this device for this parent folder and name) or the internal id of an upload this device
/// applied for (log flags `inner_name_file_name` / `inner_name_id`; equal to `fileName` is
/// always the file name). It claims the staged data (no other request can change
/// it afterwards), checks that the parent is the one signed at apply, verifies size and MD5
/// (`E0324` and nothing kept on a mismatch), looks the target up without case (a case variant
/// of an existing file replaces it and keeps its id and spelling), creates missing parent folders
/// (`folder.created`), writes the file (`file.created`, or `file.modified` with `prevMd5` and
/// the same id), journals the page diff of a note, all in one commit. In protect mode the
/// replaced bytes go to the recycle bin first. Needs the sync lock (`E0079`). Answers
/// `{path_display, id, size, name, content_hash}`.
pub async fn upload_finish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = gated_request!(state, headers, bytes);
    if !holds_sync_lock(&state, &req.slug, &req.serial) {
        return no_sync_lock(&req.serial);
    }
    let text = |k: &str| req.body.get(k).and_then(Value::as_str).map(str::to_string);
    let Some(file_name) = text("fileName").filter(|n| !n.is_empty()) else {
        return Reply::bad_params("fileName").device(&req.serial);
    };
    // The file name (what `upload/apply` answers) or the internal id; anything else is unknown.
    let Some(inner) = text("innerName").filter(|i| *i == file_name || staging::valid_inner_name(i))
    else {
        return Refusal::Cannot(FLAG_UPLOAD_MISSING).reply(&req.serial);
    };
    // The file name route first: a file may be named like an internal id.
    let by_id = inner != file_name;
    let form = if by_id {
        FLAG_INNER_NAME_ID
    } else {
        FLAG_INNER_NAME_FILE
    };
    let (Some(content_hash), Some(size)) = (text("content_hash"), size_arg(&req.body, "size"))
    else {
        return Reply::bad_params("content_hash, size").device(&req.serial);
    };
    let args = FinishArgs {
        slug: req.slug.clone(),
        serial: req.serial.clone(),
        path: text("path").unwrap_or_default(),
        file_name,
        inner,
        by_id,
        content_hash,
        size,
        protect: state.config.protect,
        now_ms: state.now_ms(),
    };
    let st = state.clone();
    let r = keep_lock(
        &state,
        &req.slug,
        &req.serial,
        blocking(&req.serial, move || finish_upload(&st, args)),
    )
    .await;
    state.invalidate_usage(&req.slug);
    let reply = match r {
        Ok(Finished::Done(e)) => req.ok(json!({
            "path_display": e.path_display,
            "id": e.id.to_string(),
            "size": e.size,
            "name": e.name,
            "content_hash": e.content_hash,
        })),
        Ok(Finished::Refused(refusal)) => refusal.reply(&req.serial),
        Err(r) => r,
    };
    // Which form of `innerName` the device sent, for the log.
    reply.flag(form)
}

// ------------------------------------------------------------------ folders, delete, move, copy

/// The id to delete is unknown (or its file is gone).
pub const E_DELETE_MISSING: &str = "E0318";
/// The move source is unknown (or gone).
pub const E_MOVE_MISSING: &str = "E0320";
/// The copy source is unknown (or gone).
pub const E_COPY_MISSING: &str = "E0308";
/// A folder cannot be moved (or copied) into itself.
pub const E_INTO_ITSELF: &str = "E0358";

pub const MSG_DELETE_MISSING: &str = "The file or folder to delete does not exist";
pub const MSG_MOVE_MISSING: &str = "The file or folder to move does not exist";
pub const MSG_COPY_MISSING: &str = "The file or folder to copy does not exist";
pub const MSG_INTO_ITSELF: &str = "A folder cannot be moved into itself";
pub const MSG_NATIVE_FOLDER: &str = "The system folders cannot be deleted or moved";

/// Log flag: a delete or move of a native folder (or one of their parents), refused.
pub const FLAG_NATIVE_FOLDER: &str = "native_folder";

/// The result of a folder, delete, move or copy call.
enum Outcome {
    /// Success with these payload fields.
    Done(Value),
    /// An error envelope with this code and message, and maybe a log flag.
    Refused(&'static str, &'static str, Option<&'static str>),
}

impl Outcome {
    fn taken() -> crate::Result<Outcome> {
        Ok(Outcome::Refused(E_NAME_TAKEN, MSG_NAME_TAKEN, None))
    }

    fn refused(code: &'static str, msg: &'static str) -> crate::Result<Outcome> {
        Ok(Outcome::Refused(code, msg, None))
    }

    fn ambiguous() -> crate::Result<Outcome> {
        Ok(Outcome::Refused(
            E_NAME_TAKEN,
            MSG_NAME_TAKEN,
            Some(FLAG_CASE_AMBIGUOUS),
        ))
    }

    fn too_long() -> crate::Result<Outcome> {
        Ok(Outcome::Refused(
            E_UPLOAD,
            MSG_NAME_TOO_LONG,
            Some(FLAG_NAME_TOO_LONG),
        ))
    }

    fn native() -> crate::Result<Outcome> {
        Ok(Outcome::Refused(
            super::envelope::E_BAD_PARAMS,
            MSG_NATIVE_FOLDER,
            Some(FLAG_NATIVE_FOLDER),
        ))
    }
}

/// Run a mutation for a device that holds the sync lock: in a blocking task, with the lock kept
/// alive while it runs; the usage cache is cleared afterwards.
async fn mutate<F>(state: &Arc<AppState>, req: &DeviceRequest, work: F) -> Reply
where
    F: FnOnce(&AppState, &str, &str) -> crate::Result<Outcome> + Send + 'static,
{
    if !holds_sync_lock(state, &req.slug, &req.serial) {
        return no_sync_lock(&req.serial);
    }
    let (st, slug, serial) = (state.clone(), req.slug.clone(), req.serial.clone());
    let r = keep_lock(
        state,
        &req.slug,
        &req.serial,
        blocking(&req.serial, move || work(&st, &slug, &serial)),
    )
    .await;
    state.invalidate_usage(&req.slug);
    match r {
        Ok(Outcome::Done(fields)) => req.ok(fields),
        Ok(Outcome::Refused(code, msg, flag)) => {
            let reply = req.error(code, msg);
            match flag {
                Some(f) => reply.flag(f),
                None => reply,
            }
        }
        Err(r) => r,
    }
}

/// Whether a name in a folder can be used.
enum NameState {
    Free,
    /// Something has this name (also in another case or Unicode normalization).
    Taken,
    /// The store never takes this name (a dot-name, not a single segment).
    Refused,
}

/// The state of `name` in `parent` (which may not exist yet). `source` is the name of the entry
/// that is renamed in place, if `parent` is its folder: it does not count as taken.
fn name_state(
    store: &Store,
    guard: &StoreGuard,
    parent: &WirePath,
    name: &str,
    source: Option<&str>,
) -> crate::Result<NameState> {
    let checked = match source {
        Some(src) => store.check_rename(guard, parent, name, src),
        None => store.check_name(guard, parent, name),
    };
    match checked {
        Err(Error::NameCollision { .. }) => return Ok(NameState::Taken),
        Err(Error::Invalid { .. }) => return Ok(NameState::Refused),
        Err(e) => return Err(e),
        Ok(()) => {}
    }
    // check_name lets an entry with exactly this name pass (an upload replaces it); here that
    // entry takes the name, also when the registry does not know it yet.
    let on_disk = std::fs::symlink_metadata(store.fs_path(parent).join(name)).is_ok();
    Ok(if source.is_none() && on_disk {
        NameState::Taken
    } else {
        NameState::Free
    })
}

/// `name(n)`: for a file the suffix goes before the extension (`Bike(1).note`), for a folder
/// at the end (`Work(1)`).
pub fn numbered_name(name: &str, n: u32, kind: Kind) -> String {
    match (kind, name.rfind('.')) {
        (Kind::File, Some(dot)) if dot > 0 => {
            format!("{}({n}){}", &name[..dot], &name[dot..])
        }
        _ => format!("{name}({n})"),
    }
}

/// The name to use for `name` in `parent`: `name` itself when it is free; with `autorename`
/// the first free `name(1)`, `name(2)`, ...; `None` when it is taken (without `autorename`) or
/// refused.
fn pick_name(
    store: &Store,
    guard: &StoreGuard,
    parent: &WirePath,
    name: &str,
    kind: Kind,
    autorename: bool,
    source: Option<&str>,
) -> crate::Result<Option<String>> {
    match name_state(store, guard, parent, name, source)? {
        NameState::Free => return Ok(Some(name.to_string())),
        NameState::Refused => return Ok(None),
        NameState::Taken if !autorename => return Ok(None),
        NameState::Taken => {}
    }
    for n in 1..=10_000 {
        let candidate = numbered_name(name, n, kind);
        match name_state(store, guard, parent, &candidate, source)? {
            NameState::Free => return Ok(Some(candidate)),
            NameState::Refused => return Ok(None),
            NameState::Taken => {}
        }
    }
    Ok(None)
}

/// Whether the folders of `parent` can hold a new entry: every existing segment is a folder
/// spelled exactly so (no case or normalization variant of an existing name, no dot-name, no
/// file on the way). Missing folders are fine (they are created).
fn way_is_open(store: &Store, guard: &StoreGuard, parent: &WirePath) -> crate::Result<bool> {
    match store.check_path(guard, parent) {
        Err(Error::NameCollision { .. } | Error::Invalid { .. }) => return Ok(false),
        Err(e) => return Err(e),
        Ok(()) => {}
    }
    let mut cur = WirePath::root();
    for segment in parent.segments() {
        cur = cur.join(segment)?;
        match store.entry(guard, &cur)? {
            Some(e) if e.tag == Kind::File => return Ok(false),
            Some(_) => {}
            None => break,
        }
    }
    Ok(true)
}

/// Create the folder `path` and each missing folder above it, parents first, one commit each
/// (`folder.created`). Existing folders are kept.
fn create_folders(
    state: &AppState,
    store: &Store,
    guard: &crate::store::WriteGuard,
    path: &WirePath,
    serial: &str,
) -> crate::Result<()> {
    let mut cur = WirePath::root();
    for segment in path.segments() {
        cur = cur.join(segment)?;
        if store
            .entry(guard, &cur)?
            .is_some_and(|e| e.tag == Kind::Folder)
        {
            continue;
        }
        let c = store.plan_create_folder(guard, &cur, Origin::Device, Some(serial.into()))?;
        store.commit(guard, c, &state.journal)?;
    }
    Ok(())
}

/// The listed entry at `path` after a commit.
fn entry_after(store: &Store, guard: &StoreGuard, path: &WirePath) -> crate::Result<Entry> {
    store.entry(guard, path)?.ok_or_else(|| Error::Invalid {
        what: "path",
        message: format!("{path} is not listed after the commit"),
    })
}

/// The entry of `id` when it is known and on disk. The source is scanned first: a folder with
/// everything below it, a file through its parent folder (direct children only). So a hand
/// change inside it is journaled before the write, and a source that is gone from disk is
/// journaled as deleted by the scan and counts as unknown.
fn source_of(
    state: &AppState,
    store: &Store,
    guard: &WriteGuard,
    id: u64,
) -> crate::Result<Option<Entry>> {
    match store.kind_and_path_of(id)? {
        None => return Ok(None),
        Some((Kind::Folder, path)) => {
            scan_scope(state, store, guard, &path, true)?;
        }
        Some((Kind::File, path)) => scan_around(state, store, guard, &path)?,
    }
    let Some(path) = store.path_of(id)? else {
        return Ok(None);
    };
    Ok(store.entry(guard, &path)?.filter(|e| e.id == id))
}

/// `{tag, id, name, path_display}` of an entry.
fn metadata_value(e: &Entry) -> Value {
    json!({
        "tag": e.tag,
        "id": e.id.to_string(),
        "name": e.name,
        "path_display": e.path_display,
    })
}

/// `POST /api/file/2/files/create_folder_v2` `{path, autorename}`: creates the folder (and any
/// missing folder above it) and answers `metadata {tag: "folder", id, name, path_display}` with
/// its real id. A folder above it in another case or Unicode normalization is that folder (its
/// spelling stays). A taken name (also in another case or Unicode normalization, or by a file)
/// answers `E0322`, or with `autorename` becomes `name(1)`, `name(2)`, ... A dot-name answers
/// `E0322`, a new name over 255 bytes `E0324` (flag `name_too_long`), a folder above it that
/// matches two entries without case `E0322` (flag `case_ambiguous`). Needs the sync lock
/// (`E0079`). Event: `folder.created` per new folder.
pub async fn create_folder_v2(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = gated_request!(state, headers, bytes);
    // The lock first: while another device syncs, a call is always E0079, whatever its body.
    if !holds_sync_lock(&state, &req.slug, &req.serial) {
        return no_sync_lock(&req.serial);
    }
    let Some(text) = req
        .body
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Reply::bad_params("path").device(&req.serial);
    };
    let autorename = flag_arg(&req.body, "autorename");
    mutate(&state, &req, move |st, slug, serial| {
        let Ok(path) = WirePath::parse_named(&text) else {
            return Outcome::taken();
        };
        let (parent, name) = match (path.parent(), path.name()) {
            (Some(p), Some(n)) => (p, n.to_string()),
            _ => return Outcome::taken(),
        };
        let store = st.store(slug)?;
        let guard = store.lock_recovered(&st.journal)?;
        gate_locked(st, &store)?;
        let Some(parent) = stored(&store, &guard, &parent)? else {
            return Outcome::ambiguous();
        };
        if parent.new_name_too_long() {
            return Outcome::too_long();
        }
        let parent = parent.path;
        scan_way(st, &store, &guard, &parent.join(&name)?)?;
        if !way_is_open(&store, &guard, &parent)? {
            return Outcome::taken();
        }
        let Some(name) = pick_name(
            &store,
            &guard,
            &parent,
            &name,
            Kind::Folder,
            autorename,
            None,
        )?
        else {
            return Outcome::taken();
        };
        if name.len() > MAX_NAME_BYTES {
            return Outcome::too_long();
        }
        let target = parent.join(&name)?;
        create_folders(st, &store, &guard, &target, serial)?;
        let e = entry_after(&store, &guard, &target)?;
        Ok(Outcome::Done(json!({"metadata": metadata_value(&e)})))
    })
    .await
}

/// `POST /api/file/3/files/delete_folder_v3` `{id}`: moves the file or folder to
/// `recycle/<ms>/<path>` with an index line (`reason: "deleted"`) and answers `metadata {tag, id,
/// name, path_display}`. Events `file.deleted` / `folder.deleted` (for a folder with content,
/// its content first), then `folder.deleted` for each user folder the delete leaves empty
/// (never a native folder): the device deletes a folder one file at a time. Unknown id:
/// `E0318`. A native folder is refused. One delete per commit. Needs the sync lock (`E0079`).
pub async fn delete_folder_v3(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = gated_request!(state, headers, bytes);
    let id = id_arg(&req.body, "id");
    let now = state.now_ms();
    mutate(&state, &req, move |st, slug, serial| {
        let IdArg::Id(id) = id else {
            return Outcome::refused(E_DELETE_MISSING, MSG_DELETE_MISSING);
        };
        let store = st.store(slug)?;
        let guard = store.lock_recovered(&st.journal)?;
        gate_locked(st, &store)?;
        let Some(e) = source_of(st, &store, &guard, id)? else {
            return Outcome::refused(E_DELETE_MISSING, MSG_DELETE_MISSING);
        };
        if crate::store::is_native_folder(&e.path) {
            return Outcome::native();
        }
        let mut c = store.plan_delete(&guard, &e.path, now, Origin::Device, Some(serial.into()))?;
        crate::pages::plan_page_delete(&store, &guard, &mut c);
        store.commit(&guard, c, &st.journal)?;
        Ok(Outcome::Done(json!({"metadata": metadata_value(&e)})))
    })
    .await
}

/// The parsed request of `move_v3` and `copy_v3`.
struct Transfer {
    id: IdArg,
    to_path: Option<String>,
    autorename: bool,
}

impl Transfer {
    fn of(body: &Value) -> Transfer {
        Transfer {
            id: id_arg(body, "id"),
            to_path: body
                .get("to_path")
                .and_then(Value::as_str)
                .map(str::to_string),
            autorename: flag_arg(body, "autorename"),
        }
    }
}

/// The checked target of a move or copy.
enum Target {
    Path(WirePath),
    Refused(Outcome),
}

/// The target of moving or copying `source` to `to_path` (the full path with the name):
/// `E0324` for a new name over 255 bytes, `E0322` for an unusable path, a blocked way or a taken
/// name (without `autorename`) or a folder on the way that matches two entries without case,
/// `E0358` for a folder into itself. Folders on the way take their stored spelling; the last
/// name stays as sent, so a case variant of another entry is taken. `in_place` names the
/// source when the target folder is its own folder and the source itself does not take the
/// name (a move, which may rename in place); then the source's own path is its target.
fn target_of(
    state: &AppState,
    store: &Store,
    guard: &WriteGuard,
    source: &Entry,
    to_path: &str,
    autorename: bool,
    in_place: bool,
) -> crate::Result<Target> {
    let taken = || {
        Ok(Target::Refused(Outcome::Refused(
            E_NAME_TAKEN,
            MSG_NAME_TAKEN,
            None,
        )))
    };
    let Ok(to) = WirePath::parse_named(to_path) else {
        return taken();
    };
    let (Some(parent), Some(name)) = (to.parent(), to.name()) else {
        return taken();
    };
    let Some(parent) = stored(store, guard, &parent)? else {
        return Ok(Target::Refused(Outcome::ambiguous()?));
    };
    if parent.new_name_too_long() {
        return Ok(Target::Refused(Outcome::too_long()?));
    }
    let parent = parent.path;
    let to = parent.join(name)?;
    if source.tag == Kind::Folder && to != source.path && to.starts_with(&source.path) {
        return Ok(Target::Refused(Outcome::Refused(
            E_INTO_ITSELF,
            MSG_INTO_ITSELF,
            None,
        )));
    }
    if in_place && to == source.path {
        return Ok(Target::Path(to));
    }
    scan_way(state, store, guard, &to)?;
    if !way_is_open(store, guard, &parent)? {
        return taken();
    }
    let own = (in_place && source.path.parent().as_ref() == Some(&parent))
        .then_some(source.name.as_str());
    match pick_name(store, guard, &parent, name, source.tag, autorename, own)? {
        // A name that is new on disk; the source's own name in any case is not (the file
        // system took it), so a case-only rename of a long name keeps working.
        Some(name)
            if name.len() > MAX_NAME_BYTES
                && crate::store::fold(&name) != crate::store::fold(&source.name) =>
        {
            Ok(Target::Refused(Outcome::too_long()?))
        }
        Some(name) => Ok(Target::Path(parent.join(&name)?)),
        None => taken(),
    }
}

/// `POST /api/file/3/files/move_v3` `{id, to_path (the full new path with the name),
/// autorename}`: moves or renames the file or folder, keeps its id and answers `entriesVO`.
/// Missing folders on the way are created (`folder.created`), then `file.moved` /
/// `folder.moved`, then `folder.deleted` for each source folder the move leaves empty (never a
/// native folder). Folders on the way in another case are those folders. A rename that only
/// changes case is allowed. A taken target (also one the registry does not know yet, or a case
/// variant of another entry) answers `E0322`, or with `autorename` becomes `name(1)`, ...; a
/// new name over 255 bytes `E0324`; an unknown source `E0320`; a folder into itself `E0358`; a
/// native folder is refused. Needs the sync lock (`E0079`).
pub async fn move_v3(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = gated_request!(state, headers, bytes);
    let t = Transfer::of(&req.body);
    let now = state.now_ms();
    mutate(&state, &req, move |st, slug, serial| {
        let IdArg::Id(id) = t.id else {
            return Outcome::refused(E_MOVE_MISSING, MSG_MOVE_MISSING);
        };
        let store = st.store(slug)?;
        let guard = store.lock_recovered(&st.journal)?;
        gate_locked(st, &store)?;
        let Some(src) = source_of(st, &store, &guard, id)? else {
            return Outcome::refused(E_MOVE_MISSING, MSG_MOVE_MISSING);
        };
        let Some(to_path) = t.to_path else {
            return Outcome::taken();
        };
        if crate::store::is_native_folder(&src.path) {
            return Outcome::native();
        }
        let to = match target_of(st, &store, &guard, &src, &to_path, t.autorename, true)? {
            Target::Path(p) => p,
            Target::Refused(o) => return Ok(o),
        };
        if to == src.path {
            // The same path: nothing to do.
            return Ok(Outcome::Done(json!({"entriesVO": entry_value(Some(&src))})));
        }
        let mut c = store.plan_move_at(
            &guard,
            &src.path,
            &to,
            now,
            Origin::Device,
            Some(serial.into()),
        )?;
        crate::pages::plan_page_move(&store, &guard, &mut c);
        store.commit(&guard, c, &st.journal)?;
        let e = entry_after(&store, &guard, &to)?;
        Ok(Outcome::Done(json!({"entriesVO": entry_value(Some(&e))})))
    })
    .await
}

/// `POST /api/file/3/files/copy_v3` `{id, to_path (the full path of the copy), autorename}`:
/// copies the file or folder and answers `entriesVO` of the copy, which gets a new id (in a
/// folder copy every descendant does). Missing folders on the way are created
/// (`folder.created`). Events: `file.copied` (`sourceId`) per file, with the page events of a
/// note, and `folder.created` per folder. Folders on the way in another case are those folders.
/// A taken target answers `E0322`, or with `autorename` becomes `name(1)`, ...; a new name over
/// 255 bytes `E0324`; an unknown source `E0308`; a folder into itself `E0358`. Needs the sync lock
/// (`E0079`). A folder copy is a series of commits (one per folder and file).
pub async fn copy_v3(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = gated_request!(state, headers, bytes);
    let t = Transfer::of(&req.body);
    mutate(&state, &req, move |st, slug, serial| {
        let IdArg::Id(id) = t.id else {
            return Outcome::refused(E_COPY_MISSING, MSG_COPY_MISSING);
        };
        let store = st.store(slug)?;
        let guard = store.lock_recovered(&st.journal)?;
        gate_locked(st, &store)?;
        let Some(src) = source_of(st, &store, &guard, id)? else {
            return Outcome::refused(E_COPY_MISSING, MSG_COPY_MISSING);
        };
        let Some(to_path) = t.to_path else {
            return Outcome::taken();
        };
        let to = match target_of(st, &store, &guard, &src, &to_path, t.autorename, false)? {
            Target::Path(p) => p,
            Target::Refused(o) => return Ok(o),
        };
        let device = Some(serial.to_string());
        let copy_file = |from: &WirePath, to: &WirePath| -> crate::Result<()> {
            let mut c = store.plan_copy(&guard, from, to, Origin::Device, device.clone())?;
            crate::pages::plan_page_events(&store, &guard, &mut c);
            store.commit(&guard, c, &st.journal)?;
            Ok(())
        };
        match src.tag {
            Kind::File => copy_file(&src.path, &to)?,
            Kind::Folder => {
                // What to copy is fixed before anything is created.
                let inside = store.list(&guard, &src.path, true)?;
                create_folders(st, &store, &guard, &to, serial)?;
                for e in inside {
                    let Some(target) = e.path.rebase(&src.path, &to) else {
                        continue;
                    };
                    match e.tag {
                        Kind::Folder => create_folders(st, &store, &guard, &target, serial)?,
                        Kind::File => copy_file(&e.path, &target)?,
                    }
                }
            }
        }
        let e = entry_after(&store, &guard, &to)?;
        // The listing hashed a copied file: its row goes to disk before the answer, as for an
        // upload.
        store.flush_cache(&guard)?;
        Ok(Outcome::Done(json!({"entriesVO": entry_value(Some(&e))})))
    })
    .await
}
