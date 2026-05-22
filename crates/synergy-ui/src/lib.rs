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

// ─── Logging helper ─────────────────────────────────────────────────────
/// Emit a structured log event to both stderr (terminal) and the frontend.
fn log_event<R: Runtime>(app: &AppHandle<R>, level: &str, source: &str, message: impl std::fmt::Display) {
    let msg = message.to_string();
    let ts = chrono::Utc::now().format("%H:%M:%S%.3f").to_string();
    eprintln!("[{ts}] [{level}] [{source}] {msg}");
    let _ = app.emit("synergy-log", serde_json::json!({
        "ts": &ts,
        "level": level,
        "source": source,
        "message": &msg,
    }));
}

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
        // Headless run mode — chat-style, no TUI glitches
        "opencode" => Arc::new(synergy_adapter::OpenCodeRunAdapter::new()),
        // Legacy TUI mode
        "opencode-tui" => Arc::new(OpenCodeAdapter),
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
            if let Some(resolved) = resolve_binary_path(&entry.bin) {
                return resolved;
            }
            return entry.bin.clone();
        }
    }
    adapter_id.to_owned()
}

/// Return the list of all supported adapters from the embedded adapters.toml.
/// Only returns adapters whose binary is actually available on the system.
#[tauri::command]
async fn get_adapters() -> Result<Vec<AdapterInfo>, String> {
    let parsed: AdaptersFile =
        toml::from_str(ADAPTERS_TOML).map_err(|e| format!("Failed to parse adapters.toml: {e}"))?;

    let adapters: Vec<AdapterInfo> = parsed
        .adapter
        .into_iter()
        .filter(|a| {
            // Always show API adapters (no binary needed)
            if a.adapter_type == "api" {
                return true;
            }
            // For CLI/GUI adapters, check if binary exists in PATH or as absolute path
            resolve_binary_path(&a.bin).is_some()
        })
        .map(|a| AdapterInfo {
            id: a.id,
            adapter_type: a.adapter_type,
            bin: a.bin,
            desc: a.desc,
        })
        .collect();

    Ok(adapters)
}

/// Check if a binary is available (in PATH or as absolute path) and return the resolved path.
fn resolve_binary_path(bin: &str) -> Option<String> {
    // Try as-is first (absolute path or in current dir)
    if std::path::Path::new(bin).exists() {
        return Some(bin.to_owned());
    }
    
    // On Windows, try appending .exe or .cmd to the direct path
    #[cfg(windows)]
    {
        let with_exe = format!("{}.exe", bin);
        if std::path::Path::new(&with_exe).exists() {
            return Some(with_exe);
        }
        let with_cmd = format!("{}.cmd", bin);
        if std::path::Path::new(&with_cmd).exists() {
            return Some(with_cmd);
        }
    }

    // Check next to the current executable (useful for Tauri sidecars)
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(parent) = exe_path.parent() {
            // First check exactly
            let exact = parent.join(bin);
            if exact.exists() {
                return Some(exact.to_string_lossy().into_owned());
            }
            #[cfg(windows)]
            {
                let with_exe = parent.join(format!("{}.exe", bin));
                if with_exe.exists() {
                    return Some(with_exe.to_string_lossy().into_owned());
                }
            }
            // Then check for Tauri sidecar suffixes (e.g. bin-x86_64-pc-windows-msvc.exe)
            if let Ok(entries) = std::fs::read_dir(parent) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with(bin) && name.contains("-") && (name.ends_with(".exe") || name.ends_with(".cmd") || !name.contains(".")) {
                        return Some(entry.path().to_string_lossy().into_owned());
                    }
                }
            }
        }
    }

    // Search in PATH
    if let Ok(path_var) = std::env::var("PATH") {
        let sep = if cfg!(windows) { ';' } else { ':' };
        for dir in path_var.split(sep) {
            let full = std::path::Path::new(dir).join(bin);
            if full.exists() {
                return Some(full.to_string_lossy().into_owned());
            }
            // On Windows, also try with extensions in PATH
            #[cfg(windows)]
            {
                let with_exe = std::path::Path::new(dir).join(format!("{}.exe", bin));
                if with_exe.exists() {
                    return Some(with_exe.to_string_lossy().into_owned());
                }
                let with_cmd = std::path::Path::new(dir).join(format!("{}.cmd", bin));
                if with_cmd.exists() {
                    return Some(with_cmd.to_string_lossy().into_owned());
                }
            }
        }
    }
    None
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
    agent: Option<String>,
    model: Option<String>,
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
    let mut bin_path = resolve_adapter_bin(&adapter_id);

    // For OpenCode, prefer the bundled binary if available
    if adapter_id == "opencode" {
        if let Some(bundled) = synergy_adapter::find_opencode_binary() {
            bin_path = bundled;
        }
    }

    // Pull proxy for the Leader (slot 0 reserved for Leader, workers get 1..=N)
    let leader_proxy = {
        let proxy_configs = cfg.proxy.to_proxy_configs(cfg.workers.count + 1);
        proxy_configs.first().map(|p| p.address.clone())
    };

    // For GUI adapters, check if the app is already running before trying to launch
    let is_gui = adapter.app_type() == AppType::Gui;

    // Force free model for OpenCode + use the `plan` agent by default, but allow overrides
    let args = if adapter_id == "opencode" {
        vec![
            "--model".to_owned(),
            model.unwrap_or_else(|| "opencode/deepseek-v4-flash-free".to_owned()),
            "--agent".to_owned(),
            agent.unwrap_or_else(|| "build".to_owned()),
        ]
    } else {
        Vec::new()
    };

    let launch_config = LaunchConfig {
        bin_path: bin_path.clone(),
        args,
        cwd: Some(project_dir.clone()),
        proxy_addr: leader_proxy.clone(),
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
        flow.set_leader(adapter_id.clone(), adapter.clone(), handle, session_id.clone())?;
    }

    // Wait for the CLI tool to fully start up before sending the system prompt.
    // OpenCode and similar TUI tools need a moment to initialize.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Start the leader output pump BEFORE sending the briefing so we
    // capture the Leader's greeting as the first chat bubble.
    spawn_leader_output_pump_tauri(app.clone(), state.session_flow.clone());

    // Give the pump a moment to start
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Send the Leader briefing as the first message so the model actually
    // SEES it (the `--prompt` flag does not work for `opencode run`).
    // The briefing makes the Leader auto-greet the user in Indonesian.
    //
    // IMPORTANT: The briefing IS the model health check. If the model is
    // rate-limited, the auto-switch mechanism in send_command will kick in
    // (timeout 20s → try next free model). The user sees the switch happen
    // in the chat as "⟳ Model X rate-limited. Switching to Y..."
    // Once a working model responds, the greeting appears automatically.
    if adapter_id == "opencode" {
        let briefing = synergy_core::leader::leader_briefing_message(
            &project_dir,
            cfg.workers.count,
        );

        // Emit status so user knows what's happening
        let _ = app.emit("leader-output", serde_json::json!({
            "chunk": "\u{200B}__SYNERGY_NEW_TURN__\u{200B}\n"
        }));

        let mut flow = state.session_flow.lock().await;
        if let Err(e) = flow.send_to_leader(&briefing).await {
            eprintln!("[choose_leader] briefing failed: {e}");
        }
    } else {
        // Other CLI tools (Aider, etc.) take system prompts directly.
        let mut flow = state.session_flow.lock().await;
        let prompt = synergy_core::leader::leader_system_prompt(
            &project_dir,
            cfg.workers.count,
        );
        flow.send_to_leader(&prompt).await.ok();
    }

    // Store DB handle
    let shared_db = Arc::new(Mutex::new(db));
    *state.db.lock().await = Some(shared_db.clone());

    // Start the task-file watcher: polls .synergy/tasks/ready.json
    // When Leader writes ready.json, Synergy auto-spawns Workers and
    // feeds each one the content of its task-N.md file.
    spawn_task_file_watcher(
        app.clone(),
        state.session_flow.clone(),
        state.orchestrator.clone(),
        state.db.clone(),
        project_dir.clone(),
        shared_db,
    );

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

    eprintln!("[send_to_leader] sending: {}", &message[..message.len().min(80)]);
    flow.send_to_leader(&message).await
}

