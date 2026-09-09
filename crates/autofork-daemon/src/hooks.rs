//! Lifecycle-hook execution: the daemon runs small shell commands at session
//! lifecycle moments (see `autofork_core::hooks` for the definition format).
//! Hooks never involve a model — they exist so resource integrations
//! (workspace leases, seat locks) can follow a session's life directly:
//! acquire on `session_start`, renew on `activity`, park on `idle`, release
//! on `session_end`.
//!
//! Since v0.24 a hook may also **deliver its stdout into the session**
//! (`deliver: context` / `deliver: wake`) — a *feed*. The delivery lanes are
//! the ones fork reports already use: the report spool (drained as
//! `additionalContext` by the next prompt hook on Claude Code and codex, and
//! by the parked poll on opencode, which has no silent lane of its own), and
//! the wake queue (a turn the model reacts to). What a feed adds is a
//! producer that costs no tokens and no model call.
//!
//! Two guards make a feed writable as "just print the current view":
//! a per-(session, hook) **content hash**, so an unchanged block is never
//! delivered twice, and `throttle:`, so an external trigger firing in a burst
//! cannot run the command faster than its author intended.
//!
//! Context rides on environment variables:
//! - `AUTOFORK_HOOK_NAME` — the hook's own name
//! - `AUTOFORK_EVENT` — `session_start` / `resume` / `activity` / `idle` /
//!   `session_end`
//! - `AUTOFORK_SESSION_ID` — the parent session's id
//! - `AUTOFORK_PROJECT_ROOT`, `AUTOFORK_CWD` — where the session lives
//! - `AUTOFORK_CLIENT` — `claude-code` or `opencode`
//! - `AUTOFORK_SOURCE` — session_start only, when known
//!   (`startup`/`resume`/`clear`/`compact`)
//! - `AUTOFORK_IDLE_SECS` — idle only: the deadline that elapsed
//! - `AUTOFORK_END_REASON` — session_end only: the client-reported reason
//!   (`clear`/`logout`/`prompt_input_exit`/`other`/`disposed`/`deleted`), or
//!   the daemon's own `lost` (poll-loss grace close), `pruned`, `timeout`.
//!
//! No reason can cover SIGKILL, crashes, or power loss — integrations must
//! keep a lease TTL as the crash fallback.

use crate::daemon::Daemon;
use autofork_core::hooks::{discover_hooks, HookDeliver, HookEntry, HookOn};
use autofork_core::store::SessionRow;
use std::path::PathBuf;
use std::sync::Arc;

/// The session context a hook command receives. Built from the triggering
/// event when there is one, or from the stored session row on the daemon's
/// own close paths.
#[derive(Debug, Clone)]
pub struct HookCtx {
    pub session_id: String,
    pub cwd: PathBuf,
    pub project_root: PathBuf,
    pub client: Option<String>,
    /// The session's transcript path, when the client has one. Claude Code
    /// spools under the CONVERSATION id (this file's stem), not the session
    /// id, so a block survives into a resumed leg — a feed must land in the
    /// same place fork reports do or it would never be drained.
    pub transcript_path: Option<PathBuf>,
}

impl HookCtx {
    pub fn from_event(ev: &autofork_core::protocol::Event) -> Self {
        Self {
            session_id: ev.session_id.clone(),
            cwd: ev.cwd.clone(),
            project_root: ev.project_root.clone(),
            client: ev.client.clone(),
            transcript_path: ev.transcript_path.clone(),
        }
    }

    pub fn from_row(row: &SessionRow) -> Self {
        Self {
            session_id: row.session_id.clone(),
            cwd: row.cwd.clone(),
            project_root: row.project_root.clone(),
            client: row.client.clone(),
            transcript_path: row.transcript_path.clone(),
        }
    }

    /// Where a delivered block must be spooled so this session's client finds
    /// it. Claude Code drains the spool under the conversation id (the
    /// transcript's stem); opencode and codex use the session id itself.
    pub fn spool_key(&self) -> String {
        match self.client.as_deref() {
            Some("opencode") | Some("codex") => self.session_id.clone(),
            _ => self
                .transcript_path
                .as_deref()
                .and_then(|p| p.file_stem())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.session_id.clone()),
        }
    }
}

