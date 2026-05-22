//! OpenCode adapter using `opencode run` (headless mode).
//!
//! Each user message invokes `opencode run --continue --agent <X> --model <Y>`
//! as a one-shot command. Synergy captures stdout, strips ANSI banners, and
//! delivers plain text to the chat UI.
//!
//! Session continuity is preserved via `--continue` after the first message.
//!
//! ## Per-instance state
//!
//! State (model, agent, session flag, output channel) lives in the
//! [`AppHandle`]'s `user_data` slot, NOT inside the adapter struct. This
//! allows the same `OpenCodeRunAdapter` value to drive multiple parallel
//! workers without trampling each other's state.

use crate::adapter::{AppAdapter, AppHandle, AppStatus, AppType, LaunchConfig};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex};

/// Marker sent on the output channel when the Leader/Worker starts thinking.
/// Frontend uses these to drive the status badge.
pub const BUSY_MARKER: &str = "\n\u{200B}__SYNERGY_BUSY__\u{200B}\n";
pub const IDLE_MARKER: &str = "\n\u{200B}__SYNERGY_IDLE__\u{200B}\n";
pub const NEW_TURN_MARKER: &str = "\n\u{200B}__SYNERGY_NEW_TURN__\u{200B}\n";

/// Matches any OpenCode CLI banner line — these all start with a `>` prefix
/// optionally preceded by whitespace/ANSI codes/tabs.
static BANNER_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\s*[>＞]\s").expect("banner regex"));

/// Per-handle state stored in `AppHandle.user_data`. Each call to
/// [`OpenCodeRunAdapter::launch`] returns a handle with its own state.
pub struct OpenCodeRunState {
    pub bin_path: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub has_session: AtomicBool,
    pub is_running: AtomicBool,
    pub output_tx: mpsc::UnboundedSender<String>,
    pub output_rx: Mutex<mpsc::UnboundedReceiver<String>>,
}

pub struct OpenCodeRunAdapter;

impl OpenCodeRunAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for OpenCodeRunAdapter {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse `--model X` and `--agent Y` flags from a `LaunchConfig.args` list.
fn parse_args(args: &[String]) -> (Option<String>, Option<String>) {
    let mut model = None;
    let mut agent = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--model" | "-m" if i + 1 < args.len() => {
                model = Some(args[i + 1].clone());
                i += 2;
            }
            "--agent" if i + 1 < args.len() => {
                agent = Some(args[i + 1].clone());
                i += 2;
            }
            _ => i += 1,
        }
    }
    (model, agent)
}

fn handle_state(handle: &AppHandle) -> Option<Arc<OpenCodeRunState>> {
    handle
        .user_data
        .as_ref()
        .and_then(|d| d.clone().downcast::<OpenCodeRunState>().ok())
}

fn is_running_flag_done(state: &Arc<OpenCodeRunState>) {
    state.is_running.store(false, Ordering::Relaxed);
}

#[async_trait]
impl AppAdapter for OpenCodeRunAdapter {
    fn id(&self) -> &str {
        "opencode"
    }

    fn display_name(&self) -> &str {
        "OpenCode (headless run mode)"
    }

    fn app_type(&self) -> AppType {
        AppType::Cli
    }

    async fn launch(&self, config: &LaunchConfig) -> Result<AppHandle> {
        let bin_path = config.bin_path.clone();
        if bin_path.is_empty() {
            return Err(anyhow!("OpenCode binary path is empty"));
        }

        let (parsed_model, parsed_agent) = parse_args(&config.args);

        // Default to a free model so users without paid auth can chat right away.
        let model = parsed_model.or_else(|| Some("opencode/deepseek-v4-flash-free".to_string()));

        let (output_tx, output_rx) = mpsc::unbounded_channel();

        let state = Arc::new(OpenCodeRunState {
            bin_path,
            cwd: config.cwd.clone(),
            model,
            agent: parsed_agent,
            has_session: AtomicBool::new(false),
            is_running: AtomicBool::new(false),
            output_tx,
            output_rx: Mutex::new(output_rx),
        });

        Ok(AppHandle {
            pty_session: None,
            window_hwnd: None,
            user_data: Some(state),
        })
    }

    async fn send_command(&self, handle: &mut AppHandle, text: &str) -> Result<()> {
        // Skip empty messages and lone Enter keypresses (xterm onData)
        let trimmed = text.trim_end_matches(|c: char| c == '\r' || c == '\n');
        if trimmed.is_empty() {
            return Ok(());
        }

        let state = handle_state(handle).ok_or_else(|| anyhow!("OpenCode adapter not launched"))?;

        let bin = state.bin_path.clone();
        let cwd = state.cwd.clone();
        let model = state.model.clone();
        let agent = state.agent.clone();
        let has_session = state.has_session.load(Ordering::Relaxed);
        let tx = state.output_tx.clone();
        let state_for_task = state.clone();

        // Mark that we now have a session for subsequent --continue calls
        state.has_session.store(true, Ordering::Relaxed);

        // Send the new-turn marker so frontend starts a new chat bubble.
        let _ = tx.send(NEW_TURN_MARKER.to_owned());
        // Mark busy → frontend status badge → "🟡 thinking"
        state.is_running.store(true, Ordering::Relaxed);
        let _ = tx.send(BUSY_MARKER.to_owned());

        let user_msg = trimmed.to_owned();
        let agent_clone = agent.clone();
        let model_clone = model.clone();

        eprintln!(
            "[opencode-run] spawning: {} run {}{}{} <{} bytes>",
            &bin,
            if has_session { "--continue " } else { "" },
            agent_clone
                .as_deref()
                .map(|a| format!("--agent {} ", a))
                .unwrap_or_default(),
            model_clone
                .as_deref()
                .map(|m| format!("--model {} ", m))
                .unwrap_or_default(),
            user_msg.len(),
        );

        tokio::spawn(async move {
            let mut cmd = Command::new(&bin);
            cmd.arg("run");
            if has_session {
                cmd.arg("--continue");
            }
            if let Some(m) = &model_clone {
                cmd.arg("--model").arg(m);
            }
            if let Some(a) = &agent_clone {
                cmd.arg("--agent").arg(a);
            }
            cmd.arg(&user_msg);
            if let Some(ref dir) = cwd {
                cmd.current_dir(dir);
            }
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());

            // Hide console window on Windows
            #[cfg(windows)]
            {
                cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
            }

            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(format!("\nError launching opencode: {}\n", e));
                    state_for_task.is_running.store(false, Ordering::Relaxed);
                    let _ = tx.send(IDLE_MARKER.to_owned());
                    return;
                }
            };

