use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};

use crate::core::CoreOptions;
use crate::launcher::ZellijLauncher;
use crate::protocol::{Request, Source, Task};

#[derive(Parser)]
#[command(
    name = "loom",
    version,
    about = "zelloom: one task, one interactive session",
    after_help = "Without a subcommand, loom starts core if needed (only possible inside a Zellij session) and runs the management TUI in the current terminal."
)]
pub struct Cli {
    /// Unix socket of loom core (overrides ZELLOOM_SOCKET)
    #[arg(long, global = true, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the core server in the foreground
    #[command(hide = true)]
    Core,
    /// Run the runner for a workspace (normally started by core in a Zellij tab)
    #[command(hide = true)]
    Runner {
        /// Workspace id to serve
        workspace: String,
    },
    /// Add a task to the queue
    Add {
        /// Workspace id (auto-detected from the current directory if omitted)
        #[arg(short = 'w', long = "workspace")]
        workspace: Option<String>,
        /// Agent to run this task with (overrides the workspace and default agent)
        #[arg(long = "agent")]
        agent: Option<String>,
        /// Task text (read from stdin if omitted)
        text: Option<String>,
    },
    /// Mark a task as done
    Done {
        /// Task id (defaults to ZELLOOM_TASK_ID)
        task_id: Option<String>,
    },
    /// List all tasks
    List,
    /// Stop loom core (refuses while tasks are running unless --force)
    Stop {
        /// Stop even if tasks are running; they become interrupted and their agents keep running until they exit
        #[arg(long)]
        force: bool,
    },
    /// Create the config file with a starter template (refuses to overwrite)
    Init,
    /// Manage registered workspaces
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    /// Manage the config file
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Open the config file in $VISUAL or $EDITOR (falls back to vi) and validate it afterwards
    Edit,
}

#[derive(Subcommand)]
pub enum WorkspaceCommand {
    /// Register a new workspace (fails if the id is already registered)
    Add {
        /// Workspace id, also used as the Zellij tab name (defaults to the directory name of the workspace path)
        id: Option<String>,
        /// Default agent for this workspace (must be defined in [agents])
        #[arg(long)]
        agent: Option<String>,
        /// Workspace directory (defaults to the git toplevel of the current directory, or the current directory itself)
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// List registered workspaces
    List,
}

pub async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    if let Some(socket) = cli.socket {
        crate::paths::set_socket_override(socket);
    }
    match cli.command {
        None => run_bare_loom().await,
        Some(Command::Core) => run_core().await,
        Some(Command::Runner { workspace }) => {
            tokio::task::spawn_blocking(move || crate::runner::run(workspace)).await?
        }
        Some(Command::Add {
            workspace,
            agent,
            text,
        }) => cmd_add(workspace, agent, text),
        Some(Command::Done { task_id }) => cmd_done(task_id),
        Some(Command::List) => cmd_list(),
        Some(Command::Stop { force }) => cmd_stop(force),
        Some(Command::Init) => cmd_init(),
        Some(Command::Workspace { command }) => match command {
            WorkspaceCommand::Add { id, agent, path } => cmd_workspace_add(id, agent, path),
            WorkspaceCommand::List => cmd_workspace_list(),
        },
        Some(Command::Config { command }) => match command {
            ConfigCommand::Edit => cmd_config_edit(),
        },
    }
}

async fn run_core() -> anyhow::Result<()> {
    let socket_path = crate::paths::socket_path();
    let session_name = std::env::var("ZELLIJ_SESSION_NAME").ok();
    let options = CoreOptions {
        socket_path: socket_path.clone(),
        config_path: crate::paths::config_path(),
        db_path: crate::paths::state_db_path(),
        launcher: Arc::new(ZellijLauncher::new(session_name, socket_path)),
    };
    crate::core::run(options).await
}

async fn run_bare_loom() -> anyhow::Result<()> {
    ensure_core(&crate::paths::socket_path())?;
    tokio::task::spawn_blocking(crate::tui::run).await?
}

pub fn ensure_core(socket_path: &std::path::Path) -> anyhow::Result<()> {
    if core_is_listening(socket_path) {
        return Ok(());
    }
    if std::env::var_os("ZELLIJ_SESSION_NAME").is_none() {
        anyhow::bail!(
            "loom core is not running on {}; it must be started from inside a Zellij session",
            socket_path.display()
        );
    }
    let mut child = spawn_detached_core(socket_path)?;
    wait_for_core_socket(&mut child, socket_path, std::time::Duration::from_secs(5))
}

