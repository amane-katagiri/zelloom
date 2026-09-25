use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{FromRequest, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use tower_http::cors::CorsLayer;

use crate::client::ClientError;
use crate::config::HttpConfig;
use crate::protocol::{Request as CoreRequest, Source, Task};

#[derive(Clone)]
struct AppState {
    socket_path: Arc<PathBuf>,
    allowed_origins: Arc<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnqueueBody {
    text: String,
    workspace: String,
    #[serde(default)]
    metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

struct AppJson<T>(T);

impl<S, T> FromRequest<S> for AppJson<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(AppJson(value)),
            Err(rejection) => Err(error_response(rejection.status(), &rejection.body_text())),
        }
    }
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

async fn create_task(
    State(state): State<AppState>,
    AppJson(body): AppJson<EnqueueBody>,
) -> Response {
    let request = CoreRequest::Enqueue {
        text: body.text,
        workspace: body.workspace,
        agent: None,
        source: Source {
            kind: "http".to_string(),
            id: None,
            sender: None,
        },
        reply_to: None,
        metadata: serde_json::Value::Object(body.metadata.unwrap_or_default()),
    };
    let socket_path = Arc::clone(&state.socket_path);
    let result =
        tokio::task::spawn_blocking(move || crate::client::call(&socket_path, &request)).await;
    match result {
        Ok(Ok(value)) => match serde_json::from_value::<Task>(value) {
            Ok(task) => (StatusCode::CREATED, Json(task)).into_response(),
            Err(e) => error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("core returned an unexpected task shape: {e}"),
            ),
        },
        Ok(Err(ClientError::Server(message))) => error_response(StatusCode::BAD_REQUEST, &message),
        Ok(Err(ClientError::CoreNotRunning(_))) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "loom core socket is not reachable",
        ),
        Ok(Err(other)) => error_response(StatusCode::SERVICE_UNAVAILABLE, &other.to_string()),
        Err(join_err) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("internal error: {join_err}"),
        ),
    }
}

fn host_is_allowed(host: &str) -> bool {
    if let Some(rest) = host.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((ipv6, after)) if after.is_empty() || after.starts_with(':') => ipv6 == "::1",
            _ => false,
        };
    }
    let host_only = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    matches!(host_only, "localhost" | "127.0.0.1")
}

async fn security_check(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let host_ok = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(host_is_allowed)
        .unwrap_or(false);
    if !host_ok {
        return error_response(StatusCode::FORBIDDEN, "invalid or missing Host header");
    }
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
        && !state.allowed_origins.iter().any(|o| o == origin)
    {
        return error_response(StatusCode::FORBIDDEN, "origin not allowed");
    }
    next.run(request).await
}

fn cors_layer(origins: &[String]) -> CorsLayer {
    let values: Vec<axum::http::HeaderValue> = origins
        .iter()
        .map(|o| o.parse().expect("origin validated by Config::validate"))
        .collect();
    CorsLayer::new()
        .allow_origin(values)
        .allow_methods([Method::POST])
        .allow_headers([header::CONTENT_TYPE])
}

fn build_router(config: &HttpConfig, socket_path: PathBuf) -> Router {
    let state = AppState {
        socket_path: Arc::new(socket_path),
        allowed_origins: Arc::new(config.allowed_origins.clone()),
    };

    let mut app = Router::new()
        .route("/tasks", post(create_task))
        .with_state(state.clone());

    if !config.allowed_origins.is_empty() {
        app = app.layer(cors_layer(&config.allowed_origins));
    }

    app.layer(middleware::from_fn_with_state(state, security_check))
}

pub async fn serve(
    listener: tokio::net::TcpListener,
    config: HttpConfig,
    socket_path: PathBuf,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let app = build_router(&config, socket_path);
    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(async move {
            let _ = shutdown.wait_for(|requested| *requested).await;
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_is_allowed_accepts_localhost_forms() {
        for good in [
            "localhost",
            "localhost:7878",
            "127.0.0.1",
            "127.0.0.1:7878",
            "[::1]",
            "[::1]:7878",
        ] {
            assert!(host_is_allowed(good), "{good:?} should be allowed");
        }
    }

    #[test]
    fn host_is_allowed_rejects_other_hosts() {
        for bad in [
            "evil.example.com",
            "evil.example.com:7878",
            "0.0.0.0",
            "[::2]",
            "[::1",
            "localhost.evil.com",
        ] {
            assert!(!host_is_allowed(bad), "{bad:?} should be rejected");
        }
    }
}
