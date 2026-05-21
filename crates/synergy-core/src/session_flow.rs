//! Session flow controller -- manages the full state machine driving a
//! Synergy session from folder selection through leader interaction, plan
//! approval, worker execution, and review.
//!
//! The controller holds:
//! * current [`SessionPhase`] and related metadata
//! * the leader adapter + PTY handle for communicating with the Leader AI
//! * references to the DB and orchestrator once created
//!
//! This module is the backbone for connecting ANY Leader AI (CLI-based like
//! opencode/aider/claude-cli/codex-cli, or API-direct) to the orchestration
//! engine.

use std::sync::Arc;
use synergy_adapter::{AppAdapter, AppHandle as AdapterAppHandle};
use synergy_proto::{SessionFlowState, SessionPhase};
use tokio::sync::Mutex;

/// Controller that manages the session flow state machine.
///
/// Each Synergy session progresses through phases:
/// `Idle -> FolderSelected -> LeaderChosen -> Planning -> PlanApproved -> Executing -> Reviewing -> Complete`
pub struct SessionFlowController {
    pub phase: SessionPhase,
    pub project_dir: Option<String>,
    pub leader_adapter_id: Option<String>,
    pub session_id: Option<String>,
    pub plan_text: Option<String>,
    pub task_count: u32,
    /// The PTY/connection handle to the Leader AI process.
    pub leader_handle: Option<AdapterAppHandle>,
    /// The adapter used for communicating with the Leader.
    pub leader_adapter: Option<Arc<dyn AppAdapter>>,
    /// Buffer accumulating leader output for plan detection.
    pub leader_output_buffer: String,
}

impl Default for SessionFlowController {
    fn default() -> Self {
        Self {
            phase: SessionPhase::Idle,
            project_dir: None,
            leader_adapter_id: None,
            session_id: None,
            plan_text: None,
            task_count: 0,
            leader_handle: None,
            leader_adapter: None,
            leader_output_buffer: String::new(),
        }
    }
}

impl SessionFlowController {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a snapshot of the current state for the frontend.
    pub fn snapshot(&self) -> SessionFlowState {
        SessionFlowState {
            phase: self.phase,
            project_dir: self.project_dir.clone(),
            leader_adapter_id: self.leader_adapter_id.clone(),
            session_id: self.session_id.clone(),
            plan_text: self.plan_text.clone(),
            task_count: self.task_count,
        }
    }

    /// Advance to FolderSelected after the user picks a project directory.
    pub fn select_folder(&mut self, path: String) -> Result<(), String> {
        if self.phase != SessionPhase::Idle && self.phase != SessionPhase::FolderSelected {
            return Err(format!("Cannot select folder in phase {:?}", self.phase));
        }
        self.project_dir = Some(path);
        self.phase = SessionPhase::FolderSelected;
        Ok(())
    }

    /// Advance to LeaderChosen after spawning the Leader process.
    ///
    /// The caller is responsible for spawning the leader (via adapter.launch)
    /// and passing the handle in. This keeps the controller synchronous and
    /// testable.
    pub fn set_leader(
        &mut self,
        adapter_id: String,
        adapter: Arc<dyn AppAdapter>,
        handle: AdapterAppHandle,
        session_id: String,
    ) -> Result<(), String> {
        // Allow choosing a new leader even if we've already chosen one
        if self.phase != SessionPhase::FolderSelected && self.phase != SessionPhase::LeaderChosen && self.phase != SessionPhase::Executing {
            // Just warn instead of hard erroring to prevent UI lockups
            eprintln!("Warning: Choosing leader in unexpected phase {:?}", self.phase);
        }
        self.leader_adapter_id = Some(adapter_id);
        self.leader_adapter = Some(adapter);
        self.leader_handle = Some(handle);
        self.session_id = Some(session_id);
        self.phase = SessionPhase::LeaderChosen;
        Ok(())
    }

    /// Advance to Planning once the user sends the first message to the Leader.
    pub fn advance_to_planning(&mut self) {
        if self.phase == SessionPhase::LeaderChosen {
            self.phase = SessionPhase::Planning;
        }
    }

    /// Store the approved plan and advance to PlanApproved.
    pub fn approve_plan(&mut self, plan_text: String, task_count: u32) {
        self.plan_text = Some(plan_text);
        self.task_count = task_count;
        self.phase = SessionPhase::PlanApproved;
    }

    /// Advance to Executing after workers are spawned.
    pub fn advance_to_executing(&mut self) {
        self.phase = SessionPhase::Executing;
    }

    /// Advance to Reviewing when all tasks are complete and batch report is sent.
    pub fn advance_to_reviewing(&mut self) {
        self.phase = SessionPhase::Reviewing;
    }

    /// Advance to Complete when the Leader approves the batch report.
    pub fn advance_to_complete(&mut self) {
        self.phase = SessionPhase::Complete;
    }

    /// Reset back to Idle for a new session.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Send a message to the Leader AI via the adapter.
    pub async fn send_to_leader(&mut self, message: &str) -> Result<(), String> {
        let adapter = self
            .leader_adapter
            .as_ref()
            .ok_or("Leader adapter not initialized")?;
        let handle = self
            .leader_handle
            .as_mut()
            .ok_or("Leader handle not initialized")?;

        adapter
            .send_command(handle, message)
            .await
            .map_err(|e| format!("{e:#}"))
    }

    /// Read output from the Leader AI (non-blocking, returns None if nothing available).
    pub async fn read_leader_output(&mut self) -> Option<String> {
        let adapter = self.leader_adapter.as_ref()?;
        let handle = self.leader_handle.as_mut()?;
        let output = adapter.read_output(handle).await?;
        if !output.is_empty() {
            self.leader_output_buffer.push_str(&output);
        }
        Some(output)
    }
}

/// Spawn a background task that continuously reads the Leader's PTY output
/// and emits Tauri events for real-time display in the frontend.
///
/// This is the "leader output pump" -- analogous to the worker output pump
/// but for the Leader AI connection.
pub fn spawn_leader_output_pump(
    session_flow: Arc<Mutex<SessionFlowController>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let mut flow = session_flow.lock().await;
            // Stop pumping if session is complete or no leader is connected
            if flow.phase == SessionPhase::Complete || flow.leader_handle.is_none() {
                break;
            }
            // Try to read output; we just accumulate it in the buffer.
            // The actual Tauri event emission is done by the caller that
            // wraps this with access to the AppHandle.
            let _output = flow.read_leader_output().await;
        }
    })
}
