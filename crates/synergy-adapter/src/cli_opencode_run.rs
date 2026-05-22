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

/// Free models to try in order. If one times out (rate limited), try the next.
/// This is populated dynamically at runtime from `opencode models` output.
/// If `opencode models` also fails, uses last-model.txt as the only option.
/// NO HARDCODED FALLBACK — all models come from OpenCode's live list.

/// Timeout in seconds before declaring a model rate-limited and trying the next.
/// Only triggers if there is ZERO output (reasoning/streaming resets the timer).
const MODEL_TIMEOUT_SECS: u64 = 20;

/// Cached list of free models fetched from `opencode models`.
static FREE_MODEL_CACHE: Lazy<Mutex<Vec<String>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// Per-handle state stored in `AppHandle.user_data`. Each call to
/// [`OpenCodeRunAdapter::launch`] returns a handle with its own state.
pub struct OpenCodeRunState {
    pub bin_path: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub proxy_addr: Option<String>,
    pub has_session: AtomicBool,
    pub is_running: AtomicBool,
    pub output_tx: mpsc::UnboundedSender<String>,
    pub output_rx: Mutex<mpsc::UnboundedReceiver<String>>,
    /// Index into free model list for auto-switch on rate limit.
    pub model_index: std::sync::atomic::AtomicU32,
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

        // Model index starts at 0 — will be resolved dynamically in send_command
        let model_idx = 0u32;

        let (output_tx, output_rx) = mpsc::unbounded_channel();

