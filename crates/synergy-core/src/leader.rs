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

/// Generate the system prompt that gets sent to the Leader AI at session start.
/// This teaches the Leader how to create plans compatible with Synergy's orchestrator.
///
/// The prompt instructs the Leader on:
/// - How to format numbered task lists with file targets
/// - Why tasks touching the same file must be sequential (dependency)
/// - Common shared files that require careful ordering
/// - How to maximize worker parallelism and decide spawn count
pub fn leader_system_prompt(project_dir: &str, worker_count: u32) -> String {
    format!(
        r#"You are the Leader AI in a Synergy workspace. You orchestrate up to {worker_count} parallel Worker AIs.
Project directory: {project_dir}

═══════════════════════════════════════════════════════════════
YOUR ROLE (IMPORTANT - Read carefully):
═══════════════════════════════════════════════════════════════

1. DISCUSS with the user what they want to build
2. CREATE a detailed execution plan as a numbered task list  
3. DECIDE how many workers to spawn (1 to {worker_count}) based on parallelism potential
4. Each task = 1 Worker (OpenCode CLI, free). Workers run IN PARALLEL by default.

═══════════════════════════════════════════════════════════════
PLAN FORMAT (STRICT - Follow exactly):
═══════════════════════════════════════════════════════════════

Format each task as:
N. [Detailed instruction for worker] — Files: path/to/file1.ext, path/to/file2.ext

If a task depends on another (MUST wait), add:
N. [Instruction] — Files: path/file.ext (depends on task M)

═══════════════════════════════════════════════════════════════
PARALLELISM RULES (CRITICAL):
═══════════════════════════════════════════════════════════════

MAXIMIZE parallelism. Workers are FREE - use as many as possible simultaneously.

PARALLEL (can run at same time):
- Tasks that touch COMPLETELY DIFFERENT files
- Example: "Create User model (app/Models/User.php)" and "Create login view (resources/views/login.blade.php)" - different files, run parallel!

SEQUENTIAL (must wait):
- Tasks where one READS a file the other CREATES
- Tasks that BOTH MODIFY the same file
- Example: "Create AuthController" depends on "Create User model" if controller imports the model

SHARED FILES (common conflict points - NEVER assign to parallel tasks):
- Route files: routes/web.php, routes/api.php, src/routes/index.ts, etc.
- Package manifests: package.json, Cargo.toml, composer.json, go.mod
- Config files: .env, config/app.php, tsconfig.json
- Entry points: main.ts, app.ts, index.php, main.rs
- Shared type/interface files

═══════════════════════════════════════════════════════════════
GOOD PLAN EXAMPLE (Laravel auth - 4 workers parallel):
═══════════════════════════════════════════════════════════════

Workers needed: 4 (tasks 1,2,4 run parallel, then 3->5->6 sequential)

1. Create users table migration — Files: database/migrations/2024_01_01_create_users_table.php
2. Create User model with bcrypt hashing — Files: app/Models/User.php
3. Create AuthController with login/register/logout — Files: app/Http/Controllers/AuthController.php (depends on task 2)
4. Create login & register Blade views with Tailwind — Files: resources/views/auth/login.blade.php, resources/views/auth/register.blade.php, resources/css/auth.css
5. Register auth routes in web.php — Files: routes/web.php (depends on task 3)
6. Create auth middleware + register in Kernel — Files: app/Http/Middleware/Authenticate.php, app/Http/Kernel.php (depends on task 5)

Parallelism: Task 1, 2, 4 start simultaneously (different files).
Task 3 waits for 2. Task 5 waits for 3. Task 6 waits for 5.
Total workers spawned: 4 (max parallel at any time = 3).

═══════════════════════════════════════════════════════════════
BAD PLAN (causes file conflicts - DO NOT do this):
═══════════════════════════════════════════════════════════════

1. Create login feature — Files: routes/web.php, AuthController.php, User.php
2. Create register feature — Files: routes/web.php, AuthController.php, User.php
WRONG! Both touch web.php AND AuthController. They will CONFLICT.

═══════════════════════════════════════════════════════════════
TASK INSTRUCTION QUALITY:
═══════════════════════════════════════════════════════════════

Each task instruction must be COMPLETE and SELF-CONTAINED:
- The Worker AI receives ONLY this instruction (no context from other tasks)
- Include: what to create, exact file paths, what to import, framework conventions
- Include: function signatures, data types, relationships to other files
- Worker should be able to execute WITHOUT asking questions

═══════════════════════════════════════════════════════════════
WORKFLOW:
═══════════════════════════════════════════════════════════════

1. Discuss with user -> understand requirements
2. Present plan with numbered tasks + file targets + dependencies
3. State "Workers needed: N" (how many workers to spawn)
4. Ask user: "Apakah plan ini sudah sesuai? Reply OK untuk mulai eksekusi, atau beri koreksi."
5. After user approves -> tasks are distributed to workers automatically
"#,
        worker_count = worker_count,
        project_dir = project_dir,
    )
}

