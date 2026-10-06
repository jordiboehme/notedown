//! The file sync session, listing, queries, download URLs, capacity and the upload apply and
//! finish steps of accept mode (`docs/sync-protocol.md` 3.4, 3.5). Uploads also need the
//! calling device to hold the live sync lock (`E0079`, flag `no_sync_lock`).
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
use crate::journal::Origin;
use crate::server::AppState;
use crate::store::{Entry, Kind, PutOptions, Store, StoreGuard, WirePath};
use crate::sync::staging;
use crate::sync::urls::{base_url, download_url, upload_url};
use crate::sync::{Purpose, SignedTarget, StartOutcome};

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

/// Log flag: `synchronous/start` refused by the protect gate (no import yet).
pub const FLAG_PROTECT_REFUSED: &str = "protect_refused";
/// Log flag: `synchronous/end` with `flag` `Y`.
pub const FLAG_SYNC_END_OK: &str = "sync_end_ok";
/// Log flag: `synchronous/end` with `flag` `N` (the device saw a failure).
pub const FLAG_SYNC_END_FAILED: &str = "sync_end_failed";
/// Log flag: `synchronous/end` with a missing or unknown `flag`.
pub const FLAG_SYNC_END_UNKNOWN: &str = "sync_end_unknown_flag";
/// Log flag: `synchronous/end` from a device that did not hold the lock.
pub const FLAG_SYNC_LOCK_NOT_HELD: &str = "sync_lock_not_held";

/// Log flag: a write (upload) from a device that does not hold the live sync lock (`E0079`).
pub const FLAG_NO_SYNC_LOCK: &str = "no_sync_lock";
/// Log flag: the staged bytes do not have the MD5 the device names.
pub const FLAG_HASH_MISMATCH: &str = "hash_mismatch";
/// Log flag: the staged bytes do not have the size the device names.
pub const FLAG_SIZE_MISMATCH: &str = "size_mismatch";
/// Log flag: `upload/finish` names an upload with no complete data (unknown, or parts missing).
pub const FLAG_UPLOAD_MISSING: &str = "upload_missing";
/// Log flag: a file or part above the size limit.
pub const FLAG_UPLOAD_TOO_LARGE: &str = "upload_too_large";
/// Log flag: a target path or name the store does not take.
pub const FLAG_BAD_NAME: &str = "bad_name";

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