#[tauri::command]
async fn send_raw_to_leader(state: State<'_, AppState>, data: String) -> Result<(), String> {
    let mut flow = state.session_flow.lock().await;

    if flow.phase == SessionPhase::LeaderChosen {
        flow.advance_to_planning();
    }

    flow.send_raw_to_leader(&data).await
}

/// Fetch the public IP address used by a given proxy.
/// If proxy is None, returns the direct IP (no proxy).
#[tauri::command]
async fn get_public_ip(proxy_addr: Option<String>) -> Result<String, String> {
    let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(8));
    if let Some(ref addr) = proxy_addr {
        if !addr.is_empty() {
            let proxy = reqwest::Proxy::all(addr).map_err(|e| format!("invalid proxy: {e}"))?;
            builder = builder.proxy(proxy);
        }
    }
    let client = builder.build().map_err(|e| e.to_string())?;

    // Try multiple endpoints — some are blocked or rate-limited
    let endpoints = ["https://api.ipify.org", "https://icanhazip.com", "https://ifconfig.me/ip"];
    for url in endpoints {
        if let Ok(resp) = client.get(url).send().await {
            if resp.status().is_success() {
                if let Ok(text) = resp.text().await {
                    let ip = text.trim().to_owned();
                    if !ip.is_empty() && ip.chars().all(|c| c.is_ascii_digit() || c == '.' || c == ':') {
                        return Ok(ip);
                    }
                }
            }
        }
    }
    Err("Could not fetch public IP".to_owned())
}

/// Get connection info for the Leader (proxy address + public IP if available).
#[tauri::command]
async fn get_leader_info(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let flow = state.session_flow.lock().await;
    let adapter_id = flow.leader_adapter_id.clone().unwrap_or_else(|| "none".to_owned());

    let cfg = SynergyConfig::load_default().unwrap_or_default();
    let proxy_configs = cfg.proxy.to_proxy_configs(cfg.workers.count + 1);
    let leader_proxy = proxy_configs.first().map(|p| p.address.clone());

    Ok(serde_json::json!({
        "adapter_id": adapter_id,
        "proxy_addr": leader_proxy,
    }))
}

