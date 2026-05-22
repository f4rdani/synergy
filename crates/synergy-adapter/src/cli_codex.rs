//! Codex CLI adapter.
//! Codex is OpenAI's coding CLI. It works like a terminal tool.
//! It can also be run as an IDE-integrated agent.

use crate::adapter::{AppAdapter, AppHandle, AppStatus, AppType, LaunchConfig};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use synergy_pty::PtySession;

pub struct CodexAdapter;

#[async_trait]
impl AppAdapter for CodexAdapter {
    fn id(&self) -> &str {
        "codex-cli"
    }

    fn display_name(&self) -> &str {
        "Codex CLI"
    }

    fn app_type(&self) -> AppType {
        AppType::Cli
    }

    async fn launch(&self, config: &LaunchConfig) -> Result<AppHandle> {
        let bin = if config.bin_path.is_empty() {
            "codex"
        } else {
            &config.bin_path
        };
        let args_ref: Vec<&str> = config.args.iter().map(|s| s.as_str()).collect();

        let mut envs = Vec::new();
        if let Some(ref proxy) = config.proxy_addr {
            envs.push(("HTTP_PROXY".to_string(), proxy.clone()));
            envs.push(("HTTPS_PROXY".to_string(), proxy.clone()));
        }

        let pty = PtySession::spawn_with_env(bin, &args_ref, config.cwd.as_deref(), envs)?;
        Ok(AppHandle {
            pty_session: Some(pty),
            window_hwnd: None,
            user_data: None,
        })
    }

    async fn send_command(&self, handle: &mut AppHandle, text: &str) -> Result<()> {
        if let Some(ref mut pty) = handle.pty_session {
            pty.write(&(text.to_owned() + "\r"))?;
            Ok(())
        } else {
            Err(anyhow!("No PTY session"))
        }
    }

    async fn read_output(&self, handle: &mut AppHandle) -> Option<String> {
        handle.pty_session.as_mut()?.try_read()
    }

    async fn detect_status(&self, output_buffer: &str) -> AppStatus {
        let tail = if output_buffer.len() > 1024 {
            &output_buffer[output_buffer.len() - 1024..]
        } else {
            output_buffer
        };
        if tail.contains("completed") || tail.contains("Done") {
            return AppStatus::Done;
        }
        if tail.contains("error") || tail.contains("Error:") {
            return AppStatus::Error("error detected".into());
        }
        if tail.trim_end().ends_with('>') || tail.trim_end().ends_with('$') {
            return AppStatus::Idle;
        }
        AppStatus::Working
    }
}
