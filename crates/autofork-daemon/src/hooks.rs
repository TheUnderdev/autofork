//! Lifecycle-hook execution: the daemon runs small shell commands at session
//! lifecycle moments (see `autofork_core::hooks` for the definition format).
//! Hooks never involve a model — they exist so resource integrations
//! (workspace leases, seat locks) can follow a session's life directly:
//! acquire on `session_start`, renew on `activity`, park on `idle`, release
//! on `session_end`.
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
use autofork_core::hooks::{discover_hooks, HookEntry, HookOn};
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
}

impl HookCtx {
    pub fn from_event(ev: &autofork_core::protocol::Event) -> Self {
        Self {
            session_id: ev.session_id.clone(),
            cwd: ev.cwd.clone(),
            project_root: ev.project_root.clone(),
            client: ev.client.clone(),
        }
    }

    pub fn from_row(row: &SessionRow) -> Self {
        Self {
            session_id: row.session_id.clone(),
            cwd: row.cwd.clone(),
            project_root: row.project_root.clone(),
            client: row.client.clone(),
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

/// Run one hook command: `sh -c <command>` in the session's cwd, context in
/// `AUTOFORK_*` env vars, killed after the hook's timeout. Output goes to the
/// daemon log; a failure is logged and otherwise inert — hooks can never
/// break scheduling.
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
    daemon.touch_busy();
    let command = entry.parsed.def.command.clone();
    let timeout = std::time::Duration::from_secs(entry.parsed.def.timeout_secs);
    let hook = entry.name.clone();
    let event_name = event_name.to_string();
    // The session's launch directory may be gone (a temp dir); fall back to
    // the project root, then to the filesystem root.
    let cwd = if ctx.cwd.is_dir() {
        ctx.cwd.clone()
    } else if ctx.project_root.is_dir() {
        ctx.project_root.clone()
    } else {
        PathBuf::from("/")
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
    ];
    env.extend(extra_env);
    let session = ctx.session_id.clone();
    tracing::info!(hook = %hook, event = %event_name, session = %session, "running lifecycle hook");
    tokio::spawn(async move {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c")
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
