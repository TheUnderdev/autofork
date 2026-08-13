//! Session reaper: periodically close sessions idle past the session timeout,
//! so a crashed session (no SessionEnd) doesn't linger open forever. It fires
//! no forks — v0.5 wakes only happen through a live parked Stop hook.

use crate::daemon::{now, Daemon};
use std::sync::Arc;
use std::time::Duration;

pub async fn session_reaper(daemon: Arc<Daemon>) {
    loop {
        tokio::select! {
            _ = daemon.shutdown.notified() => return,
            _ = tokio::time::sleep(Duration::from_secs(300)) => {}
        }
        let cutoffs: Vec<String> = {
            let store = daemon.store.lock().unwrap();
            let t = now();
            store
                .list_open_sessions()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|s| {
                    let timeout = daemon.cfg_for(Some(&s.project_root)).session_timeout_secs;
                    let idle = (t - s.last_activity).max(0) as u64;
                    (timeout > 0 && idle >= timeout).then_some(s.session_id)
                })
                .collect()
        };
        for sid in cutoffs {
            tracing::info!(session = %sid, "closing timed-out session");
            daemon.close_session_firing_hooks(&sid, "timeout");
        }
    }
}
