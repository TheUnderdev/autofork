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
    /// `cont` (wire name `continue`, additive): the fork's report ended with
    /// the chain sentinel — re-arm it for another run this pause (honored
    /// only for `chain: true` forks, up to their chain limit).
    ForkCompleted {
        session_id: String,
        fork: String,
        run_ref: String,
        status: String,
        #[serde(default, rename = "continue", skip_serializing_if = "Option::is_none")]
        cont: Option<bool>,
    },
    /// The codex Stop hook's goal fast path: evaluate NOW whether any
    /// `chain: true` fork is due at this pause's first Stop (the `idle: 0s`
    /// goal recipe) and, if so, select-and-stamp exactly those and return
    /// them as structured specs (a `Due` response). Non-chain forks are left
    /// unstamped for the regular parked poll. Additive frame (proto 1); old
    /// daemons answer `Error`, which callers treat as "nothing due".
    PeekDue { session_id: String },
    /// Spool a headless fork run's report for later silent delivery (the
    /// Claude Code headless runner). Delivered and cleared by `TakeReports`.
    SpoolReport {
        session_id: String,
        fork: String,
        text: String,
    },
    /// Take (and clear) the spooled reports for a session — called by the
    /// UserPromptSubmit hook to deliver them as additionalContext. Additive
    /// frame; old daemons answer `Error`, treated as "none".
    TakeReports { session_id: String },
    /// `flush_on_close`: the SessionEnd hook asks for every idle fork that
    /// has not yet fired this pause; the daemon selects (throttles, tags and
    /// the runaway breaker still apply), stamps, and returns them ALL — roots
    /// and dependents — in execution order, `after` report-piping preds
    /// filled in, for a detached end-runner to execute after the session
    /// dies. Answered with `Due`. Additive frame.
    TakeFinalRuns { session_id: String },
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
    /// SessionEnd reason, when the client reports one (Claude Code:
    /// `clear`/`logout`/`prompt_input_exit`/`other`; opencode:
    /// `disposed`/`deleted`). Exposed to lifecycle hooks as
    /// `AUTOFORK_END_REASON`. Additive field (no proto bump).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
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
    /// The notification's `<result>` carries the chain sentinel on a line of
    /// its own: the fork asks to run again. Additive field; the daemon honors
    /// it only when the completion matches one of its own spawns of a
    /// `chain: true` fork.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notif_continue: Option<bool>,
    /// The session's context gauge in tokens, reported by clients that track
    /// usage themselves (opencode) instead of exposing a transcript to parse.
    /// Takes precedence over transcript parsing when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    /// The session's real context window in tokens, reported by clients that
    /// know it (opencode reads the model's `limit.context` from its catalog).
    /// Wins over the model-id window heuristics — opencode model ids never
    /// carry Claude Code's `[1m]` marker, so without this a 1M session was
    /// judged against the 200k default (`context_used: 75%` fired at 150k =
    /// 15% of the real window). Additive field (no proto bump).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// The harness this event comes from (`"opencode"`; absent = Claude Code).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// For a StopWait: the session is mid-run, not pausing (opencode parks a
    /// poll even while busy so `every:`/context triggers can fire mid-run).
    /// A busy poll never arms idle deadlines or sets the pause baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub busy: Option<bool>,
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
    /// Answer to `PeekDue`: the chain forks selected to run right now
    /// (empty = nothing due on the fast path). Additive.
    Due {
        forks: Vec<WakeFork>,
    },
    /// Answer to `TakeReports`: the spooled report blocks, oldest first
    /// (now cleared). Additive.
    Reports {
        blocks: Vec<String>,
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
    /// Fork runs currently in flight: spawn observed, completion not yet.
    /// The "is it safe to close this session?" answer. Additive field.
    #[serde(default)]
    pub running: Vec<RunInfo>,
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
    /// `chain: true` fork: the client should honor a trailing chain sentinel
    /// in the run's report (turn-triggering injection + `continue` on the
    /// completion frame). Additive field (no proto bump).
    #[serde(default)]
    pub chain: bool,
    /// Model to run the fork with, already resolved for the session's client
    /// (fork frontmatter over config `[fork_models]`). `None` = inherit the
    /// session's model. Additive field (no proto bump).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Fallback models tried in order when a run on `model` fails ("if the
    /// first option is not available, the next one is used"). Additive.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_fallbacks: Vec<String>,
    /// Operation mode for the run, resolved like `model` (permission mode /
    /// sandbox / agent, per client). `None` = the client's default. Additive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
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
    /// Ordering weight (`priority:`; 0 = default wave). Additive field (no
    /// proto bump).
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub overlap: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    /// For a skill-attached fork (`FORK.md` next to `SKILL.md`): the
    /// SKILL.md path. Additive field (no proto bump).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<PathBuf>,
    /// `chain: true` fork (may request re-runs via the sentinel). Additive
    /// field (no proto bump).
    #[serde(default)]
    pub chain: bool,
    /// `gate: true` fork (holds other idle forks while unsettled). Additive
    /// field (no proto bump).
    #[serde(default)]
    pub gate: bool,
    /// Display form of the fork's `model:` (scalar or "client: value, …").
    /// Additive field (no proto bump).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Display form of the fork's `mode:`. Additive field (no proto bump).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
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
                reason: None,
                model: None,
                enable_tags: None,
                disable_tags: None,
                waking: None,
                notif_tool_use_id: None,
                notif_task_id: None,
                notif_status: None,
                notif_continue: None,
                context_tokens: None,
                context_window: None,
                client: None,
                busy: None,
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
                    chain: false,
                    model: None,
                    model_fallbacks: Vec::new(),
                    mode: None,
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
                cont,
            } => {
                assert_eq!(session_id, "s");
                assert_eq!(fork, "journal");
                assert_eq!(run_ref, "ses_abc");
                assert_eq!(status, "completed");
                // Old plugins omit the field entirely.
                assert_eq!(cont, None);
            }
            _ => panic!("wrong body"),
        }

        // The chain variant rides the wire name `continue`.
        let line = r#"{"proto":1,"id":3,"type":"fork_completed","session_id":"s","fork":"goal","run_ref":"ses_g","status":"completed","continue":true}"#;
        let req: Request = serde_json::from_str(line).unwrap();
        match req.body {
            RequestBody::ForkCompleted { cont, .. } => assert_eq!(cont, Some(true)),
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