/// List proxy addresses for all workers.
#[tauri::command]
async fn get_workers_proxy_info() -> Result<Vec<serde_json::Value>, String> {
    let cfg = SynergyConfig::load_default().unwrap_or_default();
    let proxy_configs = cfg.proxy.to_proxy_configs(cfg.workers.count + 1);
    // Skip first (Leader); rest are workers
    let workers: Vec<serde_json::Value> = proxy_configs
        .iter()
        .skip(1)
        .enumerate()
        .map(|(i, p)| {
            serde_json::json!({
                "id": i,
                "label": p.label.clone().unwrap_or_else(|| format!("worker-{}", i + 1)),
                "proxy_addr": p.address.clone(),
            })
        })
        .collect();
    Ok(workers)
}
#[tauri::command]
async fn resize_leader_pty(state: State<'_, AppState>, rows: u16, cols: u16) -> Result<(), String> {
    let mut flow = state.session_flow.lock().await;
    let adapter = flow.leader_adapter.clone().ok_or("No leader adapter")?;
    let handle = flow.leader_handle.as_mut().ok_or("No leader handle")?;
    adapter.resize_pty(handle, rows, cols).await.map_err(|e| e.to_string())
}

/// Restart the Leader process (e.g. after user typed /exit in OpenCode).
/// Re-launches the same adapter with the same project_dir.
#[tauri::command]
async fn restart_leader<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    agent: Option<String>,
    model: Option<String>,
) -> Result<(), String> {
    let (adapter_id, project_dir) = {
        let flow = state.session_flow.lock().await;
        let id = flow.leader_adapter_id.clone().ok_or("No leader to restart")?;
        let dir = flow.project_dir.clone().ok_or("No project dir")?;
        (id, dir)
    };

    // Drop old leader handle (releases PTY)
    {
        let mut flow = state.session_flow.lock().await;
        flow.leader_handle = None;
        flow.leader_adapter = None;
        flow.leader_adapter_id = None;
        flow.session_id = None;
        flow.phase = SessionPhase::FolderSelected;
    }

    // Re-launch same leader
    choose_leader(app, state, adapter_id, project_dir, agent, model).await?;
    Ok(())
}

