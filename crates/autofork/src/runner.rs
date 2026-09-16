//! The Claude Code headless fork runner (`fork_runner = "headless"`).
//!
//! In subagent mode (the default) a wake exits the Stop hook with code 2 and
//! the session's own model spawns fork subagents — cache-hot, but the wake
//! turn, the spawn calls and the completion relays are all visible in the
//! conversation. Headless mode is the opencode-style quiet alternative: the
//! parked asyncRewake Stop hook consumes wakes itself, runs each fork as a
//! `claude -p --resume <conversation> --fork-session` subprocess (full
//! history inherited, parent session untouched), and spools the report with
//! the daemon; the UserPromptSubmit hook delivers spooled reports silently as
//! `additionalContext` on the next prompt. Nothing surfaces in the
//! conversation itself.
//!
//! One report cannot wait for your next prompt: a `chain: true` run that asks
//! to continue. There the parent is the worker and the loop only advances once
//! it has seen the report, so that block is handed back to the parked hook,
//! which wakes the session with it (exit 2) instead of re-parking — the goal
//! fast path, the async twin of codex's synchronous block-and-inject.
//!
//! Trade-off, stated where it matters: a `-p` fork of an *interactive*
//! session cannot reuse its prompt cache (mode-stamped request prefixes), so
//! each run pays a cold read of the inherited history. That is the price of
//! silence — and the reason headless pairs with cheap fork models
//! (`[fork_models]` / a fork's `model:`), where the cold input is noise.

use crate::client::Client;
use autofork_core::config::Paths;
use autofork_core::protocol::{RequestBody, ResponseBody, WakeFork};
use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Wall-clock cap on one `claude -p` fork run, overridable via
/// `AUTOFORK_CLAUDE_FORK_TIMEOUT_SECS`.
const FORK_TIMEOUT_SECS: u64 = 1800;

/// The harness binary this process's forks must run — the PARENT process's
/// own executable, captured at the entrypoint (hook ppid / --bin arg). Fork
/// children must run the SAME program the user's session runs: PATH lookup
/// resolves a different install on multi-install machines (wrapper functions,
/// ~/.aisuite-style standalone checkouts, several versions side by side).
static HARNESS_BIN: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();

pub fn set_harness_bin(bin: Option<std::path::PathBuf>) {
    let _ = HARNESS_BIN.set(bin);
}

fn harness_bin() -> Option<String> {
    HARNESS_BIN
        .get()
        .and_then(|b| b.as_ref())
        .map(|p| p.to_string_lossy().into_owned())
}

fn claude_bin() -> String {
    std::env::var("AUTOFORK_CLAUDE_BIN")
        .ok()
        .or_else(harness_bin)
        .unwrap_or_else(|| "claude".to_string())
}

fn fork_timeout() -> Duration {
    let secs = std::env::var("AUTOFORK_CLAUDE_FORK_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(FORK_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// What one finished fork run leaves behind for its caller.
#[derive(Default)]
pub struct RunResult {
    /// The (sentinel-stripped) report, when the run completed with one —
    /// carried to `after` dependents and to the next chain iteration.
    pub report: Option<String>,
    /// The report block to wake the parent session with: set when a chain run
    /// asked to continue and the caller can deliver it (the parked Stop hook,
    /// which exits 2 with it). `None` means the report was spooled for silent
    /// delivery instead.
    pub wake_block: Option<String>,
    /// The run finished after its pause ended (the daemon's `RunState` said
    /// so): the user spoke, or a background task's completion started a new
    /// pause, while the run was in flight. The caller must not re-park on
    /// the old pause's behalf — the new pause has (or will have) its own
    /// parked Stop hook.
    pub stale: bool,
}

/// What one wake's runs leave behind for the parked Stop hook.
#[derive(Default)]
pub struct WakeOutcome {
    /// Continuing chain reports to wake the parent with (stderr + exit 2).
    pub wake_blocks: Vec<String>,
    /// At least one run finished after the pause it was selected in ended.
    /// The hook exits instead of re-parking: a Stop poll parked now would
    /// be a poll on a turn that is not idle, and could even fire an
    /// `idle: 0s` fork mid-turn (the epoch bump re-armed every latch).
    pub stale: bool,
}

/// Whether this process is currently executing fork runs. The harness
/// watchdog leaves a *parked* orphan alone only when it is idle: a run
/// already in flight is allowed to finish (its work and its spooled report
/// outlive the client that started it — the deliberate "fork children survive
/// a closed terminal" behavior), and the process exits once it is done.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Watch the client process behind a parked hook and exit when it dies.
///
/// The parked stop-wait hook is the one autofork process that outlives its
/// turn, so it is also the one that can be orphaned: Claude Code can exit
/// without its SessionEnd hook completing, and in headless mode this process
/// would then keep re-parking polls, which the daemon reads as a live
/// session — the reported "autofork didn't notice I closed the session".
/// Exiting closes the socket, which is the daemon's poll-loss signal.
pub fn watch_harness(harness: Option<autofork_core::harness::Harness>) {
    let Some(harness) = harness else { return };
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(5));
        if !harness.alive() && !RUNNING.load(Ordering::SeqCst) {
            std::process::exit(0);
        }
    });
}

