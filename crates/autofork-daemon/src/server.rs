//! Unix-socket JSONL server: one request line in, one response line out.
//! A `StopWait` request may block for a long time (the asyncRewake Stop hook's
//! long poll) before its single response is written.

use crate::daemon::Daemon;
use autofork_core::protocol::{
    encode, ErrorCode, ForkInfo, Request, RequestBody, Response, ResponseBody, RunInfo,
    SessionInfo, StatusInfo,
};
use autofork_core::PROTO_VERSION;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

pub async fn serve(daemon: Arc<Daemon>, listener: UnixListener) {
    loop {
        tokio::select! {
            _ = daemon.shutdown.notified() => return,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let daemon = daemon.clone();
                tokio::spawn(async move {
                    daemon.connections.fetch_add(1, Ordering::SeqCst);
                    daemon.touch_busy();
                    handle_conn(&daemon, stream).await;
                    daemon.connections.fetch_sub(1, Ordering::SeqCst);
                    daemon.touch_busy();
                });
            }
        }
    }
}

async fn handle_conn(daemon: &Arc<Daemon>, stream: UnixStream) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            // EOF/error between requests is a normal close.
            _ => return,
        };
        if line.trim().is_empty() {
            continue;
        }
        let req = match serde_json::from_str::<Request>(&line) {
            Ok(req) => req,
            Err(e) => {
                let _ = write_body(
                    &mut write,
                    0,
                    ResponseBody::Error {
                        code: ErrorCode::BadRequest,
                        message: format!("bad request: {e}"),
                    },
                )
                .await;
                continue;
            }
        };
        let id = req.id;
        // The frozen shutdown frame works across any proto skew; all other
        // bodies require a proto match.
        if req.proto != PROTO_VERSION && !matches!(req.body, RequestBody::Shutdown { .. }) {
            let _ = write_body(
                &mut write,
                id,
                ResponseBody::Error {
                    code: ErrorCode::ProtoMismatch,
                    message: format!(
                        "daemon speaks proto {PROTO_VERSION}, client sent {}",
                        req.proto
                    ),
                },
            )
            .await;
            continue;
        }

        // A StopWait may park for a long time. Race the park against the read
        // half: if the connection drops before we answer, the Claude process
        // died — that's a lost poll, not a normal answer/cancel.
        if let RequestBody::StopWait(ev) = req.body {
            let session_id = ev.session_id.clone();
            let fut = daemon.handle_stop_wait(ev);
            tokio::pin!(fut);
            let answer = loop {
                tokio::select! {
                    resp = &mut fut => break Some(resp),
                    next = lines.next_line() => match next {
                        // A stray line while parked — ignore and keep waiting.
                        Ok(Some(_)) => continue,
                        // EOF/error before we answered: the poll was lost.
                        _ => break None,
                    },
                }
            };
            match answer {
                Some(body) => {
                    if write_body(&mut write, id, body).await.is_err() {
                        return;
                    }
                }
                None => {
                    daemon.on_poll_lost(&session_id);
                    return;
                }
            }
            continue;
        }

        let body = dispatch(daemon, req.body).await;
        if write_body(&mut write, id, body).await.is_err() {
            return;
        }
    }
}

async fn write_body(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    id: u64,
    body: ResponseBody,
) -> std::io::Result<()> {
    let resp = Response {
        proto: PROTO_VERSION,
        id,
        body,
    };
    let line = encode(&resp).map_err(std::io::Error::other)?;
    write.write_all(line.as_bytes()).await
}