/// Run an OpenCode CLI subcommand (for slash commands like /model, /help, etc.)
/// Returns the stdout output as a string.
#[tauri::command]
async fn run_opencode_command(args: Vec<String>) -> Result<String, String> {
    let bin = synergy_adapter::find_opencode_binary()
        .ok_or("OpenCode binary not found")?;

    eprintln!("[run_opencode_command] {} {:?}", &bin, &args);

    let output = tokio::process::Command::new(&bin)
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("Failed to run opencode: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let mut result = stdout;
    if !stderr.is_empty() && !output.status.success() {
        result.push('\n');
        result.push_str(&stderr);
    }

    Ok(result.trim().to_owned())
}

/// Check for OpenCode updates by running `opencode upgrade`.
#[tauri::command]
async fn check_opencode_update() -> Result<String, String> {
    let bin = synergy_adapter::find_opencode_binary()
        .ok_or("OpenCode binary not found")?;

    let output = tokio::process::Command::new(&bin)
        .arg("upgrade")
        .output()
        .await
        .map_err(|e| format!("Failed to run upgrade: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}{}", stdout, stderr);

    if output.status.success() {
        if combined.trim().is_empty() {
            Ok("OpenCode is up to date".to_owned())
        } else {
            Ok(combined.trim().to_owned())
        }
    } else {
        Err(format!("Upgrade failed: {}", combined.trim()))
    }
}

/// Install or enable Cloudflare WARP for IP rotation.
/// Downloads and installs silently if not present, or enables proxy mode if already installed.
#[tauri::command]
async fn install_warp<R: Runtime>(app: AppHandle<R>) -> Result<String, String> {
    log_event(&app, "info", "warp", "Checking/installing Cloudflare WARP...");

    let result = synergy_adapter::ensure_warp_installed().await;
    if result {
        log_event(&app, "info", "warp", "✓ WARP proxy active on 127.0.0.1:40000");
        Ok("WARP proxy active. All OpenCode requests will route through Cloudflare.".to_owned())
    } else {
        log_event(&app, "warn", "warp", "WARP installation/activation failed");
        Err("Failed to install/activate WARP. Try installing manually from https://1.1.1.1/".to_owned())
    }
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
    log_event(&app, "info", "approve_plan", format!("Called with session_id={session_id}, plan_text length={}", plan_text.len()));
    log_event(&app, "debug", "approve_plan", format!("Plan text:\n{plan_text}"));

    // Parse the plan into task drafts
    let drafts = synergy_core::leader::parse_plan(&plan_text);
    log_event(&app, "info", "approve_plan", format!("Parsed {} task drafts", drafts.len()));

    if drafts.is_empty() {
        log_event(&app, "error", "approve_plan", "No tasks parsed from plan");
        return Err("No tasks could be parsed from plan".into());
    }

    let deps = synergy_core::leader::infer_dependencies(&drafts);
    let tasks = synergy_core::leader::drafts_to_tasks(&session_id, &drafts, &deps);

    for (i, t) in tasks.iter().enumerate() {
        log_event(&app, "debug", "approve_plan",
            format!("Task #{}: id={}, title='{}', deps={:?}, files={:?}",
                i + 1, t.id, t.title, t.depends_on, t.files_target));
    }

    // Insert tasks into DB
    let db_opt = state.db.lock().await;
    let db_arc = db_opt.as_ref().ok_or("Database not initialized")?;
    {
        let db = db_arc.lock().await;
        for t in &tasks {
            db.insert_task(t).map_err(|e| e.to_string())?;
        }
    }
    log_event(&app, "info", "approve_plan", format!("Inserted {} tasks into DB", tasks.len()));

    let ids: Vec<String> = tasks.iter().map(|t| t.id.clone()).collect();
    let task_count = ids.len() as u32;

    // Update session flow
    {
        let mut flow = state.session_flow.lock().await;
        flow.approve_plan(plan_text, task_count);
        flow.advance_to_executing();
    }
    log_event(&app, "info", "approve_plan", "Session flow advanced to Executing");

    // Set up orchestrator with worker adapter (default: opencode for workers)
    let cfg = SynergyConfig::load_default().unwrap_or_default();
    let worker_adapter_id = cfg.workers.adapter.clone();
    let worker_bin = resolve_binary_path(&cfg.workers.bin_path)
        .unwrap_or_else(|| cfg.workers.bin_path.clone());
    let max_workers = cfg.workers.count;

    log_event(&app, "info", "approve_plan",
        format!("Worker config: adapter={worker_adapter_id}, bin={worker_bin}, max={max_workers}"));

    // Calculate how many workers to spawn based on independent (parallelizable) tasks
    let independent_task_count = tasks.iter().filter(|t| t.depends_on.is_empty()).count();
    let actual_worker_count = (independent_task_count as u32).min(max_workers);

    log_event(&app, "info", "approve_plan",
        format!("Spawning {} workers ({} independent tasks, max={})", actual_worker_count, independent_task_count, max_workers));

    let proxy_configs = cfg.proxy.to_proxy_configs(actual_worker_count);
    let pm = ProxyManager::new(proxy_configs);

    let orchestrator = Orchestrator::with_shared_db(db_arc.clone(), pm, session_id.clone())
        .with_adapter(pick_adapter(&worker_adapter_id));

    // Get project_dir from session flow
    let project_dir = {
        let flow = state.session_flow.lock().await;
        flow.project_dir.clone()
    };

    log_event(&app, "info", "approve_plan", format!("Spawning workers in cwd={:?}", project_dir));

    orchestrator
        .spawn_workers(actual_worker_count as usize, &worker_bin, project_dir.as_deref())
        .await
        .map_err(|e| {
            log_event(&app, "error", "approve_plan", format!("Failed to spawn workers: {e:#}"));
            format!("Failed to spawn workers: {e:#}")
        })?;

    log_event(&app, "info", "approve_plan", format!("✓ {} workers spawned successfully", actual_worker_count));

    // Set worker model if configured
    let worker_model = cfg.workers.model.clone();
    if !worker_model.is_empty() {
        orchestrator
            .set_worker_model(&worker_model)
            .await
            .map_err(|e| format!("Failed to set worker model: {e:#}"))?;
        log_event(&app, "info", "approve_plan", format!("Worker model set to: {worker_model}"));
    }

    let orch_arc = Arc::new(orchestrator);
    spawn_tick_loop(app.clone(), orch_arc.clone());
    spawn_output_pump(app.clone(), orch_arc.clone());
    log_event(&app, "info", "approve_plan", "Orchestrator tick loop and output pump started");

    // Store orchestrator
    *state.orchestrator.lock().await = Some(orch_arc.clone());

    // Spawn a task that monitors completion and sends batch report to Leader
    spawn_completion_monitor(app.clone(), state.session_flow.clone(), orch_arc, db_arc.clone());
    log_event(&app, "info", "approve_plan", "Completion monitor started — will send batch report to Leader when all tasks done");

    log_event(&app, "info", "approve_plan", format!("✓ Returning {} task IDs to frontend", ids.len()));
    Ok(ids)
}

/// Return the current session flow state for the frontend to render.
#[tauri::command]
async fn get_session_flow_state(state: State<'_, AppState>) -> Result<SessionFlowState, String> {
    let flow = state.session_flow.lock().await;
    Ok(flow.snapshot())
}

/// Lightweight status check — is the Leader currently producing output?
/// Used by the frontend status badge. The actual busy/idle state is also
/// communicated inline in the leader-output stream via BUSY/IDLE markers.
#[tauri::command]
async fn get_leader_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let flow = state.session_flow.lock().await;
    let connected = flow.leader_handle.is_some();
    let phase = format!("{:?}", flow.phase);
    let buffer_len = flow.leader_output_buffer.len();
    drop(flow);

    Ok(serde_json::json!({
        "connected": connected,
        "phase": phase,
        "buffer_len": buffer_len,
    }))
}

/// Get the git changelog for the project — list of files changed since the
/// last commit. Used by the frontend Changes panel.
#[tauri::command]
async fn get_git_changes(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let project_dir = {
        let flow = state.session_flow.lock().await;
        flow.project_dir.clone()
    };
    let project_dir = project_dir.ok_or("no project")?;

    let git = synergy_core::git::GitOps::new(&project_dir);
    if !git.is_repo() {
        return Ok(serde_json::json!({
            "is_repo": false,
            "summary": "(not a git repository — initialise to track changes)",
            "files": [],
        }));
    }

    let stat = git.diff_stat().unwrap_or_default();
    // Parse `git diff --stat` lines: " path/file.ext | NN +-"
    let mut files = Vec::new();
    for line in stat.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("Update")
            || trimmed.starts_with(char::is_numeric) && trimmed.contains("changed")
        {
            continue;
        }
        if let Some((path, rest)) = trimmed.rsplit_once('|') {
            files.push(serde_json::json!({
                "path": path.trim().to_owned(),
                "summary": rest.trim().to_owned(),
            }));
        }
    }
    Ok(serde_json::json!({
        "is_repo": true,
        "summary": stat,
        "files": files,
    }))
}