/// Consume one wake's forks headlessly. Returns the report blocks of any
/// chain runs that asked to continue: the caller (the parked Stop hook) wakes
/// the parent session with them instead of re-parking, so a goal loop
/// advances on its own. Empty = nothing to wake for, re-park.
/// `resume_target` is the conversation id (transcript stem — the identity
/// that survives session resume; a resumed leg's own id is not resumable).
/// `reports` accumulates the last report per fork across the runner process's
/// life, for `after` piping and chain iterations.
pub fn execute_wake(
    paths: &Paths,
    session_id: &str,
    resume_target: &str,
    cwd: &std::path::Path,
    forks: Vec<WakeFork>,
    reports: &mut HashMap<String, String>,
) -> WakeOutcome {
    // Batch-parallel like the opencode plugin: each fork run is independent
    // (the daemon holds `after` dependents until predecessors complete).
    RUNNING.store(true, Ordering::SeqCst);
    let mut handles = Vec::new();
    for spec in forks {
        let paths = Paths::new(paths.base.clone());
        let session_id = session_id.to_string();
        let resume_target = resume_target.to_string();
        let cwd = cwd.to_path_buf();
        // `after` predecessors' reports — and, as a belt for a chain re-run
        // whose report never reached the parent (a wake that couldn't be
        // delivered), the fork's own previous report.
        let mut carried = String::new();
        for pred in &spec.after {
            if let Some(r) = reports.get(pred) {
                carried.push_str(&format!(
                    "\n\nThis fork runs after '{pred}'; its report follows so you can build on it:\n{r}"
                ));
            }
        }
        if spec.chain {
            if let Some(prev) = reports.get(&spec.name) {
                carried.push_str(&format!(
                    "\n\nYour previous run's report (not yet seen by the parent session):\n{prev}"
                ));
            }
        }
        let name = spec.name.clone();
        let h = std::thread::spawn(move || {
            run_one(
                &paths,
                &session_id,
                &resume_target,
                &cwd,
                spec,
                &carried,
                true,
            )
        });
        handles.push((name, h));
        // (reports spool under the conversation id inside run_one)
    }
    let mut outcome = WakeOutcome::default();
    for (name, h) in handles {
        let Ok(result) = h.join() else { continue };
        if let Some(report) = result.report {
            reports.insert(name, report);
        }
        if let Some(block) = result.wake_block {
            outcome.wake_blocks.push(block);
        }
        outcome.stale |= result.stale;
    }
    RUNNING.store(false, Ordering::SeqCst);
    outcome
}

/// Run one fork. `can_wake` says whether the caller can deliver a continuing
/// chain report by waking the parent (true for the parked Stop hook; false
/// for the end-runner, whose session is already gone).
fn run_one(
    paths: &Paths,
    session_id: &str,
    resume_target: &str,
    cwd: &std::path::Path,
    spec: WakeFork,
    carried: &str,
    can_wake: bool,
) -> RunResult {
    let run_ref = format!("hl:{}", crate::codex::uuid_v4());
    send(
        paths,
        RequestBody::ForkSpawned {
            session_id: session_id.to_string(),
            fork: spec.name.clone(),
            run_ref: run_ref.clone(),
        },
    );
    let spool_key = resume_target.to_string();

    let prompt = format!("{}{}", spec.prompt, carried);
    // Model candidates, tried in order: a failed run retries on the next one
    // ("if the first option is not available, the next one is used"). No
    // model at all = one inherit-the-default attempt.
    let mut candidates: Vec<Option<String>> = Vec::new();
    match &spec.model {
        Some(m) => {
            candidates.push(Some(m.clone()));
            candidates.extend(spec.model_fallbacks.iter().cloned().map(Some));
        }
        None => candidates.push(None),
    }
    let mut status = "failed";
    let mut report = String::new();
    for (i, model) in candidates.iter().enumerate() {
        let (st, rep) = run_attempt(
            session_id,
            resume_target,
            cwd,
            &spec,
            &prompt,
            model.as_deref(),
        );
        status = st;
        report = rep;
        if status == "completed" {
            break;
        }
        if i + 1 < candidates.len() {
            eprintln!(
                "[headless] fork '{}' failed on model {:?}; retrying on {:?}",
                spec.name,
                model,
                candidates[i + 1]
            );
        }
    }
    finish_run(
        paths, session_id, &spool_key, spec, run_ref, status, report, can_wake,
    )
}

/// The Claude Code tool names behind each guard tool family. `guard.tools`
/// names families; the harness denies by tool name, so every family the
/// author did NOT list becomes this many deny rules.
fn family_tools(f: autofork_core::guard::ToolFamily) -> &'static [&'static str] {
    use autofork_core::guard::ToolFamily as F;
    match f {
        F::Read => &["Read", "Glob", "Grep"],
        F::Shell => &["Bash"],
        F::Edit => &["Edit", "Write", "MultiEdit", "NotebookEdit"],
        F::Web => &["WebFetch", "WebSearch"],
        F::Task => &["Agent", "Task"],
    }
}

/// Tools that belong to no family: bookkeeping and lookup, harmless for a
/// fork that still has tools, and the last thing to take away from a
/// `tools: []` reviewer — which is meant to answer from the conversation it
/// inherited and nothing else.
const UNFAMILIED_TOOLS: [&str; 4] = ["TodoWrite", "Skill", "LSP", "ToolSearch"];