/// A lifecycle moment to fire hooks for. Idle hooks are not fired through
/// this (they need per-deadline latching — see the stop-wait loop), but
/// [`execute`] runs them with the same env plumbing.
#[derive(Debug, Clone, Copy)]
pub enum HookEvent<'a> {
    SessionStart { source: Option<&'a str> },
    Activity,
    SessionEnd { reason: &'a str },
}

/// Discover the hooks visible from the session and run every one matching
/// `event`. Fire-and-forget: each command runs on its own task.
pub fn fire_matching(daemon: &Arc<Daemon>, ctx: &HookCtx, event: HookEvent<'_>) {
    let (entries, _) = discover_hooks(&ctx.cwd, Some(&daemon.user_hooks_root()));
    for entry in entries {
        let matched: Option<(&str, Vec<(String, String)>)> = match event {
            HookEvent::SessionStart { source } => {
                let start = entry.parsed.def.on.contains(&HookOn::SessionStart);
                let resume =
                    source == Some("resume") && entry.parsed.def.on.contains(&HookOn::Resume);
                if start || resume {
                    let mut env = Vec::new();
                    if let Some(s) = source {
                        env.push(("AUTOFORK_SOURCE".to_string(), s.to_string()));
                    }
                    // A hook on both `session_start` and `resume` fires once,
                    // under the more general name.
                    Some((if start { "session_start" } else { "resume" }, env))
                } else {
                    None
                }
            }
            HookEvent::Activity => entry
                .parsed
                .def
                .on
                .contains(&HookOn::Activity)
                .then(|| ("activity", Vec::new())),
            HookEvent::SessionEnd { reason } => {
                entry.parsed.def.on.contains(&HookOn::SessionEnd).then(|| {
                    (
                        "session_end",
                        vec![("AUTOFORK_END_REASON".to_string(), reason.to_string())],
                    )
                })
            }
        };
        if let Some((event_name, extra_env)) = matched {
            execute(daemon, ctx, &entry, event_name, extra_env);
        }
    }
}

/// The idle deadlines (seconds) the session's hooks want, resolved against
/// the configured default (a bare `idle` with a zero default is disabled,
/// matching fork semantics). Deduplicated per hook.
pub fn idle_hook_deadlines(entries: &[HookEntry], default_secs: u64) -> Vec<(HookEntry, u64)> {
    let mut out: Vec<(HookEntry, u64)> = Vec::new();
    for entry in entries {
        if entry.parsed.def.command.is_empty() {
            continue;
        }
        let mut secs: Vec<u64> = entry
            .parsed
            .def
            .on
            .iter()
            .filter_map(|on| match on {
                HookOn::Idle { after_secs } => {
                    let d = after_secs.unwrap_or(default_secs);
                    (after_secs.is_some() || default_secs > 0).then_some(d)
                }
                _ => None,
            })
            .collect();
        secs.sort_unstable();
        secs.dedup();
        for d in secs {
            out.push((entry.clone(), d));
        }
    }
    out
}

/// Run one hook command: `sh -c <command>` (the platform shell — see
/// `autofork_core::sys::shell`) in the session's cwd, context in
/// `AUTOFORK_*` env vars, killed after the hook's timeout. A hook with no
/// `deliver:` is fire-and-forget — output goes to the daemon log, a failure
/// is logged and otherwise inert, and hooks can never break scheduling.
///
/// With `deliver: context` or `deliver: wake`, successful stdout becomes a
/// framed block on its way into the session (see [`deliver_block`]). The
/// hook's `throttle:` is checked (and stamped) here, before the command runs
/// — external moments can fire far faster than a session's lifecycle ever
/// does, and the throttle is the only thing standing between a busy directory
/// and a command run per write.
pub fn execute(
    daemon: &Arc<Daemon>,
    ctx: &HookCtx,
    entry: &HookEntry,
    event_name: &str,
    extra_env: Vec<(String, String)>,
) {
    if entry.parsed.def.command.is_empty() {
        return;
    }
    let t = crate::daemon::now();
    if let Some(throttle) = entry.parsed.def.throttle_secs {
        let last = {
            let store = daemon.store.lock().unwrap();
            store
                .hook_last_run(&ctx.session_id, &entry.name)
                .ok()
                .flatten()
        };
        if let Some(last) = last {
            if (t - last).max(0) < throttle as i64 {
                tracing::debug!(hook = %entry.name, event = %event_name,
                    session = %ctx.session_id, "hook throttled, skipping");
                return;
            }
        }
    }
    {
        let store = daemon.store.lock().unwrap();
        let _ = store.stamp_hook_run(&ctx.session_id, &entry.name, t);
    }
    daemon.touch_busy();
    let command = entry.parsed.def.command.clone();
    let timeout = std::time::Duration::from_secs(entry.parsed.def.timeout_secs);
    let hook = entry.name.clone();
    let deliver = entry.parsed.def.deliver;
    let max_bytes = entry.parsed.def.max_bytes;
    let event_name = event_name.to_string();
    // The session's launch directory may be gone (a temp dir); fall back to
    // the project root, then to the filesystem root.
    let cwd = if ctx.cwd.is_dir() {
        ctx.cwd.clone()
    } else if ctx.project_root.is_dir() {
        ctx.project_root.clone()
    } else {
        autofork_core::sys::root_fallback_dir()
    };
    let mut env: Vec<(String, String)> = vec![
        ("AUTOFORK_HOOK_NAME".into(), hook.clone()),
        ("AUTOFORK_EVENT".into(), event_name.clone()),
        ("AUTOFORK_SESSION_ID".into(), ctx.session_id.clone()),
        (
            "AUTOFORK_PROJECT_ROOT".into(),
            ctx.project_root.to_string_lossy().into_owned(),
        ),
        (
            "AUTOFORK_CWD".into(),
            ctx.cwd.to_string_lossy().into_owned(),
        ),
        (
            "AUTOFORK_CLIENT".into(),
            ctx.client.clone().unwrap_or_else(|| "claude-code".into()),
        ),
        ("AUTOFORK_DELIVER".into(), deliver.label().into()),
    ];
    env.extend(extra_env);
    let session = ctx.session_id.clone();
    let ctx = ctx.clone();
    let daemon = Arc::clone(daemon);
    // A context feed is counted as in flight from here until its block is
    // spooled (or it fails), so a drain that fires the hook and then asks for
    // its output — the opencode `chat.message` lane — can wait for it instead
    // of racing it. Held by a guard so every exit path, panic included,
    // releases the count.
    let inflight =
        (deliver == HookDeliver::Context).then(|| FeedInflight::new(&daemon, &ctx.spool_key()));
    tracing::info!(hook = %hook, event = %event_name, session = %session, "running lifecycle hook");
    tokio::spawn(async move {
        let _inflight = inflight;
        let (shell, shell_args) = autofork_core::sys::shell();
        let mut cmd = tokio::process::Command::new(shell);
        cmd.args(shell_args)
            .arg(&command)
            .current_dir(&cwd)
            .envs(env)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(Ok(out)) if out.status.success() => {
                tracing::debug!(hook = %hook, event = %event_name, session = %session,
                    "lifecycle hook finished");
                if deliver.delivers() {
                    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    deliver_block(&daemon, &ctx, &hook, &event_name, deliver, max_bytes, text);
                }
            }
            Ok(Ok(out)) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                tracing::warn!(hook = %hook, event = %event_name, session = %session,
                    code = ?out.status.code(), stderr = %stderr.trim(),
                    "lifecycle hook failed");
            }
            Ok(Err(e)) => {
                tracing::warn!(hook = %hook, event = %event_name, session = %session,
                    error = %e, "lifecycle hook could not run");
            }
            Err(_) => {
                tracing::warn!(hook = %hook, event = %event_name, session = %session,
                    timeout_secs = timeout.as_secs(), "lifecycle hook timed out, killed");
            }
        }
    });
}

