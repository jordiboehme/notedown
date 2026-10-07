//! The to-do routes of accept mode (`docs/sync-protocol.md` 6.1): task groups (lists), tasks and
//! the sort order of each group, as the device's To-Do app syncs them.
//!
//! The gate is the one of the digest routes ([`super::digests::app_request`],
//! [`with_app_data`]): token, `equipmentNo`, the protect gate and the `app_data_imported`
//! marker, never the sync lock (the device calls these routes beside the file sync).
//!
//! Ids: `taskId` and `taskListId` come from the device and are kept as sent; one that is no
//! plain file name (1 to 128 of `A-Z a-z 0-9 - _`) answers `422`, as does a field of the wrong
//! type. A record keeps the fields the device sends that it does not name and sends them back
//! unchanged; `equipmentNo` is not stored, `userId` of a group is the account's.
//!
//! Pulls: `schedule/task/all` first removes the tombstones older than 5 days. Without a
//! `nextSyncToken` (absent, `null`, `""` or 0) it answers the live tasks; with one, every task
//! the server wrote at or after that time, tombstones included (`isDeleted: "Y"`). "At or after"
//! (not "after"): a write in the same millisecond as the answered token comes again instead of
//! never. A token older than 5 days, in the future or no number answers `E0330` and the device
//! pulls all. `nextSyncToken` of the answer is the server time of the pull's first page, carried
//! in the page token, so a change made between page calls comes with the next pull. Tasks are
//! ordered by `taskId`, groups by `taskListId`; `maxResults` (default 20) pages them by an
//! opaque token, which only a page with more rows after it carries. Paging arguments (also
//! `nextIndexNumber` of the sort query) of an unexpected shape never answer `422`: they read as
//! the default or the first page.
//!
//! Deletes: a task delete leaves a tombstone (D5). A group delete turns every task of the group
//! into a tombstone (one journal batch), then removes its sort and the group; `group/clear` does
//! the same for the completed tasks only. A delete of something that is not there succeeds: the
//! device wants it gone, and an error would fail its to-do sync again and again.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use super::digests::{AppRequest, with_app_data};
use super::envelope::Reply;
use crate::appdata::{NO, TOMBSTONE_TTL_MS, Task, TaskGroup, TaskSort, YES, valid_task_key};
use crate::journal::Origin;
use crate::server::AppState;

/// The task group does not exist.
pub const E_GROUP_MISSING: &str = "E0328";
/// The task does not exist.
pub const E_TASK_MISSING: &str = "E0329";
/// The sync token is older than 5 days (or none the server made): the device pulls all.
pub const E_TOKEN_TOO_OLD: &str = "E0330";

pub const MSG_GROUP_MISSING: &str = "The task group does not exist";
pub const MSG_TASK_MISSING: &str = "The task does not exist";
pub const MSG_TOKEN_TOO_OLD: &str = "The sync token is too old, please sync all to-dos";

/// Page size when the device sends no `maxResults` (6.1).
pub const DEFAULT_MAX_RESULTS: u64 = 20;

/// Request fields that are not record data: never stored.
const NOT_STORED: &[&str] = &["equipmentNo"];
/// Fields of a group that the server sets.
const GROUP_SERVER_FIELDS: &[&str] = &["userId"];
/// The `status` of a done task.
const COMPLETED: &str = "completed";

// ------------------------------------------------------------------ request parsing

/// `value` as a record: an object without `equipmentNo` and the fields in `drop`; a wrong type
/// of a named field is a bad parameter.
fn record<T: DeserializeOwned>(req: &AppRequest, value: &Value, drop: &[&str]) -> Result<T, Reply> {
    let Value::Object(m) = value else {
        return Err(req.bad_params("no JSON object"));
    };
    let mut m = m.clone();
    for key in NOT_STORED.iter().chain(drop) {
        m.remove(*key);
    }
    serde_json::from_value(Value::Object(m)).map_err(|e| {
        req.bad_params("a field has the wrong type")
            .message(&e.to_string())
    })
}

/// A device key (`taskId`, `taskListId`) that may be a file name.
fn key(req: &AppRequest, value: &str, what: &str) -> Result<String, Reply> {
    if valid_task_key(value) {
        Ok(value.to_string())
    } else {
        Err(req.bad_params(what))
    }
}

