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
use autofork_core::protocol::{RequestBody, WakeFork};
use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
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
) -> Vec<String> {
    // Batch-parallel like the opencode plugin: each fork run is independent
    // (the daemon holds `after` dependents until predecessors complete).
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
    let mut wake_blocks = Vec::new();
    for (name, h) in handles {
        let Ok(result) = h.join() else { continue };
        if let Some(report) = result.report {
            reports.insert(name, report);
        }
        if let Some(block) = result.wake_block {
            wake_blocks.push(block);
        }
    }
    wake_blocks
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
    // Headless runs cannot answer permission prompts; without a mode a write
    // simply stalls until the run times out. `acceptEdits` is the smallest
    // mode that lets typical consolidation forks do their file work.
    cmd.arg("--permission-mode")
        .arg(spec.mode.as_deref().unwrap_or("acceptEdits"));
    cmd.arg(prompt)
        .current_dir(cwd)
        .env("AUTOFORK_FORK", "1")
        .env("AUTOFORK_SESSION_ID", session_id)
        .env("AUTOFORK_FORK_NAME", &spec.name)
        .env("AUTOFORK_TRIGGER", &spec.trigger)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Detach from the controlling terminal: closing the parent's terminal
    // window SIGHUPs the process group, and a fork's WORK should survive the
    // session closing even when its report cannot.
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
    // A continuing chain report is the goal loop's handoff: the parent is the
    // worker, and the loop only advances once it has SEEN the report. So it
    // goes back to the caller, which wakes the session with it, instead of
    // waiting silently in the spool for the user's next prompt. Everything
    // else — settled chains included — spools under the CONVERSATION id,
    // which survives session resume (a resumed leg gets a fresh session id),
    // so a report finished after you left still reaches you when you pick the
    // conversation back up.
    let wake_block = (chain_next && can_wake).then(|| block.clone());
    if wake_block.is_none() {
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
        report: (status == "completed" && !report.is_empty()).then_some(report),
        wake_block,
    }
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
        .arg(&specs_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2));
    if let Some(m) = parent_model {
        cmd.arg("--model").arg(m);
    }
    if let Some(m) = parent_permission_mode {
        cmd.arg("--permission-mode").arg(m);
    }
    if let Some(b) = harness_bin {
        cmd.arg("--bin").arg(b);
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

/// One flush-on-close opencode run: `opencode run -s <id> --fork` continues a
/// fork of the closed session headlessly (verified byte-identical request
/// prefixes). The report has nowhere to go (no live instance, no queue), so
/// only the run's WORK matters; leftover fork sessions are cleaned by the
/// plugin's startup sweep, which also matches the spawn-prompt fingerprint.
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
        cmd.arg("run").arg("-s").arg(session_id).arg("--fork");
        if let Some(m) = model {
            cmd.arg("-m").arg(m);
        }
        cmd.arg(&prompt)
            .current_dir(cwd)
            .env("AUTOFORK_FORK", "1")
            .env("AUTOFORK_SESSION_ID", session_id)
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
