//! The watcher behind `changed: <glob>` triggers.
//!
//! autofork watches by **polling**: every `watch_interval` the daemon stats
//! the files each watched pattern matches and diffs that against the previous
//! sweep. It does not subscribe to filesystem events, and that is a decision,
//! not a shortcut — a poll needs no new dependency, hits no per-platform watch
//! limit, opens no descriptor per directory, and costs exactly what the
//! interval says it costs. What it buys instead of instant delivery is a
//! bounded, predictable sweep; a change is noticed within one interval.
//!
//! Three properties matter more than latency here:
//!
//! - **The first sweep of a pattern is a baseline, never a trigger.** A
//!   session that starts and immediately matches five hundred files must not
//!   be told all five hundred just changed.
//! - **A burst is one trigger.** An editor writing a temp file and renaming
//!   it, or a `git pull` rewriting a hundred files, settles for
//!   `watch_debounce` before firing, and fires once with the whole path set.
//! - **Deletions count.** A file that disappears is a change; a feed that
//!   lists a directory needs to know.
//!
//! The registry is rebuilt from the open sessions on every sweep, so editing
//! a fork or hook definition mid-session takes effect on the next sweep with
//! nothing to restart.

use crate::daemon::{now, Daemon, EXTERNAL_DETAIL_CAP};
use autofork_core::glob;
use autofork_core::hooks::HookOn;
use autofork_core::moments::ExternalKind;
use autofork_core::store::SessionRow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// How deep below a pattern's literal prefix a sweep will walk. A `**` can
/// name an arbitrarily deep tree; this is the belt that keeps one careless
/// pattern from turning a sweep into a filesystem crawl.
const MAX_WALK_DEPTH: usize = 24;

/// Directory names never walked into. `.git` is the important one: its
/// internals churn constantly and none of it is a file anyone watches — while
/// a `git pull`, `git checkout` or `git stash` still rewrites the *worktree*,
/// which is what a `changed:` pattern is pointed at. The rest are build and
/// dependency trees, where a watch is almost always an accident.
const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", ".venv", "__pycache__"];

/// One session's interest in one absolutized pattern.
struct Subscriber {
    session: SessionRow,
    /// The pattern exactly as the definition wrote it — the key a fork's
    /// `run_on` / hook's `on` is matched against, and the key a pending
    /// trigger is stored under.
    raw: String,
    /// Whether a *fork* wants this (a pending trigger must be recorded for
    /// the next evaluation) as opposed to only hooks (which fire directly).
    forks: bool,
    /// Whether a *hook* wants this.
    hooks: bool,
}

/// A file's identity for change detection: modification time and size. Not a
/// content hash — a sweep must stay a stat sweep. Two writes inside one
/// filesystem timestamp tick that leave the size unchanged are the known
/// blind spot, and the reason `watch_interval` is not sub-second.
type Snapshot = HashMap<String, (u128, u64)>;

/// The watcher's state across sweeps.
#[derive(Default)]
pub struct Watcher {
    /// Absolutized pattern -> last sweep's snapshot.
    snapshots: HashMap<String, Snapshot>,
    /// Absolutized pattern -> (changed paths, when the burst started).
    pending: HashMap<String, (Vec<String>, i64)>,
    /// Patterns already warned about for exceeding `watch_max_files`, so the
    /// warning is a fact stated once and not a log every two seconds.
    warned: HashSet<String>,
}

/// Run the watch loop for the daemon's lifetime.
pub async fn watch_loop(daemon: Arc<Daemon>) {
    let mut watcher = Watcher::default();
    loop {
        let cfg = daemon.cfg_for(None);
        if cfg.watch_interval_secs == 0 {
            // `changed:` triggers disabled: keep the task alive but idle, so
            // enabling them in config only needs a daemon restart, not a
            // different code path.
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(30)) => continue,
                _ = daemon.shutdown.notified() => return,
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(cfg.watch_interval_secs)) => {}
            _ = daemon.shutdown.notified() => return,
        }
        sweep(&daemon, &mut watcher, &cfg);
    }
}

