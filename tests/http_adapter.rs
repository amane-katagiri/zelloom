use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use zelloom::client::call;
use zelloom::core::{CoreOptions, NoopLauncher, run as core_run};
use zelloom::protocol::{Request, Task, TaskStatus};

fn write_config(path: &Path, http_port: u16, auto_queue_http: bool) {
    let auto_queue_section = if auto_queue_http {
        String::new()
    } else {
        "\n[sources.http]\nauto_queue = false\n".to_string()
    };
    std::fs::write(
        path,
        format!(
            r#"
default_agent = "fake"

[agents.fake]
command = ["fake-agent"]

[workspaces.a]
path = "/tmp/zelloom-http-test-workspace-a"
agent = "fake"

[http]
listen = "127.0.0.1:{http_port}"
allowed_origins = ["http://localhost:5173"]
{auto_queue_section}
"#
        ),
    )
    .unwrap();
}

struct RunningCore {
    socket_path: PathBuf,
    http_port: u16,
    handle: std::thread::JoinHandle<()>,
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn start_core(dir: &Path, auto_queue_http: bool) -> RunningCore {
    let http_port = free_port();
    let config_path = dir.join("config.toml");
    write_config(&config_path, http_port, auto_queue_http);
    let db_path = dir.join("state.db");
    let socket_path = dir.join("default.sock");

    let options = CoreOptions {
        socket_path: socket_path.clone(),
        config_path,
        db_path,
        launcher: Arc::new(NoopLauncher),
    };

    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            if let Err(e) = core_run(options).await {
                panic!("core exited with an error: {e}");
            }
        });
    });

    wait_for_socket(&socket_path);
    wait_for_tcp(http_port);

    RunningCore {
        socket_path,
        http_port,
        handle,
    }
}

fn wait_for_socket(path: &Path) {
    for _ in 0..400 {
        if path.exists() && std::os::unix::net::UnixStream::connect(path).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("core did not start listening on {}", path.display());
}

fn wait_for_tcp(port: u16) {
    for _ in 0..400 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("core did not start listening on http port {port}");
}

fn shutdown(core: RunningCore) {
    call(&core.socket_path, &Request::Shutdown { force: true }).unwrap();
    core.handle.join().unwrap();
}

fn parse_response(raw: &str) -> (u16, HashMap<String, String>, String) {
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("could not parse status line: {status_line:?}"));
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    (status, headers, body.to_string())
}

fn send_raw(port: u16, request: &str) -> (u16, HashMap<String, String>, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    parse_response(&raw)
}

fn post_request(host: &str, origin: Option<&str>, content_type: &str, body: &str) -> String {
    let mut req = format!("POST /tasks HTTP/1.1\r\nHost: {host}\r\n");
    if let Some(origin) = origin {
        req.push_str(&format!("Origin: {origin}\r\n"));
    }
    req.push_str(&format!("Content-Type: {content_type}\r\n"));
    req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    req.push_str("Connection: close\r\n\r\n");
    req.push_str(body);
    req
}

fn preflight_request(host: &str, origin: &str) -> String {
    format!(
        "OPTIONS /tasks HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nAccess-Control-Request-Method: POST\r\nAccess-Control-Request-Headers: content-type\r\nConnection: close\r\n\r\n"
    )
}

#[test]
fn post_creates_task_visible_via_list_with_http_source() {
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path(), true);
    let host = format!("127.0.0.1:{}", core.http_port);

    let body = r#"{"text":"hello via http","workspace":"a"}"#;
    let req = post_request(&host, None, "application/json", body);
    let (status, _headers, resp_body) = send_raw(core.http_port, &req);
    assert_eq!(status, 201, "{resp_body}");
    let task: Task = serde_json::from_str(&resp_body).unwrap();
    assert_eq!(task.status, TaskStatus::Queued);
    assert_eq!(task.source.kind, "http");
    assert_eq!(task.workspace, "a");

    let listed = call(&core.socket_path, &Request::List).unwrap();
    let tasks: Vec<Task> = serde_json::from_value(listed).unwrap();
    assert!(
        tasks
            .iter()
            .any(|t| t.id == task.id && t.source.kind == "http"),
        "task not found via list: {tasks:?}"
    );

    shutdown(core);
}