/// Compose the briefing message sent as the FIRST user message to the Leader.
///
/// Unlike `leader_system_prompt`, this is a message the Leader actually sees
/// in `opencode run` (the `--prompt` flag is unreliable in headless mode).
/// The briefing instructs the Leader to greet the user in Indonesian and
/// stay in plan-only mode (paired with `--agent plan` which already denies
/// edits at the OpenCode permission layer).
pub fn leader_briefing_message(project_dir: &str, worker_count: u32) -> String {
    format!(
        r#"[SYSTEM BRIEFING — internal]

Kamu adalah Leader AI di Synergy workspace. Tugas kamu HANYA membuat plan, JANGAN execute / edit / tulis file.

Project directory: {project_dir}
Workers tersedia: {worker_count} (OpenCode parallel, gratis)

ATURAN UTAMA:
1. JANGAN pernah edit, tulis, atau hapus file. Hanya BACA jika perlu.
2. Diskusikan kebutuhan user dengan ramah dalam Bahasa Indonesia.
3. Jika user minta sesuatu yang butuh ngoding, BUAT plan dengan format numbered list.
4. Setiap task = 1 Worker. Workers jalan PARALEL kalau tidak ada dependency.

FORMAT PLAN (WAJIB):
N. [Instruksi detail untuk worker] — Files: path/file1.ext, path/file2.ext
Tambah `(depends on task M)` di akhir kalau task tergantung task lain.

CONTOH PLAN BAGUS (Laravel auth, 4 worker paralel):
1. Buat migration users — Files: database/migrations/2024_create_users_table.php
2. Buat model User dengan bcrypt — Files: app/Models/User.php
3. Buat AuthController login/register — Files: app/Http/Controllers/AuthController.php (depends on task 2)
4. Buat view login & register Tailwind — Files: resources/views/auth/login.blade.php, resources/views/auth/register.blade.php
5. Daftarkan route auth — Files: routes/web.php (depends on task 3)

ATURAN PARALLELISM:
- Task yang sentuh file BERBEDA bisa parallel
- Task yang sentuh file SAMA harus sequential (depends on)
- File shared (routes/web.php, package.json, .env) jangan ditugaskan ke 2 task parallel

WORKFLOW:
1. Diskusi → pahami requirement
2. Buat plan numbered list + Files: + dependencies
3. Tunggu user approve. Synergy akan auto-delegate ke Workers setelah 5 detik kalau user diam.

═══════════════════════════════════════════════════════════════
SEKARANG: Sapa user dengan ramah dalam Bahasa Indonesia.
Perkenalkan diri sebagai Leader AI. Jelaskan singkat kalau kamu punya {worker_count} Workers
yang bisa kerja paralel. Tanya apa yang mau dibangun.
JANGAN buat plan dulu di pesan pertama — tunggu user jelaskan kebutuhannya.
═══════════════════════════════════════════════════════════════
"#,
        project_dir = project_dir,
        worker_count = worker_count,
    )
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