/// `Edit(//abs/path/**)` for each writable root. Permission rules spell an
/// absolute path with a DOUBLE slash (`//tmp/brain`); a single slash means
/// "relative to the project root" there, which would silently point the rule
/// at the parent's workspace — the one place the guard exists to protect.
fn guard_allow_rules(g: &autofork_core::guard::Guard) -> Vec<String> {
    g.write
        .iter()
        .map(|p| {
            let s = p.display().to_string();
            let s = s.trim_end_matches('/');
            if let Some(rest) = s.strip_prefix('/') {
                format!("Edit(//{rest}/**)")
            } else {
                // Not absolute (frontmatter parsing makes these absolute, so
                // this is belt and braces): leave it project-relative rather
                // than inventing a root.
                format!("Edit({s}/**)")
            }
        })
        .collect()
}

/// One `deny:` command pattern as Claude Code `Bash(...)` deny rules.
///
/// A permission rule matches a command by prefix: `Bash(git push)` is the
/// bare command and `Bash(git push:*)` is that command with anything after
/// it, so a pattern that names a whole command needs both — otherwise
/// `ssh` is refused and `ssh host 'rm -rf /'` sails past. A pattern that
/// already ends in a star only ever means "and whatever follows", so the
/// `:*` form alone covers it.
///
/// This is a coarser net than the analyser the parent session's hook runs
/// (which sees through `bash -c`, `xargs` and a wrapper script): a rule
/// matches the command line the model wrote. The sandbox is what catches
/// what slips past — this list is the author's explicit, readable "not this
/// command", denied before anything else is considered.
fn guard_bash_deny_rules(pattern: &str) -> Vec<String> {
    let mut tokens: Vec<&str> = pattern.split_whitespace().collect();
    let mut trailing_star = false;
    if let Some(last) = tokens.last().copied() {
        if last == "*" {
            tokens.pop();
            trailing_star = true;
        } else if let Some(stem) = last.strip_suffix('*') {
            tokens.pop();
            if !stem.is_empty() {
                tokens.push(stem);
            }
            trailing_star = true;
        }
    }
    let prefix = tokens.join(" ");
    if prefix.is_empty() {
        // A bare `*`: every command. Nothing to prefix-match on, so deny the
        // tool outright rather than emitting `Bash(:*)`, which matches
        // nothing.
        return vec!["Bash".to_string()];
    }
    if trailing_star {
        vec![format!("Bash({prefix}:*)")]
    } else {
        vec![format!("Bash({prefix})"), format!("Bash({prefix}:*)")]
    }
}

/// Whole-tool deny rules for a guard: the web tools when the fork has no
/// network, every tool of every family the author did not list, and the
/// author's own `deny:` command patterns.
fn guard_deny_rules(g: &autofork_core::guard::Guard) -> Vec<String> {
    use autofork_core::guard::ToolFamily;
    let mut out: Vec<String> = Vec::new();
    let mut push = |name: &str| {
        let rule = name.to_string();
        if !out.contains(&rule) {
            out.push(rule);
        }
    };
    if !g.network {
        // The sandbox only governs Bash; WebFetch and WebSearch run
        // in-process and are gated by permission rules alone.
        push("WebFetch");
        push("WebSearch");
    }
    if let Some(allowed) = &g.tools {
        for f in ToolFamily::ALL {
            if !allowed.contains(&f) {
                for t in family_tools(f) {
                    push(t);
                }
            }
        }
        if allowed.is_empty() {
            for t in UNFAMILIED_TOOLS {
                push(t);
            }
        }
    }
    for pattern in &g.deny {
        for rule in guard_bash_deny_rules(pattern) {
            push(&rule);
        }
    }
    out
}

/// Whether the fork's working directory (the parent session's project root)
/// must be closed to sandboxed commands. The sandbox writes to the working
/// directory by default, so a guard that does not name it would otherwise
/// leave the parent's workspace open to `sh -c 'echo … > file'` even though
/// the Edit tool is refused there. The deny is skipped when the write set
/// and the cwd overlap in either direction: a `denyWrite` is documented to
/// hold inside a wider `allowWrite` (only the READ lists document a narrower
/// allow re-opening a denied region), so denying a cwd that contains a
/// writable root would take that root away too.
fn guard_denies_cwd(g: &autofork_core::guard::Guard, cwd: &std::path::Path) -> bool {
    !g.write
        .iter()
        .any(|w| w.starts_with(cwd) || cwd.starts_with(w))
}