            let stdout_handle = if let Some(stdout) = child.stdout.take() {
                let tx_out = tx.clone();
                Some(tokio::spawn(async move {
                    let reader = BufReader::new(stdout);
                    let mut lines = reader.lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let clean = strip_ansi_for_check(&line);
                        if BANNER_RE.is_match(&clean) {
                            eprintln!("[opencode-run] skipping banner: {:?}", &line);
                            continue;
                        }
                        eprintln!("[opencode-run stdout] {}", &line);
                        let _ = tx_out.send(format!("{}\n", line));
                    }
                }))
            } else {
                None
            };

            let stderr_handle = if let Some(stderr) = child.stderr.take() {
                let tx_err = tx.clone();
                Some(tokio::spawn(async move {
                    let reader = BufReader::new(stderr);
                    let mut lines = reader.lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let clean = strip_ansi_for_check(&line);
                        if BANNER_RE.is_match(&clean)
                            || clean.contains("DEBUG")
                            || clean.trim().is_empty()
                        {
                            eprintln!("[opencode-run] skipping stderr: {:?}", &line);
                            continue;
                        }
                        eprintln!("[opencode-run stderr] {}", &line);
                        let _ = tx_err.send(format!("{}\n", line));
                    }
                }))
            } else {
                None
            };

            if let Some(h) = stdout_handle {
                let _ = h.await;
            }
            if let Some(h) = stderr_handle {
                let _ = h.await;
            }

            match child.wait().await {
                Ok(status) if status.success() => {
                    eprintln!("[opencode-run] completed successfully");
                }
                Ok(status) => {
                    let _ = tx.send(format!(
                        "\n[opencode exited with code {:?}]\n",
                        status.code()
                    ));
                }
                Err(e) => {
                    let _ = tx.send(format!("\n[wait error: {}]\n", e));
                }
            }

            is_running_flag_done(&state_for_task);
            let _ = tx.send(IDLE_MARKER.to_owned());
        });

        Ok(())
    }

    async fn send_raw(&self, handle: &mut AppHandle, data: &str) -> Result<()> {
        // Just delegate to send_command which trims trailing CR/LF.
        self.send_command(handle, data).await
    }

    async fn read_output(&self, handle: &mut AppHandle) -> Option<String> {
        let state = handle_state(handle)?;
        // try_recv only returns when there is actual data — if no data and
        // not running, the pump will see None and back off.
        let mut rx = state.output_rx.lock().await;
        rx.try_recv().ok()
    }

    async fn detect_status(&self, output_buffer: &str) -> AppStatus {
        // Check the output buffer for our markers — the orchestrator passes
        // the accumulated buffer for each worker. The worker is "Done" once
        // it has emitted IDLE after BUSY (i.e. at least one round-trip).
        let last_busy = output_buffer.rfind(BUSY_MARKER);
        let last_idle = output_buffer.rfind(IDLE_MARKER);
        match (last_busy, last_idle) {
            (Some(b), Some(i)) if i > b => AppStatus::Done,
            (Some(_), _) => AppStatus::Working,
            _ => AppStatus::Idle,
        }
    }
}

/// Strip ANSI escape codes from a string for filter-matching purposes.
fn strip_ansi_for_check(s: &str) -> String {
    static ANSI_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"\x1b\[[0-9;?]*[a-zA-Z]|\x1b\][^\x07]*\x07").unwrap()
    });
    ANSI_RE.replace_all(s, "").to_string()
}

/// Resolve the OpenCode binary path. Checks bundled location first.
pub fn find_opencode_binary() -> Option<String> {
    // 1. Bundled binary next to the executable
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let bundled = dir.join("binaries").join(if cfg!(windows) {
                "opencode-x86_64-pc-windows-msvc.exe"
            } else {
                "opencode"
            });
            if bundled.exists() {
                return bundled.to_str().map(|s| s.to_owned());
            }
            let sibling = dir.join(if cfg!(windows) { "opencode.exe" } else { "opencode" });
            if sibling.exists() {
                return sibling.to_str().map(|s| s.to_owned());
            }
        }
    }

    // 2. PATH lookup
    let bin_name = if cfg!(windows) { "opencode.exe" } else { "opencode" };
    if let Ok(path_var) = std::env::var("PATH") {
        let sep = if cfg!(windows) { ';' } else { ':' };
        for dir in path_var.split(sep) {
            let candidate = PathBuf::from(dir).join(bin_name);
            if candidate.exists() {
                return candidate.to_str().map(|s| s.to_owned());
            }
        }
    }

    None
}

/// Read busy state from a launched OpenCodeRunAdapter handle.
pub fn handle_is_running(handle: &AppHandle) -> bool {
    handle_state(handle)
        .map(|s| s.is_running.load(Ordering::Relaxed))
        .unwrap_or(false)
}
