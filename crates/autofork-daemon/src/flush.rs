//! Daemon-side `flush_on_close`: run the idle forks a session never got to,
//! from whichever close path noticed first.
//!
//! The `SessionEnd` hook takes the same batch (`TakeFinalRuns`) and spawns the
//! same end-runner — when it gets the chance. It often doesn't: a client can
//! exit before its hook finishes, or without running it at all, and a session
//! that ends mid-turn has no hook in flight to begin with. The daemon outlives
//! all of that, so every close it detects itself (`gone`, `lost`) flushes here
//! instead of silently dropping the batch.
//!
//! Selection is the same `build_final_runs` the hook path uses, and it stamps
//! what it hands out, so whichever side gets there first runs the forks and
//! the other finds nothing left to take.

use crate::daemon::Daemon;
use autofork_core::protocol::WakeFork;
use autofork_core::store::SessionRow;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

/// Whether a close of this kind should flush. Everything the client itself
/// reports does (`clear`/`logout`/`prompt_input_exit`/`other`/`ended`, and
/// opencode's `disposed`/`deleted` — the same set the SessionEnd hook has
/// always flushed on), plus the two the daemon detects while the session was
/// alive moments ago (`gone`, `lost`).
///
/// `timeout` and `pruned` do not: those reapers speak for sessions that died
/// hours ago, often before this daemon even started, and resuming such a
/// conversation to run consolidation forks would be work on stale context.
fn flushable(reason: &str) -> bool {
    !matches!(reason, "timeout" | "pruned")
}

/// Select the session's unfired idle forks (stamping them) if this close
/// should flush. Called BEFORE the row is closed — closing purges the roster.
pub fn take_final_runs_for_close(
    daemon: &Arc<Daemon>,
    row: &SessionRow,
    reason: &str,
) -> Vec<WakeFork> {
    if !flushable(reason) {
        return Vec::new();
    }
    if !daemon.cfg_for(Some(&row.project_root)).flush_on_close {
        return Vec::new();
    }
    if daemon.is_fork_run_session(&row.session_id) {
        return Vec::new();
    }
    crate::planner::build_final_runs(daemon, row)
}

/// Spawn the detached `autofork final-run` end-runner for a closed session.
/// Mirrors the CLI's own `spawn_final_runner`, from the daemon's side.
pub fn spawn_end_runner(daemon: &Arc<Daemon>, row: &SessionRow, specs: &[WakeFork]) {
    if specs.is_empty() {
        return;
    }
    let Some(cli) = cli_binary() else {
        tracing::warn!(session = %row.session_id, "flush-on-close: no autofork CLI to run it with");
        return;
    };
    let tmp = daemon.paths.base.join("tmp");
    let _ = std::fs::create_dir_all(&tmp);
    let Ok(json) = serde_json::to_string(specs) else {
        return;
    };
    let specs_path = tmp.join(format!(
        "final-{}-{}.json",
        row.session_id,
        crate::daemon::now()
    ));
    if std::fs::write(&specs_path, json).is_err() {
        return;
    }
    let log_path = daemon.paths.base.join("logs/final-run.log");
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
    // The conversation id (transcript stem) is what `--fork-session` resumes;
    // fall back to the session id, as the hook path does.
    let resume_target = row
        .transcript_path
        .as_deref()
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| row.session_id.clone());
    let mut cmd = Command::new(cli);
    cmd.arg("final-run")
        .arg("--client")
        .arg(row.client.as_deref().unwrap_or("claude-code"))
        .arg("--session")
        .arg(&row.session_id)
        .arg("--resume-target")
        .arg(&resume_target)
        .arg("--cwd")
        .arg(&row.cwd)
        .arg("--specs")
        .arg(&specs_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2));
    if let Some(model) = row.model.as_deref() {
        cmd.arg("--model").arg(model);
    }
    // The client binary the session itself ran under — the same anchor the
    // hook path passes, recovered from the session's harness identity.
    if let Some(bin) = row.harness.as_ref().and_then(|h| h.bin.as_deref()) {
        cmd.arg("--bin").arg(bin);
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
    match cmd.spawn() {
        Ok(child) => tracing::info!(
            session = %row.session_id,
            pid = child.id(),
            forks = ?specs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            "flush-on-close: daemon spawned the end-runner"
        ),
        Err(e) => tracing::warn!(session = %row.session_id, error = %e,
            "flush-on-close: could not spawn the end-runner"),
    }
}

/// The `autofork` CLI next to this daemon binary (the plugin's data dir keeps
/// the pair together), else whatever is on PATH.
fn cli_binary() -> Option<PathBuf> {
    // Tests point this at a stub: the real end-runner would resume a
    // conversation and spend a model call.
    if let Some(over) = std::env::var_os("AUTOFORK_FINAL_RUNNER_BIN") {
        return Some(PathBuf::from(over));
    }
    if let Some(sibling) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.join("autofork")))
    {
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    Some(PathBuf::from("autofork"))
}
