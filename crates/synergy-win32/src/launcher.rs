//! Process launching for GUI apps.

use anyhow::{Context, Result};
use std::process::Command;

/// Launch an application and return its PID.
///
/// Does not wait for the process to exit — the caller is expected to
/// subsequently call [`crate::wait_for_window`] to find the window once
/// the app has started.
pub fn launch_app(exe: &str, args: &[&str], cwd: Option<&str>) -> Result<u32> {
    let mut cmd = Command::new(exe);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("failed to launch '{}'", exe))?;
    Ok(child.id())
}
