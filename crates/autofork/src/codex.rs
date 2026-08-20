//! `autofork codex …`: the OpenAI Codex CLI integration.
//!
//! Codex ships a lifecycle-hooks system shaped like Claude Code's
//! (`SessionStart` / `UserPromptSubmit` / `SessionEnd` command hooks reading
//! JSON on stdin), but its hooks run *synchronously* — a slow Stop hook
//! blocks the session — so there is no asyncRewake analogue to park the idle
//! long-poll in. Instead the SessionStart hook spawns a detached **waiter**
//! process per session (`autofork codex waiter`, hidden) that:
//!
//! - tails the session's rollout JSONL (the hook hands us its path as
//!   `transcript_path`) for `task_started` / `task_complete` / `token_count`
//!   lines — busy/idle transitions and the context gauge/window, with no
//!   further hooks involved;
//! - parks the `stop-wait` long-poll against the daemon (idle or busy mode,
//!   like the opencode plugin's parked subprocess);
//! - executes due forks natively: `codex exec fork <parent> <prompt> --json`
//!   forks the conversation into a fresh thread (full history inherited,
//!   parent rollout untouched) and runs the fork body as its first turn;
//! - delivers each report into the parent via the app-server
//!   `thread/queue/add` RPC — codex's durable message queue, drained by the
//!   parent's own process when the session next goes idle;
//! - exits when the codex process dies (pid watch) or the SessionEnd hook
//!   leaves a tombstone.
//!
//! Codex hooks are trust-gated (untrusted hooks are silently skipped), so
//! `autofork codex install` both merges our hooks into `$CODEX_HOME/hooks.json`
//! and trusts them through the same `hooks/list` + `config/batchWrite` RPCs
//! the codex TUI's `/hooks` command uses.

use crate::client::{spawn_daemon_detached, Client};
use autofork_core::config::Paths;
use autofork_core::protocol::{Event, EventKind, RequestBody, ResponseBody, WakeFork};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The client name stamped on every event this integration sends.
const CLIENT: &str = "codex";

/// How often the waiter polls the rollout file and the codex pid.
const TAIL_INTERVAL: Duration = Duration::from_millis(500);

/// Wall-clock cap on one fork run (`codex exec fork` child), overridable via
/// `AUTOFORK_CODEX_FORK_TIMEOUT_SECS`.
const FORK_TIMEOUT_SECS: u64 = 1800;

/// Report block header injected into the parent (keep the marker in sync with
/// `WAKE_MARKER` in wake.rs — the prompt-submit sniff keys on it, so the
/// queue-drained report turn is classified non-waking).
fn report_block(fork: &str, trigger: &str, status: &str, body: &str) -> String {
    format!("---\nsource: autofork\nfork: {fork} (trigger: {trigger}) — {status}\n---\n{body}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CxHookKind {
    /// Codex `SessionStart` hook: register the session, spawn the waiter.
    SessionStart,
    /// Codex `UserPromptSubmit` hook: cancels any parked stop-wait, bumps the
    /// pause epoch (unless the prompt is one of our own report injections).
    PromptSubmit,
    /// Codex `SessionEnd` hook: close the session, tombstone the waiter.
    SessionEnd,
}

/// The subset of codex hook stdin we consume. Unknown fields ignored.
#[derive(Debug, Deserialize)]
struct CxInput {
    session_id: String,
    /// The rollout JSONL path — codex calls it the transcript, we tail it.
    #[serde(default)]
    transcript_path: Option<PathBuf>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    /// SessionStart: `startup` / `resume` / `clear` / `compact`.
    #[serde(default)]
    source: Option<String>,
    /// SessionEnd reason.
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    model: Option<String>,
    /// `default` / `acceptEdits` / `plan` / `dontAsk` / `bypassPermissions` —
    /// mapped onto the fork child's sandbox flags.
    #[serde(default)]
    permission_mode: Option<String>,
    /// The submitted prompt (UserPromptSubmit) for the waking sniff.
    #[serde(default)]
    prompt: Option<String>,
}

pub fn run_hook(kind: CxHookKind) {
    // Never break the host session, whatever happens in here.
    let _ = run_hook_inner(kind);
}

fn run_hook_inner(kind: CxHookKind) -> Option<()> {
    // Recursion guard: our own `codex exec fork` children run with these set,
    // and their hook events must not register fork runs as real sessions.
    if std::env::var_os("AUTOFORK_FORK").is_some()
        || std::env::var_os("AUTOFORK_SESSION_ID").is_some()
    {
        return Some(());
    }
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw).ok()?;
    let input: CxInput = serde_json::from_str(&raw).ok()?;
    let paths = Paths::from_env()?;

    let cwd = input.cwd.clone().or_else(|| std::env::current_dir().ok())?;
    let root = autofork_core::project::project_root(&cwd);

    let event = |ev: EventKind| Event {
        event: ev,
        session_id: input.session_id.clone(),
        transcript_path: None, // rollout format is not a Claude Code transcript
        cwd: cwd.clone(),
        project_root: root.clone(),
        source: input.source.clone(),
        reason: input.reason.clone(),
        model: input.model.clone(),
        enable_tags: crate::hook::tags_from_env("AUTOFORK_ENABLE_TAGS"),
        disable_tags: crate::hook::tags_from_env("AUTOFORK_DISABLE_TAGS"),
        waking: None,
        notif_tool_use_id: None,
        notif_task_id: None,
        notif_status: None,
        notif_continue: None,
        context_tokens: None,
        context_window: None,
        client: Some(CLIENT.to_string()),
        busy: None,
    };

    match kind {
        CxHookKind::SessionStart => {
            let client = Client::connect_or_spawn(&paths, Duration::from_secs(5)).ok()?;
            let mut client = client.ensure_current_version(&paths).ok()?;
            let _ = client.request(RequestBody::Event(event(EventKind::SessionStart)));
            // A fresh session start supersedes any earlier tombstone.
            let _ = std::fs::remove_file(waiter_tombstone(&paths, &input.session_id));
            spawn_waiter(&paths, &input, &cwd);
        }
        CxHookKind::PromptSubmit => {
            // Hard budget; codex hooks block the turn start.
            let Ok(mut client) = Client::connect(&paths, Duration::from_millis(1500)) else {
                spawn_daemon_detached(&paths);
                return Some(());
            };
            let mut ev = event(EventKind::PromptSubmit);
            // Sniff the prompt: our queued fork reports carry the wake marker
            // and are non-waking continuations, everything else is genuine
            // user activity.
            if let Some(p) = input.prompt.as_deref() {
                ev.waking = Some(!p.contains(autofork_core::wake::WAKE_MARKER));
            }
            let _ = client.request(RequestBody::Event(ev));
            // Belt: a waiter that died mid-session comes back on the next
            // genuine prompt (the flock makes this a no-op when one lives).
            spawn_waiter(&paths, &input, &cwd);
        }
        CxHookKind::SessionEnd => {
            // Tombstone first: the waiter must not re-park for a dead session.
            let _ = std::fs::write(waiter_tombstone(&paths, &input.session_id), b"");
            let mut client = Client::connect_or_spawn(&paths, Duration::from_secs(5)).ok()?;
            let _ = client.request(RequestBody::Event(event(EventKind::SessionEnd)));
        }
    }
    Some(())
}

