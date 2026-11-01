use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// `std::env::set_var` is a process-wide, non-atomic mutation, so tests setting `ZELLOOM_ATTACH_TIMEOUT_MS` must hold this lock while it's set.
static ENV_LOCK: Mutex<()> = Mutex::new(());

use zelloom::cli::{ensure_core, stop_core};
use zelloom::client::{Client, ClientError, call};
use zelloom::core::{CoreOptions, NoopLauncher, TabLauncher, run as core_run};
use zelloom::protocol::{
    AttentionKind, InputOutcome, Outcome, Request, ResolvedAgent, Response, RunnerEvent,
    ServerMessage, Source, Task, TaskStatus, TurnState, WatchEvent,
};

fn write_config(path: &Path) {
    std::fs::write(
        path,
        r#"
default_agent = "fake"

[scheduler]
max_parallel = 4

[agents.fake]
command = ["fake-agent"]

[workspaces.a]
path = "/tmp/zelloom-test-workspace-a"
agent = "fake"

[workspaces.b]
path = "/tmp/zelloom-test-workspace-b"
agent = "fake"

[sources.inbox]
auto_queue = false
"#,
    )
    .unwrap();
}

struct RunningCore {
    socket_path: PathBuf,
    config_path: PathBuf,
    db_path: PathBuf,
    handle: std::thread::JoinHandle<()>,
}

fn start_core(dir: &Path) -> RunningCore {
    start_core_with(dir, Arc::new(NoopLauncher))
}

fn start_core_with(dir: &Path, launcher: Arc<dyn TabLauncher>) -> RunningCore {
    let config_path = dir.join("config.toml");
    write_config(&config_path);
    let db_path = dir.join("state.db");
    let socket_path = dir.join("default.sock");

    let options = CoreOptions {
        socket_path: socket_path.clone(),
        config_path: config_path.clone(),
        db_path: db_path.clone(),
        launcher,
    };

    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            if let Err(e) = core_run(options).await {
                panic!("core exited with an error: {e}");
            }
        });
    });

    wait_for_socket(&socket_path);

    RunningCore {
        socket_path,
        config_path,
        db_path,
        handle,
    }
}

fn restart_core(previous: RunningCore) -> RunningCore {
    previous.handle.join().unwrap();
    relaunch_core(previous.socket_path, previous.config_path, previous.db_path)
}

fn relaunch_core(socket_path: PathBuf, config_path: PathBuf, db_path: PathBuf) -> RunningCore {
    let options = CoreOptions {
        socket_path: socket_path.clone(),
        config_path: config_path.clone(),
        db_path: db_path.clone(),
        launcher: Arc::new(NoopLauncher),
    };

    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            if let Err(e) = core_run(options).await {
                panic!("core exited with an error: {e}");
            }
        });
    });

    wait_for_socket(&socket_path);

    RunningCore {
        socket_path,
        config_path,
        db_path,
        handle,
    }
}

fn wait_for_socket(path: &Path) {
    for _ in 0..400 {
        if path.exists() && std::os::unix::net::UnixStream::connect(path).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("core did not start listening on {}", path.display());
}

fn attach_runner(socket_path: &Path, workspace: &str) -> Client {
    let mut client = Client::connect(socket_path).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    client
        .write_request(&Request::RunnerAttach {
            workspace: workspace.to_string(),
        })
        .unwrap();
    let response = client.read_response().unwrap();
    assert!(response.ok, "runner_attach failed: {:?}", response.error);
    client
}

fn expect_start(client: &mut Client) -> (Task, ResolvedAgent) {
    match client.read_message().unwrap() {
        ServerMessage::Event(RunnerEvent::Start { task, agent }) => (task, agent),
        other => panic!("expected a start event, got {other:?}"),
    }
}

fn expect_stop(client: &mut Client) -> String {
    match client.read_message().unwrap() {
        ServerMessage::Event(RunnerEvent::Stop { task_id }) => task_id,
        other => panic!("expected a stop event, got {other:?}"),
    }
}

fn enqueue(socket_path: &Path, workspace: &str, text: &str) -> Task {
    enqueue_from(socket_path, workspace, text, "cli")
}

fn enqueue_from(socket_path: &Path, workspace: &str, text: &str, source: &str) -> Task {
    let response = request(
        socket_path,
        &Request::Enqueue {
            text: text.to_string(),
            workspace: workspace.to_string(),
            agent: None,
            source: Source {
                kind: source.to_string(),
                id: None,
                sender: None,
            },
            reply_to: None,
            metadata: serde_json::json!({}),
        },
    );
    serde_json::from_value(response.into_result().unwrap().unwrap()).unwrap()
}

fn get_task(socket_path: &Path, task_id: &str) -> Task {
    let data = call(socket_path, &Request::List).unwrap();
    let tasks: Vec<Task> = serde_json::from_value(data).unwrap();
    tasks
        .into_iter()
        .find(|t| t.id == task_id)
        .expect("task not found in list")
}

fn wait_for_status(socket_path: &Path, task_id: &str, status: TaskStatus) -> Task {
    for _ in 0..200 {
        let task = get_task(socket_path, task_id);
        if task.status == status {
            return task;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("task {task_id} did not reach status {status:?} in time");
}

#[test]
fn two_workspaces_run_concurrently_and_survive_restart() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());

    let mut runner_a = attach_runner(&core.socket_path, "a");
    let mut runner_b = attach_runner(&core.socket_path, "b");

    let task_a1 = enqueue(&core.socket_path, "a", "a1: first task in workspace a");
    let task_b1 = enqueue(&core.socket_path, "b", "b1: task in workspace b");
    let task_a2 = enqueue(&core.socket_path, "a", "a2: second task in workspace a");

    let (started_a1, agent_a) = expect_start(&mut runner_a);
    assert_eq!(started_a1.id, task_a1.id);
    assert_eq!(agent_a.cwd, "/tmp/zelloom-test-workspace-a");
    assert_eq!(agent_a.env.get("ZELLOOM_TASK_ID"), Some(&task_a1.id));
    assert_eq!(agent_a.env.get("ZELLOOM_WORKSPACE"), Some(&"a".to_string()));
    assert!(agent_a.argv.iter().any(|s| s.contains("a1: first task")));

    let (started_b1, _agent_b) = expect_start(&mut runner_b);
    assert_eq!(started_b1.id, task_b1.id);

    let still_queued = get_task(&core.socket_path, &task_a2.id);
    assert_eq!(
        still_queued.status,
        TaskStatus::Queued,
        "a2 must wait for a1 to finish"
    );

    let response = call(
        &core.socket_path,
        &Request::Complete {
            task_id: task_a1.id.clone(),
        },
    )
    .unwrap();
    let completed_a1: Task = serde_json::from_value(response).unwrap();
    assert_eq!(completed_a1.status, TaskStatus::Done);

    let stopped_id = expect_stop(&mut runner_a);
    assert_eq!(stopped_id, task_a1.id);

    let (started_a2, _) = expect_start(&mut runner_a);
    assert_eq!(started_a2.id, task_a2.id);
    assert_eq!(started_a2.status, TaskStatus::Running);

    drop(runner_b);
    let interrupted_b1 = wait_for_status(&core.socket_path, &task_b1.id, TaskStatus::Interrupted);
    assert_eq!(interrupted_b1.status, TaskStatus::Interrupted);

    call(&core.socket_path, &Request::Shutdown { force: true }).unwrap();
    let core = restart_core(core);

    let recovered_a2 = wait_for_status(&core.socket_path, &task_a2.id, TaskStatus::Interrupted);
    assert_eq!(recovered_a2.status, TaskStatus::Interrupted);

    let still_interrupted_b1 = get_task(&core.socket_path, &task_b1.id);
    assert_eq!(still_interrupted_b1.status, TaskStatus::Interrupted);

    call(&core.socket_path, &Request::Shutdown { force: true }).unwrap();
    core.handle.join().unwrap();
}

#[test]
fn complete_is_rejected_on_a_queued_task() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());

    let mut runner_a = attach_runner(&core.socket_path, "a");
    let task_a1 = enqueue(&core.socket_path, "a", "occupies workspace a");
    let task_a2 = enqueue(&core.socket_path, "a", "waits behind a1");
    let (started_a1, _) = expect_start(&mut runner_a);
    assert_eq!(started_a1.id, task_a1.id);

    let still_queued = get_task(&core.socket_path, &task_a2.id);
    assert_eq!(still_queued.status, TaskStatus::Queued);

    let err = call(
        &core.socket_path,
        &Request::Complete {
            task_id: task_a2.id.clone(),
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("queued"), "error was: {err}");

    let unchanged = get_task(&core.socket_path, &task_a2.id);
    assert_eq!(unchanged.status, TaskStatus::Queued);

    call(&core.socket_path, &Request::Shutdown { force: true }).unwrap();
    core.handle.join().unwrap();
}