/// The `--settings` JSON for one guarded run.
///
/// A fork gets no hooks (see `run_attempt`), so a PreToolUse hook — how the
/// guard is enforced in the parent's own session — is not available here.
/// Enforcement is therefore two settings layers that need no hook:
///
/// - the native Bash **sandbox**, which the OS enforces (Seatbelt on macOS,
///   bubblewrap on Linux) on every command and child process: writes only
///   under the write set, network only to the hosts the write set's own git
///   remotes use. `allowUnsandboxedCommands: false` removes the model's
///   `dangerouslyDisableSandbox` escape hatch, and `failIfUnavailable` turns
///   a missing sandbox into a failed run rather than a silently unguarded
///   one. Claude Code reports a sandbox violation in the blocked command's
///   result, naming the path or host — so the model reads a specific reason,
///   which is the whole point of a guard.
/// - **permission rules**, which gate the in-process tools the sandbox never
///   sees (Edit/Write, WebFetch, subagents). `deny` always wins; the `allow`
///   list is what makes the write set usable at all, because a guarded run
///   is forced into permission mode `default`, where anything not explicitly
///   allowed would need a prompt nobody can answer.
///
/// The author's `message:` is not in here: it rides on
/// `--append-system-prompt` (`wake::guard_paragraph`), so the fork reads its
/// scope before its first tool call and hears the same words back when a
/// rule blocks it.
fn guard_settings(
    g: &autofork_core::guard::Guard,
    disable_hooks: bool,
    cwd: &std::path::Path,
) -> serde_json::Value {
    use serde_json::json;

    let writes: Vec<String> = g
        .write
        .iter()
        .map(|p| p.display().to_string().trim_end_matches('/').to_string())
        .collect();

    let mut filesystem = json!({ "allowWrite": writes });
    if guard_denies_cwd(g, cwd) {
        filesystem["denyWrite"] = json!([cwd.display().to_string()]);
    }

    // Network. `strictAllowlist` is what makes the allowlist a wall: without
    // it a host outside the list is decided by permission mode, which in a
    // headless `default`-mode run means "ask" — and an unanswerable prompt
    // is a hung command rather than a refusal the model can read. It is
    // honoured from user, managed and CLI `--settings` settings, which is
    // what we are.
    //
    // A fork WITH network still goes through the allowlist (there is no
    // documented "allow all" switch, and a bare `*` is documented only for
    // `WebFetch(domain:*)` rules, not for `allowedDomains`) — so that is
    // exactly what we add: the sandbox's allowlist is `allowedDomains` PLUS
    // the domains of `WebFetch(domain:...)` allow rules, and there a bare
    // `*` matches every host. One rule therefore opens both the sandbox and
    // the WebFetch tool, which is what `network: true` means.
    let mut permissions_allow = guard_allow_rules(g);
    if g.network {
        permissions_allow.push("WebFetch(domain:*)".to_string());
    }

    let mut settings = json!({
        "sandbox": {
            "enabled": true,
            "autoAllowBashIfSandboxed": true,
            "allowUnsandboxedCommands": false,
            "excludedCommands": [],
            "failIfUnavailable": true,
            "filesystem": filesystem,
            "network": {
                "allowedDomains": g.remote_hosts(),
                "strictAllowlist": true,
            },
        },
        "permissions": {
            "allow": permissions_allow,
            "deny": guard_deny_rules(g),
        },
    });
    if disable_hooks {
        settings["disableAllHooks"] = json!(true);
    }
    settings
}

