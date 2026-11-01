use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

use tokio::sync::broadcast;

use crate::protocol::{
    Attention, AttentionKind, MoveDirection, Source, Task, TaskStatus, TurnState, WatchEvent,
};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("task not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,
    text TEXT NOT NULL,
    status TEXT NOT NULL,
    workspace TEXT NOT NULL,
    agent TEXT,
    source TEXT NOT NULL,
    reply_to TEXT,
    metadata TEXT NOT NULL,
    position INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS tasks_position_idx ON tasks (position);
CREATE INDEX IF NOT EXISTS tasks_status_idx ON tasks (status);
CREATE INDEX IF NOT EXISTS tasks_workspace_idx ON tasks (workspace);
";

const SELECT_COLUMNS: &str = "id, text, status, workspace, agent, source, reply_to, metadata, position, created_at, updated_at";

pub struct NewTask {
    pub text: String,
    pub workspace: String,
    pub agent: Option<String>,
    pub source: Source,
    pub reply_to: Option<serde_json::Value>,
    pub metadata: serde_json::Value,
    pub status: TaskStatus,
}

const WATCH_CHANNEL_CAPACITY: usize = 1024;

const WATCHED_STATUSES: [TaskStatus; 4] = [
    TaskStatus::Received,
    TaskStatus::Queued,
    TaskStatus::Running,
    TaskStatus::Interrupted,
];

struct TurnRecord {
    state: TurnState,
    idle_mark: u64,
    sending: Option<u64>,
}

impl TurnRecord {
    fn busy() -> TurnRecord {
        TurnRecord {
            state: TurnState::Busy,
            idle_mark: 0,
            sending: None,
        }
    }
}

struct Inner {
    conn: Connection,
    attention: HashMap<String, Attention>,
    turns: HashMap<String, TurnRecord>,
    seq: u64,
}

impl Inner {
    fn with_attention(&self, mut task: Task) -> Task {
        task.attention = self.attention.get(&task.id).cloned();
        task.turn = self.turns.get(&task.id).map(|t| t.state);
        task
    }

    fn get(&self, id: &str) -> Result<Option<Task>, StoreError> {
        Ok(get_locked(&self.conn, id)?.map(|t| self.with_attention(t)))
    }

    fn get_existing(&self, id: &str) -> Result<Task, StoreError> {
        self.get(id)?
            .ok_or_else(|| StoreError::NotFound(id.to_string()))
    }

    fn query(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Task>, StoreError> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params, map_row)?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(self.with_attention(to_task(row?)?));
        }
        Ok(tasks)
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn is_running(&self, id: &str) -> Result<Option<Task>, StoreError> {
        Ok(get_locked(&self.conn, id)?.filter(|t| t.status == TaskStatus::Running))
    }

    fn forget(&mut self, id: &str) {
        self.attention.remove(id);
        self.turns.remove(id);
    }
}

pub enum SendRejection {
    NotFound,
    NotRunning(TaskStatus),
    InFlight,
    NoAttention,
    NotAwaitingReply(AttentionKind),
    ExpectMismatch { current: u64 },
    TurnBusy,
    TurnNotEndedSinceReport,
}

pub struct AcceptedSend {
    pub task: Task,
    pub input_id: u64,
    pub previous: Attention,
}

// Every mutation publishes its watch event while still holding `inner`, so subscribers see changes in commit order and a snapshot taken under the same lock cannot miss or repeat one.
pub struct Store {
    inner: Mutex<Inner>,
    events: broadcast::Sender<WatchEvent>,
}

struct RawRow {
    id: String,
    text: String,
    status: String,
    workspace: String,
    agent: Option<String>,
    source: String,
    reply_to: Option<String>,
    metadata: String,
    position: i64,
    created_at: String,
    updated_at: String,
}

