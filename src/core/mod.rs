mod agent;
mod scheduler;

pub use scheduler::{
    NoopLauncher, Scheduler, TabLauncher, attach_timeout_from_env, input_timeout_from_env,
    watch_write_timeout_from_env,
};

use scheduler::send_json;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinSet;

use crate::protocol::{Request, Response, WatchEvent};
use crate::store::Store;

pub struct CoreOptions {
    pub socket_path: PathBuf,
    pub config_path: PathBuf,
    pub db_path: PathBuf,
    pub launcher: Arc<dyn TabLauncher>,
}

pub async fn run(options: CoreOptions) -> anyhow::Result<()> {
    ensure_socket_available(&options.socket_path).await?;
    if let Some(parent) = options.socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let initial_config = crate::config::load(&options.config_path).map_err(|e| {
        anyhow::anyhow!(
            "failed to load config {}: {e}",
            options.config_path.display()
        )
    })?;

    let http_listener = match &initial_config.http {
        Some(http_cfg) => {
            let addr = http_cfg.listen_addr();
            let std_listener = std::net::TcpListener::bind(addr)
                .map_err(|e| anyhow::anyhow!("failed to bind http listener on {addr}: {e}"))?;
            std_listener.set_nonblocking(true)?;
            Some((
                tokio::net::TcpListener::from_std(std_listener)?,
                http_cfg.clone(),
            ))
        }
        None => None,
    };

    let store = Arc::new(Store::open(&options.db_path)?);
    let interrupted = store.mark_all_running_interrupted()?;
    if interrupted > 0 {
        eprintln!("[zelloom-core] marked {interrupted} previously running task(s) as interrupted");
    }

    let zellij_session_name = std::env::var("ZELLIJ_SESSION_NAME").ok();
    let attach_timeout = attach_timeout_from_env();
    let watch_write_timeout = watch_write_timeout_from_env();
    let http_socket_path = options.socket_path.clone();

    let scheduler = Arc::new(Scheduler::new(
        store,
        options.config_path,
        options.socket_path.clone(),
        options.launcher,
        zellij_session_name,
        attach_timeout,
        input_timeout_from_env(),
    ));

    let listener = tokio::net::UnixListener::bind(&options.socket_path)?;
    eprintln!(
        "[zelloom-core] listening on {}",
        options.socket_path.display()
    );

    let http_task = match http_listener {
        Some((tcp_listener, http_cfg)) => {
            let addr = tcp_listener.local_addr()?;
            eprintln!("[zelloom-core] http listening on {addr}");
            let shutdown_rx = scheduler.subscribe_shutdown();
            Some(tokio::spawn(crate::http::serve(
                tcp_listener,
                http_cfg,
                http_socket_path,
                shutdown_rx,
            )))
        }
        None => None,
    };

    let mut shutdown = scheduler.subscribe_shutdown();
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let scheduler = Arc::clone(&scheduler);
                connections.spawn(async move {
                    if let Err(e) = handle_connection(scheduler, stream, watch_write_timeout).await {
                        eprintln!("[zelloom-core] connection error: {e}");
                    }
                });
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            _ = shutdown.wait_for(|requested| *requested) => {
                eprintln!("[zelloom-core] shutdown requested");
                break;
            }
        }
    }

    drop(listener);
    let _ = std::fs::remove_file(&options.socket_path);
    while connections.join_next().await.is_some() {}

    if let Some(task) = http_task {
        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("[zelloom-core] http server error: {e}"),
            Err(e) => eprintln!("[zelloom-core] http server task panicked: {e}"),
        }
    }

    Ok(())
}

async fn ensure_socket_available(path: &std::path::Path) -> anyhow::Result<()> {
    if path.exists() {
        match UnixStream::connect(path).await {
            Ok(_) => anyhow::bail!(
                "another zelloom core appears to be listening on {}",
                path.display()
            ),
            Err(_) => {
                std::fs::remove_file(path)?;
            }
        }
    }
    Ok(())
}

const MAX_REQUEST_LINE_BYTES: usize = 1024 * 1024;

enum Incoming {
    Request(Request),
    Invalid(String),
    TooLong,
    Closed,
}

async fn read_request(
    reader: &mut BufReader<OwnedReadHalf>,
    shutdown: &mut watch::Receiver<bool>,
) -> std::io::Result<Incoming> {
    let mut buf = Vec::new();
    let mut limited = (&mut *reader).take(MAX_REQUEST_LINE_BYTES as u64 + 1);
    let read = tokio::select! {
        read = limited.read_until(b'\n', &mut buf) => read?,
        _ = shutdown.wait_for(|requested| *requested) => 0,
    };
    if read == 0 {
        return Ok(Incoming::Closed);
    }
    if buf.len() > MAX_REQUEST_LINE_BYTES {
        return Ok(Incoming::TooLong);
    }
    let parsed = std::str::from_utf8(&buf)
        .map_err(|e| e.to_string())
        .and_then(|line| serde_json::from_str(line.trim_end()).map_err(|e| e.to_string()));
    Ok(match parsed {
        Ok(request) => Incoming::Request(request),
        Err(e) => Incoming::Invalid(format!("invalid request: {e}")),
    })
}

fn too_long_response() -> Response {
    Response::err(format!(
        "request line exceeds {MAX_REQUEST_LINE_BYTES} bytes; closing the connection"
    ))
}

