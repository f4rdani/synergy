//! Synergy orchestrator engine.
//!
//! This crate ties together the PTY layer, adapter trait, proxy manager,
//! and SQLite state store into a single "tick" loop that schedules
//! pending tasks onto idle workers, monitors their PTY output, and emits
//! events the UI can subscribe to via [`tokio::sync::broadcast`].
//!
//! The lib is intentionally async and `Send`-clean so it can be embedded
//! inside a Tauri app or driven from an integration test.

pub mod git;
pub mod leader;
pub mod session_flow;

use anyhow::Result;
use chrono::Utc;
use std::collections::HashSet;
use std::sync::Arc;
use synergy_adapter::{AppAdapter, AppHandle, AppStatus, LaunchConfig, OpenCodeAdapter};
use synergy_db::Database;
use synergy_proto::{EventLog, Task, TaskStatus};
use synergy_proxy::ProxyManager;
use tokio::sync::{broadcast, Mutex};

/// Maximum retry attempts before a task is escalated to the Leader.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// Streamed output snippet from a worker, suitable for live rendering in
/// the UI panel.
#[derive(Debug, Clone)]
pub struct WorkerOutput {
    pub worker_id: u32,
    pub task_id: Option<String>,
    pub chunk: String,
}

/// One running worker plus its in-memory state.
pub struct WorkerInstance {
    pub id: usize,
    pub handle: AppHandle,
    pub status: AppStatus,
    pub current_task: Option<Task>,
    pub output_buffer: String,
    pub proxy_addr: Option<String>,
}

/// Top-level engine. Wraps the DB, proxy manager, worker pool, and the
/// chosen worker adapter (defaults to [`OpenCodeAdapter`]).
pub struct Orchestrator {
    pub db: Arc<Mutex<Database>>,
    pub proxy_manager: Arc<ProxyManager>,
    pub workers: Arc<Mutex<Vec<WorkerInstance>>>,
    pub adapter: Arc<dyn AppAdapter>,
    pub session_id: String,
    pub max_attempts: u32,
    pub output_tx: broadcast::Sender<WorkerOutput>,
}

impl Orchestrator {
    /// Build an orchestrator that owns a fresh database handle.
    pub fn new(db: Database, proxy_manager: ProxyManager, session_id: String) -> Self {
        Self::with_shared_db(Arc::new(Mutex::new(db)), proxy_manager, session_id)
    }

    /// Build an orchestrator that shares an existing `Arc<Mutex<Database>>`
    /// with another caller (e.g. the UI process).
    pub fn with_shared_db(
        db: Arc<Mutex<Database>>,
        proxy_manager: ProxyManager,
        session_id: String,
    ) -> Self {
        let (output_tx, _) = broadcast::channel(256);
        Self {
            db,
            proxy_manager: Arc::new(proxy_manager),
            workers: Arc::new(Mutex::new(Vec::new())),
            adapter: Arc::new(OpenCodeAdapter),
            session_id,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            output_tx,
        }
    }

    /// Subscribe to streamed worker output (one chunk per PTY read).
    pub fn subscribe_outputs(&self) -> broadcast::Receiver<WorkerOutput> {
        self.output_tx.subscribe()
    }

    /// Replace the worker adapter (typically used in tests).
    pub fn with_adapter(mut self, adapter: Arc<dyn AppAdapter>) -> Self {
        self.adapter = adapter;
        self
    }

