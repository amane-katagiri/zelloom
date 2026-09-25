use std::collections::BTreeMap;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zelloom::client::call;
use zelloom::core::{CoreOptions, NoopLauncher, run as core_run};
use zelloom::protocol::{Request, Source};

const TASK_TEXT: &str =
    "fix \"quotes\" and 'single' $HOME `x` $(echo no)\n二行目 * ; exec \"$@\" \\";

const FAKE_AGENT: &str = r#"#!/bin/sh
out="$FAKE_OUT/$ZELLOOM_TASK_ID"
mkdir -p "$out"
printf '%s\0' "$@" > "$out/args"
env -0 > "$out/env"
pwd > "$out/cwd"
echo $$ > "$out/pid.tmp"
mv "$out/pid.tmp" "$out/pid"
exec sleep 1000
"#;

static NEXT_ID: AtomicU32 = AtomicU32::new(0);

fn find_program(name: &str) -> Option<PathBuf> {
    ["/bin", "/usr/bin", "/usr/local/bin"]
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|path| path.exists())
}

struct Harness {
    _dir: tempfile::TempDir,
    home: PathBuf,
    out: PathBuf,
    workspace: PathBuf,
    socket: PathBuf,
    core: Option<std::thread::JoinHandle<()>>,
    runner: Option<Child>,
    pty_output: Arc<Mutex<Vec<u8>>>,
    agent_pids: Vec<i32>,
}

impl Harness {
    fn new(shell_enabled: bool) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let rcbin = home.join("rcbin");
        let out = dir.path().join("out");
        let workspace = dir.path().join("ws");
        for d in [&rcbin, &out, &workspace, &home.join(".config/fish")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let agent_path = rcbin.join("fake-agent");
        std::fs::write(&agent_path, FAKE_AGENT).unwrap();
        std::fs::set_permissions(
            &agent_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let posix_rc = "export LOOM_RC_MARKER=rc-loaded\nexport PATH=\"$HOME/rcbin:$PATH\"\nexport RC_OVERRIDE=rc\n";
        std::fs::write(home.join(".bashrc"), posix_rc).unwrap();
        std::fs::write(home.join(".zshrc"), posix_rc).unwrap();
        std::fs::write(home.join(".shrc"), posix_rc).unwrap();
        std::fs::write(
            home.join(".config/fish/config.fish"),
            "set -gx LOOM_RC_MARKER rc-loaded\nset -gx PATH $HOME/rcbin $PATH\nset -gx RC_OVERRIDE rc\n",
        )
        .unwrap();

        let command = if shell_enabled {
            "fake-agent".to_string()
        } else {
            agent_path.display().to_string()
        };
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                r#"
default_agent = "fake"

[agents.fake]
command = [{command:?}, "--flag"]
instruction_args = ["--append-system-prompt", "{{instruction}}"]
shell = {shell_enabled}
env = {{ FAKE_OUT = {out:?}, AGENT_ONLY = "agent", BOTH = "agent", LITERAL = "$HOME x", RC_OVERRIDE = "config" }}

[workspaces.a]
path = {workspace:?}
env = {{ BOTH = "workspace" }}
"#,
                out = out.display().to_string(),
                workspace = workspace.display().to_string(),
            ),
        )
        .unwrap();

