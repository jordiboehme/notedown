//! The digest routes of accept mode (`docs/sync-protocol.md` 5.3, 5.4, 5.7): items, groups and
//! tags of the device's Digest app, and the two routes that sign the transfers of the
//! handwritten comments (`upload/apply/summary`, `download/summary`, 5.5). The bytes move on
//! `/api/oss/upload/summary` and `/api/oss/download/summary` ([`super::oss`]).
//!
//! The gate of every call: the device token (`E0712`), the body's `equipmentNo` when present
//! (`E0312`), the protect gate of the files (`E0079`, flag `protect_refused`) and then, with
//! `protect = true`, the `app_data_imported` marker of the account (`E0079`, flag
//! `app_data_refused`). Both markers are read again under the store lock. The device calls these
//! routes in parallel with the file sync, so they never take, revive or extend the sync lock.
//!
//! Ids: a new digest or group gets [`AppData::next_digest_id`], read under the same store lock as
//! its write, so two adds never share an id. Ids stay exact integers (never `f64`). An
//! `add/summary` whose `metadata.unique_identifier` matches a live digest updates that digest
//! and answers its id. An `update/summary` (or `update/summary/group`) for an id without a live
//! record creates it under that id (flags `digest_adopted`, `digest_group_adopted`): the device
//! sends back what the server lacks (5.7).
//!
//! Queries return live records only, newest `creationTime` first (ties: larger id first).
//! `page` is 1-based (`page <= 0` reads as 1), `size <= 0` means all; `currentPage` and
//! `pageSize` are answered as sent. Fields the device sends that the records do not name are
//! stored and sent back unchanged; `equipmentNo` is not stored. A missing or malformed required
//! field answers `422`. A group stores its server times `createTime`/`updateTime` in `extra`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use super::envelope::{Reply, field, parse_body};
use super::files::{
    E_DEVICE_MISMATCH, E_NO_FILE, E_UPLOAD, FLAG_BAD_NAME, IdArg, MSG_DEVICE_MISMATCH, MSG_NO_FILE,
    MSG_UPLOAD, blocking, gate_locked, id_arg, protect_gate,
};
use super::oss::MARK_TARGET;
use crate::appdata::{AppData, Digest, DigestGroup, DigestTag, NO, YES, valid_mark_name};
use crate::auth::Account;
use crate::journal::Origin;
use crate::server::AppState;
use crate::store::WriteGuard;
use crate::sync::urls::{base_url, oss_url};
use crate::sync::{Purpose, SignedTarget};

/// A tag with this name exists.
pub const E_TAG_EXISTS: &str = "E0335";
/// The tag does not exist.
pub const E_TAG_MISSING: &str = "E0336";
/// A live group with this unique identifier exists.
pub const E_GROUP_EXISTS: &str = "E0338";
/// The digest group does not exist.
pub const E_GROUP_MISSING: &str = "E0339";
/// The digest does not exist.
pub const E_DIGEST_MISSING: &str = "E0340";

pub const MSG_TAG_EXISTS: &str = "The tag already exists";
pub const MSG_TAG_MISSING: &str = "The tag does not exist";
pub const MSG_GROUP_EXISTS: &str = "The unique identifier already exists";
pub const MSG_GROUP_MISSING: &str = "The digest group does not exist";
pub const MSG_DIGEST_MISSING: &str = "The digest does not exist";

/// Log flag: a digest or to-do call refused because the app data is not imported yet (protect
/// on, no `app_data_imported` marker).
pub const FLAG_APP_DATA_REFUSED: &str = "app_data_refused";
/// Log flag: `update/summary` for an id without a live digest created (or revived) it.
pub const FLAG_DIGEST_ADOPTED: &str = "digest_adopted";
/// Log flag: `update/summary/group` for an id without a live group created (or revived) it.
pub const FLAG_DIGEST_GROUP_ADOPTED: &str = "digest_group_adopted";

/// Request fields that are not record data: never stored.
const NOT_STORED: &[&str] = &["equipmentNo"];
/// Fields of the group answer that the server sets; not stored from a request. The server times
/// are stored in the group's `extra` by the routes.
const GROUP_SERVER_FIELDS: &[&str] = &["userId", "isSummaryGroup", "createTime", "updateTime"];

