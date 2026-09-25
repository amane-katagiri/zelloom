use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{self, SigHandler, Signal};
use nix::sys::termios::{self, SetArg, Termios};
use nix::unistd::{self, Pid};

use crate::protocol::{Outcome, Request, ResolvedAgent, RunnerEvent, ServerMessage, Task};

static TERM_PIPE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn handle_term_signal(_: libc::c_int) {
    let fd = TERM_PIPE_WRITE_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        let byte = [1u8];
        unsafe {
            libc::write(fd, byte.as_ptr().cast(), 1);
        }
    }
}

// Rare, low-volume internal messages, not a hot path; not worth boxing the `Server` variant to shrink the others.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum RunnerMsg {
    Server(ServerMessage),
    ServerClosed,
    ChildExited(std::io::Result<ExitStatus>),
    TermSignal,
}

// Core can send `Start` for the next task right after `Stop` for this one, so a wait loop watching one task's outcome must be able to push an unrelated message back instead of dropping it.
struct EventStream {
    rx: mpsc::Receiver<RunnerMsg>,
    pending: std::collections::VecDeque<RunnerMsg>,
}

impl EventStream {
    fn new(rx: mpsc::Receiver<RunnerMsg>) -> EventStream {
        EventStream {
            rx,
            pending: std::collections::VecDeque::new(),
        }
    }

    fn push_back(&mut self, msg: RunnerMsg) {
        self.pending.push_back(msg);
    }

    fn recv(&mut self) -> Result<RunnerMsg, mpsc::RecvError> {
        match self.pending.pop_front() {
            Some(msg) => Ok(msg),
            None => self.rx.recv(),
        }
    }

    fn recv_timeout(&mut self, timeout: Duration) -> Result<RunnerMsg, mpsc::RecvTimeoutError> {
        match self.pending.pop_front() {
            Some(msg) => Ok(msg),
            None => self.rx.recv_timeout(timeout),
        }
    }

    fn try_recv(&mut self) -> Result<RunnerMsg, mpsc::TryRecvError> {
        match self.pending.pop_front() {
            Some(msg) => Ok(msg),
            None => self.rx.try_recv(),
        }
    }
}

pub fn run(workspace: String) -> anyhow::Result<()> {
    ignore_job_control_signals()?;

    let (tx, rx) = mpsc::channel::<RunnerMsg>();
    let mut rx = EventStream::new(rx);

    let (term_read, term_write) = unistd::pipe()?;
    TERM_PIPE_WRITE_FD.store(term_write.as_raw_fd(), Ordering::SeqCst);
    std::mem::forget(term_write);
    install_term_handlers()?;
    spawn_term_signal_thread(term_read, tx.clone());

    let socket_path = crate::paths::socket_path();
    let stream = UnixStream::connect(&socket_path).map_err(|e| {
        anyhow::anyhow!(
            "failed to connect to loom core at {}: {e}",
            socket_path.display()
        )
    })?;
    let reader_stream = stream.try_clone()?;
    let mut writer = stream;
    spawn_socket_reader(reader_stream, tx.clone());

    send_request(
        &mut writer,
        &Request::RunnerAttach {
            workspace: workspace.clone(),
        },
    )?;
    match rx.recv() {
        Ok(RunnerMsg::Server(ServerMessage::Response(resp))) if resp.ok => {}
        Ok(RunnerMsg::Server(ServerMessage::Response(resp))) => {
            anyhow::bail!("runner_attach rejected: {}", resp.error.unwrap_or_default());
        }
        Ok(RunnerMsg::TermSignal) => return Ok(()),
        _ => anyhow::bail!("core closed the connection before acking runner_attach"),
    }

    print_idle_banner(&workspace);

    loop {
        match rx.recv() {
            Ok(RunnerMsg::Server(ServerMessage::Event(RunnerEvent::Start { task, agent }))) => {
                if run_task(task, agent, &mut writer, &mut rx, &tx) {
                    print_idle_banner(&workspace);
                } else {
                    return Ok(());
                }
            }
            Ok(RunnerMsg::ServerClosed) => {
                eprintln!("[zelloom-runner] connection to loom core lost");
                return Ok(());
            }
            Ok(RunnerMsg::TermSignal) => return Ok(()),
            Ok(RunnerMsg::Server(ServerMessage::Response(resp))) if !resp.ok => {
                eprintln!(
                    "[zelloom-runner] core rejected a request: {}",
                    resp.error.unwrap_or_default()
                );
            }
            Ok(_) => {}
            Err(_) => return Ok(()),
        }
    }
}

