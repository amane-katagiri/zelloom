use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{Mutex, watch};

use super::agent::{
    build_argv, build_env, build_instruction, loom_exe_path, validate_reply,
    validate_report_message,
};
use crate::config::{self, AgentConfig, Config, WorkspaceConfig};
use crate::protocol::{
    AttentionKind, InputOutcome, Outcome, ResolvedAgent, Response, RunnerEvent, Source, Task,
    TaskStatus, TurnState, WatchEvent,
};
use crate::store::{AcceptedSend, NewTask, SendRejection, Store, StoreError};

pub trait TabLauncher: Send + Sync {
    fn launch(&self, workspace_id: &str, workspace_path: &Path) -> anyhow::Result<()>;
}

pub struct NoopLauncher;

impl TabLauncher for NoopLauncher {
    fn launch(&self, workspace_id: &str, workspace_path: &Path) -> anyhow::Result<()> {
        eprintln!(
            "[zelloom-core] launch hook: workspace '{workspace_id}' at {} has no attached runner; \
             a real TabLauncher (e.g. Zellij) must be plugged in to open a tab and run `loom runner {workspace_id}`",
            workspace_path.display()
        );
        Ok(())
    }
}

const DEFAULT_ATTACH_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_INPUT_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_WATCH_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

fn duration_from_env(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(default)
}

pub fn attach_timeout_from_env() -> Duration {
    duration_from_env("ZELLOOM_ATTACH_TIMEOUT_MS", DEFAULT_ATTACH_TIMEOUT)
}

pub fn input_timeout_from_env() -> Duration {
    duration_from_env("ZELLOOM_INPUT_TIMEOUT_MS", DEFAULT_INPUT_TIMEOUT)
}

pub fn watch_write_timeout_from_env() -> Duration {
    duration_from_env(
        "ZELLOOM_WATCH_WRITE_TIMEOUT_MS",
        DEFAULT_WATCH_WRITE_TIMEOUT,
    )
}

struct InputReport {
    outcome: InputOutcome,
    error: Option<String>,
}

struct PendingInput {
    task_id: String,
    attach_id: u64,
    reply: tokio::sync::oneshot::Sender<InputReport>,
}

struct RunnerHandle {
    attach_id: u64,
    sender: tokio::sync::mpsc::UnboundedSender<String>,
    current_task: Option<String>,
}

pub struct Scheduler {
    store: Arc<Store>,
    config_path: PathBuf,
    socket_path: PathBuf,
    launcher: Arc<dyn TabLauncher>,
    attach_timeout: Duration,
    input_timeout: Duration,
    pub zellij_session_name: Option<String>,
    runners: Mutex<HashMap<String, RunnerHandle>>,
    pending_inputs: std::sync::Mutex<HashMap<u64, PendingInput>>,
    next_attach_id: AtomicU64,
    // Per-task counter bumped on each launch attempt so a stale attach-timeout captured at an earlier generation becomes a no-op instead of interrupting a newer attempt.
    launch_generation: Mutex<HashMap<String, u64>>,
    // Held for the whole body of `schedule()` so concurrent callers can't dispatch off the same stale "task/workspace free" snapshot.
    scheduling_lock: Mutex<()>,
    shutdown: watch::Sender<bool>,
}

fn task_not_found(task_id: &str) -> Response {
    Response::err(format!("task '{task_id}' not found"))
}

fn task_not_running(task_id: &str, status: TaskStatus) -> Response {
    Response::err(format!(
        "task '{task_id}' is '{}', not 'running'",
        status.as_str()
    ))
}

pub(crate) fn send_json<T: serde::Serialize>(
    sender: &tokio::sync::mpsc::UnboundedSender<String>,
    value: &T,
) {
    if let Ok(mut line) = serde_json::to_string(value) {
        line.push('\n');
        let _ = sender.send(line);
    }
}

fn finishable_from(target: TaskStatus) -> &'static [TaskStatus] {
    match target {
        TaskStatus::Cancelled => &[
            TaskStatus::Running,
            TaskStatus::Interrupted,
            TaskStatus::Queued,
            TaskStatus::Received,
        ],
        _ => &[TaskStatus::Running, TaskStatus::Interrupted],
    }
}

