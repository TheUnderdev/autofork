//! Shared daemon state and Claude Code event handling.
//!
//! Since v0.5 the daemon is a pure scheduler: it never spawns fork
//! subprocesses. The asyncRewake Stop hook long-polls via [`handle_stop_wait`];
//! when forks come due the daemon answers with a wake payload the session's own
//! model acts on (spawning `fork` subagents). Fast events (SessionStart,
//! PromptSubmit, SessionEnd) just keep session bookkeeping — and PromptSubmit /
//! SessionEnd cancel any parked stop-wait.

use autofork_core::config::{load_config_at, Config, Paths};
use autofork_core::moments::{idle_deadlines, resolve_context_window, ForkMoment};
use autofork_core::protocol::{Event, EventKind, ResponseBody};
use autofork_core::store::{SessionStatus, Store};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Every fork moment that has elapsed for a session by `up_to`: the context
/// gauge (if known, always "elapsed" the instant the turn ended), every idle
/// deadline whose fire time (`base + d`) has passed, and the wall-clock tick
/// `every:` triggers are matched against (always present — per-fork interval
/// math happens in selection, where the fork's last run is known).
/// `pause_started_at` is `None` on a busy (mid-run) poll; on idle polls it
/// gates `every:` to at most one fire per quiet stretch — a fork whose last
/// run is inside the current pause has seen no activity since, so its
/// interval must not turn a quiet session into a periodic cron.
fn elapsed_moments(
    prompt_tokens: Option<u64>,
    max_tokens: u64,
    base: i64,
    deadlines: &[u64],
    up_to: i64,
    pause_started_at: Option<i64>,
) -> Vec<ForkMoment> {
    let mut moments = Vec::new();
    if let Some(pt) = prompt_tokens {
        moments.push(ForkMoment::Context {
            prompt_tokens: pt,
            max_tokens: Some(max_tokens),
        });
    }
    for &d in deadlines {
        if base + d as i64 <= up_to {
            moments.push(ForkMoment::Idle { deadline_secs: d });
        }
    }
    moments.push(ForkMoment::Tick {
        now: up_to,
        pause_started_at,
    });
    moments
}

pub struct Daemon {
    pub paths: Paths,
    pub store: Mutex<Store>,
    /// Per-session cancellation channels for parked stop-wait long polls.
    /// Sending `()` (or dropping) resolves the parked poll as `Waited`.
    pub waits: Mutex<HashMap<String, oneshot::Sender<()>>>,
    /// When we last issued a wake for a session — used to treat an ambiguous
    /// (prompt-less) PromptSubmit shortly after a wake as a non-waking
    /// continuation (the daemon-side belt).
    pub wake_issued_at: Mutex<HashMap<String, i64>>,
    /// Sessions with a currently-parked stop-wait poll (a liveness heartbeat:
    /// the poll's hook subprocess dies with the Claude process). Values are
    /// reference counts, so the entry exists iff a poll is parked.
    pub parked: Mutex<HashMap<String, usize>>,
    /// Sessions with a pending grace-close after a lost poll, keyed to a
    /// generation so any fresh event cancels the close regardless of the
    /// (whole-second) clock granularity.
    pub pending_close: Mutex<HashMap<String, u64>>,
    pub close_gen: AtomicU64,
    pub connections: AtomicUsize,
    pub last_busy: AtomicI64,
    pub shutdown: tokio::sync::Notify,
}

