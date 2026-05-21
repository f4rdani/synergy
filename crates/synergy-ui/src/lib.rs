//! Tauri command surface and state for the Synergy desktop shell.
//!
//! The UI process owns:
//! * the [`Database`] connection (single writer, reads through a tokio Mutex)
//! * the running [`Orchestrator`] with its workers and ticker task
//! * the [`SessionFlowController`] managing the Leader AI connection and
//!   session phase progression
//!
//! Commands are intentionally chunky (init/get/add) so the JS frontend can
//! poll a single endpoint to refresh the dashboard, while a parallel Tauri
//! event stream pushes worker terminal output for real-time rendering.
//!
//! The Leader AI is connectable to ANY provider or application:
//! - CLI-based Leaders (opencode, aider, claude-cli, codex-cli): spawned as
//!   PTY processes, user sends messages via stdin, reads responses via stdout
//! - GUI-based Leaders (cursor, kiro, windsurf): Phase 2 placeholder
//! - API-direct Leaders: use HTTP API calls directly (Anthropic, OpenAI, etc.)

use anyhow::Context;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use synergy_adapter::{
    AppAdapter, AppStatus, GenericCliAdapter, LaunchConfig, OpenCodeAdapter,
};
use synergy_config::SynergyConfig;
use synergy_core::session_flow::SessionFlowController;
use synergy_core::{Orchestrator, WorkerOutput};
use synergy_db::Database;
use synergy_proto::{AdapterInfo, EventLog, SessionFlowState, SessionPhase, Task, TaskStatus};
use synergy_proxy::ProxyManager;
use tauri::{AppHandle, Builder, Emitter, Manager, Runtime, State};
use tokio::sync::Mutex;

/// Embedded adapters.toml content -- lists all supported Leader/Worker adapters.
const ADAPTERS_TOML: &str = include_str!("../../../adapters.toml");

pub struct AppState {
    pub orchestrator: Arc<Mutex<Option<Arc<Orchestrator>>>>,
    pub db: Arc<Mutex<Option<Arc<Mutex<Database>>>>>,
    pub session_flow: Arc<Mutex<SessionFlowController>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            orchestrator: Arc::new(Mutex::new(None)),
            db: Arc::new(Mutex::new(None)),
            session_flow: Arc::new(Mutex::new(SessionFlowController::new())),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerInfo {
    pub id: u32,
    pub proxy_addr: Option<String>,
    pub status: String,
    pub current_task_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub tasks: Vec<Task>,
    pub workers: Vec<WorkerInfo>,
    pub logs: Vec<EventLog>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerOutputEvent {
    pub worker_id: u32,
    pub task_id: Option<String>,
    pub chunk: String,
}

#[tauri::command]
fn get_status() -> &'static str {
    "Synergy Engine Running"
}

#[tauri::command]
fn get_config() -> Result<SynergyConfig, String> {
    SynergyConfig::load_default().map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
pub struct InitArgs {
    pub db_path: String,
    pub project_name: String,
    pub bin_path: String,
    pub worker_count: u32,
    pub adapter_id: Option<String>,
    pub project_dir: Option<String>,
}

fn pick_adapter(id: &str) -> Arc<dyn AppAdapter> {
    match id {
        "opencode" => Arc::new(OpenCodeAdapter),
        "cursor" => Arc::new(synergy_adapter::CursorAdapter),
        "kiro" => Arc::new(synergy_adapter::KiroAdapter),
        "api-direct" | "api" => Arc::new(synergy_adapter::DirectApiAdapter::new()),
        other => Arc::new(GenericCliAdapter::new(other, other)),
    }
}

#[tauri::command]
async fn init_session<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    args: InitArgs,
) -> Result<String, String> {
    init_session_impl(app, state, args)
        .await
        .map_err(|e| format!("{e:#}"))
}

async fn init_session_impl<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    args: InitArgs,
) -> anyhow::Result<String> {
    let cfg = SynergyConfig::load_default().unwrap_or_default();

    let db = Database::open(&args.db_path).context("open primary db")?;
    db.insert_project("p1", &args.project_name, &args.db_path)
        .ok(); // ignore duplicate project on warm start

    let session_id = format!("s_{}", Utc::now().timestamp());
    let adapter_id = args.adapter_id.clone().unwrap_or(cfg.workers.adapter.clone());
    db.insert_session(&session_id, "p1", &adapter_id, args.worker_count)?;

    let proxy_configs = cfg.proxy.to_proxy_configs(args.worker_count);
    let pm = ProxyManager::new(proxy_configs);

    let bin = if args.bin_path.is_empty() {
        cfg.workers.bin_path.clone()
    } else {
        args.bin_path.clone()
    };

    // Single DB handle shared between orchestrator and UI commands.
    let shared_db = Arc::new(Mutex::new(db));

    let orchestrator =
        Orchestrator::with_shared_db(shared_db.clone(), pm, session_id.clone())
            .with_adapter(pick_adapter(&adapter_id));

    orchestrator
        .spawn_workers(args.worker_count as usize, &bin, args.project_dir.as_deref())
        .await
        .context("spawn workers")?;

    let orch_arc = Arc::new(orchestrator);
    spawn_tick_loop(app.clone(), orch_arc.clone());
    spawn_output_pump(app.clone(), orch_arc.clone());

    *state.db.lock().await = Some(shared_db);
    *state.orchestrator.lock().await = Some(orch_arc);

    Ok(session_id)
}