async fn dispatch(daemon: &Arc<Daemon>, body: RequestBody) -> ResponseBody {
    daemon.touch_busy();
    match body {
        RequestBody::Hello { version } => {
            tracing::debug!(client = %version, "hello");
            ResponseBody::HelloInfo {
                version: Daemon::version().to_string(),
            }
        }
        RequestBody::Event(ev) => daemon.handle_event(ev).await,
        RequestBody::StopWait(ev) => daemon.handle_stop_wait(ev).await,
        RequestBody::Status => status(daemon),
        RequestBody::ListForks {
            project_root: _,
            cwd,
        } => {
            let (entries, warnings) = autofork_core::discovery::discover_forks(
                &cwd,
                Some(&daemon.user_forks_root()),
                daemon.claude_dir().as_deref(),
            );
            let items = entries
                .into_iter()
                .map(|e| ForkInfo {
                    skill: autofork_core::discovery::skill_sibling(&e.path),
                    name: e.name,
                    path: e.path,
                    description: e.parsed.def.description.clone(),
                    triggers: e.parsed.def.run_on.iter().map(|r| r.label()).collect(),
                    throttle_secs: e.parsed.def.throttle_secs,
                    after: e.parsed.def.after.clone(),
                    priority: e.parsed.def.priority,
                    overlap: e.parsed.def.overlap,
                    tags: e.parsed.def.tags.clone(),
                    warnings: e
                        .parsed
                        .warnings
                        .iter()
                        .chain(warnings.iter())
                        .cloned()
                        .collect(),
                })
                .collect();
            ResponseBody::ForkList { items }
        }
        RequestBody::Prune => prune(daemon),
        RequestBody::ForkSpawned {
            session_id,
            fork,
            run_ref,
        } => daemon.handle_fork_spawned(&session_id, &fork, &run_ref),
        RequestBody::ForkCompleted {
            session_id,
            fork,
            run_ref,
            status,
        } => daemon.handle_fork_completed(&session_id, &fork, &run_ref, &status),
        RequestBody::Shutdown { drain } => {
            tracing::info!(drain, "shutdown requested");
            let daemon = daemon.clone();
            tokio::spawn(async move {
                daemon.request_shutdown(drain).await;
            });
            ResponseBody::Ack
        }
    }
}

/// The `[stale?]` heuristic — cheap honesty for a mid-turn crash the poll-loss
/// path can't see: open, no parked poll, and idle far past the deadline.
fn is_stale(daemon: &Arc<Daemon>, s: &autofork_core::store::SessionRow, now: i64) -> bool {
    let deadline = daemon
        .cfg_for(Some(&s.project_root))
        .default_idle_deadline_secs;
    !daemon.is_parked(&s.session_id)
        && deadline > 0
        && (now - s.last_activity) > 2 * deadline as i64
}

fn status(daemon: &Arc<Daemon>) -> ResponseBody {
    let now = crate::daemon::now();
    let store = daemon.store.lock().unwrap();
    let sessions = store
        .list_open_sessions()
        .unwrap_or_default()
        .into_iter()
        .map(|s| {
            let stale = is_stale(daemon, &s, now);
            SessionInfo {
                session_id: s.session_id,
                project_root: s.project_root,
                status: s.status.as_str().to_string(),
                last_activity: s.last_activity,
                prompt_tokens: s.prompt_tokens,
                stale,
            }
        })
        .collect();
    let recent_runs = store
        .list_runs(&["issued"], 20)
        .unwrap_or_default()
        .into_iter()
        .map(|r| RunInfo {
            fork: r.fork_name,
            trigger: r.trigger_label,
            session_id: r.session_id,
            state: r.state,
            started_at: r.started_at,
        })
        .collect();
    ResponseBody::StatusInfo(StatusInfo {
        version: Daemon::version().to_string(),
        daemon_proto: PROTO_VERSION,
        pid: std::process::id(),
        sessions,
        recent_runs,
    })
}

/// Close every session the `[stale?]` heuristic flags, right now, instead of
/// waiting for the session-timeout reaper. Safe by construction: a stale
/// session has no parked poll, so nothing is waiting on it — and if its Claude
/// process is somehow still alive, the next hook event reopens it.
fn prune(daemon: &Arc<Daemon>) -> ResponseBody {
    let now = crate::daemon::now();
    let store = daemon.store.lock().unwrap();
    let mut sessions = Vec::new();
    for s in store.list_open_sessions().unwrap_or_default() {
        if !is_stale(daemon, &s, now) {
            continue;
        }
        tracing::info!(session = %s.session_id, "pruning stale session");
        let _ = store.close_session(&s.session_id);
        sessions.push(SessionInfo {
            session_id: s.session_id,
            project_root: s.project_root,
            status: "closed".to_string(),
            last_activity: s.last_activity,
            prompt_tokens: s.prompt_tokens,
            stale: true,
        });
    }
    ResponseBody::Pruned { sessions }
}
