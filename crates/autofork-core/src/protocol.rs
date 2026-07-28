//! The CLI ↔ daemon wire protocol: newline-delimited JSON, one request line
//! per response line. Every frame carries `proto` and `id`.
//!
//! Compatibility rule: the `shutdown` request shape is frozen forever at
//! proto 1, so any future CLI can always retire any past daemon.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A request frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub proto: u32,
    pub id: u64,
    #[serde(flatten)]
    pub body: RequestBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestBody {
    /// Version handshake.
    Hello { version: String },
    /// A fast Claude Code lifecycle event (SessionStart / PromptSubmit /
    /// SessionEnd), forwarded from a hook. Acked immediately.
    Event(Event),
    /// The asyncRewake Stop hook's long poll: register activity, arm idle
    /// timers, then block until forks are due (a `Wake`) or the wait is
    /// cancelled/the daemon retires (a `Waited`).
    StopWait(Event),
    /// Daemon + session + run status (the `autofork status` command).
    Status,
    /// Discovered forks for a project (the `autofork forks` command).
    ListForks { project_root: PathBuf, cwd: PathBuf },
    /// Close every stale session now (the `autofork prune` command), instead
    /// of waiting for the session-timeout reaper. Stale = the same heuristic
    /// `Status` annotates: open, no parked poll, idle far past the deadline.
    Prune,
    /// An opencode fork run started: the plugin forked the session and
    /// prompted the copy. `run_ref` is the fork session's id — the same
    /// opaque role a Claude Code spawn's `tool_use_id` plays.
    ForkSpawned {
        session_id: String,
        fork: String,
        run_ref: String,
    },
    /// An opencode fork run reached a terminal status
    /// (`completed`/`failed`/`stopped`). Drives `after`-dependency release.
    ForkCompleted {
        session_id: String,
        fork: String,
        run_ref: String,
        status: String,
    },
    /// Ask the daemon to exit. With `drain`, it finishes cleanly first.
    /// Frozen shape — never change.
    Shutdown { drain: bool },
}

/// A Claude Code lifecycle event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub event: EventKind,
    pub session_id: String,
    pub transcript_path: Option<PathBuf>,
    pub cwd: PathBuf,
    pub project_root: PathBuf,
    /// SessionStart source (`startup`/`resume`/`clear`/`compact`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The session's model id (SessionStart provides it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Per-session enable (whitelist) tag filter, from `AUTOFORK_ENABLE_TAGS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_tags: Option<Vec<String>>,
    /// Per-session disable (blocklist) tag filter, from `AUTOFORK_DISABLE_TAGS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_tags: Option<Vec<String>>,
    /// For a PromptSubmit: whether this is genuine user activity (`Some(true)`)
    /// or a non-waking continuation — an asyncRewake wake or a fork-completion
    /// task notification (`Some(false)`). `None` = the CLI couldn't tell (no
    /// prompt text); the daemon decides via its post-wake grace window.
    /// When the notif ids below are present, the daemon's own classification
    /// (fork-spawn match) overrides this coarse sniff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waking: Option<bool>,
    /// For a PromptSubmit that is a `<task-notification>`: the `tool_use_id`
    /// of the tool call that started the finished task, so the daemon can
    /// check it against its recorded fork spawns. Additive field (no proto
    /// bump); old daemons ignore it and keep the coarse `waking` sniff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notif_tool_use_id: Option<String>,
    /// The finished task's own id (`<task-id>`), the fallback match key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notif_task_id: Option<String>,
    /// The notification's `<status>` (`completed`/`failed`/`stopped`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notif_status: Option<String>,
    /// The session's context gauge in tokens, reported by clients that track
    /// usage themselves (opencode) instead of exposing a transcript to parse.
    /// Takes precedence over transcript parsing when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    /// The harness this event comes from (`"opencode"`; absent = Claude Code).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    SessionStart,
    PromptSubmit,
    /// The end of a turn — carried by a `StopWait` request (the asyncRewake
    /// Stop hook), never by a plain `Event`.
    Stop,
    SessionEnd,
}