#[test]
fn completing_a_task_before_its_attach_timeout_elapses_keeps_it_done() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        std::env::set_var("ZELLOOM_ATTACH_TIMEOUT_MS", "150");
    }

    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());

    let task = enqueue(&core.socket_path, "a", "a task that finishes fast");

    let mut runner_a = attach_runner(&core.socket_path, "a");
    let (started, _agent) = expect_start(&mut runner_a);
    assert_eq!(started.id, task.id);

    let response = call(
        &core.socket_path,
        &Request::Complete {
            task_id: task.id.clone(),
        },
    )
    .unwrap();
    let completed: Task = serde_json::from_value(response).unwrap();
    assert_eq!(completed.status, TaskStatus::Done);
    expect_stop(&mut runner_a);

    std::thread::sleep(Duration::from_millis(350));
    let after_timeout = get_task(&core.socket_path, &task.id);
    assert_eq!(after_timeout.status, TaskStatus::Done);

    call(&core.socket_path, &Request::Shutdown { force: true }).unwrap();
    core.handle.join().unwrap();
    unsafe {
        std::env::remove_var("ZELLOOM_ATTACH_TIMEOUT_MS");
    }
}

#[test]
fn stale_attach_timeout_from_a_superseded_launch_does_not_interrupt_the_retry() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        std::env::set_var("ZELLOOM_ATTACH_TIMEOUT_MS", "300");
    }

    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());

    let task = enqueue(&core.socket_path, "a", "slow to attach");

    std::thread::sleep(Duration::from_millis(200));
    let cancelled = call(
        &core.socket_path,
        &Request::Cancel {
            task_id: task.id.clone(),
        },
    )
    .unwrap();
    let cancelled: Task = serde_json::from_value(cancelled).unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);

    let retried = call(
        &core.socket_path,
        &Request::Retry {
            task_id: task.id.clone(),
        },
    )
    .unwrap();
    let retried: Task = serde_json::from_value(retried).unwrap();
    assert_eq!(retried.status, TaskStatus::Queued);

    let running_again = wait_for_status(&core.socket_path, &task.id, TaskStatus::Running);
    assert_eq!(running_again.id, task.id);

    std::thread::sleep(Duration::from_millis(200));
    let still_running = get_task(&core.socket_path, &task.id);
    assert_eq!(
        still_running.status,
        TaskStatus::Running,
        "a stale timeout from the superseded first launch must not interrupt the retry"
    );

    let interrupted = wait_for_status(&core.socket_path, &task.id, TaskStatus::Interrupted);
    assert_eq!(interrupted.status, TaskStatus::Interrupted);

    call(&core.socket_path, &Request::Shutdown { force: true }).unwrap();
    core.handle.join().unwrap();
    unsafe {
        std::env::remove_var("ZELLOOM_ATTACH_TIMEOUT_MS");
    }
}

struct FailingLauncher;

impl TabLauncher for FailingLauncher {
    fn launch(&self, _workspace_id: &str, _workspace_path: &Path) -> anyhow::Result<()> {
        anyhow::bail!("launch always fails in this test")
    }
}

fn expect_response(client: &mut Client) -> Response {
    match client.read_message().unwrap() {
        ServerMessage::Response(response) => response,
        other => panic!("expected a response, got {other:?}"),
    }
}

fn send_agent_exited(client: &mut Client, task_id: &str, outcome: Outcome) -> Response {
    client
        .write_request(&Request::AgentExited {
            task_id: task_id.to_string(),
            outcome,
        })
        .unwrap();
    expect_response(client)
}

#[test]
fn failed_tab_launch_interrupts_the_task_and_core_keeps_responding() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let core = start_core_with(dir.path(), Arc::new(FailingLauncher));

    let first = enqueue(&core.socket_path, "a", "no runner for a");
    let second = enqueue(&core.socket_path, "a", "still no runner for a");

    let response = request(&core.socket_path, &Request::List);
    let tasks: Vec<Task> =
        serde_json::from_value(response.into_result().unwrap().unwrap()).unwrap();
    for id in [&first.id, &second.id] {
        let task = tasks.iter().find(|t| &t.id == id).unwrap();
        assert_eq!(task.status, TaskStatus::Interrupted);
    }

    let mut runner_b = attach_runner(&core.socket_path, "b");
    let task_b = enqueue(&core.socket_path, "b", "b has a runner");
    let (started, _) = expect_start(&mut runner_b);
    assert_eq!(started.id, task_b.id);

    request(&core.socket_path, &Request::Shutdown { force: true });
    core.handle.join().unwrap();
}