#[test]
fn unregistered_workspace_returns_400_with_core_message() {
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path(), true);
    let host = format!("127.0.0.1:{}", core.http_port);

    let body = r#"{"text":"x","workspace":"zzz"}"#;
    let req = post_request(&host, None, "application/json", body);
    let (status, _headers, resp_body) = send_raw(core.http_port, &req);
    assert_eq!(status, 400, "{resp_body}");
    let err: serde_json::Value = serde_json::from_str(&resp_body).unwrap();
    assert!(
        err["error"]
            .as_str()
            .unwrap()
            .contains("workspace 'zzz' is not registered"),
        "{err}"
    );

    shutdown(core);
}

#[test]
fn wrong_host_header_is_rejected_with_403() {
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path(), true);

    let body = r#"{"text":"x","workspace":"a"}"#;
    let req = post_request("evil.example.com", None, "application/json", body);
    let (status, _headers, resp_body) = send_raw(core.http_port, &req);
    assert_eq!(status, 403, "{resp_body}");

    shutdown(core);
}

#[test]
fn disallowed_origin_is_rejected_with_403() {
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path(), true);
    let host = format!("127.0.0.1:{}", core.http_port);

    let body = r#"{"text":"x","workspace":"a"}"#;
    let req = post_request(
        &host,
        Some("http://evil.example.com"),
        "application/json",
        body,
    );
    let (status, _headers, resp_body) = send_raw(core.http_port, &req);
    assert_eq!(status, 403, "{resp_body}");

    shutdown(core);
}

#[test]
fn allowed_origin_preflight_has_cors_headers() {
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path(), true);
    let host = format!("127.0.0.1:{}", core.http_port);

    let req = preflight_request(&host, "http://localhost:5173");
    let (status, headers, body) = send_raw(core.http_port, &req);
    assert!(status < 300, "status {status}, body {body}");
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .map(String::as_str),
        Some("http://localhost:5173"),
        "{headers:?}"
    );

    shutdown(core);
}

#[test]
fn disallowed_origin_preflight_does_not_grant_access() {
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path(), true);
    let host = format!("127.0.0.1:{}", core.http_port);

    let req = preflight_request(&host, "http://evil.example.com");
    let (status, headers, _body) = send_raw(core.http_port, &req);
    assert_eq!(status, 403);
    assert!(
        !headers.contains_key("access-control-allow-origin"),
        "{headers:?}"
    );

    shutdown(core);
}

#[test]
fn allowed_origin_post_gets_201_with_cors_header() {
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path(), true);
    let host = format!("127.0.0.1:{}", core.http_port);

    let body = r#"{"text":"from a browser tool","workspace":"a"}"#;
    let req = post_request(
        &host,
        Some("http://localhost:5173"),
        "application/json",
        body,
    );
    let (status, headers, resp_body) = send_raw(core.http_port, &req);
    assert_eq!(status, 201, "{resp_body}");
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .map(String::as_str),
        Some("http://localhost:5173"),
        "{headers:?}"
    );

    shutdown(core);
}

#[test]
fn non_json_content_type_is_rejected_with_415() {
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path(), true);
    let host = format!("127.0.0.1:{}", core.http_port);

    let req = post_request(&host, None, "text/plain", "not json");
    let (status, _headers, resp_body) = send_raw(core.http_port, &req);
    assert_eq!(status, 415, "{resp_body}");

    shutdown(core);
}

#[test]
fn unknown_field_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path(), true);
    let host = format!("127.0.0.1:{}", core.http_port);

    let body = r#"{"text":"x","workspace":"a","agent":"claude"}"#;
    let req = post_request(&host, None, "application/json", body);
    let (status, _headers, resp_body) = send_raw(core.http_port, &req);
    assert!(
        (400..500).contains(&status),
        "expected a 4xx status, got {status}: {resp_body}"
    );

    shutdown(core);
}

#[test]
fn auto_queue_false_leaves_task_received() {
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path(), false);
    let host = format!("127.0.0.1:{}", core.http_port);

    let body = r#"{"text":"needs review first","workspace":"a"}"#;
    let req = post_request(&host, None, "application/json", body);
    let (status, _headers, resp_body) = send_raw(core.http_port, &req);
    assert_eq!(status, 201, "{resp_body}");
    let task: Task = serde_json::from_str(&resp_body).unwrap();
    assert_eq!(task.status, TaskStatus::Received);

    shutdown(core);
}

#[test]
fn core_shutdown_stops_http_listener() {
    let dir = tempfile::tempdir().unwrap();
    let core = start_core(dir.path(), true);
    let port = core.http_port;

    shutdown(core);

    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_err(),
        "http listener should have stopped after core shutdown"
    );
}