fn spawn_tick_loop<R: Runtime>(_app: AppHandle<R>, orch: Arc<Orchestrator>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(err) = orch.tick().await {
                eprintln!("orchestrator tick error: {err:#}");
            }
        }
    });
}

fn spawn_output_pump<R: Runtime>(app: AppHandle<R>, orch: Arc<Orchestrator>) {
    let mut rx = orch.subscribe_outputs();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(WorkerOutput {
                    worker_id,
                    task_id,
                    chunk,
                }) => {
                    let _ = app.emit(
                        "worker-output",
                        WorkerOutputEvent {
                            worker_id,
                            task_id,
                            chunk,
                        },
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("worker output lagged by {n} chunks");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[tauri::command]
async fn get_state(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<SessionState, String> {
    let db_opt = state.db.lock().await;
    let db_arc = db_opt.as_ref().ok_or("Database not initialized")?;
    let db = db_arc.lock().await;

    let tasks = db.get_all_tasks(&session_id).map_err(|e| e.to_string())?;
    let logs = db.get_event_logs(&session_id).map_err(|e| e.to_string())?;
    drop(db);
    drop(db_opt);

    let orch_opt = state.orchestrator.lock().await;
    let mut workers_info = Vec::new();
    if let Some(ref orch) = *orch_opt {
        let workers = orch.workers.lock().await;
        for w in workers.iter() {
            workers_info.push(WorkerInfo {
                id: w.id as u32,
                proxy_addr: w.proxy_addr.clone(),
                status: match &w.status {
                    AppStatus::Idle => "idle".to_owned(),
                    AppStatus::Working => "working".to_owned(),
                    AppStatus::Done => "done".to_owned(),
                    AppStatus::Error(_) => "error".to_owned(),
                },
                current_task_id: w.current_task.as_ref().map(|t| t.id.clone()),
            });
        }
    }

    Ok(SessionState {
        tasks,
        workers: workers_info,
        logs,
    })
}

#[derive(Debug, Deserialize)]
pub struct AddTaskArgs {
    pub session_id: String,
    pub title: String,
    pub instruction: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub files_target: Vec<String>,
}

#[tauri::command]
async fn add_task(
    state: State<'_, AppState>,
    args: AddTaskArgs,
) -> Result<String, String> {
    let db_opt = state.db.lock().await;
    let db_arc = db_opt.as_ref().ok_or("Database not initialized")?;
    let db = db_arc.lock().await;

    let task_id = format!("t_{}", Utc::now().timestamp_millis());
    let task = Task {
        id: task_id.clone(),
        session_id: args.session_id,
        title: args.title,
        instruction: args.instruction,
        status: TaskStatus::Pending,
        worker_id: None,
        depends_on: args.depends_on,
        files_target: args.files_target,
        attempt: 0,
        created_at: Utc::now(),
        started_at: None,
        ended_at: None,
    };

    db.insert_task(&task).map_err(|e| e.to_string())?;
    Ok(task_id)
}

#[tauri::command]
async fn add_plan(
    state: State<'_, AppState>,
    session_id: String,
    plan_text: String,
) -> Result<Vec<String>, String> {
    let drafts = synergy_core::leader::parse_plan(&plan_text);
    if drafts.is_empty() {
        return Err("No tasks could be parsed from plan".into());
    }
    let deps = synergy_core::leader::infer_dependencies(&drafts);
    let tasks = synergy_core::leader::drafts_to_tasks(&session_id, &drafts, &deps);

    let db_opt = state.db.lock().await;
    let db_arc = db_opt.as_ref().ok_or("Database not initialized")?;
    let db = db_arc.lock().await;

    let mut ids = Vec::with_capacity(tasks.len());
    for t in &tasks {
        db.insert_task(t).map_err(|e| e.to_string())?;
        ids.push(t.id.clone());
    }
    Ok(ids)
}

#[tauri::command]
async fn send_worker_command(
    state: State<'_, AppState>,
    worker_id: u32,
    command: String,
) -> Result<(), String> {
    let orch_opt = state.orchestrator.lock().await;
    let orch = orch_opt.as_ref().ok_or("Orchestrator not initialized")?;

    let mut workers = orch.workers.lock().await;
    let worker = workers
        .iter_mut()
        .find(|w| w.id == worker_id as usize)
        .ok_or("Worker not found")?;
    orch.adapter
        .send_command(&mut worker.handle, &command)
        .await
        .map_err(|e| e.to_string())
}

// ─── GUI Embedding Commands (Phase 2) ────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct EmbedArgs {
    pub window_title: String,
    pub panel_hwnd: i64,
    pub width: i32,
    pub height: i32,
}

#[tauri::command]
async fn embed_gui_app(args: EmbedArgs) -> Result<i64, String> {
    #[cfg(not(windows))]
    {
        let _ = args;
        return Err("Windows-only feature".into());
    }

    #[cfg(windows)]
    {
        use synergy_win32::{embed_window, find_window_by_title};

        let hwnd = find_window_by_title(&args.window_title).map_err(|e| e.to_string())?;
        embed_window(hwnd, args.panel_hwnd as isize, args.width, args.height)
            .map_err(|e| e.to_string())?;
        Ok(hwnd as i64)
    }
}

#[tauri::command]
async fn detach_gui_app(hwnd: i64) -> Result<(), String> {
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        return Err("Windows-only feature".into());
    }

    #[cfg(windows)]
    {
        use synergy_win32::detach_window;
        detach_window(hwnd as isize).map_err(|e| e.to_string())
    }
}

#[tauri::command]
async fn resize_gui_app(hwnd: i64, width: i32, height: i32) -> Result<(), String> {
    #[cfg(not(windows))]
    {
        let _ = (hwnd, width, height);
        return Err("Windows-only feature".into());
    }

    #[cfg(windows)]
    {
        use synergy_win32::resize_embedded_window;
        resize_embedded_window(hwnd as isize, width, height).map_err(|e| e.to_string())
    }
}

#[tauri::command]
async fn launch_gui_app(exe: String, args: Vec<String>, cwd: Option<String>) -> Result<u32, String> {
    #[cfg(not(windows))]
    {
        let _ = (exe, args, cwd);
        return Err("Windows-only feature".into());
    }

    #[cfg(windows)]
    {
        use synergy_win32::launch_app;
        let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        launch_app(&exe, &args_ref, cwd.as_deref()).map_err(|e| e.to_string())
    }
}

// ─── Git Commands (Phase 3) ─────────────────────────────────────────────────

#[tauri::command]
async fn git_commit_task(
    project_dir: String,
    task_id: String,
    task_title: String,
    worker_id: u32,
) -> Result<String, String> {
    let git = synergy_core::git::GitOps::new(&project_dir);
    git.commit_task(&task_id, &task_title, worker_id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn git_status(project_dir: String) -> Result<String, String> {
    let git = synergy_core::git::GitOps::new(&project_dir);
    if !git.is_repo() {
        return Ok("not a git repository".to_owned());
    }
    git.diff_stat().map_err(|e| e.to_string())
}

#[tauri::command]
async fn git_log(project_dir: String, count: Option<u32>) -> Result<String, String> {
    let git = synergy_core::git::GitOps::new(&project_dir);
    git.log_short(count.unwrap_or(10)).map_err(|e| e.to_string())
}

// ─── Session Commands (Phase 3) ─────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct SessionListItem {
    pub id: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub leader_app: String,
    pub worker_count: u32,
}

#[tauri::command]
async fn list_sessions(
    state: State<'_, AppState>,
) -> Result<Vec<SessionListItem>, String> {
    let db_opt = state.db.lock().await;
    let db_arc = db_opt.as_ref().ok_or("Database not initialized")?;
    let db = db_arc.lock().await;

    let sessions = db.get_sessions("p1").map_err(|e| e.to_string())?;
    Ok(sessions
        .into_iter()
        .map(|s| SessionListItem {
            id: s.id,
            started_at: s.started_at,
            ended_at: s.ended_at,
            leader_app: s.leader_app,
            worker_count: s.worker_count,
        })
        .collect())
}

#[tauri::command]
async fn end_current_session(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<(), String> {
    let db_opt = state.db.lock().await;
    let db_arc = db_opt.as_ref().ok_or("Database not initialized")?;
    let db = db_arc.lock().await;
    db.end_session(&session_id).map_err(|e| e.to_string())
}

// ─── Session Flow Commands (Leader AI Connectivity) ─────────────────────────
//
// These commands implement the frontend-driven session flow:
// 1. get_adapters     -- list all available AI adapters (CLI/GUI/API)
// 2. select_folder    -- user picks project dir
// 3. choose_leader    -- connect to any Leader AI (opencode, aider, codex, cursor, etc.)
// 4. send_to_leader   -- send user message to Leader via PTY/API
// 5. approve_plan     -- parse plan, create tasks, spawn workers, start orchestration
// 6. get_session_flow_state -- query current phase for the UI

/// TOML structure matching adapters.toml for deserialization.
#[derive(Debug, Deserialize)]
struct AdaptersFile {
    adapter: Vec<AdapterEntry>,
}

#[derive(Debug, Deserialize)]
struct AdapterEntry {
    id: String,
    #[serde(rename = "type")]
    adapter_type: String,
    bin: String,
    desc: String,
}

/// Return the list of all supported adapters from the embedded adapters.toml.
/// The frontend uses this to display Leader selection cards.
#[tauri::command]
async fn get_adapters() -> Result<Vec<AdapterInfo>, String> {
    let parsed: AdaptersFile =
        toml::from_str(ADAPTERS_TOML).map_err(|e| format!("Failed to parse adapters.toml: {e}"))?;
    Ok(parsed
        .adapter
        .into_iter()
        .map(|a| AdapterInfo {
            id: a.id,
            adapter_type: a.adapter_type,
            bin: a.bin,
            desc: a.desc,
        })
        .collect())
}

/// Validate and store the selected project folder, advancing the session
/// flow to FolderSelected.
#[tauri::command]
async fn select_folder(
    state: State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    if path.is_empty() {
        return Err("Path cannot be empty".into());
    }
    let mut flow = state.session_flow.lock().await;
    flow.select_folder(path)?;
    Ok(())
}

/// Connect to a Leader AI by spawning the appropriate adapter process.
///
/// This is the core of leader connectivity -- it supports ANY adapter:
/// - CLI adapters (opencode, aider, claude-cli, codex-cli, generic): spawn PTY process
/// - GUI adapters (cursor, kiro, windsurf): Phase 2 placeholder (returns session but no PTY)
/// - API-direct: uses HTTP client (no PTY needed)
///
/// Returns the new session_id. After this, the frontend can call send_to_leader.
#[tauri::command]
async fn choose_leader<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    adapter_id: String,
    project_dir: String,
) -> Result<String, String> {
    // Initialize DB if not already done
    let db_path = format!("{}/.synergy/synergy.db", &project_dir);
    let db = Database::open(&db_path).map_err(|e| format!("Failed to open database: {e:#}"))?;
    db.insert_project("p1", "Project", &project_dir).ok(); // ignore duplicate

    let session_id = format!("s_{}", Utc::now().timestamp());
    let cfg = SynergyConfig::load_default().unwrap_or_default();
    let worker_count = cfg.workers.count;
    db.insert_session(&session_id, "p1", &adapter_id, worker_count)
        .map_err(|e| format!("{e:#}"))?;

    // Pick the appropriate adapter for the Leader
    let adapter = pick_adapter(&adapter_id);

    // Launch the Leader process (PTY for CLI, placeholder for GUI)
    let launch_config = LaunchConfig {
        bin_path: adapter_id.clone(),
        args: Vec::new(),
        cwd: Some(project_dir.clone()),
        proxy_addr: None,
    };

    let handle = adapter
        .launch(&launch_config)
        .await
        .map_err(|e| format!("Failed to launch leader: {e:#}"))?;

    // Store in session flow
    {
        let mut flow = state.session_flow.lock().await;
        flow.set_leader(adapter_id, adapter.clone(), handle, session_id.clone())?;
    }

    // Store DB handle
    let shared_db = Arc::new(Mutex::new(db));
    *state.db.lock().await = Some(shared_db);

    // Start the leader output pump -- reads PTY and emits 'leader-output' events
    spawn_leader_output_pump_tauri(app, state.session_flow.clone());

    Ok(session_id)
}

/// Send a user message to the Leader AI via the adapter (PTY stdin or API call).
/// The response comes asynchronously via the 'leader-output' Tauri event.
#[tauri::command]
async fn send_to_leader(
    state: State<'_, AppState>,
    message: String,
) -> Result<(), String> {
    let mut flow = state.session_flow.lock().await;

    // Advance to Planning on first message
    if flow.phase == SessionPhase::LeaderChosen {
        flow.advance_to_planning();
    }

    flow.send_to_leader(&message).await
}

/// Approve the Leader's plan: parse tasks, insert into DB, spawn workers,
/// and start the orchestrator tick loop.
///
/// Returns the list of created task IDs.
#[tauri::command]
async fn approve_plan<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    session_id: String,
    plan_text: String,
) -> Result<Vec<String>, String> {
    // Parse the plan into task drafts
    let drafts = synergy_core::leader::parse_plan(&plan_text);
    if drafts.is_empty() {
        return Err("No tasks could be parsed from plan".into());
    }
    let deps = synergy_core::leader::infer_dependencies(&drafts);
    let tasks = synergy_core::leader::drafts_to_tasks(&session_id, &drafts, &deps);

    // Insert tasks into DB
    let db_opt = state.db.lock().await;
    let db_arc = db_opt.as_ref().ok_or("Database not initialized")?;
    {
        let db = db_arc.lock().await;
        for t in &tasks {
            db.insert_task(t).map_err(|e| e.to_string())?;
        }
    }

    let ids: Vec<String> = tasks.iter().map(|t| t.id.clone()).collect();
    let task_count = ids.len() as u32;

    // Update session flow
    {
        let mut flow = state.session_flow.lock().await;
        flow.approve_plan(plan_text, task_count);
        flow.advance_to_executing();
    }

    // Set up orchestrator with worker adapter (default: opencode for workers)
    let cfg = SynergyConfig::load_default().unwrap_or_default();
    let worker_adapter_id = cfg.workers.adapter.clone();
    let worker_bin = cfg.workers.bin_path.clone();
    let worker_count = cfg.workers.count;

    let proxy_configs = cfg.proxy.to_proxy_configs(worker_count);
    let pm = ProxyManager::new(proxy_configs);

    let orchestrator = Orchestrator::with_shared_db(db_arc.clone(), pm, session_id.clone())
        .with_adapter(pick_adapter(&worker_adapter_id));

    // Get project_dir from session flow
    let project_dir = {
        let flow = state.session_flow.lock().await;
        flow.project_dir.clone()
    };

    orchestrator
        .spawn_workers(worker_count as usize, &worker_bin, project_dir.as_deref())
        .await
        .map_err(|e| format!("Failed to spawn workers: {e:#}"))?;

    let orch_arc = Arc::new(orchestrator);
    spawn_tick_loop(app.clone(), orch_arc.clone());
    spawn_output_pump(app.clone(), orch_arc.clone());

    // Store orchestrator
    *state.orchestrator.lock().await = Some(orch_arc.clone());

    // Spawn a task that monitors completion and sends batch report to Leader
    spawn_completion_monitor(app, state.session_flow.clone(), orch_arc, db_arc.clone());

    Ok(ids)
}

/// Return the current session flow state for the frontend to render.
#[tauri::command]
async fn get_session_flow_state(
    state: State<'_, AppState>,
) -> Result<SessionFlowState, String> {
    let flow = state.session_flow.lock().await;
    Ok(flow.snapshot())
}

/// Background task that reads Leader PTY output and emits Tauri events.
fn spawn_leader_output_pump_tauri<R: Runtime>(
    app: AppHandle<R>,
    session_flow: Arc<Mutex<SessionFlowController>>,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let mut flow = session_flow.lock().await;
            // Stop pumping if session is complete or no leader connected
            if flow.phase == SessionPhase::Complete || flow.leader_handle.is_none() {
                break;
            }
            if let Some(output) = flow.read_leader_output().await {
                if !output.is_empty() {
                    let _ = app.emit(
                        "leader-output",
                        serde_json::json!({"chunk": output}),
                    );
                }
            }
        }
    });
}

