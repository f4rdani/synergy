use async_trait::async_trait;
use anyhow::Result;
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
}

#[async_trait]
pub trait AppAdapter: Send + Sync {
    fn id(&self) -> &str;
    fn display_name(&self) -> &str;
    fn app_type(&self) -> AppType;

    async fn launch(&self, config: &LaunchConfig) -> Result<AppHandle>;
    async fn send_command(&self, handle: &mut AppHandle, text: &str) -> Result<()>;
    async fn read_output(&self, handle: &mut AppHandle) -> Option<String>;
    async fn detect_status(&self, output_buffer: &str) -> AppStatus;
}
