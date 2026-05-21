//! Leader-side helpers: parse a plan from free-form Leader output and
//! compose the report that gets piped back to the Leader after a batch of
//! tasks has finished.
//!
//! Kept dependency-free of the rest of [`crate`] so it stays unit-testable.

use chrono::Utc;
use once_cell::sync::Lazy;
use regex::Regex;
use synergy_proto::{Task, TaskStatus};

static NUMBERED_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^\s*(\d+)[\.\)]\s+(.+?)\s*$").expect("numbered re"));
static DASH_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^\s*[-*•]\s+(.+?)\s*$").expect("dash re"));
static APPROVAL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(ok|oke|approved|lgtm|looks\s+good|semua\s+(?:sudah\s+)?(?:benar|oke))\b|✓|✅|👍",
    )
    .expect("approval re")
});
static FILE_PATH_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:[A-Za-z]:[\\/]|\.{0,2}[\\/])?[\w\-./\\]+\.\w{1,8}")
        .expect("path re")
});

/// Parse a Leader plan into ordered task drafts.
///
/// We accept either numbered lists (`1. ...`, `2) ...`) or bullet lists
/// (`- ...`, `* ...`, `• ...`). Numbered lines win when both are present so
/// that multi-line bullet bodies inside a numbered item do not duplicate the
/// task.
pub fn parse_plan(text: &str) -> Vec<TaskDraft> {
    let numbered: Vec<_> = NUMBERED_RE
        .captures_iter(text)
        .map(|c| {
            let idx: u32 = c.get(1).unwrap().as_str().parse().unwrap_or(0);
            let body = c.get(2).unwrap().as_str().to_owned();
            (idx, body)
        })
        .collect();

    let drafts: Vec<TaskDraft> = if !numbered.is_empty() {
        numbered
            .into_iter()
            .map(|(idx, body)| TaskDraft {
                ordinal: idx,
                title: short_title(&body),
                instruction: body.trim().to_owned(),
                files_target: extract_files(&body),
            })
            .collect()
    } else {
        DASH_RE
            .captures_iter(text)
            .enumerate()
            .map(|(i, c)| {
                let body = c.get(1).unwrap().as_str().to_owned();
                TaskDraft {
                    ordinal: (i + 1) as u32,
                    title: short_title(&body),
                    instruction: body.trim().to_owned(),
                    files_target: extract_files(&body),
                }
            })
            .collect()
    };

    drafts
}

/// Detect dependencies between drafts.
///
/// Heuristic: if draft B mentions a file that draft A targets, B depends on
/// A. This handles the common "model first, controller next" case in the
/// spec example.
pub fn infer_dependencies(drafts: &[TaskDraft]) -> Vec<Vec<usize>> {
    let mut deps = vec![Vec::new(); drafts.len()];
    for (i, b) in drafts.iter().enumerate() {
        for (j, a) in drafts.iter().enumerate() {
            if i == j || a.files_target.is_empty() {
                continue;
            }
            let body = b.instruction.to_lowercase();
            let referenced = a
                .files_target
                .iter()
                .any(|f| body.contains(&f.to_lowercase()));
            if referenced {
                deps[i].push(j);
            }
        }
    }
    deps
}

/// Convert drafts plus inferred dependency indexes into [`Task`] records
/// ready to be persisted.
pub fn drafts_to_tasks(
    session_id: &str,
    drafts: &[TaskDraft],
    deps: &[Vec<usize>],
) -> Vec<Task> {
    let now = Utc::now();
    let ids: Vec<String> = drafts
        .iter()
        .map(|d| format!("t_{}_{}", session_id, d.ordinal))
        .collect();

    drafts
        .iter()
        .enumerate()
        .map(|(i, d)| Task {
            id: ids[i].clone(),
            session_id: session_id.to_owned(),
            title: d.title.clone(),
            instruction: d.instruction.clone(),
            status: TaskStatus::Pending,
            worker_id: None,
            depends_on: deps[i].iter().map(|&j| ids[j].clone()).collect(),
            files_target: d.files_target.clone(),
            attempt: 0,
            created_at: now,
            started_at: None,
            ended_at: None,
        })
        .collect()
}

/// Returns true if the Leader's reply signals approval of the report.
pub fn is_approval(text: &str) -> bool {
    APPROVAL_RE.is_match(text)
}