/// One `claude -p` attempt on one model candidate.
fn run_attempt(
    session_id: &str,
    resume_target: &str,
    cwd: &std::path::Path,
    spec: &WakeFork,
    prompt: &str,
    model: Option<&str>,
) -> (&'static str, String) {
    let mut cmd = Command::new(claude_bin());
    cmd.arg("-p")
        .arg("--resume")
        .arg(resume_target)
        .arg("--fork-session")
        .arg("--output-format")
        .arg("json");
    if let Some(m) = model {
        cmd.arg("--model").arg(m);
    }
    // Session-scoped Stop hooks outlive the session that set them: Claude
    // Code restores the one `/goal` installs from the transcript on every
    // `--resume`, and `--fork-session` is a resume. Inside a headless fork
    // that hook refuses the stop — the run never terminates, so no report is
    // captured, no `<<autofork:continue>>` sentinel survives, the parent is
    // never woken, and the fork wanders off doing the parent's work against
    // the parent's own workspace. `disableAllHooks` in flag settings gates
    // the restore (the resume path checks the same gate `/goal` itself does)
    // and keeps the fork from firing the user's settings/plugin hooks, which
    // a throwaway reviewer has no business triggering anyway — autofork's own
    // hooks already no-op on AUTOFORK_FORK=1. `AUTOFORK_FORK_HOOKS=1` opts
    // back in for anyone whose forks depend on a hook.
    let disable_hooks = std::env::var_os("AUTOFORK_FORK_HOOKS").is_none();
    match spec.guard.as_ref() {
        // Guarded run. The mode is forced to `default` (Manual) whatever the
        // fork's `mode:` or the config says: `acceptEdits` and
        // `bypassPermissions` both approve edits the guard's `allow` list
        // does not cover, which would turn the write set into a suggestion.
        // In `-p` there is nobody to answer a prompt, so "needs approval"
        // resolves to "denied, do not retry" — exactly the wall a guard
        // wants, with the allow rules cutting a hole for the write set.
        Some(g) => {
            cmd.arg("--permission-mode").arg("default");
            cmd.arg("--settings")
                .arg(guard_settings(g, disable_hooks, cwd).to_string());
            // The guard's prose, in the fork's own system prompt. A resumed
            // conversation replays the system prompt it recorded on its
            // first request, so an appended prompt would be dropped on the
            // floor without `--system-prompt-snapshot off`, which re-renders
            // it per request.
            cmd.arg("--append-system-prompt")
                .arg(autofork_core::wake::guard_paragraph(g));
            cmd.arg("--system-prompt-snapshot").arg("off");
        }
        // Unguarded run. Headless runs cannot answer permission prompts;
        // without a mode a write simply stalls until the run times out.
        // `acceptEdits` is the smallest mode that lets typical consolidation
        // forks do their file work.
        None => {
            cmd.arg("--permission-mode")
                .arg(spec.mode.as_deref().unwrap_or("acceptEdits"));
            if disable_hooks {
                cmd.arg("--settings").arg(r#"{"disableAllHooks":true}"#);
            }
        }
    }
    cmd.arg(prompt)
        .current_dir(cwd)
        .env("AUTOFORK_FORK", "1")
        .env("AUTOFORK_SESSION_ID", session_id)
        .env("AUTOFORK_FORK_NAME", &spec.name)
        .env("AUTOFORK_FORK_PATH", &spec.path)
        .env("AUTOFORK_TRIGGER", &spec.trigger)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Claude Code scrubbed CLAUDE_CODE_OAUTH_TOKEN from our env on the way
    // into the hook; a token-only session gets its fork runs authenticated
    // through AUTOFORK_CLAUDE_CODE_OAUTH_TOKEN instead (see `runenv`).
    autofork_core::runenv::apply_oauth_override(&mut cmd);
    // Detach from the controlling terminal: closing the parent's terminal
    // window SIGHUPs the process group, and a fork's WORK should survive the
    // session closing even when its report cannot.
    autofork_core::sys::detach(&mut cmd);

    match cmd.spawn() {
        Ok(mut child) => {
            let mut out = String::new();
            let deadline = std::time::Instant::now() + fork_timeout();
            let mut stdout = child.stdout.take();
            // `-p` writes its JSON result once at the end; read on a helper
            // thread so the wall-clock cap can kill a hung run.
            let reader = std::thread::spawn(move || {
                let mut s = String::new();
                if let Some(o) = stdout.as_mut() {
                    let _ = o.read_to_string(&mut s);
                }
                s
            });
            let exited = loop {
                match child.try_wait() {
                    Ok(Some(st)) => break Some(st),
                    Ok(None) if std::time::Instant::now() > deadline => {
                        let _ = child.kill();
                        break None;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(500)),
                    Err(_) => break None,
                }
            };
            out.push_str(&reader.join().unwrap_or_default());
            let parsed: Option<serde_json::Value> = serde_json::from_str(out.trim()).ok();
            let ok = exited.map(|s| s.success()).unwrap_or(false)
                && parsed
                    .as_ref()
                    .map(|v| v["is_error"] != serde_json::Value::Bool(true))
                    .unwrap_or(false);
            let text = parsed
                .and_then(|v| v["result"].as_str().map(str::to_string))
                .unwrap_or_default();
            (if ok { "completed" } else { "failed" }, text)
        }
        Err(e) => {
            eprintln!("[headless] fork '{}' spawn failed: {e}", spec.name);
            ("failed", String::new())
        }
    }
}

/// Sentinel handling, delivery and the completion frame for a finished run.
#[allow(clippy::too_many_arguments)]
fn finish_run(
    paths: &Paths,
    session_id: &str,
    spool_key: &str,
    spec: WakeFork,
    run_ref: String,
    status: &'static str,
    mut report: String,
    can_wake: bool,
) -> RunResult {
    report = report.trim().to_string();
    let chain_next =
        status == "completed" && spec.chain && autofork_core::wake::wants_continue(&report);
    if chain_next {
        report = autofork_core::wake::strip_continue(&report);
    }

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
    let block = autofork_core::wake::report_block(&spec.name, &spec.trigger, status, &body);
    // Did the pause this run was selected in end while it was in flight (the
    // user spoke; a background task's completion started a new pause)? Only
    // the parked hook asks: the end-runner's session is gone, nothing can
    // move its pause on. Old daemons answer Error — treated as fresh.
    let stale = can_wake && run_is_stale(paths, session_id, &run_ref);
    // A chain run's report is an evaluation of one stop — "here is what the
    // parent should do next, given where it stopped". If the user has spoken
    // since, that stop is history and the verdict is about a conversation
    // that no longer exists: delivering it would wake (or later feed) the
    // parent with instructions for the wrong moment. Drop it; the fork
    // re-evaluates at the new pause's first Stop, and THAT report supersedes
    // this one. Non-chain runs (a journal, a handover) still spool: their
    // report is a record of work done, not a verdict on a moment.
    let discard = stale && spec.chain;
    if discard {
        eprintln!(
            "[headless] fork '{}' finished after the user moved on; its report is stale and dropped \
             (the fork re-evaluates at the next stop)",
            spec.name
        );
    }
    // A continuing chain report is the goal loop's handoff: the parent is the
    // worker, and the loop only advances once it has SEEN the report. So it
    // goes back to the caller, which wakes the session with it, instead of
    // waiting silently in the spool for the user's next prompt. Everything
    // else — settled chains included — spools under the CONVERSATION id,
    // which survives session resume (a resumed leg gets a fresh session id),
    // so a report finished after you left still reaches you when you pick the
    // conversation back up.
    let wake_block = (chain_next && can_wake && !discard).then(|| block.clone());
    if wake_block.is_none() && !discard {
        send(
            paths,
            RequestBody::SpoolReport {
                session_id: spool_key.to_string(),
                fork: spec.name.clone(),
                text: block,
            },
        );
    }
    send(
        paths,
        RequestBody::ForkCompleted {
            session_id: session_id.to_string(),
            fork: spec.name.clone(),
            run_ref,
            status: status.to_string(),
            cont: chain_next.then_some(true),
        },
    );
    RunResult {
        // A discarded report is carried nowhere either: the next chain
        // iteration must not be told "your previous run said X" about a
        // stop that is history.
        report: (status == "completed" && !report.is_empty() && !discard).then_some(report),
        wake_block,
        stale,
    }
}

/// Ask the daemon whether a run's pause has moved on. Any failure (no
/// daemon, an old daemon answering Error) reads as fresh: the pre-v0.30
/// behavior, never a dropped report by accident.
fn run_is_stale(paths: &Paths, session_id: &str, run_ref: &str) -> bool {
    let Ok(mut client) = Client::connect_or_spawn(paths, Duration::from_secs(5)) else {
        return false;
    };
    matches!(
        client.request(RequestBody::RunState {
            session_id: session_id.to_string(),
            run_ref: run_ref.to_string(),
        }),
        Ok(ResponseBody::RunState { stale: true })
    )
}

fn send(paths: &Paths, body: RequestBody) {
    if let Ok(mut client) = Client::connect_or_spawn(paths, Duration::from_secs(5)) {
        let _ = client.request(body);
    }
}

/// Serialize the final-run specs and spawn the detached end-runner process.
/// Called from SessionEnd hooks BEFORE the session-end event (which purges
/// the roster). Fire-and-forget: the runner outlives both the hook and the
/// closing session.
#[allow(clippy::too_many_arguments)]
pub fn spawn_final_runner(
    paths: &Paths,
    client: &str,
    session_id: &str,
    resume_target: &str,
    cwd: &std::path::Path,
    parent_model: Option<&str>,
    parent_permission_mode: Option<&str>,
    harness_bin: Option<&std::path::Path>,
    specs: &[WakeFork],
) {
    if specs.is_empty() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let tmp = paths.base.join("tmp");
    let _ = std::fs::create_dir_all(&tmp);
    let specs_path = tmp.join(format!("final-{}.json", crate::codex::uuid_v4()));
    let Ok(json) = serde_json::to_string(specs) else {
        return;
    };
    if std::fs::write(&specs_path, json).is_err() {
        return;
    }
    let log_path = paths.base.join("logs/final-run.log");
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(log) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    else {
        return;
    };
    let Ok(log2) = log.try_clone() else { return };
    let mut cmd = Command::new(exe);
    cmd.arg("final-run")
        .arg("--client")
        .arg(client)
        .arg("--session")
        .arg(session_id)
        .arg("--resume-target")
        .arg(resume_target)
        .arg("--cwd")
        .arg(cwd)
        .arg("--specs")
        .arg(&specs_path);
    if let Some(m) = parent_model {
        cmd.arg("--model").arg(m);
    }
    if let Some(m) = parent_permission_mode {
        cmd.arg("--permission-mode").arg(m);
    }
    if let Some(b) = harness_bin {
        cmd.arg("--bin").arg(b);
    }
    let _ = autofork_core::sys::spawn_detached(&mut cmd, log, log2);
}

/// `autofork final-run`: execute a flush-on-close batch after the parent
/// session died. Specs arrive topologically ordered from the daemon; runs are
/// sequential so `after` reports pipe locally.
#[allow(clippy::too_many_arguments)]
pub fn run_final(
    paths: &Paths,
    client: &str,
    session_id: &str,
    resume_target: &str,
    cwd: &std::path::Path,
    parent_model: Option<&str>,
    parent_permission_mode: Option<&str>,
    specs: Vec<WakeFork>,
) {
    let mut reports: HashMap<String, String> = HashMap::new();
    for spec in specs {
        let mut carried = String::new();
        for pred in &spec.after {
            if let Some(r) = reports.get(pred) {
                carried.push_str(&format!(
                    "\n\nThis fork runs after '{pred}'; its report follows so you can build on it:\n{r}"
                ));
            }
        }
        let name = spec.name.clone();
        let report = match client {
            "codex" => crate::codex::run_final_codex(
                paths,
                session_id,
                cwd,
                parent_model,
                parent_permission_mode,
                spec,
                &carried,
            ),
            "opencode" => run_final_opencode(paths, session_id, cwd, spec, &carried),
            // No parent left to wake: a continuing chain report spools for
            // the conversation's next leg like any other.
            _ => run_one(paths, session_id, resume_target, cwd, spec, &carried, false).report,
        };
        if let Some(r) = report {
            reports.insert(name, r);
        }
    }
}

/// The `opencode run` flags for one fork run (the prompt is appended by the
/// caller, after these).
///
/// Two of them are what make the run able to do anything at all:
///
/// - `--auto`: a headless run cannot answer a permission prompt. Its stdin is
///   null and the instance that would have shown the dialog is gone, so every
///   tool call needing approval is refused and the run exits 0 having read a
///   few files and written nothing. This is opencode's counterpart to the
///   `--permission-mode` the Claude Code path passes and the sandbox flags the
///   codex path passes. It auto-approves only what is not explicitly denied,
///   so an agent's own permission config still governs the run.
/// - `--agent`: a fork's `mode:` names the opencode AGENT to run as (permission
///   mode on Claude Code, sandbox on codex, agent here). The live plugin path
///   pins it on the forked session; without it here, `mode:` and config
///   `[fork_modes]` were silently dropped on the close path — including the
///   read-only agent someone would pick to keep a fork from writing.
// NOTE: there is deliberately no `--title` here. A headless fork run would
// ideally carry our `autofork/<fork> (<trigger>)` title so the plugin's
// TITLE_PREFIX check and its startup sweep apply to it — but opencode's
// `run --fork` ignores `--title` outright (verified on 1.18.5): the flag
// only names a NEW session, while the fork path calls `session.fork()`,
// which always derives `<parent title> (fork #N)` from the parent. Passing
// it would look like a fix and do nothing. The fork copy is instead made
// harmless by the AUTOFORK_FORK env guard (its plugin instance and hook
// bridge are both inert) and swept by title pattern.
fn opencode_run_args(session_id: &str, model: Option<&str>, mode: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "-s".to_string(),
        session_id.to_string(),
        "--fork".to_string(),
    ];
    if let Some(m) = model {
        args.push("-m".to_string());
        args.push(m.to_string());
    }
    if let Some(agent) = mode {
        args.push("--agent".to_string());
        args.push(agent.to_string());
    }
    args.push("--auto".to_string());
    args
}

