use crate::adapter::{AppAdapter, AppHandle, AppStatus, AppType, LaunchConfig};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use synergy_pty::PtySession;

/// OpenCode-specific patterns for status detection.
///
/// Designed conservatively: we never flag `Working` based purely on the
/// presence of an "error" keyword inside a partial buffer (tools often print
/// `error` inside diffs or test output without actually failing).
static IDLE_PROMPT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)(^|\n)\s*[>$#]\s*$").expect("idle regex"));
static ERROR_LINE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?im)^\s*(error[: ]|fatal[: ]|panic:|\[ERROR\]|exit code [1-9]|command not found)",
    )
    .expect("error regex")
});
static DONE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(task\s+complete|completed\s+successfully|created\s+successfully|all\s+tests\s+passed|✓\s*done)")
        .expect("done regex")
});

pub struct OpenCodeAdapter;

impl OpenCodeAdapter {
    /// Strip ANSI escape sequences for accurate text matching.
    pub fn strip_ansi(input: &str) -> String {
        match strip_ansi_escapes::strip_str(input) {
            s if !s.is_empty() => s,
            _ => input.to_owned(),
        }
    }
}

#[async_trait]
impl AppAdapter for OpenCodeAdapter {
    fn id(&self) -> &str {
        "opencode"
    }

    fn display_name(&self) -> &str {
        "OpenCode CLI"
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
            // Use \r — works for both Windows ConPTY and Unix PTYs.
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

    /// Detect the current logical state from a *trailing* slice of output.
    ///
    /// The orchestrator passes the live tail of the worker's output buffer.
    /// Order of checks matters: explicit "done" wins over an "error" keyword
    /// that may legitimately appear in diff context.
    async fn detect_status(&self, output_buffer: &str) -> AppStatus {
        let clean = Self::strip_ansi(output_buffer);
        let trimmed = clean.trim_end();
        if trimmed.is_empty() {
            return AppStatus::Idle;
        }

        // Look only at the last 2 KB to avoid re-matching old errors forever.
        let tail = if trimmed.len() > 2048 {
            &trimmed[trimmed.len() - 2048..]
        } else {
            trimmed
        };

        if DONE_RE.is_match(tail) {
            return AppStatus::Done;
        }

        if ERROR_LINE_RE.is_match(tail) {
            // Capture the offending line for better diagnostics.
            let line = ERROR_LINE_RE
                .find(tail)
                .map(|m| m.as_str().trim().to_owned())
                .unwrap_or_else(|| "error pattern detected".to_owned());
            return AppStatus::Error(line);
        }

        if IDLE_PROMPT_RE.is_match(tail) {
            return AppStatus::Idle;
        }

        AppStatus::Working
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn detect_done_pattern() {
        let a = OpenCodeAdapter;
        let s = a.detect_status("Creating file User.ts\n✓ Task complete").await;
        assert!(matches!(s, AppStatus::Done));
    }

    #[tokio::test]
    async fn detect_error_pattern() {
        let a = OpenCodeAdapter;
        let s = a.detect_status("warming up\nError: cannot resolve module 'foo'").await;
        assert!(matches!(s, AppStatus::Error(_)));
    }

    #[tokio::test]
    async fn detect_idle_prompt() {
        let a = OpenCodeAdapter;
        let s = a.detect_status("hello world\n> ").await;
        assert!(matches!(s, AppStatus::Idle));
    }

    #[tokio::test]
    async fn working_when_streaming() {
        let a = OpenCodeAdapter;
        let s = a.detect_status("⠋ analyzing file structure ...").await;
        assert!(matches!(s, AppStatus::Working));
    }

    #[tokio::test]
    async fn ansi_stripped_before_match() {
        let a = OpenCodeAdapter;
        let s = a.detect_status("\u{1b}[32m> \u{1b}[0m").await;
        assert!(matches!(s, AppStatus::Idle));
    }
}
