//! The selection pipeline: turn a set of fork moments for one session into a
//! wake payload (or nothing).
//!
//! Pipeline (order matters): refresh discovery → queue roster → live re-read
//! each rostered fork → tag filter (per-session enable/disable, falling back
//! to config) → match moments → per-fork throttle → per-tag throttle →
//! once-per-session latch (context triggers) → dependency resolution → build
//! the wake payload. When a wake is issued, per-fork and per-tag throttles and
//! latches are stamped for everything selected — dependents included, since
//! their wake is merely deferred, not reconsidered.
//!
//! `after` dependents are not spawned by the wake: they are held in the store
//! ([`Store::insert_pending_dep`]) until the transcript watcher observes their
//! predecessors' completion notifications, at which point [`release_due`]
//! answers the next parked Stop poll with a release payload.

use crate::daemon::{now, Daemon};
use autofork_core::config::Config;
use autofork_core::frontmatter::{ForkParse, ForkRunOn};
use autofork_core::moments::{match_moments, ForkMoment};
use autofork_core::protocol::WakeFork;
use autofork_core::schedule::{resolve_deps, Selected};
use autofork_core::store::SessionRow;
use autofork_core::tags::tags_allowed;
use autofork_core::wake::{
    build_release_payload, build_wake_forks, build_wake_payload, DueFork, HeldFork,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A fork selected to fire at the current moment.
#[derive(Clone)]
pub struct SelectedFork {
    pub name: String,
    pub path: PathBuf,
    pub trigger: String,
    pub overlap: bool,
    pub after: Vec<String>,
    pub tags: Vec<String>,
    /// The latch this fork consumes at wake-issuance, if any: `context_*`
    /// triggers latch once per session (key = the trigger label); `idle`
    /// triggers latch once per pause (key = `idle-pause:<epoch>`). `None` means
    /// no latch (nothing here today, kept for clarity).
    pub latch_key: Option<String>,
}

/// The latch key a matched trigger consumes: context thresholds latch
/// once-per-session; idle triggers latch once-per-pause (so a fork fires at
/// most once per idle pause, never re-firing on a wake-turn's own Stop).
fn latch_key_for(trigger: &ForkRunOn, pause_epoch: i64) -> Option<String> {
    match trigger {
        ForkRunOn::Idle { .. } => Some(format!("idle-pause:{pause_epoch}")),
        ForkRunOn::ContextTokens(_) | ForkRunOn::ContextUsedPct(_) | ForkRunOn::ContextLeft(_) => {
            Some(trigger.label())
        }
        // `every` needs no latch: its own interval (measured from the run
        // stamped at issuance) is the re-fire guard.
        ForkRunOn::Every { .. } => None,
        _ => None,
    }
}

impl Selected for SelectedFork {
    fn name(&self) -> &str {
        &self.name
    }
    fn after(&self) -> Vec<&str> {
        self.after.iter().map(|a| a.as_str()).collect()
    }
}

/// Refresh discovery for a session's cwd and queue every visible fork onto
/// the session's roster.
pub fn refresh_roster(daemon: &Arc<Daemon>, session_id: &str, cwd: &Path) {
    let (entries, _) =
        autofork_core::discovery::discover_forks(cwd, Some(&daemon.user_forks_root()));
    let store = daemon.store.lock().unwrap();
    let t = now();
    for entry in entries {
        if let Ok(true) = store.queue_fork(session_id, &entry.name, &entry.path, t) {
            tracing::info!(fork = %entry.name, session = session_id, "fork rostered");
        }
    }
}

/// Run the selection pipeline for `moments` and return the forks that should
/// fire (empty = nothing due). Read-only / side-effect-free: no latches or
/// throttles are stamped here (that happens at wake-issuance in [`build_wake`],
/// so a wait cancelled during the debounce window stamps nothing).
pub fn select_forks(
    daemon: &Arc<Daemon>,
    session: &SessionRow,
    cfg: &Config,
    moments: &[ForkMoment],
) -> Vec<SelectedFork> {
    refresh_roster(daemon, &session.session_id, &session.cwd);

    let roster = {
        let store = daemon.store.lock().unwrap();
        store.roster(&session.session_id).unwrap_or_default()
    };
    let effective_enable = session
        .enable_tags
        .as_deref()
        .or(cfg.enable_tags.as_deref());
    let effective_disable = session
        .disable_tags
        .as_deref()
        .or(cfg.disable_tags.as_deref());

    let mut selected: Vec<SelectedFork> = Vec::new();
    let t = now();
    for entry in roster {
        let Ok(content) = std::fs::read_to_string(&entry.fork_path) else {
            continue;
        };
        let ForkParse::Fork(parsed) = parse_fork(&entry.fork_name, &content) else {
            continue;
        };
        if !tags_allowed(&parsed.def.tags, effective_enable, effective_disable) {
            continue;
        }
        let Some(trigger) = match_moments(
            &parsed.def,
            moments,
            cfg.default_idle_deadline_secs,
            entry.ran_at,
            session.created_at,
        ) else {
            continue;
        };
        // Per-fork throttle.
        if let (Some(throttle), Some(ran_at)) = (parsed.def.throttle_secs, entry.ran_at) {
            if (t - ran_at).max(0) < throttle as i64 {
                tracing::debug!(fork = %entry.fork_name, "throttled, skipping");
                continue;
            }
        }
        // Per-tag shared throttle.
        if !parsed.def.tags.is_empty() && !cfg.tag_throttles.is_empty() {
            let store = daemon.store.lock().unwrap();
            let mut hit = None;
            for tag in &parsed.def.tags {
                let Some(&window) = cfg.tag_throttles.get(tag) else {
                    continue;
                };
                if let Ok(Some(last)) =
                    store.last_run_for_tags(&session.project_root, std::slice::from_ref(tag))
                {
                    if (t - last).max(0) < window as i64 {
                        hit = Some(tag.clone());
                        break;
                    }
                }
            }
            drop(store);
            if let Some(tag) = hit {
                tracing::debug!(fork = %entry.fork_name, %tag, "tag-throttled, skipping");
                continue;
            }
        }
        // Latch check (read-only — the latch is consumed at issuance): skip a
        // fork already latched for its trigger's scope. Idle → once per pause;
        // context_* → once per session.
        let label = trigger.label();
        let latch_key = latch_key_for(&trigger, session.pause_epoch);
        if let Some(key) = &latch_key {
            let latched = {
                let store = daemon.store.lock().unwrap();
                store
                    .is_latched(&session.session_id, &entry.fork_name, key)
                    .unwrap_or(false)
            };
            if latched {
                continue;
            }
        }
        selected.push(SelectedFork {
            name: entry.fork_name.clone(),
            path: entry.fork_path.clone(),
            trigger: label,
            overlap: parsed.def.overlap,
            after: parsed.def.after.clone(),
            tags: parsed.def.tags.clone(),
            latch_key,
        });
    }
    selected
}

fn parse_fork(name: &str, content: &str) -> ForkParse {
    autofork_core::frontmatter::parse_fork_file(name, content)
}

/// The conversation id survives resume: a resumed leg gets a fresh session
/// id but appends to the original leg's transcript, so the transcript stem
/// is the stable identity. No transcript known → the session id.
fn conversation_id(session: &SessionRow) -> String {
    session
        .transcript_path
        .as_deref()
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| session.session_id.clone())
}

