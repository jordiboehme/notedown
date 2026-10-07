//! To-do records: tasks, task groups (lists) and their sort orders (`docs/sync-protocol.md`
//! 6.1).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{
    AppData, Change, NO, TOMBSTONE_TTL_MS, YES, change, read_record, read_records, remove_record,
    valid_task_key, write_record,
};
use crate::journal::{EventBody, Journal, Origin, TaskGroupInfo, TaskInfo};
use crate::store::WriteGuard;
use crate::{Error, Result};

const ITEMS_DIR: &str = "items";
const GROUPS_DIR: &str = "groups";
const SORT_DIR: &str = "sort";

/// One task: the task fields of 6.1 in their order, the server-only times, then the fields the
/// device sent that are not named here. Fields whose type the protocol does not pin down are
/// kept as the JSON value the device sent (integers stay exact).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Task {
    /// 32 hex, made by the device; the file name of the record.
    pub task_id: String,
    pub task_list_id: String,
    pub recurrence_id: Value,
    pub title: Option<String>,
    pub detail: Option<String>,
    pub last_modified: Value,
    /// An RFC 5545 rule.
    pub recurrence: Option<String>,
    pub is_reminder_on: Value,
    /// `needsAction` or `completed`.
    pub status: Option<String>,
    pub importance: Value,
    pub due_time: Value,
    pub completed_time: Value,
    /// Base64 of a JSON link to a note page; opaque.
    pub links: Option<String>,
    /// `"Y"` for a tombstone.
    pub is_deleted: String,
    pub sort: Value,
    pub sort_completed: Value,
    pub planer_sort: Value,
    pub all_sort: Value,
    pub all_sort_completed: Value,
    pub sort_time: Value,
    pub planer_sort_time: Value,
    pub all_sort_time: Value,
    /// Server only: when the server last wrote the record (ms), the `updatedAt` of the
    /// incremental pull.
    #[serde(rename = "updated_at_ms", skip_serializing_if = "Option::is_none")]
    pub updated_at_ms: Option<u64>,
    /// Server only: when the task was deleted (ms); set on tombstones.
    #[serde(rename = "deleted_at_ms", skip_serializing_if = "Option::is_none")]
    pub deleted_at_ms: Option<u64>,
    /// Fields of the device that are not named above, sent back unchanged.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Task {
    fn default() -> Self {
        Task {
            task_id: String::new(),
            task_list_id: String::new(),
            recurrence_id: Value::Null,
            title: None,
            detail: None,
            last_modified: Value::Null,
            recurrence: None,
            is_reminder_on: Value::Null,
            status: None,
            importance: Value::Null,
            due_time: Value::Null,
            completed_time: Value::Null,
            links: None,
            is_deleted: NO.into(),
            sort: Value::Null,
            sort_completed: Value::Null,
            planer_sort: Value::Null,
            all_sort: Value::Null,
            all_sort_completed: Value::Null,
            sort_time: Value::Null,
            planer_sort_time: Value::Null,
            all_sort_time: Value::Null,
            updated_at_ms: None,
            deleted_at_ms: None,
            extra: Map::new(),
        }
    }
}

impl Task {
    /// False for a tombstone.
    pub fn is_live(&self) -> bool {
        self.is_deleted != YES
    }

    /// The record as the device gets it: without the server-only fields.
    pub fn wire(&self) -> Task {
        Task {
            updated_at_ms: None,
            deleted_at_ms: None,
            ..self.clone()
        }
    }

    pub(crate) fn info(&self) -> TaskInfo {
        TaskInfo {
            task_id: self.task_id.clone(),
            task_list_id: self.task_list_id.clone(),
            title: self.title.clone().unwrap_or_default(),
            status: self.status.clone().unwrap_or_default(),
        }
    }
}

/// One task group (list): the fields of `scheduleTaskGroup` (6.1) in their order, the
/// server-only time, then the fields the device sent that are not named here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TaskGroup {
    /// Made by the device; the file name of the record.
    pub task_list_id: String,
    /// The owner's user id; set by the caller.
    pub user_id: u64,
    pub title: Option<String>,
    pub last_modified: Value,
    pub is_deleted: String,
    pub create_time: Value,
    /// Server only: when the server last wrote the record (ms).
    #[serde(rename = "updated_at_ms", skip_serializing_if = "Option::is_none")]
    pub updated_at_ms: Option<u64>,
    /// Fields of the device that are not named above, sent back unchanged.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for TaskGroup {
    fn default() -> Self {
        TaskGroup {
            task_list_id: String::new(),
            user_id: 0,
            title: None,
            last_modified: Value::Null,
            is_deleted: NO.into(),
            create_time: Value::Null,
            updated_at_ms: None,
            extra: Map::new(),
        }
    }
}