/// `POST /api/file/2/files/synchronous/start`. Order: device check, the protect gate (accept
/// mode with `protect = true` refuses with `E0079` until the account has an `imported` marker),
/// then the lock (`E0078` while another device holds it). Answers `synType: true`.
pub async fn sync_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = device_request!(state, headers, bytes);
    if state.config.protect && req.account.imported.is_none() {
        return Reply::sync_busy()
            .flag(FLAG_PROTECT_REFUSED)
            .device(&req.serial);
    }
    match state
        .sync_locks
        .start(&req.slug, &req.serial, state.now_ms())
    {
        StartOutcome::Busy { .. } => req.error(E_SYNC_LOCKED, MSG_SYNC_LOCKED),
        StartOutcome::Taken | StartOutcome::Renewed => req.ok(json!({"synType": true})),
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

/// The listing of a folder (the root included). `None` when it is not a folder. The id or path
/// is resolved after `lock_recovered`, so an unfinished commit is repaired first.
fn list_folder_of(
    state: &AppState,
    slug: &str,
    folder: Folder,
    recursive: bool,
) -> crate::Result<Option<Vec<Entry>>> {
    let store = state.store(slug)?;
    let guard = store.lock_recovered(&state.journal)?;
    let path = match folder {
        Folder::Path(p) => p,
        Folder::Id(id) => match store.kind_and_path_of(id)? {
            Some((Kind::Folder, p)) => p,
            _ => return Ok(None),
        },
    };
    if !path.is_root() {
        match store.entry(&guard, &path)? {
            Some(e) if e.tag == Kind::Folder => {}
            _ => return Ok(None),
        }
    }
    store.list(&guard, &path, recursive).map(Some)
}

/// `POST /api/file/2/files/list_folder` `{path, recursive}`: `entries`. `""` or `/` is the
/// root. A path that is not a folder answers `E0081` (never an empty list, which the device
/// could read as "everything deleted").
pub async fn list_folder(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = device_request!(state, headers, bytes);
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
    match blocking(&req.serial, move || {
        list_folder_of(&st, &slug, Folder::Path(path), recursive)
    })
    .await
    {
        Ok(Some(entries)) => req.ok(json!({"entries": entries_value(&entries)})),
        Ok(None) => req.error(E_PATH_MISSING, MSG_PATH_MISSING),
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
    let req = device_request!(state, headers, bytes);
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
    let r = blocking(&req.serial, move || {
        list_folder_of(&st, &slug, folder, recursive)
    })
    .await;
    match r {
        Ok(Some(entries)) => req.ok(json!({"entries": entries_value(&entries)})),
        Ok(None) => req.error(E_PATH_MISSING, MSG_PATH_MISSING),
        Err(r) => r,
    }
}

/// The entry with `id`, if the id is known and something listable is at its path.
fn entry_by_id(state: &AppState, slug: &str, id: u64) -> crate::Result<Option<Entry>> {
    let store = state.store(slug)?;
    let guard = store.lock_recovered(&state.journal)?;
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
    let req = device_request!(state, headers, bytes);
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
/// (a missing, unparsable or hidden path and the root give null).
pub async fn query_by_path_v3(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = device_request!(state, headers, bytes);
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
        store.entry(&guard, &path)
    })
    .await;
    match r {
        Ok(e) => req.ok(json!({"entriesVO": entry_value(e.as_ref())})),
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
    let req = device_request!(state, headers, bytes);
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

/// Whether `serial` holds the live sync lock of `slug`. Every write needs it, so no write can
/// pass around `synchronous/start` and its protect gate.
pub fn holds_sync_lock(state: &AppState, slug: &str, serial: &str) -> bool {
    state.sync_locks.holder(slug, state.now_ms()).as_deref() == Some(serial)
}

/// How often [`keep_lock`] extends the sync lock (well inside its 30 s TTL).
const KEEP_LOCK_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

/// Await `work` (a long blocking step: a merge, hashing, a commit) while extending the device's
/// sync lock every 10 s, so the lock does not lapse inside one request.
pub async fn keep_lock<F: std::future::Future>(
    state: &Arc<AppState>,
    slug: &str,
    serial: &str,
    work: F,
) -> F::Output {
    let (st, slug, serial) = (state.clone(), slug.to_string(), serial.to_string());
    let keeper = tokio::spawn(async move {
        let mut every = tokio::time::interval(KEEP_LOCK_EVERY);
        loop {
            every.tick().await;
            st.sync_locks.touch(&slug, &serial, st.now_ms());
        }
    });
    let out = work.await;
    keeper.abort();
    out
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

/// The target of `upload/apply`: `path` is the full path including the name
/// (`/NOTE/Note/Work/x.note`). A `path` whose last segment is not `fileName` is taken as the
/// folder. `None` for a path or name the store cannot take.
pub fn apply_target(path: &str, file_name: &str) -> Option<WirePath> {
    let p = WirePath::parse(path).ok()?;
    let parent = if p.name() == Some(file_name) {
        p.parent()?
    } else {
        p
    };
    parent.join(file_name).ok()
}

/// The target of `upload/finish`: `path` is the parent folder with a trailing slash
/// (`/NOTE/Note/Work/`), so the target is parent + `fileName`, never `x.note/x.note`. A `path`
/// without the trailing slash that already ends in `fileName` and is no folder is taken as the
/// full path.
fn finish_target(
    store: &Store,
    guard: &StoreGuard,
    path: &str,
    file_name: &str,
) -> crate::Result<Option<WirePath>> {
    let Ok(parent) = WirePath::parse(path) else {
        return Ok(None);
    };
    let full_path_form = !path.trim_end().ends_with('/') && parent.name() == Some(file_name);
    let parent = if full_path_form
        && !store
            .entry(guard, &parent)?
            .is_some_and(|e| e.tag == Kind::Folder)
    {
        parent.parent().unwrap_or_default()
    } else {
        parent
    };
    Ok(parent.join(file_name).ok())
}

/// Why an upload target is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// `E0322`: a name collision, or a folder at the target.
    Taken,
    /// `E0324` with a log flag.
    Cannot(&'static str),
}

impl Refusal {
    fn reply(self, serial: &str) -> Reply {
        match self {
            Refusal::Taken => Reply::error(E_NAME_TAKEN, MSG_NAME_TAKEN).device(serial),
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

/// Check that `target` can take a file: no case or normalization collision on its way (`E0322`),
/// no dot-name and no file in place of a parent folder (`E0324`), no folder at the target
/// (`E0322`).
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
/// the signed upload URLs. Needs the sync lock (`E0079`). The name is checked first (`E0322`
/// on a collision, `E0324` on a dot-name). `innerName` is a fresh UUID; the URLs sign the
/// account, the device, its token generation, the parent folder, `innerName` and `size`.
pub async fn upload_apply(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = device_request!(state, headers, bytes);
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
    let Some(target) = apply_target(path, &file_name) else {
        return Refusal::Cannot(FLAG_BAD_NAME).reply(&req.serial);
    };
    let size = size_arg(&req.body, "size").unwrap_or(0);
    if size > state.file_max() {
        return Refusal::Cannot(FLAG_UPLOAD_TOO_LARGE).reply(&req.serial);
    }
    let (st, slug, t) = (state.clone(), req.slug.clone(), target.clone());
    let checked = blocking(&req.serial, move || {
        let store = st.store(&slug)?;
        let guard = store.lock_recovered(&st.journal)?;
        check_target(&store, &guard, &t)
    })
    .await;
    match checked {
        Ok(None) => {}
        Ok(Some(refusal)) => return refusal.reply(&req.serial),
        Err(r) => return r,
    }
    let inner = crate::journal::new_op_id();
    let signed_target = SignedTarget {
        account: req.slug.clone(),
        device: req.serial.clone(),
        generation: req.generation,
        path: target.parent().unwrap_or_default().display(),
        id: None,
        inner: Some(inner.clone()),
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
        "innerName": inner,
        "xamzDate": signed.timestamp.to_string(),
        "authorization": "",
        "fullUploadUrl": upload_url(&base, "upload", &signed_target, &signed),
        "partUploadUrl": upload_url(&base, "upload/part", &signed_target, &signed),
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
    let entry = staging::entry_dir(store.account_dir(), &a.inner)?;
    let data = entry.join(staging::DATA_FILE);
    let len = match std::fs::symlink_metadata(&data) {
        Ok(m) if m.is_file() => m.len(),
        // Unknown upload, or parts still missing: the entry stays for a retry of the part.
        _ => return cannot(FLAG_UPLOAD_MISSING),
    };
    if len != a.size {
        staging::remove_entry(&entry);
        return cannot(FLAG_SIZE_MISMATCH);
    }
    let md5 = crate::store::md5_file(&data).map_err(|e| Error::Io {
        path: data.clone(),
        source: e,
    })?;
    if !md5.eq_ignore_ascii_case(a.content_hash.trim()) {
        staging::remove_entry(&entry);
        return cannot(FLAG_HASH_MISMATCH);
    }
    let guard = store.lock_recovered(&state.journal)?;
    let Some(target) = finish_target(&store, &guard, &a.path, &a.file_name)? else {
        staging::remove_entry(&entry);
        return cannot(FLAG_BAD_NAME);
    };
    if let Some(refusal) = check_target(&store, &guard, &target)? {
        staging::remove_entry(&entry);
        return Ok(Finished::Refused(refusal));
    }
    let options = PutOptions {
        create_parents: true,
        recycle_at_ms: a.protect.then_some(a.now_ms),
    };
    let planned = store.plan_put_with(
        &guard,
        &target,
        &data,
        Origin::Device,
        Some(a.serial.clone()),
        options,
    );
    let mut commit = match planned {
        Ok(c) => c,
        Err(e) => match refusal_of(&e) {
            Some(refusal) => {
                staging::remove_entry(&entry);
                return Ok(Finished::Refused(refusal));
            }
            None => return Err(e),
        },
    };
    crate::pages::plan_page_events(&store, &guard, &mut commit);
    store.commit(&guard, commit, &state.journal)?;
    let listed = store
        .entry(&guard, &target)?
        .filter(|e| e.tag == Kind::File)
        .ok_or_else(|| Error::Invalid {
            what: "upload",
            message: format!("{target} is not a file after the commit"),
        })?;
    drop(guard);
    staging::remove_entry(&entry);
    Ok(Finished::Done(listed))
}

/// `POST /api/file/2/files/upload/finish` `{path (parent folder + "/"), fileName,
/// content_hash, innerName, size (string)}`: verifies MD5 and size of the staged data
/// (`E0324` and nothing kept on a mismatch), creates missing parent folders
/// (`folder.created`), writes the file (`file.created`, or `file.modified` with `prevMd5` and
/// the same id), journals the page diff of a note, all in one commit. In protect mode the
/// replaced bytes go to the recycle bin first. Needs the sync lock (`E0079`). Answers
/// `{path_display, id, size, name, content_hash}`.
pub async fn upload_finish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = device_request!(state, headers, bytes);
    if !holds_sync_lock(&state, &req.slug, &req.serial) {
        return no_sync_lock(&req.serial);
    }
    let text = |k: &str| req.body.get(k).and_then(Value::as_str).map(str::to_string);
    let Some(file_name) = text("fileName").filter(|n| !n.is_empty()) else {
        return Reply::bad_params("fileName").device(&req.serial);
    };
    let Some(inner) = text("innerName").filter(|i| staging::valid_inner_name(i)) else {
        return Refusal::Cannot(FLAG_UPLOAD_MISSING).reply(&req.serial);
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
    match r {
        Ok(Finished::Done(e)) => req.ok(json!({
            "path_display": e.path_display,
            "id": e.id.to_string(),
            "size": e.size,
            "name": e.name,
            "content_hash": e.content_hash,
        })),
        Ok(Finished::Refused(refusal)) => refusal.reply(&req.serial),
        Err(r) => r,
    }
}