/// Given the forks selected to fire, stamp their throttles (per-fork and
/// per-tag) and build the wake the session should act on: the model-facing
/// payload text plus the structured due-fork specs (for programmatic
/// clients). Roots go into the payload as spawn-now blocks; `after`
/// dependents are held in the store until their predecessors' completions are
/// observed. Returns `None` when `selected` is empty.
pub fn build_wake(
    daemon: &Arc<Daemon>,
    session: &SessionRow,
    selected: Vec<SelectedFork>,
) -> Option<(String, Vec<WakeFork>)> {
    if selected.is_empty() {
        return None;
    }
    tracing::info!(
        session = %session.session_id,
        forks = ?selected.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        "issuing wake"
    );

    // Resolve `after` dependencies within the selected set.
    let deps = resolve_deps(&selected);

    // Stamp throttles and latches at wake-issuance — dependents included:
    // their spawn is deferred, not reconsidered, so they must not re-enter
    // selection while held. Accepted limitation: the daemon observes spawns
    // only after the fact (transcript), never their absence, so a context_*
    // trigger that wakes a session lacking fork support still consumes its
    // once-per-session latch (and throttles still stamp) even though no fork
    // ran. The visible one-liner in the wake payload tells the user why
    // nothing happened.
    let t = now();
    let mut roots: Vec<DueFork> = Vec::new();
    let mut held: Vec<HeldFork> = Vec::new();
    {
        let store = daemon.store.lock().unwrap();
        for (i, sel) in selected.iter().enumerate() {
            let _ = store.touch_fork_ran(&session.session_id, &sel.name, t);
            let tags_joined = (!sel.tags.is_empty()).then(|| sel.tags.join(","));
            let _ = store.record_issued_run(
                &session.session_id,
                &sel.name,
                &sel.trigger,
                tags_joined.as_deref(),
                t,
            );
            if let Some(key) = &sel.latch_key {
                let _ = store.try_latch_fire(&session.session_id, &sel.name, key, t);
            }
            let preds: Vec<String> = deps[i].iter().map(|&j| selected[j].name.clone()).collect();
            if preds.is_empty() {
                roots.push(DueFork {
                    name: sel.name.clone(),
                    path: sel.path.to_string_lossy().into_owned(),
                    trigger: sel.trigger.clone(),
                    overlap: sel.overlap,
                    after: Vec::new(),
                });
            } else {
                let _ = store.insert_pending_dep(
                    &session.session_id,
                    &sel.name,
                    &sel.path,
                    &sel.trigger,
                    sel.overlap,
                    &preds,
                    t,
                );
                held.push(HeldFork {
                    name: sel.name.clone(),
                    after: preds,
                });
            }
        }
    }
    // Record the wake for the post-wake grace window (belt for ambiguous
    // continuation PromptSubmits that arrive without prunable prompt text).
    daemon.note_wake_issued(&session.session_id);

    let conv = conversation_id(session);
    let root_str = session.project_root.to_string_lossy();
    let payload = build_wake_payload(&session.session_id, &conv, &root_str, &roots, &held);
    let forks = build_wake_forks(&session.session_id, &conv, &root_str, &roots);
    Some((payload, forks))
}

