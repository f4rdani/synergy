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
    AppAdapter, AppStatus, AppType, GenericCliAdapter, LaunchConfig, OpenCodeAdapter,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerOutputEvent {
    pub worker_id: u32,
    pub task_id: Option<String>,
    pub chunk: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub free: bool,
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
        "antigravity" => Arc::new(synergy_adapter::AntigravityAdapter),
        "codex-cli" | "codex" => Arc::new(synergy_adapter::CodexAdapter),
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
    let adapter_id = args
        .adapter_id
        .clone()
        .unwrap_or(cfg.workers.adapter.clone());
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

    let orchestrator = Orchestrator::with_shared_db(shared_db.clone(), pm, session_id.clone())
        .with_adapter(pick_adapter(&adapter_id));

    orchestrator
        .spawn_workers(
            args.worker_count as usize,
            &bin,
            args.project_dir.as_deref(),
        )
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
async fn get_state(state: State<'_, AppState>, session_id: String) -> Result<SessionState, String> {
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
async fn add_task(state: State<'_, AppState>, args: AddTaskArgs) -> Result<String, String> {
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
async fn launch_gui_app(
    exe: String,
    args: Vec<String>,
    cwd: Option<String>,
) -> Result<u32, String> {
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
    git.log_short(count.unwrap_or(10))
        .map_err(|e| e.to_string())
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
async fn list_sessions(state: State<'_, AppState>) -> Result<Vec<SessionListItem>, String> {
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
async fn end_current_session(state: State<'_, AppState>, session_id: String) -> Result<(), String> {
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

/// Resolve the actual binary path for an adapter ID from the embedded adapters.toml.
///
/// For example, "claude-cli" resolves to binary "claude", "codex-cli" to "codex".
/// Falls back to the adapter_id itself if not found (e.g. for api-direct or unknown adapters).
fn resolve_adapter_bin(adapter_id: &str) -> String {
    if let Ok(parsed) = toml::from_str::<AdaptersFile>(ADAPTERS_TOML) {
        if let Some(entry) = parsed.adapter.iter().find(|a| a.id == adapter_id) {
            return entry.bin.clone();
        }
    }
    adapter_id.to_owned()
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
async fn select_folder(state: State<'_, AppState>, path: String) -> Result<(), String> {
    if path.is_empty() {
        return Err("Path cannot be empty".into());
    }
    let p = std::path::Path::new(&path);
    if !p.exists() {
        std::fs::create_dir_all(p)
            .map_err(|e| format!("Cannot create directory '{}': {e}", path))?;
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
    // Create .synergy dir inside project_dir and open DB there
    let synergy_dir = format!("{}/.synergy", &project_dir);
    std::fs::create_dir_all(&synergy_dir)
        .map_err(|e| format!("Failed to create .synergy dir: {e}"))?;
    let db_path = format!("{}/synergy.db", &synergy_dir);
    let db = Database::open(&db_path).map_err(|e| format!("Failed to open database: {e:#}"))?;
    db.insert_project("p1", "Project", &project_dir).ok(); // ignore duplicate

    let session_id = format!("s_{}", Utc::now().timestamp());
    let cfg = SynergyConfig::load_default().unwrap_or_default();
    let worker_count = cfg.workers.count;
    db.insert_session(&session_id, "p1", &adapter_id, worker_count)
        .map_err(|e| format!("{e:#}"))?;

    // Pick the appropriate adapter for the Leader
    let adapter = pick_adapter(&adapter_id);

    // Emit connecting status
    let _ = app.emit(
        "leader-connection-status",
        serde_json::json!({
            "status": "connecting",
            "message": format!("Launching {}...", &adapter_id)
        }),
    );

    // Launch the Leader process (PTY for CLI, placeholder for GUI)
    // Resolve actual binary from adapters.toml (e.g. "claude-cli" -> "claude")
    let bin_path = resolve_adapter_bin(&adapter_id);

    // For GUI adapters, check if the app is already running before trying to launch
    let is_gui = adapter.app_type() == AppType::Gui;
    let launch_config = LaunchConfig {
        bin_path: bin_path.clone(),
        args: Vec::new(),
        cwd: Some(project_dir.clone()),
        proxy_addr: None,
    };

    let handle = match adapter.launch(&launch_config).await {
        Ok(h) => {
            let conn_type = if is_gui {
                "GUI window embedded via Win32 SetParent + UI Automation"
            } else {
                "CLI process via PTY (pseudo-terminal)"
            };
            let _ = app.emit(
                "leader-connection-status",
                serde_json::json!({
                    "status": "connected",
                    "message": format!("Successfully connected to {}", &adapter_id),
                    "connection_type": conn_type,
                    "adapter_id": &adapter_id
                }),
            );
            h
        }
        Err(e) => {
            // For GUI apps, give a helpful error message
            let err_msg = if is_gui {
                format!(
                    "Could not launch '{}'. Make sure {} is installed and running. Error: {e:#}",
                    bin_path, adapter_id
                )
            } else {
                format!("Failed to launch leader: {e:#}")
            };
            let _ = app.emit(
                "leader-connection-status",
                serde_json::json!({
                    "status": "failed",
                    "message": format!("Failed to connect: {}", &err_msg),
                    "adapter_id": &adapter_id,
                }),
            );
            return Err(err_msg);
        }
    };

    // Store in session flow
    {
        let mut flow = state.session_flow.lock().await;
        flow.set_leader(adapter_id, adapter.clone(), handle, session_id.clone())?;
    }

    // After leader is connected, send the system prompt to teach it how to plan
    {
        let mut flow = state.session_flow.lock().await;
        let prompt = synergy_core::leader::leader_system_prompt(
            &project_dir,
            cfg.workers.count,
        );
        flow.send_to_leader(&prompt).await.ok(); // best effort
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
async fn send_to_leader(state: State<'_, AppState>, message: String) -> Result<(), String> {
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
    let max_workers = cfg.workers.count;

    // Calculate how many workers to spawn based on independent (parallelizable) tasks
    let independent_task_count = tasks.iter().filter(|t| t.depends_on.is_empty()).count();
    let actual_worker_count = (independent_task_count as u32).min(max_workers);

    let proxy_configs = cfg.proxy.to_proxy_configs(actual_worker_count);
    let pm = ProxyManager::new(proxy_configs);

    let orchestrator = Orchestrator::with_shared_db(db_arc.clone(), pm, session_id.clone())
        .with_adapter(pick_adapter(&worker_adapter_id));

    // Get project_dir from session flow
    let project_dir = {
        let flow = state.session_flow.lock().await;
        flow.project_dir.clone()
    };

    orchestrator
        .spawn_workers(actual_worker_count as usize, &worker_bin, project_dir.as_deref())
        .await
        .map_err(|e| format!("Failed to spawn workers: {e:#}"))?;

    // Set worker model if configured
    let worker_model = cfg.workers.model.clone();
    if !worker_model.is_empty() {
        orchestrator
            .set_worker_model(&worker_model)
            .await
            .map_err(|e| format!("Failed to set worker model: {e:#}"))?;
    }

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
async fn get_session_flow_state(state: State<'_, AppState>) -> Result<SessionFlowState, String> {
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
            let output = {
                let mut flow = session_flow.lock().await;
                // Stop pumping if session is complete or no leader connected
                if flow.phase == SessionPhase::Complete || flow.leader_handle.is_none() {
                    break;
                }
                flow.read_leader_output().await
            }; // mutex released here
            if let Some(ref text) = output {
                if !text.is_empty() {
                    let _ = app.emit("leader-output", serde_json::json!({"chunk": text}));
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
                {
                    let mut flow = session_flow.lock().await;
                    flow.advance_to_reviewing();
                    let _ = flow.send_to_leader(&report).await;
                }

                let _ = app.emit(
                    "session-state-changed",
                    serde_json::json!({"phase": "reviewing"}),
                );

                // Wait for leader approval (poll for up to 30 seconds)
                let approval_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                loop {
                    tokio::time::sleep(Duration::from_secs(2)).await;

                    let flow = session_flow.lock().await;
                    let approved = synergy_core::leader::is_approval(&flow.leader_output_buffer);
                    drop(flow);

                    if approved || tokio::time::Instant::now() >= approval_deadline {
                        let mut flow = session_flow.lock().await;
                        flow.advance_to_complete();
                        let _ = app.emit(
                            "session-state-changed",
                            serde_json::json!({"phase": "complete"}),
                        );
                        break;
                    }
                }
                break;
            }
        }
    });
}

/// Fetch available models from the connected Leader provider.
/// - CLI leaders: send /model or /models command, parse response
/// - API-direct: call provider's model listing API
/// - GUI leaders: return known model list from config
#[tauri::command]
async fn fetch_leader_models(state: State<'_, AppState>) -> Result<Vec<ModelInfo>, String> {
    let flow = state.session_flow.lock().await;
    let adapter_id = flow.leader_adapter_id.as_deref().unwrap_or("").to_owned();
    drop(flow);

    match adapter_id.as_str() {
        // CLI-based leaders: query via PTY
        "opencode" | "aider" | "claude-cli" | "codex-cli" | "antigravity" => {
            // Send model listing command
            let cmd = match adapter_id.as_str() {
                "claude-cli" => "/model",
                "opencode" => "/model",
                "aider" => "/models",
                "codex-cli" => "codex models",
                _ => "/model",
            };

            let mut flow = state.session_flow.lock().await;
            if let Err(e) = flow.send_to_leader(cmd).await {
                return Err(format!("Failed to query models: {e}"));
            }
            drop(flow);

            // Wait for response
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;

            let flow = state.session_flow.lock().await;
            let output = &flow.leader_output_buffer;
            let models = parse_leader_model_list(output, &adapter_id);
            if models.is_empty() {
                Ok(known_models_for_provider(&adapter_id))
            } else {
                Ok(models)
            }
        }
        // API-direct: return models based on configured provider
        "api-direct" | "api" => Ok(known_models_for_provider("api-direct")),
        // GUI IDEs: return known models
        "cursor" => Ok(known_models_for_provider("cursor")),
        "kiro" => Ok(known_models_for_provider("kiro")),
        "windsurf" => Ok(known_models_for_provider("windsurf")),
        _ => Ok(known_models_for_provider(&adapter_id)),
    }
}

/// Parse model list output from a CLI leader.
fn parse_leader_model_list(output: &str, _adapter_id: &str) -> Vec<ModelInfo> {
    let clean = synergy_adapter::OpenCodeAdapter::strip_ansi(output);
    let mut models = Vec::new();

    for line in clean.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Skip header lines
        if trimmed.starts_with("Available")
            || trimmed.starts_with("Model")
            || trimmed.starts_with("Current")
            || trimmed.starts_with("---")
        {
            continue;
        }

        let cleaned = trimmed
            .trim_start_matches(|c: char| {
                c.is_ascii_digit()
                    || c == '.'
                    || c == ')'
                    || c == '*'
                    || c == '>'
                    || c == '-'
                    || c == ' '
            })
            .trim();

        if let Some(model_id) = cleaned.split_whitespace().next() {
            if model_id.contains('/') && model_id.len() > 3 {
                let parts: Vec<&str> = model_id.splitn(2, '/').collect();
                let provider = parts.first().unwrap_or(&"unknown");
                let name = parts.get(1).unwrap_or(&model_id);
                if !models.iter().any(|m: &ModelInfo| m.id == model_id) {
                    models.push(ModelInfo {
                        id: model_id.to_owned(),
                        name: name.replace('-', " "),
                        provider: capitalize_first(provider),
                        free: !trimmed.to_lowercase().contains("paid"),
                    });
                }
            }
        }
    }
    models
}

/// Known models for providers (used as fallback or for GUI IDEs).
fn known_models_for_provider(provider_id: &str) -> Vec<ModelInfo> {
    match provider_id {
        "cursor" => vec![
            ModelInfo {
                id: "claude-sonnet-4".into(),
                name: "Claude Sonnet 4".into(),
                provider: "Cursor".into(),
                free: false,
            },
            ModelInfo {
                id: "gpt-4o".into(),
                name: "GPT-4o".into(),
                provider: "Cursor".into(),
                free: false,
            },
            ModelInfo {
                id: "gpt-4o-mini".into(),
                name: "GPT-4o Mini".into(),
                provider: "Cursor".into(),
                free: true,
            },
            ModelInfo {
                id: "cursor-small".into(),
                name: "Cursor Small".into(),
                provider: "Cursor".into(),
                free: true,
            },
        ],
        "kiro" => vec![
            ModelInfo {
                id: "claude-sonnet-4".into(),
                name: "Claude Sonnet 4".into(),
                provider: "Kiro".into(),
                free: false,
            },
            ModelInfo {
                id: "claude-haiku-3.5".into(),
                name: "Claude Haiku 3.5".into(),
                provider: "Kiro".into(),
                free: false,
            },
        ],
        "windsurf" => vec![
            ModelInfo {
                id: "cascade".into(),
                name: "Cascade".into(),
                provider: "Windsurf".into(),
                free: true,
            },
            ModelInfo {
                id: "gpt-4o".into(),
                name: "GPT-4o".into(),
                provider: "Windsurf".into(),
                free: false,
            },
        ],
        "antigravity" => vec![ModelInfo {
            id: "antigravity/default".into(),
            name: "Antigravity Default".into(),
            provider: "Antigravity".into(),
            free: true,
        }],
        "codex-cli" => vec![
            ModelInfo {
                id: "codex-mini".into(),
                name: "Codex Mini".into(),
                provider: "OpenAI".into(),
                free: true,
            },
            ModelInfo {
                id: "o4-mini".into(),
                name: "o4-mini".into(),
                provider: "OpenAI".into(),
                free: true,
            },
        ],
        "api-direct" => vec![
            ModelInfo {
                id: "claude-sonnet-4-20250514".into(),
                name: "Claude Sonnet 4".into(),
                provider: "Anthropic".into(),
                free: false,
            },
            ModelInfo {
                id: "claude-haiku-3-5".into(),
                name: "Claude Haiku 3.5".into(),
                provider: "Anthropic".into(),
                free: false,
            },
            ModelInfo {
                id: "gpt-4o".into(),
                name: "GPT-4o".into(),
                provider: "OpenAI".into(),
                free: false,
            },
            ModelInfo {
                id: "gpt-4o-mini".into(),
                name: "GPT-4o Mini".into(),
                provider: "OpenAI".into(),
                free: false,
            },
        ],
        _ => vec![ModelInfo {
            id: "default".into(),
            name: "Default Model".into(),
            provider: "Auto".into(),
            free: true,
        }],
    }
}

/// Get the current worker model from config.
#[tauri::command]
async fn get_worker_model() -> Result<String, String> {
    let cfg = SynergyConfig::load_default().unwrap_or_default();
    Ok(cfg.workers.model)
}

/// Set the worker model in config and apply to running workers.
#[tauri::command]
async fn set_worker_model_cmd(
    state: State<'_, AppState>,
    model: String,
) -> Result<(), String> {
    // Update config file
    let mut cfg = SynergyConfig::load_default().unwrap_or_default();
    cfg.workers.model = model.clone();
    let path = SynergyConfig::default_path().map_err(|e| e.to_string())?;
    cfg.save_to(&path).map_err(|e| e.to_string())?;

    // If orchestrator is running, update workers live
    let orch_opt = state.orchestrator.lock().await;
    if let Some(ref orch) = *orch_opt {
        orch.set_worker_model(&model)
            .await
            .map_err(|e| format!("{e:#}"))?;
    }

    Ok(())
}

/// Query available models from a running OpenCode worker by sending `/model`.
/// OpenCode responds with a list of free models when you type `/model` without arguments.
/// Falls back to a cached/default list if no workers are running.
#[tauri::command]
async fn get_available_models(
    state: State<'_, AppState>,
) -> Result<Vec<ModelInfo>, String> {
    // Try to query from a running worker
    let orch_opt = state.orchestrator.lock().await;
    if let Some(ref orch) = *orch_opt {
        let mut workers = orch.workers.lock().await;
        // Find an idle worker to query, or use worker 0
        let worker_idx = workers
            .iter()
            .position(|w| w.status == AppStatus::Idle)
            .or_else(|| if workers.is_empty() { None } else { Some(0) });
        if let Some(idx) = worker_idx {
            // Clear the buffer first
            workers[idx].output_buffer.clear();

            // Send /model command to query available models
            if orch
                .adapter
                .send_command(&mut workers[idx].handle, "/model")
                .await
                .is_err()
            {
                drop(workers);
                drop(orch_opt);
                return Ok(fallback_models());
            }

            // Release locks and wait for output (give OpenCode time to respond)
            drop(workers);
            drop(orch_opt);
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            // Read the output
            let orch_opt2 = state.orchestrator.lock().await;
            if let Some(ref orch2) = *orch_opt2 {
                let workers2 = orch2.workers.lock().await;
                if let Some(worker2) = workers2.get(idx) {
                    let models = parse_model_list(&worker2.output_buffer);
                    if !models.is_empty() {
                        return Ok(models);
                    }
                }
            }
        }
    }

    // Fallback if no workers running
    Ok(fallback_models())
}

/// Parse the output of `/model` command from OpenCode to extract model list.
/// OpenCode typically shows models in a format like:
/// ```text
///   1. provider/model-name (free)
///   * provider/model-name
///   - provider/model-name
/// ```
fn parse_model_list(output: &str) -> Vec<ModelInfo> {
    let mut models = Vec::new();
    let clean = synergy_adapter::OpenCodeAdapter::strip_ansi(output);

    for line in clean.lines() {
        let trimmed = line.trim();
        // Skip empty lines and headers
        if trimmed.is_empty()
            || trimmed.starts_with("Available")
            || trimmed.starts_with("Model")
            || trimmed.starts_with("Current")
        {
            continue;
        }

        // Try to extract model ID from various formats:
        // "  1. anthropic/claude-sonnet-4 (free)"
        // "  * anthropic/claude-sonnet-4"
        // "  > anthropic/claude-sonnet-4 [selected]"
        // "  - anthropic/claude-sonnet-4"
        // "    anthropic/claude-sonnet-4"
        let cleaned = trimmed
            .trim_start_matches(|c: char| {
                c.is_ascii_digit()
                    || c == '.'
                    || c == ')'
                    || c == '*'
                    || c == '>'
                    || c == '-'
                    || c == '['
                    || c == ']'
            })
            .trim();

        // Must contain a '/' to look like a model id (provider/model-name)
        if let Some(model_id) = cleaned.split_whitespace().next() {
            if model_id.contains('/') && model_id.len() > 3 {
                let parts: Vec<&str> = model_id.splitn(2, '/').collect();
                let provider = parts.first().unwrap_or(&"unknown").to_string();
                let model_name = parts.get(1).unwrap_or(&model_id).to_string();
                let is_free =
                    trimmed.to_lowercase().contains("free") || !trimmed.to_lowercase().contains("paid");

                // Avoid duplicates
                if !models.iter().any(|m: &ModelInfo| m.id == model_id) {
                    models.push(ModelInfo {
                        id: model_id.to_owned(),
                        name: model_name.replace('-', " ").replace('_', " "),
                        provider: capitalize_first(&provider),
                        free: is_free,
                    });
                }
            }
        }
    }

    models
}

fn capitalize_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

/// Fallback model list shown when no workers are running to query.
fn fallback_models() -> Vec<ModelInfo> {
    vec![ModelInfo {
        id: "(query workers to see live list)".to_owned(),
        name: "Start a session first to see available models".to_owned(),
        provider: "Info".to_owned(),
        free: true,
    }]
}

/// Open native folder picker dialog using platform-native mechanisms.
/// On Windows uses PowerShell FolderBrowserDialog, on macOS uses osascript,
/// on Linux uses zenity.
#[tauri::command]
async fn open_folder_dialog() -> Result<Option<String>, String> {
    let result = tokio::task::spawn_blocking(|| {
        #[cfg(windows)]
        {
            let output = std::process::Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    "Add-Type -AssemblyName System.Windows.Forms; $dialog = New-Object System.Windows.Forms.FolderBrowserDialog; $dialog.Description = 'Select Project Folder'; $dialog.ShowNewFolderButton = $true; if ($dialog.ShowDialog() -eq 'OK') { $dialog.SelectedPath } else { '' }",
                ])
                .output();
            match output {
                Ok(out) => {
                    let path = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                    if path.is_empty() {
                        None
                    } else {
                        Some(path)
                    }
                }
                Err(_) => None,
            }
        }
        #[cfg(target_os = "macos")]
        {
            let output = std::process::Command::new("osascript")
                .args([
                    "-e",
                    "POSIX path of (choose folder with prompt \"Select Project Folder\")",
                ])
                .output();
            match output {
                Ok(out) if out.status.success() => {
                    let path = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                    if path.is_empty() {
                        None
                    } else {
                        Some(path)
                    }
                }
                _ => None,
            }
        }
        #[cfg(all(not(windows), not(target_os = "macos")))]
        {
            let output = std::process::Command::new("zenity")
                .args(["--file-selection", "--directory", "--title=Select Project Folder"])
                .output();
            match output {
                Ok(out) if out.status.success() => {
                    let path = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                    if path.is_empty() {
                        None
                    } else {
                        Some(path)
                    }
                }
                _ => None,
            }
        }
    })
    .await
    .map_err(|e| format!("{e}"))?;

    Ok(result)
}

/// Get list of recently opened project folders from config.
#[tauri::command]
async fn get_recent_projects() -> Result<Vec<String>, String> {
    let cfg = SynergyConfig::load_default().unwrap_or_default();
    Ok(cfg.general.recent_projects.clone())
}

/// Add a project to the recent projects list (max 10, most recent first).
#[tauri::command]
async fn add_recent_project(path: String) -> Result<(), String> {
    let mut cfg = SynergyConfig::load_default().unwrap_or_default();
    cfg.general.recent_projects.retain(|p| p != &path);
    cfg.general.recent_projects.insert(0, path);
    cfg.general.recent_projects.truncate(10);
    let config_path = SynergyConfig::default_path().map_err(|e| e.to_string())?;
    cfg.save_to(&config_path).map_err(|e| e.to_string())?;
    Ok(())
}

/// Minimize the main window.
#[tauri::command]
async fn minimize_window<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    app.get_webview_window("main")
        .ok_or("window not found")?
        .minimize()
        .map_err(|e| e.to_string())
}

/// Toggle maximize/restore the main window.
#[tauri::command]
async fn toggle_maximize_window<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    let win = app.get_webview_window("main").ok_or("window not found")?;
    if win.is_maximized().unwrap_or(false) {
        win.unmaximize().map_err(|e| e.to_string())
    } else {
        win.maximize().map_err(|e| e.to_string())
    }
}

/// Close the main window.
#[tauri::command]
async fn close_window<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    app.get_webview_window("main")
        .ok_or("window not found")?
        .close()
        .map_err(|e| e.to_string())
}

pub fn register_handlers<R: Runtime>(builder: Builder<R>) -> Builder<R> {
    builder
        .setup(|app| {
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
            get_session_flow_state,
            fetch_leader_models,
            get_worker_model,
            set_worker_model_cmd,
            get_available_models,
            open_folder_dialog,
            get_recent_projects,
            add_recent_project,
            minimize_window,
            toggle_maximize_window,
            close_window
        ])
}