fn map_row(row: &rusqlite::Row) -> rusqlite::Result<RawRow> {
    Ok(RawRow {
        id: row.get(0)?,
        text: row.get(1)?,
        status: row.get(2)?,
        workspace: row.get(3)?,
        agent: row.get(4)?,
        source: row.get(5)?,
        reply_to: row.get(6)?,
        metadata: row.get(7)?,
        position: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

fn to_task(raw: RawRow) -> Result<Task, StoreError> {
    Ok(Task {
        id: raw.id,
        text: raw.text,
        status: TaskStatus::parse(&raw.status).ok_or_else(|| {
            StoreError::Invalid(format!("unknown status '{}' in store", raw.status))
        })?,
        workspace: raw.workspace,
        agent: raw.agent,
        source: serde_json::from_str(&raw.source)?,
        reply_to: raw.reply_to.map(|s| serde_json::from_str(&s)).transpose()?,
        metadata: serde_json::from_str(&raw.metadata)?,
        position: raw.position,
        created_at: raw.created_at,
        updated_at: raw.updated_at,
        attention: None,
        turn: None,
    })
}

fn get_locked(conn: &Connection, id: &str) -> Result<Option<Task>, StoreError> {
    let raw = conn
        .query_row(
            &format!("SELECT {SELECT_COLUMNS} FROM tasks WHERE id = ?1"),
            params![id],
            map_row,
        )
        .optional()?;
    raw.map(to_task).transpose()
}

pub fn now_rfc3339() -> String {
    jiff::Zoned::now()
        .strftime("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

impl Store {
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Store::with_connection(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Store, StoreError> {
        Store::with_connection(Connection::open_in_memory()?)
    }

    fn with_connection(conn: Connection) -> Result<Store, StoreError> {
        conn.execute_batch(SCHEMA)?;
        Ok(Store {
            inner: Mutex::new(Inner {
                conn,
                attention: HashMap::new(),
                turns: HashMap::new(),
                seq: 0,
            }),
            events: broadcast::Sender::new(WATCH_CHANNEL_CAPACITY),
        })
    }

    fn publish(&self, task: &Task) {
        let _ = self.events.send(WatchEvent::Task { task: task.clone() });
    }

    pub fn watch(&self) -> Result<(Vec<Task>, broadcast::Receiver<WatchEvent>), StoreError> {
        let inner = self.inner.lock().unwrap();
        let receiver = self.events.subscribe();
        let watched: Vec<&str> = WATCHED_STATUSES.iter().map(TaskStatus::as_str).collect();
        let tasks = inner.query(
            &format!(
                "SELECT {SELECT_COLUMNS} FROM tasks WHERE status IN (SELECT value FROM json_each(?1)) ORDER BY position ASC"
            ),
            params![serde_json::to_string(&watched)?],
        )?;
        Ok((tasks, receiver))
    }

    fn publish_locked(&self, inner: &Inner, id: &str) -> Result<Task, StoreError> {
        let task = inner.get_existing(id)?;
        self.publish(&task);
        Ok(task)
    }

    pub fn set_attention(
        &self,
        id: &str,
        kind: AttentionKind,
        message: String,
    ) -> Result<Option<Task>, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.is_running(id)?.is_none() {
            return Ok(None);
        }
        let report_id = inner.next_seq();
        inner.attention.insert(
            id.to_string(),
            Attention {
                id: report_id,
                kind,
                message,
                at: now_rfc3339(),
            },
        );
        inner
            .turns
            .entry(id.to_string())
            .or_insert_with(TurnRecord::busy)
            .state = TurnState::Busy;
        self.publish_locked(&inner, id).map(Some)
    }

    pub fn set_turn(&self, id: &str, state: TurnState) -> Result<Option<Task>, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let Some(task) = inner.is_running(id)? else {
            return Ok(None);
        };
        let mark = match state {
            TurnState::Idle => Some(inner.next_seq()),
            TurnState::Busy => None,
        };
        let record = inner
            .turns
            .entry(id.to_string())
            .or_insert_with(TurnRecord::busy);
        let changed = record.state != state;
        record.state = state;
        if let Some(mark) = mark {
            record.idle_mark = mark;
        }
        if changed {
            return self.publish_locked(&inner, id).map(Some);
        }
        Ok(Some(inner.with_attention(task)))
    }

    pub fn reset_turn(&self, id: &str) -> Result<Option<Task>, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.is_running(id)?.is_none() {
            return Ok(None);
        }
        inner.attention.remove(id);
        inner.turns.insert(id.to_string(), TurnRecord::busy());
        self.publish_locked(&inner, id).map(Some)
    }

    pub fn begin_send(
        &self,
        id: &str,
        expect: u64,
    ) -> Result<Result<AcceptedSend, SendRejection>, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let Some(task) = get_locked(&inner.conn, id)? else {
            return Ok(Err(SendRejection::NotFound));
        };
        if task.status != TaskStatus::Running {
            return Ok(Err(SendRejection::NotRunning(task.status)));
        }
        let turn = inner.turns.get(id);
        if turn.is_some_and(|t| t.sending.is_some()) {
            return Ok(Err(SendRejection::InFlight));
        }
        let Some(attention) = inner.attention.get(id) else {
            return Ok(Err(SendRejection::NoAttention));
        };
        if !attention.kind.awaits_reply() {
            return Ok(Err(SendRejection::NotAwaitingReply(attention.kind)));
        }
        if attention.id != expect {
            return Ok(Err(SendRejection::ExpectMismatch {
                current: attention.id,
            }));
        }
        match turn {
            Some(t) if t.state == TurnState::Idle && t.idle_mark > attention.id => {}
            Some(t) if t.state == TurnState::Idle => {
                return Ok(Err(SendRejection::TurnNotEndedSinceReport));
            }
            _ => return Ok(Err(SendRejection::TurnBusy)),
        }
        let previous = attention.clone();
        let input_id = inner.next_seq();
        inner.attention.remove(id);
        let record = inner.turns.get_mut(id).expect("turn checked above");
        record.state = TurnState::Busy;
        record.sending = Some(input_id);
        let task = self.publish_locked(&inner, id)?;
        Ok(Ok(AcceptedSend {
            task,
            input_id,
            previous,
        }))
    }

    pub fn finish_send(&self, id: &str, input_id: u64) -> Result<Option<Task>, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(record) = inner.turns.get_mut(id)
            && record.sending == Some(input_id)
        {
            record.sending = None;
        }
        inner.get(id)
    }

    pub fn abort_send(
        &self,
        id: &str,
        input_id: u64,
        previous: Attention,
    ) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let ours = inner
            .turns
            .get(id)
            .is_some_and(|t| t.sending == Some(input_id));
        if !ours {
            return Ok(false);
        }
        if inner.attention.contains_key(id) {
            if let Some(record) = inner.turns.get_mut(id) {
                record.sending = None;
            }
            return Ok(false);
        }
        let mark = inner.next_seq();
        let record = inner.turns.get_mut(id).expect("checked above");
        record.sending = None;
        record.state = TurnState::Idle;
        record.idle_mark = mark;
        inner.attention.insert(id.to_string(), previous);
        self.publish_locked(&inner, id)?;
        Ok(true)
    }

    pub fn insert(&self, new_task: NewTask) -> Result<Task, StoreError> {
        let inner = self.inner.lock().unwrap();
        let conn = &inner.conn;
        let id = ulid::Ulid::generate().to_string();
        let now = now_rfc3339();
        let position: i64 = conn.query_row(
            "SELECT COALESCE(MAX(position), 0) + 1 FROM tasks",
            [],
            |row| row.get(0),
        )?;
        conn.execute(
            "INSERT INTO tasks (id, text, status, workspace, agent, source, reply_to, metadata, position, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                id,
                new_task.text,
                new_task.status.as_str(),
                new_task.workspace,
                new_task.agent,
                serde_json::to_string(&new_task.source)?,
                new_task
                    .reply_to
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                serde_json::to_string(&new_task.metadata)?,
                position,
                now,
                now,
            ],
        )?;
        let task = inner.get_existing(&id)?;
        self.publish(&task);
        Ok(task)
    }

    pub fn get(&self, id: &str) -> Result<Option<Task>, StoreError> {
        self.inner.lock().unwrap().get(id)
    }

    pub fn list_all(&self) -> Result<Vec<Task>, StoreError> {
        self.inner.lock().unwrap().query(
            &format!("SELECT {SELECT_COLUMNS} FROM tasks ORDER BY position ASC"),
            [],
        )
    }

    pub fn list_by_status(&self, status: TaskStatus) -> Result<Vec<Task>, StoreError> {
        self.inner.lock().unwrap().query(
            &format!("SELECT {SELECT_COLUMNS} FROM tasks WHERE status = ?1 ORDER BY position ASC"),
            params![status.as_str()],
        )
    }

    pub fn update_status_if(
        &self,
        id: &str,
        expected: TaskStatus,
        status: TaskStatus,
    ) -> Result<Option<Task>, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let now = now_rfc3339();
        let changed = inner.conn.execute(
            "UPDATE tasks SET status = ?1, updated_at = ?2 WHERE id = ?3 AND status = ?4",
            params![status.as_str(), now, id, expected.as_str()],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        if status == TaskStatus::Running {
            inner.attention.remove(id);
            inner.turns.insert(id.to_string(), TurnRecord::busy());
        } else {
            inner.forget(id);
        }
        let task = inner.get_existing(id)?;
        self.publish(&task);
        Ok(Some(task))
    }

    pub fn update_text_unless(
        &self,
        id: &str,
        text: &str,
        excluded: TaskStatus,
    ) -> Result<Option<Task>, StoreError> {
        let inner = self.inner.lock().unwrap();
        let now = now_rfc3339();
        let changed = inner.conn.execute(
            "UPDATE tasks SET text = ?1, updated_at = ?2 WHERE id = ?3 AND status != ?4",
            params![text, now, id, excluded.as_str()],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        let task = inner.get_existing(id)?;
        self.publish(&task);
        Ok(Some(task))
    }

    pub fn delete_unless(&self, id: &str, excluded: TaskStatus) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let changed = inner.conn.execute(
            "DELETE FROM tasks WHERE id = ?1 AND status != ?2",
            params![id, excluded.as_str()],
        )?;
        if changed == 0 {
            return Ok(false);
        }
        inner.forget(id);
        let _ = self.events.send(WatchEvent::TaskDeleted {
            task_id: id.to_string(),
        });
        Ok(true)
    }

    pub fn retry_if(&self, id: &str, allowed: &[TaskStatus]) -> Result<Option<Task>, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let now = now_rfc3339();
        let allowed: Vec<&str> = allowed.iter().map(TaskStatus::as_str).collect();
        let allowed = serde_json::to_string(&allowed)?;
        let changed = inner.conn.execute(
            "UPDATE tasks
             SET status = 'queued',
                 position = (SELECT COALESCE(MAX(position), 0) + 1 FROM tasks),
                 updated_at = ?1
             WHERE id = ?2 AND status IN (SELECT value FROM json_each(?3))",
            params![now, id, allowed],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        inner.forget(id);
        let task = inner.get_existing(id)?;
        self.publish(&task);
        Ok(Some(task))
    }

    pub fn move_task(&self, id: &str, direction: MoveDirection) -> Result<Task, StoreError> {
        let inner = self.inner.lock().unwrap();
        let conn = &inner.conn;
        let mut stmt = conn.prepare(
            "SELECT id, position FROM tasks WHERE status IN ('queued', 'received') ORDER BY position ASC",
        )?;
        let rows: Vec<(String, i64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        drop(stmt);

        let idx = rows
            .iter()
            .position(|(row_id, _)| row_id == id)
            .ok_or_else(|| StoreError::Invalid(format!("task {id} is not queued or received")))?;

        let neighbor_idx = match direction {
            MoveDirection::Up => idx.checked_sub(1),
            MoveDirection::Down => (idx + 1 < rows.len()).then_some(idx + 1),
        };

        if let Some(neighbor_idx) = neighbor_idx {
            let (this_id, this_pos) = rows[idx].clone();
            let (neighbor_id, neighbor_pos) = rows[neighbor_idx].clone();
            let now = now_rfc3339();
            conn.execute(
                "UPDATE tasks SET position = ?1, updated_at = ?2 WHERE id = ?3",
                params![neighbor_pos, now, this_id],
            )?;
            conn.execute(
                "UPDATE tasks SET position = ?1, updated_at = ?2 WHERE id = ?3",
                params![this_pos, now, neighbor_id],
            )?;
            self.publish(&inner.get_existing(&neighbor_id)?);
            let task = inner.get_existing(&this_id)?;
            self.publish(&task);
            return Ok(task);
        }

        inner.get_existing(id)
    }

    pub fn mark_all_running_interrupted(&self) -> Result<usize, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner.attention.clear();
        inner.turns.clear();
        let now = now_rfc3339();
        let changed = inner.conn.execute(
            "UPDATE tasks SET status = 'interrupted', updated_at = ?1 WHERE status = 'running'",
            params![now],
        )?;
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(workspace: &str, status: TaskStatus) -> NewTask {
        NewTask {
            text: format!("task for {workspace}"),
            workspace: workspace.to_string(),
            agent: None,
            source: Source::cli(),
            reply_to: None,
            metadata: serde_json::json!({}),
            status,
        }
    }

    #[test]
    fn insert_assigns_ulid_and_increasing_position() {
        let store = Store::open_in_memory().unwrap();
        let t1 = store.insert(sample("a", TaskStatus::Queued)).unwrap();
        let t2 = store.insert(sample("a", TaskStatus::Queued)).unwrap();
        assert_ne!(t1.id, t2.id);
        assert!(t2.position > t1.position);
        assert_eq!(t1.status, TaskStatus::Queued);
        assert!(!t1.created_at.is_empty());
        assert_eq!(t1.created_at, t1.updated_at);
    }

    #[test]
    fn update_status_bumps_updated_at() {
        let store = Store::open_in_memory().unwrap();
        let t1 = store.insert(sample("a", TaskStatus::Queued)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let updated = store
            .update_status_if(&t1.id, TaskStatus::Queued, TaskStatus::Running)
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, TaskStatus::Running);
        assert_ne!(updated.updated_at, updated.created_at);
    }

    #[test]
    fn delete_unless_skips_task_that_became_running() {
        let store = Store::open_in_memory().unwrap();
        let t1 = store.insert(sample("a", TaskStatus::Queued)).unwrap();
        store
            .update_status_if(&t1.id, TaskStatus::Queued, TaskStatus::Running)
            .unwrap();
        assert!(!store.delete_unless(&t1.id, TaskStatus::Running).unwrap());
        assert!(store.get(&t1.id).unwrap().is_some());

        store
            .update_status_if(&t1.id, TaskStatus::Running, TaskStatus::Done)
            .unwrap();
        assert!(store.delete_unless(&t1.id, TaskStatus::Running).unwrap());
        assert!(store.get(&t1.id).unwrap().is_none());
        assert!(!store.delete_unless(&t1.id, TaskStatus::Running).unwrap());
    }

    #[test]
    fn update_text_unless_skips_task_that_became_running() {
        let store = Store::open_in_memory().unwrap();
        let t1 = store.insert(sample("a", TaskStatus::Queued)).unwrap();
        store
            .update_status_if(&t1.id, TaskStatus::Queued, TaskStatus::Running)
            .unwrap();
        assert!(
            store
                .update_text_unless(&t1.id, "new", TaskStatus::Running)
                .unwrap()
                .is_none()
        );
        assert_eq!(store.get(&t1.id).unwrap().unwrap().text, t1.text);

        store
            .update_status_if(&t1.id, TaskStatus::Running, TaskStatus::Interrupted)
            .unwrap();
        let edited = store
            .update_text_unless(&t1.id, "new", TaskStatus::Running)
            .unwrap()
            .unwrap();
        assert_eq!(edited.text, "new");
    }

    #[test]
    fn accept_via_update_status_if_skips_task_cancelled_in_between() {
        let store = Store::open_in_memory().unwrap();
        let t1 = store.insert(sample("a", TaskStatus::Received)).unwrap();
        store
            .update_status_if(&t1.id, TaskStatus::Received, TaskStatus::Cancelled)
            .unwrap();
        assert!(
            store
                .update_status_if(&t1.id, TaskStatus::Received, TaskStatus::Queued)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store.get(&t1.id).unwrap().unwrap().status,
            TaskStatus::Cancelled
        );
    }

    #[test]
    fn move_swaps_position_among_queued_and_received_only() {
        let store = Store::open_in_memory().unwrap();
        let t1 = store.insert(sample("a", TaskStatus::Queued)).unwrap();
        let t2 = store.insert(sample("a", TaskStatus::Queued)).unwrap();
        let t3 = store.insert(sample("a", TaskStatus::Queued)).unwrap();

        let moved = store.move_task(&t2.id, MoveDirection::Up).unwrap();
        assert_eq!(moved.position, t1.position);
        let first = store.get(&t1.id).unwrap().unwrap();
        assert_eq!(first.position, t2.position);

        let unchanged = store.get(&t3.id).unwrap().unwrap();
        assert_eq!(unchanged.position, t3.position);
    }

    #[test]
    fn move_at_boundary_is_noop() {
        let store = Store::open_in_memory().unwrap();
        let t1 = store.insert(sample("a", TaskStatus::Queued)).unwrap();
        let moved = store.move_task(&t1.id, MoveDirection::Up).unwrap();
        assert_eq!(moved.position, t1.position);
    }

    #[test]
    fn retry_moves_task_to_end_of_queue() {
        let store = Store::open_in_memory().unwrap();
        let t1 = store.insert(sample("a", TaskStatus::Queued)).unwrap();
        let t2 = store.insert(sample("a", TaskStatus::Failed)).unwrap();
        let t3 = store.insert(sample("a", TaskStatus::Queued)).unwrap();

        let retried = store
            .retry_if(&t2.id, &[TaskStatus::Failed])
            .unwrap()
            .unwrap();
        assert_eq!(retried.status, TaskStatus::Queued);
        assert!(retried.position > t1.position);
        assert!(retried.position > t3.position);
    }

    #[test]
    fn retry_if_skips_task_whose_status_changed_in_between() {
        let store = Store::open_in_memory().unwrap();
        let t1 = store.insert(sample("a", TaskStatus::Failed)).unwrap();
        store.insert(sample("a", TaskStatus::Queued)).unwrap();
        let allowed = [
            TaskStatus::Interrupted,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ];
        let first = store.retry_if(&t1.id, &allowed).unwrap().unwrap();
        assert_eq!(first.status, TaskStatus::Queued);
        assert!(store.retry_if(&t1.id, &allowed).unwrap().is_none());
        let unchanged = store.get(&t1.id).unwrap().unwrap();
        assert_eq!(unchanged.position, first.position);
        assert!(store.retry_if("missing", &allowed).unwrap().is_none());
    }

    #[test]
    fn mark_all_running_interrupted_only_touches_running() {
        let store = Store::open_in_memory().unwrap();
        let t1 = store.insert(sample("a", TaskStatus::Running)).unwrap();
        let t2 = store.insert(sample("a", TaskStatus::Queued)).unwrap();
        let count = store.mark_all_running_interrupted().unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            store.get(&t1.id).unwrap().unwrap().status,
            TaskStatus::Interrupted
        );
        assert_eq!(
            store.get(&t2.id).unwrap().unwrap().status,
            TaskStatus::Queued
        );
    }

    #[test]
    fn update_status_if_only_applies_when_expected_matches() {
        let store = Store::open_in_memory().unwrap();
        let t1 = store.insert(sample("a", TaskStatus::Running)).unwrap();

        let missed = store
            .update_status_if(&t1.id, TaskStatus::Queued, TaskStatus::Interrupted)
            .unwrap();
        assert!(missed.is_none());
        assert_eq!(
            store.get(&t1.id).unwrap().unwrap().status,
            TaskStatus::Running
        );

        let hit = store
            .update_status_if(&t1.id, TaskStatus::Running, TaskStatus::Interrupted)
            .unwrap();
        assert_eq!(hit.unwrap().status, TaskStatus::Interrupted);
    }

    #[test]
    fn list_by_status_filters_and_orders() {
        let store = Store::open_in_memory().unwrap();
        store.insert(sample("a", TaskStatus::Queued)).unwrap();
        store.insert(sample("b", TaskStatus::Running)).unwrap();
        store.insert(sample("a", TaskStatus::Queued)).unwrap();

        let queued = store.list_by_status(TaskStatus::Queued).unwrap();
        assert_eq!(queued.len(), 2);
        assert!(queued[0].position < queued[1].position);
    }
}