/// Compose the human-readable batch report that gets written to the
/// Leader's stdin once all running tasks have finished.
pub fn compose_batch_report(completed: &[Task]) -> String {
    let mut report = String::new();
    report.push_str("\n══════════════════════════════════════\n");
    report.push_str("  WORKER BATCH REPORT\n");
    report.push_str("══════════════════════════════════════\n\n");

    if completed.is_empty() {
        report.push_str("(no tasks completed in this batch)\n");
    } else {
        for t in completed {
            let status = match t.status {
                TaskStatus::Done => "✓ DONE",
                TaskStatus::Failed => "✗ FAILED",
                TaskStatus::Escalated => "⚠ ESCALATED",
                _ => "(in progress)",
            };
            let worker = t
                .worker_id
                .map(|id| format!("Worker {id}"))
                .unwrap_or_else(|| "—".to_owned());
            report.push_str(&format!(
                "[{worker}] Task {}: {}\n",
                t.id, t.title,
            ));
            report.push_str(&format!("  Status : {status}\n"));
            if let (Some(start), Some(end)) = (t.started_at, t.ended_at) {
                let secs = (end - start).num_seconds().max(0);
                report.push_str(&format!("  Time   : {secs}s\n"));
            }
            if !t.files_target.is_empty() {
                report.push_str(&format!(
                    "  Files  : {}\n",
                    t.files_target.join(", ")
                ));
            }
            report.push('\n');
        }
    }

    report.push_str("══════════════════════════════════════\n");
    report.push_str("Reply 'OK' to approve, or send corrections.\n");
    report.push_str("══════════════════════════════════════\n");
    report
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDraft {
    pub ordinal: u32,
    pub title: String,
    pub instruction: String,
    pub files_target: Vec<String>,
}

fn short_title(body: &str) -> String {
    let cut: String = body.chars().take(60).collect();
    let cut = cut.trim().to_owned();
    if body.chars().count() > 60 {
        format!("{cut}…")
    } else {
        cut
    }
}

fn extract_files(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for m in FILE_PATH_RE.find_iter(body) {
        let s = m.as_str();
        // Filter out things that look like sentence endings ("login.").
        if s.ends_with('.') {
            continue;
        }
        // Require a real extension (>=2 chars after the dot).
        if let Some(ext_idx) = s.rfind('.') {
            let ext = &s[ext_idx + 1..];
            if ext.len() >= 2 && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
                out.push(s.to_owned());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numbered_plan() {
        let text = "Baik, saya pecah jadi 3 task:\n\
                    1. Buat file src/models/User.ts dengan bcrypt.\n\
                    2. Buat src/controllers/AuthController.ts.\n\
                    3. Buat halaman login (EJS template).";
        let drafts = parse_plan(text);
        assert_eq!(drafts.len(), 3);
        assert!(drafts[0].instruction.contains("User.ts"));
        assert!(drafts[0].files_target.iter().any(|f| f.contains("User.ts")));
    }

    #[test]
    fn parses_dash_list_when_no_numbers() {
        let text = "- Add migration\n- Add controller";
        let drafts = parse_plan(text);
        assert_eq!(drafts.len(), 2);
    }

    #[test]
    fn infers_file_dependency() {
        let drafts = vec![
            TaskDraft {
                ordinal: 1,
                title: "model".into(),
                instruction: "Buat src/models/User.ts".into(),
                files_target: vec!["src/models/User.ts".into()],
            },
            TaskDraft {
                ordinal: 2,
                title: "controller".into(),
                instruction: "Import dari src/models/User.ts ke controller".into(),
                files_target: vec!["src/controllers/Auth.ts".into()],
            },
        ];
        let deps = infer_dependencies(&drafts);
        assert_eq!(deps[0], Vec::<usize>::new());
        assert_eq!(deps[1], vec![0]);
    }

    #[test]
    fn approval_detection() {
        assert!(is_approval("Oke, semua sudah benar"));
        assert!(is_approval("LGTM"));
        assert!(is_approval("approved ✅"));
        assert!(!is_approval("ada masalah di task 3"));
    }

    #[test]
    fn batch_report_contains_all_tasks() {
        let now = Utc::now();
        let t = Task {
            id: "t_1".into(),
            session_id: "s".into(),
            title: "demo".into(),
            instruction: "do stuff".into(),
            status: TaskStatus::Done,
            worker_id: Some(0),
            depends_on: vec![],
            files_target: vec!["src/x.rs".into()],
            attempt: 0,
            created_at: now,
            started_at: Some(now),
            ended_at: Some(now),
        };
        let r = compose_batch_report(&[t]);
        assert!(r.contains("Task t_1"));
        assert!(r.contains("Worker 0"));
        assert!(r.contains("DONE"));
    }
}
