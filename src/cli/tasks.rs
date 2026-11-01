use std::io::Read;

use crate::protocol::{AttentionKind, Request, Source, Task, TurnState};

pub(super) fn cmd_add(
    workspace: Option<String>,
    agent: Option<String>,
    source: String,
    text: Option<String>,
) -> anyhow::Result<()> {
    validate_source_type(&source)?;
    let text = arg_or_stdin(text)?;
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
        source: Source {
            kind: source,
            id: None,
            sender: None,
        },
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

fn arg_or_stdin(value: Option<String>) -> anyhow::Result<String> {
    match value {
        Some(v) => Ok(v),
        None => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            Ok(buf.trim_end().to_string())
        }
    }
}

fn validate_source_type(kind: &str) -> anyhow::Result<()> {
    if kind.is_empty() {
        anyhow::bail!("source type is empty");
    }
    if kind.chars().any(|c| c.is_whitespace() || c.is_control()) {
        anyhow::bail!("source type '{kind}' must not contain whitespace or control characters");
    }
    Ok(())
}

fn missing_config_error(config_path: &std::path::Path) -> anyhow::Error {
    anyhow::anyhow!(
        "config file {} does not exist.\n  create it:    loom init\n  then:         cd <project> && loom workspace add",
        config_path.display()
    )
}

fn task_id_or_env(task_id: Option<String>) -> anyhow::Result<String> {
    task_id
        .or_else(|| std::env::var("ZELLOOM_TASK_ID").ok())
        .ok_or_else(|| anyhow::anyhow!("no task id given and ZELLOOM_TASK_ID is not set"))
}