fn ignore_job_control_signals() -> anyhow::Result<()> {
    for sig in [
        Signal::SIGTTOU,
        Signal::SIGTTIN,
        Signal::SIGINT,
        Signal::SIGQUIT,
        Signal::SIGTSTP,
    ] {
        unsafe { signal::signal(sig, SigHandler::SigIgn) }?;
    }
    Ok(())
}

fn install_term_handlers() -> anyhow::Result<()> {
    unsafe {
        signal::signal(Signal::SIGHUP, SigHandler::Handler(handle_term_signal))?;
        signal::signal(Signal::SIGTERM, SigHandler::Handler(handle_term_signal))?;
    }
    Ok(())
}

fn spawn_term_signal_thread(read_fd: OwnedFd, tx: mpsc::Sender<RunnerMsg>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 1];
        loop {
            match unistd::read(&read_fd, &mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    if tx.send(RunnerMsg::TermSignal).is_err() {
                        break;
                    }
                }
                Err(nix::errno::Errno::EINTR) => continue,
                Err(_) => break,
            }
        }
    });
}

fn spawn_socket_reader(stream: UnixStream, tx: mpsc::Sender<RunnerMsg>) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = tx.send(RunnerMsg::ServerClosed);
                    break;
                }
                Ok(_) => match serde_json::from_str::<ServerMessage>(line.trim_end()) {
                    Ok(msg) => {
                        if tx.send(RunnerMsg::Server(msg)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("[zelloom-runner] failed to parse server message: {e}");
                    }
                },
                Err(e) => {
                    eprintln!("[zelloom-runner] socket read error: {e}");
                    let _ = tx.send(RunnerMsg::ServerClosed);
                    break;
                }
            }
        }
    });
}

fn send_request(writer: &mut UnixStream, request: &Request) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    writer.flush()?;
    Ok(())
}

fn print_idle_banner(workspace: &str) {
    println!("\nzelloom runner: {workspace} — waiting for tasks\n");
}

fn open_tty() -> anyhow::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|e| anyhow::anyhow!("failed to open controlling terminal: {e}"))
}

const FALLBACK_SHELL: &str = "/bin/sh";

fn shell_argv(shell: Option<&OsStr>, argv: &[String]) -> Vec<OsString> {
    let shell = match shell {
        Some(shell) if !shell.is_empty() => shell.to_os_string(),
        _ => OsString::from(FALLBACK_SHELL),
    };
    let is_fish = Path::new(&shell)
        .file_name()
        .is_some_and(|name| name == "fish");
    let mut wrapped = vec![shell, OsString::from("-i"), OsString::from("-c")];
    if is_fish {
        wrapped.push(OsString::from("exec $argv"));
    } else {
        wrapped.push(OsString::from("exec \"$@\""));
        wrapped.push(OsString::from("loom-agent"));
    }
    wrapped.extend(argv.iter().map(OsString::from));
    wrapped
}

fn command_argv(agent: &ResolvedAgent, shell: Option<&OsStr>) -> anyhow::Result<Vec<OsString>> {
    if agent.argv.is_empty() {
        anyhow::bail!("agent argv is empty");
    }
    Ok(if agent.shell {
        shell_argv(shell, &agent.argv)
    } else {
        agent.argv.iter().map(OsString::from).collect()
    })
}