impl Scheduler {
    pub fn new(
        store: Arc<Store>,
        config_path: PathBuf,
        socket_path: PathBuf,
        launcher: Arc<dyn TabLauncher>,
        zellij_session_name: Option<String>,
        attach_timeout: Duration,
        input_timeout: Duration,
    ) -> Scheduler {
        Scheduler {
            store,
            config_path,
            socket_path,
            launcher,
            attach_timeout,
            input_timeout,
            zellij_session_name,
            runners: Mutex::new(HashMap::new()),
            pending_inputs: std::sync::Mutex::new(HashMap::new()),
            next_attach_id: AtomicU64::new(1),
            launch_generation: Mutex::new(HashMap::new()),
            scheduling_lock: Mutex::new(()),
            shutdown: watch::Sender::new(false),
        }
    }

    fn load_config(&self) -> Result<Config, String> {
        config::load(&self.config_path).map_err(|e| e.to_string())
    }

    fn resolved_agent(
        &self,
        agent_cfg: &AgentConfig,
        ws: &WorkspaceConfig,
        task: &Task,
    ) -> ResolvedAgent {
        let loom_exe = loom_exe_path();
        let instruction = build_instruction(agent_cfg, &loom_exe);
        let argv = build_argv(agent_cfg, &instruction, &task.text);
        let env = build_env(
            agent_cfg,
            ws,
            task,
            &self.socket_path.to_string_lossy(),
            &loom_exe,
        );
        ResolvedAgent {
            argv,
            cwd: ws.path.to_string_lossy().into_owned(),
            env,
            shell: agent_cfg.shell,
            oneshot: agent_cfg.oneshot,
        }
    }

    pub async fn handle_request(self: &Arc<Self>, request: crate::protocol::Request) -> Response {
        use crate::protocol::Request;
        match request {
            Request::Enqueue {
                text,
                workspace,
                agent,
                source,
                reply_to,
                metadata,
            } => {
                self.enqueue(text, workspace, agent, source, reply_to, metadata)
                    .await
            }
            Request::List => self.list().await,
            Request::Get { task_id } => self.get(&task_id),
            Request::Complete { task_id } => self.finish(&task_id, TaskStatus::Done).await,
            Request::Fail { task_id } => self.finish(&task_id, TaskStatus::Failed).await,
            Request::Cancel { task_id } => self.finish(&task_id, TaskStatus::Cancelled).await,
            Request::Delete { task_id } => self.delete(&task_id).await,
            Request::Edit { task_id, text } => self.edit(&task_id, text).await,
            Request::Move { task_id, direction } => self.move_task(&task_id, direction).await,
            Request::Accept { task_id } => self.accept(&task_id).await,
            Request::Reject { task_id } => self.reject(&task_id).await,
            Request::Retry { task_id } => self.retry(&task_id).await,
            Request::Status => self.status().await,
            Request::Shutdown { force } => self.shutdown(force).await,
            Request::RunnerAttach { .. } => {
                Response::err("runner_attach must be the first message on a connection")
            }
            Request::AgentExited { .. } => {
                Response::err("agent_exited is only accepted on a runner connection")
            }
            Request::InputResult { .. } => {
                Response::err("input_result is only accepted on a runner connection")
            }
            Request::AgentGone { .. } => {
                Response::err("agent_gone is only accepted on a runner connection")
            }
            Request::Turn { task_id, state } => self.turn(&task_id, state),
            Request::Report {
                task_id,
                kind,
                message,
            } => self.report(&task_id, kind, message),
            Request::Watch => Response::err("watch must be the first message on a connection"),
            Request::Send {
                task_id,
                expect,
                text,
            } => self.send_reply(&task_id, expect, &text).await,
        }
    }

    pub fn watch(
        &self,
    ) -> Result<(Vec<Task>, tokio::sync::broadcast::Receiver<WatchEvent>), StoreError> {
        self.store.watch()
    }

    fn not_running_error(&self, task_id: &str) -> Response {
        match self.current_status(task_id) {
            Ok(current) => task_not_running(task_id, current),
            Err(response) => response,
        }
    }

    fn report(&self, task_id: &str, kind: AttentionKind, message: String) -> Response {
        let message = match validate_report_message(&message) {
            Ok(m) => m,
            Err(e) => return Response::err(e),
        };
        match self.store.set_attention(task_id, kind, message) {
            Ok(Some(task)) => Response::ok(serde_json::to_value(&task).unwrap()),
            Ok(None) => self.not_running_error(task_id),
            Err(e) => Response::err(e.to_string()),
        }
    }

    fn turn(&self, task_id: &str, state: TurnState) -> Response {
        match self.store.set_turn(task_id, state) {
            Ok(Some(task)) => Response::ok(serde_json::to_value(&task).unwrap()),
            Ok(None) => self.not_running_error(task_id),
            Err(e) => Response::err(e.to_string()),
        }
    }