    /// Set the AI model for all workers by sending the `/model` command to each PTY.
    /// In OpenCode CLI, `/model <name>` switches the active model.
    pub async fn set_worker_model(&self, model: &str) -> Result<()> {
        if model.is_empty() {
            return Ok(());
        }
        let mut workers = self.workers.lock().await;
        for worker in workers.iter_mut() {
            self.adapter
                .send_command(&mut worker.handle, &format!("/model {}", model))
                .await?;
            // Give a moment for the model switch to complete
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        Ok(())
    }

    /// Spawn `count` workers with the configured adapter.
    pub async fn spawn_workers(
        &self,
        count: usize,
        bin_path: &str,
        cwd: Option<&str>,
    ) -> Result<()> {
        let mut workers = self.workers.lock().await;
        let db = self.db.lock().await;

        for i in 0..count {
            let proxy_addr = self.proxy_manager.get_proxy_for_worker(i).await;
            let config = LaunchConfig {
                bin_path: bin_path.to_owned(),
                args: Vec::new(),
                cwd: cwd.map(|s| s.to_owned()),
                proxy_addr: proxy_addr.clone(),
            };

            let handle = self.adapter.launch(&config).await?;

            db.insert_worker(
                i as u32,
                &self.session_id,
                proxy_addr.as_deref(),
                Some(&format!("Proxy {}", i)),
                "idle",
            )?;

            workers.push(WorkerInstance {
                id: i,
                handle,
                status: AppStatus::Idle,
                current_task: None,
                output_buffer: String::new(),
                proxy_addr,
            });
        }
        Ok(())
    }

    /// Drive one iteration of the event loop. Reads worker output, advances
    /// finished tasks, and assigns pending tasks to idle workers.
    pub async fn tick(&self) -> Result<()> {
        self.read_outputs().await?;
        self.schedule_tasks().await?;
        Ok(())
    }

    async fn read_outputs(&self) -> Result<()> {
        let mut workers = self.workers.lock().await;
        let db = self.db.lock().await;

        for worker in workers.iter_mut() {
            if worker.status != AppStatus::Working {
                continue;
            }

            let Some(output) = self.adapter.read_output(&mut worker.handle).await else {
                continue;
            };
            if output.is_empty() {
                continue;
            }

            // Always stream, even before parsing — the UI wants the raw
            // bytes so terminals look "live".
            let _ = self.output_tx.send(WorkerOutput {
                worker_id: worker.id as u32,
                task_id: worker.current_task.as_ref().map(|t| t.id.clone()),
                chunk: output.clone(),
            });

            worker.output_buffer.push_str(&output);

            if let Some(ref task) = worker.current_task {
                let log = EventLog {
                    id: None,
                    session_id: self.session_id.clone(),
                    ts: Utc::now(),
                    source: format!("worker-{}", worker.id),
                    event_type: "TaskProgress".to_owned(),
                    payload: trim_for_log(&output),
                };
                db.insert_event_log(&log)?;
                let _ = task; // explicit: just here to make borrow scope obvious
            }

            let new_status = self.adapter.detect_status(&worker.output_buffer).await;
            match new_status {
                AppStatus::Done | AppStatus::Idle => {
                    // For CLI adapters, returning to idle prompt means the
                    // task finished successfully (§9.4 of the spec).
                    // Require at least some output before declaring done to
                    // avoid false positives on the very first tick.
                    if worker.current_task.is_some() && worker.output_buffer.len() > 4 {
                        self.on_worker_done(&db, worker)?;
                    }
                }
                AppStatus::Error(err) => self.on_worker_error(&db, worker, err)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn on_worker_done(&self, db: &Database, worker: &mut WorkerInstance) -> Result<()> {
        if let Some(task) = worker.current_task.take() {
            db.update_task_status(&task.id, TaskStatus::Done)?;
            db.insert_event_log(&EventLog {
                id: None,
                session_id: self.session_id.clone(),
                ts: Utc::now(),
                source: "orchestrator".to_owned(),
                event_type: "TaskDone".to_owned(),
                payload: format!("Task {} completed on worker {}", task.id, worker.id),
            })?;

            // Immediately unblock tasks that depended on this one
            let all_tasks = db.get_all_tasks(&self.session_id)?;
            for blocked_task in all_tasks.iter().filter(|t| t.status == TaskStatus::Blocked) {
                if blocked_task.depends_on.contains(&task.id) {
                    // Check if ALL dependencies are now done
                    let all_deps_done = blocked_task.depends_on.iter().all(|dep_id| {
                        all_tasks
                            .iter()
                            .any(|t| t.id == *dep_id && t.status == TaskStatus::Done)
                    });
                    if all_deps_done {
                        db.update_task_status(&blocked_task.id, TaskStatus::Pending)?;
                        db.insert_event_log(&EventLog {
                            id: None,
                            session_id: self.session_id.clone(),
                            ts: Utc::now(),
                            source: "orchestrator".to_owned(),
                            event_type: "TaskUnblocked".to_owned(),
                            payload: format!(
                                "Task {} unblocked (dependency {} completed by worker {})",
                                blocked_task.id, task.id, worker.id
                            ),
                        })?;
                    }
                }
            }
        }
        worker.status = AppStatus::Idle;
        worker.output_buffer.clear();
        db.update_worker_status(worker.id as u32, "idle")?;
        Ok(())
    }

    fn on_worker_error(
        &self,
        db: &Database,
        worker: &mut WorkerInstance,
        err_msg: String,
    ) -> Result<()> {
        if let Some(mut task) = worker.current_task.take() {
            task.attempt += 1;
            db.update_task_attempt(&task.id, task.attempt)?;
            db.insert_event_log(&EventLog {
                id: None,
                session_id: self.session_id.clone(),
                ts: Utc::now(),
                source: "orchestrator".to_owned(),
                event_type: "TaskError".to_owned(),
                payload: format!(
                    "Task {} failed on worker {}: {} (attempt {}/{})",
                    task.id, worker.id, err_msg, task.attempt, self.max_attempts
                ),
            })?;

            let next_status = if task.attempt >= self.max_attempts {
                TaskStatus::Escalated
            } else if is_dependency_error(&err_msg) && !task.depends_on.is_empty() {
                // Looks like a missing file/dependency error - re-block so it retries
                // when the dependency truly completes
                db.insert_event_log(&EventLog {
                    id: None,
                    session_id: self.session_id.clone(),
                    ts: Utc::now(),
                    source: "orchestrator".to_owned(),
                    event_type: "TaskReblocked".to_owned(),
                    payload: format!(
                        "Task {} re-blocked due to dependency error (will auto-retry when deps are ready)",
                        task.id
                    ),
                })?;
                TaskStatus::Blocked
            } else {
                TaskStatus::Pending
            };
            db.update_task_status(&task.id, next_status)?;
        }
        worker.status = AppStatus::Idle;
        worker.output_buffer.clear();
        db.update_worker_status(worker.id as u32, "idle")?;
        Ok(())
    }

    async fn schedule_tasks(&self) -> Result<()> {
        let mut workers = self.workers.lock().await;
        let db = self.db.lock().await;

        let mut idle_indices: Vec<usize> = workers
            .iter()
            .enumerate()
            .filter(|(_, w)| w.status == AppStatus::Idle)
            .map(|(idx, _)| idx)
            .collect();

        if idle_indices.is_empty() {
            return Ok(());
        }

        let all_tasks = db.get_all_tasks(&self.session_id)?;
        let done_ids: HashSet<String> = all_tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Done)
            .map(|t| t.id.clone())
            .collect();

        // Files currently locked by running workers.
        let mut locked_files: HashSet<String> = workers
            .iter()
            .filter_map(|w| w.current_task.as_ref())
            .flat_map(|t| t.files_target.iter().cloned())
            .collect();

        for task in all_tasks.iter() {
            if !matches!(task.status, TaskStatus::Pending | TaskStatus::Blocked) {
                continue;
            }

            // Dependency check.
            let deps_ok = task.depends_on.iter().all(|d| done_ids.contains(d));
            if !deps_ok {
                if task.status != TaskStatus::Blocked {
                    db.update_task_status(&task.id, TaskStatus::Blocked)?;
                    // Log which dependencies are still pending
                    let pending_deps: Vec<&String> = task
                        .depends_on
                        .iter()
                        .filter(|d| !done_ids.contains(*d))
                        .collect();
                    db.insert_event_log(&EventLog {
                        id: None,
                        session_id: self.session_id.clone(),
                        ts: Utc::now(),
                        source: "orchestrator".to_owned(),
                        event_type: "TaskWaiting".to_owned(),
                        payload: format!(
                            "Task {} waiting for: {}",
                            task.id,
                            pending_deps
                                .iter()
                                .map(|d| d.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    })?;
                }
                continue;
            }

            // File lock check.
            if task.files_target.iter().any(|f| locked_files.contains(f)) {
                continue;
            }

            let Some(worker_idx) = idle_indices.pop() else {
                break;
            };

            for f in &task.files_target {
                locked_files.insert(f.clone());
            }

            let worker = &mut workers[worker_idx];

            db.assign_task_to_worker(&task.id, worker.id as u32)?;
            db.update_task_status(&task.id, TaskStatus::Running)?;
            db.update_worker_status(worker.id as u32, "working")?;

            // Enrich instruction with dependency context so worker AI knows what files exist
            let enriched_instruction = if !task.depends_on.is_empty() {
                let dep_context: Vec<String> = task
                    .depends_on
                    .iter()
                    .filter_map(|dep_id| {
                        all_tasks
                            .iter()
                            .find(|t| t.id == *dep_id && t.status == TaskStatus::Done)
                    })
                    .map(|dep_task| {
                        let files = if dep_task.files_target.is_empty() {
                            "various files".to_owned()
                        } else {
                            dep_task.files_target.join(", ")
                        };
                        format!(
                            "- Completed: \"{}\" (files created: {})",
                            dep_task.title, files
                        )
                    })
                    .collect();

                if dep_context.is_empty() {
                    task.instruction.clone()
                } else {
                    format!(
                        "{}\n\n[CONTEXT: The following prerequisite tasks are already completed. Their output files exist in the project directory:]\n{}",
                        task.instruction,
                        dep_context.join("\n")
                    )
                }
            } else {
                task.instruction.clone()
            };

            self.adapter
                .send_command(&mut worker.handle, &enriched_instruction)
                .await?;

            db.insert_event_log(&EventLog {
                id: None,
                session_id: self.session_id.clone(),
                ts: Utc::now(),
                source: "orchestrator".to_owned(),
                event_type: "TaskAssigned".to_owned(),
                payload: format!("Task {} assigned to worker {}", task.id, worker.id),
            })?;

            let mut task = task.clone();
            task.status = TaskStatus::Running;
            task.worker_id = Some(worker.id as u32);
            task.started_at = Some(Utc::now());
            worker.status = AppStatus::Working;
            worker.output_buffer.clear();
            worker.current_task = Some(task);
        }

        Ok(())
    }
}

/// Truncate long PTY chunks before storing in the event log so the SQLite
/// row stays small.
fn trim_for_log(s: &str) -> String {
    const MAX: usize = 1024;
    if s.len() <= MAX {
        s.to_owned()
    } else {
        let mut t = s[..MAX].to_owned();
        t.push_str("…(truncated)");
        t
    }
}

/// Detect if an error message is likely caused by a missing dependency/file
/// that another worker hasn't created yet.
fn is_dependency_error(err: &str) -> bool {
    let lower = err.to_lowercase();
    lower.contains("not found")
        || lower.contains("no such file")
        || lower.contains("cannot find")
        || lower.contains("does not exist")
        || lower.contains("module not found")
        || lower.contains("import error")
        || lower.contains("could not resolve")
        || lower.contains("cannot resolve")
        || lower.contains("file not found")
        || lower.contains("enoent")
        || lower.contains("class not found")
        || lower.contains("namespace")
}

#[cfg(test)]
mod tests {
    use super::*;
    use synergy_adapter::{AppAdapter, AppHandle, AppStatus, AppType, LaunchConfig};

    struct StubAdapter {
        next_status: AppStatus,
    }

    #[async_trait::async_trait]
    impl AppAdapter for StubAdapter {
        fn id(&self) -> &str {
            "stub"
        }
        fn display_name(&self) -> &str {
            "Stub"
        }
        fn app_type(&self) -> AppType {
            AppType::Cli
        }
        async fn launch(&self, _: &LaunchConfig) -> Result<AppHandle> {
            Ok(AppHandle {
                pty_session: None,
                window_hwnd: None,
            })
        }
        async fn send_command(&self, _: &mut AppHandle, _: &str) -> Result<()> {
            Ok(())
        }
        async fn read_output(&self, _: &mut AppHandle) -> Option<String> {
            None
        }
        async fn detect_status(&self, _: &str) -> AppStatus {
            self.next_status.clone()
        }
    }

    fn task(id: &str, deps: &[&str], files: &[&str]) -> Task {
        Task {
            id: id.to_owned(),
            session_id: "s1".to_owned(),
            title: id.to_owned(),
            instruction: format!("do {id}"),
            status: TaskStatus::Pending,
            worker_id: None,
            depends_on: deps.iter().map(|s| (*s).to_owned()).collect(),
            files_target: files.iter().map(|s| (*s).to_owned()).collect(),
            attempt: 0,
            created_at: Utc::now(),
            started_at: None,
            ended_at: None,
        }
    }

    async fn orch(adapter: AppStatus) -> Orchestrator {
        let db = Database::open_in_memory().unwrap();
        db.insert_project("p1", "P", ".").unwrap();
        db.insert_session("s1", "p1", "stub", 1).unwrap();
        let pm = ProxyManager::new(vec![]);
        let mut o = Orchestrator::new(db, pm, "s1".to_owned());
        o.adapter = Arc::new(StubAdapter {
            next_status: adapter,
        });
        o
    }

    fn push_worker(o: &mut Orchestrator, id: usize) {
        // direct push since spawn_workers needs a real binary
        let mut workers = o.workers.try_lock().expect("not locked");
        workers.push(WorkerInstance {
            id,
            handle: AppHandle {
                pty_session: None,
                window_hwnd: None,
            },
            status: AppStatus::Idle,
            current_task: None,
            output_buffer: String::new(),
            proxy_addr: None,
        });
    }

    #[tokio::test]
    async fn schedules_independent_task() {
        let mut o = orch(AppStatus::Idle).await;
        push_worker(&mut o, 0);
        {
            let db = o.db.lock().await;
            db.insert_task(&task("t1", &[], &["src/a.rs"])).unwrap();
        }

        o.schedule_tasks().await.unwrap();

        let workers = o.workers.lock().await;
        assert_eq!(workers[0].status, AppStatus::Working);
        assert_eq!(workers[0].current_task.as_ref().unwrap().id, "t1");
    }

    #[tokio::test]
    async fn dependency_blocks_then_releases() {
        let mut o = orch(AppStatus::Idle).await;
        push_worker(&mut o, 0);
        push_worker(&mut o, 1);
        {
            let db = o.db.lock().await;
            db.insert_task(&task("t1", &[], &[])).unwrap();
            db.insert_task(&task("t2", &["t1"], &[])).unwrap();
        }

        o.schedule_tasks().await.unwrap();

        // t1 on a worker, t2 still blocked.
        {
            let db = o.db.lock().await;
            let tasks = db.get_all_tasks("s1").unwrap();
            let t2 = tasks.iter().find(|t| t.id == "t2").unwrap();
            assert_eq!(t2.status, TaskStatus::Blocked);
        }

        // Mark t1 as done manually and re-schedule.
        {
            let db = o.db.lock().await;
            db.update_task_status("t1", TaskStatus::Done).unwrap();
        }
        o.schedule_tasks().await.unwrap();

        let db = o.db.lock().await;
        let tasks = db.get_all_tasks("s1").unwrap();
        let t2 = tasks.iter().find(|t| t.id == "t2").unwrap();
        assert_eq!(t2.status, TaskStatus::Running);
    }

    #[tokio::test]
    async fn file_lock_prevents_concurrent_edit() {
        let mut o = orch(AppStatus::Idle).await;
        push_worker(&mut o, 0);
        push_worker(&mut o, 1);
        {
            let db = o.db.lock().await;
            db.insert_task(&task("t1", &[], &["src/x.rs"])).unwrap();
            db.insert_task(&task("t2", &[], &["src/x.rs"])).unwrap();
        }

        o.schedule_tasks().await.unwrap();

        let db = o.db.lock().await;
        let tasks = db.get_all_tasks("s1").unwrap();
        let running: Vec<_> = tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Running)
            .collect();
        assert_eq!(
            running.len(),
            1,
            "only one task should run when files clash"
        );
    }

    #[tokio::test]
    async fn error_increments_attempt_and_repends() {
        let o = orch(AppStatus::Idle).await;

        // Set a working stub then mutate to error path.
        let working = Arc::new(StubAdapter {
            next_status: AppStatus::Error("boom".to_owned()),
        });
        let mut o = o.with_adapter(working);

        push_worker(&mut o, 0);
        {
            let db = o.db.lock().await;
            db.insert_task(&task("t1", &[], &[])).unwrap();
        }
        o.schedule_tasks().await.unwrap();

        // Manually transition to working then trigger error path through
        // a synthesized output read.
        {
            let mut workers = o.workers.lock().await;
            workers[0].status = AppStatus::Working;
            workers[0].output_buffer.push_str("Error: explosion");
            let db = o.db.lock().await;
            o.on_worker_error(&db, &mut workers[0], "boom".to_owned())
                .unwrap();
        }

        let db = o.db.lock().await;
        let tasks = db.get_all_tasks("s1").unwrap();
        let t1 = tasks.iter().find(|t| t.id == "t1").unwrap();
        assert_eq!(t1.attempt, 1);
        assert_eq!(t1.status, TaskStatus::Pending);
    }
}
