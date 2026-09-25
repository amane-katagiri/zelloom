use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error(
        "workspace '{workspace}' has max_parallel = {value}, but only 1 is supported for workspaces"
    )]
    InvalidWorkspaceMaxParallel { workspace: String, value: u32 },
    #[error("workspace '{workspace}' refers to unknown agent '{agent}'")]
    UnknownAgent { workspace: String, agent: String },
    #[error("default_agent '{0}' is not defined in [agents]")]
    UnknownDefaultAgent(String),
    #[error("agent '{0}' has an empty command")]
    EmptyAgentCommand(String),
    #[error("invalid workspace id '{id}': {reason}")]
    InvalidWorkspaceId { id: String, reason: &'static str },
    #[error("workspace '{0}' is not registered")]
    UnknownWorkspace(String),
    #[error("{owner} has an invalid env key {key:?}: {reason}")]
    InvalidEnvKey {
        owner: String,
        key: String,
        reason: &'static str,
    },
    #[error("{owner} has an invalid value for env key {key:?}: must not contain NUL")]
    InvalidEnvValue { owner: String, key: String },
    #[error("{0}")]
    Invalid(String),
    #[error(
        "cannot use '{id}' (from {path}) as a workspace id: {reason}; pass an explicit id: loom workspace add <ID>"
    )]
    InvalidDerivedWorkspaceId {
        id: String,
        path: PathBuf,
        reason: &'static str,
    },
    #[error("workspace '{id}' is already registered -> {path}")]
    WorkspaceAlreadyRegistered { id: String, path: String },
    #[error("config file {} does not exist; run `loom init` first", path.display())]
    MissingConfig { path: PathBuf },
    #[error("config file {} already exists; not overwriting it", path.display())]
    ConfigAlreadyExists { path: PathBuf },
    #[error("failed to edit config file {path}: {source}")]
    TomlEdit {
        path: PathBuf,
        #[source]
        source: toml_edit::TomlError,
    },
}

