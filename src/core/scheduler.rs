use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{Mutex, watch};

use crate::config::{self, AgentConfig, Config, INSTRUCTION_PLACEHOLDER, WorkspaceConfig};
use crate::protocol::{Outcome, ResolvedAgent, Response, RunnerEvent, Source, Task, TaskStatus};
use crate::store::{NewTask, Store, StoreError};

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

pub fn attach_timeout_from_env() -> Duration {
    std::env::var("ZELLOOM_ATTACH_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_ATTACH_TIMEOUT)
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
    pub zellij_session_name: Option<String>,
    runners: Mutex<HashMap<String, RunnerHandle>>,
    next_attach_id: AtomicU64,
    // Per-task counter bumped on each launch attempt so a stale attach-timeout captured at an earlier generation becomes a no-op instead of interrupting a newer attempt.
    launch_generation: Mutex<HashMap<String, u64>>,
    // Held for the whole body of `schedule()` so concurrent callers can't dispatch off the same stale "task/workspace free" snapshot.
    scheduling_lock: Mutex<()>,
    shutdown: watch::Sender<bool>,
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

fn loom_exe_path() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "loom".to_string())
}

fn build_instruction(loom_exe: &str) -> String {
    format!(
        "You are running inside zelloom.\n\
         \n\
         Each session corresponds to exactly one task.\n\
         \n\
         Do not end the task merely because you believe the work is complete.\n\
         \n\
         When the user explicitly confirms that the current task is finished, run:\n\
         \n\
         \x20   {loom_exe} done\n"
    )
}

fn build_argv(agent_cfg: &AgentConfig, instruction: &str, text: &str) -> Vec<String> {
    let mut argv = agent_cfg.command.clone();
    if agent_cfg.oneshot {
        argv.push(text.to_string());
        return argv;
    }
    match &agent_cfg.instruction_args {
        Some(args) => {
            argv.extend(
                args.iter()
                    .map(|arg| arg.replace(INSTRUCTION_PLACEHOLDER, instruction)),
            );
            argv.push(text.to_string());
        }
        None => {
            argv.push(format!("{instruction}\n{text}"));
        }
    }
    argv
}

fn build_env(
    agent_cfg: &AgentConfig,
    ws: &WorkspaceConfig,
    task: &Task,
    socket_path: &str,
) -> BTreeMap<String, String> {
    let mut env = agent_cfg.env.clone();
    env.extend(ws.env.clone());
    env.insert("ZELLOOM_TASK_ID".to_string(), task.id.clone());
    env.insert("ZELLOOM_WORKSPACE".to_string(), task.workspace.clone());
    env.insert("ZELLOOM_SOCKET".to_string(), socket_path.to_string());
    env
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
    ) -> Scheduler {
        Scheduler {
            store,
            config_path,
            socket_path,
            launcher,
            attach_timeout,
            zellij_session_name,
            runners: Mutex::new(HashMap::new()),
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
        let instruction = build_instruction(&loom_exe);
        let argv = build_argv(agent_cfg, &instruction, &task.text);
        let env = build_env(agent_cfg, ws, task, &self.socket_path.to_string_lossy());
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
        let task = match self.store.get(task_id) {
            Ok(Some(t)) => t,
            Ok(None) => return Response::err(format!("task '{task_id}' not found")),
            Err(e) => return Response::err(e.to_string()),
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

    fn current_status(&self, task_id: &str) -> Result<TaskStatus, Response> {
        match self.store.get(task_id) {
            Ok(Some(task)) => Ok(task.status),
            Ok(None) => Err(Response::err(format!("task '{task_id}' not found"))),
            Err(e) => Err(Response::err(e.to_string())),
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
            Err(StoreError::NotFound(id)) => Response::err(format!("task '{id}' not found")),
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
        let task = match self.store.get(task_id) {
            Ok(Some(t)) => t,
            Ok(None) => return Response::err(format!("task '{task_id}' not found")),
            Err(e) => return Response::err(e.to_string()),
        };
        if task.status != TaskStatus::Running {
            return Response::err(format!(
                "task '{task_id}' is '{}', not 'running'",
                task.status.as_str()
            ));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> Task {
        Task {
            id: "t1".to_string(),
            text: "text".to_string(),
            status: TaskStatus::Running,
            workspace: "w".to_string(),
            agent: None,
            source: Source::cli(),
            reply_to: None,
            metadata: serde_json::json!({}),
            position: 1,
            created_at: "2026-01-01T00:00:00+09:00".to_string(),
            updated_at: "2026-01-01T00:00:00+09:00".to_string(),
        }
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn env_merge_order_is_agent_then_workspace_then_zelloom() {
        let agent = AgentConfig {
            command: vec!["a".into()],
            instruction_args: None,
            shell: true,
            oneshot: false,
            env: map(&[
                ("ONLY_AGENT", "agent"),
                ("BOTH", "agent"),
                ("ZELLOOM_TASK_ID", "agent"),
            ]),
        };
        let ws = WorkspaceConfig {
            path: "/tmp/w".into(),
            agent: None,
            max_parallel: None,
            env: map(&[("ONLY_WS", "ws"), ("BOTH", "ws"), ("ZELLOOM_SOCKET", "ws")]),
        };
        let env = build_env(&agent, &ws, &task(), "/tmp/s.sock");
        assert_eq!(
            env,
            map(&[
                ("BOTH", "ws"),
                ("ONLY_AGENT", "agent"),
                ("ONLY_WS", "ws"),
                ("ZELLOOM_SOCKET", "/tmp/s.sock"),
                ("ZELLOOM_TASK_ID", "t1"),
                ("ZELLOOM_WORKSPACE", "w"),
            ])
        );
    }

    #[test]
    fn instruction_args_embed_instruction_before_task_text() {
        let agent = AgentConfig {
            command: vec!["codex".into()],
            instruction_args: Some(vec![
                "-c".into(),
                "developer_instructions={instruction}".into(),
            ]),
            shell: true,
            oneshot: false,
            env: BTreeMap::new(),
        };
        assert_eq!(
            build_argv(&agent, "be nice\n", "fix it"),
            vec![
                "codex".to_string(),
                "-c".to_string(),
                "developer_instructions=be nice\n".to_string(),
                "fix it".to_string(),
            ]
        );
    }

    #[test]
    fn oneshot_passes_only_task_text() {
        let agent = AgentConfig {
            command: vec!["claude".into(), "-p".into()],
            instruction_args: None,
            shell: true,
            oneshot: true,
            env: BTreeMap::new(),
        };
        assert_eq!(
            build_argv(&agent, "be nice\n", "fix it"),
            vec!["claude".to_string(), "-p".to_string(), "fix it".to_string()]
        );
    }
}
