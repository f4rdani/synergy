use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub session_id: String,
    pub title: String,
    pub instruction: String,
    pub status: TaskStatus,
    pub worker_id: Option<u32>,
    pub depends_on: Vec<String>,
    pub files_target: Vec<String>,
    pub attempt: u32,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    Blocked,
    Running,
    Done,
    Failed,
    Escalated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub ts: DateTime<Utc>,
    pub from: String,
    pub to: String,
    pub payload: MessagePayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "PascalCase")]
pub enum MessagePayload {
    TaskAssign {
        task_id: String,
        instruction: String,
        files_target: Vec<String>,
        depends_on: Vec<String>,
    },
    TaskProgress {
        task_id: String,
        data: String,
    },
    TaskDone {
        task_id: String,
        result: String,
    },
    TaskFailed {
        task_id: String,
        error: String,
    },
    TaskReassign {
        task_id: String,
        worker_id: u32,
    },
    LeaderQuery {
        query: String,
    },
    LeaderResponse {
        response: String,
    },
    StatusUpdate {
        status: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventLog {
    pub id: Option<i64>,
    pub session_id: String,
    pub ts: DateTime<Utc>,
    pub source: String,
    pub event_type: String,
    pub payload: String,
}
