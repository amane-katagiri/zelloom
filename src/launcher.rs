use std::path::{Path, PathBuf};

use crate::core::TabLauncher;
use crate::zellij::{PaneInfo, TabInfo, Zellij, ZellijError};

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

struct Sockets<'a> {
    own: &'a Path,
    default: &'a Path,
}

pub const CLOSE_PANE_FLAG: &str = "--close-pane-on-exit";

fn is_loom_exe(exe: &str, exe_name: &str) -> bool {
    Path::new(exe.trim()).file_name().and_then(|n| n.to_str()) == Some(exe_name)
}

fn split_socket(prefix: &str) -> (&str, Option<&str>) {
    match prefix.split_once(" --socket ") {
        Some((exe, socket)) => (exe, Some(socket)),
        None => (prefix, None),
    }
}

impl Sockets<'_> {
    fn is_own(&self, socket: Option<&str>) -> bool {
        match socket {
            Some(socket) => Path::new(socket) == self.own,
            None => self.own == self.default,
        }
    }
}

fn parse_runner_command<'a>(
    command: &'a str,
    exe_name: &str,
) -> Option<(Option<&'a str>, &'a str)> {
    let (prefix, workspace) = command.rsplit_once(' ')?;
    let prefix = prefix
        .strip_suffix(&format!(" {CLOSE_PANE_FLAG}"))
        .unwrap_or(prefix);
    let (exe, socket) = split_socket(prefix.strip_suffix(" runner")?);
    is_loom_exe(exe, exe_name).then_some((socket, workspace))
}

fn is_runner_command(command: &str, exe_name: &str, workspace_id: &str) -> bool {
    parse_runner_command(command, exe_name).is_some_and(|(_, ws)| ws == workspace_id)
}

fn is_own_runner_command(command: &str, exe_name: &str, sockets: &Sockets) -> bool {
    parse_runner_command(command, exe_name).is_some_and(|(socket, _)| sockets.is_own(socket))
}

fn is_tui_command(command: &str, exe_name: &str, sockets: &Sockets) -> bool {
    let (exe, socket) = split_socket(command);
    is_loom_exe(exe, exe_name) && sockets.is_own(socket)
}