/// Counts one running `deliver: context` hook against its spool key for as
/// long as it lives. See [`Daemon::feed_hooks_inflight`].
struct FeedInflight {
    daemon: Arc<Daemon>,
    key: String,
}

impl FeedInflight {
    fn new(daemon: &Arc<Daemon>, key: &str) -> Self {
        daemon.feed_hook_started(key);
        Self {
            daemon: Arc::clone(daemon),
            key: key.to_string(),
        }
    }
}

impl Drop for FeedInflight {
    fn drop(&mut self) {
        self.daemon.feed_hook_finished(&self.key);
    }
}

/// Put a feed's stdout into the session, or decide not to.
///
/// Three things happen here, in this order, and each is a deliberate "do
/// nothing" case:
///
/// - **Empty output means nothing to say.** A feed command that has no news
///   prints nothing and costs the session not one token; this is the normal
///   quiet path, not a failure.
/// - **Oversize output is truncated, not dropped.** `max_bytes` exists so one
///   chatty feed cannot crowd out the fork reports sharing the same lane.
/// - **Unchanged output is not delivered again — for `deliver: context`.**
///   The hash comparison is what lets a quiet feed be written the simple way:
///   print the whole current view every time, and it reaches the model only
///   when something actually changed. `deliver: wake` is deliberately exempt:
///   its author asked to interrupt the session, and the same words arriving
///   twice ("the deploy failed") is news the second time too. What bounds a
///   wake feed is its trigger and its `throttle:`, not its wording.
fn deliver_block(
    daemon: &Arc<Daemon>,
    ctx: &HookCtx,
    hook: &str,
    event: &str,
    deliver: HookDeliver,
    max_bytes: usize,
    mut text: String,
) {
    if text.is_empty() {
        tracing::debug!(hook = %hook, session = %ctx.session_id,
            "feed produced no output, nothing delivered");
        return;
    }
    if text.len() > max_bytes {
        let mut cut = max_bytes;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n[…feed truncated at max_bytes]");
    }
    if deliver == HookDeliver::Context {
        let hash = content_hash(&text);
        let store = daemon.store.lock().unwrap();
        match store.feed_hash_changed(&ctx.session_id, hook, &hash) {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(hook = %hook, session = %ctx.session_id,
                    "feed output unchanged since the last delivery, skipping");
                return;
            }
            Err(e) => {
                tracing::warn!(hook = %hook, error = %e, "feed dedupe check failed");
            }
        }
    }
    let block = autofork_core::wake::feed_block(hook, event, &text);
    let t = crate::daemon::now();
    match deliver {
        HookDeliver::None => {}
        HookDeliver::Context => {
            let key = ctx.spool_key();
            let store = daemon.store.lock().unwrap();
            if let Err(e) = store.spool_report(&key, hook, &block, t) {
                tracing::warn!(hook = %hook, error = %e, "could not spool feed block");
                return;
            }
            drop(store);
            tracing::info!(hook = %hook, session = %ctx.session_id, event = %event,
                bytes = block.len(), "feed delivered (quiet)");
            // opencode drains this spool at its next turn (the plugin's
            // `chat.message` hook, the additionalContext equivalent). Nudge
            // the parked poll as well: while the session sits idle there is
            // no next turn in sight, and the plugin takes blocks off the poll
            // and injects them as no-reply messages. Whichever lane arrives
            // first clears the spool, so a block is delivered exactly once.
            if ctx.client.as_deref() == Some("opencode") {
                daemon.nudge(&ctx.session_id);
            }
        }
        HookDeliver::Wake => {
            {
                let store = daemon.store.lock().unwrap();
                if let Err(e) = store.spool_wake_block(&ctx.session_id, hook, &block, t) {
                    tracing::warn!(hook = %hook, error = %e, "could not queue feed wake block");
                    return;
                }
            }
            tracing::info!(hook = %hook, session = %ctx.session_id, event = %event,
                bytes = block.len(), "feed queued for wake delivery");
            daemon.nudge(&ctx.session_id);
        }
    }
}

