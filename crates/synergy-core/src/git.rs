//! Git integration for Synergy.
//!
//! Provides:
//! - Auto-commit after each task completes
//! - Branch management (create feature branch per session)
//! - Conflict detection via `git diff`
//! - Status checking

use anyhow::{Context, Result};
use std::process::Command;

/// Git operations scoped to a project directory.
pub struct GitOps {
    cwd: String,
}

impl GitOps {
    /// Create a new GitOps instance for the given project directory.
    pub fn new(cwd: impl Into<String>) -> Self {
        Self { cwd: cwd.into() }
    }

    /// Check if the directory is a git repository.
    pub fn is_repo(&self) -> bool {
        self.run_git(&["rev-parse", "--is-inside-work-tree"])
            .map(|out| out.trim() == "true")
            .unwrap_or(false)
    }

    /// Initialize a git repo if one doesn't exist.
    pub fn init_if_needed(&self) -> Result<()> {
        if !self.is_repo() {
            self.run_git(&["init"]).context("git init")?;
        }
        Ok(())
    }

    /// Get current branch name.
    pub fn current_branch(&self) -> Result<String> {
        let out = self.run_git(&["branch", "--show-current"])?;
        Ok(out.trim().to_owned())
    }

    /// Create and checkout a new branch for this session.
    pub fn create_session_branch(&self, session_id: &str) -> Result<()> {
        let branch_name = format!("synergy/{}", session_id);
        self.run_git(&["checkout", "-b", &branch_name])
            .with_context(|| format!("create branch {}", branch_name))?;
        Ok(())
    }

    /// Stage all changes and commit with a message.
    pub fn commit_all(&self, message: &str) -> Result<String> {
        self.run_git(&["add", "-A"])?;

        // Check if there's anything to commit
        let status = self.run_git(&["status", "--porcelain"])?;
        if status.trim().is_empty() {
            return Ok("nothing to commit".to_owned());
        }

        let out = self.run_git(&["commit", "-m", message])?;
        Ok(out)
    }

    /// Auto-commit after a task completes.
    pub fn commit_task(&self, task_id: &str, task_title: &str, worker_id: u32) -> Result<String> {
        let msg = format!(
            "[synergy] Task {} completed by Worker #{}\n\n{}",
            task_id, worker_id, task_title
        );
        self.commit_all(&msg)
    }

    /// Check for uncommitted changes (dirty working tree).
    pub fn has_changes(&self) -> Result<bool> {
        let out = self.run_git(&["status", "--porcelain"])?;
        Ok(!out.trim().is_empty())
    }

    /// Get a short diff summary of current changes.
    pub fn diff_stat(&self) -> Result<String> {
        self.run_git(&["diff", "--stat"])
    }

    /// Detect merge conflicts in the working tree.
    pub fn has_conflicts(&self) -> Result<bool> {
        let out = self.run_git(&["diff", "--name-only", "--diff-filter=U"])?;
        Ok(!out.trim().is_empty())
    }

    /// Get list of conflicted files.
    pub fn conflicted_files(&self) -> Result<Vec<String>> {
        let out = self.run_git(&["diff", "--name-only", "--diff-filter=U"])?;
        Ok(out.lines().map(|l| l.trim().to_owned()).filter(|l| !l.is_empty()).collect())
    }

    /// Get the log of recent commits (last N).
    pub fn log_short(&self, count: u32) -> Result<String> {
        self.run_git(&[
            "log",
            "--oneline",
            &format!("-{}", count),
        ])
    }

    // ─── Internal ────────────────────────────────────────────────────────

    fn run_git(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.cwd)
            .output()
            .with_context(|| format!("running git {:?}", args))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git {:?} failed: {}", args, stderr.trim());
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn init_and_commit() {
        let dir = tempdir().unwrap();
        let git = GitOps::new(dir.path().to_str().unwrap());

        git.init_if_needed().unwrap();
        assert!(git.is_repo());

        // Create a file and commit
        std::fs::write(dir.path().join("hello.txt"), "world").unwrap();
        let result = git.commit_all("initial commit").unwrap();
        assert!(!result.contains("nothing to commit"));

        // No changes after commit
        assert!(!git.has_changes().unwrap());

        // Branch name
        let branch = git.current_branch().unwrap();
        assert!(!branch.is_empty());
    }

    #[test]
    fn commit_task_format() {
        let dir = tempdir().unwrap();
        let git = GitOps::new(dir.path().to_str().unwrap());
        git.init_if_needed().unwrap();

        std::fs::write(dir.path().join("auth.ts"), "export class Auth {}").unwrap();
        let result = git.commit_task("t_001", "Create auth module", 2).unwrap();
        assert!(!result.contains("nothing to commit"));

        let log = git.log_short(1).unwrap();
        assert!(log.contains("[synergy]"));
    }
}