    fn send_rejection(task_id: &str, expect: u64, rejection: SendRejection) -> Response {
        let message = match rejection {
            SendRejection::NotFound => return task_not_found(task_id),
            SendRejection::NotRunning(status) => return task_not_running(task_id, status),
            SendRejection::InFlight => {
                format!("a reply to task '{task_id}' is still being delivered")
            }
            SendRejection::NoAttention => {
                format!("task '{task_id}' is not waiting for a reply (no attention)")
            }
            SendRejection::NotAwaitingReply(kind) => format!(
                "task '{task_id}' is not waiting for a reply (attention: {})",
                kind.as_str()
            ),
            SendRejection::ExpectMismatch { current } => format!(
                "the reply is for report {expect}, but the current report of task '{task_id}' is {current}; read it before replying"
            ),
            SendRejection::TurnBusy => format!(
                "task '{task_id}' is still in its turn (turn: busy); wait until its agent ends the turn"
            ),
            SendRejection::TurnNotEndedSinceReport => format!(
                "task '{task_id}' has not ended its turn since report {expect}; wait until it does"
            ),
        };
        Response::err(message)
    }

    async fn send_reply(&self, task_id: &str, expect: u64, text: &str) -> Response {
        let text = match validate_reply(text) {
            Ok(t) => t,
            Err(e) => return Response::err(e),
        };
        let task = match self.running_task(task_id) {
            Ok(task) => task,
            Err(response) => return response,
        };
        let (accepted, report) = {
            let runners = self.runners.lock().await;
            let Some(handle) = runners.get(&task.workspace) else {
                return Response::err(format!(
                    "no runner is attached for workspace '{}'",
                    task.workspace
                ));
            };
            if handle.current_task.as_deref() != Some(task_id) {
                return Response::err(format!(
                    "task '{task_id}' is not the current task of the runner for workspace '{}'",
                    task.workspace
                ));
            }
            let accepted = match self.store.begin_send(task_id, expect) {
                Ok(Ok(accepted)) => accepted,
                Ok(Err(rejection)) => return Self::send_rejection(task_id, expect, rejection),
                Err(e) => return Response::err(e.to_string()),
            };
            let (reply, report) = tokio::sync::oneshot::channel();
            self.pending_inputs.lock().unwrap().insert(
                accepted.input_id,
                PendingInput {
                    task_id: task_id.to_string(),
                    attach_id: handle.attach_id,
                    reply,
                },
            );
            send_json(
                &handle.sender,
                &RunnerEvent::Input {
                    task_id: task_id.to_string(),
                    input_id: accepted.input_id,
                    text,
                },
            );
            (accepted, report)
        };
        let waited = tokio::time::timeout(self.input_timeout, report).await;
        self.pending_inputs
            .lock()
            .unwrap()
            .remove(&accepted.input_id);
        self.finish_reply(task_id, accepted, waited)
    }

    fn finish_reply(
        &self,
        task_id: &str,
        accepted: AcceptedSend,
        waited: Result<
            Result<InputReport, tokio::sync::oneshot::error::RecvError>,
            tokio::time::error::Elapsed,
        >,
    ) -> Response {
        let AcceptedSend {
            task,
            input_id,
            previous,
        } = accepted;
        let unconfirmed = |why: String| {
            Response::err(format!(
                "delivery of the reply to task '{task_id}' is unconfirmed: {why}; check its tab before replying again"
            ))
        };
        let detail = |error: Option<String>| error.map(|e| format!(": {e}")).unwrap_or_default();
        match waited {
            Ok(Ok(InputReport {
                outcome: InputOutcome::NotTyped,
                error,
            })) => {
                let restored = self.store.abort_send(task_id, input_id, previous);
                let state = match restored {
                    Ok(true) => "its report is open again".to_string(),
                    Ok(false) => "its report was replaced or the agent is gone".to_string(),
                    Err(e) => format!("failed to restore its report: {e}"),
                };
                Response::err(format!(
                    "the reply was not typed into task '{task_id}'{}; {state}",
                    detail(error)
                ))
            }
            other => {
                if let Err(e) = self.store.finish_send(task_id, input_id) {
                    eprintln!("[zelloom-core] failed to finish the reply to task {task_id}: {e}");
                }
                match other {
                    Ok(Ok(InputReport {
                        outcome: InputOutcome::Typed,
                        ..
                    })) => {
                        let task = self.store.get(task_id).ok().flatten().unwrap_or(task);
                        Response::ok(serde_json::to_value(&task).unwrap())
                    }
                    Ok(Ok(InputReport { error, .. })) => unconfirmed(format!(
                        "the runner may have typed only part of it{}",
                        detail(error)
                    )),
                    Ok(Err(_)) => unconfirmed("the runner disconnected".to_string()),
                    Err(_) => unconfirmed(format!(
                        "the runner did not answer within {} ms",
                        self.input_timeout.as_millis()
                    )),
                }
            }
        }
    }

