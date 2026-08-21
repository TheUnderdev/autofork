//! Harness liveness sweep: close sessions whose client process is gone.
//!
//! The two pre-existing close signals both depend on the client behaving on
//! the way out — a `SessionEnd` hook that runs to completion, or a parked
//! stop-wait poll whose socket reaches EOF. Neither covers a client that exits
//! before its hook finishes, a session that ends mid-turn with no poll parked,
//! or a poll process orphaned into surviving its client. Those sessions stayed
//! `[open]` until the 12h `session_timeout` reaper, with their flush-on-close
//! forks never run.
//!
//! This sweep asks the OS directly, every `SWEEP_SECS`, and closes with reason
//! `gone` — which flushes, like any other close of a session that was alive
//! moments ago.

use crate::daemon::Daemon;
use std::sync::Arc;
use std::time::Duration;

/// How often to check. Cheap (one `kill(pid, 0)` plus one start-token read per
/// open session), so this can be tight: the point is that a close is noticed in
/// seconds, while resuming the conversation for consolidation forks still makes
/// sense. `AUTOFORK_LIVENESS_SWEEP_SECS` overrides (tests shorten it).
fn sweep_secs() -> u64 {
    std::env::var("AUTOFORK_LIVENESS_SWEEP_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&s| s > 0)
        .unwrap_or(15)
}

pub async fn harness_reaper(daemon: Arc<Daemon>) {
    let period = Duration::from_secs(sweep_secs());
    loop {
        tokio::select! {
            _ = daemon.shutdown.notified() => return,
            _ = tokio::time::sleep(period) => {}
        }
        for (sid, last_activity) in daemon.dead_harness_sessions() {
            // A row left open by a PREVIOUS daemon (a kill, a reboot): its
            // client died at some unknown past moment. Close it, but don't
            // flush — consolidation forks belong to a session that ended just
            // now, not to whatever conversation was open when the machine
            // went down.
            let inherited = last_activity < daemon.started_at;
            tracing::info!(session = %sid, inherited,
                "client process gone, closing session");
            daemon.close_session_with_flush(&sid, "gone", !inherited);
        }
    }
}