impl TaskGroup {
    /// False once deleted.
    pub fn is_live(&self) -> bool {
        self.is_deleted != YES
    }

    /// The record as the device gets it: without the server-only field.
    pub fn wire(&self) -> TaskGroup {
        TaskGroup {
            updated_at_ms: None,
            ..self.clone()
        }
    }

    fn info(&self) -> TaskGroupInfo {
        TaskGroupInfo {
            task_list_id: self.task_list_id.clone(),
            title: self.title.clone().unwrap_or_default(),
        }
    }
}

/// The sort order of one task group (6.1: `lastModify` without "d"); opaque to the server.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TaskSort {
    /// The file name of the record.
    pub task_list_id: String,
    pub title: Option<String>,
    pub last_modify: Value,
    pub content: Value,
    /// Fields of the device that are not named above, sent back unchanged.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn task_event(c: Change, info: TaskInfo) -> EventBody {
    match c {
        Change::Created => EventBody::TaskCreated(info),
        Change::Updated => EventBody::TaskUpdated(info),
        Change::Deleted => EventBody::TaskDeleted(info),
    }
}

fn group_event(c: Change, info: TaskGroupInfo) -> EventBody {
    match c {
        Change::Created => EventBody::TaskGroupCreated(info),
        Change::Updated => EventBody::TaskGroupUpdated(info),
        Change::Deleted => EventBody::TaskGroupDeleted(info),
    }
}

fn checked_key<'k>(key: &'k str, what: &'static str) -> Result<&'k str> {
    if valid_task_key(key) {
        Ok(key)
    } else {
        Err(Error::Invalid {
            what,
            message: format!("{key:?} is not 1 to 128 of A-Z, a-z, 0-9, - and _"),
        })
    }
}

