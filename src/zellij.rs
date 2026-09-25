use std::path::Path;
use std::process::Command;

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ZellijError {
    #[error("failed to run zellij: {0}")]
    Spawn(std::io::Error),
    #[error("zellij exited with status {status}: {stderr}")]
    Command {
        status: std::process::ExitStatus,
        stderr: String,
    },
    #[error("failed to parse zellij output: {0}")]
    Parse(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Deserialize)]
pub struct TabInfo {
    pub tab_id: u32,
    pub name: String,
    #[serde(default)]
    pub position: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PaneInfo {
    pub id: u32,
    #[serde(default)]
    pub is_plugin: bool,
    #[serde(default)]
    pub is_focused: bool,
    #[serde(default)]
    pub exited: bool,
    #[serde(default)]
    pub exit_status: Option<i32>,
    #[serde(default)]
    pub terminal_command: Option<String>,
    #[serde(default)]
    pub pane_command: Option<String>,
    #[serde(default)]
    pub pane_cwd: Option<String>,
    pub tab_id: u32,
    #[serde(default)]
    pub tab_name: Option<String>,
}

fn zellij_bin() -> String {
    std::env::var("ZELLOOM_ZELLIJ").unwrap_or_else(|_| "zellij".to_string())
}

pub struct Zellij {
    session: String,
}

impl Zellij {
    pub fn new(session: impl Into<String>) -> Zellij {
        Zellij {
            session: session.into(),
        }
    }

    fn run(&self, args: &[&str]) -> Result<String, ZellijError> {
        let output = Command::new(zellij_bin())
            .arg("--session")
            .arg(&self.session)
            .args(args)
            .output()
            .map_err(ZellijError::Spawn)?;
        if !output.status.success() {
            return Err(ZellijError::Command {
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    pub fn list_tabs(&self) -> Result<Vec<TabInfo>, ZellijError> {
        let out = self.run(&["action", "list-tabs", "-a", "-j"])?;
        Ok(serde_json::from_str(&out)?)
    }

    pub fn list_panes(&self) -> Result<Vec<PaneInfo>, ZellijError> {
        let out = self.run(&["action", "list-panes", "-a", "-j"])?;
        Ok(serde_json::from_str(&out)?)
    }

    pub fn new_tab(&self, name: &str, cwd: &Path, argv: &[String]) -> Result<(), ZellijError> {
        let mut args: Vec<String> = vec![
            "action".to_string(),
            "new-tab".to_string(),
            "--name".to_string(),
            name.to_string(),
            "--cwd".to_string(),
            cwd.to_string_lossy().into_owned(),
            "--no-focus".to_string(),
            "--".to_string(),
        ];
        args.extend(argv.iter().cloned());
        let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&args_ref)?;
        Ok(())
    }

    pub fn close_tab_by_id(&self, tab_id: u32) -> Result<(), ZellijError> {
        self.run(&["action", "close-tab-by-id", &tab_id.to_string()])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TABS: &str = r#"[
      {
        "position": 0,
        "name": "Tab #1",
        "active": false,
        "panes_to_hide": 0,
        "is_fullscreen_active": false,
        "is_sync_panes_active": false,
        "are_floating_panes_visible": false,
        "other_focused_clients": [],
        "active_swap_layout_name": null,
        "is_swap_layout_dirty": false,
        "viewport_rows": 49,
        "viewport_columns": 50,
        "display_area_rows": 50,
        "display_area_columns": 50,
        "selectable_tiled_panes_count": 1,
        "selectable_floating_panes_count": 0,
        "tab_id": 0,
        "has_bell_notification": false,
        "is_flashing_bell": false
      },
      {
        "position": 1,
        "name": "amanejp",
        "active": false,
        "panes_to_hide": 0,
        "is_fullscreen_active": false,
        "is_sync_panes_active": false,
        "are_floating_panes_visible": false,
        "other_focused_clients": [],
        "active_swap_layout_name": null,
        "is_swap_layout_dirty": false,
        "viewport_rows": 0,
        "viewport_columns": 0,
        "display_area_rows": 0,
        "display_area_columns": 0,
        "selectable_tiled_panes_count": 0,
        "selectable_floating_panes_count": 0,
        "tab_id": 1,
        "has_bell_notification": false,
        "is_flashing_bell": false
      }
    ]"#;

    const SAMPLE_PANES: &str = r#"[
      {
        "id": 0,
        "is_plugin": true,
        "is_focused": false,
        "is_fullscreen": false,
        "is_floating": false,
        "is_suppressed": false,
        "title": "zellij:compact-bar",
        "exited": false,
        "exit_status": null,
        "is_held": false,
        "pane_x": 0,
        "pane_content_x": 0,
        "pane_y": 49,
        "pane_content_y": 49,
        "pane_rows": 1,
        "pane_content_rows": 1,
        "pane_columns": 50,
        "pane_content_columns": 50,
        "cursor_coordinates_in_pane": null,
        "terminal_command": null,
        "plugin_url": "zellij:compact-bar",
        "is_selectable": false,
        "index_in_pane_group": {},
        "default_fg": null,
        "default_bg": null,
        "tab_id": 0,
        "tab_position": 0,
        "tab_name": "Tab #1"
      },
      {
        "id": 0,
        "is_plugin": false,
        "is_focused": true,
        "is_fullscreen": false,
        "is_floating": false,
        "is_suppressed": false,
        "title": "loom runner",
        "exited": true,
        "exit_status": 0,
        "is_held": true,
        "pane_x": 0,
        "pane_content_x": 0,
        "pane_y": 0,
        "pane_content_y": 0,
        "pane_rows": 49,
        "pane_content_rows": 49,
        "pane_columns": 50,
        "pane_content_columns": 50,
        "cursor_coordinates_in_pane": [42, 0],
        "terminal_command": "/usr/bin/loom --socket /run/user/1000/zelloom/default.sock runner amanejp",
        "plugin_url": null,
        "is_selectable": true,
        "index_in_pane_group": {},
        "default_fg": null,
        "default_bg": null,
        "tab_id": 1,
        "tab_position": 1,
        "tab_name": "amanejp",
        "pane_command": "/usr/bin/loom --socket /run/user/1000/zelloom/default.sock runner amanejp",
        "pane_cwd": "/home/amane/src/amanejp"
      }
    ]"#;

    #[test]
    fn parses_sample_tabs_json() {
        let tabs: Vec<TabInfo> = serde_json::from_str(SAMPLE_TABS).unwrap();
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[1].tab_id, 1);
        assert_eq!(tabs[1].name, "amanejp");
        assert_eq!(tabs[1].position, 1);
    }

    #[test]
    fn parses_sample_panes_json() {
        let panes: Vec<PaneInfo> = serde_json::from_str(SAMPLE_PANES).unwrap();
        assert_eq!(panes.len(), 2);
        assert!(panes[0].is_plugin);
        assert!(!panes[1].is_plugin);
        assert!(panes[1].exited);
        assert_eq!(panes[1].tab_id, 1);
        assert_eq!(
            panes[1].terminal_command.as_deref(),
            Some("/usr/bin/loom --socket /run/user/1000/zelloom/default.sock runner amanejp")
        );
        assert!(panes[0].terminal_command.is_none());
        assert_eq!(
            panes[1].pane_cwd.as_deref(),
            Some("/home/amane/src/amanejp")
        );
    }
}
