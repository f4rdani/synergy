use anyhow::Result;
use rusqlite::{params, Connection, Row};
use std::path::Path;
use chrono::{DateTime, Utc};
use synergy_proto::{Task, TaskStatus, EventLog};

/// Lightweight session metadata for listing/restoring.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub project_id: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub leader_app: String,
    pub worker_count: u32,
}

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path)?;
        let db = Database { conn };
        db.initialize_schema()?;
        Ok(db)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Database { conn };
        db.initialize_schema()?;
        Ok(db)
    }

    fn initialize_schema(&self) -> Result<()> {
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS project (
              id          TEXT PRIMARY KEY,
              name        TEXT NOT NULL,
              root_path   TEXT NOT NULL,
              created_at  TEXT NOT NULL
            );",
            [],
        )?;

        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS session (
              id          TEXT PRIMARY KEY,
              project_id  TEXT NOT NULL REFERENCES project(id),
              started_at  TEXT NOT NULL,
              ended_at    TEXT,
              leader_app  TEXT NOT NULL,
              worker_count INTEGER NOT NULL DEFAULT 6
            );",
            [],
        )?;

        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS task (
              id          TEXT PRIMARY KEY,
              session_id  TEXT NOT NULL REFERENCES session(id),
              title       TEXT NOT NULL,
              instruction TEXT NOT NULL,
              status      TEXT NOT NULL CHECK (status IN
                ('pending','blocked','running','done','failed','escalated')),
              worker_id   INTEGER,
              depends_on  TEXT,
              files_target TEXT,
              attempt     INTEGER NOT NULL DEFAULT 0,
              created_at  TEXT NOT NULL,
              started_at  TEXT,
              ended_at    TEXT
            );",
            [],
        )?;

        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS worker (
              id          INTEGER PRIMARY KEY,
              session_id  TEXT NOT NULL REFERENCES session(id),
              proxy_addr  TEXT,
              proxy_label TEXT,
              status      TEXT NOT NULL CHECK (status IN ('idle','working','error','offline')),
              pid         INTEGER
            );",
            [],
        )?;

        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS event_log (
              id          INTEGER PRIMARY KEY AUTOINCREMENT,
              session_id  TEXT NOT NULL,
              ts          TEXT NOT NULL,
              source      TEXT NOT NULL,
              type        TEXT NOT NULL,
              payload     TEXT NOT NULL
            );",
            [],
        )?;

        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS proxy (
              id          INTEGER PRIMARY KEY AUTOINCREMENT,
              address     TEXT NOT NULL,
              label       TEXT,
              healthy     INTEGER NOT NULL DEFAULT 1,
              last_check  TEXT
            );",
            [],
        )?;

        Ok(())
    }

    pub fn insert_project(&self, id: &str, name: &str, root_path: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO project (id, name, root_path, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, name, root_path, now],
        )?;
        Ok(())
    }

    pub fn insert_session(&self, id: &str, project_id: &str, leader_app: &str, worker_count: u32) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO session (id, project_id, started_at, leader_app, worker_count) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, project_id, now, leader_app, worker_count],
        )?;
        Ok(())
    }

    pub fn end_session(&self, id: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE session SET ended_at = ?1 WHERE id = ?2",
            params![now, id],
        )?;
        Ok(())
    }

    /// Get all sessions for a project, ordered by most recent first.
    pub fn get_sessions(&self, project_id: &str) -> Result<Vec<SessionInfo>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, started_at, ended_at, leader_app, worker_count
             FROM session WHERE project_id = ?1 ORDER BY started_at DESC",
        )?;
        let rows = stmt.query_map(params![project_id], |row| {
            Ok(SessionInfo {
                id: row.get(0)?,
                project_id: row.get(1)?,
                started_at: row.get(2)?,
                ended_at: row.get(3)?,
                leader_app: row.get(4)?,
                worker_count: row.get(5)?,
            })
        })?;
        let mut list = Vec::new();
        for r in rows {
            list.push(r?);
        }
        Ok(list)
    }

    /// Get the most recent active (not ended) session for a project.
    pub fn get_active_session(&self, project_id: &str) -> Result<Option<SessionInfo>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, started_at, ended_at, leader_app, worker_count
             FROM session WHERE project_id = ?1 AND ended_at IS NULL
             ORDER BY started_at DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![project_id], |row| {
            Ok(SessionInfo {
                id: row.get(0)?,
                project_id: row.get(1)?,
                started_at: row.get(2)?,
                ended_at: row.get(3)?,
                leader_app: row.get(4)?,
                worker_count: row.get(5)?,
            })
        })?;
        match rows.next() {
            Some(Ok(s)) => Ok(Some(s)),
            _ => Ok(None),
        }
    }

    pub fn insert_task(&self, task: &Task) -> Result<()> {
        let status_str = match task.status {
            TaskStatus::Pending => "pending",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Running => "running",
            TaskStatus::Done => "done",
            TaskStatus::Failed => "failed",
            TaskStatus::Escalated => "escalated",
        };
        let depends_json = serde_json::to_string(&task.depends_on)?;
        let files_json = serde_json::to_string(&task.files_target)?;
        let created_at_str = task.created_at.to_rfc3339();
        let started_at_str = task.started_at.map(|dt| dt.to_rfc3339());
        let ended_at_str = task.ended_at.map(|dt| dt.to_rfc3339());

        self.conn.execute(
            "INSERT OR IGNORE INTO task (id, session_id, title, instruction, status, worker_id, depends_on, files_target, attempt, created_at, started_at, ended_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                task.id,
                task.session_id,
                task.title,
                task.instruction,
                status_str,
                task.worker_id,
                depends_json,
                files_json,
                task.attempt,
                created_at_str,
                started_at_str,
                ended_at_str
            ],
        )?;
        Ok(())
    }

    pub fn update_task_status(&self, id: &str, status: TaskStatus) -> Result<()> {
        let status_str = match status {
            TaskStatus::Pending => "pending",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Running => "running",
            TaskStatus::Done => "done",
            TaskStatus::Failed => "failed",
            TaskStatus::Escalated => "escalated",
        };
        let now = Utc::now().to_rfc3339();

        if status == TaskStatus::Running {
            self.conn.execute(
                "UPDATE task SET status = ?1, started_at = ?2 WHERE id = ?3",
                params![status_str, now, id],
            )?;
        } else if status == TaskStatus::Done || status == TaskStatus::Failed || status == TaskStatus::Escalated {
            self.conn.execute(
                "UPDATE task SET status = ?1, ended_at = ?2 WHERE id = ?3",
                params![status_str, now, id],
            )?;
        } else {
            self.conn.execute(
                "UPDATE task SET status = ?1 WHERE id = ?2",
                params![status_str, id],
            )?;
        }
        Ok(())
    }

    pub fn assign_task_to_worker(&self, task_id: &str, worker_id: u32) -> Result<()> {
        self.conn.execute(
            "UPDATE task SET worker_id = ?1 WHERE id = ?2",
            params![worker_id, task_id],
        )?;
        Ok(())
    }

    pub fn update_task_attempt(&self, task_id: &str, attempt: u32) -> Result<()> {
        self.conn.execute(
            "UPDATE task SET attempt = ?1 WHERE id = ?2",
            params![attempt, task_id],
        )?;
        Ok(())
    }

    /// Update the instruction text for a task (used when Leader sends fix feedback).
    pub fn update_task_instruction(&self, task_id: &str, instruction: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE task SET instruction = ?1 WHERE id = ?2",
            params![instruction, task_id],
        )?;
        Ok(())
    }

    pub fn insert_event_log(&self, log: &EventLog) -> Result<()> {
        let ts_str = log.ts.to_rfc3339();
        self.conn.execute(
            "INSERT INTO event_log (session_id, ts, source, type, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![log.session_id, ts_str, log.source, log.event_type, log.payload],
        )?;
        Ok(())
    }

    pub fn insert_worker(&self, id: u32, session_id: &str, proxy_addr: Option<&str>, proxy_label: Option<&str>, status: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO worker (id, session_id, proxy_addr, proxy_label, status) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, session_id, proxy_addr, proxy_label, status],
        )?;
        Ok(())
    }

    pub fn update_worker_status(&self, id: u32, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE worker SET status = ?1 WHERE id = ?2",
            params![status, id],
        )?;
        Ok(())
    }

    pub fn get_event_logs(&self, session_id: &str) -> Result<Vec<EventLog>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, ts, source, type, payload FROM event_log WHERE session_id = ?1 ORDER BY ts ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            let ts_str: String = row.get(2)?;
            let ts = DateTime::parse_from_rfc3339(&ts_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            Ok(EventLog {
                id: Some(row.get(0)?),
                session_id: row.get(1)?,
                ts,
                source: row.get(3)?,
                event_type: row.get(4)?,
                payload: row.get(5)?,
            })
        })?;

        let mut list = Vec::new();
        for r in rows {
            list.push(r?);
        }
        Ok(list)
    }

    pub fn get_all_tasks(&self, session_id: &str) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, title, instruction, status, worker_id, depends_on, files_target, attempt, created_at, started_at, ended_at
             FROM task WHERE session_id = ?1",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            self.map_row_to_task(row)
        })?;

        let mut list = Vec::new();
        for r in rows {
            list.push(r?);
        }
        Ok(list)
    }

    fn map_row_to_task(&self, row: &Row) -> rusqlite::Result<Task> {
        let status_str: String = row.get(4)?;
        let status = match status_str.as_str() {
            "blocked" => TaskStatus::Blocked,
            "running" => TaskStatus::Running,
            "done" => TaskStatus::Done,
            "failed" => TaskStatus::Failed,
            "escalated" => TaskStatus::Escalated,
            _ => TaskStatus::Pending,
        };

        let depends_json: String = row.get(6)?;
        let depends_on = serde_json::from_str(&depends_json).unwrap_or_default();

        let files_json: String = row.get(7)?;
        let files_target = serde_json::from_str(&files_json).unwrap_or_default();

        let created_at_str: String = row.get(9)?;
        let created_at = DateTime::parse_from_rfc3339(&created_at_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());

        let started_at_str: Option<String> = row.get(10)?;
        let started_at = started_at_str.and_then(|s| {
            DateTime::parse_from_rfc3339(&s)
                .map(|dt| dt.with_timezone(&Utc))
                .ok()
        });

        let ended_at_str: Option<String> = row.get(11)?;
        let ended_at = ended_at_str.and_then(|s| {
            DateTime::parse_from_rfc3339(&s)
                .map(|dt| dt.with_timezone(&Utc))
                .ok()
        });

        Ok(Task {
            id: row.get(0)?,
            session_id: row.get(1)?,
            title: row.get(2)?,
            instruction: row.get(3)?,
            status,
            worker_id: row.get(5)?,
            depends_on,
            files_target,
            attempt: row.get(8)?,
            created_at,
            started_at,
            ended_at,
        })
    }
}
