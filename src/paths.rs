use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static SOCKET_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

pub fn set_socket_override(path: PathBuf) {
    let _ = SOCKET_OVERRIDE.set(path);
}

pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("ZELLOOM_CONFIG") {
        return PathBuf::from(p);
    }
    xdg_config_home().join("zelloom").join("config.toml")
}

pub fn state_dir() -> PathBuf {
    if let Ok(p) = std::env::var("ZELLOOM_STATE_DIR") {
        return PathBuf::from(p);
    }
    xdg_state_home().join("zelloom")
}

pub fn state_db_path() -> PathBuf {
    state_dir().join("state.db")
}

pub fn log_dir() -> PathBuf {
    state_dir()
}

pub fn socket_path() -> PathBuf {
    match SOCKET_OVERRIDE.get() {
        Some(p) => p.clone(),
        None => default_socket_path(),
    }
}

pub fn default_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("ZELLOOM_SOCKET") {
        return PathBuf::from(p);
    }
    runtime_base_dir().join("default.sock")
}

fn xdg_config_home() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".config"))
}

fn xdg_state_home() -> PathBuf {
    std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".local").join("state"))
}

fn runtime_base_dir() -> PathBuf {
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(p) => PathBuf::from(p).join("zelloom"),
        Err(_) => PathBuf::from(format!("/tmp/zelloom-{}", current_uid())),
    }
}

fn current_uid() -> u32 {
    nix::unistd::Uid::current().as_raw()
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

pub fn ensure_parent_dir(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_env() {
        for var in [
            "ZELLOOM_CONFIG",
            "ZELLOOM_STATE_DIR",
            "ZELLOOM_SOCKET",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_RUNTIME_DIR",
        ] {
            unsafe { std::env::remove_var(var) };
        }
    }

    #[test]
    fn env_overrides_win() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        unsafe {
            std::env::set_var("ZELLOOM_CONFIG", "/tmp/explicit-config.toml");
            std::env::set_var("ZELLOOM_STATE_DIR", "/tmp/explicit-state");
            std::env::set_var("ZELLOOM_SOCKET", "/tmp/explicit.sock");
        }
        assert_eq!(config_path(), PathBuf::from("/tmp/explicit-config.toml"));
        assert_eq!(state_dir(), PathBuf::from("/tmp/explicit-state"));
        assert_eq!(socket_path(), PathBuf::from("/tmp/explicit.sock"));
        clear_env();
    }

    #[test]
    fn xdg_fallbacks_used_when_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        unsafe {
            std::env::set_var("HOME", "/home/testuser");
        }
        assert_eq!(
            config_path(),
            PathBuf::from("/home/testuser/.config/zelloom/config.toml")
        );
        assert_eq!(
            state_dir(),
            PathBuf::from("/home/testuser/.local/state/zelloom")
        );
        clear_env();
    }

    #[test]
    fn runtime_dir_missing_falls_back_to_tmp_uid() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let expected = PathBuf::from(format!("/tmp/zelloom-{}/default.sock", current_uid()));
        assert_eq!(socket_path(), expected);
        clear_env();
    }
}
