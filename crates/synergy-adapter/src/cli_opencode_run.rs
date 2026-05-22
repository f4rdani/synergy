//! OpenCode adapter using `opencode run` (headless mode).
//!
//! Instead of spawning the interactive TUI in a PTY, each user message
//! invokes `opencode run --continue "<message>"` as a one-shot command.
//! Synergy captures stdout, strips ANSI, and returns plain text — perfect
//! for a chat-style UI like Telegram bots.
//!
//! Session continuity is preserved by always passing `--continue` after
//! the first message.

use crate::adapter::{AppAdapter, AppHandle, AppStatus, AppType, LaunchConfig};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex};

/// Matches any OpenCode CLI banner line — these all start with a `>` prefix
/// optionally preceded by whitespace/ANSI codes/tabs.
/// Examples filtered:
///   "> build · model-name"
///   ">  cwd: /path"
///   "> session: abc123"
static BANNER_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^\s*[>＞]\s")
        .expect("banner regex")
});

/// Holds the OpenCode binary path + cwd + a continuation flag.
pub struct OpenCodeRunState {
    pub bin_path: String,
    pub cwd: Option<String>,
    pub has_session: bool,
    pub model: Option<String>,
    /// Channel that receives output chunks from running `opencode run` calls.
    pub output_tx: mpsc::UnboundedSender<String>,
    pub output_rx: Arc<Mutex<mpsc::UnboundedReceiver<String>>>,
}

pub struct OpenCodeRunAdapter {
    pub state: Arc<Mutex<Option<OpenCodeRunState>>>,
}

impl OpenCodeRunAdapter {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(None)),
        }
    }
}

impl Default for OpenCodeRunAdapter {
    fn default() -> Self {
        Self::new()
    }
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
        // Verify the binary exists
        let bin_path = config.bin_path.clone();
        if bin_path.is_empty() {
            return Err(anyhow!("OpenCode binary path is empty"));
        }

        // For run mode we don't spawn anything yet — we just store the config.
        // The actual `opencode run` calls happen in send_command.
        let (output_tx, output_rx) = mpsc::unbounded_channel();

        // Default to a free model so users without paid auth can chat right away.
        // User can change via set_model() later.
        let default_model = Some("opencode/deepseek-v4-flash-free".to_string());

        let new_state = OpenCodeRunState {
            bin_path,
            cwd: config.cwd.clone(),
            has_session: false,
            model: default_model,
            output_tx,
            output_rx: Arc::new(Mutex::new(output_rx)),
        };

        *self.state.lock().await = Some(new_state);

        // Send a welcome banner so the UI shows something immediately.
        if let Some(ref s) = *self.state.lock().await {
            let _ = s.output_tx.send(format!(
                "OpenCode connected (model: {}). Type a message to start chatting.\n",
                s.model.as_deref().unwrap_or("default")
            ));
        }