#[test]
fn second_runner_for_the_same_workspace_is_rejected() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());

    let mut first = attach_runner(&core.socket_path, "a");
    let task = enqueue(&core.socket_path, "a", "owned by the first runner");
    let (started, _) = expect_start(&mut first);
    assert_eq!(started.id, task.id);

    let mut second = Client::connect(&core.socket_path).unwrap();
    second
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    second
        .write_request(&Request::RunnerAttach {
            workspace: "a".to_string(),
        })
        .unwrap();
    let response = second.read_response().unwrap();
    assert!(!response.ok);
    assert!(
        response
            .error
            .as_deref()
            .unwrap()
            .contains("already attached"),
        "error was: {:?}",
        response.error
    );
    assert!(matches!(
        second.read_message(),
        Err(zelloom::client::ClientError::ConnectionClosed)
    ));
    drop(second);

    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        get_task(&core.socket_path, &task.id).status,
        TaskStatus::Running
    );

    let done = send_agent_exited(&mut first, &task.id, Outcome::Done);
    assert!(done.ok, "error was: {:?}", done.error);
    assert_eq!(
        get_task(&core.socket_path, &task.id).status,
        TaskStatus::Done
    );

    drop(first);
    std::thread::sleep(Duration::from_millis(100));
    let mut replacement = attach_runner(&core.socket_path, "a");
    let next = enqueue(&core.socket_path, "a", "after the first runner left");
    let (started, _) = expect_start(&mut replacement);
    assert_eq!(started.id, next.id);

    call(&core.socket_path, &Request::Shutdown { force: true }).unwrap();
    core.handle.join().unwrap();
}

#[test]
fn agent_exited_only_applies_to_the_runners_current_running_task() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());

    let mut runner_a = attach_runner(&core.socket_path, "a");
    let mut runner_b = attach_runner(&core.socket_path, "b");
    let task_a = enqueue(&core.socket_path, "a", "task a");
    let task_b = enqueue(&core.socket_path, "b", "task b");
    expect_start(&mut runner_a);
    expect_start(&mut runner_b);

    let err = call(
        &core.socket_path,
        &Request::AgentExited {
            task_id: task_a.id.clone(),
            outcome: Outcome::Done,
        },
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("runner connection"),
        "error was: {err}"
    );

    let foreign = send_agent_exited(&mut runner_a, &task_b.id, Outcome::Failed);
    assert!(!foreign.ok);
    assert_eq!(
        get_task(&core.socket_path, &task_b.id).status,
        TaskStatus::Running
    );

    call(
        &core.socket_path,
        &Request::Complete {
            task_id: task_a.id.clone(),
        },
    )
    .unwrap();
    assert_eq!(expect_stop(&mut runner_a), task_a.id);

    for outcome in [Outcome::Failed, Outcome::Restart] {
        let late = send_agent_exited(&mut runner_a, &task_a.id, outcome);
        assert!(
            !late.ok,
            "{outcome:?} must be rejected after the task finished"
        );
        assert_eq!(
            get_task(&core.socket_path, &task_a.id).status,
            TaskStatus::Done
        );
    }
    runner_a
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    assert!(
        runner_a.read_message().is_err(),
        "a rejected restart must not send another start event"
    );

    runner_b
        .write_request(&Request::AgentExited {
            task_id: task_b.id.clone(),
            outcome: Outcome::Restart,
        })
        .unwrap();
    let (restarted, _) = expect_start(&mut runner_b);
    assert_eq!(restarted.id, task_b.id);
    let restart = expect_response(&mut runner_b);
    assert!(restart.ok, "error was: {:?}", restart.error);
    let failed = send_agent_exited(&mut runner_b, &task_b.id, Outcome::Failed);
    assert!(failed.ok, "error was: {:?}", failed.error);
    assert_eq!(
        get_task(&core.socket_path, &task_b.id).status,
        TaskStatus::Failed
    );

    call(&core.socket_path, &Request::Shutdown { force: true }).unwrap();
    core.handle.join().unwrap();
}

#[test]
fn queued_and_received_tasks_can_be_cancelled() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());

    let mut runner_a = attach_runner(&core.socket_path, "a");
    let running = enqueue(&core.socket_path, "a", "occupies workspace a");
    expect_start(&mut runner_a);
    let queued = enqueue(&core.socket_path, "a", "waits behind the running task");
    let received: Task = serde_json::from_value(
        call(
            &core.socket_path,
            &Request::Enqueue {
                text: "from an inbox source".to_string(),
                workspace: "a".to_string(),
                agent: None,
                source: Source {
                    kind: "inbox".to_string(),
                    id: None,
                    sender: None,
                },
                reply_to: None,
                metadata: serde_json::json!({}),
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(received.status, TaskStatus::Received);

    for task in [&queued, &received] {
        let cancelled: Task = serde_json::from_value(
            call(
                &core.socket_path,
                &Request::Cancel {
                    task_id: task.id.clone(),
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(cancelled.status, TaskStatus::Cancelled);
    }

    call(
        &core.socket_path,
        &Request::Complete {
            task_id: running.id.clone(),
        },
    )
    .unwrap();
    assert_eq!(expect_stop(&mut runner_a), running.id);
    runner_a
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    assert!(
        runner_a.read_message().is_err(),
        "a cancelled queued task must not be started"
    );

    call(&core.socket_path, &Request::Shutdown { force: true }).unwrap();
    core.handle.join().unwrap();
}

fn expect_connection_closed(client: &mut Client) {
    match client.read_message() {
        Err(ClientError::ConnectionClosed) => {}
        other => panic!("expected the core to close the runner connection, got {other:?}"),
    }
}

#[test]
fn stop_is_refused_while_a_task_is_running_unless_forced() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());

    let mut runner_a = attach_runner(&core.socket_path, "a");
    let mut runner_b = attach_runner(&core.socket_path, "b");
    let task = enqueue(&core.socket_path, "a", "keeps running across stop");
    expect_start(&mut runner_a);

    let refused = stop_core(&core.socket_path, false, Duration::from_secs(5))
        .expect_err("stop without --force must be refused while a task is running")
        .to_string();
    assert!(refused.contains(&task.id), "{refused}");
    assert!(refused.contains("keeps running across stop"), "{refused}");
    assert!(refused.contains("--force"), "{refused}");
    assert_eq!(
        get_task(&core.socket_path, &task.id).status,
        TaskStatus::Running
    );

    let stopped = stop_core(&core.socket_path, true, Duration::from_secs(5)).unwrap();
    assert_eq!(stopped, "core stopped");
    expect_connection_closed(&mut runner_a);
    expect_connection_closed(&mut runner_b);

    let RunningCore {
        socket_path,
        config_path,
        db_path,
        handle,
    } = core;
    handle.join().unwrap();
    assert!(!socket_path.exists(), "core must remove its socket file");

    let core = relaunch_core(socket_path, config_path, db_path);
    assert_eq!(
        get_task(&core.socket_path, &task.id).status,
        TaskStatus::Interrupted
    );

    let stopped = stop_core(&core.socket_path, false, Duration::from_secs(5)).unwrap();
    assert_eq!(stopped, "core stopped");
    core.handle.join().unwrap();
}

#[test]
fn stop_without_a_core_reports_not_running() {
    let dir = tempfile::tempdir().unwrap();
    let message = stop_core(
        &dir.path().join("missing.sock"),
        false,
        Duration::from_secs(1),
    )
    .unwrap();
    assert_eq!(message, "core is not running");
}

#[test]
fn ensure_core_with_running_core_needs_no_zellij() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        std::env::remove_var("ZELLIJ_SESSION_NAME");
    }
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());

    ensure_core(&core.socket_path).unwrap();

    stop_core(&core.socket_path, false, Duration::from_secs(5)).unwrap();
    core.handle.join().unwrap();
}