/// How long after issuing a wake an unattributable PromptSubmit — no prompt
/// text, or a task notification the spawn registry can't match — is assumed to
/// be a continuation rather than genuine user activity. Overridable via
/// `AUTOFORK_WAKE_GRACE_SECS` (tests shorten it).
fn wake_grace_secs() -> i64 {
    std::env::var("AUTOFORK_WAKE_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20)
}

/// After a parked poll drops unanswered, wait this long for a fresh event
/// before closing the session (the Claude process is presumed dead). Overridable
/// via `AUTOFORK_POLL_LOSS_GRACE_MS` (tests shorten it).
fn poll_loss_grace() -> Duration {
    std::env::var("AUTOFORK_POLL_LOSS_GRACE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(90))
}

/// RAII marker that a session has a parked stop-wait poll. Increments on
/// creation and decrements on drop — including when the poll future is dropped
/// mid-await (a lost connection), so `parked` stays accurate on every exit path.
pub struct ParkGuard {
    daemon: Arc<Daemon>,
    session_id: String,
}

impl ParkGuard {
    fn new(daemon: &Arc<Daemon>, session_id: &str) -> Self {
        *daemon
            .parked
            .lock()
            .unwrap()
            .entry(session_id.to_string())
            .or_insert(0) += 1;
        Self {
            daemon: daemon.clone(),
            session_id: session_id.to_string(),
        }
    }
}

impl Drop for ParkGuard {
    fn drop(&mut self) {
        let mut parked = self.daemon.parked.lock().unwrap();
        if let Some(c) = parked.get_mut(&self.session_id) {
            *c -= 1;
            if *c == 0 {
                parked.remove(&self.session_id);
            }
        }
    }
}

impl Daemon {
    pub fn new(paths: Paths, store: Store) -> Arc<Self> {
        Arc::new(Self {
            paths,
            store: Mutex::new(store),
            waits: Mutex::new(HashMap::new()),
            wake_issued_at: Mutex::new(HashMap::new()),
            parked: Mutex::new(HashMap::new()),
            pending_close: Mutex::new(HashMap::new()),
            close_gen: AtomicU64::new(0),
            connections: AtomicUsize::new(0),
            last_busy: AtomicI64::new(now()),
            shutdown: tokio::sync::Notify::new(),
        })
    }

    pub fn touch_busy(&self) {
        self.last_busy.store(now(), Ordering::SeqCst);
    }

    /// The user-level forks root (`<base>/forks`).
    pub fn user_forks_root(&self) -> PathBuf {
        self.paths.base.join("forks")
    }

    /// The user-level `.claude` directory, whose `forks/` and `skills/`
    /// subdirs are extra discovery roots. `AUTOFORK_CLAUDE_DIR` overrides
    /// (tests use it to keep the real home directory out of fixtures).
    pub fn claude_dir(&self) -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("AUTOFORK_CLAUDE_DIR") {
            return Some(PathBuf::from(dir));
        }
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude"))
    }

    /// Effective config for a project.
    pub fn cfg_for(&self, project_root: Option<&Path>) -> Config {
        load_config_at(project_root, &self.paths.user_config()).0
    }

    pub fn version() -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// Cancel a parked stop-wait for a session (resolves it as `Waited`).
    fn cancel_wait(&self, session_id: &str) {
        if let Some(tx) = self.waits.lock().unwrap().remove(session_id) {
            let _ = tx.send(());
        }
    }

    /// Record that a wake was just issued for a session (grace-window belt).
    pub fn note_wake_issued(&self, session_id: &str) {
        self.wake_issued_at
            .lock()
            .unwrap()
            .insert(session_id.to_string(), now());
    }

    /// Whether a session currently has a parked stop-wait poll.
    pub fn is_parked(&self, session_id: &str) -> bool {
        self.parked.lock().unwrap().contains_key(session_id)
    }

    /// Cancel any pending grace-close for a session (a fresh event proves it is
    /// alive). Called on every event and whenever a new poll parks.
    fn clear_pending_close(&self, session_id: &str) {
        self.pending_close.lock().unwrap().remove(session_id);
    }

    /// A parked poll dropped without the daemon answering it (no Wake, no
    /// Waited): the Claude process likely died. After a grace window, close the
    /// session unless a fresh event cancelled the pending close. A later event
    /// re-opens it via the normal upsert path.
    ///
    /// Note: the asyncRewake hook's own 14400s timeout also drops the poll on a
    /// live-but-long-idle session; the grace-close will close it, and the next
    /// real event re-opens it — acceptable self-correction.
    pub fn on_poll_lost(self: &Arc<Self>, session_id: &str) {
        let gen = self.close_gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.pending_close
            .lock()
            .unwrap()
            .insert(session_id.to_string(), gen);
        let daemon = self.clone();
        let sid = session_id.to_string();
        let grace = poll_loss_grace();
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            // Still the same pending close (no fresh event superseded it)?
            {
                let mut pc = daemon.pending_close.lock().unwrap();
                if pc.get(&sid) != Some(&gen) {
                    return;
                }
                pc.remove(&sid);
            }
            let store = daemon.store.lock().unwrap();
            if let Ok(Some(s)) = store.get_session(&sid) {
                if s.status == SessionStatus::Open {
                    tracing::info!(session = %sid, "stop-wait lost, closing session");
                    let _ = store.close_session(&sid);
                }
            }
        });
    }

    /// Whether a wake was issued for this session within the grace window.
    fn recently_woke(&self, session_id: &str, t: i64) -> bool {
        self.wake_issued_at
            .lock()
            .unwrap()
            .get(session_id)
            .is_some_and(|&at| t - at < wake_grace_secs())
    }

    /// Whether this id names one of our own fork-run sessions. Such ids must
    /// never be registered or scheduled: a fork-run session that slips past
    /// the plugin's eligibility check (lost title marker, duplicate plugin
    /// instance, event race at creation) would otherwise become a scheduled
    /// session whose idle forks fork it again — forks breeding forks.
    fn is_fork_run_session(&self, id: &str) -> bool {
        let store = self.store.lock().unwrap();
        store.is_fork_run_ref(id).unwrap_or(false)
    }

    /// Handle one fast lifecycle event; returns the response body.
    pub async fn handle_event(self: &Arc<Self>, ev: Event) -> ResponseBody {
        self.touch_busy();
        // Lifecycle events for a fork-run session are dropped (SessionEnd is
        // let through: it only closes a row, cleaning up after a session that
        // was registered before its spawn frame landed).
        if ev.event != EventKind::SessionEnd && self.is_fork_run_session(&ev.session_id) {
            tracing::info!(session = %ev.session_id, kind = ?ev.event,
                "ignoring event for a fork-run session");
            return ResponseBody::Ack;
        }
        // A fresh event proves the session is alive: cancel any pending
        // lost-poll close.
        self.clear_pending_close(&ev.session_id);
        let t = now();
        let enable_tags = ev.enable_tags.as_ref().map(|v| v.join(","));
        let disable_tags = ev.disable_tags.as_ref().map(|v| v.join(","));
        match ev.event {
            EventKind::SessionStart => {
                let store = self.store.lock().unwrap();
                let _ = store.upsert_session(
                    &ev.session_id,
                    &ev.project_root,
                    &ev.cwd,
                    ev.transcript_path.as_deref(),
                    ev.model.as_deref(),
                    enable_tags.as_deref(),
                    disable_tags.as_deref(),
                    t,
                );
                if let Some(w) = ev.context_window {
                    let _ = store.set_context_window(&ev.session_id, w);
                }
                ResponseBody::Ack
            }
            EventKind::PromptSubmit => {
                // Is this genuine user activity, or a non-waking continuation?
                // An asyncRewake wake reminder sniffs on its marker (the CLI's
                // `waking` field). A task notification is only a continuation
                // when it reports one of the daemon's own fork spawns — any
                // other background task finishing is the session picking real
                // work back up, so it must start a new pause (otherwise idle
                // forks stay latched to the old one and never fire again). The
                // post-wake grace window remains the belt for notifications
                // the spawn registry can't vouch for either way (e.g. a fork
                // that completed before its spawn's Stop was ever ingested).
                let waking = if ev.notif_tool_use_id.is_some() || ev.notif_task_id.is_some() {
                    // Refresh the spawn registry from the transcript BEFORE
                    // classifying: the spawn's tool_use is always on disk by
                    // the time its completion notification is delivered, but
                    // the last Stop's ingest may predate it (observed live: a
                    // Stop racing the transcript flush — or no Stop-wait read
                    // at all between spawn and completion — left the registry
                    // empty, misclassified the fork's own completion as
                    // foreign activity, and re-fired the idle fork forever
                    // after, once per fork run).
                    self.ingest_transcript(&ev);
                    let store = self.store.lock().unwrap();
                    let status = ev.notif_status.as_deref().unwrap_or("");
                    let matched = if autofork_core::notification::is_terminal_status(status) {
                        store
                            .mark_spawn_terminal(
                                &ev.session_id,
                                ev.notif_tool_use_id.as_deref(),
                                ev.notif_task_id.as_deref(),
                                status,
                                t,
                            )
                            .unwrap_or(false)
                    } else {
                        store
                            .is_fork_spawn(
                                &ev.session_id,
                                ev.notif_tool_use_id.as_deref(),
                                ev.notif_task_id.as_deref(),
                            )
                            .unwrap_or(false)
                    };
                    drop(store);
                    !matched && !self.recently_woke(&ev.session_id, t)
                } else {
                    ev.waking
                        .unwrap_or_else(|| !self.recently_woke(&ev.session_id, t))
                };
                {
                    let store = self.store.lock().unwrap();
                    let _ = store.upsert_session(
                        &ev.session_id,
                        &ev.project_root,
                        &ev.cwd,
                        ev.transcript_path.as_deref(),
                        ev.model.as_deref(),
                        enable_tags.as_deref(),
                        disable_tags.as_deref(),
                        t,
                    );
                    let _ = store.set_last_activity(&ev.session_id, t);
                    // Genuine activity begins a new pause: advance the epoch
                    // (releasing per-pause idle latches), reset the baseline,
                    // and drop any dependents still held for the old moment
                    // (their pause is over; they re-select on the next one).
                    if waking {
                        let _ = store.bump_pause_epoch(&ev.session_id);
                        if let Ok(n) = store.clear_pending_deps(&ev.session_id) {
                            if n > 0 {
                                tracing::info!(
                                    session = %ev.session_id,
                                    dropped = n,
                                    "user activity dropped held dependents"
                                );
                            }
                        }
                    }
                }
                // A turn is in flight either way: cancel any parked stop-wait so
                // no wake fires mid-turn.
                self.cancel_wait(&ev.session_id);
                ResponseBody::Ack
            }
            EventKind::SessionEnd => {
                self.cancel_wait(&ev.session_id);
                let store = self.store.lock().unwrap();
                let _ = store.close_session(&ev.session_id);
                ResponseBody::Ack
            }
            // Stop never arrives as a plain event (it is a StopWait long poll).
            EventKind::Stop => ResponseBody::Ack,
        }
    }

    /// The asyncRewake Stop hook's long poll: record activity + the context
    /// gauge, then wait until forks come due (returning a `Wake`) or the wait
    /// is cancelled / the daemon retires (returning `Waited`).
    pub async fn handle_stop_wait(self: &Arc<Self>, ev: Event) -> ResponseBody {
        self.touch_busy();
        // Never park a poll for (or schedule forks on) one of our own
        // fork-run sessions — the breeding-loop guard. Answer Waited so a
        // confused plugin's poll resolves instead of hanging.
        if self.is_fork_run_session(&ev.session_id) {
            tracing::info!(session = %ev.session_id,
                "refusing to schedule a fork-run session");
            return ResponseBody::Waited;
        }
        // A new poll parking proves the session is alive.
        self.clear_pending_close(&ev.session_id);
        let t = now();
        // A busy poll (opencode parks one mid-run so `every:`/context
        // triggers can fire without a pause) must not start a pause or arm
        // idle deadlines — the session is still working.
        let busy = ev.busy.unwrap_or(false);
        let enable_tags = ev.enable_tags.as_ref().map(|v| v.join(","));
        let disable_tags = ev.disable_tags.as_ref().map(|v| v.join(","));
        {
            let store = self.store.lock().unwrap();
            let _ = store.upsert_session(
                &ev.session_id,
                &ev.project_root,
                &ev.cwd,
                ev.transcript_path.as_deref(),
                ev.model.as_deref(),
                enable_tags.as_deref(),
                disable_tags.as_deref(),
                t,
            );
            let _ = store.set_last_activity(&ev.session_id, t);
            if let Some(w) = ev.context_window {
                let _ = store.set_context_window(&ev.session_id, w);
            }
            // The first Stop of a pause sets the baseline; a wake-turn's own
            // Stop keeps the existing one, so idle deadlines don't reset.
            if !busy {
                let _ = store.set_pause_started_at_if_unset(&ev.session_id, t);
            }
        }
        // Clients that track usage themselves (opencode) report the gauge on
        // the event; otherwise it comes from the transcript delta.
        let prompt_tokens = if let Some(gauge) = ev.context_tokens {
            let store = self.store.lock().unwrap();
            let _ = store.set_prompt_tokens(&ev.session_id, gauge);
            Some(gauge)
        } else {
            self.ingest_transcript(&ev)
        };
        let cfg = self.cfg_for(Some(&ev.project_root));

        // Register this wait so PromptSubmit / SessionEnd can cancel it. A
        // stale wait for the same session (if any) is cancelled by the insert.
        let (tx, mut rx) = oneshot::channel::<()>();
        if let Some(old) = self.waits.lock().unwrap().insert(ev.session_id.clone(), tx) {
            let _ = old.send(());
        }
        // Mark the session as having a live parked poll (a liveness heartbeat).
        // The guard is dropped on every exit path, including when this future is
        // dropped because the connection was lost.
        let _park = ParkGuard::new(self, &ev.session_id);

        let Some(session) = ({
            let store = self.store.lock().unwrap();
            store.get_session(&ev.session_id).ok().flatten()
        }) else {
            return ResponseBody::Waited;
        };
        // Held dependents whose predecessors' completions the transcript (or a
        // notification PromptSubmit) just confirmed release right now — this is
        // the Stop that follows the completion's relay turn, so the reports are
        // already in the session's context.
        if let Some((payload, forks)) = crate::planner::release_due(self, &session) {
            return ResponseBody::Wake {
                payload,
                forks: Some(forks),
            };
        }
        // Idle timing is measured from the pause baseline (the first Stop of
        // this pause), so a wake-turn's own Stop doesn't restart the clock.
        let baseline = session.pause_started_at.unwrap_or(t);
        // Context thresholds are judged against the session's real window: an
        // explicitly reported window (opencode's model catalog) wins; else the
        // hook-reported model id keeps Claude Code's `[1m]` marker (the session
        // row holds the latest non-null value), and an oversized gauge bumps
        // an under-assumed window.
        let max_tokens = resolve_context_window(
            session.model.as_deref(),
            prompt_tokens,
            session.context_window,
        );

        // Idle deadlines (seconds from the baseline) this session's forks
        // need — none on a busy poll (the session isn't pausing) — plus the
        // absolute instants at which `every:` intervals next elapse.
        let (entries, _) = autofork_core::discovery::discover_forks(
            &session.cwd,
            Some(&self.user_forks_root()),
            self.claude_dir().as_deref(),
        );
        let deadlines = if busy {
            Vec::new()
        } else {
            idle_deadlines(
                entries.iter().map(|e| &e.parsed.def),
                cfg.default_idle_deadline_secs,
            )
        };
        let every_times = {
            let ran: std::collections::HashMap<String, Option<i64>> = {
                let store = self.store.lock().unwrap();
                store
                    .roster(&session.session_id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|e| (e.fork_name, e.ran_at))
                    .collect()
            };
            autofork_core::moments::every_fire_times(
                entries.iter().map(|e| (e.name.as_str(), &e.parsed.def)),
                |name| ran.get(name).copied().flatten(),
                session.created_at,
            )
        };

        // Phase A: find the first instant ≥1 fork is due (read-only eval).
        // Context thresholds are known immediately (the turn just ended);
        // idle forks come due as their deadlines elapse, `every:` intervals
        // at their absolute fire instants.
        // Busy polls carry no pause: `every:` fires freely mid-run. Idle
        // polls carry the pause start, capping `every:` at one fire per
        // quiet stretch.
        let pause_gate = if busy { None } else { Some(baseline) };
        let due_now = |slf: &Arc<Self>| -> bool {
            let moments = elapsed_moments(
                prompt_tokens,
                max_tokens,
                baseline,
                &deadlines,
                now(),
                pause_gate,
            );
            !crate::planner::select_forks(slf, &session, &cfg, &moments).is_empty()
        };

        let fire_instants: Vec<i64> = {
            let mut v: Vec<i64> = deadlines.iter().map(|&d| baseline + d as i64).collect();
            v.extend(every_times);
            v.sort_unstable();
            v.dedup();
            v
        };
        let mut due = due_now(self);
        if !due {
            for &fire_at in &fire_instants {
                let wait = (fire_at - now()).max(0) as u64;
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(wait)) => {
                        if due_now(self) { due = true; break; }
                    }
                    _ = &mut rx => return ResponseBody::Waited,
                    _ = self.shutdown.notified() => return ResponseBody::Waited,
                }
            }
        }
        if !due {
            // No deadline yielded anything; park until cancelled or shutdown.
            tokio::select! {
                _ = &mut rx => {}
                _ = self.shutdown.notified() => {}
            }
            return ResponseBody::Waited;
        }

        // Phase B: debounce so near-simultaneous forks batch into one wake.
        // Cancellation / shutdown during the window wins (nothing is stamped).
        if cfg.wake_debounce_secs > 0 {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(cfg.wake_debounce_secs)) => {}
                _ = &mut rx => return ResponseBody::Waited,
                _ = self.shutdown.notified() => return ResponseBody::Waited,
            }
        }

        // Phase C: re-evaluate over every moment elapsed by now (deadlines that
        // landed during the debounce join the batch), then issue one wake —
        // stamping throttles and latches at this point.
        let moments = elapsed_moments(
            prompt_tokens,
            max_tokens,
            baseline,
            &deadlines,
            now(),
            pause_gate,
        );
        let selected = crate::planner::select_forks(self, &session, &cfg, &moments);
        if let Some((payload, forks)) = crate::planner::build_wake(self, &session, selected) {
            return ResponseBody::Wake {
                payload,
                forks: Some(forks),
            };
        }
        // Nothing survived re-evaluation; park.
        tokio::select! {
            _ = &mut rx => {}
            _ = self.shutdown.notified() => {}
        }
        ResponseBody::Waited
    }

    /// An opencode fork run started: record it in the spawn registry, keyed by
    /// the fork session id in the `tool_use_id` role. The registry drives
    /// `after`-dependency release and run bookkeeping, same as a Claude Code
    /// spawn observed in the transcript.
    pub fn handle_fork_spawned(
        self: &Arc<Self>,
        session_id: &str,
        fork: &str,
        run_ref: &str,
    ) -> ResponseBody {
        self.touch_busy();
        let store = self.store.lock().unwrap();
        let _ = store.record_spawn(session_id, run_ref, Some(fork), now());
        ResponseBody::Ack
    }

    /// An opencode fork run finished. Mark it terminal, then nudge the
    /// session's parked stop-wait (resolving it `Waited`): the plugin re-parks
    /// while the session stays idle, and the fresh poll's entry check releases
    /// any `after` dependents this completion unblocked. (Claude Code gets the
    /// same effect from the completion notification's relay turn ending in a
    /// new Stop poll; opencode has no such turn, hence the nudge.)
    pub fn handle_fork_completed(
        self: &Arc<Self>,
        session_id: &str,
        fork: &str,
        run_ref: &str,
        status: &str,
    ) -> ResponseBody {
        self.touch_busy();
        {
            let store = self.store.lock().unwrap();
            let matched = store
                .mark_spawn_terminal(session_id, Some(run_ref), None, status, now())
                .unwrap_or(false);
            tracing::debug!(session = %session_id, fork, run_ref, status, matched,
                "opencode fork completion");
        }
        self.cancel_wait(session_id);
        ResponseBody::Ack
    }

    /// Read the transcript delta (updating the stored offset): refresh the
    /// context gauge, record fork spawns and their task ids, and mark spawns
    /// terminal on completion notifications. Returns the session's best-known
    /// prompt token count, or `None` when unavailable.
    fn ingest_transcript(&self, ev: &Event) -> Option<u64> {
        let transcript = ev.transcript_path.as_deref()?;
        let session = {
            let store = self.store.lock().unwrap();
            store.get_session(&ev.session_id).ok().flatten()?
        };
        match crate::transcript::read_delta(transcript, session.transcript_offset) {
            Ok(delta) => {
                let t = now();
                let store = self.store.lock().unwrap();
                for (tool_use_id, fork_name) in &delta.spawns {
                    tracing::debug!(session = %ev.session_id, tool_use_id, fork = ?fork_name,
                        "fork spawn observed");
                    let _ =
                        store.record_spawn(&ev.session_id, tool_use_id, fork_name.as_deref(), t);
                }
                for (tool_use_id, task_id) in &delta.task_ids {
                    let _ = store.set_spawn_task_id(&ev.session_id, tool_use_id, task_id);
                }
                for n in &delta.notifications {
                    let Some(status) = n
                        .status
                        .as_deref()
                        .filter(|s| autofork_core::notification::is_terminal_status(s))
                    else {
                        continue;
                    };
                    if let Ok(true) = store.mark_spawn_terminal(
                        &ev.session_id,
                        n.tool_use_id.as_deref(),
                        n.task_id.as_deref(),
                        status,
                        t,
                    ) {
                        tracing::debug!(session = %ev.session_id, status,
                            tool_use_id = ?n.tool_use_id, "fork completion observed");
                    }
                }
                let _ = store.set_transcript_gauge(
                    &ev.session_id,
                    delta.new_offset,
                    delta.prompt_tokens,
                );
                delta.prompt_tokens.or(session.prompt_tokens)
            }
            Err(e) => {
                tracing::debug!(error = %e, "transcript delta unavailable");
                session.prompt_tokens
            }
        }
    }

    /// True when the daemon has nothing to live for right now (no open
    /// connection, which includes any parked stop-wait).
    pub fn is_quiet(&self) -> bool {
        self.connections.load(Ordering::SeqCst) == 0
    }

    /// Exit once quiet for the configured period.
    pub async fn quiet_reaper(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let quiet_period = self.cfg_for(None).quiet_period_secs as i64;
            let quiet_since = now() - self.last_busy.load(Ordering::SeqCst);
            if self.is_quiet() && quiet_since >= quiet_period {
                tracing::info!("quiet for {quiet_since}s, exiting");
                self.shutdown.notify_waiters();
                return;
            }
        }
    }

    /// Begin shutdown. Parked stop-waits resolve (`Waited`) via the shutdown
    /// notify; `drain` is accepted for wire compatibility but there are no
    /// in-flight runs to drain.
    pub async fn request_shutdown(self: &Arc<Self>, _drain: bool) {
        self.shutdown.notify_waiters();
    }
}