        Ok(AppHandle {
            pty_session: None,
            window_hwnd: None,
        })
    }

    async fn send_command(&self, _handle: &mut AppHandle, text: &str) -> Result<()> {
        // Skip empty messages and lone Enter keypresses (xterm onData)
        let trimmed = text.trim_end_matches(|c: char| c == '\r' || c == '\n');
        if trimmed.is_empty() {
            return Ok(());
        }

        let state_arc = self.state.clone();
        let mut state_guard = state_arc.lock().await;
        let state = state_guard.as_mut().ok_or_else(|| anyhow!("OpenCode adapter not launched"))?;

        let bin_path = state.bin_path.clone();
        let cwd = state.cwd.clone();
        let model = state.model.clone();
        let has_session = state.has_session;
        let tx = state.output_tx.clone();

        // Mark that we now have a session for subsequent --continue calls
        state.has_session = true;
        drop(state_guard);

        // Send a marker that this is a new response, frontend uses this to
        // separate the previous response from the new one.
        let _ = tx.send("\n\u{200B}__SYNERGY_NEW_TURN__\u{200B}\n".to_owned());

        // For first message, prepend a system context (OS + project info)
        // so the AI knows what environment it's working in.
        let user_msg = if !has_session {
            let os = if cfg!(windows) { "Windows" }
                     else if cfg!(target_os = "macos") { "macOS" }
                     else { "Linux" };
            let project = cwd.as_deref().unwrap_or(".");
            format!(
                "[Context] OS: {os} | Project directory: {project} | Workers: 6 OpenCode instances available for parallel task execution.\n\n[User message] {}",
                trimmed
            )
        } else {
            trimmed.to_owned()
        };

        let bin = bin_path.clone();
        let cwd_clone = cwd.clone();
        let model_clone = model.clone();

        eprintln!("[opencode-run] spawning: {} run {}", &bin,
            if has_session { "--continue <msg>" } else { "<msg>" });

        tokio::spawn(async move {
            let mut cmd = Command::new(&bin);
            cmd.arg("run");
            if has_session {
                cmd.arg("--continue");
            }
            if let Some(m) = &model_clone {
                cmd.arg("--model").arg(m);
            }
            cmd.arg(&user_msg);
            if let Some(ref dir) = cwd_clone {
                cmd.current_dir(dir);
            }
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());

            // Hide console window on Windows
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
            }

            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(format!("\nError launching opencode: {}\n", e));
                    return;
                }
            };

            // Stream stdout line by line
            let stdout_handle = if let Some(stdout) = child.stdout.take() {
                let tx_out = tx.clone();
                Some(tokio::spawn(async move {
                    let reader = BufReader::new(stdout);
                    let mut lines = reader.lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        // Strip ANSI codes for filter check
                        let clean = strip_ansi_for_check(&line);
                        // Filter all OpenCode CLI banner lines (start with ">").
                        // The line may have ANSI codes around the ">" so we
                        // strip those before checking.
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
                        // Filter banner lines (> build · model, etc) and DEBUG noise
                        if BANNER_RE.is_match(&clean) || clean.contains("DEBUG") || clean.trim().is_empty() {
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

            // Wait for stdout/stderr readers to finish
            if let Some(h) = stdout_handle { let _ = h.await; }
            if let Some(h) = stderr_handle { let _ = h.await; }

            match child.wait().await {
                Ok(status) if status.success() => {
                    eprintln!("[opencode-run] completed successfully");
                }
                Ok(status) => {
                    let _ = tx.send(format!("\n[opencode exited with code {:?}]\n", status.code()));
                }
                Err(e) => {
                    let _ = tx.send(format!("\n[wait error: {}]\n", e));
                }
            }
        });

        Ok(())
    }

    async fn send_raw(&self, handle: &mut AppHandle, data: &str) -> Result<()> {
        // Buffer raw keystrokes — when Enter is pressed, treat the buffer as a message.
        // Simpler: just delegate to send_command which handles the Enter trim.
        self.send_command(handle, data).await
    }

    async fn read_output(&self, _handle: &mut AppHandle) -> Option<String> {
        let state_guard = self.state.lock().await;
        let state = state_guard.as_ref()?;
        let rx_arc = state.output_rx.clone();
        drop(state_guard);

        let mut rx = rx_arc.lock().await;
        rx.try_recv().ok()
    }

    async fn detect_status(&self, _output_buffer: &str) -> AppStatus {
        AppStatus::Idle
    }
}

/// Helper to set the model for the active OpenCodeRunAdapter.
impl OpenCodeRunAdapter {
    pub async fn set_model(&self, model: impl Into<String>) {
        if let Some(ref mut s) = *self.state.lock().await {
            s.model = Some(model.into());
        }
    }

    pub async fn get_bin_path(&self) -> Option<String> {
        self.state.lock().await.as_ref().map(|s| s.bin_path.clone())
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
            // Also check sibling "opencode" or "opencode.exe"
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
