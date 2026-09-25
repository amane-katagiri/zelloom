use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::protocol::{Task, TaskStatus};
use crate::tui::app::{App, Mode, SECTION_STATUSES};

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Min(3),
        Constraint::Min(3),
        Constraint::Min(3),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);

    let [running, queued, inbox, interrupted] = SECTION_STATUSES.map(|s| app.section_tasks(s));

    let selected_id = app.selected_task().map(|t| t.id.clone());

    render_section(frame, chunks[0], "RUNNING", &running, &selected_id, |t| {
        format!("\u{25cf} {:<14} {}", t.workspace, t.text)
    });
    render_section(frame, chunks[1], "QUEUED", &queued, &selected_id, |t| {
        format!("  {:<14} {}", t.workspace, t.text)
    });
    render_section(frame, chunks[2], "INBOX", &inbox, &selected_id, |t| {
        format!("\u{25cb} {}: {}", t.source.kind, t.text)
    });

    let (done, cancelled) = app.finished_counts();
    let interrupted_title = if done + cancelled > 0 {
        format!("INTERRUPTED / FAILED (total: {done} done / {cancelled} cancelled)")
    } else {
        "INTERRUPTED / FAILED".to_string()
    };
    render_section(
        frame,
        chunks[3],
        &interrupted_title,
        &interrupted,
        &selected_id,
        |t| {
            let mark = if t.status == TaskStatus::Failed {
                '\u{2717}'
            } else {
                ' '
            };
            format!("{mark} {:<14} {}", t.workspace, t.text)
        },
    );

    let status_text = if !app.core_reachable {
        "core not running - retrying...".to_string()
    } else if let Some(msg) = &app.last_message {
        msg.clone()
    } else {
        "n:add  e:edit  dd:delete  J/K:move  a:accept  r:reject  f:complete  R:retry  D:done  c:cancel  Enter:focus  q:quit"
            .to_string()
    };
    frame.render_widget(Paragraph::new(status_text), chunks[4]);

    render_input_line(frame, chunks[5], app);
}

fn render_section(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    tasks: &[&Task],
    selected_id: &Option<String>,
    fmt: impl Fn(&Task) -> String,
) {
    let items: Vec<ListItem> = tasks
        .iter()
        .map(|t| {
            let line = fmt(t);
            let style = if selected_id.as_deref() == Some(t.id.as_str()) {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            ListItem::new(line).style(style)
        })
        .collect();
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title.to_string()),
    );
    frame.render_widget(list, area);
}

fn render_input_line(frame: &mut Frame, area: Rect, app: &App) {
    if let Mode::ConfirmQuit { running } = app.mode {
        let prompt = App::confirm_quit_prompt(running);
        frame.render_widget(
            Paragraph::new(prompt).style(Style::default().fg(Color::Yellow)),
            area,
        );
        return;
    }
    let prefix = "> ";
    let (text, cursor, placeholder) = match &app.mode {
        Mode::Normal => ("add task...".to_string(), None, true),
        Mode::Input { buffer, cursor, .. } => {
            (buffer.iter().collect::<String>(), Some(*cursor), false)
        }
        Mode::ConfirmQuit { .. } => unreachable!("confirm prompt rendered above"),
    };
    let style = if placeholder {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };
    let line = format!("{prefix}{text}");
    frame.render_widget(Paragraph::new(line).style(style), area);

    if let Some(cursor) = cursor {
        let prefix_width = UnicodeWidthStr::width(prefix) as u16;
        let text_width: usize = text
            .chars()
            .take(cursor)
            .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
            .sum();
        let x = area.x + prefix_width + text_width as u16;
        let x = x.min(area.x + area.width.saturating_sub(1));
        frame.set_cursor_position((x, area.y));
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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

    fn rendered_text(app: &App) -> String {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            let mut x = 0u16;
            while x < buffer.area.width {
                let symbol = buffer[(x, y)].symbol();
                out.push_str(symbol);
                let width = UnicodeWidthStr::width(symbol).max(1) as u16;
                x += width;
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn renders_all_sections_with_sample_tasks() {
        let mut app = App::new();
        app.set_tasks(vec![
            task("1", TaskStatus::Running, "amanejp", "ログイン処理を修正", 1),
            task("2", TaskStatus::Queued, "amanejp", "テスト追加", 2),
            task("3", TaskStatus::Queued, "swing", "README更新", 3),
            task("4", TaskStatus::Received, "swing", "CI失敗を確認", 4),
            task(
                "5",
                TaskStatus::Interrupted,
                "zelloom",
                "Nostr adapter追加",
                5,
            ),
            task("6", TaskStatus::Failed, "zelloom", "壊れたタスク", 6),
            task("7", TaskStatus::Done, "zelloom", "終わったタスク", 7),
        ]);

        let text = rendered_text(&app);
        assert!(text.contains("RUNNING"));
        assert!(text.contains("QUEUED"));
        assert!(text.contains("INBOX"));
        assert!(text.contains("INTERRUPTED / FAILED (total: 1 done / 0 cancelled)"));
        assert!(text.contains("\u{2717} zelloom"));
        assert!(text.contains("壊れたタスク"));
        assert!(!text.contains("終わったタスク"));
        assert!(text.contains("ログイン処理を修正"));
        assert!(text.contains("テスト追加"));
        assert!(text.contains("README更新"));
        assert!(text.contains("CI失敗を確認"));
        assert!(text.contains("Nostr adapter追加"));
        assert!(text.contains("add task..."));
    }

    #[test]
    fn renders_input_buffer_when_in_input_mode() {
        let mut app = App::new();
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('n'),
            crossterm::event::KeyModifiers::NONE,
        ));
        for c in "amanejp: 新しいタスク".chars() {
            app.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        let text = rendered_text(&app);
        assert!(text.contains("amanejp: 新しいタスク"));
    }

    #[test]
    fn renders_confirm_quit_prompt_on_bottom_line() {
        let mut app = App::new();
        app.set_tasks(vec![task("1", TaskStatus::Running, "a", "one", 1)]);
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('q'),
            crossterm::event::KeyModifiers::NONE,
        ));
        let text = rendered_text(&app);
        let last = text.lines().last().unwrap();
        assert!(last.contains(
            "Stop core too? 1 task(s) running will become interrupted. [y]es / [n]o / [Esc] cancel"
        ));
        assert!(!text.contains("add task..."));
    }

    #[test]
    fn shows_core_unreachable_message() {
        let mut app = App::new();
        app.core_reachable = false;
        let text = rendered_text(&app);
        assert!(text.contains("core not running"));
    }
}
