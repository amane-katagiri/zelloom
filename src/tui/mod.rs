pub mod app;
mod ui;

use std::io::Stdout;
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::protocol::{Request, Source, Task};
use app::{Action, App};

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

type Term = Terminal<CrosstermBackend<Stdout>>;

pub fn run() -> anyhow::Result<()> {
    install_panic_hook();
    let mut terminal = setup_terminal()?;
    let socket_path = crate::paths::socket_path();
    let mut app = App::new();
    let result = run_app(&mut terminal, &mut app, &socket_path);
    restore_terminal(&mut terminal)?;
    result
}

fn setup_terminal() -> anyhow::Result<Term> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Term) -> anyhow::Result<()> {
    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = crossterm::execute!(std::io::stdout(), LeaveAlternateScreen);
        original(info);
    }));
}

fn run_app(terminal: &mut Term, app: &mut App, socket_path: &Path) -> anyhow::Result<()> {
    let mut last_poll = Instant::now() - POLL_INTERVAL;
    loop {
        if last_poll.elapsed() >= POLL_INTERVAL {
            refresh(socket_path, app);
            last_poll = Instant::now();
        }

        terminal.draw(|f| ui::render(f, app))?;

        let timeout = POLL_INTERVAL.saturating_sub(last_poll.elapsed());
        if event::poll(timeout)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            let action = app.handle_key(key);
            match action {
                Action::Quit => return Ok(()),
                Action::StopCoreAndQuit { force } => {
                    app.last_message = Some("stopping core...".to_string());
                    terminal.draw(|f| ui::render(f, app))?;
                    match crate::cli::stop_core(socket_path, force, STOP_TIMEOUT) {
                        Ok(_) => return Ok(()),
                        Err(e) => {
                            app.last_message = Some(format!("error: {e}").replace('\n', "  "));
                        }
                    }
                }
                action => execute_action(socket_path, app, action),
            }
        }
    }
}

fn refresh(socket_path: &Path, app: &mut App) {
    match crate::client::call(socket_path, &Request::List) {
        Ok(value) => match serde_json::from_value::<Vec<Task>>(value) {
            Ok(tasks) => {
                app.core_reachable = true;
                app.set_tasks(tasks);
                if let Some(ids) = fetch_workspace_ids(socket_path) {
                    app.workspace_ids = ids;
                }
            }
            Err(e) => {
                app.core_reachable = false;
                app.last_message = Some(format!("bad response from core: {e}"));
            }
        },
        Err(_) => {
            app.core_reachable = false;
        }
    }
}

fn fetch_workspace_ids(socket_path: &Path) -> Option<Vec<String>> {
    let status = crate::client::call(socket_path, &Request::Status).ok()?;
    serde_json::from_value(status.get("workspaces")?.clone()).ok()
}

fn execute_action(socket_path: &Path, app: &mut App, action: Action) {
    let request = match action {
        Action::None | Action::Quit | Action::StopCoreAndQuit { .. } => return,
        Action::Enqueue { workspace, text } => Request::Enqueue {
            text,
            workspace,
            agent: None,
            source: Source::cli(),
            reply_to: None,
            metadata: serde_json::json!({}),
        },
        Action::Complete(task_id) => Request::Complete { task_id },
        Action::Cancel(task_id) => Request::Cancel { task_id },
        Action::Retry(task_id) => Request::Retry { task_id },
        Action::Delete(task_id) => Request::Delete { task_id },
        Action::Edit { task_id, text } => Request::Edit { task_id, text },
        Action::Move { task_id, direction } => Request::Move { task_id, direction },
        Action::Accept(task_id) => Request::Accept { task_id },
        Action::Reject(task_id) => Request::Reject { task_id },
        Action::FocusWorkspaceTab(workspace) => {
            match focus_zellij_tab(&workspace) {
                Ok(()) => app.last_message = Some(format!("focused workspace '{workspace}'")),
                Err(e) => app.last_message = Some(e),
            }
            return;
        }
    };

    match crate::client::call(socket_path, &request) {
        Ok(_) => {
            app.last_message = None;
            refresh(socket_path, app);
        }
        Err(e) => {
            app.last_message = Some(format!("error: {e}"));
        }
    }
}

fn focus_zellij_tab(workspace: &str) -> Result<(), String> {
    let session = std::env::var("ZELLIJ_SESSION_NAME").map_err(|_| {
        "not running inside a Zellij session (ZELLIJ_SESSION_NAME not set)".to_string()
    })?;
    let bin = std::env::var("ZELLOOM_ZELLIJ").unwrap_or_else(|_| "zellij".to_string());
    let status = std::process::Command::new(&bin)
        .args(["--session", &session, "action", "go-to-tab-name", workspace])
        .status()
        .map_err(|e| format!("failed to run '{bin}': {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "'{bin} action go-to-tab-name {workspace}' exited with {status}"
        ))
    }
}
