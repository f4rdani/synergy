use crate::adapter::{AppAdapter, AppHandle, AppStatus, AppType, LaunchConfig};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use synergy_pty::PtySession;

/// Generic CLI / shell adapter.
///
/// Used for plain shells (`pwsh`, `bash`, `cmd`) or any tool that follows the
/// "prompt then output then prompt" pattern. Detection is intentionally
/// lighter than [`crate::OpenCodeAdapter`]: we only look for shell prompts
/// and unmistakable error markers.
pub struct GenericCliAdapter {
    id: String,
    display_name: String,
}

impl GenericCliAdapter {
    pub fn new(id: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            display_name: display_name.into(),
        }
    }
}

impl Default for GenericCliAdapter {
    fn default() -> Self {
        Self::new("cli-generic", "Generic CLI")
    }
}

static SHELL_PROMPT_RE: Lazy<Regex> = Lazy::new(|| {
    // Detect shell prompts. On Windows ConPTY, cursor-positioning escapes
    // replace newlines, so after ANSI stripping the prompt may be glued to
    // the preceding output. We therefore also match a drive-letter prompt
    // (`C:\...>`) anywhere in the tail, not just at line start.
    Regex::new(r"(?m)(?:(?:^|\n)\s*(?:PS\s+[^\n]*?>|\$|>|#)|[A-Z]:\\[^\n]*?>)\s*$")
        .expect("prompt regex")
});

static FATAL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?im)^\s*(fatal:|panic:|exit code [1-9])").expect("fatal regex"));

#[async_trait]
impl AppAdapter for GenericCliAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    fn app_type(&self) -> AppType {
        AppType::Cli
    }

    async fn launch(&self, config: &LaunchConfig) -> Result<AppHandle> {
        let args_ref: Vec<&str> = config.args.iter().map(|s| s.as_str()).collect();

        let mut envs = Vec::new();
        if let Some(ref proxy) = config.proxy_addr {
            envs.push(("HTTP_PROXY".to_string(), proxy.clone()));
            envs.push(("HTTPS_PROXY".to_string(), proxy.clone()));
            envs.push(("ALL_PROXY".to_string(), proxy.clone()));
        }

        let pty = PtySession::spawn_with_env(
            &config.bin_path,
            &args_ref,
            config.cwd.as_deref(),
            envs,
        )?;

        Ok(AppHandle {
            pty_session: Some(pty),
            window_hwnd: None,
        })
    }

    async fn send_command(&self, handle: &mut AppHandle, text: &str) -> Result<()> {
        if let Some(ref mut pty) = handle.pty_session {
            pty.write(&(text.to_owned() + "\r"))?;
            Ok(())
        } else {
            Err(anyhow!("No PTY session active"))
        }
    }

    async fn read_output(&self, handle: &mut AppHandle) -> Option<String> {
        if let Some(ref mut pty) = handle.pty_session {
            pty.try_read()
        } else {
            None
        }
    }

    async fn detect_status(&self, output_buffer: &str) -> AppStatus {
        let clean = crate::cli_opencode::OpenCodeAdapter::strip_ansi(output_buffer);
        let trimmed = clean.trim_end();
        if trimmed.is_empty() {
            return AppStatus::Idle;
        }

        let tail = if trimmed.len() > 2048 {
            &trimmed[trimmed.len() - 2048..]
        } else {
            trimmed
        };

        if FATAL_RE.is_match(tail) {
            return AppStatus::Error("fatal pattern detected".to_owned());
        }

        if SHELL_PROMPT_RE.is_match(tail) {
            return AppStatus::Idle;
        }

        AppStatus::Working
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pwsh_prompt_is_idle() {
        let a = GenericCliAdapter::default();
        let s = a.detect_status("foo\nPS C:\\Users\\dev>").await;
        assert!(matches!(s, AppStatus::Idle));
    }

    #[tokio::test]
    async fn bash_prompt_is_idle() {
        let a = GenericCliAdapter::default();
        let s = a.detect_status("running\n$").await;
        assert!(matches!(s, AppStatus::Idle));
    }

    #[tokio::test]
    async fn cmd_prompt_glued_is_idle() {
        // ConPTY strips newlines; prompt is glued to output
        let a = GenericCliAdapter::default();
        let s = a.detect_status("synergy_worksC:\\Users\\->").await;
        assert!(matches!(s, AppStatus::Idle), "got {:?}", s);
    }

    #[tokio::test]
    async fn fatal_is_error() {
        let a = GenericCliAdapter::default();
        let s = a.detect_status("hello\nfatal: not a git repository").await;
        assert!(matches!(s, AppStatus::Error(_)));
    }
}
