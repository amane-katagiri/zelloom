mod scheduler;

pub use scheduler::{NoopLauncher, Scheduler, TabLauncher, attach_timeout_from_env};

use scheduler::send_json;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::protocol::Request;
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
    let http_socket_path = options.socket_path.clone();

    let scheduler = Arc::new(Scheduler::new(
        store,
        options.config_path,
        options.socket_path.clone(),
        options.launcher,
        zellij_session_name,
        attach_timeout,
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
                    if let Err(e) = handle_connection(scheduler, stream).await {
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

async fn handle_connection(scheduler: Arc<Scheduler>, stream: UnixStream) -> anyhow::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    let writer_task = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if write_half.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    let mut shutdown = scheduler.subscribe_shutdown();
    let mut line = String::new();
    let mut first_message = true;
    loop {
        line.clear();
        let bytes_read = tokio::select! {
            read = reader.read_line(&mut line) => read?,
            _ = shutdown.wait_for(|requested| *requested) => 0,
        };
        if bytes_read == 0 {
            break;
        }
        let is_first_message = std::mem::replace(&mut first_message, false);
        let request: Request = match serde_json::from_str(line.trim_end()) {
            Ok(r) => r,
            Err(e) => {
                send_json(
                    &tx,
                    &crate::protocol::Response::err(format!("invalid request: {e}")),
                );
                continue;
            }
        };

        if let Request::RunnerAttach { workspace } = request {
            if !is_first_message {
                send_json(
                    &tx,
                    &crate::protocol::Response::err(
                        "runner_attach must be the first message on a connection",
                    ),
                );
                continue;
            }
            let Some(attach_id) = scheduler
                .runner_attached(workspace.clone(), tx.clone())
                .await
            else {
                break;
            };

            loop {
                line.clear();
                let bytes_read = tokio::select! {
                    read = reader.read_line(&mut line) => match read {
                        Ok(n) => n,
                        Err(e) => {
                            eprintln!("[zelloom-core] runner connection error: {e}");
                            0
                        }
                    },
                    _ = shutdown.wait_for(|requested| *requested) => 0,
                };
                if bytes_read == 0 {
                    break;
                }
                let request: Request = match serde_json::from_str(line.trim_end()) {
                    Ok(r) => r,
                    Err(e) => {
                        send_json(
                            &tx,
                            &crate::protocol::Response::err(format!("invalid request: {e}")),
                        );
                        continue;
                    }
                };
                let response = match request {
                    Request::AgentExited { task_id, outcome } => {
                        scheduler
                            .agent_exited(&workspace, attach_id, &task_id, outcome)
                            .await
                    }
                    Request::RunnerAttach { .. } => crate::protocol::Response::err(format!(
                        "connection is already attached as runner for {workspace}"
                    )),
                    other => scheduler.handle_request(other).await,
                };
                send_json(&tx, &response);
            }

            scheduler.runner_disconnected(&workspace, attach_id).await;
            break;
        }

        let response = scheduler.handle_request(request).await;
        send_json(&tx, &response);
    }

    drop(tx);
    let _ = writer_task.await;
    Ok(())
}
