//! End-to-end orchestrator smoke test.
//!
//! Spawns one real worker process (`cmd.exe` on Windows, `sh` elsewhere)
//! through the [`Orchestrator`], schedules a pending task, drives the
//! tick loop, and asserts the task reaches [`TaskStatus::Done`].
//!
//! This exercises the full Phase-1 pipeline: PTY → adapter → status
//! detection → DB transition → file lock release.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use synergy_adapter::GenericCliAdapter;
use synergy_core::Orchestrator;
use synergy_db::Database;
use synergy_proto::{Task, TaskStatus};
use synergy_proxy::ProxyManager;
use tokio::sync::Mutex;

#[cfg(windows)]
const SHELL_BIN: &str = "cmd.exe";

#[cfg(not(windows))]
const SHELL_BIN: &str = "sh";

fn make_task(id: &str, session_id: &str, instruction: &str) -> Task {
    Task {
        id: id.to_owned(),
        session_id: session_id.to_owned(),
        title: id.to_owned(),
        instruction: instruction.to_owned(),
        status: TaskStatus::Pending,
        worker_id: None,
        depends_on: Vec::new(),
        files_target: Vec::new(),
        attempt: 0,
        created_at: Utc::now(),
        started_at: None,
        ended_at: None,
    }
}

async fn run_until<F>(orch: &Orchestrator, deadline: Duration, mut predicate: F) -> bool
where
    F: FnMut(&[Task]) -> bool,
{
    let started = Instant::now();
    while started.elapsed() < deadline {
        if let Err(err) = orch.tick().await {
            eprintln!("tick error: {err:#}");
        }
        let db = orch.db.lock().await;
        let tasks = db
            .get_all_tasks(&orch.session_id)
            .expect("get tasks");
        drop(db);
        if predicate(&tasks) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    false
}

#[tokio::test]
async fn worker_completes_simple_task() {
    let db = Database::open_in_memory().expect("open db");
    db.insert_project("p1", "Test Project", ".").expect("project");
    let session_id = "s_test_e2e".to_owned();
    db.insert_session(&session_id, "p1", "cli-generic", 1)
        .expect("session");

    let shared_db = Arc::new(Mutex::new(db));
    let pm = ProxyManager::new(Vec::new());
    let orch = Orchestrator::with_shared_db(shared_db.clone(), pm, session_id.clone())
        .with_adapter(Arc::new(GenericCliAdapter::default()));

    orch.spawn_workers(1, SHELL_BIN, None)
        .await
        .expect("spawn worker");

    // Allow the shell to fully boot and print its first prompt before we
    // check status, otherwise the very first tick may interpret the empty
    // buffer as Idle and race the scheduler.
    tokio::time::sleep(Duration::from_millis(800)).await;

    let task = make_task("t_echo", &session_id, "echo synergy_works");
    {
        let db = shared_db.lock().await;
        db.insert_task(&task).expect("insert task");
    }

    let became_running = run_until(&orch, Duration::from_secs(8), |tasks| {
        tasks
            .iter()
            .any(|t| t.id == "t_echo" && matches!(t.status, TaskStatus::Running | TaskStatus::Done))
    })
    .await;
    assert!(became_running, "task never reached Running state");

    let became_done = run_until(&orch, Duration::from_secs(15), |tasks| {
        tasks
            .iter()
            .any(|t| t.id == "t_echo" && t.status == TaskStatus::Done)
    })
    .await;
    assert!(became_done, "task never reached Done state");

    // Sanity: an event log entry was written for the assignment.
    let db = shared_db.lock().await;
    let logs = db.get_event_logs(&session_id).expect("logs");
    let assigned = logs.iter().any(|l| l.event_type == "TaskAssigned");
    assert!(assigned, "expected TaskAssigned event in log, got: {logs:#?}");
}