/// A response frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub proto: u32,
    pub id: u64,
    #[serde(flatten)]
    pub body: ResponseBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseBody {
    Ack,
    HelloInfo {
        version: String,
    },
    /// Forks are due: the hook prints `payload` to stderr and exits 2 to wake
    /// the session. `forks` carries the same due set structured, for clients
    /// that execute fork runs programmatically (opencode) instead of handing
    /// the text to a model. Additive: old clients ignore it.
    Wake {
        payload: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        forks: Option<Vec<WakeFork>>,
    },
    /// The stop-wait resolved without a wake (cancelled by activity, nothing
    /// due, or the daemon is retiring): the hook exits 0 silently.
    Waited,
    StatusInfo(StatusInfo),
    ForkList {
        items: Vec<ForkInfo>,
    },
    /// The sessions a `Prune` closed (empty when nothing was stale).
    Pruned {
        sessions: Vec<SessionInfo>,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    ProtoMismatch,
    BadRequest,
    NotFound,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusInfo {
    pub version: String,
    /// The daemon's protocol version (the frame's own `proto` field is the
    /// transport's; keep the names distinct — this struct is flattened).
    pub daemon_proto: u32,
    pub pid: u32,
    pub sessions: Vec<SessionInfo>,
    /// Recent wakes issued (forks handed to sessions to spawn).
    pub recent_runs: Vec<RunInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub project_root: PathBuf,
    pub status: String,
    /// Unix epoch seconds.
    pub last_activity: i64,
    pub prompt_tokens: Option<u64>,
    /// Open, but with no parked poll and no activity for a long time — likely a
    /// session whose Claude process died mid-turn (annotated `[stale?]`).
    #[serde(default)]
    pub stale: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunInfo {
    pub fork: String,
    pub trigger: String,
    pub session_id: String,
    pub state: String,
    /// Unix epoch seconds.
    pub started_at: i64,
}

/// One due fork, structured, for programmatic clients: everything needed to
/// run it without parsing the human/model-facing wake payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WakeFork {
    pub name: String,
    /// Absolute path to the fork's `.md` definition.
    pub path: String,
    /// The matched trigger label (e.g. `idle`, `context_used:80%`).
    pub trigger: String,
    /// Whether concurrent runs of this fork are allowed.
    pub overlap: bool,
    /// In a release wake: the finished predecessor fork names whose reports
    /// the client should append to the prompt. Empty in a normal wake.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    /// The prompt to run the fork with (already carries the fork file path,
    /// trigger, session/conversation ids and project root).
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkInfo {
    pub name: String,
    pub path: PathBuf,
    pub description: Option<String>,
    pub triggers: Vec<String>,
    pub throttle_secs: Option<u64>,
    #[serde(default)]
    pub after: Vec<String>,
    #[serde(default)]
    pub overlap: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    pub warnings: Vec<String>,
}

/// Serialize a frame as one JSONL line (with trailing newline).
pub fn encode<T: Serialize>(frame: &T) -> Result<String, serde_json::Error> {
    let mut s = serde_json::to_string(frame)?;
    s.push('\n');
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_stop_wait() {
        let req = Request {
            proto: crate::PROTO_VERSION,
            id: 7,
            body: RequestBody::StopWait(Event {
                event: EventKind::Stop,
                session_id: "abc".into(),
                transcript_path: Some("/t.jsonl".into()),
                cwd: "/p".into(),
                project_root: "/p".into(),
                source: None,
                model: None,
                enable_tags: None,
                disable_tags: None,
                waking: None,
                notif_tool_use_id: None,
                notif_task_id: None,
                notif_status: None,
                context_tokens: None,
                client: None,
            }),
        };
        let line = encode(&req).unwrap();
        assert!(line.ends_with('\n'));
        let back: Request = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(back.id, 7);
        match back.body {
            RequestBody::StopWait(e) => assert_eq!(e.event, EventKind::Stop),
            _ => panic!("wrong body"),
        }
    }

    #[test]
    fn wake_response_round_trips() {
        let resp = Response {
            proto: crate::PROTO_VERSION,
            id: 1,
            body: ResponseBody::Wake {
                payload: "hello".into(),
                forks: Some(vec![WakeFork {
                    name: "journal".into(),
                    path: "/x/journal.md".into(),
                    trigger: "idle".into(),
                    overlap: false,
                    after: Vec::new(),
                    prompt: "Read the file /x/journal.md".into(),
                }]),
            },
        };
        let line = encode(&resp).unwrap();
        let back: Response = serde_json::from_str(line.trim()).unwrap();
        match back.body {
            ResponseBody::Wake { payload, forks } => {
                assert_eq!(payload, "hello");
                let forks = forks.expect("structured forks survive the round trip");
                assert_eq!(forks.len(), 1);
                assert_eq!(forks[0].name, "journal");
            }
            _ => panic!("wrong body"),
        }
    }

    #[test]
    fn wake_without_forks_still_parses() {
        // An old daemon's Wake (payload only) must keep parsing.
        let line = r#"{"proto":1,"id":1,"type":"wake","payload":"p"}"#;
        let resp: Response = serde_json::from_str(line).unwrap();
        match resp.body {
            ResponseBody::Wake { payload, forks } => {
                assert_eq!(payload, "p");
                assert!(forks.is_none());
            }
            _ => panic!("wrong body"),
        }
    }

    #[test]
    fn opencode_fork_frames_round_trip() {
        let line = r#"{"proto":1,"id":2,"type":"fork_completed","session_id":"s","fork":"journal","run_ref":"ses_abc","status":"completed"}"#;
        let req: Request = serde_json::from_str(line).unwrap();
        match req.body {
            RequestBody::ForkCompleted {
                session_id,
                fork,
                run_ref,
                status,
            } => {
                assert_eq!(session_id, "s");
                assert_eq!(fork, "journal");
                assert_eq!(run_ref, "ses_abc");
                assert_eq!(status, "completed");
            }
            _ => panic!("wrong body"),
        }
    }

    #[test]
    fn shutdown_shape_is_frozen() {
        // Guard: this exact JSON must parse forever.
        let line = r#"{"proto":1,"id":1,"type":"shutdown","drain":true}"#;
        let req: Request = serde_json::from_str(line).unwrap();
        match req.body {
            RequestBody::Shutdown { drain } => assert!(drain),
            _ => panic!("wrong body"),
        }
    }
}