/// Release any held dependents whose predecessors have all reached a terminal
/// status since the dependent was held. Returns the release wake payload, or
/// `None` when nothing is releasable. Latches and throttles were stamped when
/// the dependents were first selected, so nothing is re-stamped here.
pub fn release_due(daemon: &Arc<Daemon>, session: &SessionRow) -> Option<(String, Vec<WakeFork>)> {
    let released: Vec<autofork_core::store::PendingDep> = {
        let store = daemon.store.lock().unwrap();
        let pending = store.list_pending_deps(&session.session_id).ok()?;
        pending
            .into_iter()
            .filter(|dep| {
                dep.preds.iter().all(|pred| {
                    store
                        .fork_completed_since(&session.session_id, pred, dep.created_at)
                        .unwrap_or(false)
                })
            })
            .collect()
    };
    if released.is_empty() {
        return None;
    }
    tracing::info!(
        session = %session.session_id,
        forks = ?released.iter().map(|d| d.fork_name.as_str()).collect::<Vec<_>>(),
        "releasing held dependents"
    );
    let due: Vec<DueFork> = released
        .iter()
        .map(|dep| DueFork {
            name: dep.fork_name.clone(),
            path: dep.fork_path.to_string_lossy().into_owned(),
            trigger: dep.trigger_label.clone(),
            overlap: dep.overlap,
            after: dep.preds.clone(),
        })
        .collect();
    {
        let store = daemon.store.lock().unwrap();
        for dep in &released {
            let _ = store.delete_pending_dep(&session.session_id, &dep.fork_name);
        }
    }
    daemon.note_wake_issued(&session.session_id);
    let conv = conversation_id(session);
    let root_str = session.project_root.to_string_lossy();
    let payload = build_release_payload(&session.session_id, &conv, &root_str, &due);
    let forks = build_wake_forks(&session.session_id, &conv, &root_str, &due);
    Some((payload, forks))
}