/// FNV-1a (64-bit) over the block text — the dedupe key. A hash, not a
/// comparison against the stored text: the store keeps one short column
/// instead of a copy of every feed's last output, and a 64-bit collision
/// would cost exactly one skipped delivery.
fn content_hash(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{h:016x}")
}

/// Fire every hook of `ctx`'s session that listens for an external trigger —
/// a watched path that changed, or a named event someone emitted. Unlike the
/// lifecycle events, these can arrive while the session is mid-turn: nothing
/// here waits for a pause, because the whole point of an external trigger is
/// that the outside world does not keep to the session's rhythm.
pub fn fire_external(
    daemon: &Arc<Daemon>,
    ctx: &HookCtx,
    kind: autofork_core::moments::ExternalKind,
    key: &str,
    detail: &str,
) {
    use autofork_core::moments::ExternalKind;
    let (entries, _) = discover_hooks(&ctx.cwd, Some(&daemon.user_hooks_root()));
    for entry in entries {
        let matched = entry.parsed.def.on.iter().any(|on| match (kind, on) {
            (ExternalKind::Changed, HookOn::Changed { pattern }) => pattern == key,
            (ExternalKind::Event, HookOn::Event { name }) => name == key,
            _ => false,
        });
        if !matched {
            continue;
        }
        let mut env = vec![(
            "AUTOFORK_TRIGGER".to_string(),
            format!("{}:{key}", kind.label()),
        )];
        match kind {
            ExternalKind::Changed => {
                env.push(("AUTOFORK_WATCH".into(), key.to_string()));
                env.push(("AUTOFORK_CHANGED_PATHS".into(), detail.to_string()));
            }
            ExternalKind::Event => {
                env.push(("AUTOFORK_EMIT_NAME".into(), key.to_string()));
                env.push(("AUTOFORK_EMIT_PAYLOAD".into(), detail.to_string()));
            }
        }
        execute(daemon, ctx, &entry, kind.label(), env);
    }
}