fn default_max_parallel() -> u32 {
    4
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct Config {
    #[serde(default)]
    pub default_agent: Option<String>,
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentConfig>,
    #[serde(default)]
    pub workspaces: BTreeMap<String, WorkspaceConfig>,
    #[serde(default)]
    pub sources: BTreeMap<String, SourceConfig>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SchedulerConfig {
    #[serde(default = "default_max_parallel")]
    pub max_parallel: u32,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        SchedulerConfig {
            max_parallel: default_max_parallel(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct AgentConfig {
    pub command: Vec<String>,
    #[serde(default)]
    pub instruction_flag: Option<String>,
    #[serde(default = "default_true")]
    pub shell: bool,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct WorkspaceConfig {
    pub path: PathBuf,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub max_parallel: Option<u32>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SourceConfig {
    #[serde(default = "default_true")]
    pub auto_queue: bool,
}

impl Default for SourceConfig {
    fn default() -> Self {
        SourceConfig { auto_queue: true }
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (name, agent) in &self.agents {
            if agent
                .command
                .first()
                .is_none_or(|program| program.is_empty())
            {
                return Err(ConfigError::EmptyAgentCommand(name.clone()));
            }
            validate_env(&format!("agent '{name}'"), &agent.env)?;
        }
        for (id, ws) in &self.workspaces {
            validate_workspace_id(id)?;
            validate_env(&format!("workspace '{id}'"), &ws.env)?;
            if let Some(max_parallel) = ws.max_parallel
                && max_parallel != 1
            {
                return Err(ConfigError::InvalidWorkspaceMaxParallel {
                    workspace: id.clone(),
                    value: max_parallel,
                });
            }
            if let Some(agent) = &ws.agent
                && !self.agents.contains_key(agent)
            {
                return Err(ConfigError::UnknownAgent {
                    workspace: id.clone(),
                    agent: agent.clone(),
                });
            }
        }
        if let Some(default_agent) = &self.default_agent
            && !self.agents.contains_key(default_agent)
        {
            return Err(ConfigError::UnknownDefaultAgent(default_agent.clone()));
        }
        Ok(())
    }

    pub fn max_parallel(&self) -> u32 {
        self.scheduler.max_parallel
    }

    pub fn resolve_agent(&self, task_agent: Option<&str>, workspace_id: &str) -> Option<String> {
        task_agent
            .map(|s| s.to_string())
            .or_else(|| {
                self.workspaces
                    .get(workspace_id)
                    .and_then(|ws| ws.agent.clone())
            })
            .or_else(|| self.default_agent.clone())
    }

    pub fn auto_queue(&self, source_type: &str) -> bool {
        self.sources
            .get(source_type)
            .map(|s| s.auto_queue)
            .unwrap_or(true)
    }
}

pub const RESERVED_ENV_PREFIX: &str = "ZELLOOM_";

fn validate_env(owner: &str, env: &BTreeMap<String, String>) -> Result<(), ConfigError> {
    for (key, value) in env {
        let reason = if key.is_empty() {
            Some("must not be empty")
        } else if key.contains('=') {
            Some("must not contain '='")
        } else if key.contains('\0') {
            Some("must not contain NUL")
        } else if key.starts_with(RESERVED_ENV_PREFIX) {
            Some("ZELLOOM_* variables are set by zelloom and cannot be configured")
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(ConfigError::InvalidEnvKey {
                owner: owner.to_string(),
                key: key.clone(),
                reason,
            });
        }
        if value.contains('\0') {
            return Err(ConfigError::InvalidEnvValue {
                owner: owner.to_string(),
                key: key.clone(),
            });
        }
    }
    Ok(())
}

pub fn validate_workspace_id(id: &str) -> Result<(), ConfigError> {
    let reason = if id.is_empty() {
        Some("must not be empty")
    } else if id.chars().any(|c| c.is_control() || c.is_whitespace()) {
        Some("must not contain whitespace or control characters")
    } else if id.contains(':') {
        Some("must not contain ':'")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(ConfigError::InvalidWorkspaceId {
            id: id.to_string(),
            reason,
        }),
        None => Ok(()),
    }
}

pub const CONFIG_TEMPLATE: &str = r#"# zelloom configuration
# Register workspaces with `loom workspace add`; they are appended as [workspaces.<id>] tables.

default_agent = "claude"

[scheduler]
max_parallel = 4

[agents.claude]
command = ["claude"]
instruction_flag = "--append-system-prompt"

[agents.codex]
command = ["codex"]
"#;

pub fn init_config(config_path: &Path) -> Result<(), ConfigError> {
    let io_err = |source| ConfigError::Io {
        path: config_path.to_path_buf(),
        source,
    };
    crate::paths::ensure_parent_dir(config_path).map_err(io_err)?;
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(config_path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(ConfigError::ConfigAlreadyExists {
                path: config_path.to_path_buf(),
            });
        }
        Err(err) => return Err(io_err(err)),
    };
    std::io::Write::write_all(&mut file, CONFIG_TEMPLATE.as_bytes()).map_err(io_err)
}

pub fn resolve_workspace_path(explicit: Option<&Path>, cwd: &Path) -> PathBuf {
    let path = match explicit {
        Some(p) => cwd.join(p),
        None => git_toplevel(cwd).unwrap_or_else(|| cwd.to_path_buf()),
    };
    canonicalize_or_self(&path)
}

pub fn workspace_id_from_path(path: &Path) -> Result<String, ConfigError> {
    let id = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    match validate_workspace_id(&id) {
        Ok(()) => Ok(id),
        Err(ConfigError::InvalidWorkspaceId { id, reason }) => {
            Err(ConfigError::InvalidDerivedWorkspaceId {
                id,
                path: path.to_path_buf(),
                reason,
            })
        }
        Err(err) => Err(err),
    }
}

pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(err) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source: err,
            });
        }
    };
    let config: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    config.validate()?;
    Ok(config)
}

pub fn add_workspace(
    config_path: &Path,
    id: &str,
    workspace_path: &Path,
    agent: Option<&str>,
) -> Result<(), ConfigError> {
    validate_workspace_id(id)?;

    let existing = match std::fs::read_to_string(config_path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && agent.is_some() => {
            return Err(ConfigError::MissingConfig {
                path: config_path.to_path_buf(),
            });
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(ConfigError::Io {
                path: config_path.to_path_buf(),
                source: err,
            });
        }
    };

    let mut doc = existing
        .parse::<toml_edit::DocumentMut>()
        .map_err(|source| ConfigError::TomlEdit {
            path: config_path.to_path_buf(),
            source,
        })?;

    if doc.get("workspaces").is_none() {
        let mut workspaces = toml_edit::Table::new();
        workspaces.set_implicit(true);
        doc["workspaces"] = toml_edit::Item::Table(workspaces);
    }
    let workspaces = doc["workspaces"]
        .as_table_mut()
        .ok_or_else(|| ConfigError::Invalid("`workspaces` is not a table".to_string()))?;

    if let Some(existing) = workspaces.get(id) {
        let path = existing
            .get("path")
            .and_then(|p| p.as_str())
            .unwrap_or("?")
            .to_string();
        return Err(ConfigError::WorkspaceAlreadyRegistered {
            id: id.to_string(),
            path,
        });
    }
    let mut ws_table = toml_edit::Table::new();

    ws_table.insert(
        "path",
        toml_edit::value(workspace_path.to_string_lossy().into_owned()),
    );
    if let Some(agent) = agent {
        ws_table.insert("agent", toml_edit::value(agent));
    }
    workspaces.insert(id, toml_edit::Item::Table(ws_table));

    let updated = doc.to_string();
    let config: Config = toml::from_str(&updated).map_err(|source| ConfigError::Parse {
        path: config_path.to_path_buf(),
        source,
    })?;
    config.validate()?;

    crate::paths::ensure_parent_dir(config_path).map_err(|source| ConfigError::Io {
        path: config_path.to_path_buf(),
        source,
    })?;
    std::fs::write(config_path, updated).map_err(|source| ConfigError::Io {
        path: config_path.to_path_buf(),
        source,
    })?;

    Ok(())
}

pub fn detect_workspace(config: &Config, cwd: &Path) -> Option<String> {
    let cwd = canonicalize_or_self(cwd);
    if let Some(id) = best_prefix_match(config, &cwd) {
        return Some(id);
    }
    if let Some(toplevel) = git_toplevel(&cwd) {
        let toplevel = canonicalize_or_self(&toplevel);
        if let Some(id) = best_prefix_match(config, &toplevel) {
            return Some(id);
        }
    }
    None
}

fn canonicalize_or_self(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn best_prefix_match(config: &Config, target: &Path) -> Option<String> {
    let target_components: Vec<_> = target.components().collect();
    let mut best: Option<(usize, String)> = None;
    for (id, ws) in &config.workspaces {
        let ws_path = canonicalize_or_self(&ws.path);
        let ws_components: Vec<_> = ws_path.components().collect();
        if ws_components.len() > target_components.len() {
            continue;
        }
        let is_prefix = ws_components
            .iter()
            .zip(target_components.iter())
            .all(|(a, b)| a == b);
        if is_prefix {
            let len = ws_components.len();
            if best
                .as_ref()
                .map(|(best_len, _)| len > *best_len)
                .unwrap_or(true)
            {
                best = Some((len, id.clone()));
            }
        }
    }
    best.map(|(_, id)| id)
}

fn git_toplevel(cwd: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(PathBuf::from(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn workspace(path: impl Into<PathBuf>, agent: Option<&str>) -> WorkspaceConfig {
        WorkspaceConfig {
            path: path.into(),
            agent: agent.map(str::to_string),
            max_parallel: None,
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn missing_config_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.toml");
        let config = load(&path).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn agent_resolution_priority() {
        let mut config = Config {
            default_agent: Some("claude".to_string()),
            ..Config::default()
        };
        config.agents.insert(
            "claude".to_string(),
            AgentConfig {
                command: vec!["claude".into()],
                instruction_flag: None,
                shell: true,
                env: BTreeMap::new(),
            },
        );
        config.agents.insert(
            "codex".to_string(),
            AgentConfig {
                command: vec!["codex".into()],
                instruction_flag: None,
                shell: true,
                env: BTreeMap::new(),
            },
        );
        config.agents.insert(
            "opus".to_string(),
            AgentConfig {
                command: vec!["claude".into(), "--model".into(), "opus".into()],
                instruction_flag: None,
                shell: true,
                env: BTreeMap::new(),
            },
        );
        config.workspaces.insert(
            "amanejp".to_string(),
            workspace("/tmp/amanejp", Some("opus")),
        );

        assert_eq!(
            config.resolve_agent(Some("codex"), "amanejp"),
            Some("codex".to_string())
        );
        assert_eq!(
            config.resolve_agent(None, "amanejp"),
            Some("opus".to_string())
        );
        assert_eq!(
            config.resolve_agent(None, "unknown-workspace"),
            Some("claude".to_string())
        );
    }

    #[test]
    fn workspace_max_parallel_other_than_one_is_error() {
        let mut config = Config::default();
        config.workspaces.insert(
            "a".to_string(),
            WorkspaceConfig {
                path: "/tmp/a".into(),
                agent: None,
                max_parallel: Some(2),
                env: BTreeMap::new(),
            },
        );
        let err = config.validate().unwrap_err();
        assert!(matches!(
            err,
            ConfigError::InvalidWorkspaceMaxParallel { .. }
        ));
    }

    #[test]
    fn workspace_max_parallel_one_is_ok() {
        let mut config = Config::default();
        config.workspaces.insert(
            "a".to_string(),
            WorkspaceConfig {
                path: "/tmp/a".into(),
                agent: None,
                max_parallel: Some(1),
                env: BTreeMap::new(),
            },
        );
        config.validate().unwrap();
    }

    #[test]
    fn workspace_unknown_agent_is_error() {
        let mut config = Config::default();
        config
            .workspaces
            .insert("a".to_string(), workspace("/tmp/a", Some("ghost")));
        let err = config.validate().unwrap_err();
        assert!(matches!(err, ConfigError::UnknownAgent { .. }));
    }

    #[test]
    fn auto_queue_defaults_to_true() {
        let config = Config::default();
        assert!(config.auto_queue("cli"));
        assert!(config.auto_queue("nostr"));
    }

    #[test]
    fn auto_queue_respects_explicit_false() {
        let mut config = Config::default();
        config
            .sources
            .insert("nostr".to_string(), SourceConfig { auto_queue: false });
        assert!(!config.auto_queue("nostr"));
        assert!(config.auto_queue("cli"));
    }

    #[test]
    fn detect_workspace_longest_prefix_match() {
        let mut config = Config::default();
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path().join("src");
        let inner = outer.join("amanejp");
        let sub = inner.join("crates").join("core");
        std::fs::create_dir_all(&sub).unwrap();

        config
            .workspaces
            .insert("outer".to_string(), workspace(&outer, None));
        config
            .workspaces
            .insert("inner".to_string(), workspace(&inner, None));

        let detected = detect_workspace(&config, &sub);
        assert_eq!(detected, Some("inner".to_string()));
    }

    #[test]
    fn detect_workspace_no_match_returns_none() {
        let config = Config::default();
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(detect_workspace(&config, dir.path()), None);
    }

    #[test]
    fn add_workspace_preserves_existing_formatting() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let mut file = std::fs::File::create(&config_path).unwrap();
        writeln!(
            file,
            "# a hand-written comment\ndefault_agent = \"claude\"\n\n[agents.claude]\ncommand = [\"claude\"]\n"
        )
        .unwrap();
        drop(file);

        add_workspace(
            &config_path,
            "amanejp",
            Path::new("/home/amane/src/amanejp"),
            Some("claude"),
        )
        .unwrap();

        let contents = std::fs::read_to_string(&config_path).unwrap();
        assert!(contents.contains("# a hand-written comment"));
        assert!(contents.contains("default_agent = \"claude\""));
        assert!(contents.contains("[workspaces.amanejp]"));
        assert!(contents.contains("path = \"/home/amane/src/amanejp\""));
        assert!(contents.contains("agent = \"claude\""));

        let config = load(&config_path).unwrap();
        assert_eq!(
            config.workspaces.get("amanejp").unwrap().path,
            PathBuf::from("/home/amane/src/amanejp")
        );
    }

    #[test]
    fn add_workspace_on_missing_file_creates_it() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("nested").join("config.toml");
        add_workspace(
            &config_path,
            "swing",
            Path::new("/home/amane/src/swing"),
            None,
        )
        .unwrap();
        let config = load(&config_path).unwrap();
        assert_eq!(
            config.workspaces.get("swing").unwrap().path,
            PathBuf::from("/home/amane/src/swing")
        );
        assert_eq!(config.workspaces.get("swing").unwrap().agent, None);
    }

    #[test]
    fn agent_with_empty_command_is_error() {
        let mut config = Config::default();
        config.agents.insert(
            "broken".to_string(),
            AgentConfig {
                command: vec![],
                instruction_flag: None,
                shell: true,
                env: BTreeMap::new(),
            },
        );
        let err = config.validate().unwrap_err();
        assert!(matches!(err, ConfigError::EmptyAgentCommand(name) if name == "broken"));

        config.agents.insert(
            "broken".to_string(),
            AgentConfig {
                command: vec![String::new()],
                instruction_flag: None,
                shell: true,
                env: BTreeMap::new(),
            },
        );
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::EmptyAgentCommand(_)
        ));
    }

    #[test]
    fn workspace_id_validation() {
        for bad in ["", "my ws", "tab\tname", "a:b", "x\u{7}"] {
            assert!(
                matches!(
                    validate_workspace_id(bad),
                    Err(ConfigError::InvalidWorkspaceId { .. })
                ),
                "{bad:?} should be rejected"
            );
        }
        for good in ["myapp", "my-app_2", "ワークスペース", "loom", "loom2"] {
            validate_workspace_id(good).unwrap();
        }
    }

    #[test]
    fn add_workspace_rejects_unknown_agent_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let original = "[agents.claude]\ncommand = [\"claude\"]\n";
        std::fs::write(&config_path, original).unwrap();

        let err = add_workspace(
            &config_path,
            "myapp",
            Path::new("/tmp/myapp"),
            Some("ghost"),
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::UnknownAgent { .. }), "{err}");
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original);

        add_workspace(
            &config_path,
            "myapp",
            Path::new("/tmp/myapp"),
            Some("claude"),
        )
        .unwrap();
        assert_eq!(
            load(&config_path).unwrap().workspaces["myapp"]
                .agent
                .as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn add_workspace_rejects_invalid_id_without_creating_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("nested").join("config.toml");
        let err = add_workspace(&config_path, "a:b", Path::new("/tmp/ab"), None).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidWorkspaceId { .. }));
        assert!(!config_path.exists());
        assert!(!config_path.parent().unwrap().exists());
    }

    #[test]
    fn loom_is_a_valid_workspace_id() {
        assert_eq!(
            workspace_id_from_path(Path::new("/tmp/loom")).unwrap(),
            "loom"
        );
    }

    #[test]
    fn add_workspace_accepts_loom_as_id() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        add_workspace(&config_path, "loom", Path::new("/tmp/loom"), None).unwrap();
        assert_eq!(
            load(&config_path).unwrap().workspaces["loom"].path,
            Path::new("/tmp/loom")
        );
    }

    fn git_init(dir: &Path) {
        let status = std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(dir)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn resolve_workspace_path_uses_git_toplevel_from_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("myrepo");
        let sub = repo.join("src").join("deep");
        std::fs::create_dir_all(&sub).unwrap();
        git_init(&repo);

        let resolved = resolve_workspace_path(None, &sub);
        assert_eq!(resolved, repo.canonicalize().unwrap());
        assert_eq!(workspace_id_from_path(&resolved).unwrap(), "myrepo");
    }

    #[test]
    fn resolve_workspace_path_without_git_uses_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain-dir");
        std::fs::create_dir_all(&plain).unwrap();

        let resolved = resolve_workspace_path(None, &plain);
        assert_eq!(resolved, plain.canonicalize().unwrap());
        assert_eq!(workspace_id_from_path(&resolved).unwrap(), "plain-dir");
    }

    #[test]
    fn resolve_workspace_path_explicit_path_overrides_git() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let sub = repo.join("sub");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        git_init(&repo);

        assert_eq!(
            resolve_workspace_path(Some(&other), &sub),
            other.canonicalize().unwrap()
        );
        assert_eq!(
            resolve_workspace_path(Some(Path::new("..")), &sub),
            repo.canonicalize().unwrap()
        );
    }

    #[test]
    fn derived_workspace_id_invalid_is_error() {
        for path in ["/", "/tmp/my dir", "/tmp/a:b"] {
            let err = workspace_id_from_path(Path::new(path)).unwrap_err();
            assert!(
                matches!(err, ConfigError::InvalidDerivedWorkspaceId { .. }),
                "{path}: {err}"
            );
            assert!(err.to_string().contains("loom workspace add <ID>"));
        }
    }

    #[test]
    fn add_workspace_duplicate_id_is_error_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        add_workspace(&config_path, "myapp", Path::new("/tmp/myapp"), None).unwrap();
        let before = std::fs::read_to_string(&config_path).unwrap();

        let err =
            add_workspace(&config_path, "myapp", Path::new("/tmp/elsewhere"), None).unwrap_err();
        assert!(
            matches!(&err, ConfigError::WorkspaceAlreadyRegistered { id, path } if id == "myapp" && path == "/tmp/myapp"),
            "{err}"
        );
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), before);
    }

    #[test]
    fn add_workspace_with_agent_on_missing_config_points_to_init() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let err = add_workspace(
            &config_path,
            "myapp",
            Path::new("/tmp/myapp"),
            Some("claude"),
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::MissingConfig { .. }), "{err}");
        assert!(err.to_string().contains("loom init"));
        assert!(!config_path.exists());
    }

    #[test]
    fn init_config_writes_valid_template() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("nested").join("config.toml");
        init_config(&config_path).unwrap();

        let config = load(&config_path).unwrap();
        assert_eq!(config.default_agent.as_deref(), Some("claude"));
        assert_eq!(config.max_parallel(), 4);
        assert_eq!(
            config.agents["claude"].instruction_flag.as_deref(),
            Some("--append-system-prompt")
        );
        assert_eq!(config.agents["codex"].command, vec!["codex".to_string()]);
        assert!(config.workspaces.is_empty());

        add_workspace(
            &config_path,
            "myapp",
            Path::new("/tmp/myapp"),
            Some("codex"),
        )
        .unwrap();
        assert_eq!(
            load(&config_path).unwrap().workspaces["myapp"]
                .agent
                .as_deref(),
            Some("codex")
        );
    }

    #[test]
    fn init_config_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "# mine\n").unwrap();

        let err = init_config(&config_path).unwrap_err();
        assert!(
            matches!(err, ConfigError::ConfigAlreadyExists { .. }),
            "{err}"
        );
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), "# mine\n");
    }

    fn parse(text: &str) -> Result<Config, ConfigError> {
        let config: Config = toml::from_str(text).unwrap();
        config.validate().map(|()| config)
    }

    #[test]
    fn agent_shell_defaults_to_true_and_env_to_empty() {
        let config = parse(
            "[agents.a]\ncommand = [\"a\"]\n\n[agents.b]\ncommand = [\"b\"]\nshell = false\n\n[workspaces.w]\npath = \"/tmp/w\"\n",
        )
        .unwrap();
        assert!(config.agents["a"].shell);
        assert!(!config.agents["b"].shell);
        assert!(config.agents["a"].env.is_empty());
        assert!(config.workspaces["w"].env.is_empty());
    }

    #[test]
    fn env_tables_are_parsed_as_literal_strings() {
        let config = parse(
            r#"
[agents.a]
command = ["a"]
env = { FOO = "$HOME/x", "WITH SPACE" = "a b" }

[workspaces.w]
path = "/tmp/w"
env = { BAR = "日本語\n" }
"#,
        )
        .unwrap();
        assert_eq!(config.agents["a"].env["FOO"], "$HOME/x");
        assert_eq!(config.agents["a"].env["WITH SPACE"], "a b");
        assert_eq!(config.workspaces["w"].env["BAR"], "日本語\n");
    }

    #[test]
    fn env_rejects_reserved_and_invalid_keys() {
        for (table, key) in [
            ("agents.a", "ZELLOOM_TASK_ID"),
            ("agents.a", "ZELLOOM_ANYTHING"),
            ("workspaces.w", "ZELLOOM_SOCKET"),
            ("agents.a", ""),
            ("workspaces.w", "A=B"),
            ("agents.a", "NUL\\u0000"),
        ] {
            let text = format!(
                "[agents.a]\ncommand = [\"a\"]\n\n[workspaces.w]\npath = \"/tmp/w\"\n\n[{table}.env]\n\"{key}\" = \"v\"\n"
            );
            let err = parse(&text).unwrap_err();
            assert!(
                matches!(err, ConfigError::InvalidEnvKey { .. }),
                "{table} {key:?}: {err}"
            );
            if key.starts_with("ZELLOOM_") {
                assert!(err.to_string().contains("ZELLOOM_"), "{err}");
            }
        }
    }

    #[test]
    fn env_rejects_nul_in_value() {
        let err =
            parse("[agents.a]\ncommand = [\"a\"]\nenv = { K = \"a\\u0000b\" }\n").unwrap_err();
        assert!(matches!(err, ConfigError::InvalidEnvValue { .. }), "{err}");
    }
}