// ─── Task File Watcher (file-based delegation) ──────────────────────────────
//
// The Leader AI writes task files into `.synergy/tasks/`:
//   plan.md        — overall plan summary
//   task-1.md      — instruction for Worker 1
//   task-2.md      — instruction for Worker 2
//   ...
//   ready.json     — manifest that triggers Synergy to spawn Workers
//
// This watcher polls for ready.json every 2 seconds. Once found, it:
// 1. Parses ready.json to get task metadata
// 2. Reads each task-N.md for the full instruction
// 3. Inserts tasks into the DB with proper dependencies
// 4. Spawns Workers and starts the orchestrator tick loop
// 5. Renames ready.json → ready.done.json to prevent re-triggering

/// Schema for `.synergy/tasks/ready.json` written by the Leader.
#[derive(Debug, Clone, Deserialize)]
struct ReadyJson {
    total_tasks: u32,
    tasks: Vec<ReadyTask>,
    #[serde(default)]
    parallel_groups: Vec<Vec<u32>>,
}

#[derive(Debug, Clone, Deserialize)]
struct ReadyTask {
    id: u32,
    title: String,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    depends_on: Vec<u32>,
}

fn spawn_task_file_watcher<R: Runtime>(
    app: AppHandle<R>,
    session_flow: Arc<Mutex<SessionFlowController>>,
    orchestrator_slot: Arc<Mutex<Option<Arc<Orchestrator>>>>,
    _db_slot: Arc<Mutex<Option<Arc<Mutex<Database>>>>>,
    project_dir: String,
    shared_db: Arc<Mutex<Database>>,
) {
    tokio::spawn(async move {
        let tasks_dir = format!("{}/.synergy/tasks", &project_dir);
        let ready_path = format!("{}/ready.json", &tasks_dir);
        let done_path = format!("{}/ready.done.json", &tasks_dir);

        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            // Check if session is still active
            {
                let flow = session_flow.lock().await;
                if flow.phase == SessionPhase::Complete || flow.leader_handle.is_none() {
                    break;
                }
            }

            // Check if ready.json exists
            let ready_file = std::path::Path::new(&ready_path);
            if !ready_file.exists() {
                continue;
            }

            log_event(&app, "info", "task-watcher", "🎯 ready.json detected! Starting worker delegation...");

            // Read and parse ready.json
            let ready_content = match tokio::fs::read_to_string(&ready_path).await {
                Ok(c) => c,
                Err(e) => {
                    log_event(&app, "error", "task-watcher", format!("Failed to read ready.json: {e}"));
                    continue;
                }
            };

            let ready: ReadyJson = match serde_json::from_str(&ready_content) {
                Ok(r) => r,
                Err(e) => {
                    log_event(&app, "error", "task-watcher", format!("Failed to parse ready.json: {e}"));
                    // Rename to .error.json so Leader can see the issue
                    let err_path = format!("{}/ready.error.json", &tasks_dir);
                    let _ = tokio::fs::rename(&ready_path, &err_path).await;
                    continue;
                }
            };

            log_event(&app, "info", "task-watcher",
                format!("Plan: {} tasks, parallel_groups: {:?}", ready.total_tasks, ready.parallel_groups));

            // Read each task-N.md file for full instructions
            let mut task_instructions: Vec<(u32, String, String)> = Vec::new(); // (id, title, instruction)
            for task_meta in &ready.tasks {
                let task_file = format!("{}/task-{}.md", &tasks_dir, task_meta.id);
                let instruction = match tokio::fs::read_to_string(&task_file).await {
                    Ok(content) => content,
                    Err(_) => {
                        // Fallback: use the title as instruction
                        log_event(&app, "warn", "task-watcher",
                            format!("task-{}.md not found, using title as instruction", task_meta.id));
                        task_meta.title.clone()
                    }
                };
                task_instructions.push((task_meta.id, task_meta.title.clone(), instruction));
            }

            // Get session_id
            let session_id = {
                let flow = session_flow.lock().await;
                match &flow.session_id {
                    Some(id) => id.clone(),
                    None => {
                        log_event(&app, "error", "task-watcher", "No session_id available");
                        continue;
                    }
                }
            };

            // Build Task records with proper dependencies
            let now = chrono::Utc::now();
            let task_id_map: std::collections::HashMap<u32, String> = ready.tasks.iter()
                .map(|t| (t.id, format!("t_{}_{}", &session_id, t.id)))
                .collect();

            let mut tasks_to_insert: Vec<Task> = Vec::new();
            for task_meta in &ready.tasks {
                let db_task_id = task_id_map.get(&task_meta.id).unwrap().clone();
                let depends_on: Vec<String> = task_meta.depends_on.iter()
                    .filter_map(|dep_id| task_id_map.get(dep_id).cloned())
                    .collect();

                let instruction = task_instructions.iter()
                    .find(|(id, _, _)| *id == task_meta.id)
                    .map(|(_, _, instr)| instr.clone())
                    .unwrap_or_else(|| task_meta.title.clone());

                let status = if depends_on.is_empty() {
                    TaskStatus::Pending
                } else {
                    TaskStatus::Blocked
                };

                tasks_to_insert.push(Task {
                    id: db_task_id,
                    session_id: session_id.clone(),
                    title: task_meta.title.clone(),
                    instruction,
                    status,
                    worker_id: None,
                    depends_on,
                    files_target: task_meta.files.clone(),
                    attempt: 0,
                    created_at: now,
                    started_at: None,
                    ended_at: None,
                });
            }

            // Insert tasks into DB
            {
                let db = shared_db.lock().await;
                for t in &tasks_to_insert {
                    if let Err(e) = db.insert_task(t) {
                        log_event(&app, "error", "task-watcher", format!("Failed to insert task {}: {e}", t.id));
                    }
                }
            }
            log_event(&app, "info", "task-watcher", format!("Inserted {} tasks into DB", tasks_to_insert.len()));

            // Spawn Workers via orchestrator
            let cfg = SynergyConfig::load_default().unwrap_or_default();
            let worker_adapter_id = cfg.workers.adapter.clone();
            let worker_bin = resolve_binary_path(&cfg.workers.bin_path)
                .or_else(|| synergy_adapter::find_opencode_binary())
                .unwrap_or_else(|| cfg.workers.bin_path.clone());
            let max_workers = cfg.workers.count;

            let independent_count = tasks_to_insert.iter().filter(|t| t.depends_on.is_empty()).count();
            let actual_worker_count = (independent_count as u32).min(max_workers).max(1);

            log_event(&app, "info", "task-watcher",
                format!("Spawning {} workers (adapter={}, bin={})", actual_worker_count, worker_adapter_id, worker_bin));

            let proxy_configs = cfg.proxy.to_proxy_configs(actual_worker_count);
            let pm = ProxyManager::new(proxy_configs);

            let orchestrator = Orchestrator::with_shared_db(shared_db.clone(), pm, session_id.clone())
                .with_adapter(pick_adapter(&worker_adapter_id));

            if let Err(e) = orchestrator
                .spawn_workers(actual_worker_count as usize, &worker_bin, Some(project_dir.as_str()))
                .await
            {
                log_event(&app, "error", "task-watcher", format!("Failed to spawn workers: {e:#}"));
                continue;
            }

            log_event(&app, "info", "task-watcher", format!("✓ {} workers spawned successfully!", actual_worker_count));

            // Set worker model if configured
            if !cfg.workers.model.is_empty() {
                if let Err(e) = orchestrator.set_worker_model(&cfg.workers.model).await {
                    log_event(&app, "warn", "task-watcher", format!("Failed to set worker model: {e}"));
                }
            }

            let orch_arc = Arc::new(orchestrator);
            spawn_tick_loop(app.clone(), orch_arc.clone());
            spawn_output_pump(app.clone(), orch_arc.clone());

            // Store orchestrator
            *orchestrator_slot.lock().await = Some(orch_arc.clone());

            // Update session flow
            {
                let mut flow = session_flow.lock().await;
                flow.approve_plan(ready_content.clone(), ready.total_tasks);
                flow.advance_to_executing();
            }

            // Emit event to frontend
            let _ = app.emit("session-state-changed", serde_json::json!({
                "phase": "executing",
                "total_tasks": ready.total_tasks,
                "workers": actual_worker_count,
            }));

            // Spawn completion monitor
            spawn_completion_monitor(app.clone(), session_flow.clone(), orch_arc, shared_db.clone());

            // Rename ready.json → ready.done.json to prevent re-triggering
            if let Err(e) = tokio::fs::rename(&ready_path, &done_path).await {
                log_event(&app, "warn", "task-watcher", format!("Could not rename ready.json: {e}"));
                // Try to delete instead
                let _ = tokio::fs::remove_file(&ready_path).await;
            }

            log_event(&app, "info", "task-watcher",
                format!("🚀 Delegation complete! {} tasks queued, {} workers active. Orchestrator running.", 
                    tasks_to_insert.len(), actual_worker_count));

            break; // Watcher's job is done for this session
        }
        eprintln!("[task-watcher] stopped");
    });
}

