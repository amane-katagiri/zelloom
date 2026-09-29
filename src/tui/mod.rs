pub mod app;
mod ui;

use std::io::Stdout;
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind};
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
    crossterm::execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Term) -> anyhow::Result<()> {
    disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
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
        if !event::poll(timeout)? {
            continue;
        }
        let action = match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => app.handle_key(key),
            Event::Paste(text) => {
                app.handle_paste(&text);
                Action::None
            }
            _ => Action::None,
        };
        let action = match action {
            Action::OpenEditor { initial } => {
                let result = edit_in_terminal(terminal, &initial);
                app.finish_editor(result)
            }
            action => action,
        };
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

fn edit_in_terminal(terminal: &mut Term, initial: &str) -> anyhow::Result<Option<String>> {
    restore_terminal(terminal)?;
    let result = crate::editor::compose(initial, |mut command| command.status());
    enable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableBracketedPaste
    )?;
    terminal.clear()?;
    result
}

fn refresh(socket_path: &Path, app: &mut App) {
    match crate::client::call(socket_path, &Request::List) {
        Ok(value) => match serde_json::from_value::<Vec<Task>>(value) {
            Ok(tasks) => {
                app.core_reachable = true;
                app.set_tasks(tasks);
                if let Some(status) = fetch_status(socket_path) {
                    if let Some(ids) = status.workspaces {
                        app.workspace_ids = ids;
                    }
                    app.default_workspace = status.tui_default_workspace;
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

#[derive(serde::Deserialize)]
struct StatusView {
    workspaces: Option<Vec<String>>,
    tui_default_workspace: Option<String>,
}

fn fetch_status(socket_path: &Path) -> Option<StatusView> {
    let status = crate::client::call(socket_path, &Request::Status).ok()?;
    serde_json::from_value(status).ok()
}

fn execute_action(socket_path: &Path, app: &mut App, action: Action) {
    let request = match action {
        Action::None
        | Action::Quit
        | Action::StopCoreAndQuit { .. }
        | Action::OpenEditor { .. } => return,
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