impl AppData<'_> {
    fn task_path(&self, task_id: &str) -> Result<PathBuf> {
        let key = checked_key(task_id, "taskId")?;
        Ok(self.tasks_dir().join(ITEMS_DIR).join(format!("{key}.json")))
    }

    fn task_group_path(&self, task_list_id: &str) -> Result<PathBuf> {
        let key = checked_key(task_list_id, "taskListId")?;
        Ok(self
            .tasks_dir()
            .join(GROUPS_DIR)
            .join(format!("{key}.json")))
    }

    fn task_sort_path(&self, task_list_id: &str) -> Result<PathBuf> {
        let key = checked_key(task_list_id, "taskListId")?;
        Ok(self.tasks_dir().join(SORT_DIR).join(format!("{key}.json")))
    }

    /// Every task, tombstones included, by `taskId`.
    pub fn tasks(&self, guard: &WriteGuard) -> Result<Vec<Task>> {
        self.check(guard)?;
        read_records(&self.tasks_dir().join(ITEMS_DIR))
    }

    /// The task `task_id`, a tombstone too.
    pub fn task(&self, guard: &WriteGuard, task_id: &str) -> Result<Option<Task>> {
        self.check(guard)?;
        read_record(&self.task_path(task_id)?)
    }

    /// Write one task with its server times; its event, if any.
    fn store_task(&self, guard: &WriteGuard, task: &Task, now: u64) -> Result<Option<EventBody>> {
        let old = self.task(guard, &task.task_id)?;
        let mut record = task.clone();
        record.updated_at_ms = Some(now);
        record.deleted_at_ms = if record.is_live() {
            None
        } else {
            old.as_ref()
                .filter(|o| !o.is_live())
                .and_then(|o| o.deleted_at_ms)
                .or(Some(now))
        };
        write_record(&self.task_path(&record.task_id)?, &record)?;
        let was_live = old.as_ref().is_some_and(Task::is_live);
        Ok(change(was_live, record.is_live()).map(|c| task_event(c, record.info())))
    }

    /// Store `task` under its `taskId` (a full replace) with `updated_at_ms` = the journal
    /// clock's now, then journal `task.created` (new, or back from a tombstone),
    /// `task.updated` or `task.deleted` (`isDeleted` turned `"Y"`: a tombstone with
    /// `deleted_at_ms`).
    pub fn put_task(
        &self,
        guard: &WriteGuard,
        task: &Task,
        origin: Origin,
        device: Option<&str>,
        journal: &Journal,
    ) -> Result<()> {
        self.put_tasks(guard, std::slice::from_ref(task), origin, device, journal)
    }

    /// [`AppData::put_task`] for several tasks, journaled as one batch (one `opId`). The
    /// `taskId` of every task is checked before anything is written.
    pub fn put_tasks(
        &self,
        guard: &WriteGuard,
        tasks: &[Task],
        origin: Origin,
        device: Option<&str>,
        journal: &Journal,
    ) -> Result<()> {
        for t in tasks {
            checked_key(&t.task_id, "taskId")?;
        }
        let now = journal.now_ms();
        let mut bodies = Vec::new();
        for t in tasks {
            bodies.extend(self.store_task(guard, t, now)?);
        }
        self.journal(bodies, origin, device, journal)
    }

    /// Delete the task `task_id` (hard on the wire): the record stays as a tombstone
    /// (`isDeleted: "Y"`, `deleted_at_ms`) for the incremental pull until
    /// [`AppData::sweep_tombstones`] removes it; `task.deleted` keeps its title. False when
    /// there is no live task with that id. Nothing goes to `recycle/`.
    pub fn delete_task(
        &self,
        guard: &WriteGuard,
        task_id: &str,
        origin: Origin,
        device: Option<&str>,
        journal: &Journal,
    ) -> Result<bool> {
        let Some(mut t) = self.task(guard, task_id)?.filter(Task::is_live) else {
            return Ok(false);
        };
        t.is_deleted = YES.into();
        self.put_task(guard, &t, origin, device, journal)?;
        Ok(true)
    }

    /// Remove the task tombstones deleted more than [`TOMBSTONE_TTL_MS`] before `now_ms`.
    /// Not journaled (`task.deleted` was). Returns how many were removed.
    pub fn sweep_tombstones(&self, guard: &WriteGuard, now_ms: u64) -> Result<usize> {
        let mut removed = 0;
        for t in self.tasks(guard)? {
            let expired = !t.is_live()
                && t.deleted_at_ms
                    .is_some_and(|at| now_ms.saturating_sub(at) > TOMBSTONE_TTL_MS);
            if expired && remove_record(&self.task_path(&t.task_id)?)? {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Every task group, by `taskListId`.
    pub fn task_groups(&self, guard: &WriteGuard) -> Result<Vec<TaskGroup>> {
        self.check(guard)?;
        read_records(&self.tasks_dir().join(GROUPS_DIR))
    }

    /// The task group `task_list_id`.
    pub fn task_group(&self, guard: &WriteGuard, task_list_id: &str) -> Result<Option<TaskGroup>> {
        self.check(guard)?;
        read_record(&self.task_group_path(task_list_id)?)
    }

    /// Store `group` under its `taskListId` (a full replace) with `updated_at_ms` = the journal
    /// clock's now, then journal `task_group.created`, `.updated` or `.deleted`.
    pub fn put_task_group(
        &self,
        guard: &WriteGuard,
        group: &TaskGroup,
        origin: Origin,
        device: Option<&str>,
        journal: &Journal,
    ) -> Result<()> {
        let path = self.task_group_path(&group.task_list_id)?;
        let old = self.task_group(guard, &group.task_list_id)?;
        let mut record = group.clone();
        record.updated_at_ms = Some(journal.now_ms());
        write_record(&path, &record)?;
        let was_live = old.as_ref().is_some_and(TaskGroup::is_live);
        let bodies = change(was_live, record.is_live())
            .map(|c| group_event(c, record.info()))
            .into_iter()
            .collect();
        self.journal(bodies, origin, device, journal)
    }

    /// Remove the task group `task_list_id` and journal `task_group.deleted`. Its tasks and
    /// sort are not touched here. False when there is no such group.
    pub fn delete_task_group(
        &self,
        guard: &WriteGuard,
        task_list_id: &str,
        origin: Origin,
        device: Option<&str>,
        journal: &Journal,
    ) -> Result<bool> {
        let Some(old) = self.task_group(guard, task_list_id)? else {
            return Ok(false);
        };
        remove_record(&self.task_group_path(task_list_id)?)?;
        let bodies = if old.is_live() {
            vec![EventBody::TaskGroupDeleted(old.info())]
        } else {
            Vec::new()
        };
        self.journal(bodies, origin, device, journal)?;
        Ok(true)
    }

    /// Every task sort, by `taskListId`.
    pub fn task_sorts(&self, guard: &WriteGuard) -> Result<Vec<TaskSort>> {
        self.check(guard)?;
        read_records(&self.tasks_dir().join(SORT_DIR))
    }

    /// The sort of the task group `task_list_id`.
    pub fn task_sort(&self, guard: &WriteGuard, task_list_id: &str) -> Result<Option<TaskSort>> {
        self.check(guard)?;
        read_record(&self.task_sort_path(task_list_id)?)
    }

    /// Store `sort` under its `taskListId` (a full replace). Sorts are not journaled.
    pub fn put_task_sort(&self, guard: &WriteGuard, sort: &TaskSort) -> Result<()> {
        self.check(guard)?;
        write_record(&self.task_sort_path(&sort.task_list_id)?, sort)
    }

    /// Remove the sort of `task_list_id`; false when there is none.
    pub fn delete_task_sort(&self, guard: &WriteGuard, task_list_id: &str) -> Result<bool> {
        self.check(guard)?;
        remove_record(&self.task_sort_path(task_list_id)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appdata::testutil::{Fixture, T0};

    const DAY: i64 = 24 * 60 * 60 * 1000;

    fn task(id: &str) -> Task {
        Task {
            task_id: id.into(),
            task_list_id: "l1".into(),
            title: Some(format!("task {id}")),
            status: Some("needsAction".into()),
            ..Task::default()
        }
    }

    #[test]
    fn task_serializes_in_wire_order_with_extra_last() {
        let mut extra = Map::new();
        extra.insert("newField".into(), Value::from(1));
        let t = Task {
            task_id: "t".into(),
            task_list_id: "l".into(),
            recurrence_id: Value::from("r"),
            title: Some("ti".into()),
            detail: Some("de".into()),
            last_modified: Value::from(1_700_000_000_000_u64),
            recurrence: Some("RRULE:FREQ=DAILY".into()),
            is_reminder_on: Value::from("N"),
            status: Some("completed".into()),
            importance: Value::from(0),
            due_time: Value::from(1),
            completed_time: Value::from(2),
            links: Some("eyJ9".into()),
            is_deleted: "N".into(),
            sort: Value::from(3),
            sort_completed: Value::from(4),
            planer_sort: Value::from(5),
            all_sort: Value::from(6),
            all_sort_completed: Value::from(7),
            sort_time: Value::from(8),
            planer_sort_time: Value::from(9),
            all_sort_time: Value::from(895_334_188_016_730_112_u64),
            updated_at_ms: Some(10),
            deleted_at_ms: Some(11),
            extra,
        };
        let text = serde_json::to_string(&t).unwrap();
        assert_eq!(
            text,
            concat!(
                r#"{"taskId":"t","taskListId":"l","recurrenceId":"r","title":"ti","detail":"de","#,
                r#""lastModified":1700000000000,"recurrence":"RRULE:FREQ=DAILY","isReminderOn":"N","#,
                r#""status":"completed","importance":0,"dueTime":1,"completedTime":2,"links":"eyJ9","#,
                r#""isDeleted":"N","sort":3,"sortCompleted":4,"planerSort":5,"allSort":6,"#,
                r#""allSortCompleted":7,"sortTime":8,"planerSortTime":9,"#,
                r#""allSortTime":895334188016730112,"updated_at_ms":10,"deleted_at_ms":11,"#,
                r#""newField":1}"#
            )
        );
        assert_eq!(serde_json::from_str::<Task>(&text).unwrap(), t);
        let wire = serde_json::to_string(&t.wire()).unwrap();
        assert!(
            !wire.contains("_at_ms")
                && wire.ends_with(r#""allSortTime":895334188016730112,"newField":1}"#)
        );
    }

    #[test]
    fn group_and_sort_serialize_in_wire_order() {
        let g = TaskGroup {
            task_list_id: "l".into(),
            user_id: 1_000_001,
            title: Some("Home".into()),
            last_modified: Value::from(1),
            is_deleted: "N".into(),
            create_time: Value::from(2),
            updated_at_ms: Some(3),
            extra: Map::new(),
        };
        assert_eq!(
            serde_json::to_string(&g).unwrap(),
            r#"{"taskListId":"l","userId":1000001,"title":"Home","lastModified":1,"isDeleted":"N","createTime":2,"updated_at_ms":3}"#
        );
        assert_eq!(
            serde_json::to_string(&g.wire()).unwrap(),
            r#"{"taskListId":"l","userId":1000001,"title":"Home","lastModified":1,"isDeleted":"N","createTime":2}"#
        );
        let s: TaskSort = serde_json::from_str(
            r#"{"taskListId":"l","title":"t","lastModify":5,"content":"[1]","x":true}"#,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"taskListId":"l","title":"t","lastModify":5,"content":"[1]","x":true}"#
        );
    }

    #[test]
    fn a_deleted_task_stays_as_a_tombstone_outside_recycle() {
        let f = Fixture::new();
        let app = AppData::new(&f.store);
        let g = f.store.lock_recovered(&f.journal).unwrap();
        app.put_task(&g, &task("t1"), Origin::Device, Some("SN1"), &f.journal)
            .unwrap();
        f.set_now(T0 + 1000);
        assert!(
            app.delete_task(&g, "t1", Origin::Device, Some("SN1"), &f.journal)
                .unwrap()
        );
        let t = app.task(&g, "t1").unwrap().unwrap();
        assert_eq!(t.is_deleted, "Y");
        assert_eq!(t.deleted_at_ms, Some(T0 as u64 + 1000));
        assert_eq!(t.updated_at_ms, Some(T0 as u64 + 1000));
        assert_eq!(t.title.as_deref(), Some("task t1"));
        assert!(f.store.account_dir().join("tasks/items/t1.json").is_file());
        assert!(!f.store.account_dir().join("recycle").exists());
        // The device makes the same task again: live, no deletion time.
        app.put_task(&g, &task("t1"), Origin::Device, Some("SN1"), &f.journal)
            .unwrap();
        let t = app.task(&g, "t1").unwrap().unwrap();
        assert!(t.is_live());
        assert_eq!(t.deleted_at_ms, None);
    }

    #[test]
    fn tombstones_older_than_five_days_are_swept() {
        let f = Fixture::new();
        let app = AppData::new(&f.store);
        let g = f.store.lock_recovered(&f.journal).unwrap();
        for id in ["old", "young", "live"] {
            app.put_task(&g, &task(id), Origin::Cli, None, &f.journal)
                .unwrap();
        }
        app.delete_task(&g, "old", Origin::Cli, None, &f.journal)
            .unwrap();
        f.set_now(T0 + DAY);
        app.delete_task(&g, "young", Origin::Cli, None, &f.journal)
            .unwrap();
        let now = (T0 + 5 * DAY + 1) as u64;
        assert_eq!(app.sweep_tombstones(&g, now).unwrap(), 1);
        let left: Vec<String> = app
            .tasks(&g)
            .unwrap()
            .into_iter()
            .map(|t| t.task_id)
            .collect();
        assert_eq!(left, vec!["live".to_string(), "young".to_string()]);
        assert_eq!(app.sweep_tombstones(&g, (T0 + 60 * DAY) as u64).unwrap(), 1);
        assert_eq!(app.tasks(&g).unwrap().len(), 1);
    }

    #[test]
    fn device_keys_must_be_plain_file_names() {
        let f = Fixture::new();
        let app = AppData::new(&f.store);
        let g = f.store.lock_recovered(&f.journal).unwrap();
        assert!(
            app.put_task(&g, &task("../x"), Origin::Cli, None, &f.journal)
                .is_err()
        );
        assert!(app.task(&g, "a/b").is_err());
        let bad_group = TaskGroup {
            task_list_id: "..".into(),
            ..TaskGroup::default()
        };
        assert!(
            app.put_task_group(&g, &bad_group, Origin::Cli, None, &f.journal)
                .is_err()
        );
        // A batch with one bad key writes nothing.
        assert!(
            app.put_tasks(
                &g,
                &[task("ok"), task("no.json")],
                Origin::Cli,
                None,
                &f.journal
            )
            .is_err()
        );
        assert!(app.tasks(&g).unwrap().is_empty());
        assert_eq!(f.journal.last_seq().unwrap(), 0);
    }

    #[test]
    fn groups_and_sorts_are_removed_on_delete() {
        let f = Fixture::new();
        let app = AppData::new(&f.store);
        let g = f.store.lock_recovered(&f.journal).unwrap();
        let group = TaskGroup {
            task_list_id: "l1".into(),
            title: Some("Home".into()),
            ..TaskGroup::default()
        };
        app.put_task_group(&g, &group, Origin::Cli, None, &f.journal)
            .unwrap();
        assert_eq!(
            app.task_group(&g, "l1").unwrap().unwrap().updated_at_ms,
            Some(T0 as u64)
        );
        let sort = TaskSort {
            task_list_id: "l1".into(),
            content: Value::from("[]"),
            ..TaskSort::default()
        };
        app.put_task_sort(&g, &sort).unwrap();
        assert_eq!(app.task_sorts(&g).unwrap(), vec![sort.clone()]);
        assert_eq!(app.task_sort(&g, "l1").unwrap(), Some(sort));
        assert!(app.delete_task_sort(&g, "l1").unwrap());
        assert!(!app.delete_task_sort(&g, "l1").unwrap());
        assert!(
            app.delete_task_group(&g, "l1", Origin::Cli, None, &f.journal)
                .unwrap()
        );
        assert!(
            !app.delete_task_group(&g, "l1", Origin::Cli, None, &f.journal)
                .unwrap()
        );
        assert!(app.task_groups(&g).unwrap().is_empty());
    }
}