/// A checked app data request: the token's account and device and the parsed body.
#[derive(Debug)]
pub struct AppRequest {
    pub slug: String,
    pub serial: String,
    /// The account's user id (`userId` of the answers).
    pub user_id: u64,
    /// The token's generation (signed into transfer URLs).
    pub generation: u64,
    pub body: Value,
}

impl AppRequest {
    /// `422` for a missing or malformed field, logged with the device.
    pub(crate) fn bad_params(&self, what: &str) -> Reply {
        Reply::bad_params(what).device(&self.serial)
    }
}

/// Whether the app data routes are open for `account`: protect off, or the app data imported.
pub fn app_data_open(state: &AppState, account: &Account) -> bool {
    !state.config.protect || account.app_data_imported.is_some()
}

/// The answer of the closed app data gate: `E0079` with flag `app_data_refused`.
pub fn app_data_refused(serial: &str) -> Reply {
    Reply::sync_busy()
        .flag(FLAG_APP_DATA_REFUSED)
        .device(serial)
}

/// Authenticate the token, parse the body, check `equipmentNo` against the token's device
/// (`E0312`; a body without it is accepted), then the protect gate and the app data gate. No
/// call on the sync lock: digest and to-do calls run beside the file sync.
pub fn app_request(
    state: &AppState,
    headers: &HeaderMap,
    bytes: &[u8],
) -> Result<AppRequest, Reply> {
    let (a, _) = super::server::authenticate(state, headers)?;
    let serial = a.claims.equipment_no;
    let generation = a.claims.generation;
    let body = parse_body(bytes).map_err(|r| r.device(&serial))?;
    if let Some(sent) = field(&body, "equipmentNo")
        && sent != serial
    {
        return Err(Reply::error(E_DEVICE_MISMATCH, MSG_DEVICE_MISMATCH).device(&serial));
    }
    if let Some(reply) = protect_gate(state, &a.account, &serial) {
        return Err(reply);
    }
    if !app_data_open(state, &a.account) {
        return Err(app_data_refused(&serial));
    }
    Ok(AppRequest {
        slug: a.account.slug.clone(),
        serial,
        user_id: a.account.user_id,
        generation,
        body,
    })
}

/// What a route's store work sees: the app data under the store lock, and who asks.
pub struct Ctx<'a> {
    pub state: &'a AppState,
    pub app: AppData<'a>,
    pub guard: &'a WriteGuard,
    pub serial: &'a str,
    pub user_id: u64,
}

impl Ctx<'_> {
    pub(crate) fn ok(&self, fields: Value) -> Reply {
        Reply::ok(fields).device(self.serial)
    }

    pub(crate) fn error(&self, code: &str, msg: &str) -> Reply {
        Reply::error(code, msg).device(self.serial)
    }

    fn put_digest(&self, d: &Digest) -> crate::Result<()> {
        self.app.put_digest(
            self.guard,
            d,
            Origin::Device,
            Some(self.serial),
            &self.state.journal,
        )
    }

    fn put_group(&self, g: &DigestGroup) -> crate::Result<()> {
        self.app.put_digest_group(
            self.guard,
            g,
            Origin::Device,
            Some(self.serial),
            &self.state.journal,
        )
    }
}