    pub async fn input_result(
        &self,
        attach_id: u64,
        task_id: &str,
        input_id: u64,
        outcome: InputOutcome,
        error: Option<String>,
    ) -> Response {
        let pending = {
            let mut pending = self.pending_inputs.lock().unwrap();
            match pending.get(&input_id) {
                Some(p) if p.attach_id == attach_id && p.task_id == task_id => {
                    pending.remove(&input_id)
                }
                _ => None,
            }
        };
        match pending {
            Some(p) => {
                let _ = p.reply.send(InputReport { outcome, error });
                Response::ok_empty()
            }
            None => Response::err(format!(
                "no reply {input_id} to task '{task_id}' is waiting for a result from this runner"
            )),
        }
    }

    pub async fn agent_gone(&self, workspace: &str, attach_id: u64, task_id: &str) -> Response {
        let runners = self.runners.lock().await;
        let is_current = runners
            .get(workspace)
            .filter(|h| h.attach_id == attach_id)
            .is_some_and(|h| h.current_task.as_deref() == Some(task_id));
        if !is_current {
            return Response::err(format!(
                "task '{task_id}' is not the current task of this runner for workspace '{workspace}'"
            ));
        }
        match self.store.reset_turn(task_id) {
            Ok(Some(task)) => Response::ok(serde_json::to_value(&task).unwrap()),
            Ok(None) => self.not_running_error(task_id),
            Err(e) => Response::err(e.to_string()),
        }
    }

    async fn enqueue(
        self: &Arc<Self>,
        text: String,
        workspace: String,
        agent: Option<String>,
        source: Source,
        reply_to: Option<serde_json::Value>,
        metadata: serde_json::Value,
    ) -> Response {
        let config = match self.load_config() {
            Ok(c) => c,
            Err(e) => return Response::err(e),
        };
        if !config.workspaces.contains_key(&workspace) {
            return Response::err(format!("workspace '{workspace}' is not registered"));
        }
        if let Some(agent) = &agent
            && !config.agents.contains_key(agent)
        {
            return Response::err(format!("agent '{agent}' is not defined in [agents]"));
        }
        let status = if config.auto_queue(&source.kind) {
            TaskStatus::Queued
        } else {
            TaskStatus::Received
        };
        let new_task = NewTask {
            text,
            workspace,
            agent,
            source,
            reply_to,
            metadata,
            status,
        };
        let task = match self.store.insert(new_task) {
            Ok(t) => t,
            Err(e) => return Response::err(e.to_string()),
        };
        if status == TaskStatus::Queued {
            self.schedule().await;
        }
        Response::ok(serde_json::to_value(&task).unwrap())
    }