#[test]
fn ensure_core_without_core_outside_zellij_fails() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        std::env::remove_var("ZELLIJ_SESSION_NAME");
    }
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("missing.sock");

    let err = ensure_core(&socket_path).unwrap_err().to_string();
    assert!(err.contains("Zellij session"), "{err}");
    assert!(!socket_path.exists());
}

#[test]
fn restart_that_cannot_resolve_the_agent_fails_the_task_and_frees_the_workspace() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());

    let mut runner_a = attach_runner(&core.socket_path, "a");
    let task = enqueue(&core.socket_path, "a", "restart after workspace removal");
    expect_start(&mut runner_a);

    let original = std::fs::read_to_string(&core.config_path).unwrap();
    let without_a = original.replace(
        "[workspaces.a]\npath = \"/tmp/zelloom-test-workspace-a\"\nagent = \"fake\"\n",
        "",
    );
    assert_ne!(original, without_a);
    std::fs::write(&core.config_path, &without_a).unwrap();

    let restart = send_agent_exited(&mut runner_a, &task.id, Outcome::Restart);
    assert!(!restart.ok);
    let error = restart.error.unwrap();
    assert!(error.contains("workspace 'a' is not registered"), "{error}");
    assert!(error.contains("marked failed"), "{error}");
    assert_eq!(
        get_task(&core.socket_path, &task.id).status,
        TaskStatus::Failed
    );

    let status = call(&core.socket_path, &Request::Status).unwrap();
    let runner = status["runners"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["workspace"] == "a")
        .unwrap()
        .clone();
    assert!(runner["current_task"].is_null(), "{runner}");

    std::fs::write(&core.config_path, &original).unwrap();
    let next = enqueue(&core.socket_path, "a", "workspace a is free again");
    let (started, _) = expect_start(&mut runner_a);
    assert_eq!(started.id, next.id);

    call(&core.socket_path, &Request::Shutdown { force: true }).unwrap();
    core.handle.join().unwrap();
}

#[test]
fn enqueue_rejects_unknown_agent_and_unknown_workspace() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());

    let request = |workspace: &str, agent: Option<&str>| Request::Enqueue {
        text: "validation".to_string(),
        workspace: workspace.to_string(),
        agent: agent.map(str::to_string),
        source: Source::cli(),
        reply_to: None,
        metadata: serde_json::json!({}),
    };

    let err = call(&core.socket_path, &request("a", Some("nope")))
        .unwrap_err()
        .to_string();
    assert!(err.contains("agent 'nope' is not defined"), "{err}");

    let err = call(&core.socket_path, &request("zzz", None))
        .unwrap_err()
        .to_string();
    assert!(err.contains("workspace 'zzz' is not registered"), "{err}");

    let tasks: Vec<Task> =
        serde_json::from_value(call(&core.socket_path, &Request::List).unwrap()).unwrap();
    assert!(tasks.is_empty());

    let ok: Task =
        serde_json::from_value(call(&core.socket_path, &request("a", Some("fake"))).unwrap())
            .unwrap();
    assert_eq!(ok.agent.as_deref(), Some("fake"));

    call(&core.socket_path, &Request::Shutdown { force: true }).unwrap();
    core.handle.join().unwrap();
}

#[test]
fn runner_attach_is_only_accepted_as_the_first_message() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());

    let mut late = Client::connect(&core.socket_path).unwrap();
    late.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    assert!(late.call(&Request::List).unwrap().ok);
    let refused = late
        .call(&Request::RunnerAttach {
            workspace: "a".to_string(),
        })
        .unwrap();
    assert!(!refused.ok);
    assert_eq!(
        refused.error.as_deref(),
        Some("runner_attach must be the first message on a connection")
    );
    assert!(late.call(&Request::List).unwrap().ok);
    let status = late.call(&Request::Status).unwrap();
    assert_eq!(status.data.unwrap()["runners"], serde_json::json!([]));
    let exited = late
        .call(&Request::AgentExited {
            task_id: "x".to_string(),
            outcome: Outcome::Done,
        })
        .unwrap();
    assert!(
        exited
            .error
            .as_deref()
            .unwrap()
            .contains("runner connection")
    );

    let mut runner_a = attach_runner(&core.socket_path, "a");
    runner_a
        .write_request(&Request::RunnerAttach {
            workspace: "b".to_string(),
        })
        .unwrap();
    let again = expect_response(&mut runner_a);
    assert!(!again.ok);
    assert_eq!(
        again.error.as_deref(),
        Some("connection is already attached as runner for a")
    );

    let task = enqueue(&core.socket_path, "a", "runner a is still attached");
    let (started, _) = expect_start(&mut runner_a);
    assert_eq!(started.id, task.id);
    let mut runner_b = attach_runner(&core.socket_path, "b");
    let task_b = enqueue(&core.socket_path, "b", "b attached separately");
    let (started_b, _) = expect_start(&mut runner_b);
    assert_eq!(started_b.id, task_b.id);

    call(&core.socket_path, &Request::Shutdown { force: true }).unwrap();
    core.handle.join().unwrap();
}

fn request(socket_path: &Path, request: &Request) -> Response {
    let mut client = Client::connect(socket_path).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    client
        .call(request)
        .expect("core did not respond in time (deadlock?)")
}

fn report(socket_path: &Path, task_id: &str, kind: AttentionKind, message: &str) -> Response {
    request(
        socket_path,
        &Request::Report {
            task_id: task_id.to_string(),
            kind,
            message: message.to_string(),
        },
    )
}

fn send_reply(socket_path: &Path, task_id: &str, expect: u64, text: &str) -> Response {
    request(
        socket_path,
        &Request::Send {
            task_id: task_id.to_string(),
            expect,
            text: text.to_string(),
        },
    )
}

fn send_in_background(
    socket_path: &Path,
    task_id: &str,
    expect: u64,
    text: &str,
) -> std::thread::JoinHandle<Response> {
    let socket_path = socket_path.to_path_buf();
    let task_id = task_id.to_string();
    let text = text.to_string();
    std::thread::spawn(move || send_reply(&socket_path, &task_id, expect, &text))
}

fn set_turn(socket_path: &Path, task_id: &str, state: TurnState) -> Response {
    request(
        socket_path,
        &Request::Turn {
            task_id: task_id.to_string(),
            state,
        },
    )
}