/// One flush-on-close opencode run: `opencode run -s <id> --fork` continues a
/// fork of the closed session headlessly (verified byte-identical request
/// prefixes). The report has nowhere to go (no live instance, no queue), so
/// only the run's WORK matters; leftover fork sessions are cleaned by the
/// plugin's startup sweep, which matches opencode's own `(fork #N)` title
/// plus the spawn-prompt fingerprint. The child inherits AUTOFORK_FORK=1,
/// which makes its own plugin instance and `autofork opencode hook` inert —
/// a fork copy of a real session must never register as one.
fn run_final_opencode(
    paths: &Paths,
    session_id: &str,
    cwd: &std::path::Path,
    spec: WakeFork,
    carried: &str,
) -> Option<String> {
    let run_ref = format!("fr:{}", crate::codex::uuid_v4());
    send(
        paths,
        RequestBody::ForkSpawned {
            session_id: session_id.to_string(),
            fork: spec.name.clone(),
            run_ref: run_ref.clone(),
        },
    );
    let prompt = format!("{}{}", spec.prompt, carried);
    let mut candidates: Vec<Option<String>> = Vec::new();
    match &spec.model {
        Some(m) => {
            candidates.push(Some(m.clone()));
            candidates.extend(spec.model_fallbacks.iter().cloned().map(Some));
        }
        None => candidates.push(None),
    }
    let opencode_bin = std::env::var("AUTOFORK_OPENCODE_BIN")
        .ok()
        .or_else(harness_bin)
        .unwrap_or_else(|| "opencode".to_string());
    let mut status = "failed";
    let mut report = String::new();
    for model in &candidates {
        let mut cmd = Command::new(&opencode_bin);
        cmd.args(opencode_run_args(
            session_id,
            model.as_deref(),
            spec.mode.as_deref(),
        ));
        cmd.arg(&prompt)
            .current_dir(cwd)
            .env("AUTOFORK_FORK", "1")
            .env("AUTOFORK_SESSION_ID", session_id)
            .env("AUTOFORK_FORK_NAME", &spec.name)
            .env("AUTOFORK_FORK_PATH", &spec.path)
            .env("AUTOFORK_TRIGGER", &spec.trigger)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let out = cmd.output();
        match out {
            Ok(o) if o.status.success() => {
                status = "completed";
                report = String::from_utf8_lossy(&o.stdout).trim().to_string();
                break;
            }
            _ => status = "failed",
        }
    }
    send(
        paths,
        RequestBody::ForkCompleted {
            session_id: session_id.to_string(),
            fork: spec.name.clone(),
            run_ref,
            status: status.to_string(),
            cont: None,
        },
    );
    (status == "completed" && !report.is_empty()).then_some(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opencode_fork_runs_can_use_tools() {
        // Without `--auto` the run is denied every tool call that needs
        // approval — nobody is there to answer — and finishes having done
        // nothing.
        let args = opencode_run_args("ses_1", None, None);
        assert_eq!(args, ["run", "-s", "ses_1", "--fork", "--auto"]);
    }

    #[test]
    fn opencode_fork_runs_honor_model_and_mode() {
        // `mode:` is the agent on opencode, and it reaches the close path's
        // runs the same way it reaches the live plugin's.
        let args = opencode_run_args("ses_1", Some("anthropic/claude-haiku-4-5"), Some("plan"));
        assert_eq!(
            args,
            [
                "run",
                "-s",
                "ses_1",
                "--fork",
                "-m",
                "anthropic/claude-haiku-4-5",
                "--agent",
                "plan",
                "--auto",
            ]
        );
    }

    use autofork_core::guard::{Guard, ToolFamily};
    use serde_json::json;
    use std::path::{Path, PathBuf};

    fn guard(write: &[&str]) -> Guard {
        Guard {
            write: write.iter().map(PathBuf::from).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn guard_settings_confine_writes_and_network() {
        // The whole enforcement surface of a guarded headless run, spelled
        // out: no write outside the list, no host but the write set's own
        // git remotes, no escape hatch, and the web tools off.
        let g = guard(&["/tmp/brain"]);
        let v = guard_settings(&g, true, Path::new("/work/proj"));
        assert_eq!(
            v,
            json!({
                "disableAllHooks": true,
                "sandbox": {
                    "enabled": true,
                    "autoAllowBashIfSandboxed": true,
                    "allowUnsandboxedCommands": false,
                    "excludedCommands": [],
                    "failIfUnavailable": true,
                    "filesystem": {
                        "allowWrite": ["/tmp/brain"],
                        "denyWrite": ["/work/proj"],
                    },
                    "network": {
                        "allowedDomains": [],
                        "strictAllowlist": true,
                    },
                },
                "permissions": {
                    "allow": ["Edit(//tmp/brain/**)"],
                    "deny": ["WebFetch", "WebSearch"],
                },
            })
        );
    }

    #[test]
    fn guard_settings_keep_hooks_when_opted_back_in() {
        // `AUTOFORK_FORK_HOOKS=1` only drops the hook gate; every guard key
        // stays.
        let v = guard_settings(&guard(&["/tmp/brain"]), false, Path::new("/work/proj"));
        assert!(v.get("disableAllHooks").is_none(), "{v}");
        assert_eq!(v["sandbox"]["enabled"], json!(true));
    }

    #[test]
    fn guard_edit_rules_use_the_double_slash_absolute_form() {
        // `Edit(/tmp/brain/**)` with ONE slash is a project-relative rule —
        // it would point at the parent's workspace instead.
        assert_eq!(
            guard_allow_rules(&guard(&["/tmp/brain/", "/var/tmp/out"])),
            ["Edit(//tmp/brain/**)", "Edit(//var/tmp/out/**)"]
        );
    }

    #[test]
    fn guard_with_network_opens_the_allowlist_with_a_webfetch_wildcard() {
        // There is no `allowedDomains: ["*"]`; the sandbox's allowlist is
        // `allowedDomains` plus `WebFetch(domain:...)` allow rules, and a
        // bare `*` is honoured there. The same rule un-denies the web tools.
        let g = Guard {
            network: true,
            ..guard(&["/tmp/brain"])
        };
        let v = guard_settings(&g, true, Path::new("/work/proj"));
        assert_eq!(
            v["permissions"]["allow"],
            json!(["Edit(//tmp/brain/**)", "WebFetch(domain:*)"])
        );
        assert_eq!(v["permissions"]["deny"], json!([]));
        assert_eq!(v["sandbox"]["network"]["strictAllowlist"], json!(true));
    }

    #[test]
    fn guard_denies_every_family_the_author_left_out() {
        let g = Guard {
            tools: Some(vec![ToolFamily::Read]),
            ..guard(&[])
        };
        assert_eq!(
            guard_deny_rules(&g),
            [
                "WebFetch",
                "WebSearch",
                "Bash",
                "Edit",
                "Write",
                "MultiEdit",
                "NotebookEdit",
                "Agent",
                "Task",
            ]
        );
    }

    #[test]
    fn guard_with_no_tools_denies_the_unfamilied_ones_too() {
        // `tools: []` is a reviewer that answers from the conversation it
        // inherited; even TodoWrite is taken away.
        let g = Guard {
            tools: Some(vec![]),
            network: true,
            ..guard(&[])
        };
        let deny = guard_deny_rules(&g);
        for t in [
            "Read",
            "Glob",
            "Grep",
            "Bash",
            "Edit",
            "Write",
            "MultiEdit",
            "NotebookEdit",
            "WebFetch",
            "WebSearch",
            "Agent",
            "Task",
            "TodoWrite",
            "Skill",
            "LSP",
            "ToolSearch",
        ] {
            assert!(deny.contains(&t.to_string()), "{t} missing from {deny:?}");
        }
        // No duplicates: the web family can be excluded both by `tools:` and
        // by having no network.
        let mut sorted = deny.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), deny.len(), "{deny:?}");
    }

    #[test]
    fn guard_deny_patterns_become_bash_prefix_rules() {
        // A whole-command pattern needs both forms: without `Bash(ssh)` the
        // bare command runs, without `Bash(ssh:*)` every invocation with an
        // argument does.
        assert_eq!(guard_bash_deny_rules("ssh"), ["Bash(ssh)", "Bash(ssh:*)"]);
        // A star already means "and whatever follows".
        assert_eq!(guard_bash_deny_rules("git push*"), ["Bash(git push:*)"]);
        assert_eq!(guard_bash_deny_rules("rm -rf *"), ["Bash(rm -rf:*)"]);
        // Nothing to prefix-match on: take the tool away instead of
        // emitting a rule that matches nothing.
        assert_eq!(guard_bash_deny_rules("*"), ["Bash"]);
    }

    #[test]
    fn guard_deny_patterns_reach_the_settings() {
        let g = Guard {
            deny: vec!["ssh".into(), "gh pr comment".into()],
            network: true,
            ..guard(&["/tmp/brain"])
        };
        assert_eq!(
            guard_settings(&g, true, Path::new("/work/proj"))["permissions"]["deny"],
            json!([
                "Bash(ssh)",
                "Bash(ssh:*)",
                "Bash(gh pr comment)",
                "Bash(gh pr comment:*)"
            ])
        );
    }

    #[test]
    fn guard_does_not_deny_a_cwd_that_overlaps_the_write_set() {
        // Denying the cwd would take a writable root inside it away with it:
        // a `denyWrite` holds inside a wider `allowWrite`.
        assert!(!guard_denies_cwd(
            &guard(&["/work/proj/docs"]),
            Path::new("/work/proj")
        ));
        assert!(!guard_denies_cwd(
            &guard(&["/work/proj"]),
            Path::new("/work/proj/crates")
        ));
        assert!(guard_denies_cwd(
            &guard(&["/tmp/brain"]),
            Path::new("/work/proj")
        ));
        // A read-only fork (no write set at all) closes the cwd.
        assert!(guard_denies_cwd(&guard(&[]), Path::new("/work/proj")));
    }
}