/// One sweep: rebuild the registry, diff every watched pattern, and dispatch
/// whatever has settled.
pub fn sweep(daemon: &Arc<Daemon>, watcher: &mut Watcher, cfg: &autofork_core::config::Config) {
    let registry = build_registry(daemon);
    // Drop snapshots for patterns nobody watches any more, so a closed
    // session's tree stops being swept and a later re-open re-baselines.
    watcher.snapshots.retain(|k, _| registry.contains_key(k));
    watcher.pending.retain(|k, _| registry.contains_key(k));

    let t = now();
    for pattern in registry.keys() {
        let (snap, truncated) = scan(pattern, cfg.watch_max_files);
        if truncated && watcher.warned.insert(pattern.clone()) {
            tracing::warn!(
                pattern = %pattern, cap = cfg.watch_max_files,
                "watched pattern matches more files than watch_max_files; \
                 watching the first {} and ignoring the rest (narrow the glob, \
                 or raise watch_max_files)", cfg.watch_max_files
            );
        }
        let Some(prev) = watcher.snapshots.get(pattern) else {
            // First sight of this pattern: baseline only.
            tracing::debug!(pattern = %pattern, files = snap.len(), "watch baseline");
            watcher.snapshots.insert(pattern.clone(), snap);
            continue;
        };
        let changed = diff(prev, &snap);
        watcher.snapshots.insert(pattern.clone(), snap);
        if changed.is_empty() {
            continue;
        }
        let entry = watcher
            .pending
            .entry(pattern.clone())
            .or_insert_with(|| (Vec::new(), t));
        for c in changed {
            if !entry.0.contains(&c) {
                entry.0.push(c);
            }
        }
        // A burst that keeps producing changes keeps the debounce open, but
        // only up to twice the window: a directory written to continuously
        // must still fire, or a busy tree would never deliver anything.
        if t - entry.1 > 2 * cfg.watch_debounce_secs as i64 {
            entry.1 = t - cfg.watch_debounce_secs as i64;
        }
    }

    let settled: Vec<String> = watcher
        .pending
        .iter()
        .filter(|(_, (_, started))| t - *started >= cfg.watch_debounce_secs as i64)
        .map(|(k, _)| k.clone())
        .collect();
    for pattern in settled {
        let Some((paths, _)) = watcher.pending.remove(&pattern) else {
            continue;
        };
        let Some(subs) = registry.get(&pattern) else {
            continue;
        };
        dispatch(daemon, subs, &paths, t);
    }
}

/// Hand a settled change to every session watching it.
fn dispatch(daemon: &Arc<Daemon>, subs: &[Subscriber], paths: &[String], t: i64) {
    let detail = paths.join("\n");
    for sub in subs {
        tracing::info!(
            session = %sub.session.session_id, pattern = %sub.raw, files = paths.len(),
            "watched paths changed"
        );
        if sub.forks {
            let store = daemon.store.lock().unwrap();
            let _ = store.record_pending_trigger(
                &sub.session.session_id,
                ExternalKind::Changed.label(),
                &sub.raw,
                &detail,
                EXTERNAL_DETAIL_CAP,
                t,
            );
        }
        if sub.hooks {
            crate::hooks::fire_external(
                daemon,
                &crate::hooks::HookCtx::from_row(&sub.session),
                ExternalKind::Changed,
                &sub.raw,
                &detail,
            );
        }
        // Even a hooks-only subscriber is nudged: a feed's block lands in the
        // spool moments later and the parked poll is what carries it on
        // clients with no silent lane.
        daemon.nudge(&sub.session.session_id);
    }
}