/// Background task that monitors when all tasks are complete, then composes
/// and sends the batch report to the Leader for review.
fn spawn_completion_monitor<R: Runtime>(
    app: AppHandle<R>,
    session_flow: Arc<Mutex<SessionFlowController>>,
    _orch: Arc<Orchestrator>,
    db: Arc<Mutex<Database>>,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(1000));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;

            let flow = session_flow.lock().await;
            if flow.phase != SessionPhase::Executing {
                if flow.phase == SessionPhase::Complete {
                    break;
                }
                drop(flow);
                continue;
            }
            let session_id = match &flow.session_id {
                Some(id) => id.clone(),
                None => {
                    drop(flow);
                    continue;
                }
            };
            drop(flow);

            // Check if all tasks are done
            let all_done = {
                let db_lock = db.lock().await;
                match db_lock.get_all_tasks(&session_id) {
                    Ok(tasks) => {
                        !tasks.is_empty()
                            && tasks.iter().all(|t| {
                                matches!(
                                    t.status,
                                    TaskStatus::Done | TaskStatus::Failed | TaskStatus::Escalated
                                )
                            })
                    }
                    Err(_) => false,
                }
            };

            if all_done {
                // Compose batch report
                let completed_tasks = {
                    let db_lock = db.lock().await;
                    db_lock.get_all_tasks(&session_id).unwrap_or_default()
                };
                let report = synergy_core::leader::compose_batch_report(&completed_tasks);

                // Advance to Reviewing and send report to Leader
                let mut flow = session_flow.lock().await;
                flow.advance_to_reviewing();
                let _ = flow.send_to_leader(&report).await;

                let _ = app.emit(
                    "session-state-changed",
                    serde_json::json!({"phase": "reviewing"}),
                );
                break;
            }
        }
    });
}

pub fn register_handlers<R: Runtime>(builder: Builder<R>) -> Builder<R> {
    builder.setup(|app| {
        app.manage(AppState::default());
        Ok(())
    })
    .invoke_handler(tauri::generate_handler![
        get_status,
        get_config,
        init_session,
        get_state,
        add_task,
        add_plan,
        send_worker_command,
        embed_gui_app,
        detach_gui_app,
        resize_gui_app,
        launch_gui_app,
        git_commit_task,
        git_status,
        git_log,
        list_sessions,
        end_current_session,
        get_adapters,
        select_folder,
        choose_leader,
        send_to_leader,
        approve_plan,
        get_session_flow_state
    ])
}