        let socket = PathBuf::from(format!(
            "/tmp/zl-sh-{}-{}.sock",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_file(&socket);
        let options = CoreOptions {
            socket_path: socket.clone(),
            config_path,
            db_path: dir.path().join("state.db"),
            launcher: Arc::new(NoopLauncher),
        };
        let core = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                core_run(options).await.unwrap();
            });
        });
        wait_until("core socket", Duration::from_secs(10), || {
            std::os::unix::net::UnixStream::connect(&socket).is_ok()
        });

        Harness {
            _dir: dir,
            home,
            out,
            workspace,
            socket,
            core: Some(core),
            runner: None,
            pty_output: Arc::new(Mutex::new(Vec::new())),
            agent_pids: Vec::new(),
        }
    }

    fn start_runner(&mut self, shell: &Path) {
        let pty = nix::pty::openpty(None, None).unwrap();
        let slave: OwnedFd = pty.slave;
        let mut command = Command::new(env!("CARGO_BIN_EXE_loom"));
        command
            .arg("--socket")
            .arg(&self.socket)
            .arg("runner")
            .arg("a")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("ENV", self.home.join(".shrc"))
            .env("SHELL", shell)
            .env("TERM", "xterm")
            .env("ZELLOOM_ZELLIJ", "/nonexistent")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        self.runner = Some(command.spawn().unwrap());

        let output = self.pty_output.clone();
        let mut master = std::fs::File::from(pty.master);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = master.read(&mut buf) {
                if n == 0 {
                    break;
                }
                output.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });

        wait_until("runner banner", Duration::from_secs(10), || {
            String::from_utf8_lossy(&self.pty_output.lock().unwrap()).contains("waiting for tasks")
        });
    }

    fn pty_text(&self) -> String {
        String::from_utf8_lossy(&self.pty_output.lock().unwrap()).into_owned()
    }

    fn enqueue(&self) -> String {
        let task = call(
            &self.socket,
            &Request::Enqueue {
                text: TASK_TEXT.to_string(),
                workspace: "a".to_string(),
                agent: None,
                source: Source::cli(),
                reply_to: None,
                metadata: serde_json::json!({}),
            },
        )
        .unwrap();
        task["id"].as_str().unwrap().to_string()
    }

    fn wait_agent(&mut self, task_id: &str) -> AgentRecord {
        let dir = self.out.join(task_id);
        let pid_file = dir.join("pid");
        let deadline = Instant::now() + Duration::from_secs(15);
        while !pid_file.exists() {
            if Instant::now() > deadline {
                panic!(
                    "agent for {task_id} did not start; stopped processes: {:?}\npty output:\n{}",
                    stopped_descendants(),
                    self.pty_text()
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        self.agent_pids.push(pid);
        let args = std::fs::read(dir.join("args")).unwrap();
        let args: Vec<String> = args
            .split(|b| *b == 0)
            .map(|a| String::from_utf8(a.to_vec()).unwrap())
            .collect();
        let env: BTreeMap<String, String> = std::fs::read(dir.join("env"))
            .unwrap()
            .split(|b| *b == 0)
            .filter(|e| !e.is_empty())
            .filter_map(|e| {
                let e = String::from_utf8_lossy(e).into_owned();
                e.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect();
        let cwd = std::fs::read_to_string(dir.join("cwd")).unwrap();
        AgentRecord {
            pid,
            args: args[..args.len() - 1].to_vec(),
            env,
            cwd: cwd.trim_end().to_string(),
        }
    }

    fn loom_done(&self, env: &BTreeMap<String, String>) {
        let status = Command::new(env!("CARGO_BIN_EXE_loom"))
            .arg("done")
            .env_clear()
            .env("ZELLOOM_TASK_ID", &env["ZELLOOM_TASK_ID"])
            .env("ZELLOOM_SOCKET", &env["ZELLOOM_SOCKET"])
            .env("ZELLOOM_ZELLIJ", "/nonexistent")
            .env("HOME", &self.home)
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn finish(mut self) {
        call(&self.socket, &Request::Shutdown { force: true }).unwrap();
        self.core.take().unwrap().join().unwrap();
        let mut runner = self.runner.take().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if runner.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() > deadline {
                let _ = runner.kill();
                let _ = runner.wait();
                panic!("runner did not exit after core shutdown");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        for pid in &self.agent_pids {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
                libc::kill(*pid, libc::SIGKILL);
            }
        }
        if let Some(mut runner) = self.runner.take() {
            kill_session(runner.id() as i32);
            let _ = runner.kill();
            let _ = runner.wait();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

struct AgentRecord {
    pid: i32,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: String,
}

struct ProcStat {
    state: char,
    pgrp: i32,
    session: i32,
    tpgid: i32,
}

fn proc_stat(pid: i32) -> Option<ProcStat> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &text[text.rfind(')')? + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    Some(ProcStat {
        state: fields[0].chars().next()?,
        pgrp: fields[2].parse().ok()?,
        session: fields[3].parse().ok()?,
        tpgid: fields[5].parse().ok()?,
    })
}

fn kill_session(sid: i32) {
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if pid != sid && proc_stat(pid).is_some_and(|stat| stat.session == sid) {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

fn stopped_descendants() -> Vec<(i32, String)> {
    let mut stopped = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if let Some(stat) = proc_stat(pid)
            && stat.state == 'T'
        {
            let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
            stopped.push((pid, comm.trim().to_string()));
        }
    }
    stopped
}

fn wait_until(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !cond() {
        if Instant::now() > deadline {
            panic!("timed out waiting for {what}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn assert_agent(harness: &Harness, agent: &AgentRecord, task_id: &str, expect_rc: bool) {
    let stat = proc_stat(agent.pid).expect("agent process is alive");
    assert_ne!(stat.state, 'T', "agent is stopped");
    assert_eq!(stat.pgrp, agent.pid, "agent leads its own process group");
    assert_eq!(stat.tpgid, agent.pid, "agent is the terminal foreground");
    for fd in 0..3 {
        let target = std::fs::read_link(format!("/proc/{}/fd/{fd}", agent.pid)).unwrap();
        assert!(target.starts_with("/dev/pts/"), "fd {fd} -> {target:?}");
    }

    assert_eq!(agent.args.len(), 4, "{:?}", agent.args);
    assert_eq!(agent.args[0], "--flag");
    assert_eq!(agent.args[1], "--append-system-prompt");
    assert!(agent.args[2].starts_with("You are running inside zelloom.\n"));
    assert!(agent.args[2].ends_with(" done\n"));
    assert_eq!(agent.args[3], TASK_TEXT);

    assert_eq!(
        agent.cwd,
        harness
            .workspace
            .canonicalize()
            .unwrap()
            .display()
            .to_string()
    );
    assert_eq!(agent.env["AGENT_ONLY"], "agent");
    assert_eq!(agent.env["BOTH"], "workspace");
    assert_eq!(agent.env["LITERAL"], "$HOME x");
    assert_eq!(agent.env["ZELLOOM_TASK_ID"], task_id);
    assert_eq!(agent.env["ZELLOOM_WORKSPACE"], "a");
    assert_eq!(
        agent.env["ZELLOOM_SOCKET"],
        harness.socket.display().to_string()
    );
    if expect_rc {
        assert_eq!(agent.env["LOOM_RC_MARKER"], "rc-loaded");
        assert_eq!(agent.env["RC_OVERRIDE"], "rc");
    } else {
        assert_eq!(agent.env.get("LOOM_RC_MARKER"), None);
        assert_eq!(agent.env["RC_OVERRIDE"], "config");
    }
}

fn run_scenario(shell: &Path, shell_enabled: bool, expect_rc: bool) {
    let mut harness = Harness::new(shell_enabled);
    harness.start_runner(shell);

    let mut previous_pid = None;
    for _ in 0..2 {
        let task_id = harness.enqueue();
        let agent = harness.wait_agent(&task_id);
        assert_ne!(Some(agent.pid), previous_pid);
        std::thread::sleep(Duration::from_millis(200));
        assert_agent(&harness, &agent, &task_id, expect_rc);

        harness.loom_done(&agent.env);
        wait_until("agent to be killed", Duration::from_secs(10), || {
            proc_stat(agent.pid).is_none()
        });
        previous_pid = Some(agent.pid);
    }
    wait_until("runner to go idle", Duration::from_secs(10), || {
        harness.pty_text().matches("waiting for tasks").count() >= 3
    });
    harness.finish();
}

fn run_with_shell(name: &str) {
    let Some(shell) = find_program(name) else {
        eprintln!("skipping: {name} is not installed");
        return;
    };
    run_scenario(&shell, true, true);
}

#[test]
fn bash_agent_runs_through_interactive_shell() {
    run_with_shell("bash");
}

#[test]
fn sh_agent_runs_through_interactive_shell() {
    run_with_shell("sh");
}

#[test]
fn zsh_agent_runs_through_interactive_shell() {
    run_with_shell("zsh");
}

#[test]
fn fish_agent_runs_through_interactive_shell() {
    run_with_shell("fish");
}

#[test]
fn shell_false_runs_agent_directly() {
    run_scenario(&find_program("bash").unwrap(), false, false);
}
