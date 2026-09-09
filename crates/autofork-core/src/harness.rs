//! Harness process identity: which OS process is the client (Claude Code,
//! codex) a session belongs to, and is it still alive?
//!
//! Session liveness used to rest entirely on two client-cooperative signals:
//! the `SessionEnd` hook firing, and a parked stop-wait poll's socket
//! reaching EOF. Both are missable — a client that exits before its
//! `SessionEnd` hook completes (or never runs it), a session that ends
//! mid-turn with no poll parked, a poll process that survives its client as
//! an orphan. The result was a session the daemon believed open for hours:
//! no flush-on-close runs, a stale `[open]` row in `autofork status`.
//!
//! The anchor here is the OS itself: every hook forwards the pid of the
//! process that spawned it (the harness) plus a start-time token that makes
//! the identity immune to pid reuse. The daemon can then ask, at any moment
//! and without the client's cooperation, whether the session's process still
//! exists.

use crate::sys;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The client process a session's hooks come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Harness {
    /// The harness process id.
    pub pid: u32,
    /// An opaque, platform-native process start-time token: two processes
    /// with the same pid but different tokens are different processes (pid
    /// reuse). `None` when the platform wouldn't tell us.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<i64>,
    /// The harness executable, when resolvable — a second identity check on
    /// platforms whose start token isn't wall-clock absolute, and the binary
    /// fork children re-execute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<PathBuf>,
}

impl Harness {
    /// Whether this exact process is still running.
    pub fn alive(&self) -> bool {
        if !pid_exists(self.pid) {
            return false;
        }
        match (self.start, start_token(self.pid)) {
            // Same pid, different birth: the pid was reused.
            (Some(recorded), Some(current)) => recorded == current,
            // No token to compare (unknown platform): existence is all we have.
            _ => true,
        }
    }
}

/// The client process behind a hook.
///
/// Claude Code exports its own pid as `CLAUDE_PID` into everything it spawns,
/// which is the exact answer when it is there. Otherwise walk up from our
/// parent, stepping over shells: the plugin shim `exec`s the binary, but
/// whether the `sh -c` wrapper Claude Code runs it through execs away too is
/// its business, not ours — and anchoring a session on a wrapper shell would
/// be anchoring on the wrong lifetime.
///
/// `None` when nothing trustworthy resolves (already orphaned, or a platform
/// that won't say): no anchor is better than a wrong one, and sessions
/// without one keep the older poll-and-hook behavior.
pub fn client_process() -> Option<Harness> {
    if let Some(h) = std::env::var("CLAUDE_PID")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .and_then(of_pid)
    {
        return Some(h);
    }
    let mut pid = sys::parent_pid();
    for _ in 0..MAX_ANCESTOR_HOPS {
        if pid <= 1 {
            return None;
        }
        let h = of_pid(pid)?;
        if !is_shell(h.bin.as_deref()) {
            return Some(h);
        }
        pid = parent_of(pid)?;
    }
    of_pid(pid)
}

/// How far up the tree the ancestor walk looks for a non-shell process.
const MAX_ANCESTOR_HOPS: usize = 4;

/// Whether an executable is a shell (or a shell-shaped exec wrapper) — a
/// process that stands between us and the client rather than being it.
fn is_shell(bin: Option<&std::path::Path>) -> bool {
    let Some(path) = bin.and_then(|p| p.to_str()) else {
        // Unknown binary: treat it as a real process, not a wrapper. Stopping
        // early anchors on something at least as long-lived as the client.
        return false;
    };
    // Split on both separators: the harness identity may have been recorded
    // on one platform's spelling and read back on another's.
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    sys::is_shell_name(name)
}

/// A process's parent pid.
pub fn parent_of(pid: u32) -> Option<u32> {
    sys::parent_of(pid)
}

/// The identity of a pid we already know (the codex waiter's `--codex-pid`).
/// `None` when the process is already gone.
pub fn of_pid(pid: u32) -> Option<Harness> {
    if pid <= 1 || !pid_exists(pid) {
        return None;
    }
    Some(Harness {
        pid,
        start: start_token(pid),
        bin: exe_path(pid),
    })
}