    pub fn subscribe_shutdown(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    async fn shutdown(&self, force: bool) -> Response {
        let _guard = self.scheduling_lock.lock().await;
        if !force {
            let running = match self.store.list_by_status(TaskStatus::Running) {
                Ok(tasks) => tasks,
                Err(e) => return Response::err(e.to_string()),
            };
            if !running.is_empty() {
                return Response {
                    ok: false,
                    data: Some(serde_json::json!({ "running": running })),
                    error: Some(format!("{} task(s) are running", running.len())),
                };
            }
        }
        self.shutdown.send_replace(true);
        Response::ok_empty()
    }

    async fn list(&self) -> Response {
        match self.store.list_all() {
            Ok(tasks) => Response::ok(serde_json::to_value(&tasks).unwrap()),
            Err(e) => Response::err(e.to_string()),
        }
    }

    async fn finish(self: &Arc<Self>, task_id: &str, status: TaskStatus) -> Response {
        let task = match self.existing_task(task_id) {
            Ok(task) => task,
            Err(response) => return response,
        };
        let allowed = finishable_from(status);
        if !allowed.contains(&task.status) {
            let names: Vec<&str> = allowed.iter().map(TaskStatus::as_str).collect();
            return Response::err(format!(
                "task '{task_id}' is '{}'; only {} tasks can be marked {}",
                task.status.as_str(),
                names.join(" or "),
                status.as_str()
            ));
        }
        let was_running = task.status == TaskStatus::Running;
        let updated = match self.store.update_status_if(task_id, task.status, status) {
            Ok(Some(t)) => t,
            Ok(None) => {
                return Response::err(format!(
                    "task '{task_id}' changed state concurrently; try again"
                ));
            }
            Err(e) => return Response::err(e.to_string()),
        };
        if was_running {
            let mut runners = self.runners.lock().await;
            if let Some(handle) = runners.get_mut(&task.workspace)
                && handle.current_task.as_deref() == Some(task_id)
            {
                send_json(
                    &handle.sender,
                    &RunnerEvent::Stop {
                        task_id: task_id.to_string(),
                    },
                );
                handle.current_task = None;
            }
        }
        self.schedule().await;
        Response::ok(serde_json::to_value(&updated).unwrap())
    }

    fn existing_task(&self, task_id: &str) -> Result<Task, Response> {
        match self.store.get(task_id) {
            Ok(Some(task)) => Ok(task),
            Ok(None) => Err(task_not_found(task_id)),
            Err(e) => Err(Response::err(e.to_string())),
        }
    }

    fn running_task(&self, task_id: &str) -> Result<Task, Response> {
        let task = self.existing_task(task_id)?;
        if task.status == TaskStatus::Running {
            Ok(task)
        } else {
            Err(task_not_running(task_id, task.status))
        }
    }

    fn current_status(&self, task_id: &str) -> Result<TaskStatus, Response> {
        self.existing_task(task_id).map(|task| task.status)
    }

    fn get(&self, task_id: &str) -> Response {
        match self.existing_task(task_id) {
            Ok(task) => Response::ok(serde_json::to_value(&task).unwrap()),
            Err(response) => response,
        }
    }

    async fn delete(self: &Arc<Self>, task_id: &str) -> Response {
        match self.store.delete_unless(task_id, TaskStatus::Running) {
            Ok(true) => Response::ok_empty(),
            Ok(false) => match self.current_status(task_id) {
                Ok(_) => Response::err("cannot delete a running task"),
                Err(response) => response,
            },
            Err(e) => Response::err(e.to_string()),
        }
    }

    async fn edit(self: &Arc<Self>, task_id: &str, text: String) -> Response {
        match self
            .store
            .update_text_unless(task_id, &text, TaskStatus::Running)
        {
            Ok(Some(task)) => Response::ok(serde_json::to_value(&task).unwrap()),
            Ok(None) => match self.current_status(task_id) {
                Ok(_) => Response::err("cannot edit a running task"),
                Err(response) => response,
            },
            Err(e) => Response::err(e.to_string()),
        }
    }

    async fn move_task(
        self: &Arc<Self>,
        task_id: &str,
        direction: crate::protocol::MoveDirection,
    ) -> Response {
        match self.store.move_task(task_id, direction) {
            Ok(task) => Response::ok(serde_json::to_value(&task).unwrap()),
            Err(StoreError::NotFound(id)) => task_not_found(&id),
            Err(e) => Response::err(e.to_string()),
        }
    }

    fn transition_received(&self, task_id: &str, status: TaskStatus) -> Result<Task, Response> {
        match self
            .store
            .update_status_if(task_id, TaskStatus::Received, status)
        {
            Ok(Some(task)) => Ok(task),
            Ok(None) => Err(match self.current_status(task_id) {
                Ok(current) => Response::err(format!(
                    "task '{task_id}' is '{}', not 'received'",
                    current.as_str()
                )),
                Err(response) => response,
            }),
            Err(e) => Err(Response::err(e.to_string())),
        }
    }

    async fn accept(self: &Arc<Self>, task_id: &str) -> Response {
        match self.transition_received(task_id, TaskStatus::Queued) {
            Ok(task) => {
                self.schedule().await;
                Response::ok(serde_json::to_value(&task).unwrap())
            }
            Err(response) => response,
        }
    }

    async fn reject(self: &Arc<Self>, task_id: &str) -> Response {
        match self.transition_received(task_id, TaskStatus::Rejected) {
            Ok(task) => Response::ok(serde_json::to_value(&task).unwrap()),
            Err(response) => response,
        }
    }

    async fn retry(self: &Arc<Self>, task_id: &str) -> Response {
        const RETRYABLE: [TaskStatus; 3] = [
            TaskStatus::Interrupted,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ];
        match self.store.retry_if(task_id, &RETRYABLE) {
            Ok(Some(task)) => {
                self.schedule().await;
                Response::ok(serde_json::to_value(&task).unwrap())
            }
            Ok(None) => match self.current_status(task_id) {
                Ok(current) => Response::err(format!(
                    "task '{task_id}' is '{}'; only interrupted, failed or cancelled tasks can be retried",
                    current.as_str()
                )),
                Err(response) => response,
            },
            Err(e) => Response::err(e.to_string()),
        }
    }

    async fn status(&self) -> Response {
        let tasks = match self.store.list_all() {
            Ok(t) => t,
            Err(e) => return Response::err(e.to_string()),
        };
        let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
        for task in &tasks {
            *counts.entry(task.status.as_str()).or_insert(0) += 1;
        }
        let config = self.load_config().ok();
        let workspaces: Option<Vec<String>> = config
            .as_ref()
            .map(|c| c.workspaces.keys().cloned().collect());
        let tui_default_workspace = config.and_then(|c| c.tui.default_workspace);
        let runners = self.runners.lock().await;
        let runner_info: Vec<serde_json::Value> = runners
            .iter()
            .map(|(workspace, handle)| {
                serde_json::json!({
                    "workspace": workspace,
                    "attached": true,
                    "current_task": handle.current_task,
                })
            })
            .collect();
        Response::ok(serde_json::json!({
            "tasks_by_status": counts,
            "runners": runner_info,
            "workspaces": workspaces,
            "tui_default_workspace": tui_default_workspace,
            "zellij_session_name": self.zellij_session_name,
        }))
    }

    pub async fn agent_exited(
        self: &Arc<Self>,
        workspace: &str,
        attach_id: u64,
        task_id: &str,
        outcome: Outcome,
    ) -> Response {
        let mut runners = self.runners.lock().await;
        let Some(handle) = runners
            .get_mut(workspace)
            .filter(|h| h.attach_id == attach_id)
        else {
            return Response::err(format!(
                "this connection is not the attached runner for workspace '{workspace}'"
            ));
        };
        if handle.current_task.as_deref() != Some(task_id) {
            return Response::err(format!(
                "task '{task_id}' is not the current task of the runner for workspace '{workspace}'"
            ));
        }
        let task = match self.running_task(task_id) {
            Ok(task) => task,
            Err(response) => return response,
        };
        match outcome {
            Outcome::Done | Outcome::Failed => {
                let new_status = if outcome == Outcome::Done {
                    TaskStatus::Done
                } else {
                    TaskStatus::Failed
                };
                let updated =
                    match self
                        .store
                        .update_status_if(task_id, TaskStatus::Running, new_status)
                    {
                        Ok(Some(t)) => t,
                        Ok(None) => {
                            return Response::err(format!(
                                "task '{task_id}' is no longer 'running'"
                            ));
                        }
                        Err(e) => return Response::err(e.to_string()),
                    };
                handle.current_task = None;
                drop(runners);
                self.schedule().await;
                Response::ok(serde_json::to_value(&updated).unwrap())
            }
            Outcome::Restart => {
                let resolved = match self.resolve_for_restart(&task) {
                    Ok(resolved) => resolved,
                    Err(reason) => {
                        let marked = self.store.update_status_if(
                            task_id,
                            TaskStatus::Running,
                            TaskStatus::Failed,
                        );
                        handle.current_task = None;
                        drop(runners);
                        let message = match marked {
                            Ok(Some(_)) => format!(
                                "cannot restart task '{task_id}': {reason}; the task was marked failed"
                            ),
                            Ok(None) => format!(
                                "cannot restart task '{task_id}': {reason}; the task is no longer 'running'"
                            ),
                            Err(e) => format!(
                                "cannot restart task '{task_id}': {reason}; failed to mark it failed: {e}"
                            ),
                        };
                        eprintln!("[zelloom-core] {message}");
                        self.schedule().await;
                        return Response::err(message);
                    }
                };
                if let Err(e) = self.store.reset_turn(task_id) {
                    eprintln!("[zelloom-core] failed to reset the turn of task {task_id}: {e}");
                }
                let task = match self.store.get(task_id) {
                    Ok(Some(t)) => t,
                    _ => task,
                };
                send_json(
                    &handle.sender,
                    &RunnerEvent::Start {
                        task: task.clone(),
                        agent: resolved,
                    },
                );
                Response::ok(serde_json::to_value(&task).unwrap())
            }
        }
    }

    fn resolve_for_restart(&self, task: &Task) -> Result<ResolvedAgent, String> {
        let config = self.load_config()?;
        let ws = config
            .workspaces
            .get(&task.workspace)
            .ok_or_else(|| format!("workspace '{}' is not registered", task.workspace))?;
        let agent_name = config
            .resolve_agent(task.agent.as_deref(), &task.workspace)
            .ok_or_else(|| "no agent could be resolved".to_string())?;
        let agent_cfg = config
            .agents
            .get(&agent_name)
            .ok_or_else(|| format!("agent '{agent_name}' is not defined"))?;
        Ok(self.resolved_agent(agent_cfg, ws, task))
    }

    pub async fn runner_attached(
        self: &Arc<Self>,
        workspace: String,
        sender: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Option<u64> {
        let attach_id = {
            let mut runners = self.runners.lock().await;
            if runners.contains_key(&workspace) {
                send_json(
                    &sender,
                    &Response::err(format!(
                        "a runner for workspace '{workspace}' is already attached"
                    )),
                );
                return None;
            }
            let attach_id = self.next_attach_id.fetch_add(1, Ordering::Relaxed);
            send_json(&sender, &Response::ok_empty());
            runners.insert(
                workspace.clone(),
                RunnerHandle {
                    attach_id,
                    sender,
                    current_task: None,
                },
            );
            attach_id
        };

        let running_task_for_ws = match self.store.list_by_status(TaskStatus::Running) {
            Ok(tasks) => tasks.into_iter().find(|t| t.workspace == workspace),
            Err(e) => {
                eprintln!("[zelloom-core] failed to inspect running tasks on attach: {e}");
                None
            }
        };

        if let Some(task) = running_task_for_ws {
            let config = match self.load_config() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!(
                        "[zelloom-core] failed to load config while reattaching runner for '{workspace}': {e}"
                    );
                    return Some(attach_id);
                }
            };
            if let Some(ws) = config.workspaces.get(&workspace)
                && let Some(agent_name) = config.resolve_agent(task.agent.as_deref(), &workspace)
                && let Some(agent_cfg) = config.agents.get(&agent_name)
            {
                let resolved = self.resolved_agent(agent_cfg, ws, &task);
                let mut runners = self.runners.lock().await;
                if let Some(handle) = runners
                    .get_mut(&workspace)
                    .filter(|h| h.attach_id == attach_id && h.current_task.is_none())
                {
                    handle.current_task = Some(task.id.clone());
                    send_json(
                        &handle.sender,
                        &RunnerEvent::Start {
                            task,
                            agent: resolved,
                        },
                    );
                }
            }
        } else {
            self.schedule().await;
        }
        Some(attach_id)
    }