fn pane_commands(pane: &PaneInfo) -> impl Iterator<Item = &str> {
    [
        pane.terminal_command.as_deref(),
        pane.pane_command.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
}

fn tab_has_pane(panes: &[PaneInfo], tab_id: u32, pred: impl Fn(&str) -> bool) -> bool {
    panes
        .iter()
        .filter(|p| p.tab_id == tab_id && !p.is_plugin)
        .any(|p| pane_commands(p).any(&pred))
}

fn left_moves(
    tabs: &[TabInfo],
    panes: &[PaneInfo],
    new_tab_id: u32,
    exe_name: &str,
    sockets: &Sockets,
) -> usize {
    let mut order: Vec<&TabInfo> = tabs.iter().collect();
    order.sort_by_key(|t| t.position);
    let Some(new_idx) = order.iter().position(|t| t.tab_id == new_tab_id) else {
        return 0;
    };
    let Some(tui_idx) = order.iter().position(|t| {
        t.tab_id != new_tab_id
            && tab_has_pane(panes, t.tab_id, |c| is_tui_command(c, exe_name, sockets))
    }) else {
        return 0;
    };
    let mut target = tui_idx + 1;
    while target < order.len()
        && order[target].tab_id != new_tab_id
        && tab_has_pane(panes, order[target].tab_id, |c| {
            is_own_runner_command(c, exe_name, sockets)
        })
    {
        target += 1;
    }
    new_idx.saturating_sub(target)
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
            .filter(|p| pane_commands(p).any(|c| is_runner_command(c, exe_name, workspace_id)))
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

fn place_next_to_tui(
    zellij: &Zellij,
    panes: &[PaneInfo],
    tab_id: u32,
    exe_name: &str,
    sockets: &Sockets,
) -> Result<(), ZellijError> {
    let tabs = zellij.list_tabs()?;
    for _ in 0..left_moves(&tabs, panes, tab_id, exe_name, sockets) {
        zellij.move_tab_left(tab_id)?;
    }
    Ok(())
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
        let exe_name = exe_file_name(&exe_path);
        let tabs = zellij.list_tabs()?;
        let panes = zellij.list_panes()?;

        match decide(&tabs, &panes, workspace_id, &exe_name) {
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
            CLOSE_PANE_FLAG.to_string(),
            workspace_id.to_string(),
        ];
        let tab_id = zellij.new_tab(workspace_id, workspace_path, &argv)?;
        let default_socket = crate::paths::default_socket_path();
        let sockets = Sockets {
            own: &self.socket_path,
            default: &default_socket,
        };
        if let Err(e) = place_next_to_tui(&zellij, &panes, tab_id, &exe_name, &sockets) {
            eprintln!("[zelloom-core] failed to move tab '{workspace_id}' next to the TUI: {e}");
        }
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

    #[test]
    fn runner_command_matching_accepts_close_pane_flag() {
        assert!(is_runner_command(
            "/opt/bin/loom --socket /tmp/s.sock runner --close-pane-on-exit ws",
            "loom",
            "ws"
        ));
        assert!(!is_runner_command(
            "/opt/bin/loom runner --other-flag ws",
            "loom",
            "ws"
        ));
    }

    #[test]
    fn tui_command_matching() {
        let sockets = Sockets {
            own: Path::new("/tmp/a b.sock"),
            default: Path::new("/tmp/a b.sock"),
        };
        assert!(is_tui_command("/opt/bin/loom", "loom", &sockets));
        assert!(is_tui_command(
            "loom --socket /tmp/a b.sock",
            "loom",
            &sockets
        ));
        assert!(!is_tui_command(
            "loom --socket /tmp/other.sock",
            "loom",
            &sockets
        ));
        assert!(!is_tui_command("loom list", "loom", &sockets));
        assert!(!is_tui_command("/opt/bin/loom runner ws", "loom", &sockets));
        assert!(!is_tui_command("/opt/bin/notloom", "loom", &sockets));
    }

    #[test]
    fn bare_tui_command_matches_only_when_core_uses_default_socket() {
        let sockets = Sockets {
            own: Path::new("/tmp/custom.sock"),
            default: Path::new("/tmp/default.sock"),
        };
        assert!(!is_tui_command("/opt/bin/loom", "loom", &sockets));
        assert!(is_tui_command(
            "/opt/bin/loom --socket /tmp/custom.sock",
            "loom",
            &sockets
        ));
    }

    fn tui_pane(tab_id: u32) -> PaneInfo {
        let mut p = pane(tab_id, Some("/bin/bash"), false);
        p.pane_command = Some("loom".to_string());
        p
    }

    fn other_runner_pane(tab_id: u32, workspace: &str) -> PaneInfo {
        pane(
            tab_id,
            Some(&format!(
                "/usr/local/bin/loom --socket /tmp/s.sock runner --close-pane-on-exit {workspace}"
            )),
            false,
        )
    }

    fn moves(tabs: &[TabInfo], panes: &[PaneInfo], new_tab_id: u32) -> usize {
        let sockets = Sockets {
            own: Path::new("/tmp/s.sock"),
            default: Path::new("/tmp/s.sock"),
        };
        left_moves(tabs, panes, new_tab_id, "loom", &sockets)
    }

    #[test]
    fn new_tab_moves_right_after_tui_tab() {
        let tabs = vec![
            tab(0, "a"),
            tab(1, "loom"),
            tab(2, "b"),
            tab(3, "c"),
            tab(4, "ws"),
        ];
        let panes = vec![tui_pane(1)];
        assert_eq!(moves(&tabs, &panes, 4), 2);
    }

    #[test]
    fn new_tab_goes_after_runner_tabs_next_to_tui() {
        let tabs = vec![
            tab(0, "loom"),
            tab(1, "x"),
            tab(2, "y"),
            tab(3, "user"),
            tab(4, "z"),
            tab(5, "ws"),
        ];
        let panes = vec![
            tui_pane(0),
            other_runner_pane(1, "x"),
            other_runner_pane(2, "y"),
            other_runner_pane(4, "z"),
        ];
        assert_eq!(moves(&tabs, &panes, 5), 2);
    }

    #[test]
    fn runner_tab_of_another_core_does_not_extend_the_group() {
        let tabs = vec![tab(0, "loom"), tab(1, "x"), tab(2, "ws")];
        let panes = vec![
            tui_pane(0),
            pane(
                1,
                Some("/usr/local/bin/loom --socket /tmp/other.sock runner x"),
                false,
            ),
        ];
        assert_eq!(moves(&tabs, &panes, 2), 1);
    }

    #[test]
    fn new_tab_stays_when_already_in_place_or_no_tui() {
        let tabs = vec![tab(0, "a"), tab(1, "loom"), tab(2, "ws")];
        assert_eq!(moves(&tabs, &[tui_pane(1)], 2), 0);
        assert_eq!(moves(&tabs, &[], 2), 0);
    }

    #[test]
    fn tab_order_follows_position_not_id() {
        let tabs = vec![
            TabInfo {
                tab_id: 7,
                name: "loom".to_string(),
                position: 0,
            },
            TabInfo {
                tab_id: 2,
                name: "b".to_string(),
                position: 1,
            },
            TabInfo {
                tab_id: 5,
                name: "ws".to_string(),
                position: 2,
            },
        ];
        assert_eq!(moves(&tabs, &[tui_pane(7)], 5), 1);
    }
}