// ---------------------------------------------------------------------------
// Waiter spawn + identity
// ---------------------------------------------------------------------------

fn run_dir(paths: &Paths) -> PathBuf {
    paths.base.join("run")
}

/// A short filesystem-safe tag for a session id.
fn session_tag(session_id: &str) -> String {
    session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(48)
        .collect()
}

fn waiter_lock(paths: &Paths, session_id: &str) -> PathBuf {
    run_dir(paths).join(format!("codex-{}.lock", session_tag(session_id)))
}

fn waiter_tombstone(paths: &Paths, session_id: &str) -> PathBuf {
    run_dir(paths).join(format!("codex-{}.end", session_tag(session_id)))
}

/// Spawn the detached waiter for this session. The waiter's own flock makes
/// duplicate spawns exit immediately, so this is safe to call from every hook.
fn spawn_waiter(paths: &Paths, input: &CxInput, cwd: &Path) {
    let Some(rollout) = input.transcript_path.as_ref() else {
        return;
    };
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let log_path = paths.base.join("logs/codex-waiter.log");
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::create_dir_all(run_dir(paths));
    let Ok(log) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    else {
        return;
    };
    let Ok(log2) = log.try_clone() else { return };

    // The hook's parent is the codex process itself (codex spawns hook
    // commands directly, no shell) — the waiter's liveness anchor.
    let codex_pid = std::os::unix::process::parent_id();

    let mut cmd = Command::new(exe);
    cmd.arg("codex")
        .arg("waiter")
        .arg("--session")
        .arg(&input.session_id)
        .arg("--rollout")
        .arg(rollout)
        .arg("--codex-pid")
        .arg(codex_pid.to_string())
        .arg("--cwd")
        .arg(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2));
    if let Some(m) = &input.model {
        cmd.arg("--model").arg(m);
    }
    if let Some(p) = &input.permission_mode {
        cmd.arg("--permission-mode").arg(p);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let _ = cmd.spawn();
}

// ---------------------------------------------------------------------------
// Rollout tailing
// ---------------------------------------------------------------------------

/// What the waiter learns from the rollout tail.
#[derive(Debug, Default, Clone)]
struct RolloutState {
    /// A turn is in flight (`task_started` seen after the last
    /// `task_complete`).
    busy: bool,
    /// Context gauge: input + output tokens of the last recorded turn.
    context_tokens: Option<u64>,
    /// The model's real context window, straight from codex.
    context_window: Option<u64>,
    /// Model id from the latest `turn_context`.
    model: Option<String>,
}

/// Incremental reader over the rollout JSONL: keeps a byte offset and a
/// partial-line buffer, applies complete lines to a [`RolloutState`].
struct RolloutTail {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
}