    pub async fn runner_disconnected(self: &Arc<Self>, workspace: &str, attach_id: u64) {
        let interrupted_task = {
            let mut runners = self.runners.lock().await;
            if runners.get(workspace).map(|h| h.attach_id) != Some(attach_id) {
                return;
            }
            self.pending_inputs
                .lock()
                .unwrap()
                .retain(|_, p| p.attach_id != attach_id);
            runners.remove(workspace).and_then(|h| h.current_task)
        };
        if let Some(task_id) = interrupted_task
            && let Err(e) =
                self.store
                    .update_status_if(&task_id, TaskStatus::Running, TaskStatus::Interrupted)
        {
            eprintln!(
                "[zelloom-core] failed to mark task {task_id} interrupted after runner disconnect: {e}"
            );
        }
        self.schedule().await;
    }

    // Boxed to break the schedule -> start_task -> (spawned task) -> schedule type-level recursion; a plain `async fn` can't compile since the compiler can't resolve the cyclic opaque Future type.
    pub fn schedule<'a>(self: &'a Arc<Self>) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let _guard = self.scheduling_lock.lock().await;
            if *self.shutdown.borrow() {
                return;
            }
            loop {
                let config = match self.load_config() {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("[zelloom-core] failed to load config while scheduling: {e}");
                        return;
                    }
                };
                let all = match self.store.list_all() {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("[zelloom-core] failed to list tasks while scheduling: {e}");
                        return;
                    }
                };
                let running_count = all
                    .iter()
                    .filter(|t| t.status == TaskStatus::Running)
                    .count() as u32;
                if running_count >= config.max_parallel() {
                    return;
                }
                let busy_workspaces: HashSet<&str> = all
                    .iter()
                    .filter(|t| t.status == TaskStatus::Running)
                    .map(|t| t.workspace.as_str())
                    .collect();
                let next_task = all
                    .iter()
                    .find(|t| {
                        t.status == TaskStatus::Queued
                            && !busy_workspaces.contains(t.workspace.as_str())
                    })
                    .cloned();
                let Some(task) = next_task else {
                    return;
                };
                if !self.start_task(&config, task).await {
                    return;
                }
            }
        })
    }

    async fn start_task(self: &Arc<Self>, config: &Config, task: Task) -> bool {
        let Some(ws) = config.workspaces.get(&task.workspace) else {
            eprintln!(
                "[zelloom-core] workspace '{}' is not registered; failing task {}",
                task.workspace, task.id
            );
            return self.fail_queued(&task.id);
        };
        let Some(agent_name) = config.resolve_agent(task.agent.as_deref(), &task.workspace) else {
            eprintln!(
                "[zelloom-core] no agent resolved for task {} in workspace '{}'; failing task",
                task.id, task.workspace
            );
            return self.fail_queued(&task.id);
        };
        let Some(agent_cfg) = config.agents.get(&agent_name) else {
            eprintln!(
                "[zelloom-core] agent '{agent_name}' is not defined; failing task {}",
                task.id
            );
            return self.fail_queued(&task.id);
        };

        let resolved = self.resolved_agent(agent_cfg, ws, &task);

        let running_task =
            match self
                .store
                .update_status_if(&task.id, TaskStatus::Queued, TaskStatus::Running)
            {
                Ok(Some(t)) => t,
                Ok(None) => return true,
                Err(e) => {
                    eprintln!(
                        "[zelloom-core] failed to mark task {} running: {e}",
                        task.id
                    );
                    return false;
                }
            };

        {
            let mut runners = self.runners.lock().await;
            if let Some(handle) = runners.get_mut(&task.workspace)
                && handle.current_task.is_none()
            {
                handle.current_task = Some(task.id.clone());
                send_json(
                    &handle.sender,
                    &RunnerEvent::Start {
                        task: running_task,
                        agent: resolved,
                    },
                );
                return true;
            }
        }

        let generation = {
            let mut generations = self.launch_generation.lock().await;
            let slot = generations.entry(task.id.clone()).or_insert(0);
            *slot += 1;
            *slot
        };

        let launcher = Arc::clone(&self.launcher);
        let workspace_id = task.workspace.clone();
        let workspace_path = ws.path.clone();
        let launched =
            tokio::task::spawn_blocking(move || launcher.launch(&workspace_id, &workspace_path))
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("launcher task failed: {e}")));
        if let Err(e) = launched {
            eprintln!(
                "[zelloom-core] failed to launch tab for workspace '{}': {e}",
                task.workspace
            );
            self.interrupt_if_still_waiting(&task.id, &task.workspace, generation)
                .await;
            return true;
        }

        let scheduler = Arc::clone(self);
        let workspace = task.workspace.clone();
        let task_id = task.id.clone();
        let timeout = self.attach_timeout;
        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            if scheduler
                .interrupt_if_still_waiting(&task_id, &workspace, generation)
                .await
            {
                scheduler.schedule().await;
            }
        });
        true
    }

    fn fail_queued(&self, task_id: &str) -> bool {
        match self
            .store
            .update_status_if(task_id, TaskStatus::Queued, TaskStatus::Failed)
        {
            Ok(_) => true,
            Err(e) => {
                eprintln!("[zelloom-core] failed to mark task {task_id} failed: {e}");
                false
            }
        }
    }

    // Guarded to only transition out of `Running`: checking the runner's live `current_task` alone can't tell "never attached" apart from "attached and already finished" through another path.
    async fn interrupt_if_still_waiting(
        self: &Arc<Self>,
        task_id: &str,
        workspace: &str,
        generation: u64,
    ) -> bool {
        let is_current_attempt = {
            let generations = self.launch_generation.lock().await;
            generations.get(task_id).copied() == Some(generation)
        };
        if !is_current_attempt {
            return false;
        }
        let assigned = {
            let runners = self.runners.lock().await;
            runners
                .get(workspace)
                .map(|h| h.current_task.as_deref() == Some(task_id))
                .unwrap_or(false)
        };
        if assigned {
            return false;
        }
        match self
            .store
            .update_status_if(task_id, TaskStatus::Running, TaskStatus::Interrupted)
        {
            Ok(Some(_)) => {
                eprintln!(
                    "[zelloom-core] timed out waiting for a runner to attach for workspace '{workspace}'; \
                     marking task {task_id} interrupted"
                );
                true
            }
            Ok(None) => false,
            Err(e) => {
                eprintln!("[zelloom-core] failed to mark task {task_id} interrupted: {e}");
                false
            }
        }
    }
}