/// Background task that reads Leader PTY output and emits Tauri events.
fn spawn_leader_output_pump_tauri<R: Runtime>(
    app: AppHandle<R>,
    session_flow: Arc<Mutex<SessionFlowController>>,
) {
    tokio::spawn(async move {
        let mut consecutive_empty = 0u32;
        loop {
            let poll_ms = if consecutive_empty < 20 { 50 } else { 200 };
            tokio::time::sleep(Duration::from_millis(poll_ms)).await;

            let output = {
                let mut flow = session_flow.lock().await;
                if flow.phase == SessionPhase::Complete || flow.leader_handle.is_none() {
                    break;
                }
                flow.read_leader_output().await
            };
            match output {
                Some(ref text) if !text.is_empty() => {
                    consecutive_empty = 0;
                    let _ = app.emit("leader-output", serde_json::json!({"chunk": text}));
                    eprintln!("[leader-pump] emitted {} bytes", text.len());
                }
                _ => {
                    consecutive_empty += 1;
                }
            }
        }
        eprintln!("[leader-pump] stopped");
    });
}

/// Background task that implements the Leader review loop:
///
/// 1. Monitor each task as it completes
/// 2. When a task finishes → send per-task report to Leader
/// 3. Leader reviews (reads the task-N.md + checks output) and either:
///    a. Approves → mark task as verified
///    b. Sends fix instruction → re-assign to the same Worker
/// 4. Loop until ALL tasks are verified
/// 5. Leader sends final summary to user
///
/// The Leader communicates via a file-based protocol:
/// - Synergy writes `.synergy/tasks/report-N.md` when task N completes
/// - Leader reads it, then writes `.synergy/tasks/verdict-N.json`:
///   {"task_id": N, "verdict": "pass"} or
///   {"task_id": N, "verdict": "fix", "instruction": "...fix details..."}
/// - Synergy picks up the verdict and acts accordingly
fn spawn_completion_monitor<R: Runtime>(
    app: AppHandle<R>,
    session_flow: Arc<Mutex<SessionFlowController>>,
    _orch: Arc<Orchestrator>,
    db: Arc<Mutex<Database>>,
) {
    tokio::spawn(async move {
        let project_dir = {
            let flow = session_flow.lock().await;
            flow.project_dir.clone().unwrap_or_default()
        };
        let tasks_dir = format!("{}/.synergy/tasks", &project_dir);

        // Track which tasks have been reported to Leader
        let mut reported_tasks: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Track which tasks have been verified (approved by Leader)
        let mut verified_tasks: std::collections::HashSet<String> = std::collections::HashSet::new();

        let mut interval = tokio::time::interval(Duration::from_millis(1500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            // Check session phase
            let (phase, session_id) = {
                let flow = session_flow.lock().await;
                (flow.phase, flow.session_id.clone())
            };

            if phase == SessionPhase::Complete {
                break;
            }
            if phase != SessionPhase::Executing && phase != SessionPhase::Reviewing {
                continue;
            }

            let session_id = match session_id {
                Some(id) => id,
                None => continue,
            };

            // Get all tasks from DB
            let all_tasks = {
                let db_lock = db.lock().await;
                db_lock.get_all_tasks(&session_id).unwrap_or_default()
            };

            // ── STEP 1: Report newly completed tasks to Leader ──────────────
            for task in all_tasks.iter() {
                if task.status != TaskStatus::Done {
                    continue;
                }
                if reported_tasks.contains(&task.id) {
                    continue;
                }

                // Task just completed! Write report file and notify Leader.
                reported_tasks.insert(task.id.clone());

                let task_num = task.id.split('_').last().unwrap_or("?");
                let worker_id = task.worker_id.map(|w| w.to_string()).unwrap_or_else(|| "?".to_owned());
                let duration = match (task.started_at, task.ended_at) {
                    (Some(s), Some(e)) => format!("{}s", (e - s).num_seconds().max(0)),
                    _ => "?".to_owned(),
                };

                // Write report-N.md for Leader to review
                let report_content = format!(
                    "# Task {} Report\n\n\
                     ## Status: COMPLETED\n\
                     - **Title**: {}\n\
                     - **Worker**: {}\n\
                     - **Duration**: {}\n\
                     - **Files created/modified**: {}\n\n\
                     ## Original Instruction\n\
                     {}\n\n\
                     ## Action Required\n\
                     Please review the output. Write verdict-{}.json with:\n\
                     - `{{\"task_id\": {}, \"verdict\": \"pass\"}}` if everything looks good\n\
                     - `{{\"task_id\": {}, \"verdict\": \"fix\", \"instruction\": \"...what to fix...\"}}` if fixes are needed\n",
                    task_num,
                    task.title,
                    worker_id,
                    duration,
                    if task.files_target.is_empty() { "(none specified)".to_owned() } else { task.files_target.join(", ") },
                    task.instruction.chars().take(500).collect::<String>(),
                    task_num,
                    task_num,
                    task_num,
                );

                let report_path = format!("{}/report-{}.md", &tasks_dir, task_num);
                if let Err(e) = tokio::fs::write(&report_path, &report_content).await {
                    log_event(&app, "warn", "review-loop", format!("Failed to write report-{}.md: {e}", task_num));
                }

                // Send notification to Leader via PTY
                let leader_msg = format!(
                    "\n[SYNERGY] ✅ Task {} selesai (Worker {}, {}). \
                     File: {}. \
                     Silakan review dan tulis verdict-{}.json (pass/fix).\n",
                    task_num,
                    worker_id,
                    duration,
                    if task.files_target.is_empty() { "various".to_owned() } else { task.files_target.join(", ") },
                    task_num,
                );

                {
                    let mut flow = session_flow.lock().await;
                    let _ = flow.send_to_leader(&leader_msg).await;
                }

                log_event(&app, "info", "review-loop",
                    format!("Task {} completed → report sent to Leader for review", task_num));

                let _ = app.emit("task-completed", serde_json::json!({
                    "task_id": &task.id,
                    "task_num": task_num,
                    "title": &task.title,
                    "worker_id": task.worker_id,
                    "files": &task.files_target,
                }));
            }

            // ── STEP 2: Check for Leader verdicts ───────────────────────────
            for task in all_tasks.iter() {
                if !reported_tasks.contains(&task.id) {
                    continue;
                }
                if verified_tasks.contains(&task.id) {
                    continue;
                }

                let task_num = task.id.split('_').last().unwrap_or("?");
                let verdict_path = format!("{}/verdict-{}.json", &tasks_dir, task_num);

                if !std::path::Path::new(&verdict_path).exists() {
                    continue;
                }

                // Read verdict
                let verdict_content = match tokio::fs::read_to_string(&verdict_path).await {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                #[derive(Deserialize)]
                struct Verdict {
                    #[allow(dead_code)]
                    task_id: u32,
                    verdict: String,
                    #[serde(default)]
                    instruction: String,
                }

                let verdict: Verdict = match serde_json::from_str(&verdict_content) {
                    Ok(v) => v,
                    Err(e) => {
                        log_event(&app, "warn", "review-loop",
                            format!("Invalid verdict-{}.json: {e}", task_num));
                        continue;
                    }
                };

                // Remove verdict file so it doesn't get re-processed
                let _ = tokio::fs::remove_file(&verdict_path).await;

                match verdict.verdict.as_str() {
                    "pass" | "ok" | "approved" | "lgtm" => {
                        verified_tasks.insert(task.id.clone());
                        log_event(&app, "info", "review-loop",
                            format!("✓ Task {} PASSED Leader review", task_num));

                        let _ = app.emit("task-verified", serde_json::json!({
                            "task_id": &task.id,
                            "task_num": task_num,
                            "verdict": "pass",
                        }));
                    }
                    "fix" | "retry" | "revise" => {
                        log_event(&app, "info", "review-loop",
                            format!("⟳ Task {} needs FIX: {}", task_num, &verdict.instruction));

                        // Re-queue the task with the fix instruction
                        let fix_instruction = format!(
                            "[FIX REQUIRED — Leader review feedback]\n\n\
                             Original task: {}\n\n\
                             Fix instruction from Leader:\n{}\n\n\
                             Please fix the issues described above in the files you previously created/modified.",
                            task.title,
                            verdict.instruction,
                        );

                        // Reset task status to Pending so orchestrator picks it up
                        {
                            let db_lock = db.lock().await;
                            db_lock.update_task_status(&task.id, TaskStatus::Pending).ok();
                            // Update the instruction in DB (for the enriched fix context)
                            db_lock.update_task_instruction(&task.id, &fix_instruction).ok();
                        }

                        // Remove from reported so it gets re-reported when done again
                        reported_tasks.remove(&task.id);

                        let _ = app.emit("task-fix-requested", serde_json::json!({
                            "task_id": &task.id,
                            "task_num": task_num,
                            "instruction": &verdict.instruction,
                        }));

                        // Notify Leader
                        {
                            let mut flow = session_flow.lock().await;
                            let _ = flow.send_to_leader(&format!(
                                "\n[SYNERGY] ⟳ Task {} di-reassign ke Worker untuk diperbaiki.\n",
                                task_num
                            )).await;
                        }
                    }
                    other => {
                        log_event(&app, "warn", "review-loop",
                            format!("Unknown verdict '{}' for task {}", other, task_num));
                    }
                }
            }

            // ── STEP 3: Check if ALL tasks are verified ─────────────────────
            let total_tasks = all_tasks.len();
            let all_verified = total_tasks > 0
                && all_tasks.iter().all(|t| {
                    verified_tasks.contains(&t.id)
                        || matches!(t.status, TaskStatus::Failed | TaskStatus::Escalated)
                });

            if all_verified {
                log_event(&app, "info", "review-loop",
                    format!("🎉 All {} tasks verified by Leader!", total_tasks));

                // Send final summary to Leader
                let summary = format!(
                    "\n[SYNERGY] 🎉 Semua {} task sudah selesai dan lulus review!\n\
                     Silakan beri laporan akhir ke user tentang apa yang sudah dibangun.\n",
                    total_tasks
                );
                {
                    let mut flow = session_flow.lock().await;
                    let _ = flow.send_to_leader(&summary).await;
                    flow.advance_to_reviewing();
                }

                let _ = app.emit("session-state-changed", serde_json::json!({
                    "phase": "reviewing",
                    "all_verified": true,
                    "total_tasks": total_tasks,
                }));

                // Wait for Leader to compose the final report to user (give it time)
                tokio::time::sleep(Duration::from_secs(15)).await;

                // Mark session complete
                {
                    let mut flow = session_flow.lock().await;
                    flow.advance_to_complete();
                }
                let _ = app.emit("session-state-changed", serde_json::json!({
                    "phase": "complete",
                }));

                break;
            }

            // ── STEP 4: Handle failed/escalated tasks ───────────────────────
            let has_escalated = all_tasks.iter().any(|t| t.status == TaskStatus::Escalated);
            if has_escalated {
                let escalated: Vec<&Task> = all_tasks.iter()
                    .filter(|t| t.status == TaskStatus::Escalated)
                    .collect();

                for task in &escalated {
                    if reported_tasks.contains(&task.id) {
                        continue;
                    }
                    reported_tasks.insert(task.id.clone());

                    let task_num = task.id.split('_').last().unwrap_or("?");
                    let msg = format!(
                        "\n[SYNERGY] ⚠️ Task {} ESCALATED (gagal setelah {} percobaan). \
                         Title: {}. Perlu intervensi manual.\n",
                        task_num, task.attempt, task.title
                    );
                    {
                        let mut flow = session_flow.lock().await;
                        let _ = flow.send_to_leader(&msg).await;
                    }
                }
            }
        }
        eprintln!("[review-loop] stopped");
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
        // For OpenCode, run the `models` subcommand directly
        "opencode" => {
            let mut bin_path = resolve_adapter_bin("opencode");
            if let Some(bundled) = synergy_adapter::find_opencode_binary() {
                bin_path = bundled;
            }
            let output = tokio::process::Command::new(&bin_path)
                .arg("models")
                .output()
                .await
                .map_err(|e| format!("Failed to run opencode models: {e}"))?;
            let output_str = String::from_utf8_lossy(&output.stdout);
            let models = parse_leader_model_list(&output_str, &adapter_id);
            if models.is_empty() {
                Ok(known_models_for_provider(&adapter_id))
            } else {
                Ok(models)
            }
        }
        // CLI-based leaders: query via PTY
        "aider" | "claude-cli" | "codex-cli" | "antigravity" => {
            // Send model listing command
            let cmd = match adapter_id.as_str() {
                "claude-cli" => "/model",
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
            send_raw_to_leader,
            resize_leader_pty,
            restart_leader,
            check_opencode_update,
            install_warp,
            run_opencode_command,
            get_public_ip,
            get_leader_info,
            get_workers_proxy_info,
            approve_plan,
            get_session_flow_state,
            get_leader_status,
            get_git_changes,
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