impl RolloutTail {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            partial: Vec::new(),
        }
    }

    /// Read whatever the rollout has appended and fold it into `state`.
    /// Returns true when at least one line was applied.
    fn poll(&mut self, state: &mut RolloutState) -> bool {
        let Ok(mut f) = std::fs::File::open(&self.path) else {
            return false;
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            // Truncated/rotated: start over.
            self.offset = 0;
            self.partial.clear();
        }
        if len == self.offset {
            return false;
        }
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return false;
        }
        let mut buf = Vec::new();
        let mut reader = BufReader::new(&mut f);
        if reader.read_to_end(&mut buf).is_err() {
            return false;
        }
        self.offset += buf.len() as u64;
        let mut applied = false;
        let mut data = std::mem::take(&mut self.partial);
        data.extend_from_slice(&buf);
        let mut rest = &data[..];
        while let Some(nl) = rest.iter().position(|b| *b == b'\n') {
            let line = &rest[..nl];
            rest = &rest[nl + 1..];
            if apply_rollout_line(line, state) {
                applied = true;
            }
        }
        self.partial = rest.to_vec();
        applied
    }
}

/// Fold one rollout JSONL line into the state. Returns true when the line
/// changed anything we track.
fn apply_rollout_line(line: &[u8], state: &mut RolloutState) -> bool {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else {
        return false;
    };
    let payload = &v["payload"];
    match v["type"].as_str() {
        Some("event_msg") => match payload["type"].as_str() {
            // Wire names are codex's v1-legacy: task_* with turn_* aliases.
            Some("task_started") | Some("turn_started") => {
                state.busy = true;
                if let Some(w) = payload["model_context_window"].as_u64() {
                    state.context_window = Some(w);
                }
                true
            }
            Some("task_complete") | Some("turn_complete") | Some("turn_aborted") => {
                state.busy = false;
                true
            }
            Some("token_count") => {
                let info = &payload["info"];
                let last = &info["last_token_usage"];
                let input = last["input_tokens"].as_u64();
                let output = last["output_tokens"].as_u64();
                if let Some(i) = input {
                    state.context_tokens = Some(i + output.unwrap_or(0));
                }
                if let Some(w) = info["model_context_window"].as_u64() {
                    state.context_window = Some(w);
                }
                true
            }
            _ => false,
        },
        Some("turn_context") => {
            if let Some(m) = payload["model"].as_str() {
                state.model = Some(m.to_string());
            }
            true
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Waiter
// ---------------------------------------------------------------------------

/// Per-run bookkeeping shared between the waiter loop and runner threads.
#[derive(Default)]
struct WaiterShared {
    /// Live run count per fork name (the `overlap: false` gate).
    live_by_fork: HashMap<String, usize>,
    /// Last report per fork, appended to `after`-dependent prompts.
    reports: HashMap<String, String>,
}

pub struct WaiterArgs {
    pub session: String,
    pub rollout: PathBuf,
    pub codex_pid: u32,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
}

/// `autofork codex waiter`: the per-session poll owner and fork executor.
pub fn run_waiter(args: WaiterArgs) {
    let Some(paths) = Paths::from_env() else {
        return;
    };
    // Singleton per session: the flock is held for the waiter's life.
    let _ = std::fs::create_dir_all(run_dir(&paths));
    let Some(_lock) = crate::client::try_flock(&waiter_lock(&paths, &args.session)) else {
        return; // another waiter lives
    };
    eprintln!(
        "[codex-waiter] session {} start (pid {}, codex pid {})",
        args.session,
        std::process::id(),
        args.codex_pid
    );
    waiter_loop(&paths, &args);
    eprintln!("[codex-waiter] session {} exit", args.session);
}

fn codex_alive(pid: u32) -> bool {
    // kill(pid, 0): 0 or EPERM = alive.
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn waiter_loop(paths: &Paths, args: &WaiterArgs) {
    let root = autofork_core::project::project_root(&args.cwd);
    let mut tail = RolloutTail::new(args.rollout.clone());
    let mut state = RolloutState {
        model: args.model.clone(),
        ..Default::default()
    };
    // Catch up on the existing rollout before the first park.
    tail.poll(&mut state);

    let shared = Arc::new(Mutex::new(WaiterShared::default()));
    let (tx, rx) = std::sync::mpsc::channel::<(u64, Option<ResponseBody>)>();
    let mut generation: u64 = 0;
    let mut parked_mode: Option<bool> = None; // Some(busy) of the current poll
    let mut backoff = Duration::from_secs(1);
    let mut last_park: Instant = Instant::now();
    let tombstone = waiter_tombstone(paths, &args.session);

    let park = |generation: u64,
                ev: Event,
                tx: std::sync::mpsc::Sender<(u64, Option<ResponseBody>)>,
                paths: Paths| {
        std::thread::spawn(move || {
            let res = (|| {
                let client = Client::connect_or_spawn(&paths, Duration::from_secs(10)).ok()?;
                let mut client = client.ensure_current_version(&paths).ok()?;
                client.stop_wait(ev).ok()
            })();
            let _ = tx.send((generation, res));
        });
    };

    let build_event = |state: &RolloutState, busy: bool| Event {
        event: EventKind::Stop,
        session_id: args.session.clone(),
        transcript_path: None,
        cwd: args.cwd.clone(),
        project_root: root.clone(),
        source: None,
        reason: None,
        model: state.model.clone(),
        enable_tags: crate::hook::tags_from_env("AUTOFORK_ENABLE_TAGS"),
        disable_tags: crate::hook::tags_from_env("AUTOFORK_DISABLE_TAGS"),
        waking: None,
        notif_tool_use_id: None,
        notif_task_id: None,
        notif_status: None,
        notif_continue: None,
        context_tokens: state.context_tokens,
        context_window: state.context_window,
        client: Some(CLIENT.to_string()),
        busy: busy.then_some(true),
    };

    loop {
        if !codex_alive(args.codex_pid) || tombstone.exists() {
            let _ = std::fs::remove_file(&tombstone);
            return;
        }
        tail.poll(&mut state);

        // (Re)park when the mode changed or no poll is parked.
        if parked_mode != Some(state.busy) {
            generation += 1;
            parked_mode = Some(state.busy);
            last_park = Instant::now();
            park(
                generation,
                build_event(&state, state.busy),
                tx.clone(),
                Paths::new(paths.base.clone()),
            );
        }

        match rx.recv_timeout(TAIL_INTERVAL) {
            Ok((gen, res)) if gen == generation => {
                let woke = matches!(&res, Some(ResponseBody::Wake { .. }));
                if let Some(ResponseBody::Wake { forks, .. }) = res {
                    for spec in forks.unwrap_or_default() {
                        run_fork(paths, args, &state, spec, Arc::clone(&shared));
                    }
                }
                // Re-park for whatever the session is doing now, with a
                // backoff so a misbehaving daemon can't spin us.
                let long_park = last_park.elapsed() > Duration::from_secs(5);
                backoff = if woke || long_park {
                    Duration::from_secs(1)
                } else {
                    (backoff * 2).min(Duration::from_secs(60))
                };
                std::thread::sleep(backoff);
                parked_mode = None; // force a re-park on the next iteration
            }
            Ok(_) => {} // superseded poll resolving late — ignore
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

// ---------------------------------------------------------------------------
// Fork execution
// ---------------------------------------------------------------------------

fn codex_bin() -> String {
    std::env::var("AUTOFORK_CODEX_BIN").unwrap_or_else(|_| "codex".to_string())
}

/// Map the parent's permission mode onto `codex exec` sandbox flags.
fn sandbox_args(permission_mode: Option<&str>) -> Vec<&'static str> {
    match permission_mode {
        Some("bypassPermissions") => vec!["--dangerously-bypass-approvals-and-sandbox"],
        Some("plan") => vec!["--sandbox", "read-only"],
        _ => vec!["--sandbox", "workspace-write"],
    }
}

fn fork_timeout() -> Duration {
    let secs = std::env::var("AUTOFORK_CODEX_FORK_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(FORK_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// Execute one fork run in its own thread: fork the parent thread, stream the
/// run, report frames to the daemon, deliver the report into the parent.
fn run_fork(
    paths: &Paths,
    args: &WaiterArgs,
    state: &RolloutState,
    spec: WakeFork,
    shared: Arc<Mutex<WaiterShared>>,
) {
    {
        let mut sh = shared.lock().unwrap();
        if !spec.overlap && sh.live_by_fork.get(&spec.name).copied().unwrap_or(0) > 0 {
            return;
        }
        *sh.live_by_fork.entry(spec.name.clone()).or_insert(0) += 1;
    }
    let paths = Paths::new(paths.base.clone());
    let session = args.session.clone();
    let cwd = args.cwd.clone();
    let model = state.model.clone().or_else(|| args.model.clone());
    let permission_mode = args.permission_mode.clone();
    std::thread::spawn(move || {
        let name = spec.name.clone();
        run_fork_inner(
            &paths,
            &session,
            &cwd,
            model.as_deref(),
            permission_mode.as_deref(),
            spec,
            &shared,
        );
        let mut sh = shared.lock().unwrap();
        if let Some(n) = sh.live_by_fork.get_mut(&name) {
            *n = n.saturating_sub(1);
        }
    });
}

fn run_fork_inner(
    paths: &Paths,
    session: &str,
    cwd: &Path,
    model: Option<&str>,
    permission_mode: Option<&str>,
    spec: WakeFork,
    shared: &Arc<Mutex<WaiterShared>>,
) {
    let mut prompt = spec.prompt.clone();
    for pred in spec.after.iter() {
        let report = shared.lock().unwrap().reports.get(pred).cloned();
        if let Some(r) = report {
            prompt.push_str(&format!(
                "\n\nThis fork runs after '{pred}'; its report follows so you can build on it:\n{r}"
            ));
        }
    }

    let mut cmd = Command::new(codex_bin());
    cmd.arg("exec")
        .arg("--skip-git-repo-check")
        .arg("--json")
        .arg("-C")
        .arg(cwd);
    for a in sandbox_args(permission_mode) {
        cmd.arg(a);
    }
    if let Some(m) = model {
        cmd.arg("-m").arg(m);
    }
    cmd.arg("fork").arg(session).arg(&prompt);
    cmd.env("AUTOFORK_FORK", "1")
        .env("AUTOFORK_SESSION_ID", session)
        .env("AUTOFORK_FORK_NAME", &spec.name)
        .env("AUTOFORK_TRIGGER", &spec.trigger)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[codex-waiter] fork '{}' spawn failed: {e}", spec.name);
            // Nothing ran; report a failed run so `after` dependents release.
            send_fork_frame(paths, session, &spec.name, None, Some(("failed", None)));
            return;
        }
    };

    let stdout = child.stdout.take();
    let deadline = Instant::now() + fork_timeout();
    let mut run_ref: Option<String> = None;
    let mut last_message: Option<String> = None;
    let mut failed = false;
    let mut completed = false;

    if let Some(out) = stdout {
        let reader = BufReader::new(out);
        for line in reader.lines() {
            if Instant::now() > deadline {
                let _ = child.kill();
                failed = true;
                break;
            }
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            match v["type"].as_str() {
                Some("thread.started") => {
                    if let Some(id) = v["thread_id"].as_str() {
                        run_ref = Some(id.to_string());
                        // Register the run ref BEFORE the run proceeds: the
                        // daemon refuses to schedule fork-run sessions from
                        // here on (defense in depth next to the env guard).
                        send_fork_frame(paths, session, &spec.name, Some(id), None);
                    }
                }
                Some("item.completed") => {
                    if v["item"]["type"].as_str() == Some("agent_message") {
                        if let Some(t) = v["item"]["text"].as_str() {
                            last_message = Some(t.to_string());
                        }
                    }
                }
                Some("turn.completed") => completed = true,
                Some("turn.failed") | Some("error") => failed = true,
                _ => {}
            }
        }
    }
    let status_ok = child.wait().map(|s| s.success()).unwrap_or(false);
    let status = if completed && status_ok && !failed {
        "completed"
    } else {
        "failed"
    };

    let mut report = last_message.unwrap_or_default().trim().to_string();
    let chain_next =
        status == "completed" && spec.chain && autofork_core::wake::wants_continue(&report);
    if chain_next {
        report = autofork_core::wake::strip_continue(&report);
    }
    if status == "completed" && !report.is_empty() {
        shared
            .lock()
            .unwrap()
            .reports
            .insert(spec.name.clone(), report.clone());
    }

    // Deliver the report into the parent session via codex's durable queue:
    // the parent's own process drains it when the session next goes idle.
    let body = if status == "completed" {
        if report.is_empty() {
            "(the fork finished without a report)".to_string()
        } else {
            report.clone()
        }
    } else {
        format!(
            "(the fork run failed{})",
            if report.is_empty() {
                String::new()
            } else {
                format!("; its last message:\n{report}")
            }
        )
    };
    let block = report_block(&spec.name, &spec.trigger, status, &body);
    if let Err(e) = queue_message(session, &block) {
        eprintln!(
            "[codex-waiter] fork '{}' report delivery failed: {e}",
            spec.name
        );
    }

    // The completion frame rides even if delivery failed: the daemon settles
    // the run and (for chains) re-arms the fork.
    send_fork_frame(
        paths,
        session,
        &spec.name,
        run_ref.as_deref(),
        Some((status, chain_next.then_some(true))),
    );

    // A completed run's thread has served its purpose; delete it so fork runs
    // never accumulate in codex's session store. Failed runs stay readable.
    if status == "completed" && std::env::var_os("AUTOFORK_KEEP_FORK_SESSIONS").is_none() {
        if let Some(id) = run_ref.as_deref() {
            // `--force`: without a terminal, delete refuses to confirm.
            let _ = Command::new(codex_bin())
                .arg("delete")
                .arg("--force")
                .arg(id)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// Send a ForkSpawned (completion=None) or ForkCompleted frame.
fn send_fork_frame(
    paths: &Paths,
    session: &str,
    fork: &str,
    run_ref: Option<&str>,
    completion: Option<(&str, Option<bool>)>,
) {
    let Ok(mut client) = Client::connect_or_spawn(paths, Duration::from_secs(5)) else {
        return;
    };
    let run_ref = run_ref.unwrap_or("unknown").to_string();
    let body = match completion {
        None => RequestBody::ForkSpawned {
            session_id: session.to_string(),
            fork: fork.to_string(),
            run_ref,
        },
        Some((status, cont)) => RequestBody::ForkCompleted {
            session_id: session.to_string(),
            fork: fork.to_string(),
            run_ref,
            status: status.to_string(),
            cont,
        },
    };
    let _ = client.request(body);
}

// ---------------------------------------------------------------------------
// app-server RPC (queue delivery, hook trust)
// ---------------------------------------------------------------------------

/// A tiny JSON-RPC-over-stdio client for a transient `codex app-server`.
struct AppServer {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl AppServer {
    fn start() -> Result<Self, String> {
        let mut child = Command::new(codex_bin())
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawning codex app-server: {e}"))?;
        let stdout = child.stdout.take().ok_or("no app-server stdout")?;
        let mut s = Self {
            child,
            reader: BufReader::new(stdout),
            next_id: 1,
        };
        s.request(
            "initialize",
            serde_json::json!({
                "clientInfo": {"name": "autofork", "title": "autofork", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"experimentalApi": true}
            }),
        )?;
        s.notify("initialized")?;
        Ok(s)
    }

    fn send(&mut self, v: &serde_json::Value) -> Result<(), String> {
        let stdin = self.child.stdin.as_mut().ok_or("no app-server stdin")?;
        let mut line = serde_json::to_string(v).map_err(|e| e.to_string())?;
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .and_then(|_| stdin.flush())
            .map_err(|e| format!("writing to app-server: {e}"))
    }

    fn notify(&mut self, method: &str) -> Result<(), String> {
        self.send(&serde_json::json!({"method": method}))
    }

    fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&serde_json::json!({"method": method, "id": id, "params": params}))?;
        // Read until our id answers (notifications interleave).
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut line = String::new();
        while Instant::now() < deadline {
            line.clear();
            match self.reader.read_line(&mut line) {
                Ok(0) => return Err("app-server closed".into()),
                Ok(_) => {}
                Err(e) => return Err(format!("reading app-server: {e}")),
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if v["id"].as_u64() == Some(id) {
                if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
                    return Err(format!("{method}: {err}"));
                }
                return Ok(v["result"].clone());
            }
        }
        Err(format!("{method}: timed out"))
    }
}

impl Drop for AppServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A v4 UUID from the OS RNG (no extra dependency).
fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    let mut f = std::fs::File::open("/dev/urandom").expect("urandom");
    f.read_exact(&mut b).expect("urandom read");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// Queue a message into a codex thread — codex's own durable queue, drained
/// by the owning process when the thread next goes idle.
fn queue_message(thread_id: &str, text: &str) -> Result<(), String> {
    let mut srv = AppServer::start()?;
    srv.request(
        "thread/queue/add",
        serde_json::json!({
            "threadId": thread_id,
            "clientUserMessageId": uuid_v4(),
            "input": [{"type": "text", "text": text, "textElements": []}],
        }),
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Install / uninstall / doctor
// ---------------------------------------------------------------------------

/// `$CODEX_HOME`, defaulting to `~/.codex`.
pub fn codex_home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(h));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex"))
}

fn hooks_json_path() -> Option<PathBuf> {
    codex_home().map(|h| h.join("hooks.json"))
}

/// The (codex event name, autofork hook kind) pairs we install.
const HOOK_EVENTS: [(&str, &str); 3] = [
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "prompt-submit"),
    ("SessionEnd", "session-end"),
];

fn hook_command(kind: &str) -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "autofork".to_string());
    format!("{exe} codex hook {kind}")
}

/// Is this handler one of ours (any autofork binary path, any hook kind)?
fn is_ours(handler: &serde_json::Value) -> bool {
    handler["command"]
        .as_str()
        .map(|c| c.contains(" codex hook "))
        .unwrap_or(false)
}

/// Merge our hooks into an existing hooks.json value (removing any previous
/// autofork entries first), preserving everything else.
fn merge_hooks(mut root: serde_json::Value) -> serde_json::Value {
    if !root.is_object() {
        root = serde_json::json!({});
    }
    if !root["hooks"].is_object() {
        root["hooks"] = serde_json::json!({});
    }
    let hooks = root["hooks"].as_object_mut().unwrap();
    for (event, kind) in HOOK_EVENTS {
        let arr = hooks
            .entry(event.to_string())
            .or_insert_with(|| serde_json::json!([]));
        if !arr.is_array() {
            *arr = serde_json::json!([]);
        }
        let groups = arr.as_array_mut().unwrap();
        groups.retain(|g| {
            !g["hooks"]
                .as_array()
                .map(|hs| hs.iter().all(is_ours) && !hs.is_empty())
                .unwrap_or(false)
        });
        groups.push(serde_json::json!({
            "hooks": [{"type": "command", "command": hook_command(kind)}]
        }));
    }
    root
}

/// Remove our hooks from a hooks.json value.
fn unmerge_hooks(mut root: serde_json::Value) -> serde_json::Value {
    if let Some(hooks) = root["hooks"].as_object_mut() {
        for (_, arr) in hooks.iter_mut() {
            if let Some(groups) = arr.as_array_mut() {
                groups.retain(|g| {
                    !g["hooks"]
                        .as_array()
                        .map(|hs| hs.iter().all(is_ours) && !hs.is_empty())
                        .unwrap_or(false)
                });
            }
        }
        hooks.retain(|_, v| v.as_array().map(|a| !a.is_empty()).unwrap_or(true));
    }
    root
}

/// `autofork codex install`: merge our hooks into `$CODEX_HOME/hooks.json`
/// and trust them (codex silently skips untrusted hooks).
pub fn install(print: bool) -> Result<(), String> {
    let path = hooks_json_path().ok_or("cannot determine codex home")?;
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s)
            .map_err(|e| format!("existing {} is not valid JSON: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    let merged = merge_hooks(existing);
    let rendered = serde_json::to_string_pretty(&merged).map_err(|e| e.to_string())?;
    if print {
        println!("{rendered}");
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    std::fs::write(&path, rendered).map_err(|e| format!("writing {}: {e}", path.display()))?;
    println!("installed autofork hooks into {}", path.display());
    match trust_our_hooks(&path) {
        Ok(n) => println!("trusted {n} autofork hook(s) with codex"),
        Err(e) => {
            return Err(format!(
                "hooks written but NOT trusted ({e}) — codex silently skips untrusted hooks; \
                 re-run `autofork codex install` or trust them in codex via /hooks"
            ))
        }
    }
    println!("restart codex sessions to pick the hooks up");
    Ok(())
}

/// `autofork codex uninstall`: remove our hooks from hooks.json.
pub fn uninstall() -> Result<(), String> {
    let path = hooks_json_path().ok_or("cannot determine codex home")?;
    let existing: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s)
            .map_err(|e| format!("existing {} is not valid JSON: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("not installed ({} absent)", path.display());
            return Ok(());
        }
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    let cleaned = unmerge_hooks(existing);
    let rendered = serde_json::to_string_pretty(&cleaned).map_err(|e| e.to_string())?;
    std::fs::write(&path, rendered).map_err(|e| format!("writing {}: {e}", path.display()))?;
    println!("removed autofork hooks from {}", path.display());
    println!("(stale hooks.state trust entries in config.toml are inert and left in place)");
    Ok(())
}

/// Trust every autofork hook found in the given hooks file via the same RPCs
/// the codex TUI's /hooks command uses. Returns the number trusted.
fn trust_our_hooks(hooks_path: &Path) -> Result<usize, String> {
    let mut srv = AppServer::start()?;
    let listed = srv.request("hooks/list", serde_json::json!({}))?;
    let mut state = serde_json::Map::new();
    let mut count = 0;
    for scope in listed["data"].as_array().into_iter().flatten() {
        for h in scope["hooks"].as_array().into_iter().flatten() {
            let source = h["sourcePath"].as_str().unwrap_or_default();
            if Path::new(source) != hooks_path || !is_ours(h) {
                continue;
            }
            let (Some(key), Some(hash)) = (h["key"].as_str(), h["currentHash"].as_str()) else {
                continue;
            };
            state.insert(
                key.to_string(),
                serde_json::json!({"enabled": true, "trusted_hash": hash}),
            );
            count += 1;
        }
    }
    if count == 0 {
        return Err("codex reported none of our hooks (is `codex` current?)".into());
    }
    srv.request(
        "config/batchWrite",
        serde_json::json!({
            "edits": [{
                "keyPath": "hooks.state",
                "mergeStrategy": "upsert",
                "value": serde_json::Value::Object(state),
            }]
        }),
    )?;
    Ok(count)
}

/// Doctor check lines for the codex integration. Empty when codex isn't in
/// use; "hooks installed" lines print as ok, everything else as WARN.
pub fn doctor_lines() -> Vec<String> {
    let mut lines = Vec::new();
    let codex_version = Command::new(codex_bin())
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let Some(path) = hooks_json_path() else {
        return lines;
    };
    let installed: Option<serde_json::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let ours: Vec<String> = installed
        .as_ref()
        .map(|root| {
            let mut cmds = Vec::new();
            if let Some(hooks) = root["hooks"].as_object() {
                for (_, arr) in hooks {
                    for g in arr.as_array().into_iter().flatten() {
                        for h in g["hooks"].as_array().into_iter().flatten() {
                            if is_ours(h) {
                                cmds.push(h["command"].as_str().unwrap_or_default().to_string());
                            }
                        }
                    }
                }
            }
            cmds
        })
        .unwrap_or_default();

    if ours.is_empty() {
        if codex_version.is_some() {
            lines.push(
                "codex detected but the autofork hooks are not installed — run `autofork codex install` to enable forks in codex sessions"
                    .into(),
            );
        }
        return lines;
    }
    if ours.len() < HOOK_EVENTS.len() {
        lines.push(format!(
            "codex hooks partially installed ({} of {}) — run `autofork codex install` to repair",
            ours.len(),
            HOOK_EVENTS.len()
        ));
        return lines;
    }
    // The hook commands embed the binary path they were installed from; a
    // moved binary means the hooks (and their trust hashes) point at nothing.
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from));
    if let Some(exe) = exe {
        if !ours.iter().all(|c| c.starts_with(&exe)) {
            lines.push(
                "codex hooks point at a different autofork binary — run `autofork codex install` to repoint and re-trust them"
                    .into(),
            );
            return lines;
        }
    }
    if codex_version.is_none() {
        lines.push("codex hooks are installed but `codex` was not found on PATH".into());
        return lines;
    }
    // Trust: codex silently skips untrusted hooks, so verify.
    match trust_status(&path) {
        Ok(true) => lines.push(format!("codex hooks installed ({})", path.display())),
        Ok(false) => lines.push(
            "codex hooks are installed but not trusted — run `autofork codex install` to re-trust them"
                .into(),
        ),
        Err(e) => lines.push(format!(
            "codex hooks installed but trust could not be verified ({e})"
        )),
    }
    lines
}

/// Are all our hooks in the given file trusted?
fn trust_status(hooks_path: &Path) -> Result<bool, String> {
    let mut srv = AppServer::start()?;
    let listed = srv.request("hooks/list", serde_json::json!({}))?;
    let mut seen = 0;
    for scope in listed["data"].as_array().into_iter().flatten() {
        for h in scope["hooks"].as_array().into_iter().flatten() {
            let source = h["sourcePath"].as_str().unwrap_or_default();
            if Path::new(source) != hooks_path || !is_ours(h) {
                continue;
            }
            seen += 1;
            if h["trustStatus"].as_str() != Some("trusted") {
                return Ok(false);
            }
        }
    }
    Ok(seen > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cx_input_parses_minimal_and_full() {
        let min: CxInput = serde_json::from_str(r#"{"session_id":"s"}"#).unwrap();
        assert_eq!(min.session_id, "s");
        assert!(min.transcript_path.is_none());
        let full: CxInput = serde_json::from_str(
            r#"{"session_id":"s","transcript_path":"/r.jsonl","cwd":"/p","source":"resume",
                "model":"gpt-5.6-sol","permission_mode":"default","prompt":"hi","reason":"other"}"#,
        )
        .unwrap();
        assert_eq!(full.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(full.source.as_deref(), Some("resume"));
        assert_eq!(full.permission_mode.as_deref(), Some("default"));
    }

    #[test]
    fn rollout_lines_drive_the_state() {
        let mut s = RolloutState::default();
        assert!(apply_rollout_line(
            br#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"t","model_context_window":258400}}"#,
            &mut s
        ));
        assert!(s.busy);
        assert_eq!(s.context_window, Some(258_400));
        assert!(apply_rollout_line(
            br#"{"type":"turn_context","payload":{"turn_id":"t","model":"gpt-5.6-sol"}}"#,
            &mut s
        ));
        assert_eq!(s.model.as_deref(), Some("gpt-5.6-sol"));
        assert!(apply_rollout_line(
            br#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100},"last_token_usage":{"input_tokens":11478,"output_tokens":5},"model_context_window":258400}}}"#,
            &mut s
        ));
        assert_eq!(s.context_tokens, Some(11_483));
        assert!(apply_rollout_line(
            br#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t"}}"#,
            &mut s
        ));
        assert!(!s.busy);
        // Unknown lines are inert.
        assert!(!apply_rollout_line(
            br#"{"type":"response_item","payload":{}}"#,
            &mut s
        ));
        assert!(!apply_rollout_line(b"not json", &mut s));
    }

    #[test]
    fn rollout_tail_handles_partial_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.jsonl");
        std::fs::write(&path, b"").unwrap();
        let mut tail = RolloutTail::new(path.clone());
        let mut state = RolloutState::default();
        assert!(!tail.poll(&mut state));
        // A partial line stays buffered until its newline arrives.
        let line =
            br#"{"type":"event_msg","payload":{"type":"task_started","model_context_window":100}}"#;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&line[..20])
            .unwrap();
        assert!(!tail.poll(&mut state));
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(&line[20..]).unwrap();
        f.write_all(b"\n").unwrap();
        assert!(tail.poll(&mut state));
        assert!(state.busy);
    }

    #[test]
    fn merge_is_idempotent_and_preserves_foreign_hooks() {
        let existing = serde_json::json!({
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "my-own-stop-hook"}]}],
                "SessionStart": [{"hooks": [{"type": "command", "command": "/old/autofork codex hook session-start"}]}],
            }
        });
        let merged = merge_hooks(existing);
        let again = merge_hooks(merged.clone());
        assert_eq!(merged, again, "merge must be idempotent");
        // Foreign hook preserved.
        assert_eq!(
            merged["hooks"]["Stop"][0]["hooks"][0]["command"],
            "my-own-stop-hook"
        );
        // The stale autofork entry was replaced, not duplicated.
        let starts = merged["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(starts.len(), 1);
        assert!(starts[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .ends_with(" codex hook session-start"));
        // All three of our events are present.
        for (event, _) in HOOK_EVENTS {
            assert!(merged["hooks"][event]
                .as_array()
                .is_some_and(|a| !a.is_empty()));
        }
        // Unmerge removes exactly ours.
        let cleaned = unmerge_hooks(merged);
        assert_eq!(
            cleaned["hooks"]["Stop"][0]["hooks"][0]["command"],
            "my-own-stop-hook"
        );
        assert!(cleaned["hooks"].get("SessionStart").is_none());
    }

    #[test]
    fn sandbox_args_map_permission_modes() {
        assert_eq!(
            sandbox_args(Some("bypassPermissions")),
            vec!["--dangerously-bypass-approvals-and-sandbox"]
        );
        assert_eq!(sandbox_args(Some("plan")), vec!["--sandbox", "read-only"]);
        assert_eq!(
            sandbox_args(Some("default")),
            vec!["--sandbox", "workspace-write"]
        );
        assert_eq!(sandbox_args(None), vec!["--sandbox", "workspace-write"]);
    }

    #[test]
    fn uuid_v4_shape() {
        let u = uuid_v4();
        assert_eq!(u.len(), 36);
        assert_eq!(u.as_bytes()[14], b'4');
    }

    #[test]
    fn report_block_carries_the_wake_marker() {
        let b = report_block("journal", "idle:600", "completed", "did things");
        assert!(b.contains(autofork_core::wake::WAKE_MARKER));
        assert!(b.contains("journal"));
    }
}