fn core_is_listening(socket_path: &std::path::Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket_path).is_ok()
}

fn spawn_detached_core(socket_path: &std::path::Path) -> anyhow::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;

    let exe = std::env::current_exe()?;
    let log_path = crate::paths::log_dir().join("core.log");
    crate::paths::ensure_parent_dir(&log_path)?;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_file_err = log_file.try_clone()?;

    let mut command = std::process::Command::new(exe);
    command
        .arg("--socket")
        .arg(socket_path)
        .arg("core")
        .stdin(std::process::Stdio::null())
        .stdout(log_file)
        .stderr(log_file_err);
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setsid().map_err(std::io::Error::from)?;
            Ok(())
        });
    }
    Ok(command.spawn()?)
}

fn wait_for_core_socket(
    child: &mut std::process::Child,
    socket_path: &std::path::Path,
    timeout: std::time::Duration,
) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if core_is_listening(socket_path) {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            anyhow::bail!(
                "loom core exited during startup ({status}); see {}",
                crate::paths::log_dir().join("core.log").display()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    anyhow::bail!(
        "timed out waiting for loom core to start listening on {}",
        socket_path.display()
    );
}

fn cmd_add(
    workspace: Option<String>,
    agent: Option<String>,
    text: Option<String>,
) -> anyhow::Result<()> {
    let text = match text {
        Some(t) => t,
        None => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf.trim_end().to_string()
        }
    };
    if text.is_empty() {
        anyhow::bail!("task text is empty");
    }

    let config_path = crate::paths::config_path();
    let config = crate::config::load(&config_path)?;

    let workspace = match workspace {
        Some(w) => w,
        None => {
            let cwd = std::env::current_dir()?;
            match crate::config::detect_workspace(&config, &cwd) {
                Some(w) => w,
                None if !config_path.exists() => return Err(missing_config_error(&config_path)),
                None => {
                    let suggested = crate::config::workspace_id_from_path(
                        &crate::config::resolve_workspace_path(None, &cwd),
                    )
                    .unwrap_or_else(|_| "<ID>".to_string());
                    anyhow::bail!(
                        "could not auto-detect a workspace from the current directory.\n  register it:  loom workspace add {suggested}\n  or specify:   loom add -w <workspace> ..."
                    );
                }
            }
        }
    };

    if !config.workspaces.contains_key(&workspace) {
        if !config_path.exists() {
            return Err(missing_config_error(&config_path));
        }
        anyhow::bail!(
            "workspace '{workspace}' is not registered; run `loom workspace add {workspace}` first"
        );
    }

    let socket_path = crate::paths::socket_path();
    let request = Request::Enqueue {
        text,
        workspace,
        agent,
        source: Source::cli(),
        reply_to: None,
        metadata: serde_json::json!({}),
    };
    let data = crate::client::call(&socket_path, &request)?;
    let task: Task = serde_json::from_value(data)?;
    println!(
        "added task {} [{}] in workspace '{}'",
        task.id,
        task.status.as_str(),
        task.workspace
    );
    Ok(())
}

fn missing_config_error(config_path: &std::path::Path) -> anyhow::Error {
    anyhow::anyhow!(
        "config file {} does not exist.\n  create it:    loom init\n  then:         cd <project> && loom workspace add",
        config_path.display()
    )
}

fn cmd_done(task_id: Option<String>) -> anyhow::Result<()> {
    let task_id = task_id
        .or_else(|| std::env::var("ZELLOOM_TASK_ID").ok())
        .ok_or_else(|| anyhow::anyhow!("no task id given and ZELLOOM_TASK_ID is not set"))?;

    let socket_path = crate::paths::socket_path();
    crate::client::call(
        &socket_path,
        &Request::Complete {
            task_id: task_id.clone(),
        },
    )?;
    println!("task {task_id} marked done");
    Ok(())
}