/// Run `f` in a blocking task under the account's recovered store lock, after both gates were
/// read again there (an `import --replace` may have cleared a marker meanwhile). Store errors
/// become the internal-error envelope (a damaged store `E0079`).
pub async fn with_app_data<F>(state: &Arc<AppState>, req: &AppRequest, f: F) -> Reply
where
    F: FnOnce(Ctx<'_>) -> crate::Result<Reply> + Send + 'static,
{
    on_app_data(state, req, move |c| f(c).map(Ok))
        .await
        .unwrap_or_else(|reply| reply)
}

/// [`with_app_data`] for work that hands back a value (an opened file): `Err` is the answer of
/// a closed gate, of a store error, or the refusal `f` returns.
pub async fn on_app_data<T, F>(state: &Arc<AppState>, req: &AppRequest, f: F) -> Result<T, Reply>
where
    T: Send + 'static,
    F: FnOnce(Ctx<'_>) -> crate::Result<Result<T, Reply>> + Send + 'static,
{
    let (st, slug, serial, user_id) = (
        state.clone(),
        req.slug.clone(),
        req.serial.clone(),
        req.user_id,
    );
    let done = blocking(&req.serial, move || {
        let store = st.store(&slug)?;
        let guard = store.lock_recovered(&st.journal)?;
        gate_locked(&st, store.slug())?;
        let open = st
            .auth
            .accounts()
            .get(store.slug())?
            .is_some_and(|a| app_data_open(&st, &a));
        if !open {
            return Ok(Err(app_data_refused(&serial)));
        }
        f(Ctx {
            state: &st,
            app: AppData::new(&store),
            guard: &guard,
            serial: &serial,
            user_id,
        })
    })
    .await;
    done.and_then(|r| r)
}

// ------------------------------------------------------------------ request parsing

/// The body as a record: an object without `id` and the fields in `drop`; a wrong type of a
/// named field is a bad parameter.
fn record_from<T: DeserializeOwned>(req: &AppRequest, drop: &[&str]) -> Result<T, Reply> {
    let Value::Object(mut m) = req.body.clone() else {
        return Err(req.bad_params("body is no JSON object"));
    };
    for key in NOT_STORED.iter().chain(drop).chain(&["id"]) {
        m.remove(*key);
    }
    serde_json::from_value(Value::Object(m)).map_err(|e| {
        req.bad_params("a field has the wrong type")
            .message(&e.to_string())
    })
}

/// The required `id`: a positive integer, as a number or a decimal string.
fn required_id(req: &AppRequest) -> Result<u64, Reply> {
    match id_arg(&req.body, "id") {
        IdArg::Id(id) if id > 0 => Ok(id),
        _ => Err(req.bad_params("id")),
    }
}

/// An integer argument: absent or `null` is 0; a number or a decimal string.
fn int_arg(body: &Value, key: &str) -> Option<i64> {
    match body.get(key) {
        None | Some(Value::Null) => Some(0),
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) if s.trim().is_empty() => Some(0),
        Some(Value::String(s)) => s.trim().parse().ok(),
        Some(_) => None,
    }
}

/// The `ids` argument: absent or `null` is empty; else an array of ids (numbers or strings).
fn ids_arg(body: &Value) -> Option<Vec<u64>> {
    match body.get("ids") {
        None | Some(Value::Null) => Some(Vec::new()),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| match v {
                Value::Number(n) => n.as_u64(),
                Value::String(s) => s.trim().parse().ok(),
                _ => None,
            })
            .collect(),
        Some(_) => None,
    }
}

/// The arguments of the digest and group queries.
#[derive(Debug)]
struct Query {
    page: i64,
    size: i64,
    /// Only items of this group (empty or absent: all).
    parent: Option<String>,
    /// Only these ids (empty: all).
    ids: Vec<u64>,
}

fn query_arg(req: &AppRequest) -> Result<Query, Reply> {
    let page = int_arg(&req.body, "page").ok_or_else(|| req.bad_params("page"))?;
    let size = int_arg(&req.body, "size").ok_or_else(|| req.bad_params("size"))?;
    let parent = match req.body.get("parentUniqueIdentifier") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(req.bad_params("parentUniqueIdentifier")),
    };
    let ids = ids_arg(&req.body).ok_or_else(|| req.bad_params("ids"))?;
    Ok(Query {
        page,
        size,
        parent,
        ids,
    })
}

impl Query {
    fn admits(&self, d: &Digest) -> bool {
        d.is_live()
            && self
                .parent
                .as_ref()
                .is_none_or(|p| d.parent_unique_identifier.as_ref() == Some(p))
            && (self.ids.is_empty() || self.ids.contains(&d.id))
    }