/// Whether a pid names a live process (a process we may not own counts).
pub fn pid_exists(pid: u32) -> bool {
    sys::pid_exists(pid)
}

/// The executable path of a running process, when the platform exposes it.
pub fn exe_path(pid: u32) -> Option<PathBuf> {
    sys::exe_path(pid)
}

/// A process's start time as an opaque token — see [`sys::start_token`].
pub fn start_token(pid: u32) -> Option<i64> {
    sys::start_token(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_own_process_is_alive_and_stably_tokened() {
        let me = std::process::id();
        assert!(pid_exists(me));
        let a = start_token(me);
        let b = start_token(me);
        assert_eq!(a, b, "the start token must be stable for one process");
        let h = Harness {
            pid: me,
            start: a,
            bin: exe_path(me),
        };
        assert!(h.alive());
    }

    #[test]
    fn a_wrong_start_token_means_the_pid_was_reused() {
        let me = std::process::id();
        // Only meaningful where the platform gives us a token at all.
        if start_token(me).is_none() {
            return;
        }
        let h = Harness {
            pid: me,
            start: Some(-1),
            bin: None,
        };
        assert!(!h.alive());
    }

    /// A child that exits immediately, on every platform.
    fn short_lived_child() -> std::process::Child {
        #[cfg(windows)]
        {
            std::process::Command::new("cmd")
                .args(["/C", "exit", "0"])
                .spawn()
                .unwrap()
        }
        #[cfg(not(windows))]
        {
            std::process::Command::new("true").spawn().unwrap()
        }
    }

    #[test]
    fn a_dead_pid_is_not_alive() {
        // Reap a real child so its pid is certainly gone. Its start token is
        // recorded while it lives: Windows reuses pids eagerly, and the token
        // is what tells the reaped child from a newcomer wearing its pid.
        let mut child = short_lived_child();
        let pid = child.id();
        let start = start_token(pid);
        child.wait().unwrap();
        let h = Harness {
            pid,
            start,
            bin: None,
        };
        assert!(!h.alive());
    }

    #[test]
    fn the_client_anchor_resolves_and_is_alive() {
        // The test binary's ancestry (cargo, a shell, the terminal) always
        // holds something to anchor on.
        let h = client_process().expect("a test process always has an ancestor");
        assert!(h.alive());
    }

    #[test]
    fn claude_pid_wins_when_live_and_is_ignored_when_dead() {
        // One test, not two: it mutates process-wide env, and the test runner
        // is threaded.
        let me = std::process::id();
        std::env::set_var("CLAUDE_PID", me.to_string());
        let h = client_process().unwrap();
        assert_eq!(h.pid, me, "an exported live CLAUDE_PID is the anchor");

        let mut child = short_lived_child();
        let dead = child.id();
        child.wait().unwrap();
        std::env::set_var("CLAUDE_PID", dead.to_string());
        let h = client_process();
        std::env::remove_var("CLAUDE_PID");
        let h = h.expect("the ancestor walk still finds something");
        // (A reused pid would make this a live newcomer; the token check is
        // what `alive()` tests, and `of_pid` never resurrects a dead pid.)
        assert!(
            h.pid != dead || pid_exists(dead),
            "a dead CLAUDE_PID must not be trusted"
        );
        assert!(h.alive());
    }

    #[test]
    fn the_walk_steps_over_shells() {
        assert!(is_shell(Some(std::path::Path::new("/bin/sh"))));
        assert!(is_shell(Some(std::path::Path::new(
            "/opt/homebrew/bin/zsh"
        ))));
        assert!(is_shell(Some(std::path::Path::new(
            r"C:\Program Files\Git\bin\bash.exe"
        ))));
        assert!(!is_shell(Some(std::path::Path::new(
            "/Users/x/.local/bin/claude"
        ))));
        // An unresolvable binary is treated as a real process, not a wrapper.
        assert!(!is_shell(None));
    }

    #[test]
    fn parent_of_walks_one_step_up() {
        let me = std::process::id();
        assert_eq!(
            parent_of(me),
            Some(sys::parent_pid()),
            "parent_of must agree with the platform parent id"
        );
    }
}
