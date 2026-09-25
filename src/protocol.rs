use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Received,
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
    Rejected,
    Interrupted,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Received => "received",
            TaskStatus::Queued => "queued",
            TaskStatus::Running => "running",
            TaskStatus::Done => "done",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
            TaskStatus::Rejected => "rejected",
            TaskStatus::Interrupted => "interrupted",
        }
    }

    pub fn parse(s: &str) -> Option<TaskStatus> {
        Some(match s {
            "received" => TaskStatus::Received,
            "queued" => TaskStatus::Queued,
            "running" => TaskStatus::Running,
            "done" => TaskStatus::Done,
            "failed" => TaskStatus::Failed,
            "cancelled" => TaskStatus::Cancelled,
            "rejected" => TaskStatus::Rejected,
            "interrupted" => TaskStatus::Interrupted,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
}

impl Source {
    pub fn cli() -> Source {
        Source {
            kind: "cli".to_string(),
            id: None,
            sender: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub text: String,
    pub status: TaskStatus,
    pub workspace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub source: Source,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<serde_json::Value>,
    #[serde(default = "default_metadata")]
    pub metadata: serde_json::Value,
    pub position: i64,
    pub created_at: String,
    pub updated_at: String,
}

fn default_metadata() -> serde_json::Value {
    serde_json::json!({})
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Done,
    Failed,
    Restart,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Enqueue {
        text: String,
        workspace: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(default = "Source::cli")]
        source: Source,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<serde_json::Value>,
        #[serde(default = "default_metadata")]
        metadata: serde_json::Value,
    },
    List,
    Complete {
        task_id: String,
    },
    Fail {
        task_id: String,
    },
    Cancel {
        task_id: String,
    },
    Delete {
        task_id: String,
    },
    Edit {
        task_id: String,
        text: String,
    },
    Move {
        task_id: String,
        direction: MoveDirection,
    },
    Accept {
        task_id: String,
    },
    Reject {
        task_id: String,
    },
    Retry {
        task_id: String,
    },
    Status,
    Shutdown {
        #[serde(default)]
        force: bool,
    },
    RunnerAttach {
        workspace: String,
    },
    AgentExited {
        task_id: String,
        outcome: Outcome,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(data: serde_json::Value) -> Response {
        Response {
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn ok_empty() -> Response {
        Response {
            ok: true,
            data: None,
            error: None,
        }
    }

    pub fn err(message: impl Into<String>) -> Response {
        Response {
            ok: false,
            data: None,
            error: Some(message.into()),
        }
    }

    pub fn into_result(self) -> Result<Option<serde_json::Value>, String> {
        if self.ok {
            Ok(self.data)
        } else {
            Err(self.error.unwrap_or_else(|| "unknown error".to_string()))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedAgent {
    pub argv: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub shell: bool,
}

// Rare, small-volume IPC messages, not a hot path, so we accept the size difference instead of boxing `Task`/`ResolvedAgent`.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RunnerEvent {
    Start { task: Task, agent: ResolvedAgent },
    Stop { task_id: String },
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ServerMessage {
    Event(RunnerEvent),
    Response(Response),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_enqueue_round_trips() {
        let json = r#"{"op":"enqueue","text":"do x","workspace":"amanejp"}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        match req {
            Request::Enqueue {
                text,
                workspace,
                agent,
                source,
                reply_to,
                metadata,
            } => {
                assert_eq!(text, "do x");
                assert_eq!(workspace, "amanejp");
                assert_eq!(agent, None);
                assert_eq!(source.kind, "cli");
                assert_eq!(reply_to, None);
                assert_eq!(metadata, serde_json::json!({}));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn response_ok_shape() {
        let resp = Response::ok(serde_json::json!({"foo": 1}));
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"ok":true,"data":{"foo":1}}"#);
    }

    #[test]
    fn response_err_shape() {
        let resp = Response::err("nope");
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"ok":false,"error":"nope"}"#);
    }

    #[test]
    fn server_message_distinguishes_event_and_response() {
        let event_json = r#"{"event":"stop","task_id":"01H"}"#;
        let msg: ServerMessage = serde_json::from_str(event_json).unwrap();
        assert!(matches!(
            msg,
            ServerMessage::Event(RunnerEvent::Stop { .. })
        ));

        let resp_json = r#"{"ok":true,"data":null}"#;
        let msg: ServerMessage = serde_json::from_str(resp_json).unwrap();
        assert!(matches!(msg, ServerMessage::Response(_)));
    }

    #[test]
    fn agent_exited_parses_outcome() {
        let json = r#"{"op":"agent_exited","task_id":"01H","outcome":"restart"}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        match req {
            Request::AgentExited { task_id, outcome } => {
                assert_eq!(task_id, "01H");
                assert_eq!(outcome, Outcome::Restart);
            }
            _ => panic!("wrong variant"),
        }
    }
}