fn report_id(socket_path: &Path, task_id: &str, kind: AttentionKind, message: &str) -> u64 {
    let response = report(socket_path, task_id, kind, message);
    assert!(response.ok, "{response:?}");
    let task: Task = serde_json::from_value(response.data.unwrap()).unwrap();
    assert_eq!(task.turn, Some(TurnState::Busy));
    task.attention.unwrap().id
}

fn waiting_report(socket_path: &Path, task_id: &str, kind: AttentionKind) -> u64 {
    let id = report_id(socket_path, task_id, kind, "waiting");
    assert!(set_turn(socket_path, task_id, TurnState::Idle).ok);
    id
}

fn expect_input(client: &mut Client) -> (String, u64, String) {
    match client.read_message().unwrap() {
        ServerMessage::Event(RunnerEvent::Input {
            task_id,
            input_id,
            text,
        }) => (task_id, input_id, text),
        other => panic!("expected an input event, got {other:?}"),
    }
}

fn answer_input(
    client: &mut Client,
    task_id: &str,
    input_id: u64,
    outcome: InputOutcome,
    error: Option<&str>,
) -> Response {
    client
        .write_request(&Request::InputResult {
            task_id: task_id.to_string(),
            input_id,
            outcome,
            error: error.map(str::to_string),
        })
        .unwrap();
    expect_response(client)
}

fn expect_error_containing(response: Response, parts: &[&str]) {
    assert!(!response.ok, "expected an error, got {response:?}");
    let error = response.error.unwrap();
    for part in parts {
        assert!(error.contains(part), "{error:?} does not contain {part:?}");
    }
}

fn expect_error(response: Response, expected: &str) {
    assert!(!response.ok, "expected an error, got {response:?}");
    assert_eq!(response.error.as_deref(), Some(expected));
}

fn open_watch(socket_path: &Path) -> Client {
    let mut client = Client::connect(socket_path).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    client.write_request(&Request::Watch).unwrap();
    client
}

fn next_event(client: &mut Client) -> WatchEvent {
    let line = client
        .read_raw_line()
        .unwrap()
        .expect("watch stream closed unexpectedly");
    serde_json::from_str(line.trim_end()).unwrap()
}

fn expect_task_event(client: &mut Client) -> Task {
    match next_event(client) {
        WatchEvent::Task { task } => task,
        other => panic!("expected a task event, got {other:?}"),
    }
}

#[test]
fn report_sets_attention_only_on_running_tasks_and_finishing_clears_it() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());
    let mut runner = attach_runner(&core.socket_path, "a");

    let first = enqueue(&core.socket_path, "a", "first");
    let second = enqueue(&core.socket_path, "a", "second");
    let (started, _) = expect_start(&mut runner);
    assert_eq!(started.id, first.id);
    assert!(get_task(&core.socket_path, &first.id).attention.is_none());

    expect_error(
        report(&core.socket_path, &second.id, AttentionKind::Question, "q"),
        &format!("task '{}' is 'queued', not 'running'", second.id),
    );
    expect_error(
        report(&core.socket_path, "missing", AttentionKind::Question, "q"),
        "task 'missing' not found",
    );

    let reported = report(
        &core.socket_path,
        &first.id,
        AttentionKind::Question,
        "which db?\n",
    );
    assert!(reported.ok, "{reported:?}");
    let attention = get_task(&core.socket_path, &first.id).attention.unwrap();
    assert_eq!(attention.kind, AttentionKind::Question);
    assert_eq!(attention.message, "which db?");
    assert!(!attention.at.is_empty());

    assert!(report(&core.socket_path, &first.id, AttentionKind::Ready, "done?").ok);
    let attention = get_task(&core.socket_path, &first.id).attention.unwrap();
    assert_eq!(attention.kind, AttentionKind::Ready);
    assert_eq!(attention.message, "done?");

    call(
        &core.socket_path,
        &Request::Complete {
            task_id: first.id.clone(),
        },
    )
    .unwrap();
    assert_eq!(expect_stop(&mut runner), first.id);
    let done = get_task(&core.socket_path, &first.id);
    assert_eq!(done.status, TaskStatus::Done);
    assert!(done.attention.is_none());
    expect_error(
        report(&core.socket_path, &first.id, AttentionKind::Gate, "g"),
        &format!("task '{}' is 'done', not 'running'", first.id),
    );

    let (started, _) = expect_start(&mut runner);
    assert_eq!(started.id, second.id);
    assert!(
        report(
            &core.socket_path,
            &second.id,
            AttentionKind::Blocked,
            "stuck"
        )
        .ok
    );
    drop(runner);
    let interrupted = wait_for_status(&core.socket_path, &second.id, TaskStatus::Interrupted);
    assert!(interrupted.attention.is_none());

    call(&core.socket_path, &Request::Shutdown { force: true }).unwrap();
    core.handle.join().unwrap();
}

#[test]
fn watch_sends_snapshot_then_synced_then_every_change() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());
    let mut runner = attach_runner(&core.socket_path, "a");

    let held = enqueue_from(&core.socket_path, "a", "held in inbox", "inbox");
    let finished = enqueue(&core.socket_path, "a", "finished before watching");
    expect_start(&mut runner);
    call(
        &core.socket_path,
        &Request::Complete {
            task_id: finished.id.clone(),
        },
    )
    .unwrap();
    expect_stop(&mut runner);

    let mut watch = open_watch(&core.socket_path);
    let snapshot = expect_task_event(&mut watch);
    assert_eq!(snapshot.id, held.id);
    assert_eq!(snapshot.status, TaskStatus::Received);
    assert!(matches!(next_event(&mut watch), WatchEvent::Synced));

    let task = enqueue(&core.socket_path, "a", "watched");
    let created = expect_task_event(&mut watch);
    assert_eq!(
        (created.id.as_str(), created.status),
        (task.id.as_str(), TaskStatus::Queued)
    );
    let started = expect_task_event(&mut watch);
    assert_eq!(
        (started.id.as_str(), started.status),
        (task.id.as_str(), TaskStatus::Running)
    );
    expect_start(&mut runner);

    assert!(report(&core.socket_path, &task.id, AttentionKind::Gate, "push?").ok);
    let reported = expect_task_event(&mut watch);
    assert_eq!(reported.attention.unwrap().message, "push?");

    call(
        &core.socket_path,
        &Request::Edit {
            task_id: held.id.clone(),
            text: "edited".to_string(),
        },
    )
    .unwrap();
    let edited = expect_task_event(&mut watch);
    assert_eq!(
        (edited.id.as_str(), edited.text.as_str()),
        (held.id.as_str(), "edited")
    );

    call(
        &core.socket_path,
        &Request::Complete {
            task_id: task.id.clone(),
        },
    )
    .unwrap();
    expect_stop(&mut runner);
    let completed = expect_task_event(&mut watch);
    assert_eq!(completed.status, TaskStatus::Done);
    assert!(completed.attention.is_none());

    call(
        &core.socket_path,
        &Request::Delete {
            task_id: task.id.clone(),
        },
    )
    .unwrap();
    match next_event(&mut watch) {
        WatchEvent::TaskDeleted { task_id } => assert_eq!(task_id, task.id),
        other => panic!("expected task_deleted, got {other:?}"),
    }

    let mut late = Client::connect(&core.socket_path).unwrap();
    late.call(&Request::Status).unwrap();
    expect_error(
        late.call(&Request::Watch).unwrap(),
        "watch must be the first message on a connection",
    );

    call(&core.socket_path, &Request::Shutdown { force: true }).unwrap();
    assert!(watch.read_raw_line().unwrap().is_none());
    core.handle.join().unwrap();
}

