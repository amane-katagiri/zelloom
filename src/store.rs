use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

use crate::protocol::{MoveDirection, Source, Task, TaskStatus};

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

pub struct Store {
    conn: Mutex<Connection>,
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

fn now_rfc3339() -> String {
    jiff::Zoned::now()
        .strftime("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

impl Store {
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> Result<Store, StoreError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    pub fn insert(&self, new_task: NewTask) -> Result<Task, StoreError> {
        let conn = self.conn.lock().unwrap();
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
        get_locked(&conn, &id)?.ok_or(StoreError::NotFound(id))
    }

    pub fn get(&self, id: &str) -> Result<Option<Task>, StoreError> {
        let conn = self.conn.lock().unwrap();
        get_locked(&conn, id)
    }

    pub fn list_all(&self) -> Result<Vec<Task>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {SELECT_COLUMNS} FROM tasks ORDER BY position ASC"
        ))?;
        let rows = stmt.query_map([], map_row)?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(to_task(row?)?);
        }
        Ok(tasks)
    }

    pub fn list_by_status(&self, status: TaskStatus) -> Result<Vec<Task>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {SELECT_COLUMNS} FROM tasks WHERE status = ?1 ORDER BY position ASC"
        ))?;
        let rows = stmt.query_map(params![status.as_str()], map_row)?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(to_task(row?)?);
        }
        Ok(tasks)
    }

    pub fn update_status_if(
        &self,
        id: &str,
        expected: TaskStatus,
        status: TaskStatus,
    ) -> Result<Option<Task>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let now = now_rfc3339();
        let changed = conn.execute(
            "UPDATE tasks SET status = ?1, updated_at = ?2 WHERE id = ?3 AND status = ?4",
            params![status.as_str(), now, id, expected.as_str()],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        get_locked(&conn, id)?
            .ok_or_else(|| StoreError::NotFound(id.to_string()))
            .map(Some)
    }

    pub fn update_text_unless(
        &self,
        id: &str,
        text: &str,
        excluded: TaskStatus,
    ) -> Result<Option<Task>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let now = now_rfc3339();
        let changed = conn.execute(
            "UPDATE tasks SET text = ?1, updated_at = ?2 WHERE id = ?3 AND status != ?4",
            params![text, now, id, excluded.as_str()],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        get_locked(&conn, id)
    }

    pub fn delete_unless(&self, id: &str, excluded: TaskStatus) -> Result<bool, StoreError> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "DELETE FROM tasks WHERE id = ?1 AND status != ?2",
            params![id, excluded.as_str()],
        )?;
        Ok(changed > 0)
    }

    pub fn retry_if(&self, id: &str, allowed: &[TaskStatus]) -> Result<Option<Task>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let now = now_rfc3339();
        let allowed: Vec<&str> = allowed.iter().map(TaskStatus::as_str).collect();
        let allowed = serde_json::to_string(&allowed)?;
        let changed = conn.execute(
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
        get_locked(&conn, id)
    }

    pub fn move_task(&self, id: &str, direction: MoveDirection) -> Result<Task, StoreError> {
        let conn = self.conn.lock().unwrap();
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
        }

        get_locked(&conn, id)?.ok_or_else(|| StoreError::NotFound(id.to_string()))
    }

    pub fn mark_all_running_interrupted(&self) -> Result<usize, StoreError> {
        let conn = self.conn.lock().unwrap();
        let now = now_rfc3339();
        let changed = conn.execute(
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