pub(super) fn cmd_done(task_id: Option<String>) -> anyhow::Result<()> {
    let task_id = task_id_or_env(task_id)?;

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

pub(super) fn cmd_report(
    kind: AttentionKind,
    message: Option<String>,
    task: Option<String>,
) -> anyhow::Result<()> {
    let task_id = task_id_or_env(task)?;
    let message = arg_or_stdin(message)?;
    crate::client::call(
        &crate::paths::socket_path(),
        &Request::Report {
            task_id: task_id.clone(),
            kind,
            message,
        },
    )?;
    println!("task {task_id} reported {}", kind.as_str());
    Ok(())
}

pub(super) fn cmd_turn(state: TurnState, task: Option<String>) -> anyhow::Result<()> {
    let task_id = task_id_or_env(task)?;
    crate::client::call(
        &crate::paths::socket_path(),
        &Request::Turn { task_id, state },
    )?;
    Ok(())
}

pub(super) fn cmd_send(task_id: String, expect: u64, text: Option<String>) -> anyhow::Result<()> {
    let text = arg_or_stdin(text)?;
    crate::client::call(
        &crate::paths::socket_path(),
        &Request::Send {
            task_id: task_id.clone(),
            expect,
            text,
        },
    )?;
    println!("sent the reply to task {task_id}");
    Ok(())
}

pub(super) fn cmd_watch() -> anyhow::Result<()> {
    use std::io::Write;

    let socket_path = crate::paths::socket_path();
    let mut client = crate::client::Client::connect(&socket_path)?;
    client.write_request(&Request::Watch)?;
    let mut stdout = std::io::stdout().lock();
    while let Some(line) = client.read_raw_line()? {
        stdout.write_all(line.as_bytes())?;
        stdout.flush()?;
    }
    anyhow::bail!("connection to loom core closed; run `loom watch` again to resync")
}

pub(super) fn cmd_show(task_id: &str, json: bool) -> anyhow::Result<()> {
    let data = crate::client::call(
        &crate::paths::socket_path(),
        &Request::Get {
            task_id: task_id.to_string(),
        },
    )?;
    let task: Task = serde_json::from_value(data)?;
    if json {
        println!("{}", serde_json::to_string(&task)?);
    } else {
        print!("{}", format_task_detail(&task));
    }
    Ok(())
}

fn format_task_detail(task: &Task) -> String {
    let mut out = String::new();
    let mut field = |key: &str, value: &str| {
        out.push_str(&format!("{key}: {}\n", escape_control(value, false)));
    };
    field("id", &task.id);
    field("status", task.status.as_str());
    field("workspace", &task.workspace);
    field("agent", task.agent.as_deref().unwrap_or("-"));
    field("source", &task.source.kind);
    if let Some(id) = &task.source.id {
        field("source_id", id);
    }
    if let Some(sender) = &task.source.sender {
        field("source_sender", sender);
    }
    if let Some(reply_to) = &task.reply_to {
        field("reply_to", &reply_to.to_string());
    }
    if task.metadata.as_object().is_none_or(|m| !m.is_empty()) {
        field("metadata", &task.metadata.to_string());
    }
    field("position", &task.position.to_string());
    field("created_at", &task.created_at);
    field("updated_at", &task.updated_at);
    if let Some(turn) = task.turn {
        field("turn", turn.as_str());
    }
    if let Some(attention) = &task.attention {
        field("attention", attention.kind.as_str());
        field("attention_id", &attention.id.to_string());
        field("attention_at", &attention.at);
        out.push_str("attention_message:\n");
        out.push_str(&escape_control(&attention.message, true));
        out.push('\n');
    }
    out.push_str("text:\n");
    out.push_str(&escape_control(&task.text, true));
    out.push('\n');
    out
}

fn escape_control(text: &str, keep_newlines: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' if keep_newlines => out.push(c),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c if ('\u{80}'..='\u{9f}').contains(&c) => {
                out.push_str(&format!("\\u{{{:x}}}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

pub(super) fn cmd_list(json: bool) -> anyhow::Result<()> {
    let data = crate::client::call(&crate::paths::socket_path(), &Request::List)?;
    let tasks: Vec<Task> = serde_json::from_value(data)?;
    if json {
        println!("{}", serde_json::to_string(&tasks)?);
        return Ok(());
    }

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
            escape_control(&task.workspace, false),
            escape_control(task.agent.as_deref().unwrap_or("-"), false),
            escape_control(&task.text, true),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::TaskStatus;

    fn task(id: &str, text: &str) -> Task {
        Task {
            id: id.to_string(),
            text: text.to_string(),
            status: TaskStatus::Queued,
            workspace: "w".to_string(),
            agent: None,
            source: Source::cli(),
            reply_to: None,
            metadata: serde_json::json!({}),
            position: 2,
            created_at: "c".to_string(),
            updated_at: "u".to_string(),
            attention: None,
            turn: None,
        }
    }

    #[test]
    fn source_type_validation() {
        assert!(validate_source_type("webhook").is_ok());
        assert!(validate_source_type("my-app.v2").is_ok());
        for bad in ["", "a b", "a\tb", "a\nb", " ", "a\u{7}"] {
            assert!(validate_source_type(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn escape_control_makes_terminal_controls_visible() {
        assert_eq!(
            escape_control("plain 日本語 \u{a0}ok", true),
            "plain 日本語 \u{a0}ok"
        );
        assert_eq!(
            escape_control("\x1b[31mred\x1b[0m", true),
            "\\x1b[31mred\\x1b[0m"
        );
        assert_eq!(
            escape_control("a\tb\rc\x07\x00d\x7fe", true),
            "a\\x09b\\x0dc\\x07\\x00d\\x7fe"
        );
        assert_eq!(
            escape_control("c1\u{9b}2J\u{80}\u{9f}", true),
            "c1\\u{9b}2J\\u{80}\\u{9f}"
        );
        assert_eq!(escape_control("l1\nl2", true), "l1\nl2");
        assert_eq!(escape_control("l1\nl2", false), "l1\\nl2");
        assert_eq!(escape_control("", true), "");
    }

    #[test]
    fn task_detail_includes_full_multiline_text() {
        let out = format_task_detail(&task("a", "line1\nline2"));
        assert!(out.contains("id: a\n"), "{out}");
        assert!(out.contains("status: queued\n"), "{out}");
        assert!(out.contains("source: cli\n"), "{out}");
        assert!(out.ends_with("text:\nline1\nline2\n"), "{out}");
    }

    #[test]
    fn task_detail_escapes_control_characters_in_every_field() {
        let mut t = task("a", "boom\x1b]0;title\x07\nnext");
        t.source.sender = Some("x\nstatus: done".to_string());
        t.attention = Some(crate::protocol::Attention {
            id: 1,
            kind: AttentionKind::Question,
            message: "q\u{9b}2J\nmore".to_string(),
            at: "t".to_string(),
        });
        let out = format_task_detail(&t);
        assert!(out.contains("source_sender: x\\nstatus: done\n"), "{out}");
        assert!(
            out.contains("attention_message:\nq\\u{9b}2J\nmore\n"),
            "{out}"
        );
        assert!(
            out.ends_with("text:\nboom\\x1b]0;title\\x07\nnext\n"),
            "{out}"
        );
        assert!(!out.contains('\x1b') && !out.contains('\u{9b}'), "{out:?}");
    }

    #[test]
    fn task_detail_shows_attention() {
        let mut t = task("a", "body");
        t.turn = Some(TurnState::Idle);
        t.attention = Some(crate::protocol::Attention {
            id: 12,
            kind: AttentionKind::Question,
            message: "which db?\nsqlite or pg".to_string(),
            at: "2026-01-01T00:00:00+09:00".to_string(),
        });
        let out = format_task_detail(&t);
        assert!(out.contains("turn: idle\n"), "{out}");
        assert!(
            out.contains("attention: question\nattention_id: 12\n"),
            "{out}"
        );
        assert!(
            out.contains("attention_at: 2026-01-01T00:00:00+09:00\n"),
            "{out}"
        );
        assert!(
            out.contains("attention_message:\nwhich db?\nsqlite or pg\ntext:\nbody\n"),
            "{out}"
        );
    }
}