#[test]
fn watch_reports_lag_and_closes_the_stream() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());
    let held = enqueue_from(&core.socket_path, "a", "held", "inbox");

    let mut watch = open_watch(&core.socket_path);
    let mut editor = Client::connect(&core.socket_path).unwrap();
    let text = "x".repeat(4096);
    for _ in 0..2000 {
        let response = editor
            .call(&Request::Edit {
                task_id: held.id.clone(),
                text: text.clone(),
            })
            .unwrap();
        assert!(response.ok);
    }

    let mut last = None;
    while let Some(line) = watch.read_raw_line().unwrap() {
        last = Some(serde_json::from_str::<WatchEvent>(line.trim_end()).unwrap());
    }
    match last {
        Some(WatchEvent::Error { error }) => assert!(error.contains("fell behind"), "{error}"),
        other => panic!("expected the stream to end with an error, got {other:?}"),
    }

    call(&core.socket_path, &Request::Shutdown { force: true }).unwrap();
    core.handle.join().unwrap();
}

#[test]
fn turn_follows_the_agent_and_reports_get_increasing_ids() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());
    let mut runner = attach_runner(&core.socket_path, "a");

    let task = enqueue(&core.socket_path, "a", "worker");
    let queued = enqueue(&core.socket_path, "a", "waits");
    expect_start(&mut runner);
    assert_eq!(
        get_task(&core.socket_path, &task.id).turn,
        Some(TurnState::Busy)
    );
    assert_eq!(get_task(&core.socket_path, &queued.id).turn, None);
    expect_error(
        set_turn(&core.socket_path, &queued.id, TurnState::Idle),
        &format!("task '{}' is 'queued', not 'running'", queued.id),
    );
    expect_error(
        set_turn(&core.socket_path, "missing", TurnState::Idle),
        "task 'missing' not found",
    );

    let mut watch = open_watch(&core.socket_path);
    assert_eq!(expect_task_event(&mut watch).id, task.id);
    expect_task_event(&mut watch);
    assert!(matches!(next_event(&mut watch), WatchEvent::Synced));

    assert!(set_turn(&core.socket_path, &task.id, TurnState::Idle).ok);
    assert_eq!(expect_task_event(&mut watch).turn, Some(TurnState::Idle));
    assert!(set_turn(&core.socket_path, &task.id, TurnState::Idle).ok);
    assert!(set_turn(&core.socket_path, &task.id, TurnState::Busy).ok);
    assert_eq!(expect_task_event(&mut watch).turn, Some(TurnState::Busy));
    assert!(set_turn(&core.socket_path, &task.id, TurnState::Busy).ok);

    let first = report_id(&core.socket_path, &task.id, AttentionKind::Question, "a");
    let reported = expect_task_event(&mut watch);
    assert_eq!(reported.attention.unwrap().id, first);
    assert!(set_turn(&core.socket_path, &task.id, TurnState::Idle).ok);
    let second = report_id(&core.socket_path, &task.id, AttentionKind::Gate, "b");
    let third = report_id(&core.socket_path, &task.id, AttentionKind::Blocked, "c");
    assert!(first < second && second < third, "{first} {second} {third}");
    assert_eq!(
        get_task(&core.socket_path, &task.id).turn,
        Some(TurnState::Busy)
    );

    call(
        &core.socket_path,
        &Request::Complete {
            task_id: task.id.clone(),
        },
    )
    .unwrap();
    expect_stop(&mut runner);
    let done = get_task(&core.socket_path, &task.id);
    assert_eq!((done.turn, done.attention), (None, None));
    let (next, _) = expect_start(&mut runner);
    assert_eq!(next.id, queued.id);
    assert_eq!(next.turn, Some(TurnState::Busy));

    call(&core.socket_path, &Request::Shutdown { force: true }).unwrap();
    core.handle.join().unwrap();
}

#[test]
fn send_is_refused_unless_the_worker_ended_its_turn_after_the_expected_report() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());
    let mut runner = attach_runner(&core.socket_path, "a");

    let task = enqueue(&core.socket_path, "a", "needs answers");
    let queued = enqueue(&core.socket_path, "a", "waits its turn");
    expect_start(&mut runner);
    let socket = core.socket_path.as_path();

    expect_error(
        send_reply(socket, &task.id, 1, "hello"),
        &format!(
            "task '{}' is not waiting for a reply (no attention)",
            task.id
        ),
    );
    expect_error(
        send_reply(socket, &queued.id, 1, "hello"),
        &format!("task '{}' is 'queued', not 'running'", queued.id),
    );
    expect_error(
        send_reply(socket, "missing", 1, "hello"),
        "task 'missing' not found",
    );

    let asked = report_id(socket, &task.id, AttentionKind::Question, "which db?");
    expect_error(
        send_reply(socket, &task.id, asked, "sqlite"),
        &format!(
            "task '{}' is still in its turn (turn: busy); wait until its agent ends the turn",
            task.id
        ),
    );

    assert!(set_turn(socket, &task.id, TurnState::Idle).ok);
    let asked_again = report_id(
        socket,
        &task.id,
        AttentionKind::Question,
        "which db, again?",
    );
    expect_error_containing(
        send_reply(socket, &task.id, asked_again, "sqlite"),
        &["turn: busy"],
    );

    assert!(set_turn(socket, &task.id, TurnState::Idle).ok);
    expect_error(
        send_reply(socket, &task.id, asked, "sqlite"),
        &format!(
            "the reply is for report {asked}, but the current report of task '{}' is {asked_again}; read it before replying",
            task.id
        ),
    );

    for (text, expected) in [
        (" \r\n", "reply text is empty"),
        ("yes\x1b[Z", "U+001B"),
        ("yes\tno", "U+0009"),
        ("\x03", "U+0003"),
        ("yes\u{9b}", "U+009B"),
        ("!rm -rf target", "shell command"),
        ("  /permissions", "slash command"),
    ] {
        expect_error_containing(send_reply(socket, &task.id, asked_again, text), &[expected]);
    }
    expect_error_containing(
        send_reply(socket, &task.id, asked_again, &"y".repeat(16 * 1024 + 1)),
        &["limit is 16384"],
    );
    let task_now = get_task(socket, &task.id);
    assert_eq!(task_now.attention.unwrap().id, asked_again);
    assert_eq!(task_now.turn, Some(TurnState::Idle));

    let blocked = waiting_report(socket, &task.id, AttentionKind::Blocked);
    expect_error(
        send_reply(socket, &task.id, blocked, "yes"),
        &format!(
            "task '{}' is not waiting for a reply (attention: blocked)",
            task.id
        ),
    );

    expect_error_containing(
        report(socket, &task.id, AttentionKind::Question, "a\0b"),
        &["NUL"],
    );
    expect_error_containing(
        report(
            socket,
            &task.id,
            AttentionKind::Question,
            &"m".repeat(16 * 1024 + 1),
        ),
        &["limit is 16384"],
    );
    assert_eq!(get_task(socket, &task.id).attention.unwrap().id, blocked);

    let other = enqueue(socket, "b", "no runner for b");
    wait_for_status(socket, &other.id, TaskStatus::Running);
    let other_report = waiting_report(socket, &other.id, AttentionKind::Question);
    expect_error(
        send_reply(socket, &other.id, other_report, "hello"),
        "no runner is attached for workspace 'b'",
    );

    call(socket, &Request::Shutdown { force: true }).unwrap();
    core.handle.join().unwrap();
}

