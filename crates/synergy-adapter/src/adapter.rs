use async_trait::async_trait;
use anyhow::Result;
use std::any::Any;
use std::sync::Arc;
use synergy_pty::PtySession;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppType {
    Cli,
    Gui,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppStatus {
    Idle,
    Working,
    Error(String),
    Done,
}

#[derive(Debug, Clone)]
pub struct LaunchConfig {
    pub bin_path: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub proxy_addr: Option<String>,
}

pub struct AppHandle {
    pub pty_session: Option<PtySession>,
    pub window_hwnd: Option<usize>,
    /// Adapter-specific per-instance state (e.g. OpenCodeRunState).
    /// Each call to `adapter.launch()` returns a fresh handle with its own
    /// state, so workers don't trample each other.
    pub user_data: Option<Arc<dyn Any + Send + Sync>>,
}

impl AppHandle {
    pub fn empty() -> Self {
        Self {
            pty_session: None,
            window_hwnd: None,
            user_data: None,
        }
    }
}

#[async_trait]
pub trait AppAdapter: Send + Sync {
    fn id(&self) -> &str;
    fn display_name(&self) -> &str;
    fn app_type(&self) -> AppType;

    async fn launch(&self, config: &LaunchConfig) -> Result<AppHandle>;
    async fn send_command(&self, handle: &mut AppHandle, text: &str) -> Result<()>;
    async fn send_raw(&self, handle: &mut AppHandle, data: &str) -> Result<()> {
        self.send_command(handle, data).await
    }
    async fn read_output(&self, handle: &mut AppHandle) -> Option<String>;
    async fn detect_status(&self, output_buffer: &str) -> AppStatus;

    /// Resize the underlying PTY (if any). Default implementation is a no-op.
    async fn resize_pty(&self, handle: &mut AppHandle, rows: u16, cols: u16) -> Result<()> {
        if let Some(ref pty) = handle.pty_session {
            pty.resize(rows, cols)?;
        }
        let _ = (rows, cols);
        Ok(())
    }
}
