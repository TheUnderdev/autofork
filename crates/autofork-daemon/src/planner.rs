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
use autofork_core::schedule::{effective_priorities, resolve_deps, Selected};
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
    /// Declared ordering weight (`priority:`, z-index-like; default 0).
    pub priority: i64,
    pub tags: Vec<String>,
    /// `chain: true`: runs may request another via the continue sentinel.
    pub chain: bool,
    /// `gate: true`: holds the session's other idle forks while unsettled.
    pub gate: bool,
    /// Raw `model:` frontmatter (client-scoped); resolved at wake build.
    pub model: autofork_core::frontmatter::ClientScoped,
    /// Raw `mode:` frontmatter (client-scoped); resolved at wake build.
    pub mode: autofork_core::frontmatter::ClientScoped,
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
    fn priority(&self) -> i64 {
        self.priority
    }
}

/// Refresh discovery for a session's cwd and queue every visible fork onto
/// the session's roster.
pub fn refresh_roster(daemon: &Arc<Daemon>, session_id: &str, cwd: &Path) {
    let (entries, _) = autofork_core::discovery::discover_forks(
        cwd,
        Some(&daemon.user_forks_root()),
        daemon.claude_dir().as_deref(),
    );
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
        // Daemon-side overlap gate: `overlap: false` with a live run of this
        // fork still in flight (a spawn the registry hasn't seen go terminal)
        // means skip. The client-side gates (the wake payload's skip line, the
        // opencode plugin's in-memory live map) die with their instance and
        // multiply with duplicated ones; the spawn registry is the one copy
        // that survives both. Aged-out spawns (terminal status lost to a
        // crash) stop blocking after `overlap_spawn_max_age_secs`.
        if !parsed.def.overlap {
            let live = {
                let store = daemon.store.lock().unwrap();
                store
                    .live_spawn_newer_than(
                        &session.session_id,
                        &entry.fork_name,
                        t - overlap_spawn_max_age_secs(),
                    )
                    .unwrap_or(false)
            };
            if live {
                tracing::info!(session = %session.session_id, fork = %entry.fork_name,
                    "a run is still in flight and overlap is false, skipping");
                continue;
            }
        }
        // Runaway breaker: a hard wall-clock cap on wakes of one fork, immune
        // to the counters a runaway can reset (pause epochs, baselines,
        // per-pause chain limits). Guards against self-sustaining loops — a
        // duplicated client event stream misreporting autofork's own turns as
        // user activity re-arms every idle fork each cycle, and a chain fork
        // then pumps the session forever with zero user input. `every:`
        // triggers are exempt: their interval is an explicit contract.
        if cfg.runaway_limit > 0 && !matches!(trigger, ForkRunOn::Every { .. }) {
            let recent = {
                let store = daemon.store.lock().unwrap();
                store
                    .count_runs_since(
                        &session.session_id,
                        &entry.fork_name,
                        t - runaway_window_secs(),
                    )
                    .unwrap_or(0)
            };
            if recent >= cfg.runaway_limit as i64 {
                tracing::warn!(session = %session.session_id, fork = %entry.fork_name,
                    runs = recent, limit = cfg.runaway_limit,
                    "runaway breaker: fork hit its hourly run cap, skipping \
                     (raise `runaway_limit` in config if this rate is intended)");
                continue;
            }
        }
        selected.push(SelectedFork {
            name: entry.fork_name.clone(),
            path: entry.fork_path.clone(),
            trigger: label,
            overlap: parsed.def.overlap,
            after: parsed.def.after.clone(),
            priority: parsed.def.priority,
            tags: parsed.def.tags.clone(),
            chain: parsed.def.chain,
            gate: parsed.def.gate,
            model: parsed.def.model.clone(),
            mode: parsed.def.mode.clone(),
            latch_key,
        });
    }
    apply_gate_filter(daemon, session, &mut selected);
    selected
}

/// On codex sessions, `idle: 0s` chain forks belong to the Stop hook's
/// `PeekDue` (the goal fast path — synchronous block-and-inject at the
/// pause's first Stop). The waiter's parked poll fires at the same instant
/// and must not race it for them: whichever selects first consumes the
/// once-per-pause latch, and a poll win would strand the goal loop on the
/// slow queue path. `autofork doctor` flags installs whose hooks predate the
/// Stop hook (3 of 4), where this reservation would otherwise silence such
/// forks.
pub fn reserve_fast_path(session: &SessionRow, selected: &mut Vec<SelectedFork>) {
    if session.client.as_deref() == Some("codex") {
        selected.retain(|s| !(s.chain && s.trigger == "idle:0"));
    }
}

