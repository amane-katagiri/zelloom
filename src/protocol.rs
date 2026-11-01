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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<Attention>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<TurnState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    Question,
    Gate,
    Blocked,
    Ready,
}

impl AttentionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            AttentionKind::Question => "question",
            AttentionKind::Gate => "gate",
            AttentionKind::Blocked => "blocked",
            AttentionKind::Ready => "ready",
        }
    }

    pub fn awaits_reply(&self) -> bool {
        matches!(
            self,
            AttentionKind::Question | AttentionKind::Gate | AttentionKind::Ready
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attention {
    pub id: u64,
    pub kind: AttentionKind,
    pub message: String,
    pub at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    Busy,
    Idle,
}

impl TurnState {
    pub fn as_str(&self) -> &'static str {
        match self {
            TurnState::Busy => "busy",
            TurnState::Idle => "idle",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputOutcome {
    Typed,
    NotTyped,
    Partial,
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
    Get {
        task_id: String,
    },
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
    Report {
        task_id: String,
        kind: AttentionKind,
        message: String,
    },
    Watch,
    Send {
        task_id: String,
        expect: u64,
        text: String,
    },
    Turn {
        task_id: String,
        state: TurnState,
    },
    InputResult {
        task_id: String,
        input_id: u64,
        outcome: InputOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    AgentGone {
        task_id: String,
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
    pub oneshot: bool,
}

// Rare, small-volume IPC messages, not a hot path, so we accept the size difference instead of boxing `Task`/`ResolvedAgent`.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RunnerEvent {
    Start {
        task: Task,
        agent: ResolvedAgent,
    },
    Stop {
        task_id: String,
    },
    Input {
        task_id: String,
        input_id: u64,
        text: String,
    },
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WatchEvent {
    Task { task: Task },
    TaskDeleted { task_id: String },
    Synced,
    Error { error: String },
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
    fn attention_is_omitted_when_absent_and_shaped_when_present() {
        let mut task: Task = serde_json::from_str(
            r#"{"id":"t","text":"x","status":"running","workspace":"w","source":{"type":"cli"},"position":1,"created_at":"c","updated_at":"u"}"#,
        )
        .unwrap();
        assert!(task.attention.is_none());
        assert!(!serde_json::to_string(&task).unwrap().contains("attention"));
        task.attention = Some(Attention {
            id: 7,
            kind: AttentionKind::Gate,
            message: "ok to push?".to_string(),
            at: "a".to_string(),
        });
        let value = serde_json::to_value(&task).unwrap();
        assert_eq!(
            value["attention"],
            serde_json::json!({"id": 7, "kind": "gate", "message": "ok to push?", "at": "a"})
        );
        assert!(value.get("turn").is_none());
        task.turn = Some(TurnState::Idle);
        assert_eq!(serde_json::to_value(&task).unwrap()["turn"], "idle");
    }

    #[test]
    fn watch_events_are_tagged() {
        assert_eq!(
            serde_json::to_string(&WatchEvent::Synced).unwrap(),
            r#"{"event":"synced"}"#
        );
        assert_eq!(
            serde_json::to_string(&WatchEvent::TaskDeleted {
                task_id: "t".to_string()
            })
            .unwrap(),
            r#"{"event":"task_deleted","task_id":"t"}"#
        );
        let input: ServerMessage =
            serde_json::from_str(r#"{"event":"input","task_id":"t","input_id":3,"text":"yes"}"#)
                .unwrap();
        assert!(matches!(
            input,
            ServerMessage::Event(RunnerEvent::Input { .. })
        ));
    }

    #[test]
    fn send_requires_expect_and_input_result_parses_outcome() {
        assert!(
            serde_json::from_str::<Request>(r#"{"op":"send","task_id":"t","text":"x"}"#).is_err()
        );
        let json = r#"{"op":"input_result","task_id":"t","input_id":4,"outcome":"not_typed","error":"no pane"}"#;
        match serde_json::from_str::<Request>(json).unwrap() {
            Request::InputResult {
                input_id,
                outcome,
                error,
                ..
            } => {
                assert_eq!(input_id, 4);
                assert_eq!(outcome, InputOutcome::NotTyped);
                assert_eq!(error.as_deref(), Some("no pane"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
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