fn build_command(agent: &ResolvedAgent, argv: &[OsString], tty: &File) -> Command {
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    command.current_dir(&agent.cwd);
    for (key, value) in &agent.env {
        command.env(key, value);
    }
    let tty_fd = tty.as_raw_fd();
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // An interactive shell stops itself with SIGTTIN if it starts before the parent's tcsetpgrp, so take the foreground here while SIGTTOU is still ignored.
            libc::tcsetpgrp(tty_fd, libc::getpid());
            for sig in [
                libc::SIGTTOU,
                libc::SIGTTIN,
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTSTP,
            ] {
                libc::signal(sig, libc::SIG_DFL);
            }
            let mut empty: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut empty);
            libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
            Ok(())
        });
    }
    command
}

fn write_escape(tty: &File, seq: &str) {
    let mut w = tty;
    let _ = w.write_all(seq.as_bytes());
    let _ = w.flush();
}

const LEAVE_TERMINAL_MODES: &str = "\x1b[?1049l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[<u\x1b[0m";

fn restore_terminal(tty: &File, saved: Option<&Termios>, runner_pgid: Pid) {
    if let Some(termios) = saved {
        let _ = termios::tcsetattr(tty, SetArg::TCSANOW, termios);
    }
    let _ = unistd::tcsetpgrp(tty, runner_pgid);
    write_escape(tty, LEAVE_TERMINAL_MODES);
}

fn spawn_agent(agent: &ResolvedAgent) -> anyhow::Result<(File, Option<Termios>, Child)> {
    let argv = command_argv(agent, std::env::var_os("SHELL").as_deref())?;
    let tty = open_tty()?;
    let saved_termios = termios::tcgetattr(&tty).ok();
    let child = build_command(agent, &argv, &tty).spawn()?;
    Ok((tty, saved_termios, child))
}

fn report_failed(writer: &mut UnixStream, task_id: &str) {
    if let Err(e) = send_request(
        writer,
        &Request::AgentExited {
            task_id: task_id.to_string(),
            outcome: Outcome::Failed,
        },
    ) {
        eprintln!("[zelloom-runner] failed to report the task as failed: {e}");
    }
}

fn run_task(
    task: Task,
    agent: ResolvedAgent,
    writer: &mut UnixStream,
    rx: &mut EventStream,
    tx: &mpsc::Sender<RunnerMsg>,
) -> bool {
    println!("\n=== task {} ===\n{}\n", task.id, task.text);

    let runner_pgid = unistd::getpgid(None).unwrap_or_else(|_| Pid::this());
    let (tty, saved_termios, child) = match spawn_agent(&agent) {
        Ok(started) => started,
        Err(e) => {
            eprintln!("[zelloom-runner] failed to start agent: {e:#}");
            report_failed(writer, &task.id);
            return true;
        }
    };

    let pid = Pid::from_raw(child.id() as i32);
    let _ = unistd::setpgid(pid, pid);
    if let Err(e) = unistd::tcsetpgrp(&tty, pid) {
        eprintln!("[zelloom-runner] failed to move agent to the foreground: {e}");
    }

    let (exit_status, stop_requested, server_closed, term_signal) =
        wait_for_agent(child, pid, &task.id, rx, tx);

    restore_terminal(&tty, saved_termios.as_ref(), runner_pgid);

    if let Err(e) = &exit_status {
        eprintln!("[zelloom-runner] error while waiting for agent: {e}");
    }

    if term_signal {
        return false;
    }
    if stop_requested {
        if server_closed {
            eprintln!("[zelloom-runner] connection to loom core lost");
            return false;
        }
        return true;
    }
    if server_closed {
        eprintln!("[zelloom-runner] connection to loom core lost; exiting after agent finished");
        return false;
    }

    let prompt = if agent.oneshot {
        PromptResult::Outcome(oneshot_outcome(&exit_status))
    } else {
        match prompt_outcome(&tty, &task.id, rx) {
            Ok(prompt) => prompt,
            Err(e) => {
                eprintln!("[zelloom-runner] failed to ask for the task outcome: {e:#}");
                report_failed(writer, &task.id);
                return true;
            }
        }
    };
    match prompt {
        PromptResult::Outcome(outcome) => {
            if let Err(e) = send_request(
                writer,
                &Request::AgentExited {
                    task_id: task.id.clone(),
                    outcome,
                },
            ) {
                eprintln!("[zelloom-runner] failed to report the task outcome: {e}");
                return false;
            }
            true
        }
        PromptResult::Cancelled => true,
        PromptResult::Exit => false,
    }
}