/// A trigger label produced by an idle deadline (`idle` / `idle:<secs>`) —
/// the only trigger family a gate holds back.
fn is_idle_trigger(label: &str) -> bool {
    label == "idle" || label.starts_with("idle:")
}

/// How long an active gate survives without an observed spawn before the
/// belt lifts it (a wake the model fumbled must not silence every other fork
/// for the rest of the pause). Overridable via `AUTOFORK_GATE_GRACE_SECS`
/// (tests shorten it).
pub(crate) fn gate_grace_secs() -> i64 {
    std::env::var("AUTOFORK_GATE_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(180)
}

/// How old a live (non-terminal) spawn may be and still block an `overlap:
/// false` fork from re-firing. Runs finish in minutes; a spawn this stale
/// almost certainly lost its terminal status to a crash. Overridable via
/// `AUTOFORK_OVERLAP_SPAWN_MAX_AGE_SECS` (tests shorten it).
fn overlap_spawn_max_age_secs() -> i64 {
    std::env::var("AUTOFORK_OVERLAP_SPAWN_MAX_AGE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30 * 60)
}

/// The rolling window the runaway breaker counts runs against. Overridable
/// via `AUTOFORK_RUNAWAY_WINDOW_SECS` (tests shorten it).
pub(crate) fn runaway_window_secs() -> i64 {
    std::env::var("AUTOFORK_RUNAWAY_WINDOW_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600)
}

/// While a `gate: true` fork's run/chain is unsettled, hold every other
/// idle-triggered fork: drop them from the selection *without* stamping their
/// latches or throttles, so they fire intact once the gate clears. Two
/// prongs: a gate fork in the current selection always holds its batch-mates
/// (covers the re-arm gap between chain iterations, when the persisted gate
/// may have lapsed); otherwise the session's persisted `active_gate` holds —
/// unless it is neither live (a spawn in flight) nor recent (wake just
/// issued), in which case the belt lifts it.
fn apply_gate_filter(daemon: &Arc<Daemon>, session: &SessionRow, selected: &mut Vec<SelectedFork>) {
    let selected_gate = selected.iter().any(|s| s.gate);
    let mut holding = selected_gate;
    if !holding {
        // Re-read the gate fresh: the snapshot in `session` predates any
        // chain-end processed while this poll was parked.
        let gate = {
            let store = daemon.store.lock().unwrap();
            store
                .get_session(&session.session_id)
                .ok()
                .flatten()
                .and_then(|s| s.active_gate)
        };
        if let Some(g) = gate {
            let store = daemon.store.lock().unwrap();
            let live = store
                .live_spawn_exists(&session.session_id, &g)
                .unwrap_or(false);
            let recent = store
                .last_issued_at(&session.session_id, &g)
                .ok()
                .flatten()
                .is_some_and(|at| now() - at < gate_grace_secs());
            if live || recent {
                holding = true;
            } else {
                tracing::warn!(session = %session.session_id, gate = %g,
                    "active gate has no live spawn and no recent wake — lifting it");
                let _ = store.clear_active_gate(&session.session_id);
            }
        }
    }
    if holding {
        let before = selected.len();
        selected.retain(|s| s.gate || !is_idle_trigger(&s.trigger));
        if selected.len() < before {
            tracing::info!(session = %session.session_id, held = before - selected.len(),
                "gate active: holding other idle forks");
        }
    }
}

fn parse_fork(name: &str, content: &str) -> ForkParse {
    autofork_core::frontmatter::parse_fork_file(name, content)
}

/// Resolve a client-scoped frontmatter value for a session's client, falling
/// back to the config table for that client. Sessions with no client tag are
/// Claude Code.
fn resolve_scoped(
    scoped: &autofork_core::frontmatter::ClientScoped,
    client: Option<&str>,
    cfg_table: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    let client = client.unwrap_or("claude-code");
    scoped
        .resolve(client)
        .map(str::to_string)
        .or_else(|| cfg_table.get(client).cloned())
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

    // For resolving client-scoped model/mode into the structured specs.
    let cfg = daemon.cfg_for(Some(&session.project_root));

    // Resolve `after` dependencies within the selected set, then layer the
    // priority waves on top: each fork's effective priority is its declared
    // one lifted to its predecessors' (`after` wins), and a fork gates on
    // every batch-mate of strictly lower effective priority (order-only —
    // those extra predecessors' reports are not piped).
    let deps = resolve_deps(&selected);
    let eff = effective_priorities(&selected, &deps);
    let min_eff = eff.iter().min().copied().unwrap_or(0);

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
            let report_preds: Vec<String> =
                deps[i].iter().map(|&j| selected[j].name.clone()).collect();
            // The full gate: `after` deps plus every lower-wave batch-mate.
            let mut gate = report_preds.clone();
            if eff[i] > min_eff {
                for (j, other) in selected.iter().enumerate() {
                    if j != i && eff[j] < eff[i] && !gate.contains(&other.name) {
                        gate.push(other.name.clone());
                    }
                }
            }
            if sel.gate {
                let _ = store.set_active_gate(&session.session_id, &sel.name);
            }
            if gate.is_empty() {
                roots.push(DueFork {
                    name: sel.name.clone(),
                    path: sel.path.to_string_lossy().into_owned(),
                    trigger: sel.trigger.clone(),
                    overlap: sel.overlap,
                    after: Vec::new(),
                    skill: autofork_core::discovery::skill_sibling(&sel.path)
                        .map(|p| p.to_string_lossy().into_owned()),
                    chain: sel.chain,
                    model: resolve_scoped(&sel.model, session.client.as_deref(), &cfg.fork_models),
                    mode: resolve_scoped(&sel.mode, session.client.as_deref(), &cfg.fork_modes),
                });
            } else {
                let _ = store.insert_pending_dep(
                    &session.session_id,
                    &sel.name,
                    &sel.path,
                    &sel.trigger,
                    sel.overlap,
                    &gate,
                    &report_preds,
                    t,
                );
                held.push(HeldFork {
                    name: sel.name.clone(),
                    after: gate,
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
        // An active gate holds releases too (a dependent spawning mid-chain
        // would defeat the gate); the gated deps release on the first poll
        // after the gate clears. The gate fork itself is never held here.
        let active_gate = store
            .get_session(&session.session_id)
            .ok()
            .flatten()
            .and_then(|s| s.active_gate);
        let pending = store.list_pending_deps(&session.session_id).ok()?;
        pending
            .into_iter()
            .filter(|dep| active_gate.as_deref().is_none_or(|g| g == dep.fork_name))
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
    // Pending rows carry no chain/gate/model info; re-read each definition so
    // a released chain fork still learns the sentinel, a released gate fork
    // still claims the gate, and model/mode overrides still resolve.
    let def_of =
        |dep: &autofork_core::store::PendingDep| -> Option<autofork_core::frontmatter::ForkDef> {
            std::fs::read_to_string(&dep.fork_path).ok().and_then(|c| {
                match parse_fork(&dep.fork_name, &c) {
                    ForkParse::Fork(p) => Some(p.def),
                    _ => None,
                }
            })
        };
    let cfg = daemon.cfg_for(Some(&session.project_root));
    let due: Vec<DueFork> = released
        .iter()
        .map(|dep| {
            let def = def_of(dep);
            DueFork {
                name: dep.fork_name.clone(),
                path: dep.fork_path.to_string_lossy().into_owned(),
                trigger: dep.trigger_label.clone(),
                overlap: dep.overlap,
                // Only the true `after` deps are quoted/report-piped; priority
                // gates were order-only.
                after: dep.report_preds.clone(),
                skill: autofork_core::discovery::skill_sibling(&dep.fork_path)
                    .map(|p| p.to_string_lossy().into_owned()),
                chain: def.as_ref().map(|d| d.chain).unwrap_or(false),
                model: def.as_ref().and_then(|d| {
                    resolve_scoped(&d.model, session.client.as_deref(), &cfg.fork_models)
                }),
                mode: def.as_ref().and_then(|d| {
                    resolve_scoped(&d.mode, session.client.as_deref(), &cfg.fork_modes)
                }),
            }
        })
        .collect();
    {
        let store = daemon.store.lock().unwrap();
        for dep in &released {
            let _ = store.delete_pending_dep(&session.session_id, &dep.fork_name);
            if def_of(dep).map(|d| d.gate).unwrap_or(false) {
                let _ = store.set_active_gate(&session.session_id, &dep.fork_name);
            }
        }
    }
    daemon.note_wake_issued(&session.session_id);
    let conv = conversation_id(session);
    let root_str = session.project_root.to_string_lossy();
    let payload = build_release_payload(&session.session_id, &conv, &root_str, &due);
    let forks = build_wake_forks(&session.session_id, &conv, &root_str, &due);
    Some((payload, forks))
}