#[test]
fn send_answers_after_the_runner_typed_the_reply() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());
    let mut runner = attach_runner(&core.socket_path, "a");
    let task = enqueue(&core.socket_path, "a", "needs answers");
    expect_start(&mut runner);
    let socket = core.socket_path.as_path();

    for (kind, text, expected) in [
        (AttentionKind::Question, "use sqlite\r\n", "use sqlite"),
        (
            AttentionKind::Gate,
            "go ahead\rpush it\n\n",
            "go ahead\npush it",
        ),
        (
            AttentionKind::Ready,
            "-y please add tests",
            "-y please add tests",
        ),
    ] {
        let expect = waiting_report(socket, &task.id, kind);
        let pending = send_in_background(socket, &task.id, expect, text);
        let (task_id, input_id, typed) = expect_input(&mut runner);
        assert_eq!(
            (task_id.as_str(), typed.as_str()),
            (task.id.as_str(), expected)
        );
        assert!(input_id > expect);

        let in_flight = get_task(socket, &task.id);
        assert_eq!(
            (in_flight.attention, in_flight.turn),
            (None, Some(TurnState::Busy))
        );
        expect_error(
            send_reply(socket, &task.id, expect, "again"),
            &format!("a reply to task '{}' is still being delivered", task.id),
        );
        assert!(!pending.is_finished());

        expect_error_containing(
            answer_input(
                &mut runner,
                &task.id,
                input_id + 1000,
                InputOutcome::Typed,
                None,
            ),
            &["no reply"],
        );
        assert!(answer_input(&mut runner, &task.id, input_id, InputOutcome::Typed, None).ok);
        let sent = pending.join().unwrap();
        assert!(sent.ok, "{sent:?}");
        let sent: Task = serde_json::from_value(sent.data.unwrap()).unwrap();
        assert_eq!((sent.attention, sent.turn), (None, Some(TurnState::Busy)));
        expect_error(
            send_reply(socket, &task.id, expect, text),
            &format!(
                "task '{}' is not waiting for a reply (no attention)",
                task.id
            ),
        );
    }

    let mut client = Client::connect(socket).unwrap();
    expect_error(
        client
            .call(&Request::InputResult {
                task_id: task.id.clone(),
                input_id: 1,
                outcome: InputOutcome::Typed,
                error: None,
            })
            .unwrap(),
        "input_result is only accepted on a runner connection",
    );

    call(socket, &Request::Shutdown { force: true }).unwrap();
    core.handle.join().unwrap();
}

#[test]
fn not_typed_reply_reopens_the_report_unless_a_newer_one_replaced_it() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());
    let mut runner = attach_runner(&core.socket_path, "a");
    let task = enqueue(&core.socket_path, "a", "needs answers");
    expect_start(&mut runner);
    let socket = core.socket_path.as_path();

    let expect = waiting_report(socket, &task.id, AttentionKind::Question);
    let pending = send_in_background(socket, &task.id, expect, "sqlite");
    let (_, input_id, _) = expect_input(&mut runner);
    assert!(
        answer_input(
            &mut runner,
            &task.id,
            input_id,
            InputOutcome::NotTyped,
            Some("no pane")
        )
        .ok
    );
    expect_error(
        pending.join().unwrap(),
        &format!(
            "the reply was not typed into task '{}': no pane; its report is open again",
            task.id
        ),
    );
    let reopened = get_task(socket, &task.id);
    assert_eq!(reopened.attention.unwrap().id, expect);
    assert_eq!(reopened.turn, Some(TurnState::Idle));

    let pending = send_in_background(socket, &task.id, expect, "sqlite");
    let (_, input_id, _) = expect_input(&mut runner);
    let newer = report_id(socket, &task.id, AttentionKind::Gate, "push?");
    assert!(
        answer_input(
            &mut runner,
            &task.id,
            input_id,
            InputOutcome::NotTyped,
            None
        )
        .ok
    );
    expect_error_containing(pending.join().unwrap(), &["not typed", "replaced"]);
    let replaced = get_task(socket, &task.id);
    assert_eq!(replaced.attention.unwrap().id, newer);
    assert_eq!(replaced.turn, Some(TurnState::Busy));

    call(socket, &Request::Shutdown { force: true }).unwrap();
    core.handle.join().unwrap();
}

#[test]
fn unconfirmed_delivery_leaves_the_report_closed() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        std::env::set_var("ZELLOOM_INPUT_TIMEOUT_MS", "300");
    }
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());
    unsafe {
        std::env::remove_var("ZELLOOM_INPUT_TIMEOUT_MS");
    }
    let mut runner = attach_runner(&core.socket_path, "a");
    let task = enqueue(&core.socket_path, "a", "needs answers");
    expect_start(&mut runner);
    let socket = core.socket_path.as_path();
    let assert_closed = || {
        let task = get_task(socket, &task.id);
        assert_eq!((task.attention, task.turn), (None, Some(TurnState::Busy)));
    };

    let expect = waiting_report(socket, &task.id, AttentionKind::Question);
    let pending = send_in_background(socket, &task.id, expect, "sqlite");
    let (_, input_id, _) = expect_input(&mut runner);
    assert!(
        answer_input(
            &mut runner,
            &task.id,
            input_id,
            InputOutcome::Partial,
            Some("Enter was not sent")
        )
        .ok
    );
    expect_error_containing(
        pending.join().unwrap(),
        &[
            "unconfirmed",
            "only part",
            "Enter was not sent",
            "check its tab",
        ],
    );
    assert_closed();

    let expect = waiting_report(socket, &task.id, AttentionKind::Question);
    let pending = send_in_background(socket, &task.id, expect, "sqlite");
    let (_, input_id, _) = expect_input(&mut runner);
    expect_error_containing(
        pending.join().unwrap(),
        &["unconfirmed", "did not answer within 300 ms"],
    );
    assert_closed();
    expect_error_containing(
        answer_input(&mut runner, &task.id, input_id, InputOutcome::Typed, None),
        &["no reply"],
    );

    let expect = waiting_report(socket, &task.id, AttentionKind::Question);
    let pending = send_in_background(socket, &task.id, expect, "sqlite");
    expect_input(&mut runner);
    drop(runner);
    expect_error_containing(
        pending.join().unwrap(),
        &["unconfirmed", "the runner disconnected"],
    );
    let interrupted = wait_for_status(socket, &task.id, TaskStatus::Interrupted);
    assert_eq!((interrupted.attention, interrupted.turn), (None, None));

    call(socket, &Request::Shutdown { force: true }).unwrap();
    core.handle.join().unwrap();
}