fn wait_for_agent(
    mut child: Child,
    pid: Pid,
    task_id: &str,
    rx: &mut EventStream,
    tx: &mpsc::Sender<RunnerMsg>,
) -> (std::io::Result<ExitStatus>, bool, bool, bool) {
    let waiter_tx = tx.clone();
    std::thread::spawn(move || {
        let status = child.wait();
        let _ = waiter_tx.send(RunnerMsg::ChildExited(status));
    });

    let mut stop_requested = false;
    let mut server_closed = false;
    let mut term_signal = false;
    let mut deadline: Option<Instant> = None;
    // Unrelated messages must not be dropped, but re-reading them here via `rx.recv()` would spin forever, so they're only handed back to `rx` once this loop is done waiting.
    let mut deferred: Vec<RunnerMsg> = Vec::new();

    let exit_status = loop {
        let msg = match deadline {
            Some(d) => {
                let now = Instant::now();
                if now >= d {
                    let _ = signal::killpg(pid, Signal::SIGKILL);
                    deadline = None;
                    match rx.recv() {
                        Ok(m) => m,
                        Err(_) => {
                            server_closed = true;
                            break Err(std::io::Error::other("runner channel closed"));
                        }
                    }
                } else {
                    match rx.recv_timeout(d - now) {
                        Ok(m) => m,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            server_closed = true;
                            break Err(std::io::Error::other("runner channel closed"));
                        }
                    }
                }
            }
            None => match rx.recv() {
                Ok(m) => m,
                Err(_) => {
                    server_closed = true;
                    break Err(std::io::Error::other("runner channel closed"));
                }
            },
        };

        match msg {
            RunnerMsg::ChildExited(status) => break status,
            RunnerMsg::Server(ServerMessage::Event(RunnerEvent::Stop { task_id: id }))
                if id == task_id =>
            {
                if !stop_requested {
                    stop_requested = true;
                    let _ = signal::killpg(pid, Signal::SIGTERM);
                    deadline = Some(Instant::now() + Duration::from_secs(3));
                }
            }
            RunnerMsg::ServerClosed => {
                server_closed = true;
            }
            RunnerMsg::TermSignal => {
                term_signal = true;
                if deadline.is_none() {
                    let _ = signal::killpg(pid, Signal::SIGTERM);
                    deadline = Some(Instant::now() + Duration::from_secs(3));
                }
            }
            other => deferred.push(other),
        }
    };

    for msg in deferred {
        rx.push_back(msg);
    }

    (exit_status, stop_requested, server_closed, term_signal)
}

fn oneshot_outcome(exit_status: &std::io::Result<ExitStatus>) -> Outcome {
    match exit_status {
        Ok(status) if status.success() => Outcome::Done,
        _ => Outcome::Failed,
    }
}

enum PromptResult {
    Outcome(Outcome),
    Cancelled,
    Exit,
}

fn prompt_outcome(tty: &File, task_id: &str, rx: &mut EventStream) -> anyhow::Result<PromptResult> {
    write_escape(
        tty,
        "\nagent exited on its own. [d]one / [r]estart / [f]ailed? ",
    );

    let saved = termios::tcgetattr(tty)?;
    let mut raw = saved.clone();
    termios::cfmakeraw(&mut raw);
    termios::tcsetattr(tty, SetArg::TCSANOW, &raw)?;

    let result = prompt_loop(tty, task_id, rx);

    let _ = termios::tcsetattr(tty, SetArg::TCSANOW, &saved);
    write_escape(tty, "\n");
    result
}

