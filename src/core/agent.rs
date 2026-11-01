use std::collections::BTreeMap;

use crate::config::{AgentConfig, INSTRUCTION_PLACEHOLDER, LOOM_EXE_PLACEHOLDER, WorkspaceConfig};
use crate::protocol::Task;

pub(super) const MAX_MESSAGE_BYTES: usize = 16 * 1024;

pub(super) fn loom_exe_path() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "loom".to_string())
}

pub(super) fn build_instruction(agent_cfg: &AgentConfig, loom_exe: &str) -> String {
    if let Some(custom) = &agent_cfg.instruction {
        return custom.replace(LOOM_EXE_PLACEHOLDER, loom_exe);
    }
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

pub(super) fn build_argv(agent_cfg: &AgentConfig, instruction: &str, text: &str) -> Vec<String> {
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

pub(super) fn build_env(
    agent_cfg: &AgentConfig,
    ws: &WorkspaceConfig,
    task: &Task,
    socket_path: &str,
    loom_exe: &str,
) -> BTreeMap<String, String> {
    let mut env = agent_cfg.env.clone();
    env.extend(ws.env.clone());
    env.insert("ZELLOOM_TASK_ID".to_string(), task.id.clone());
    env.insert("ZELLOOM_WORKSPACE".to_string(), task.workspace.clone());
    env.insert("ZELLOOM_SOCKET".to_string(), socket_path.to_string());
    env.insert("ZELLOOM_LOOM_EXE".to_string(), loom_exe.to_string());
    env
}

fn is_forbidden_control(c: char) -> bool {
    c.is_control() && c != '\n'
}

pub(super) fn validate_reply(text: &str) -> Result<String, String> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let text = text.trim_end();
    if text.is_empty() {
        return Err("reply text is empty".to_string());
    }
    if text.len() > MAX_MESSAGE_BYTES {
        return Err(format!(
            "reply text is {} bytes; the limit is {MAX_MESSAGE_BYTES}",
            text.len()
        ));
    }
    if let Some(c) = text.chars().find(|c| is_forbidden_control(*c)) {
        return Err(format!(
            "reply text contains the control character U+{:04X}; only line breaks are allowed",
            c as u32
        ));
    }
    match text.trim_start().chars().next() {
        Some('!') => Err(
            "reply text must not start with '!': the agent would run the rest as a shell command"
                .to_string(),
        ),
        Some('/') => Err(
            "reply text must not start with '/': the agent would run it as a slash command"
                .to_string(),
        ),
        _ => Ok(text.to_string()),
    }
}

pub(super) fn validate_report_message(message: &str) -> Result<String, String> {
    let message = message.trim_end();
    if message.len() > MAX_MESSAGE_BYTES {
        return Err(format!(
            "report message is {} bytes; the limit is {MAX_MESSAGE_BYTES}",
            message.len()
        ));
    }
    if message.contains('\0') {
        return Err("report message contains a NUL character".to_string());
    }
    Ok(message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Source, TaskStatus};

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
            attention: None,
            turn: None,
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
        let mut agent = AgentConfig::for_test(&["a"]);
        agent.env = map(&[
            ("ONLY_AGENT", "agent"),
            ("BOTH", "agent"),
            ("ZELLOOM_TASK_ID", "agent"),
        ]);
        let ws = WorkspaceConfig {
            path: "/tmp/w".into(),
            agent: None,
            max_parallel: None,
            env: map(&[("ONLY_WS", "ws"), ("BOTH", "ws"), ("ZELLOOM_SOCKET", "ws")]),
        };
        let env = build_env(&agent, &ws, &task(), "/tmp/s.sock", "/bin/loom");
        assert_eq!(
            env,
            map(&[
                ("BOTH", "ws"),
                ("ONLY_AGENT", "agent"),
                ("ONLY_WS", "ws"),
                ("ZELLOOM_LOOM_EXE", "/bin/loom"),
                ("ZELLOOM_SOCKET", "/tmp/s.sock"),
                ("ZELLOOM_TASK_ID", "t1"),
                ("ZELLOOM_WORKSPACE", "w"),
            ])
        );
    }

    #[test]
    fn instruction_args_embed_instruction_before_task_text() {
        let mut agent = AgentConfig::for_test(&["codex"]);
        agent.instruction_args = Some(vec![
            "-c".into(),
            "developer_instructions={instruction}".into(),
        ]);
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
    fn default_instruction_mentions_loom_exe() {
        let agent = AgentConfig::for_test(&["a"]);
        let text = build_instruction(&agent, "/bin/loom");
        assert!(text.contains("/bin/loom done"), "{text}");
    }

    #[test]
    fn custom_instruction_replaces_default_and_expands_loom_exe() {
        let mut agent = AgentConfig::for_test(&["a"]);
        agent.instruction = Some("finish with {loom_exe} done, then {loom_exe} list".into());
        assert_eq!(
            build_instruction(&agent, "/bin/loom"),
            "finish with /bin/loom done, then /bin/loom list"
        );
        let argv = build_argv(&agent, &build_instruction(&agent, "/bin/loom"), "fix it");
        assert_eq!(
            argv.last().unwrap(),
            "finish with /bin/loom done, then /bin/loom list\nfix it"
        );
    }

    #[test]
    fn reply_text_is_normalized() {
        assert_eq!(validate_reply("yes\r\n"), Ok("yes".to_string()));
        assert_eq!(validate_reply("a\r\nb\rc\n  \n"), Ok("a\nb\nc".to_string()));
        assert_eq!(
            validate_reply("  keep leading"),
            Ok("  keep leading".to_string())
        );
        assert_eq!(
            validate_reply("日本語で\n-y はオプションではない"),
            Ok("日本語で\n-y はオプションではない".to_string())
        );
        assert_eq!(
            validate_reply("path a/b and a ! later"),
            Ok("path a/b and a ! later".to_string())
        );
        assert_eq!(
            validate_reply("ok\n/later line"),
            Ok("ok\n/later line".to_string())
        );
    }

    #[test]
    fn reply_text_rejects_empty_control_command_and_oversized_text() {
        assert_eq!(
            validate_reply(" \r\n\t"),
            Err("reply text is empty".to_string())
        );
        assert_eq!(validate_reply(""), Err("reply text is empty".to_string()));
        for (text, code) in [
            ("a\x1bb", "001B"),
            ("tab\there", "0009"),
            ("\x03", "0003"),
            ("\x04", "0004"),
            ("x\x1b[Z", "001B"),
            ("\x1b[200~paste", "001B"),
            ("x\0y", "0000"),
            ("del\x7f", "007F"),
            ("c1\u{9b}2J", "009B"),
            ("osc\u{9d}0;t", "009D"),
        ] {
            let error = validate_reply(text).unwrap_err();
            assert!(error.contains(&format!("U+{code}")), "{text:?}: {error}");
        }
        for text in ["!rm -rf x", "  !ls", "\n\n!ls"] {
            let error = validate_reply(text).unwrap_err();
            assert!(error.contains("shell command"), "{text:?}: {error}");
        }
        for text in ["/permissions", " /exit", "\n/clear"] {
            let error = validate_reply(text).unwrap_err();
            assert!(error.contains("slash command"), "{text:?}: {error}");
        }
        assert!(validate_reply(&"a".repeat(MAX_MESSAGE_BYTES)).is_ok());
        assert!(
            validate_reply(&"a".repeat(MAX_MESSAGE_BYTES + 1))
                .unwrap_err()
                .contains("limit")
        );
        assert!(
            validate_reply(&"あ".repeat(MAX_MESSAGE_BYTES / 3 + 1))
                .unwrap_err()
                .contains("bytes")
        );
    }

    #[test]
    fn report_message_keeps_control_characters_but_rejects_nul_and_oversize() {
        assert_eq!(
            validate_report_message("a\x1b[31mred\tb\n\n"),
            Ok("a\x1b[31mred\tb".to_string())
        );
        assert_eq!(validate_report_message(""), Ok(String::new()));
        assert!(validate_report_message("a\0b").unwrap_err().contains("NUL"));
        assert!(validate_report_message(&"x".repeat(MAX_MESSAGE_BYTES)).is_ok());
        assert!(
            validate_report_message(&"x".repeat(MAX_MESSAGE_BYTES + 1))
                .unwrap_err()
                .contains("limit")
        );
    }

    #[test]
    fn oneshot_passes_only_task_text() {
        let mut agent = AgentConfig::for_test(&["claude", "-p"]);
        agent.oneshot = true;
        assert_eq!(
            build_argv(&agent, "be nice\n", "fix it"),
            vec!["claude".to_string(), "-p".to_string(), "fix it".to_string()]
        );
    }
}