#[test]
fn agent_gone_closes_the_report_and_restart_starts_a_busy_turn() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());
    let mut runner = attach_runner(&core.socket_path, "a");
    let task = enqueue(&core.socket_path, "a", "needs answers");
    expect_start(&mut runner);
    let socket = core.socket_path.as_path();

    let expect = waiting_report(socket, &task.id, AttentionKind::Question);
    expect_error(
        request(
            socket,
            &Request::AgentGone {
                task_id: task.id.clone(),
            },
        ),
        "agent_gone is only accepted on a runner connection",
    );
    runner
        .write_request(&Request::AgentGone {
            task_id: "missing".to_string(),
        })
        .unwrap();
    assert!(!expect_response(&mut runner).ok);
    runner
        .write_request(&Request::AgentGone {
            task_id: task.id.clone(),
        })
        .unwrap();
    let gone: Task = serde_json::from_value(expect_response(&mut runner).data.unwrap()).unwrap();
    assert_eq!((gone.attention, gone.turn), (None, Some(TurnState::Busy)));
    expect_error(
        send_reply(socket, &task.id, expect, "sqlite"),
        &format!(
            "task '{}' is not waiting for a reply (no attention)",
            task.id
        ),
    );

    waiting_report(socket, &task.id, AttentionKind::Question);
    runner
        .write_request(&Request::AgentExited {
            task_id: task.id.clone(),
            outcome: Outcome::Restart,
        })
        .unwrap();
    let (restarted, _) = expect_start(&mut runner);
    assert_eq!(restarted.id, task.id);
    assert_eq!(
        (restarted.attention, restarted.turn),
        (None, Some(TurnState::Busy))
    );
    assert!(expect_response(&mut runner).ok);

    call(socket, &Request::Shutdown { force: true }).unwrap();
    core.handle.join().unwrap();
}

const REQUEST_LINE_LIMIT: usize = 1024 * 1024;

fn padded_list_line(total_bytes: usize) -> Vec<u8> {
    let mut line = br#"{"op":"list"}"#.to_vec();
    line.resize(total_bytes - 1, b' ');
    line.push(b'\n');
    line
}

fn send_raw_line(socket_path: &Path, line: &[u8]) -> Response {
    use std::io::{BufRead, BufReader, Write};

    let mut stream = std::os::unix::net::UnixStream::connect(socket_path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let _ = stream.write_all(line);
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).unwrap();
    serde_json::from_str(reply.trim_end()).unwrap()
}

#[test]
fn request_lines_are_limited_to_one_mebibyte_on_every_connection_type() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());
    let socket = core.socket_path.as_path();

    assert!(send_raw_line(socket, &padded_list_line(REQUEST_LINE_LIMIT)).ok);
    expect_error_containing(
        send_raw_line(socket, &padded_list_line(REQUEST_LINE_LIMIT + 1)),
        &["exceeds"],
    );

    let big_text = "x".repeat(900 * 1024);
    let task = enqueue_from(socket, "a", &big_text, "inbox");
    assert_eq!(get_task(socket, &task.id).text.len(), big_text.len());

    let oversized = Request::Enqueue {
        text: "y".repeat(REQUEST_LINE_LIMIT + 1),
        workspace: "a".to_string(),
        agent: None,
        source: Source::cli(),
        reply_to: None,
        metadata: serde_json::json!({}),
    };
    let mut client = Client::connect(socket).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    assert!(client.call(&Request::List).unwrap().ok);
    expect_error_containing(client.call(&oversized).unwrap(), &["exceeds"]);
    expect_connection_closed(&mut client);

    let mut runner = attach_runner(socket, "a");
    runner
        .write_request(&Request::AgentExited {
            task_id: "x".to_string(),
            outcome: Outcome::Done,
        })
        .unwrap();
    expect_response(&mut runner);
    expect_error_containing(runner.call(&oversized).unwrap(), &["exceeds"]);
    expect_connection_closed(&mut runner);

    assert_eq!(get_task(socket, &task.id).status, TaskStatus::Received);
    call(socket, &Request::Shutdown { force: true }).unwrap();
    core.handle.join().unwrap();
}

#[test]
fn get_returns_one_task_or_not_found() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());
    let socket = core.socket_path.as_path();

    let task = enqueue_from(socket, "a", "find me", "inbox");
    enqueue_from(socket, "b", "another", "inbox");
    let response = request(
        socket,
        &Request::Get {
            task_id: task.id.clone(),
        },
    );
    let found: Task = serde_json::from_value(response.data.unwrap()).unwrap();
    assert_eq!((found.id, found.text), (task.id, "find me".to_string()));
    expect_error(
        request(
            socket,
            &Request::Get {
                task_id: "nope".to_string(),
            },
        ),
        "task 'nope' not found",
    );

    call(socket, &Request::Shutdown { force: true }).unwrap();
    core.handle.join().unwrap();
}

#[test]
fn watcher_that_never_reads_is_disconnected_after_the_write_timeout() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        std::env::set_var("ZELLOOM_WATCH_WRITE_TIMEOUT_MS", "300");
    }
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path());
    unsafe {
        std::env::remove_var("ZELLOOM_WATCH_WRITE_TIMEOUT_MS");
    }
    let socket = core.socket_path.as_path();

    enqueue_from(socket, "a", &"x".repeat(900 * 1024), "inbox");
    let mut watch = open_watch(socket);
    std::thread::sleep(Duration::from_millis(1500));
    let mut received = 0;
    while let Some(line) = watch.read_raw_line().expect("the stream was not closed") {
        received += line.len();
    }
    assert!(received < 900 * 1024, "{received}");

    assert!(request(socket, &Request::List).ok);
    call(socket, &Request::Shutdown { force: true }).unwrap();
    core.handle.join().unwrap();
}