async fn handle_connection(
    scheduler: Arc<Scheduler>,
    stream: UnixStream,
    watch_write_timeout: Duration,
) -> anyhow::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut shutdown = scheduler.subscribe_shutdown();

    let mut incoming = read_request(&mut reader, &mut shutdown).await?;
    match incoming {
        Incoming::Request(Request::Watch) => {
            return watch_stream(
                &scheduler,
                reader,
                write_half,
                shutdown,
                watch_write_timeout,
            )
            .await;
        }
        Incoming::TooLong => {
            let mut line = serde_json::to_string(&too_long_response())?;
            line.push('\n');
            let _ =
                tokio::time::timeout(watch_write_timeout, write_half.write_all(line.as_bytes()))
                    .await;
            return Ok(());
        }
        _ => {}
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let writer_task = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if write_half.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    let mut is_first_message = true;
    loop {
        match incoming {
            Incoming::Closed => break,
            Incoming::TooLong => {
                send_json(&tx, &too_long_response());
                break;
            }
            Incoming::Invalid(message) => send_json(&tx, &Response::err(message)),
            Incoming::Request(Request::RunnerAttach { workspace }) => {
                if !is_first_message {
                    send_json(
                        &tx,
                        &Response::err("runner_attach must be the first message on a connection"),
                    );
                } else {
                    serve_runner(&scheduler, workspace, &mut reader, &tx, &mut shutdown).await;
                    break;
                }
            }
            Incoming::Request(request) => {
                let response = scheduler.handle_request(request).await;
                send_json(&tx, &response);
            }
        }
        is_first_message = false;
        incoming = read_request(&mut reader, &mut shutdown).await?;
    }

    drop(tx);
    let _ = writer_task.await;
    Ok(())
}

async fn serve_runner(
    scheduler: &Arc<Scheduler>,
    workspace: String,
    reader: &mut BufReader<OwnedReadHalf>,
    tx: &mpsc::UnboundedSender<String>,
    shutdown: &mut watch::Receiver<bool>,
) {
    let Some(attach_id) = scheduler
        .runner_attached(workspace.clone(), tx.clone())
        .await
    else {
        return;
    };

    loop {
        let incoming = match read_request(reader, shutdown).await {
            Ok(incoming) => incoming,
            Err(e) => {
                eprintln!("[zelloom-core] runner connection error: {e}");
                break;
            }
        };
        let response = match incoming {
            Incoming::Closed => break,
            Incoming::TooLong => {
                send_json(tx, &too_long_response());
                break;
            }
            Incoming::Invalid(message) => Response::err(message),
            Incoming::Request(Request::AgentExited { task_id, outcome }) => {
                scheduler
                    .agent_exited(&workspace, attach_id, &task_id, outcome)
                    .await
            }
            Incoming::Request(Request::InputResult {
                task_id,
                input_id,
                outcome,
                error,
            }) => {
                scheduler
                    .input_result(attach_id, &task_id, input_id, outcome, error)
                    .await
            }
            Incoming::Request(Request::AgentGone { task_id }) => {
                scheduler.agent_gone(&workspace, attach_id, &task_id).await
            }
            Incoming::Request(Request::RunnerAttach { .. }) => Response::err(format!(
                "connection is already attached as runner for {workspace}"
            )),
            Incoming::Request(other) => scheduler.handle_request(other).await,
        };
        send_json(tx, &response);
    }

    scheduler.runner_disconnected(&workspace, attach_id).await;
}

async fn watch_stream(
    scheduler: &Scheduler,
    mut reader: BufReader<OwnedReadHalf>,
    mut writer: OwnedWriteHalf,
    mut shutdown: watch::Receiver<bool>,
    write_timeout: Duration,
) -> anyhow::Result<()> {
    let (snapshot, mut events) = match scheduler.watch() {
        Ok(subscription) => subscription,
        Err(e) => {
            let error = WatchEvent::Error {
                error: e.to_string(),
            };
            write_event(&mut writer, &mut shutdown, write_timeout, &error).await;
            return Ok(());
        }
    };
    for task in snapshot {
        let event = WatchEvent::Task { task };
        if !write_event(&mut writer, &mut shutdown, write_timeout, &event).await {
            return Ok(());
        }
    }
    if !write_event(
        &mut writer,
        &mut shutdown,
        write_timeout,
        &WatchEvent::Synced,
    )
    .await
    {
        return Ok(());
    }

    let mut discard = [0u8; 1024];
    loop {
        let event = tokio::select! {
            event = events.recv() => event,
            read = reader.read(&mut discard) => match read {
                Ok(0) | Err(_) => break,
                Ok(_) => continue,
            },
            _ = shutdown.wait_for(|requested| *requested) => break,
        };
        match event {
            Ok(event) => {
                if !write_event(&mut writer, &mut shutdown, write_timeout, &event).await {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                let error = WatchEvent::Error {
                    error: format!(
                        "watch stream fell behind by {missed} event(s); reconnect to get a fresh snapshot"
                    ),
                };
                write_event(&mut writer, &mut shutdown, write_timeout, &error).await;
                break;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    Ok(())
}

async fn write_event(
    writer: &mut OwnedWriteHalf,
    shutdown: &mut watch::Receiver<bool>,
    timeout: Duration,
    event: &WatchEvent,
) -> bool {
    let Ok(mut line) = serde_json::to_string(event) else {
        return false;
    };
    line.push('\n');
    tokio::select! {
        written = tokio::time::timeout(timeout, writer.write_all(line.as_bytes())) => {
            matches!(written, Ok(Ok(())))
        }
        _ = shutdown.wait_for(|requested| *requested) => false,
    }
}
