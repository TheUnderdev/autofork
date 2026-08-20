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

fn claude_bin() -> String {
    std::env::var("AUTOFORK_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string())
}

fn fork_timeout() -> Duration {
    let secs = std::env::var("AUTOFORK_CLAUDE_FORK_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(FORK_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// The report block spooled for silent delivery. Carries the wake marker so
/// nothing downstream mistakes it for user text.
fn report_block(fork: &str, trigger: &str, status: &str, body: &str) -> String {
    format!("---\nsource: autofork\nfork: {fork} (trigger: {trigger}) — {status}\n---\n{body}")
}

/// Consume one wake's forks headlessly, then return so the caller re-parks.
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
) {
    // Batch-parallel like the opencode plugin: each fork run is independent
    // (the daemon holds `after` dependents until predecessors complete).
    let mut handles = Vec::new();
    for spec in forks {
        let paths = Paths::new(paths.base.clone());
        let session_id = session_id.to_string();
        let resume_target = resume_target.to_string();
        let cwd = cwd.to_path_buf();
        // `after` predecessors' reports — and, for a chain re-run, the fork's
        // own previous report (the parent never saw it mid-chain, so the next
        // iteration must carry it itself).
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
            run_one(&paths, &session_id, &resume_target, &cwd, spec, &carried)
        });
        handles.push((name, h));
    }
    for (name, h) in handles {
        if let Ok(Some(report)) = h.join() {
            reports.insert(name, report);
        }
    }
}

/// Run one fork; returns the (sentinel-stripped) report on completion.
fn run_one(
    paths: &Paths,
    session_id: &str,
    resume_target: &str,
    cwd: &std::path::Path,
    spec: WakeFork,
    carried: &str,
) -> Option<String> {
    let run_ref = format!("hl:{}", crate::codex::uuid_v4());
    send(
        paths,
        RequestBody::ForkSpawned {
            session_id: session_id.to_string(),
            fork: spec.name.clone(),
            run_ref: run_ref.clone(),
        },
    );

    let prompt = format!("{}{}", spec.prompt, carried);
    let mut cmd = Command::new(claude_bin());
    cmd.arg("-p")
        .arg("--resume")
        .arg(resume_target)
        .arg("--fork-session")
        .arg("--output-format")
        .arg("json");
    if let Some(m) = &spec.model {
        cmd.arg("--model").arg(m);
    }
    // Headless runs cannot answer permission prompts; without a mode a write
    // simply stalls until the run times out. `acceptEdits` is the smallest
    // mode that lets typical consolidation forks do their file work.
    cmd.arg("--permission-mode")
        .arg(spec.mode.as_deref().unwrap_or("acceptEdits"));
    cmd.arg(&prompt)
        .current_dir(cwd)
        .env("AUTOFORK_FORK", "1")
        .env("AUTOFORK_SESSION_ID", session_id)
        .env("AUTOFORK_FORK_NAME", &spec.name)
        .env("AUTOFORK_TRIGGER", &spec.trigger)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let (status, mut report) = match cmd.spawn() {
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
    };

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
    send(
        paths,
        RequestBody::SpoolReport {
            session_id: session_id.to_string(),
            fork: spec.name.clone(),
            text: report_block(&spec.name, &spec.trigger, status, &body),
        },
    );
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
    (status == "completed" && !report.is_empty()).then_some(report)
}

fn send(paths: &Paths, body: RequestBody) {
    if let Ok(mut client) = Client::connect_or_spawn(paths, Duration::from_secs(5)) {
        let _ = client.request(body);
    }
}