fn prompt_loop(tty: &File, task_id: &str, rx: &mut EventStream) -> anyhow::Result<PromptResult> {
    let mut deferred: Vec<RunnerMsg> = Vec::new();

    let result = loop {
        match rx.try_recv() {
            Ok(RunnerMsg::Server(ServerMessage::Event(RunnerEvent::Stop { task_id: id })))
                if id == task_id =>
            {
                break Ok(PromptResult::Cancelled);
            }
            Ok(RunnerMsg::ServerClosed) | Ok(RunnerMsg::TermSignal) => {
                break Ok(PromptResult::Exit);
            }
            Ok(other) => deferred.push(other),
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => break Ok(PromptResult::Exit),
        }

        let mut fds = [PollFd::new(tty.as_fd(), PollFlags::POLLIN)];
        let ready = poll(&mut fds, PollTimeout::from(100u16));
        let ready = match ready {
            Ok(n) => n,
            Err(e) => break Err(e.into()),
        };
        if ready > 0 && fds[0].any().unwrap_or(false) {
            let mut buf = [0u8; 1];
            match unistd::read(tty, &mut buf) {
                Ok(0) => continue,
                Ok(_) => {}
                Err(e) => break Err(e.into()),
            }
            match buf[0] {
                b'd' | b'D' => break Ok(PromptResult::Outcome(Outcome::Done)),
                b'r' | b'R' => break Ok(PromptResult::Outcome(Outcome::Restart)),
                b'f' | b'F' => break Ok(PromptResult::Outcome(Outcome::Failed)),
                _ => {}
            }
        }
    };

    for msg in deferred {
        rx.push_back(msg);
    }

    result
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::process::Stdio;

    use super::*;
    use crate::protocol::{Source, TaskStatus};

    fn task(id: &str) -> Task {
        Task {
            id: id.to_string(),
            text: "text".to_string(),
            status: TaskStatus::Running,
            workspace: "a".to_string(),
            agent: None,
            source: Source::cli(),
            reply_to: None,
            metadata: serde_json::json!({}),
            position: 1,
            created_at: "2026-01-01T00:00:00+09:00".to_string(),
            updated_at: "2026-01-01T00:00:00+09:00".to_string(),
        }
    }

    fn run_with_argv(argv: Vec<String>) -> (bool, Request) {
        let (mut writer, peer) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        let mut rx = EventStream::new(rx);
        let agent = ResolvedAgent {
            argv,
            cwd: "/".to_string(),
            env: BTreeMap::new(),
            shell: false,
            oneshot: false,
        };
        let keep_running = run_task(task("t1"), agent, &mut writer, &mut rx, &tx);
        let mut line = String::new();
        BufReader::new(peer).read_line(&mut line).unwrap();
        (keep_running, serde_json::from_str(line.trim_end()).unwrap())
    }

    fn assert_reported_failed(request: Request) {
        match request {
            Request::AgentExited { task_id, outcome } => {
                assert_eq!(task_id, "t1");
                assert_eq!(outcome, Outcome::Failed);
            }
            other => panic!("expected agent_exited, got {other:?}"),
        }
    }

    #[test]
    fn empty_argv_reports_failed_and_keeps_runner_alive() {
        let (keep_running, request) = run_with_argv(vec![]);
        assert!(keep_running);
        assert_reported_failed(request);
    }

    #[test]
    fn unstartable_agent_reports_failed_and_keeps_runner_alive() {
        let (keep_running, request) =
            run_with_argv(vec!["/nonexistent/zelloom-test-agent".to_string()]);
        assert!(keep_running);
        assert_reported_failed(request);
    }

    fn strings(values: &[OsString]) -> Vec<String> {
        values
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect()
    }

    const TRICKY_ARGS: &[&str] = &[
        "a b",
        "line1\nline2\n",
        "q\"u'o`te\\",
        "$HOME $(echo injected) ${PATH}",
        "日本語のタスク",
        "",
        "-i",
        "*",
        "exec \"$@\"; echo x",
    ];

    #[test]
    fn shell_argv_uses_posix_wrapper_for_posix_and_unknown_shells() {
        let argv = vec!["claude".to_string(), "a b".to_string()];
        for shell in [
            "/bin/bash",
            "/usr/bin/zsh",
            "/bin/sh",
            "/bin/dash",
            "/bin/ksh",
            "/opt/bin/unknownsh",
        ] {
            assert_eq!(
                strings(&shell_argv(Some(OsStr::new(shell)), &argv)),
                vec![
                    shell,
                    "-i",
                    "-c",
                    "exec \"$@\"",
                    "loom-agent",
                    "claude",
                    "a b"
                ],
                "{shell}"
            );
        }
    }

    #[test]
    fn shell_argv_uses_argv_for_fish() {
        let argv = vec!["claude".to_string(), "a b".to_string()];
        assert_eq!(
            strings(&shell_argv(Some(OsStr::new("/usr/bin/fish")), &argv)),
            vec!["/usr/bin/fish", "-i", "-c", "exec $argv", "claude", "a b"]
        );
    }

    #[test]
    fn shell_argv_falls_back_to_bin_sh() {
        let argv = vec!["claude".to_string()];
        for shell in [None, Some(OsStr::new(""))] {
            assert_eq!(
                strings(&shell_argv(shell, &argv)),
                vec!["/bin/sh", "-i", "-c", "exec \"$@\"", "loom-agent", "claude"]
            );
        }
    }

    #[test]
    fn command_argv_respects_shell_flag() {
        let mut agent = ResolvedAgent {
            argv: vec!["claude".to_string(), "x".to_string()],
            cwd: "/".to_string(),
            env: BTreeMap::new(),
            shell: false,
            oneshot: false,
        };
        assert_eq!(
            strings(&command_argv(&agent, Some(OsStr::new("/bin/bash"))).unwrap()),
            vec!["claude", "x"]
        );
        agent.shell = true;
        assert_eq!(
            strings(&command_argv(&agent, Some(OsStr::new("/bin/bash"))).unwrap())[..3],
            ["/bin/bash", "-i", "-c"]
        );
        agent.argv.clear();
        assert!(command_argv(&agent, Some(OsStr::new("/bin/bash"))).is_err());
    }

    fn find_shell(name: &str) -> Option<PathBuf> {
        ["/bin", "/usr/bin", "/usr/local/bin"]
            .iter()
            .map(|dir| Path::new(dir).join(name))
            .find(|path| path.exists())
    }

    #[test]
    fn shell_wrapper_passes_args_through_unchanged() {
        let home = tempfile::tempdir().unwrap();
        let mut argv = vec!["printf".to_string(), "%s\\0".to_string()];
        argv.extend(TRICKY_ARGS.iter().map(|s| s.to_string()));
        let expected: Vec<u8> = TRICKY_ARGS
            .iter()
            .flat_map(|arg| arg.bytes().chain([0]))
            .collect();

        let mut tested = 0;
        for name in ["bash", "sh", "dash", "zsh", "fish"] {
            let Some(shell) = find_shell(name) else {
                continue;
            };
            let wrapped = shell_argv(Some(shell.as_os_str()), &argv);
            let output = Command::new(&wrapped[0])
                .args(&wrapped[1..])
                .env("HOME", home.path())
                .env("XDG_CONFIG_HOME", home.path())
                .env("ZDOTDIR", home.path())
                .env_remove("ENV")
                .env_remove("BASH_ENV")
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(output.status.success(), "{name}: {output:?}");
            assert_eq!(output.stdout, expected, "{name}");
            tested += 1;
        }
        assert!(tested > 0);
    }
}