fn cmd_list() -> anyhow::Result<()> {
    let socket_path = crate::paths::socket_path();
    let data = crate::client::call(&socket_path, &Request::List)?;
    let tasks: Vec<Task> = serde_json::from_value(data)?;

    if tasks.is_empty() {
        println!("(no tasks)");
        return Ok(());
    }

    println!(
        "{:<28}  {:<11}  {:<14}  {:<8}  TEXT",
        "ID", "STATUS", "WORKSPACE", "AGENT"
    );
    for task in tasks {
        println!(
            "{:<28}  {:<11}  {:<14}  {:<8}  {}",
            task.id,
            task.status.as_str(),
            task.workspace,
            task.agent.unwrap_or_else(|| "-".to_string()),
            task.text,
        );
    }
    Ok(())
}

fn cmd_stop(force: bool) -> anyhow::Result<()> {
    let socket_path = crate::paths::socket_path();
    let message = stop_core(&socket_path, force, std::time::Duration::from_secs(5))?;
    println!("{message}");
    Ok(())
}

pub fn stop_core(
    socket_path: &std::path::Path,
    force: bool,
    timeout: std::time::Duration,
) -> anyhow::Result<&'static str> {
    let mut client = match crate::client::Client::connect(socket_path) {
        Ok(client) => client,
        Err(_) => return Ok("core is not running"),
    };
    let response = client.call(&Request::Shutdown { force })?;
    if !response.ok {
        let running: Vec<Task> = response
            .data
            .as_ref()
            .and_then(|d| d.get("running"))
            .and_then(|r| serde_json::from_value(r.clone()).ok())
            .unwrap_or_default();
        let mut message = format!(
            "refusing to stop core: {}",
            response
                .error
                .unwrap_or_else(|| "unknown error".to_string())
        );
        for task in &running {
            message.push_str(&format!(
                "\n  [{}] {}  ({})",
                task.workspace, task.text, task.id
            ));
        }
        if !running.is_empty() {
            message.push_str(
                "\nuse `loom stop --force` to stop anyway (running tasks become interrupted)",
            );
        }
        anyhow::bail!(message);
    }
    drop(client);

    let start = std::time::Instant::now();
    while core_is_listening(socket_path) {
        if start.elapsed() >= timeout {
            anyhow::bail!(
                "loom core did not stop within {}s (socket {})",
                timeout.as_secs(),
                socket_path.display()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok("core stopped")
}

fn cmd_init() -> anyhow::Result<()> {
    let config_path = crate::paths::config_path();
    crate::config::init_config(&config_path)?;
    println!("wrote {}", config_path.display());
    println!("next: cd <project> && loom workspace add");
    Ok(())
}

fn cmd_config_edit() -> anyhow::Result<()> {
    let config_path = crate::paths::config_path();
    if !config_path.exists() {
        anyhow::bail!(
            "config file {} does not exist; create it with `loom init`",
            config_path.display()
        );
    }
    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "vi".to_string());
    // Run through sh so that editors with arguments such as "code --wait" work, as git does.
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg(&editor)
        .arg(&config_path)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run editor {editor:?}: {e}"))?;
    if !status.success() {
        anyhow::bail!("editor {editor:?} exited with {status}");
    }
    crate::config::load(&config_path)?;
    println!("{} is valid", config_path.display());
    Ok(())
}

fn cmd_workspace_add(
    id: Option<String>,
    agent: Option<String>,
    path: Option<PathBuf>,
) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let path = crate::config::resolve_workspace_path(path.as_deref(), &cwd);
    let id = match id {
        Some(id) => id,
        None => crate::config::workspace_id_from_path(&path)?,
    };

    let config_path = crate::paths::config_path();
    crate::config::add_workspace(&config_path, &id, &path, agent.as_deref())?;
    println!(
        "registered workspace {id} -> {} (agent: {})",
        path.display(),
        agent.as_deref().unwrap_or("default")
    );
    Ok(())
}

fn cmd_workspace_list() -> anyhow::Result<()> {
    let config = crate::config::load(&crate::paths::config_path())?;
    if config.workspaces.is_empty() {
        println!("(no workspaces registered)");
        return Ok(());
    }
    for (id, ws) in &config.workspaces {
        println!(
            "{:<16}  {:<8}  {}",
            id,
            ws.agent.as_deref().unwrap_or("-"),
            ws.path.display()
        );
    }
    Ok(())
}
