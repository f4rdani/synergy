//! Tauri command surface and state for the Synergy desktop shell.
//!
//! The UI process owns:
//! * the [`Database`] connection (single writer, reads through a tokio Mutex)
//! * the running [`Orchestrator`] with its workers and ticker task
//!
//! Commands are intentionally chunky (init/get/add) so the JS frontend can
//! poll a single endpoint to refresh the dashboard, while a parallel Tauri
//! event stream pushes worker terminal output for real-time rendering.

use anyhow::Context;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use synergy_adapter::{AppAdapter, AppStatus, GenericCliAdapter, OpenCodeAdapter};
use synergy_config::SynergyConfig;
use synergy_core::{Orchestrator, WorkerOutput};
use synergy_db::Database;
use synergy_proto::{EventLog, Task, TaskStatus};
use synergy_proxy::ProxyManager;
use tauri::{AppHandle, Builder, Emitter, Manager, Runtime, State};
use tokio::sync::Mutex;

pub struct AppState {
    pub orchestrator: Arc<Mutex<Option<Arc<Orchestrator>>>>,
    pub db: Arc<Mutex<Option<Arc<Mutex<Database>>>>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            orchestrator: Arc::new(Mutex::new(None)),
            db: Arc::new(Mutex::new(None)),
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
        end_current_session
    ])
}
