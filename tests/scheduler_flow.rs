use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// `std::env::set_var` is a process-wide, non-atomic mutation, so tests setting `ZELLOOM_ATTACH_TIMEOUT_MS` must hold this lock while it's set.
static ENV_LOCK: Mutex<()> = Mutex::new(());

use zelloom::cli::{ensure_core, stop_core};
use zelloom::client::{Client, ClientError, call};
use zelloom::core::{CoreOptions, NoopLauncher, TabLauncher, run as core_run};
use zelloom::protocol::{
    Outcome, Request, ResolvedAgent, Response, RunnerEvent, ServerMessage, Source, Task, TaskStatus,
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
    let request = Request::Enqueue {
        text: text.to_string(),
        workspace: workspace.to_string(),
        agent: None,
        source: Source::cli(),
        reply_to: None,
        metadata: serde_json::json!({}),
    };
    let data = call(socket_path, &request).unwrap();
    serde_json::from_value(data).unwrap()
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

fn call_with_timeout(socket_path: &Path, request: &Request) -> Response {
    let mut client = Client::connect(socket_path).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    client
        .call(request)
        .expect("core did not respond in time (deadlock?)")
}

fn enqueue_with_timeout(socket_path: &Path, workspace: &str, text: &str) -> Task {
    let response = call_with_timeout(
        socket_path,
        &Request::Enqueue {
            text: text.to_string(),
            workspace: workspace.to_string(),
            agent: None,
            source: Source::cli(),
            reply_to: None,
            metadata: serde_json::json!({}),
        },
    );
    serde_json::from_value(response.into_result().unwrap().unwrap()).unwrap()
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

    let first = enqueue_with_timeout(&core.socket_path, "a", "no runner for a");
    let second = enqueue_with_timeout(&core.socket_path, "a", "still no runner for a");

    let response = call_with_timeout(&core.socket_path, &Request::List);
    let tasks: Vec<Task> =
        serde_json::from_value(response.into_result().unwrap().unwrap()).unwrap();
    for id in [&first.id, &second.id] {
        let task = tasks.iter().find(|t| &t.id == id).unwrap();
        assert_eq!(task.status, TaskStatus::Interrupted);
    }

    let mut runner_b = attach_runner(&core.socket_path, "b");
    let task_b = enqueue_with_timeout(&core.socket_path, "b", "b has a runner");
    let (started, _) = expect_start(&mut runner_b);
    assert_eq!(started.id, task_b.id);

    call_with_timeout(&core.socket_path, &Request::Shutdown { force: true });
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
