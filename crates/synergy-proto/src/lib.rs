use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ─── Session Flow Types ─────────────────────────────────────────────────────

/// Represents the current phase of the session flow state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Idle,
    FolderSelected,
    LeaderChosen,
    Planning,
    PlanApproved,
    Executing,
    Reviewing,
    Complete,
}

impl Default for SessionPhase {
    fn default() -> Self {
        Self::Idle
    }
}

/// Snapshot of the session flow state, queryable by the frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFlowState {
    pub phase: SessionPhase,
    pub project_dir: Option<String>,
    pub leader_adapter_id: Option<String>,
    pub session_id: Option<String>,
    pub plan_text: Option<String>,
    pub task_count: u32,
}

impl Default for SessionFlowState {
    fn default() -> Self {
        Self {
            phase: SessionPhase::Idle,
            project_dir: None,
            leader_adapter_id: None,
            session_id: None,
            plan_text: None,
            task_count: 0,
        }
    }
}

/// Metadata about a supported adapter, parsed from adapters.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterInfo {
    pub id: String,
    #[serde(rename = "type")]
    pub adapter_type: String,
    pub bin: String,
    pub desc: String,
}

// ─── Task & Message Types ───────────────────────────────────────────────────

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