/// Every (absolutized pattern -> interested sessions) pair right now.
/// Rebuilt per sweep from the open sessions: a definition edited mid-session
/// takes effect on the next sweep, with nothing to restart and no cache to
/// invalidate.
fn build_registry(daemon: &Arc<Daemon>) -> HashMap<String, Vec<Subscriber>> {
    let sessions = {
        let store = daemon.store.lock().unwrap();
        store.list_open_sessions().unwrap_or_default()
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut out: HashMap<String, Vec<Subscriber>> = HashMap::new();
    for session in sessions {
        // Patterns are written relative to the project the definition serves,
        // which is the session's project root — the same reading a path in a
        // fork body would get.
        let base = &session.project_root;
        let mut wanted: HashMap<String, (bool, bool)> = HashMap::new();
        let (forks, _) = autofork_core::discovery::discover_forks(
            &session.cwd,
            Some(&daemon.user_forks_root()),
            daemon.claude_dir().as_deref(),
            daemon.agents_dir().as_deref(),
        );
        for entry in &forks {
            for trigger in &entry.parsed.def.run_on {
                if let autofork_core::frontmatter::ForkRunOn::Changed { pattern } = trigger {
                    wanted.entry(pattern.clone()).or_default().0 = true;
                }
            }
        }
        let (hooks, _) =
            autofork_core::hooks::discover_hooks(&session.cwd, Some(&daemon.user_hooks_root()));
        for entry in &hooks {
            if entry.parsed.def.command.is_empty() {
                continue;
            }
            for on in &entry.parsed.def.on {
                if let HookOn::Changed { pattern } = on {
                    wanted.entry(pattern.clone()).or_default().1 = true;
                }
            }
        }
        for (raw, (forks, hooks)) in wanted {
            let abs = glob::absolutize(&raw, base, home.as_deref());
            out.entry(abs).or_default().push(Subscriber {
                session: session.clone(),
                raw,
                forks,
                hooks,
            });
        }
    }
    out
}

/// Stat every file matching `pattern`, up to `cap`. Returns the snapshot and
/// whether the cap truncated it.
fn scan(pattern: &str, cap: usize) -> (Snapshot, bool) {
    let mut snap = Snapshot::new();
    let root = glob::literal_prefix(pattern);
    if !glob::has_wildcard(pattern) {
        // A literal path: one stat, no walk.
        if let Some(id) = file_id(&root) {
            snap.insert(root.to_string_lossy().into_owned(), id);
        }
        return (snap, false);
    }
    let mut truncated = false;
    walk(&root, 0, pattern, cap, &mut snap, &mut truncated);
    (snap, truncated)
}

fn walk(
    dir: &Path,
    depth: usize,
    pattern: &str,
    cap: usize,
    snap: &mut Snapshot,
    truncated: &mut bool,
) {
    if depth > MAX_WALK_DEPTH || snap.len() >= cap {
        *truncated = *truncated || snap.len() >= cap;
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for item in read.filter_map(|e| e.ok()) {
        if snap.len() >= cap {
            *truncated = true;
            return;
        }
        let path = item.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let is_dir = item.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            if SKIP_DIRS.contains(&name) {
                continue;
            }
            walk(&path, depth + 1, pattern, cap, snap, truncated);
            continue;
        }
        let s = path.to_string_lossy();
        if glob::matches(pattern, &s) {
            if let Some(id) = file_id(&path) {
                snap.insert(s.into_owned(), id);
            }
        }
    }
}

/// `(mtime_nanos, size)` for a path, or `None` if it cannot be stat'ed.
fn file_id(path: &Path) -> Option<(u128, u64)> {
    let md = std::fs::metadata(path).ok()?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some((mtime, md.len()))
}

/// Paths that appeared, changed, or vanished between two snapshots.
fn diff(prev: &Snapshot, next: &Snapshot) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (path, id) in next {
        if prev.get(path) != Some(id) {
            out.push(path.clone());
        }
    }
    for path in prev.keys() {
        if !next.contains_key(path) {
            out.push(path.clone());
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn diff_reports_adds_edits_and_deletes() {
        let mut a = Snapshot::new();
        a.insert("/x/1.md".into(), (1, 10));
        a.insert("/x/2.md".into(), (1, 10));
        let mut b = Snapshot::new();
        b.insert("/x/2.md".into(), (2, 11)); // edited
        b.insert("/x/3.md".into(), (1, 5)); // added
        let d = diff(&a, &b);
        assert_eq!(d, vec!["/x/1.md", "/x/2.md", "/x/3.md"]);
    }

    #[test]
    fn scan_matches_the_glob_and_skips_noise() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("h/2026/08")).unwrap();
        fs::create_dir_all(root.join(".git/objects")).unwrap();
        fs::write(root.join("h/2026/08/a.md"), "a").unwrap();
        fs::write(root.join("h/2026/08/b.txt"), "b").unwrap();
        fs::write(root.join(".git/objects/c.md"), "c").unwrap();
        let pattern = format!("{}/h/**/*.md", root.display());
        let (snap, truncated) = scan(&pattern, 1000);
        assert!(!truncated);
        let keys: Vec<&String> = snap.keys().collect();
        assert_eq!(keys.len(), 1, "{keys:?}");
        assert!(keys[0].ends_with("h/2026/08/a.md"));
    }

    #[test]
    fn scan_honors_the_file_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for i in 0..10 {
            fs::write(root.join(format!("f{i}.md")), "x").unwrap();
        }
        let pattern = format!("{}/*.md", root.display());
        let (snap, truncated) = scan(&pattern, 4);
        assert!(truncated);
        assert!(snap.len() <= 4);
    }

    #[test]
    fn literal_pattern_needs_no_walk() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let f = root.join("one.md");
        fs::write(&f, "x").unwrap();
        let (snap, _) = scan(&f.to_string_lossy(), 1000);
        assert_eq!(snap.len(), 1);
        // A literal pattern that does not exist yet simply has no entry —
        // and its later creation is a change, which is the point.
        let (snap, _) = scan(&root.join("later.md").to_string_lossy(), 1000);
        assert!(snap.is_empty());
    }
}