/// A task from a request object; its `taskId` must be a valid key.
fn task_from(req: &AppRequest, value: &Value) -> Result<Task, Reply> {
    let task: Task = record(req, value, &[])?;
    key(req, &task.task_id, "taskId")?;
    Ok(task)
}

/// The body's `taskListId`, a string or an integer as the records take it; empty when absent.
fn list_id_arg(req: &AppRequest) -> Result<String, Reply> {
    match req.body.get("taskListId") {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Number(n)) if n.is_u64() => Ok(n.to_string()),
        Some(_) => Err(req.bad_params("taskListId")),
    }
}

/// An integer argument: `Ok(None)` when absent, `null` or `""`; a number or a decimal string;
/// `Err` for anything else.
fn int_arg(body: &Value, name: &str) -> Result<Option<i64>, ()> {
    match body.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n.as_i64().map(Some).ok_or(()),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => s.trim().parse().map(Some).map_err(|_| ()),
        Some(_) => Err(()),
    }
}

/// A paging argument as text: a string, a number or a one-element array of either (6.1 names
/// the token field `nextPageTokens`, plural); `None` when absent or of another shape. Paging
/// arguments never answer `422`: a poll that fails would fail the device's to-do sync each time.
fn paging_text(body: &Value, name: &str) -> Option<String> {
    let value = match body.get(name)? {
        Value::Array(a) if a.len() == 1 => &a[0],
        v => v,
    };
    match value {
        Value::String(s) => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// `maxResults`: a positive decimal; anything else is [`DEFAULT_MAX_RESULTS`].
fn max_results(body: &Value) -> u64 {
    paging_text(body, "maxResults")
        .and_then(|t| t.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_RESULTS)
}

/// `pageToken` of `group/all`: the decimal offset this server answered; anything else is the
/// first page.
fn group_page_token(body: &Value) -> u64 {
    paging_text(body, "pageToken")
        .and_then(|t| t.parse().ok())
        .unwrap_or(0)
}

/// `nextPageTokens` of `task/all`: `<offset>:<pull time>` as this server answers it, the pull
/// time being the server time of the pull's first page (or a bare offset). Anything else is
/// the first page of a new pull.
fn task_page_token(body: &Value) -> (u64, Option<u64>) {
    let Some(text) = paging_text(body, "nextPageTokens") else {
        return (0, None);
    };
    let (offset, time) = match text.split_once(':') {
        Some((o, t)) => (o, t.parse().ok()),
        None => (text.as_str(), None),
    };
    match offset.parse() {
        Ok(offset) => (offset, time),
        Err(_) => (0, None),
    }
}

/// `nextIndexNumber` of the sort query: a decimal index; anything else is the first sort.
fn sort_index(body: &Value) -> u64 {
    paging_text(body, "nextIndexNumber")
        .and_then(|t| t.parse().ok())
        .unwrap_or(0)
}

/// The `nextSyncToken` of a pull.
#[derive(Debug, Clone, Copy)]
enum SyncToken {
    /// Absent, `null`, `""` or 0: a full pull.
    Full,
    /// Changes since this server time (ms).
    Since(u64),
    /// No number: answered like a token that is too old.
    Bad,
}

fn sync_token(body: &Value) -> SyncToken {
    match int_arg(body, "nextSyncToken") {
        Ok(None | Some(0)) => SyncToken::Full,
        Ok(Some(t)) if t > 0 => SyncToken::Since(t as u64),
        _ => SyncToken::Bad,
    }
}

// ------------------------------------------------------------------ answers

/// One page of `rows` from `offset`, and the offset of the next page when rows follow.
fn page<T>(rows: Vec<T>, offset: u64, max: u64) -> (Vec<T>, Option<u64>) {
    let total = rows.len() as u64;
    let end = offset.saturating_add(max);
    let rows = rows
        .into_iter()
        .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        .take(usize::try_from(max).unwrap_or(usize::MAX))
        .collect();
    (rows, (end < total).then_some(end))
}

/// A group as the device gets it, with the account's `userId`.
fn group_view(g: &TaskGroup, user_id: u64) -> Value {
    let mut w = g.wire();
    w.user_id = user_id;
    serde_json::to_value(w).unwrap_or_else(|_| json!({}))
}

fn task_view(t: &Task) -> Value {
    serde_json::to_value(t.wire()).unwrap_or_else(|_| json!({}))
}

/// The fields of `value` (an object) as the answer's payload, plus `extra`.
fn with_fields(value: Value, extra: &[(&str, Value)]) -> Value {
    let mut m = match value {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    for (k, v) in extra {
        m.insert((*k).to_string(), v.clone());
    }
    Value::Object(m)
}

/// Turn `tasks` into tombstones, journaled as one batch.
fn tombstone(c: &super::digests::Ctx<'_>, tasks: Vec<Task>) -> crate::Result<()> {
    let tasks: Vec<Task> = tasks
        .into_iter()
        .map(|mut t| {
            t.is_deleted = YES.into();
            t
        })
        .collect();
    c.app.put_tasks(
        c.guard,
        &tasks,
        Origin::Device,
        Some(c.serial),
        &c.state.journal,
    )
}

/// The live tasks of the group `list` that `keep` selects.
fn tasks_of_group(
    c: &super::digests::Ctx<'_>,
    list: &str,
    keep: impl Fn(&Task) -> bool,
) -> crate::Result<Vec<Task>> {
    Ok(c.app
        .tasks(c.guard)?
        .into_iter()
        .filter(|t| t.is_live() && t.task_list_id == list && keep(t))
        .collect())
}

// ------------------------------------------------------------------ groups

/// `POST /api/file/schedule/group/all` `{maxResults, pageToken}`: `scheduleTaskGroup` of the
/// groups in `taskListId` order, and `pageToken` when more pages follow.
pub async fn group_all(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let max = max_results(&req.body);
    let offset = group_page_token(&req.body);
    with_app_data(&state, &req, move |c| {
        let mut groups: Vec<TaskGroup> = c
            .app
            .task_groups(c.guard)?
            .into_iter()
            .filter(TaskGroup::is_live)
            .collect();
        groups.sort_by(|a, b| a.task_list_id.cmp(&b.task_list_id));
        let (rows, next) = page(groups, offset, max);
        let list: Vec<Value> = rows.iter().map(|g| group_view(g, c.user_id)).collect();
        let mut fields = json!({"scheduleTaskGroup": list});
        if let Some(next) = next {
            fields["pageToken"] = json!(next.to_string());
        }
        Ok(c.ok(fields))
    })
    .await
}

/// `POST` and `PUT /api/file/schedule/group`: store the group under the device's `taskListId`
/// (an upsert). A stored `createTime` stays (the update carries none); a new group without one
/// gets the server time. `POST` answers `{taskListId}`.
async fn save_group(state: Arc<AppState>, headers: HeaderMap, bytes: Bytes, create: bool) -> Reply {
    let req = app_request!(state, headers, bytes);
    let mut group: TaskGroup = match record(&req, &req.body, GROUP_SERVER_FIELDS) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if let Err(r) = key(&req, &group.task_list_id, "taskListId") {
        return r;
    }
    with_app_data(&state, &req, move |c| {
        let old = c.app.task_group(c.guard, &group.task_list_id)?;
        if let Some(created) = old.map(|o| o.create_time).filter(|t| !t.is_null()) {
            group.create_time = created;
        } else if group.create_time.is_null() {
            group.create_time = json!(c.state.journal.now_ms());
        }
        group.user_id = c.user_id;
        group.is_deleted = NO.into();
        c.app.put_task_group(
            c.guard,
            &group,
            Origin::Device,
            Some(c.serial),
            &c.state.journal,
        )?;
        Ok(c.ok(if create {
            json!({"taskListId": group.task_list_id})
        } else {
            json!({})
        }))
    })
    .await
}

/// `POST /api/file/schedule/group` `{taskListId, title, lastModified, createTime}`.
pub async fn add_group(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    save_group(state, headers, bytes, true).await
}

/// `PUT /api/file/schedule/group` `{taskListId, title, lastModified}`.
pub async fn update_group(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    save_group(state, headers, bytes, false).await
}

/// `DELETE /api/file/schedule/group/{taskListId}`: tombstones for the group's tasks (one
/// journal batch), then its sort and the group. Tasks first: a crash in between leaves the
/// group, and the device's retry finishes the cascade.
pub async fn delete_group(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let id = match key(&req, &id, "taskListId") {
        Ok(v) => v,
        Err(r) => return r,
    };
    with_app_data(&state, &req, move |c| {
        let tasks = tasks_of_group(&c, &id, |_| true)?;
        tombstone(&c, tasks)?;
        c.app.delete_task_sort(c.guard, &id)?;
        c.app.delete_task_group(
            c.guard,
            &id,
            Origin::Device,
            Some(c.serial),
            &c.state.journal,
        )?;
        Ok(c.ok(json!({})))
    })
    .await
}

/// `POST /api/file/schedule/group/clear` `{taskListId, lastModified}`: tombstones for the
/// group's completed tasks (one journal batch); the group takes the `lastModified` sent.
pub async fn clear_group(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let sent = match list_id_arg(&req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let id = match key(&req, &sent, "taskListId") {
        Ok(v) => v,
        Err(r) => return r,
    };
    let last_modified = req.body.get("lastModified").cloned().unwrap_or(Value::Null);
    with_app_data(&state, &req, move |c| {
        let done = tasks_of_group(&c, &id, |t| t.status.as_deref() == Some(COMPLETED))?;
        tombstone(&c, done)?;
        if let Some(mut group) = c.app.task_group(c.guard, &id)?.filter(TaskGroup::is_live)
            && !last_modified.is_null()
        {
            group.last_modified = last_modified;
            c.app.put_task_group(
                c.guard,
                &group,
                Origin::Device,
                Some(c.serial),
                &c.state.journal,
            )?;
        }
        Ok(c.ok(json!({})))
    })
    .await
}

/// `GET /api/file/schedule/group/{taskListId}`: the group's fields (`E0328` without one).
pub async fn get_group(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let id = match key(&req, &id, "taskListId") {
        Ok(v) => v,
        Err(r) => return r,
    };
    with_app_data(&state, &req, move |c| {
        Ok(
            match c.app.task_group(c.guard, &id)?.filter(TaskGroup::is_live) {
                Some(g) => c.ok(group_view(&g, c.user_id)),
                None => c.error(E_GROUP_MISSING, MSG_GROUP_MISSING),
            },
        )
    })
    .await
}

// ------------------------------------------------------------------ tasks

/// `POST /api/file/schedule/task/all` `{maxResults, nextPageTokens, nextSyncToken}` (D8):
/// `scheduleTask`, `nextSyncToken` (the server time) and `nextPageToken` when more pages follow.
/// Tombstones older than 5 days are removed first.
pub async fn task_all(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let max = max_results(&req.body);
    let (offset, pulled) = task_page_token(&req.body);
    let token = sync_token(&req.body);
    with_app_data(&state, &req, move |c| {
        let now = c.state.journal.now_ms();
        c.app.sweep_tombstones(c.guard, now)?;
        let since = match token {
            SyncToken::Full => None,
            SyncToken::Since(t) if t <= now && now - t <= TOMBSTONE_TTL_MS => Some(t),
            SyncToken::Since(_) | SyncToken::Bad => {
                return Ok(c.error(E_TOKEN_TOO_OLD, MSG_TOKEN_TOO_OLD));
            }
        };
        let mut tasks: Vec<Task> = c
            .app
            .tasks(c.guard)?
            .into_iter()
            .filter(|t| match since {
                None => t.is_live(),
                Some(since) => t.updated_at_ms.is_some_and(|at| at >= since),
            })
            .collect();
        tasks.sort_by(|a, b| a.task_id.cmp(&b.task_id));
        // Every page of one pull answers the time of its first page: a change made between
        // page calls comes with the next incremental pull.
        let pulled = pulled.filter(|t| *t > 0 && *t <= now).unwrap_or(now);
        let (rows, next) = page(tasks, offset, max);
        let list: Vec<Value> = rows.iter().map(task_view).collect();
        let mut fields = json!({"scheduleTask": list, "nextSyncToken": pulled});
        if let Some(next) = next {
            fields["nextPageToken"] = json!(format!("{next}:{pulled}"));
        }
        Ok(c.ok(fields))
    })
    .await
}

/// `POST` and `PUT /api/file/schedule/task`: store the task under its `taskId` (an upsert),
/// answer `{taskId}`.
pub async fn save_task(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let task = match task_from(&req, &req.body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    with_app_data(&state, &req, move |c| {
        c.app.put_task(
            c.guard,
            &task,
            Origin::Device,
            Some(c.serial),
            &c.state.journal,
        )?;
        Ok(c.ok(json!({"taskId": task.task_id})))
    })
    .await
}

/// `PUT /api/file/schedule/task/list` `{taskListId, updateScheduleTaskList}`: store every task
/// (one journal batch). A task without a `taskListId` takes the outer one. One bad task stores
/// none.
pub async fn save_task_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let list = match list_id_arg(&req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let items = match req.body.get("updateScheduleTaskList") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.clone(),
        Some(_) => return req.bad_params("updateScheduleTaskList"),
    };
    let mut tasks = Vec::with_capacity(items.len());
    for item in &items {
        let mut task = match task_from(&req, item) {
            Ok(v) => v,
            Err(r) => return r,
        };
        if task.task_list_id.is_empty() {
            task.task_list_id = list.clone();
        }
        tasks.push(task);
    }
    with_app_data(&state, &req, move |c| {
        c.app.put_tasks(
            c.guard,
            &tasks,
            Origin::Device,
            Some(c.serial),
            &c.state.journal,
        )?;
        Ok(c.ok(json!({})))
    })
    .await
}

/// `DELETE /api/file/schedule/task/{taskId}`: hard on the wire, a tombstone on disk (D5).
pub async fn delete_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let id = match key(&req, &id, "taskId") {
        Ok(v) => v,
        Err(r) => return r,
    };
    with_app_data(&state, &req, move |c| {
        c.app.delete_task(
            c.guard,
            &id,
            Origin::Device,
            Some(c.serial),
            &c.state.journal,
        )?;
        Ok(c.ok(json!({})))
    })
    .await
}

/// `GET /api/file/schedule/task/{taskId}`: the task's fields and `scheduleRecurTask: []`
/// (`E0329` without a live task).
pub async fn get_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let id = match key(&req, &id, "taskId") {
        Ok(v) => v,
        Err(r) => return r,
    };
    with_app_data(&state, &req, move |c| {
        Ok(match c.app.task(c.guard, &id)?.filter(Task::is_live) {
            Some(t) => c.ok(with_fields(
                task_view(&t),
                &[("scheduleRecurTask", json!([]))],
            )),
            None => c.error(E_TASK_MISSING, MSG_TASK_MISSING),
        })
    })
    .await
}

// ------------------------------------------------------------------ sort

/// `POST` and `PUT /api/file/schedule/sort` `{taskListId, title, lastModify, content}`: store
/// the group's sort (an upsert).
pub async fn save_sort(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let sort: TaskSort = match record(&req, &req.body, &[]) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if let Err(r) = key(&req, &sort.task_list_id, "taskListId") {
        return r;
    }
    with_app_data(&state, &req, move |c| {
        c.app.put_task_sort(c.guard, &sort)?;
        Ok(c.ok(json!({})))
    })
    .await
}

/// `DELETE /api/file/schedule/sort/{taskListId}`.
pub async fn delete_sort(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let id = match key(&req, &id, "taskListId") {
        Ok(v) => v,
        Err(r) => return r,
    };
    with_app_data(&state, &req, move |c| {
        c.app.delete_task_sort(c.guard, &id)?;
        Ok(c.ok(json!({})))
    })
    .await
}

/// `POST /api/file/query/schedule/sort` `{nextIndexNumber}`: the sort at that index (`null` is
/// 0) in `taskListId` order, and `nextIndexNumber` of the next one (`null` after the last).
/// Without a sort at the index all fields are `null`.
pub async fn query_sort(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Reply {
    let req = app_request!(state, headers, bytes);
    let index = sort_index(&req.body);
    with_app_data(&state, &req, move |c| {
        let mut sorts = c.app.task_sorts(c.guard)?;
        sorts.sort_by(|a, b| a.task_list_id.cmp(&b.task_list_id));
        let at = usize::try_from(index).unwrap_or(usize::MAX);
        let fields = match sorts.get(at) {
            Some(sort) => {
                let next = if at + 1 < sorts.len() {
                    json!(at + 1)
                } else {
                    Value::Null
                };
                with_fields(
                    serde_json::to_value(sort).unwrap_or_else(|_| json!({})),
                    &[("nextIndexNumber", next)],
                )
            }
            None => json!({"taskListId": null, "title": null, "lastModify": null,
                           "content": null, "nextIndexNumber": null}),
        };
        Ok(c.ok(fields))
    })
    .await
}