    /// One page of `rows` (already in answer order) and the paging fields (D7).
    fn page<T>(&self, rows: Vec<T>) -> (Vec<T>, Map<String, Value>) {
        let total = rows.len() as u64;
        let (rows, pages) = if self.size <= 0 {
            (rows, 1)
        } else {
            let size = self.size as u64;
            let start = (self.page.max(1) as u64 - 1).saturating_mul(size);
            let rows = rows
                .into_iter()
                .skip(usize::try_from(start).unwrap_or(usize::MAX))
                .take(usize::try_from(size).unwrap_or(usize::MAX))
                .collect();
            (rows, total.div_ceil(size).max(1))
        };
        let mut m = Map::new();
        m.insert("totalRecords".into(), json!(total));
        m.insert("totalPages".into(), json!(pages));
        m.insert("currentPage".into(), json!(self.page));
        m.insert("pageSize".into(), json!(self.size));
        (rows, m)
    }
}

/// Newest `creationTime` first, ties by the larger id; a record without one comes last.
fn order_key(creation: Option<i64>, id: u64) -> std::cmp::Reverse<(i64, u64)> {
    std::cmp::Reverse((creation.unwrap_or(i64::MIN), id))
}

// ------------------------------------------------------------------ answers

/// A digest as the device gets it (5.1), with the account's `userId`.
fn digest_view(d: &Digest, user_id: u64) -> Value {
    let mut w = d.wire();
    w.user_id = user_id;
    serde_json::to_value(w).unwrap_or(Value::Null)
}

