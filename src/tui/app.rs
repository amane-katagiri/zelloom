use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::protocol::{MoveDirection, Task, TaskStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputMode {
    Add,
    Edit { task_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Input {
        mode: InputMode,
        buffer: Vec<char>,
        cursor: usize,
    },
    ConfirmQuit {
        running: usize,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    None,
    Quit,
    StopCoreAndQuit {
        force: bool,
    },
    Enqueue {
        workspace: String,
        text: String,
    },
    Complete(String),
    Cancel(String),
    Retry(String),
    Delete(String),
    Edit {
        task_id: String,
        text: String,
    },
    Move {
        task_id: String,
        direction: MoveDirection,
    },
    Accept(String),
    Reject(String),
    FocusWorkspaceTab(String),
}

pub struct App {
    pub tasks: Vec<Task>,
    pub selected: Option<usize>,
    pub mode: Mode,
    pub pending_delete: bool,
    pub last_message: Option<String>,
    pub core_reachable: bool,
    pub workspace_ids: Vec<String>,
}

pub const SECTION_STATUSES: [&[TaskStatus]; 4] = [
    &[TaskStatus::Running],
    &[TaskStatus::Queued],
    &[TaskStatus::Received],
    &[TaskStatus::Interrupted, TaskStatus::Failed],
];

const RETRYABLE: &[TaskStatus] = &[TaskStatus::Interrupted, TaskStatus::Failed];
const MARK_DONE: &[TaskStatus] = &[TaskStatus::Interrupted];
const CANCELLABLE: &[TaskStatus] = &[
    TaskStatus::Queued,
    TaskStatus::Received,
    TaskStatus::Interrupted,
];
const DELETABLE: &[TaskStatus] = &[
    TaskStatus::Queued,
    TaskStatus::Received,
    TaskStatus::Interrupted,
    TaskStatus::Failed,
];

impl Default for App {
    fn default() -> App {
        App::new()
    }
}

impl App {
    pub fn new() -> App {
        App {
            tasks: Vec::new(),
            selected: None,
            mode: Mode::Normal,
            pending_delete: false,
            last_message: None,
            core_reachable: true,
            workspace_ids: Vec::new(),
        }
    }

    pub fn selectable_tasks(&self) -> Vec<&Task> {
        SECTION_STATUSES
            .iter()
            .flat_map(|statuses| self.section_tasks(statuses))
            .collect()
    }

    pub fn section_tasks(&self, statuses: &[TaskStatus]) -> Vec<&Task> {
        self.tasks
            .iter()
            .filter(|t| statuses.contains(&t.status))
            .collect()
    }

    pub fn finished_counts(&self) -> (usize, usize) {
        let count = |status| self.tasks.iter().filter(|t| t.status == status).count();
        (count(TaskStatus::Done), count(TaskStatus::Cancelled))
    }

    pub fn selected_task(&self) -> Option<&Task> {
        let list = self.selectable_tasks();
        self.selected.and_then(|i| list.get(i).copied())
    }

    pub fn set_tasks(&mut self, tasks: Vec<Task>) {
        let selected_id = self.selected_task().map(|t| t.id.clone());
        self.tasks = tasks;
        let list = self.selectable_tasks();
        if list.is_empty() {
            self.selected = None;
            return;
        }
        if let Some(id) = selected_id
            && let Some(idx) = list.iter().position(|t| t.id == id)
        {
            self.selected = Some(idx);
            return;
        }
        let clamped = self.selected.unwrap_or(0).min(list.len() - 1);
        self.selected = Some(clamped);
    }

    fn move_selection(&mut self, delta: i32) {
        let count = self.selectable_tasks().len();
        if count == 0 {
            self.selected = None;
            return;
        }
        let current = self.selected.unwrap_or(0) as i32;
        let next = (current + delta).clamp(0, count as i32 - 1);
        self.selected = Some(next as usize);
    }

    fn require_status_in(
        &mut self,
        statuses: &[TaskStatus],
        make: impl FnOnce(String) -> Action,
        verb: &str,
    ) -> Action {
        match self.selected_task() {
            Some(task) if statuses.contains(&task.status) => make(task.id.clone()),
            Some(_) => {
                self.last_message = Some(format!("cannot {verb} the selected task"));
                Action::None
            }
            None => {
                self.last_message = Some(format!("no task selected to {verb}"));
                Action::None
            }
        }
    }

    fn parse_add_input(&self, input: &str) -> Result<(String, String), String> {
        let trimmed = input.trim();
        if let Some((prefix, rest)) = trimmed.split_once(':') {
            let prefix_trimmed = prefix.trim();
            if self.workspace_ids.iter().any(|id| id == prefix_trimmed) {
                let text = rest.trim_start().to_string();
                if text.is_empty() {
                    return Err("task text is empty".to_string());
                }
                return Ok((prefix_trimmed.to_string(), text));
            }
        }
        if trimmed.is_empty() {
            return Err("task text is empty".to_string());
        }
        let workspace = self
            .selected_task()
            .map(|t| t.workspace.clone())
            .ok_or_else(|| {
                "no workspace given (use 'ws: text') and no task selected".to_string()
            })?;
        Ok((workspace, trimmed.to_string()))
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return match self.mode {
                Mode::ConfirmQuit { .. } => Action::Quit,
                _ => self.request_quit(),
            };
        }
        match self.mode {
            Mode::Normal => self.handle_normal_key(key),
            Mode::Input { .. } => self.handle_input_key(key),
            Mode::ConfirmQuit { running } => self.handle_confirm_quit_key(key, running),
        }
    }

    fn request_quit(&mut self) -> Action {
        self.pending_delete = false;
        if !self.core_reachable {
            return Action::Quit;
        }
        let running = self.section_tasks(&[TaskStatus::Running]).len();
        self.mode = Mode::ConfirmQuit { running };
        Action::None
    }

    pub fn confirm_quit_prompt(running: usize) -> String {
        if running == 0 {
            "Stop core too? [y]es / [n]o / [Esc] cancel".to_string()
        } else {
            format!(
                "Stop core too? {running} task(s) running will become interrupted. [y]es / [n]o / [Esc] cancel"
            )
        }
    }

    fn handle_confirm_quit_key(&mut self, key: KeyEvent, running: usize) -> Action {
        match key.code {
            KeyCode::Char('y') => {
                self.mode = Mode::Normal;
                Action::StopCoreAndQuit { force: running > 0 }
            }
            KeyCode::Char('n') => Action::Quit,
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> Action {
        let code = key.code;
        if !matches!(code, KeyCode::Char('d')) {
            self.pending_delete = false;
        }
        match code {
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                Action::None
            }
            KeyCode::Char('n') | KeyCode::Char('o') => {
                self.mode = Mode::Input {
                    mode: InputMode::Add,
                    buffer: Vec::new(),
                    cursor: 0,
                };
                Action::None
            }
            KeyCode::Enter => match self.selected_task() {
                Some(task) if task.status == TaskStatus::Running => {
                    Action::FocusWorkspaceTab(task.workspace.clone())
                }
                _ => Action::None,
            },
            KeyCode::Char('e') => {
                if let Some(task) = self.selected_task() {
                    if task.status != TaskStatus::Running {
                        let id = task.id.clone();
                        let chars: Vec<char> = task.text.chars().collect();
                        let cursor = chars.len();
                        self.mode = Mode::Input {
                            mode: InputMode::Edit { task_id: id },
                            buffer: chars,
                            cursor,
                        };
                    } else {
                        self.last_message = Some("cannot edit a running task".to_string());
                    }
                } else {
                    self.last_message = Some("no task selected to edit".to_string());
                }
                Action::None
            }
            KeyCode::Char('d') => {
                if self.pending_delete {
                    self.pending_delete = false;
                    return self.require_status_in(DELETABLE, Action::Delete, "delete");
                }
                self.pending_delete = true;
                Action::None
            }
            KeyCode::Char('J') => self.require_status_in(
                &[TaskStatus::Queued, TaskStatus::Received],
                |id| Action::Move {
                    task_id: id,
                    direction: MoveDirection::Down,
                },
                "move down",
            ),
            KeyCode::Char('K') => self.require_status_in(
                &[TaskStatus::Queued, TaskStatus::Received],
                |id| Action::Move {
                    task_id: id,
                    direction: MoveDirection::Up,
                },
                "move up",
            ),
            KeyCode::Char('a') => {
                self.require_status_in(&[TaskStatus::Received], Action::Accept, "accept")
            }
            KeyCode::Char('r') => {
                self.require_status_in(&[TaskStatus::Received], Action::Reject, "reject")
            }
            KeyCode::Char('f') => {
                self.require_status_in(&[TaskStatus::Running], Action::Complete, "complete")
            }
            KeyCode::Char('R') => self.require_status_in(RETRYABLE, Action::Retry, "retry"),
            KeyCode::Char('D') => self.require_status_in(MARK_DONE, Action::Complete, "mark done"),
            KeyCode::Char('c') => self.require_status_in(CANCELLABLE, Action::Cancel, "cancel"),
            _ => Action::None,
        }
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                return Action::None;
            }
            KeyCode::Enter => {
                let (input_mode, text) = match &self.mode {
                    Mode::Input { mode, buffer, .. } => {
                        (mode.clone(), buffer.iter().collect::<String>())
                    }
                    _ => unreachable!("handle_input_key called outside input mode"),
                };
                return match input_mode {
                    InputMode::Add => match self.parse_add_input(&text) {
                        Ok((workspace, text)) => {
                            self.mode = Mode::Normal;
                            Action::Enqueue { workspace, text }
                        }
                        Err(e) => {
                            self.last_message = Some(e);
                            Action::None
                        }
                    },
                    InputMode::Edit { task_id } => {
                        if text.trim().is_empty() {
                            self.last_message = Some("task text cannot be empty".to_string());
                            Action::None
                        } else {
                            self.mode = Mode::Normal;
                            Action::Edit { task_id, text }
                        }
                    }
                };
            }
            _ => {}
        }
        if let Mode::Input { buffer, cursor, .. } = &mut self.mode {
            match key.code {
                KeyCode::Backspace => {
                    if *cursor > 0 {
                        *cursor -= 1;
                        buffer.remove(*cursor);
                    }
                }
                KeyCode::Delete => {
                    if *cursor < buffer.len() {
                        buffer.remove(*cursor);
                    }
                }
                KeyCode::Left => {
                    if *cursor > 0 {
                        *cursor -= 1;
                    }
                }
                KeyCode::Right => {
                    if *cursor < buffer.len() {
                        *cursor += 1;
                    }
                }
                KeyCode::Home => *cursor = 0,
                KeyCode::End => *cursor = buffer.len(),
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    buffer.insert(*cursor, c);
                    *cursor += 1;
                }
                _ => {}
            }
        }
        Action::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Source;

    fn task(id: &str, status: TaskStatus, workspace: &str, text: &str, position: i64) -> Task {
        Task {
            id: id.to_string(),
            text: text.to_string(),
            status,
            workspace: workspace.to_string(),
            agent: None,
            source: Source::cli(),
            reply_to: None,
            metadata: serde_json::json!({}),
            position,
            created_at: "2026-01-01T00:00:00+09:00".to_string(),
            updated_at: "2026-01-01T00:00:00+09:00".to_string(),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn selectable_tasks_ordered_by_section() {
        let mut app = App::new();
        app.set_tasks(vec![
            task("1", TaskStatus::Interrupted, "a", "int", 1),
            task("2", TaskStatus::Queued, "a", "q", 2),
            task("3", TaskStatus::Running, "a", "run", 3),
            task("4", TaskStatus::Received, "a", "recv", 4),
            task("5", TaskStatus::Done, "a", "done", 5),
            task("6", TaskStatus::Failed, "a", "failed", 6),
        ]);
        let ids: Vec<&str> = app
            .selectable_tasks()
            .iter()
            .map(|t| t.id.as_str())
            .collect();
        assert_eq!(ids, vec!["3", "2", "4", "1", "6"]);
    }

    #[test]
    fn selection_clamps_after_refresh_shrinks_list() {
        let mut app = App::new();
        app.set_tasks(vec![
            task("1", TaskStatus::Queued, "a", "one", 1),
            task("2", TaskStatus::Queued, "a", "two", 2),
            task("3", TaskStatus::Queued, "a", "three", 3),
        ]);
        app.selected = Some(2);
        app.set_tasks(vec![
            task("1", TaskStatus::Queued, "a", "one", 1),
            task("2", TaskStatus::Queued, "a", "two", 2),
        ]);
        assert_eq!(app.selected, Some(1));
        assert_eq!(app.selected_task().unwrap().id, "2");
    }

    #[test]
    fn selection_follows_task_id_when_order_changes() {
        let mut app = App::new();
        app.set_tasks(vec![
            task("1", TaskStatus::Queued, "a", "one", 1),
            task("2", TaskStatus::Queued, "a", "two", 2),
        ]);
        app.selected = Some(1);
        app.set_tasks(vec![
            task("2", TaskStatus::Queued, "a", "two", 1),
            task("1", TaskStatus::Queued, "a", "one", 2),
        ]);
        assert_eq!(app.selected_task().unwrap().id, "2");
    }

    #[test]
    fn selection_none_when_list_empty() {
        let mut app = App::new();
        app.set_tasks(vec![task("1", TaskStatus::Queued, "a", "one", 1)]);
        app.selected = Some(0);
        app.set_tasks(vec![]);
        assert_eq!(app.selected, None);
        assert!(app.selected_task().is_none());
    }

    #[test]
    fn move_selection_up_down_clamped() {
        let mut app = App::new();
        app.set_tasks(vec![
            task("1", TaskStatus::Queued, "a", "one", 1),
            task("2", TaskStatus::Queued, "a", "two", 2),
        ]);
        assert_eq!(app.selected, Some(0));
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.selected, Some(0));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.selected, Some(1));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.selected, Some(1));
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.selected, Some(0));
    }

    #[test]
    fn n_focuses_input_for_add() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(
            app.mode,
            Mode::Input {
                mode: InputMode::Add,
                ..
            }
        ));
    }

    #[test]
    fn add_input_with_workspace_prefix() {
        let mut app = App::new();
        app.workspace_ids = vec!["amanejp".to_string()];
        app.handle_key(key(KeyCode::Char('n')));
        for c in "amanejp: do a thing".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        let action = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            action,
            Action::Enqueue {
                workspace: "amanejp".to_string(),
                text: "do a thing".to_string(),
            }
        );
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn add_input_without_prefix_uses_selected_task_workspace() {
        let mut app = App::new();
        app.set_tasks(vec![task("1", TaskStatus::Queued, "amanejp", "one", 1)]);
        app.handle_key(key(KeyCode::Char('n')));
        for c in "no prefix here".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        let action = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            action,
            Action::Enqueue {
                workspace: "amanejp".to_string(),
                text: "no prefix here".to_string(),
            }
        );
    }

    #[test]
    fn add_input_without_prefix_and_no_selection_errors() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::Char('n')));
        for c in "no ws".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        let action = app.handle_key(key(KeyCode::Enter));
        assert_eq!(action, Action::None);
        assert!(app.last_message.is_some());
        assert!(matches!(app.mode, Mode::Input { .. }));
    }

    #[test]
    fn esc_cancels_input() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::Char('n')));
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn ctrl_and_alt_chars_are_not_inserted_in_input_mode() {
        let mut app = App::new();
        app.set_tasks(vec![task("1", TaskStatus::Queued, "amanejp", "one", 1)]);
        app.handle_key(key(KeyCode::Char('n')));
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(ctrl(KeyCode::Char('w')));
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT));
        app.handle_key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        app.handle_key(KeyEvent::new(KeyCode::Char('B'), KeyModifiers::SHIFT));
        let action = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            action,
            Action::Enqueue {
                workspace: "amanejp".to_string(),
                text: "aB".to_string(),
            }
        );
    }

    #[test]
    fn multibyte_input_editing_with_backspace() {
        let mut app = App::new();
        app.set_tasks(vec![task("1", TaskStatus::Queued, "amanejp", "one", 1)]);
        app.handle_key(key(KeyCode::Char('n')));
        for c in "こんにちは".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Char('!')));
        let action = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            action,
            Action::Enqueue {
                workspace: "amanejp".to_string(),
                text: "こんにち!".to_string(),
            }
        );
    }

    #[test]
    fn multibyte_cursor_left_insert_stays_on_char_boundary() {
        let mut app = App::new();
        app.set_tasks(vec![task("1", TaskStatus::Queued, "amanejp", "one", 1)]);
        app.handle_key(key(KeyCode::Char('n')));
        for c in "あい".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Char('X')));
        let action = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            action,
            Action::Enqueue {
                workspace: "amanejp".to_string(),
                text: "あXい".to_string(),
            }
        );
    }

    #[test]
    fn dd_deletes_selected_non_running_task() {
        let mut app = App::new();
        app.set_tasks(vec![task("1", TaskStatus::Queued, "a", "one", 1)]);
        let action1 = app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(action1, Action::None);
        let action2 = app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(action2, Action::Delete("1".to_string()));
    }

    #[test]
    fn d_then_other_key_resets_pending_delete() {
        let mut app = App::new();
        app.set_tasks(vec![task("1", TaskStatus::Queued, "a", "one", 1)]);
        app.handle_key(key(KeyCode::Char('d')));
        app.handle_key(key(KeyCode::Char('j')));
        let action = app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(action, Action::None);
        assert!(app.pending_delete);
    }

    #[test]
    fn dd_on_running_task_is_rejected() {
        let mut app = App::new();
        app.set_tasks(vec![task("1", TaskStatus::Running, "a", "one", 1)]);
        app.handle_key(key(KeyCode::Char('d')));
        let action = app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(action, Action::None);
        assert!(app.last_message.is_some());
    }

    #[test]
    fn e_prefills_edit_and_enter_submits() {
        let mut app = App::new();
        app.set_tasks(vec![task("1", TaskStatus::Queued, "a", "old text", 1)]);
        app.handle_key(key(KeyCode::Char('e')));
        match &app.mode {
            Mode::Input {
                mode,
                buffer,
                cursor,
            } => {
                assert_eq!(
                    *mode,
                    InputMode::Edit {
                        task_id: "1".to_string()
                    }
                );
                assert_eq!(buffer.iter().collect::<String>(), "old text");
                assert_eq!(*cursor, "old text".chars().count());
            }
            _ => panic!("expected input mode"),
        }
        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Backspace));
        for c in "code".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        let action = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            action,
            Action::Edit {
                task_id: "1".to_string(),
                text: "old code".to_string(),
            }
        );
    }

    #[test]
    fn e_on_running_task_is_rejected() {
        let mut app = App::new();
        app.set_tasks(vec![task("1", TaskStatus::Running, "a", "one", 1)]);
        let action = app.handle_key(key(KeyCode::Char('e')));
        assert_eq!(action, Action::None);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.last_message.is_some());
    }

    #[test]
    fn j_k_move_queued_task() {
        let mut app = App::new();
        app.set_tasks(vec![task("1", TaskStatus::Queued, "a", "one", 1)]);
        let down = app.handle_key(key(KeyCode::Char('J')));
        assert_eq!(
            down,
            Action::Move {
                task_id: "1".to_string(),
                direction: MoveDirection::Down,
            }
        );
        let up = app.handle_key(key(KeyCode::Char('K')));
        assert_eq!(
            up,
            Action::Move {
                task_id: "1".to_string(),
                direction: MoveDirection::Up,
            }
        );
    }

    #[test]
    fn j_move_on_running_rejected() {
        let mut app = App::new();
        app.set_tasks(vec![task("1", TaskStatus::Running, "a", "one", 1)]);
        let action = app.handle_key(key(KeyCode::Char('J')));
        assert_eq!(action, Action::None);
        assert!(app.last_message.is_some());
    }

    #[test]
    fn accept_and_reject_require_received() {
        let mut app = App::new();
        app.set_tasks(vec![task("1", TaskStatus::Received, "a", "one", 1)]);
        assert_eq!(
            app.handle_key(key(KeyCode::Char('a'))),
            Action::Accept("1".to_string())
        );
        app.set_tasks(vec![task("1", TaskStatus::Received, "a", "one", 1)]);
        assert_eq!(
            app.handle_key(key(KeyCode::Char('r'))),
            Action::Reject("1".to_string())
        );
    }

    #[test]
    fn f_completes_running_task() {
        let mut app = App::new();
        app.set_tasks(vec![task("1", TaskStatus::Running, "a", "one", 1)]);
        assert_eq!(
            app.handle_key(key(KeyCode::Char('f'))),
            Action::Complete("1".to_string())
        );
    }

    #[test]
    fn interrupted_retry_done_cancel() {
        let mut app = App::new();
        app.set_tasks(vec![task("1", TaskStatus::Interrupted, "a", "one", 1)]);
        assert_eq!(
            app.handle_key(key(KeyCode::Char('R'))),
            Action::Retry("1".to_string())
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('D'))),
            Action::Complete("1".to_string())
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('c'))),
            Action::Cancel("1".to_string())
        );
    }

    #[test]
    fn enter_on_running_task_focuses_workspace() {
        let mut app = App::new();
        app.set_tasks(vec![task("1", TaskStatus::Running, "amanejp", "one", 1)]);
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::FocusWorkspaceTab("amanejp".to_string())
        );
    }

    #[test]
    fn enter_on_non_running_task_does_nothing() {
        let mut app = App::new();
        app.set_tasks(vec![task("1", TaskStatus::Queued, "amanejp", "one", 1)]);
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    }

    #[test]
    fn q_without_running_tasks_asks_to_stop_core() {
        let mut app = App::new();
        app.set_tasks(vec![task("1", TaskStatus::Queued, "a", "one", 1)]);
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Action::None);
        assert_eq!(app.mode, Mode::ConfirmQuit { running: 0 });
        assert_eq!(
            App::confirm_quit_prompt(0),
            "Stop core too? [y]es / [n]o / [Esc] cancel"
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            Action::StopCoreAndQuit { force: false }
        );
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn q_with_running_tasks_confirms_and_forces_stop() {
        let mut app = App::new();
        app.set_tasks(vec![
            task("1", TaskStatus::Running, "a", "one", 1),
            task("2", TaskStatus::Running, "b", "two", 2),
            task("3", TaskStatus::Queued, "a", "three", 3),
        ]);
        app.handle_key(key(KeyCode::Char('q')));
        assert_eq!(app.mode, Mode::ConfirmQuit { running: 2 });
        assert_eq!(
            App::confirm_quit_prompt(2),
            "Stop core too? 2 task(s) running will become interrupted. [y]es / [n]o / [Esc] cancel"
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            Action::StopCoreAndQuit { force: true }
        );
    }

    #[test]
    fn confirm_quit_n_quits_without_stopping_core() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::Char('q')));
        assert_eq!(app.handle_key(key(KeyCode::Char('n'))), Action::Quit);
    }

    #[test]
    fn confirm_quit_esc_returns_to_normal() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::Char('q')));
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::None);
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn confirm_quit_ignores_other_keys() {
        let mut app = App::new();
        app.set_tasks(vec![task("1", TaskStatus::Queued, "a", "one", 1)]);
        app.handle_key(key(KeyCode::Char('q')));
        for code in [
            KeyCode::Char('q'),
            KeyCode::Char('j'),
            KeyCode::Char('d'),
            KeyCode::Char('c'),
            KeyCode::Char('Y'),
            KeyCode::Enter,
        ] {
            assert_eq!(app.handle_key(key(code)), Action::None);
            assert_eq!(app.mode, Mode::ConfirmQuit { running: 0 });
        }
        assert!(!app.pending_delete);
    }

    #[test]
    fn q_quits_immediately_when_core_unreachable() {
        let mut app = App::new();
        app.core_reachable = false;
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Action::Quit);
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn ctrl_c_asks_first_and_quits_on_second_press() {
        let mut app = App::new();
        app.set_tasks(vec![task("1", TaskStatus::Running, "a", "one", 1)]);
        assert_eq!(app.handle_key(ctrl(KeyCode::Char('c'))), Action::None);
        assert_eq!(app.mode, Mode::ConfirmQuit { running: 1 });
        assert_eq!(app.handle_key(ctrl(KeyCode::Char('c'))), Action::Quit);
    }

    #[test]
    fn ctrl_c_in_confirm_entered_by_q_quits() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::Char('q')));
        assert_eq!(app.handle_key(ctrl(KeyCode::Char('c'))), Action::Quit);
    }

    #[test]
    fn ctrl_c_quits_immediately_when_core_unreachable() {
        let mut app = App::new();
        app.core_reachable = false;
        assert_eq!(app.handle_key(ctrl(KeyCode::Char('c'))), Action::Quit);
    }

    #[test]
    fn ctrl_c_in_input_mode_leaves_input_and_asks() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::Char('n')));
        app.handle_key(key(KeyCode::Char('x')));
        assert_eq!(app.handle_key(ctrl(KeyCode::Char('c'))), Action::None);
        assert_eq!(app.mode, Mode::ConfirmQuit { running: 0 });
        assert_eq!(app.handle_key(ctrl(KeyCode::Char('c'))), Action::Quit);
    }

    #[test]
    fn unregistered_prefix_is_part_of_the_task_text() {
        let mut app = App::new();
        app.workspace_ids = vec!["amanejp".to_string()];
        app.set_tasks(vec![task("1", TaskStatus::Queued, "amanejp", "one", 1)]);
        app.handle_key(key(KeyCode::Char('n')));
        for c in "fix: broken build".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::Enqueue {
                workspace: "amanejp".to_string(),
                text: "fix: broken build".to_string(),
            }
        );
    }

    #[test]
    fn failed_task_can_be_retried_and_deleted_but_not_cancelled_or_marked_done() {
        let mut app = App::new();
        app.set_tasks(vec![task("1", TaskStatus::Failed, "a", "one", 1)]);
        assert_eq!(
            app.handle_key(key(KeyCode::Char('R'))),
            Action::Retry("1".to_string())
        );
        assert_eq!(app.handle_key(key(KeyCode::Char('c'))), Action::None);
        assert_eq!(app.handle_key(key(KeyCode::Char('D'))), Action::None);
        app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('d'))),
            Action::Delete("1".to_string())
        );
    }

    #[test]
    fn queued_and_received_tasks_can_be_cancelled() {
        for status in [TaskStatus::Queued, TaskStatus::Received] {
            let mut app = App::new();
            app.set_tasks(vec![task("1", status, "a", "one", 1)]);
            assert_eq!(
                app.handle_key(key(KeyCode::Char('c'))),
                Action::Cancel("1".to_string())
            );
        }
    }
}