        let state = Arc::new(OpenCodeRunState {
            bin_path,
            cwd: config.cwd.clone(),
            model,
            agent: parsed_agent,
            proxy_addr: config.proxy_addr.clone(),
            has_session: AtomicBool::new(false),
            is_running: AtomicBool::new(false),
            output_tx,
            output_rx: Mutex::new(output_rx),
            model_index: std::sync::atomic::AtomicU32::new(model_idx),
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
        let agent = state.agent.clone();
        let proxy_addr = state.proxy_addr.clone();
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

        tokio::spawn(async move {
            // Fetch available free models dynamically
            let free_models = get_free_models(&bin, &cwd).await;
            let max_attempts = free_models.len();
            let mut attempt = 0;

            loop {
                // Pick current model from rotation
                let model_idx = state_for_task.model_index.load(Ordering::Relaxed) as usize;
                let current_model = free_models[model_idx % free_models.len()].clone();

                eprintln!(
                    "[opencode-run] attempt {}/{} model={} msg=<{} bytes>",
                    attempt + 1, max_attempts, &current_model, user_msg.len(),
                );

                let mut cmd = Command::new(&bin);
                cmd.arg("run");
                if has_session && attempt == 0 {
                    cmd.arg("--continue");
                }
                cmd.arg("--model").arg(&current_model);
                if let Some(ref a) = agent {
                    cmd.arg("--agent").arg(a);
                }
                cmd.arg(&user_msg);
                if let Some(ref dir) = cwd {
                    cmd.current_dir(dir);
                }
                cmd.stdout(Stdio::piped());
                cmd.stderr(Stdio::piped());

                // Set proxy: prioritize configured proxy, fallback to WARP if available
                if let Some(ref addr) = proxy_addr {
                    if !addr.is_empty() {
                        cmd.env("HTTP_PROXY", addr);
                        cmd.env("HTTPS_PROXY", addr);
                        if attempt == 0 {
                            eprintln!("[opencode-run] using configured proxy: {}", addr);
                        }
                    }
                } else if detect_warp_proxy().await {
                    cmd.env("HTTP_PROXY", "socks5://127.0.0.1:40000");
                    cmd.env("HTTPS_PROXY", "socks5://127.0.0.1:40000");
                    if attempt == 0 {
                        eprintln!("[opencode-run] WARP proxy detected, routing via 127.0.0.1:40000");
                    }
                }

                // Hide console window on Windows
                #[cfg(windows)]
                {
                    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
                }

                let mut child = match cmd.spawn() {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx.send(format!("\nError launching opencode: {}\n", e));
                        break;
                    }
                };

                // Track if we got any real output (not just banners)
                let got_output = Arc::new(AtomicBool::new(false));
                // Track last time we received output (for smart timeout)
                let last_output_time = Arc::new(Mutex::new(tokio::time::Instant::now()));

                let stdout_handle = if let Some(stdout) = child.stdout.take() {
                    let tx_out = tx.clone();
                    let got = got_output.clone();
                    let last_time = last_output_time.clone();
                    Some(tokio::spawn(async move {
                        let reader = BufReader::new(stdout);
                        let mut lines = reader.lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            let clean = strip_ansi_for_check(&line);
                            if BANNER_RE.is_match(&clean) {
                                continue;
                            }
                            // Don't count provider errors as "real output"
                            if clean.contains("ProviderModelNotFoundError")
                                || clean.contains("ModelNotFoundError")
                            {
                                eprintln!("[opencode-run] model error in stdout: {}", &clean);
                                // Still forward to user so they see what happened
                                let _ = tx_out.send(format!("{}\n", line));
                                continue;
                            }
                            got.store(true, Ordering::Relaxed);
                            *last_time.lock().await = tokio::time::Instant::now();
                            let _ = tx_out.send(format!("{}\n", line));
                        }
                    }))
                } else {
                    None
                };

                let stderr_handle = if let Some(stderr) = child.stderr.take() {
                    let tx_err = tx.clone();
                    let got = got_output.clone();
                    let last_time = last_output_time.clone();
                    Some(tokio::spawn(async move {
                        let reader = BufReader::new(stderr);
                        let mut lines = reader.lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            let clean = strip_ansi_for_check(&line);
                            if BANNER_RE.is_match(&clean)
                                || clean.contains("DEBUG")
                                || clean.trim().is_empty()
                            {
                                continue;
                            }
                            // Don't forward provider errors to user — they trigger model switch
                            if clean.contains("ProviderModelNotFoundError")
                                || clean.contains("ModelNotFoundError")
                                || clean.contains("providerID")
                            {
                                eprintln!("[opencode-run] model error (will switch): {}", &clean);
                                continue;
                            }
                            got.store(true, Ordering::Relaxed);
                            *last_time.lock().await = tokio::time::Instant::now();
                            let _ = tx_err.send(format!("{}\n", line));
                        }
                    }))
                } else {
                    None
                };

                // Smart timeout: only timeout if NO output received for MODEL_TIMEOUT_SECS.
                // If model is actively streaming (reasoning), we keep waiting.
                let timed_out;
                loop {
                    let check_interval = tokio::time::Duration::from_secs(5);
                    match tokio::time::timeout(check_interval, child.wait()).await {
                        Ok(Ok(status)) => {
                            // Process finished
                            timed_out = false;

                            if let Some(h) = stdout_handle { let _ = h.await; }
                            if let Some(h) = stderr_handle { let _ = h.await; }

                            if got_output.load(Ordering::Relaxed) {
                                eprintln!("[opencode-run] ✓ model {} responded successfully", &current_model);
                                save_last_working_model(&cwd, &current_model).await;
                            } else if !status.success() {
                                eprintln!("[opencode-run] model {} failed with code {:?}", &current_model, status.code());
                            } else {
                                eprintln!("[opencode-run] model {} returned empty (rate limited?)", &current_model);
                            }
                            break;
                        }
                        Ok(Err(e)) => {
                            eprintln!("[opencode-run] wait error: {e}");
                            timed_out = false;
                            if let Some(h) = stdout_handle { let _ = h.await; }
                            if let Some(h) = stderr_handle { let _ = h.await; }
                            break;
                        }
                        Err(_) => {
                            // Check interval elapsed — see if we should keep waiting
                            let elapsed_since_output = {
                                let t = last_output_time.lock().await;
                                t.elapsed().as_secs()
                            };

                            if !got_output.load(Ordering::Relaxed) && elapsed_since_output >= MODEL_TIMEOUT_SECS {
                                // No output at all for MODEL_TIMEOUT_SECS — rate limited
                                eprintln!(
                                    "[opencode-run] TIMEOUT ({}s no output) on model {}",
                                    MODEL_TIMEOUT_SECS, &current_model
                                );
                                timed_out = true;
                                let _ = child.kill().await;
                                if let Some(h) = stdout_handle { h.abort(); }
                                if let Some(h) = stderr_handle { h.abort(); }
                                break;
                            }
                            // Model is actively producing output — keep waiting
                            continue;
                        }
                    }
                }

                if !timed_out && got_output.load(Ordering::Relaxed) {
                    // Success — we're done
                    break;
                }

                if !timed_out && !got_output.load(Ordering::Relaxed) {
                    // Process exited but no output — treat as rate limited, try next
                }

                // Switch to next model
                attempt += 1;
                if attempt >= max_attempts {
                    let _ = tx.send(format!(
                        "\n⚠️ All {} free models timed out or rate-limited. Try again later or configure a proxy.\n",
                        max_attempts
                    ));
                    break;
                }

                let next_idx = (model_idx + 1) % free_models.len();
                state_for_task.model_index.store(next_idx as u32, Ordering::Relaxed);
                let next_model = &free_models[next_idx];
                let _ = tx.send(format!(
                    "\n⟳ Model {} rate-limited/timeout. Switching to {}...\n",
                    &current_model, next_model
                ));
                eprintln!("[opencode-run] switching to model {}", next_model);

                // Small delay before retry
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
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

/// Detect if Cloudflare WARP is running by checking if its SOCKS5 proxy
/// port (40000) is listening on localhost. This is a fast TCP connect check.
async fn detect_warp_proxy() -> bool {
    use tokio::net::TcpStream;
    match tokio::time::timeout(
        tokio::time::Duration::from_millis(100),
        TcpStream::connect("127.0.0.1:40000"),
    )
    .await
    {
        Ok(Ok(_)) => true,
        _ => false,
    }
}

/// Save the last model that successfully responded to .synergy/last-model.txt
async fn save_last_working_model(cwd: &Option<String>, model: &str) {
    let dir = match cwd {
        Some(d) => format!("{}/.synergy", d),
        None => return,
    };
    let _ = tokio::fs::create_dir_all(&dir).await;
    let path = format!("{}/last-model.txt", &dir);
    let _ = tokio::fs::write(&path, model).await;
    eprintln!("[opencode-run] saved last working model: {}", model);
}

/// Load the last working model from .synergy/last-model.txt
fn load_last_working_model(cwd: &Option<String>) -> Option<String> {
    let dir = cwd.as_ref()?;
    let path = format!("{}/.synergy/last-model.txt", dir);
    std::fs::read_to_string(&path).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

/// Fetch the list of free models from `opencode models` command.
/// Caches the result so subsequent calls are instant.
/// All models come from `opencode models` — NO hardcoded fallback.
/// If fetch fails, uses last-model.txt as the only option.
/// The last working model is always placed first in the list.
async fn get_free_models(bin_path: &str, cwd: &Option<String>) -> Vec<String> {
    // Check cache first
    {
        let cache = FREE_MODEL_CACHE.lock().await;
        if !cache.is_empty() {
            return cache.clone();
        }
    }

    // Fetch from opencode models
    eprintln!("[opencode-run] fetching free model list from `opencode models`...");
    let mut models: Vec<String> = Vec::new();

    if let Ok(Ok(output)) = tokio::time::timeout(
        tokio::time::Duration::from_secs(10),
        tokio::process::Command::new(bin_path)
            .arg("models")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output(),
    )
    .await
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty()
                || trimmed.starts_with("Available")
                || trimmed.starts_with("Model")
                || trimmed.starts_with("Current")
                || trimmed.starts_with("---")
            {
                continue;
            }

            let cleaned = trimmed
                .trim_start_matches(|c: char| {
                    c.is_ascii_digit() || c == '.' || c == ')' || c == '*' || c == '>' || c == '-' || c == ' '
                })
                .trim();

            if let Some(model_id) = cleaned.split_whitespace().next() {
                // Any model listed by `opencode models` is available to use
                if model_id.contains('/') && model_id.len() > 5 {
                    if !models.contains(&model_id.to_owned()) {
                        models.push(model_id.to_owned());
                    }
                }
            }
        }
    } else {
        eprintln!("[opencode-run] `opencode models` timed out or failed");
    }

    if models.is_empty() {
        eprintln!("[opencode-run] no models from `opencode models`, using last-model.txt only");
        if let Some(last) = load_last_working_model(cwd) {
            models.push(last);
        }
    } else {
        eprintln!("[opencode-run] found {} free models: {:?}", models.len(), &models);
    }

    // Prioritize last working model (put it first in the list)
    if models.len() > 1 {
        if let Some(last) = load_last_working_model(cwd) {
            if !last.is_empty() {
                models.retain(|m| m != &last);
                models.insert(0, last.clone());
                eprintln!("[opencode-run] prioritizing last working model: {}", &last);
            }
        }
    }

    // Cache the result
    {
        let mut cache = FREE_MODEL_CACHE.lock().await;
        *cache = models.clone();
    }

    models
}

/// Ensure Cloudflare WARP is active in proxy mode.
/// If WARP is installed but not running, try to connect and enable proxy mode.
/// Does NOT auto-download/install — user must install WARP manually from https://1.1.1.1/
/// Returns true if WARP proxy is available after this call.
pub async fn ensure_warp_installed() -> bool {
    // Check if already running
    if detect_warp_proxy().await {
        return true;
    }

    // Check if warp-cli exists
    let warp_cli = if cfg!(windows) {
        r"C:\Program Files\Cloudflare\Cloudflare WARP\warp-cli.exe"
    } else {
        "warp-cli"
    };

    if !std::path::Path::new(warp_cli).exists() {
        eprintln!("[warp] WARP not installed. Install from https://1.1.1.1/");
        return false;
    }

    // Installed but not running in proxy mode — try to enable
    eprintln!("[warp] WARP installed, enabling proxy mode...");
    let _ = tokio::process::Command::new(warp_cli)
        .args(["set-mode", "proxy"])
        .output()
        .await;
    let _ = tokio::process::Command::new(warp_cli)
        .args(["connect"])
        .output()
        .await;
    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    detect_warp_proxy().await
}