/// `metadata` parsed (numbers stay integers); `{}` when it is no JSON object.
fn metadata_map(d: &Digest) -> Value {
    d.metadata
        .as_deref()
        .and_then(|m| serde_json::from_str::<Value>(m).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

/// One entry of `summaryInfoVOList` (note `handwriteMd5` with lowercase `d5` here).
fn hash_view(d: &Digest, user_id: u64) -> Value {
    json!({
        "id": d.id,
        "userId": user_id,
        "md5Hash": d.md5_hash,
        "handwriteMd5": d.handwrite_md5,
        "commentHandwriteName": d.comment_handwrite_name,
        "lastModifiedTime": d.last_modified_time,
        "metadataMap": metadata_map(d),
    })
}

/// A server time of a group, ms: kept in the record's `extra` under its wire name
/// (`DigestGroup` has no field for it).
fn group_time(g: &DigestGroup, key: &str) -> Option<u64> {
    g.extra.get(key).and_then(Value::as_u64)
}

/// Set the server times of a group a device writes: `createTime` from the stored record (now
/// for a new one), `updateTime` now.
fn stamp_group(group: &mut DigestGroup, old: Option<&DigestGroup>, now: u64) {
    let created = old.and_then(|o| group_time(o, "createTime")).unwrap_or(now);
    group.extra.insert("createTime".into(), json!(created));
    group.extra.insert("updateTime".into(), json!(now));
}

/// A group as the device gets it: its record plus `userId`, `isSummaryGroup: "Y"` and the
/// server times `createTime`/`updateTime`. An imported group has none; it answers its device
/// times (`creationTime`, `lastModifiedTime`), or 0.
fn group_view(g: &DigestGroup, user_id: u64) -> Value {
    let mut v = serde_json::to_value(g).unwrap_or_else(|_| json!({}));
    if let Value::Object(m) = &mut v {
        let device_created = g.creation_time.unwrap_or(0).max(0) as u64;
        let device_updated = g
            .last_modified_time
            .map_or(device_created, |t| t.max(0) as u64);
        let created = group_time(g, "createTime").unwrap_or(device_created);
        let updated = group_time(g, "updateTime").unwrap_or(device_updated);
        m.insert("userId".into(), json!(user_id));
        m.insert("isSummaryGroup".into(), json!(YES));
        m.insert("createTime".into(), json!(created));
        m.insert("updateTime".into(), json!(updated));
    }
    v
}

/// The live digests that `q` admits, in answer order.
fn live_digests(c: &Ctx<'_>, q: &Query) -> crate::Result<Vec<Digest>> {
    let mut rows: Vec<Digest> = c
        .app
        .digests(c.guard)?
        .into_iter()
        .filter(|d| q.admits(d))
        .collect();
    rows.sort_by_key(|d| order_key(d.creation_time, d.id));
    Ok(rows)
}

/// Server-set fields of a digest from a device.
fn own(d: &mut Digest, id: u64, user_id: u64) {
    d.id = id;
    d.user_id = user_id;
    d.is_summary_group = NO.into();
    d.is_deleted = NO.into();
}

// ------------------------------------------------------------------ items

/// `POST /api/file/add/summary`: a new digest, `{id}`. A live digest with the same
/// `metadata.unique_identifier` is replaced instead and keeps its id (D3).
pub async fn add_summary(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let mut digest: Digest = match record_from(&req, &[]) {
        Ok(d) => d,
        Err(r) => return r,
    };
    with_app_data(&state, &req, move |c| {
        let known = match digest.identity() {
            Some(identity) => c
                .app
                .digests(c.guard)?
                .into_iter()
                .find(|d| d.is_live() && d.identity().as_deref() == Some(identity.as_str())),
            None => None,
        };
        let id = match known {
            Some(d) => d.id,
            None => c.app.next_digest_id(c.guard)?,
        };
        own(&mut digest, id, c.user_id);
        c.put_digest(&digest)?;
        Ok(c.ok(json!({"id": id})))
    })
    .await
}

/// `PUT /api/file/update/summary`: replace the digest `id` with the body. `creationTime`,
/// `fileId` and `uniqueIdentifier` stay as stored (the update carries none). An id without a
/// live digest is created (or revived) under that id, flag `digest_adopted`.
pub async fn update_summary(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let id = match required_id(&req) {
        Ok(id) => id,
        Err(r) => return r,
    };
    let mut digest: Digest = match record_from(&req, &[]) {
        Ok(d) => d,
        Err(r) => return r,
    };
    with_app_data(&state, &req, move |c| {
        let old = c.app.digest(c.guard, id)?;
        let adopted = !old.as_ref().is_some_and(Digest::is_live);
        if let Some(old) = old {
            digest.creation_time = old.creation_time.or(digest.creation_time);
            digest.unique_identifier = old.unique_identifier.or(digest.unique_identifier);
            if !old.file_id.is_null() {
                digest.file_id = old.file_id;
            }
        }
        own(&mut digest, id, c.user_id);
        c.put_digest(&digest)?;
        let reply = c.ok(json!({}));
        Ok(if adopted {
            reply.flag(FLAG_DIGEST_ADOPTED)
        } else {
            reply
        })
    })
    .await
}

/// `DELETE /api/file/delete/summary` `{id}`: soft delete (`E0340` without a live digest).
pub async fn delete_summary(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let id = match required_id(&req) {
        Ok(id) => id,
        Err(r) => return r,
    };
    with_app_data(&state, &req, move |c| {
        let deleted = c.app.delete_digest(
            c.guard,
            id,
            Origin::Device,
            Some(c.serial),
            &c.state.journal,
        )?;
        Ok(if deleted {
            c.ok(json!({}))
        } else {
            c.error(E_DIGEST_MISSING, MSG_DIGEST_MISSING)
        })
    })
    .await
}

/// `POST /api/file/query/summary` `{page, size, parentUniqueIdentifier, ids}`: the paging
/// fields and `summaryDOList`.
pub async fn query_summary(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let q = match query_arg(&req) {
        Ok(q) => q,
        Err(r) => return r,
    };
    with_app_data(&state, &req, move |c| {
        let (rows, mut fields) = q.page(live_digests(&c, &q)?);
        let list = rows.iter().map(|d| digest_view(d, c.user_id)).collect();
        fields.insert("summaryDOList".into(), Value::Array(list));
        Ok(c.ok(Value::Object(fields)))
    })
    .await
}

/// `POST /api/file/query/summary/hash`: as `query/summary`, but `summaryInfoVOList` of
/// `{id, userId, md5Hash, handwriteMd5, commentHandwriteName, lastModifiedTime, metadataMap}`.
pub async fn query_summary_hash(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let q = match query_arg(&req) {
        Ok(q) => q,
        Err(r) => return r,
    };
    with_app_data(&state, &req, move |c| {
        let (rows, mut fields) = q.page(live_digests(&c, &q)?);
        let list = rows.iter().map(|d| hash_view(d, c.user_id)).collect();
        fields.insert("summaryInfoVOList".into(), Value::Array(list));
        Ok(c.ok(Value::Object(fields)))
    })
    .await
}

/// `POST /api/file/query/summary/id` `{ids}`: `summaryDOList` of the live digests with these
/// ids (none for an empty list).
pub async fn query_summary_id(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let Some(ids) = ids_arg(&req.body) else {
        return req.bad_params("ids");
    };
    with_app_data(&state, &req, move |c| {
        let list = if ids.is_empty() {
            Vec::new()
        } else {
            let q = Query {
                page: 0,
                size: 0,
                parent: None,
                ids,
            };
            live_digests(&c, &q)?
                .iter()
                .map(|d| digest_view(d, c.user_id))
                .collect()
        };
        Ok(c.ok(json!({"summaryDOList": list})))
    })
    .await
}

// ------------------------------------------------------------------ groups

/// `POST /api/file/add/summary/group` (`md5Hash` required): a new group, `{id}`. `E0338` when a
/// live group has the same `uniqueIdentifier`.
pub async fn add_group(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let mut group: DigestGroup = match record_from(&req, GROUP_SERVER_FIELDS) {
        Ok(g) => g,
        Err(r) => return r,
    };
    if group.md5_hash.as_deref().is_none_or(str::is_empty) {
        return req.bad_params("md5Hash");
    }
    with_app_data(&state, &req, move |c| {
        if let Some(uid) = group.unique_identifier.as_deref().filter(|u| !u.is_empty()) {
            let taken = c
                .app
                .digest_groups(c.guard)?
                .iter()
                .any(|g| g.is_live() && g.unique_identifier.as_deref() == Some(uid));
            if taken {
                return Ok(c.error(E_GROUP_EXISTS, MSG_GROUP_EXISTS));
            }
        }
        group.id = c.app.next_digest_id(c.guard)?;
        group.is_deleted = NO.into();
        stamp_group(&mut group, None, c.state.journal.now_ms());
        c.put_group(&group)?;
        Ok(c.ok(json!({"id": group.id})))
    })
    .await
}

/// `PUT /api/file/update/summary/group`: replace the group `id` with the body (`creationTime`
/// stays as stored). An id without a live group is created (or revived) under that id, flag
/// `digest_group_adopted`.
pub async fn update_group(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let id = match required_id(&req) {
        Ok(id) => id,
        Err(r) => return r,
    };
    let mut group: DigestGroup = match record_from(&req, GROUP_SERVER_FIELDS) {
        Ok(g) => g,
        Err(r) => return r,
    };
    with_app_data(&state, &req, move |c| {
        let old = c.app.digest_group(c.guard, id)?;
        let adopted = !old.as_ref().is_some_and(DigestGroup::is_live);
        stamp_group(&mut group, old.as_ref(), c.state.journal.now_ms());
        if let Some(old) = old {
            group.creation_time = old.creation_time.or(group.creation_time);
        }
        group.id = id;
        group.is_deleted = NO.into();
        c.put_group(&group)?;
        let reply = c.ok(json!({}));
        Ok(if adopted {
            reply.flag(FLAG_DIGEST_GROUP_ADOPTED)
        } else {
            reply
        })
    })
    .await
}

/// `DELETE /api/file/delete/summary/group` `{id}`: soft-delete the group's live items
/// (`parentUniqueIdentifier` = its `uniqueIdentifier`, when it has one), then the group.
/// `E0339` without a live group.
pub async fn delete_group(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let id = match required_id(&req) {
        Ok(id) => id,
        Err(r) => return r,
    };
    with_app_data(&state, &req, move |c| {
        let Some(group) = c
            .app
            .digest_group(c.guard, id)?
            .filter(DigestGroup::is_live)
        else {
            return Ok(c.error(E_GROUP_MISSING, MSG_GROUP_MISSING));
        };
        // Items first: a crash in between leaves the group live, so the device's retry
        // finishes the cascade.
        if let Some(uid) = group.unique_identifier.as_deref().filter(|u| !u.is_empty()) {
            for d in c.app.digests(c.guard)? {
                if d.is_live() && d.parent_unique_identifier.as_deref() == Some(uid) {
                    c.app.delete_digest(
                        c.guard,
                        d.id,
                        Origin::Device,
                        Some(c.serial),
                        &c.state.journal,
                    )?;
                }
            }
        }
        c.app.delete_digest_group(
            c.guard,
            id,
            Origin::Device,
            Some(c.serial),
            &c.state.journal,
        )?;
        Ok(c.ok(json!({})))
    })
    .await
}

/// `POST /api/file/query/summary/group` `{page, size}` (the device sends `{"page":0,"size":0}`
/// for all): the paging fields and `summaryDOList` of the live groups.
pub async fn query_group(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let q = match query_arg(&req) {
        Ok(q) => q,
        Err(r) => return r,
    };
    with_app_data(&state, &req, move |c| {
        let mut rows: Vec<DigestGroup> = c
            .app
            .digest_groups(c.guard)?
            .into_iter()
            .filter(DigestGroup::is_live)
            .collect();
        rows.sort_by_key(|g| order_key(g.creation_time, g.id));
        let (rows, mut fields) = q.page(rows);
        let list = rows.iter().map(|g| group_view(g, c.user_id)).collect();
        fields.insert("summaryDOList".into(), Value::Array(list));
        Ok(c.ok(Value::Object(fields)))
    })
    .await
}

// ------------------------------------------------------------------ tags

/// A new tag's unique identifier: 32 hex characters.
fn new_tag_identifier() -> String {
    let mut b = [0u8; 16];
    rand::fill(&mut b[..]);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `POST /api/file/add/summary/tag` `{name}`: a new tag, `{id}` (`E0335` when the name exists).
/// The body's `uniqueIdentifier` is kept; without one the server makes one.
pub async fn add_tag(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let Some(name) = field(&req.body, "name") else {
        return req.bad_params("name");
    };
    let unique = field(&req.body, "uniqueIdentifier").unwrap_or_else(new_tag_identifier);
    with_app_data(&state, &req, move |c| {
        let mut tags = c.app.tags(c.guard)?;
        if tags.iter().any(|t| t.name == name) {
            return Ok(c.error(E_TAG_EXISTS, MSG_TAG_EXISTS));
        }
        let id = c.app.next_tag_id(c.guard)?;
        tags.push(DigestTag {
            id,
            name,
            unique_identifier: unique,
            created_at: c.state.journal.now_ms(),
        });
        c.app.put_tags(c.guard, &tags)?;
        Ok(c.ok(json!({"id": id})))
    })
    .await
}

/// `PUT /api/file/update/summary/tag` `{id, name}`: rename (`E0336` unknown tag, `E0335` the
/// name belongs to another tag).
pub async fn update_tag(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let id = match required_id(&req) {
        Ok(id) => id,
        Err(r) => return r,
    };
    let Some(name) = field(&req.body, "name") else {
        return req.bad_params("name");
    };
    with_app_data(&state, &req, move |c| {
        let mut tags = c.app.tags(c.guard)?;
        if !tags.iter().any(|t| t.id == id) {
            return Ok(c.error(E_TAG_MISSING, MSG_TAG_MISSING));
        }
        if tags.iter().any(|t| t.id != id && t.name == name) {
            return Ok(c.error(E_TAG_EXISTS, MSG_TAG_EXISTS));
        }
        for t in tags.iter_mut().filter(|t| t.id == id) {
            t.name = name.clone();
        }
        c.app.put_tags(c.guard, &tags)?;
        Ok(c.ok(json!({})))
    })
    .await
}

/// `DELETE /api/file/delete/summary/tag` `{id}`: remove the tag (`E0336` unknown tag).
pub async fn delete_tag(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let id = match required_id(&req) {
        Ok(id) => id,
        Err(r) => return r,
    };
    with_app_data(&state, &req, move |c| {
        let mut tags = c.app.tags(c.guard)?;
        let before = tags.len();
        tags.retain(|t| t.id != id);
        if tags.len() == before {
            return Ok(c.error(E_TAG_MISSING, MSG_TAG_MISSING));
        }
        c.app.put_tags(c.guard, &tags)?;
        Ok(c.ok(json!({})))
    })
    .await
}

/// `GET /api/file/query/summary/tag`: `summaryTagDOList` of
/// `{id, name, userId, uniqueIdentifier, createdAt}`.
pub async fn query_tag(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    with_app_data(&state, &req, move |c| {
        let list: Vec<Value> = c
            .app
            .tags(c.guard)?
            .iter()
            .map(|t| {
                json!({
                    "id": t.id,
                    "name": t.name,
                    "userId": c.user_id,
                    "uniqueIdentifier": t.unique_identifier,
                    "createdAt": t.created_at,
                })
            })
            .collect();
        Ok(c.ok(json!({"summaryTagDOList": list})))
    })
    .await
}

// ------------------------------------------------------------------ handwritten comments

/// The base URL of the signed transfer URLs: `external_base_url`, else the request's host.
fn transfer_base(state: &AppState, headers: &HeaderMap) -> String {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok());
    base_url(state.config.external_base_url.as_deref(), host)
}

/// `POST /api/file/upload/apply/summary` `{fileName, equipmentNo}` (5.4 step 3, 5.5):
/// `fullUploadUrl` (signed, `POST /api/oss/upload/summary`, valid 30 min), `partUploadUrl: ""`
/// and `innerName` = `fileName`. A name that is no plain `<name>.mark` of at most 64 bytes
/// answers `E0324` (flag `bad_name`). Nothing is written here.
pub async fn upload_apply_summary(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let Some(name) = field(&req.body, "fileName").filter(|n| valid_mark_name(n)) else {
        return Reply::error(E_UPLOAD, MSG_UPLOAD)
            .flag(FLAG_BAD_NAME)
            .device(&req.serial);
    };
    let target = SignedTarget {
        account: req.slug.clone(),
        device: req.serial.clone(),
        generation: req.generation,
        path: MARK_TARGET.into(),
        id: None,
        inner: Some(name.clone()),
        size: 0,
    }
    .encode();
    let signed = state
        .url_signer
        .sign("POST", &target, Purpose::Upload, 0, state.now_ms());
    let base = transfer_base(&state, &headers);
    Reply::ok(json!({
        "fullUploadUrl": oss_url(&base, "upload/summary", &target, &signed),
        "partUploadUrl": "",
        "innerName": name,
    }))
    .device(&req.serial)
}

/// `POST /api/file/download/summary` `{id}` (5.4 step 5): `url` (signed,
/// `GET /api/oss/download/summary`, valid 24 h) for the handwritten comment of the digest.
/// `E0340` without a live digest; `E0321` when it names no comment, a name that is no valid
/// `.mark` name, or one whose file is missing.
pub async fn download_summary(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let id = match required_id(&req) {
        Ok(id) => id,
        Err(r) => return r,
    };
    let base = transfer_base(&state, &headers);
    let (slug, generation) = (req.slug.clone(), req.generation);
    with_app_data(&state, &req, move |c| {
        let Some(digest) = c.app.digest(c.guard, id)?.filter(Digest::is_live) else {
            return Ok(c.error(E_DIGEST_MISSING, MSG_DIGEST_MISSING));
        };
        let present = match digest.comment_handwrite_name.as_deref() {
            Some(name) if valid_mark_name(name) => c.app.open_mark(c.guard, name)?.is_some(),
            _ => false,
        };
        if !present {
            return Ok(c.error(E_NO_FILE, MSG_NO_FILE));
        }
        let target = SignedTarget {
            account: slug,
            device: c.serial.to_string(),
            generation,
            path: MARK_TARGET.into(),
            id: Some(id),
            inner: None,
            size: 0,
        }
        .encode();
        let signed =
            c.state
                .url_signer
                .sign("GET", &target, Purpose::Download, 0, c.state.now_ms());
        Ok(c.ok(json!({"url": oss_url(&base, "download/summary", &target, &signed)})))
    })
    .await
}
