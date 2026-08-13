//! `autofork hook <event>`: the Claude Code hook entrypoint. Reads the hook
//! JSON from stdin and forwards it to the daemon.
//!
//! The Stop hook (`stop-wait`) is an asyncRewake command: it long-polls the
//! daemon and, when forks come due, prints the wake payload to stderr and
//! exits 2 so Claude Code wakes the idle session. Every other path — and every
//! failure — exits 0 so a hook never breaks or wedges the session.

use crate::client::{spawn_daemon_detached, Client};
use autofork_core::config::Paths;
use autofork_core::project::project_root;
use autofork_core::protocol::{Event, EventKind, RequestBody, ResponseBody};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum HookKind {
    SessionStart,
    UserPromptSubmit,
    /// The asyncRewake Stop hook (`stop-wait`): long-poll for due forks.
    StopWait,
    SessionEnd,
}

/// The subset of Claude Code hook stdin we consume. Unknown fields ignored.
#[derive(Debug, Deserialize)]
struct HookInput {
    session_id: String,
    #[serde(default)]
    transcript_path: Option<PathBuf>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    source: Option<String>,
    /// The SessionEnd reason (`clear`/`logout`/`prompt_input_exit`/`other`),
    /// forwarded so `session_end` lifecycle hooks can see it.
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    model: Option<String>,
    /// The submitted prompt (UserPromptSubmit). Used to tell genuine user
    /// activity from a non-waking continuation. Absent for other events.
    #[serde(default)]
    prompt: Option<String>,
}

pub fn run_hook(kind: HookKind) {
    // Never break the session, whatever happens in here. (The stop-wait Wake
    // path exits 2 inline; every other path returns and main exits 0.)
    let _ = run_hook_inner(kind);
}

fn run_hook_inner(kind: HookKind) -> Option<()> {
    // Recursion guard, kept as zero-cost defense in depth: fork subagents emit
    // SubagentStop, not Stop, so they never reach the trigger path — but if a
    // fork's environment ever carried these vars, do nothing.
    if std::env::var_os("AUTOFORK_FORK").is_some()
        || std::env::var_os("AUTOFORK_SESSION_ID").is_some()
    {
        return Some(());
    }
    let mut raw = String::new();
    use std::io::Read;
    std::io::stdin().read_to_string(&mut raw).ok()?;
    let input: HookInput = serde_json::from_str(&raw).ok()?;
    let paths = Paths::from_env()?;

    let cwd = input.cwd.clone().or_else(|| std::env::current_dir().ok())?;
    let root = project_root(&cwd);

    // Per-session tag filter, inherited from the Claude Code process env.
    let enable_tags = tags_from_env("AUTOFORK_ENABLE_TAGS");
    let disable_tags = tags_from_env("AUTOFORK_DISABLE_TAGS");

    let event = |ev: EventKind| Event {
        event: ev,
        session_id: input.session_id.clone(),
        transcript_path: input.transcript_path.clone(),
        cwd: cwd.clone(),
        project_root: root.clone(),
        source: input.source.clone(),
        reason: input.reason.clone(),
        model: input.model.clone(),
        enable_tags: enable_tags.clone(),
        disable_tags: disable_tags.clone(),
        waking: None,
        notif_tool_use_id: None,
        notif_task_id: None,
        notif_status: None,
        notif_continue: None,
        context_tokens: None,
        context_window: None,
        client: None,
        busy: None,
    };

    match kind {
        HookKind::SessionStart => {
            // SessionStart has slack: spawn-and-wait, retire outdated daemons.
            let client = Client::connect_or_spawn(&paths, Duration::from_secs(5)).ok()?;
            let mut client = client.ensure_current_version(&paths).ok()?;
            let _ = client.request(RequestBody::Event(event(EventKind::SessionStart)));
        }
        HookKind::UserPromptSubmit => {
            // Hard budget; never wait on a daemon spawn here. This cancels any
            // parked stop-wait so no fork fires mid-turn.
            let Ok(mut client) = Client::connect(&paths, Duration::from_millis(1500)) else {
                spawn_daemon_detached(&paths);
                return Some(());
            };
            // Sniff the prompt. An asyncRewake wake reminder (our marker) is
            // always a non-waking continuation. A task notification gets its
            // envelope ids forwarded so the daemon can decide whether it is
            // one of its own fork completions (non-waking) or some other
            // background task finishing (genuine activity — a new pause); the
            // coarse `waking: false` stays as the old-daemon fallback. `None`
            // (no prompt text) lets the daemon decide via its post-wake grace
            // window.
            let mut ev = event(EventKind::PromptSubmit);
            if let Some(p) = input.prompt.as_deref() {
                if p.contains(autofork_core::wake::WAKE_MARKER) {
                    ev.waking = Some(false);
                } else if let Some(n) = autofork_core::notification::parse_task_notification(p) {
                    ev.waking = Some(false);
                    ev.notif_tool_use_id = n.tool_use_id;
                    ev.notif_task_id = n.task_id;
                    ev.notif_status = n.status;
                    ev.notif_continue = Some(n.continue_requested);
                } else {
                    ev.waking = Some(true);
                }
            }
            let _ = client.request(RequestBody::Event(ev));
        }
        HookKind::StopWait => {
            // Runs async (Claude Code doesn't block): fine to spawn + retire.
            let client = Client::connect_or_spawn(&paths, Duration::from_secs(10)).ok()?;
            let mut client = client.ensure_current_version(&paths).ok()?;
            // Long-poll until forks are due or the wait is cancelled/retired.
            // Waited / error / closed socket / proto skew: exit 0 silently.
            if let Ok(ResponseBody::Wake { payload, .. }) = client.stop_wait(event(EventKind::Stop))
            {
                // Wake the idle session: stderr shown as a system reminder.
                eprintln!("{payload}");
                std::process::exit(2);
            }
        }
        HookKind::SessionEnd => {
            let mut client = Client::connect_or_spawn(&paths, Duration::from_secs(5)).ok()?;
            let _ = client.request(RequestBody::Event(event(EventKind::SessionEnd)));
        }
    }
    Some(())
}

/// Read a comma-separated tag env var into a normalized list (trimmed,
/// empties dropped, deduped). An unset or all-empty value yields `None` so the
/// daemon falls back to the config default.
pub(crate) fn tags_from_env(var: &str) -> Option<Vec<String>> {
    let raw = std::env::var(var).ok()?;
    let mut out: Vec<String> = Vec::new();
    for piece in raw.split(',') {
        let t = piece.trim();
        if !t.is_empty() && !out.iter().any(|e| e == t) {
            out.push(t.to_string());
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}
