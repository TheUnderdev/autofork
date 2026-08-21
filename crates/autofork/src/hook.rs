//! `autofork hook <event>`: the Claude Code hook entrypoint. Reads the hook
//! JSON from stdin and forwards it to the daemon.
//!
//! The Stop hook (`stop-wait`) is an asyncRewake command: it long-polls the
//! daemon and, when forks come due, prints the wake payload to stderr and
//! exits 2 so Claude Code wakes the idle session. In headless mode the same
//! exit-2 wake carries a continuing chain fork's report (the goal fast path —
//! see `runner`). Every other path — and every failure — exits 0 so a hook
//! never breaks or wedges the session.

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

    // The client process behind this hook — the session's liveness anchor,
    // resolved once and reused (a parked poll outlives its parent, so it must
    // remember who that parent WAS).
    let harness = autofork_core::harness::client_process();

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
        harness: harness.clone(),
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
            // Headless-runner reports spooled since the last prompt are
            // delivered here, silently, as additionalContext (invisible in
            // the transcript). Spooled under the CONVERSATION id (transcript
            // stem) so a report finished after a session closed still reaches
            // its resumed leg. Old daemons answer Error — treated as none.
            let spool_key = input
                .transcript_path
                .as_deref()
                .and_then(|p| p.file_stem())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| input.session_id.clone());
            if let Ok(ResponseBody::Reports { blocks }) = client.request(RequestBody::TakeReports {
                session_id: spool_key,
            }) {
                if !blocks.is_empty() {
                    print_additional_context(&blocks);
                }
            }
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
            // This process outlives the turn: it parks a long poll (up to 4h)
            // and, in headless mode, keeps re-parking. If Claude Code dies
            // meanwhile — the exit path where no SessionEnd hook ever
            // completes — nothing else would end it, and a poll that keeps
            // re-parking would hold the session "alive" in the daemon
            // forever. Watch the client and leave when it does.
            crate::runner::watch_harness(harness.clone());
            // Runs async (Claude Code doesn't block): fine to spawn + retire.
            let client = Client::connect_or_spawn(&paths, Duration::from_secs(10)).ok()?;
            let mut client = client.ensure_current_version(&paths).ok()?;
            let headless = {
                let (cfg, _warnings) =
                    autofork_core::config::load_config_at(Some(&root), &paths.user_config());
                cfg.fork_runner == autofork_core::config::ForkRunner::Headless
            };
            if headless {
                // The quiet mode: this parked hook process consumes wakes
                // itself — no wake turn, no visible spawns. It runs each fork
                // as a `claude -p --fork-session` subprocess, spools the
                // report for silent delivery at the next prompt, and re-parks
                // until the wait is cancelled by activity. The one exception
                // is a chain run that asks to continue: that report IS the
                // parent's next instruction, so it wakes the session (exit 2)
                // like subagent mode does.
                // Fork children run the PARENT Claude Code process's exact
                // binary (captured now — the poll outlives reparenting).
                crate::runner::set_harness_bin(crate::client::parent_exe());
                let resume_target = input
                    .transcript_path
                    .as_deref()
                    .and_then(|p| p.file_stem())
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| input.session_id.clone());
                let mut reports = std::collections::HashMap::new();
                while let Ok(ResponseBody::Wake { forks, .. }) =
                    client.stop_wait(event(EventKind::Stop))
                {
                    let wake_blocks = crate::runner::execute_wake(
                        &paths,
                        &input.session_id,
                        &resume_target,
                        &cwd,
                        forks.unwrap_or_default(),
                        &mut reports,
                    );
                    if !wake_blocks.is_empty() {
                        // The goal fast path: a chain run asked to continue,
                        // so its report is work for the parent — deliver it by
                        // waking the session (stderr + exit 2) instead of
                        // re-parking and letting it rot in the spool until the
                        // user's next prompt. The daemon has already re-armed
                        // the fork; it fires again at the Stop that ends the
                        // turn this wake starts, which is the loop.
                        eprintln!(
                            "{}",
                            autofork_core::wake::build_chain_wake_payload(&wake_blocks)
                        );
                        std::process::exit(2);
                    }
                    // Re-park on a fresh connection (the wake resolved this
                    // one's poll); a brief pause guards against a misbehaving
                    // daemon spinning us. A client that died while the run was
                    // in flight gets no fresh poll: the run's work and its
                    // spooled report survive, the session does not.
                    std::thread::sleep(Duration::from_secs(1));
                    if harness.as_ref().is_some_and(|h| !h.alive()) {
                        return Some(());
                    }
                    let c = Client::connect_or_spawn(&paths, Duration::from_secs(10)).ok()?;
                    client = c.ensure_current_version(&paths).ok()?;
                }
            } else {
                // Long-poll until forks are due or the wait is cancelled/
                // retired. Waited / error / closed socket / proto skew: exit
                // 0 silently.
                if let Ok(ResponseBody::Wake { payload, .. }) =
                    client.stop_wait(event(EventKind::Stop))
                {
                    // Wake the idle session: stderr shown as a system reminder.
                    eprintln!("{payload}");
                    std::process::exit(2);
                }
            }
        }
        HookKind::SessionEnd => {
            let mut client = Client::connect_or_spawn(&paths, Duration::from_secs(5)).ok()?;
            // `flush_on_close`: hand every idle fork that hadn't yet fired
            // this pause to a detached end-runner BEFORE the close purges the
            // roster.
            let flush = {
                let (cfg, _w) =
                    autofork_core::config::load_config_at(Some(&root), &paths.user_config());
                cfg.flush_on_close
            };
            if flush {
                if let Ok(ResponseBody::Due { forks }) =
                    client.request(RequestBody::TakeFinalRuns {
                        session_id: input.session_id.clone(),
                    })
                {
                    let resume_target = input
                        .transcript_path
                        .as_deref()
                        .and_then(|p| p.file_stem())
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| input.session_id.clone());
                    crate::runner::spawn_final_runner(
                        &paths,
                        "claude-code",
                        &input.session_id,
                        &resume_target,
                        &cwd,
                        None,
                        None,
                        crate::client::parent_exe().as_deref(),
                        &forks,
                    );
                }
            }
            let _ = client.request(RequestBody::Event(event(EventKind::SessionEnd)));
        }
    }
    Some(())
}

/// Print spooled fork reports as UserPromptSubmit additionalContext (exit-0
/// JSON on stdout). Claude Code caps additionalContext at 10k characters;
/// truncate to fit rather than lose the delivery entirely.
fn print_additional_context(blocks: &[String]) {
    const CAP: usize = 9_800;
    let mut text = blocks.join("\n\n");
    if text.len() > CAP {
        let mut cut = CAP;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n[…report truncated to fit the context budget]");
    }
    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": text,
        }
    });
    println!("{out}");
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
