use std::path::{Path, PathBuf};

use crate::core::TabLauncher;
use crate::zellij::{PaneInfo, TabInfo, Zellij};

pub struct ZellijLauncher {
    session_name: Option<String>,
    socket_path: PathBuf,
}

impl ZellijLauncher {
    pub fn new(session_name: Option<String>, socket_path: PathBuf) -> ZellijLauncher {
        ZellijLauncher {
            session_name,
            socket_path,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LaunchDecision {
    CreateTab,
    CloseThenCreate(Vec<u32>),
    AlreadyRunning,
}

fn is_runner_command(command: &str, exe_name: &str, workspace_id: &str) -> bool {
    let Some(prefix) = command.strip_suffix(&format!(" runner {workspace_id}")) else {
        return false;
    };
    let exe = match prefix.find(" --socket ") {
        Some(idx) => &prefix[..idx],
        None => prefix,
    };
    Path::new(exe.trim()).file_name().and_then(|n| n.to_str()) == Some(exe_name)
}

fn decide(
    tabs: &[TabInfo],
    panes: &[PaneInfo],
    workspace_id: &str,
    exe_name: &str,
) -> LaunchDecision {
    let mut exited_tabs = Vec::new();
    for tab in tabs.iter().filter(|t| t.name == workspace_id) {
        let runner_panes: Vec<&PaneInfo> = panes
            .iter()
            .filter(|p| p.tab_id == tab.tab_id && !p.is_plugin)
            .filter(|p| {
                [p.terminal_command.as_deref(), p.pane_command.as_deref()]
                    .into_iter()
                    .flatten()
                    .any(|c| is_runner_command(c.trim(), exe_name, workspace_id))
            })
            .collect();
        if runner_panes.iter().any(|p| !p.exited) {
            return LaunchDecision::AlreadyRunning;
        }
        if !runner_panes.is_empty() {
            exited_tabs.push(tab.tab_id);
        }
    }
    if exited_tabs.is_empty() {
        LaunchDecision::CreateTab
    } else {
        LaunchDecision::CloseThenCreate(exited_tabs)
    }
}

fn exe_file_name(exe: &Path) -> String {
    let name = exe
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.trim_end_matches(" (deleted)").to_string()
}

impl TabLauncher for ZellijLauncher {
    fn launch(&self, workspace_id: &str, workspace_path: &Path) -> anyhow::Result<()> {
        let Some(session) = &self.session_name else {
            anyhow::bail!(
                "loom core must be started inside a Zellij session to launch runner tabs"
            );
        };
        let zellij = Zellij::new(session.clone());

        let exe_path = std::env::current_exe()?;
        let tabs = zellij.list_tabs()?;
        let panes = zellij.list_panes()?;

        match decide(&tabs, &panes, workspace_id, &exe_file_name(&exe_path)) {
            LaunchDecision::AlreadyRunning => return Ok(()),
            LaunchDecision::CloseThenCreate(tab_ids) => {
                for tab_id in tab_ids {
                    zellij.close_tab_by_id(tab_id)?;
                }
            }
            LaunchDecision::CreateTab => {}
        }

        let argv = vec![
            exe_path.to_string_lossy().into_owned(),
            "--socket".to_string(),
            self.socket_path.to_string_lossy().into_owned(),
            "runner".to_string(),
            workspace_id.to_string(),
        ];
        zellij.new_tab(workspace_id, workspace_path, &argv)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(id: u32, name: &str) -> TabInfo {
        TabInfo {
            tab_id: id,
            name: name.to_string(),
            position: id,
        }
    }

    fn pane(tab_id: u32, command: Option<&str>, exited: bool) -> PaneInfo {
        PaneInfo {
            id: 0,
            is_plugin: false,
            is_focused: false,
            exited,
            exit_status: if exited { Some(0) } else { None },
            terminal_command: command.map(str::to_string),
            pane_command: None,
            pane_cwd: None,
            tab_id,
            tab_name: None,
        }
    }

    fn runner_pane(tab_id: u32, exited: bool) -> PaneInfo {
        pane(
            tab_id,
            Some("/usr/local/bin/loom --socket /run/user/1000/zelloom/default.sock runner amanejp"),
            exited,
        )
    }

    fn plugin_pane(tab_id: u32) -> PaneInfo {
        PaneInfo {
            id: 0,
            is_plugin: true,
            is_focused: false,
            exited: false,
            exit_status: None,
            terminal_command: None,
            pane_command: None,
            pane_cwd: None,
            tab_id,
            tab_name: None,
        }
    }

    fn run(tabs: &[TabInfo], panes: &[PaneInfo]) -> LaunchDecision {
        decide(tabs, panes, "amanejp", "loom")
    }

    #[test]
    fn no_existing_tab_creates_one() {
        let tabs = vec![tab(0, "Tab #1")];
        let panes = vec![plugin_pane(0)];
        assert_eq!(run(&tabs, &panes), LaunchDecision::CreateTab);
    }

    #[test]
    fn live_runner_pane_is_not_duplicated() {
        let tabs = vec![tab(0, "Tab #1"), tab(1, "amanejp")];
        let panes = vec![plugin_pane(0), runner_pane(1, false)];
        assert_eq!(run(&tabs, &panes), LaunchDecision::AlreadyRunning);
    }

    #[test]
    fn exited_runner_pane_closes_then_creates() {
        let tabs = vec![tab(0, "Tab #1"), tab(1, "amanejp")];
        let panes = vec![plugin_pane(0), plugin_pane(1), runner_pane(1, true)];
        assert_eq!(run(&tabs, &panes), LaunchDecision::CloseThenCreate(vec![1]));
    }

    #[test]
    fn same_named_tab_without_runner_pane_is_ignored() {
        let tabs = vec![tab(0, "Tab #1"), tab(1, "amanejp")];
        let panes = vec![plugin_pane(0), pane(1, Some("/usr/bin/loom"), false)];
        assert_eq!(run(&tabs, &panes), LaunchDecision::CreateTab);
    }

    #[test]
    fn same_named_tab_with_no_panes_listed_is_ignored() {
        let tabs = vec![tab(0, "Tab #1"), tab(1, "amanejp")];
        let panes = vec![plugin_pane(0)];
        assert_eq!(run(&tabs, &panes), LaunchDecision::CreateTab);
    }

    #[test]
    fn user_tab_running_the_tui_does_not_count_as_runner() {
        let tabs = vec![tab(0, "loom")];
        let panes = vec![pane(0, Some("/usr/local/bin/loom"), false)];
        assert_eq!(
            decide(&tabs, &panes, "loom", "loom"),
            LaunchDecision::CreateTab
        );
    }

    #[test]
    fn runner_for_another_workspace_does_not_count() {
        let tabs = vec![tab(1, "amanejp")];
        let panes = vec![pane(
            1,
            Some("/usr/local/bin/loom --socket /tmp/s.sock runner other"),
            false,
        )];
        assert_eq!(run(&tabs, &panes), LaunchDecision::CreateTab);
    }

    #[test]
    fn other_program_named_runner_does_not_count() {
        let tabs = vec![tab(1, "amanejp")];
        let panes = vec![pane(1, Some("/usr/bin/cargo runner amanejp"), false)];
        assert_eq!(run(&tabs, &panes), LaunchDecision::CreateTab);
    }

    #[test]
    fn live_runner_among_duplicate_named_tabs_wins() {
        let tabs = vec![tab(1, "amanejp"), tab(2, "amanejp"), tab(3, "amanejp")];
        let panes = vec![
            pane(1, Some("/usr/local/bin/loom"), false),
            runner_pane(2, true),
            runner_pane(3, false),
        ];
        assert_eq!(run(&tabs, &panes), LaunchDecision::AlreadyRunning);
    }

    #[test]
    fn only_tabs_with_exited_runner_are_closed() {
        let tabs = vec![tab(1, "amanejp"), tab(2, "amanejp"), tab(3, "amanejp")];
        let panes = vec![
            pane(1, Some("/usr/local/bin/loom"), false),
            runner_pane(2, true),
            runner_pane(3, true),
        ];
        assert_eq!(
            run(&tabs, &panes),
            LaunchDecision::CloseThenCreate(vec![2, 3])
        );
    }

    #[test]
    fn runner_busy_with_agent_is_found_by_terminal_command() {
        let tabs = vec![tab(1, "amanejp")];
        let mut busy = runner_pane(1, false);
        busy.pane_command = Some("claude --append-system-prompt hi".to_string());
        assert_eq!(run(&tabs, &[busy]), LaunchDecision::AlreadyRunning);
    }

    #[test]
    fn runner_started_from_a_shell_is_found_by_pane_command() {
        let tabs = vec![tab(1, "amanejp")];
        let mut manual = pane(1, None, false);
        manual.pane_command = Some("loom runner amanejp".to_string());
        assert_eq!(run(&tabs, &[manual]), LaunchDecision::AlreadyRunning);
    }

    #[test]
    fn runner_command_matching_accepts_socket_args_and_spaces() {
        assert!(is_runner_command(
            "/opt/my tools/loom runner ws",
            "loom",
            "ws"
        ));
        assert!(is_runner_command(
            "/opt/bin/loom --socket /tmp/a b.sock runner ws",
            "loom",
            "ws"
        ));
        assert!(is_runner_command("loom runner ws", "loom", "ws"));
        assert!(!is_runner_command("/opt/bin/loom runner ws2", "loom", "ws"));
        assert!(!is_runner_command("/opt/bin/loom list", "loom", "ws"));
        assert!(!is_runner_command(
            "/opt/bin/notloom runner ws",
            "loom",
            "ws"
        ));
    }

    #[test]
    fn exe_file_name_strips_deleted_suffix() {
        assert_eq!(exe_file_name(Path::new("/usr/bin/loom (deleted)")), "loom");
        assert_eq!(exe_file_name(Path::new("/usr/bin/loom")), "loom");
    }
}
