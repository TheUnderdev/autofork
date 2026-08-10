//! `autofork opencode …`: the opencode integration.
//!
//! opencode has no hook-command/exit-2 mechanism; instead an opencode server
//! plugin (installed by `autofork opencode install`) shells out to
//! `autofork opencode hook <kind>` with a small JSON object on stdin — the
//! same transport role Claude Code's hooks play, reusing the daemon
//! spawn/flock and version-handshake logic. Unlike the Claude Code hooks,
//! `stop-wait` answers on **stdout** with structured JSON: the plugin runs
//! fork sessions itself (opencode's native session fork + a prompt), so no
//! model-facing payload or exit-code signalling is involved.

use crate::client::{spawn_daemon_detached, Client};
use autofork_core::config::Paths;
use autofork_core::protocol::{Event, EventKind, RequestBody, ResponseBody};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

/// The client name stamped on every event this integration sends.
const CLIENT: &str = "opencode";

/// The plugin file name inside opencode's global config plugin dir.
const PLUGIN_FILE: &str = "autofork.js";

/// The embedded opencode plugin. `{{VERSION}}` is replaced at install time so
/// `doctor` can tell an out-of-date installed copy from the current one.
const PLUGIN_SOURCE: &str = include_str!("../assets/opencode-plugin.js");

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OcHookKind {
    /// First sight of a session (or its resume): register it.
    SessionStart,
    /// A genuine user turn started: cancels any parked stop-wait, bumps the
    /// pause epoch.
    PromptSubmit,
    /// The session went idle: long-poll for due forks. Answers on stdout:
    /// `{"wake":{"payload":…,"forks":[…]}}` or `{"waited":true}`.
    StopWait,
    /// The session was deleted.
    SessionEnd,
    /// The plugin forked the session and prompted the copy.
    ForkSpawned,
    /// A fork run reached a terminal status.
    ForkCompleted,
}

/// What the plugin writes on stdin. One shape for every kind; unused fields
/// are simply absent.
#[derive(Debug, Deserialize)]
struct OcInput {
    session_id: String,
    /// The opencode instance directory (the project the plugin serves).
    directory: PathBuf,
    /// The worktree root, when opencode resolves one above `directory`.
    #[serde(default)]
    worktree: Option<PathBuf>,
    /// Model id of the session's last assistant message (e.g.
    /// `claude-haiku-4-5`), for context-window resolution.
    #[serde(default)]
    model: Option<String>,
    /// The session's context gauge in tokens (input + cache read + cache
    /// write of the last assistant step).
    #[serde(default)]
    context_tokens: Option<u64>,
    /// The model's real context window (`limit.context` from opencode's
    /// provider catalog), for context-threshold resolution.
    #[serde(default)]
    context_window: Option<u64>,
    /// fork-spawned / fork-completed: the fork's name.
    #[serde(default)]
    fork: Option<String>,
    /// fork-spawned / fork-completed: the fork session's id.
    #[serde(default)]
    run_ref: Option<String>,
    /// fork-completed: `completed` / `failed` / `stopped`.
    #[serde(default)]
    status: Option<String>,
    /// stop-wait: the session is mid-run (busy poll — `every:`/context
    /// triggers only; no idle deadlines, no pause baseline).
    #[serde(default)]
    busy: Option<bool>,
    /// prompt-submit: whether this turn is genuine user activity. The plugin
    /// sends `false` for the turn its own chain-report injection starts —
    /// that turn must not bump the pause epoch. Absent = genuine (`true`).
    #[serde(default)]
    waking: Option<bool>,
    /// fork-completed: the run's report ended with the chain sentinel.
    #[serde(default, rename = "continue")]
    cont: Option<bool>,
}

pub fn run_hook(kind: OcHookKind) {
    // Like the Claude Code hooks: never break the host, whatever happens.
    // stop-wait must still answer JSON so the plugin's read completes.
    if run_hook_inner(kind).is_none() && kind == OcHookKind::StopWait {
        println!("{{\"waited\":true}}");
    }
}

fn run_hook_inner(kind: OcHookKind) -> Option<()> {
    let mut raw = String::new();
    use std::io::Read;
    std::io::stdin().read_to_string(&mut raw).ok()?;
    let input: OcInput = serde_json::from_str(&raw).ok()?;
    let paths = Paths::from_env()?;

    let root = project_root_for(input.worktree.as_deref(), &input.directory);

    let event = |ev: EventKind| Event {
        event: ev,
        session_id: input.session_id.clone(),
        transcript_path: None,
        cwd: input.directory.clone(),
        project_root: root.clone(),
        source: None,
        model: input.model.clone(),
        enable_tags: crate::hook::tags_from_env("AUTOFORK_ENABLE_TAGS"),
        disable_tags: crate::hook::tags_from_env("AUTOFORK_DISABLE_TAGS"),
        waking: None,
        notif_tool_use_id: None,
        notif_task_id: None,
        notif_status: None,
        notif_continue: None,
        context_tokens: input.context_tokens,
        context_window: input.context_window,
        client: Some(CLIENT.to_string()),
        busy: input.busy,
    };

    match kind {
        OcHookKind::SessionStart => {
            let client = Client::connect_or_spawn(&paths, Duration::from_secs(5)).ok()?;
            let mut client = client.ensure_current_version(&paths).ok()?;
            let _ = client.request(RequestBody::Event(event(EventKind::SessionStart)));
        }
        OcHookKind::PromptSubmit => {
            // Same hard budget as the Claude Code prompt hook: never wait on a
            // daemon spawn in the turn-start path.
            let Ok(mut client) = Client::connect(&paths, Duration::from_millis(1500)) else {
                spawn_daemon_detached(&paths);
                return Some(());
            };
            let mut ev = event(EventKind::PromptSubmit);
            // The plugin skips its own zero-turn report injections entirely,
            // and flags the one turn it *does* start itself — a chain fork's
            // turn-triggering report — as `waking: false`. Everything else is
            // a genuine user turn and starts a new pause.
            ev.waking = Some(input.waking.unwrap_or(true));
            let _ = client.request(RequestBody::Event(ev));
        }
        OcHookKind::StopWait => {
            // Orphan watchdog: this subprocess is the session's liveness
            // heartbeat. The plugin that spawned it dies with the opencode
            // process, but nothing kills *us* — and an orphaned poll keeps
            // the daemon convinced the session is alive forever (no
            // [stale?], no grace-close). When our parent dies we get
            // reparented; exit then, dropping the poll so the daemon's
            // poll-loss grace-close fires. Covers crashes and exits that
            // never reach the plugin's dispose hook.
            let ppid0 = std::os::unix::process::parent_id();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(5));
                if std::os::unix::process::parent_id() != ppid0 {
                    std::process::exit(0);
                }
            });
            let client = Client::connect_or_spawn(&paths, Duration::from_secs(10)).ok()?;
            let mut client = client.ensure_current_version(&paths).ok()?;
            match client.stop_wait(event(EventKind::Stop)) {
                Ok(ResponseBody::Wake { payload, forks }) => {
                    let out = serde_json::json!({
                        "wake": {
                            "payload": payload,
                            "forks": forks.unwrap_or_default(),
                        }
                    });
                    println!("{out}");
                }
                _ => println!("{{\"waited\":true}}"),
            }
        }
        OcHookKind::SessionEnd => {
            let mut client = Client::connect_or_spawn(&paths, Duration::from_secs(5)).ok()?;
            let _ = client.request(RequestBody::Event(event(EventKind::SessionEnd)));
        }
        OcHookKind::ForkSpawned => {
            let mut client = Client::connect_or_spawn(&paths, Duration::from_secs(5)).ok()?;
            let _ = client.request(RequestBody::ForkSpawned {
                session_id: input.session_id.clone(),
                fork: input.fork.clone()?,
                run_ref: input.run_ref.clone()?,
            });
        }
        OcHookKind::ForkCompleted => {
            let mut client = Client::connect_or_spawn(&paths, Duration::from_secs(5)).ok()?;
            let _ = client.request(RequestBody::ForkCompleted {
                session_id: input.session_id.clone(),
                fork: input.fork.clone()?,
                run_ref: input.run_ref.clone()?,
                status: input.status.clone().unwrap_or_else(|| "completed".into()),
                cont: input.cont,
            });
        }
    }
    Some(())
}

/// The project root for an opencode instance. The worktree (when known) is
/// the project identity; discovery walks up from it either way. opencode
/// reports `/` as the worktree for directories outside any VCS — that is no
/// project root, so fall back to the directory itself (likewise for a
/// worktree that doesn't actually contain the directory).
fn project_root_for(worktree: Option<&std::path::Path>, directory: &std::path::Path) -> PathBuf {
    worktree
        .filter(|w| *w != std::path::Path::new("/") && directory.starts_with(w))
        .map(|w| w.to_path_buf())
        .unwrap_or_else(|| directory.to_path_buf())
}

/// The rendered plugin source (version stamped).
pub fn plugin_source() -> String {
    PLUGIN_SOURCE.replace("{{VERSION}}", env!("CARGO_PKG_VERSION"))
}

/// opencode's global config dir (`$XDG_CONFIG_HOME/opencode`, defaulting to
/// `~/.config/opencode`).
pub fn opencode_config_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("opencode"));
    }
    dirs_home().map(|h| h.join(".config").join("opencode"))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Where the plugin gets installed.
pub fn plugin_path() -> Option<PathBuf> {
    Some(opencode_config_dir()?.join("plugin").join(PLUGIN_FILE))
}

/// `autofork opencode install`: write the plugin into opencode's global
/// plugin dir (opencode auto-discovers `plugin/*.js` there).
pub fn install(print: bool) -> Result<(), String> {
    if print {
        print!("{}", plugin_source());
        return Ok(());
    }
    let path = plugin_path().ok_or("cannot determine opencode config dir")?;
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    std::fs::write(&path, plugin_source())
        .map_err(|e| format!("writing {}: {e}", path.display()))?;
    println!("installed {}", path.display());
    println!("restart opencode to load it (plugins load at instance start)");
    Ok(())
}

/// `autofork opencode uninstall`: remove the installed plugin.
pub fn uninstall() -> Result<(), String> {
    let path = plugin_path().ok_or("cannot determine opencode config dir")?;
    match std::fs::remove_file(&path) {
        Ok(()) => {
            println!("removed {}", path.display());
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("not installed ({} absent)", path.display());
            Ok(())
        }
        Err(e) => Err(format!("removing {}: {e}", path.display())),
    }
}

/// Doctor check: is the plugin installed and current? Returns lines to print
/// (empty when everything is fine or opencode isn't in use).
pub fn doctor_lines() -> Vec<String> {
    let mut lines = Vec::new();
    let have_opencode = std::process::Command::new("opencode")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let Some(path) = plugin_path() else {
        return lines;
    };
    match std::fs::read_to_string(&path) {
        Ok(installed) => {
            if installed != plugin_source() {
                lines.push(format!(
                    "opencode plugin at {} is outdated or modified — run `autofork opencode install` to refresh it",
                    path.display()
                ));
            } else {
                lines.push(format!("opencode plugin installed ({})", path.display()));
            }
            if !have_opencode {
                lines.push(
                    "opencode plugin is installed but `opencode` was not found on PATH".into(),
                );
            }
        }
        Err(_) => {
            if have_opencode {
                lines.push(
                    "opencode detected but the autofork plugin is not installed — run `autofork opencode install` to enable forks in opencode sessions"
                        .into(),
                );
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_source_is_version_stamped() {
        let src = plugin_source();
        assert!(!src.contains("{{VERSION}}"));
        assert!(src.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn project_root_ignores_the_slash_worktree() {
        use std::path::Path;
        // A real worktree above the directory wins.
        assert_eq!(
            project_root_for(Some(Path::new("/repo")), Path::new("/repo/sub")),
            Path::new("/repo")
        );
        // opencode's non-VCS sentinel `/` is not a project root.
        assert_eq!(
            project_root_for(Some(Path::new("/")), Path::new("/tmp/proj")),
            Path::new("/tmp/proj")
        );
        // A worktree that doesn't contain the directory is ignored too.
        assert_eq!(
            project_root_for(Some(Path::new("/elsewhere")), Path::new("/tmp/proj")),
            Path::new("/tmp/proj")
        );
        assert_eq!(
            project_root_for(None, Path::new("/tmp/proj")),
            Path::new("/tmp/proj")
        );
    }

    #[test]
    fn oc_input_parses_minimal_and_full() {
        let min: OcInput = serde_json::from_str(r#"{"session_id":"s","directory":"/p"}"#).unwrap();
        assert_eq!(min.session_id, "s");
        assert!(min.model.is_none());
        let full: OcInput = serde_json::from_str(
            r#"{"session_id":"s","directory":"/p","worktree":"/w","model":"claude-haiku-4-5",
                "context_tokens":1234,"context_window":1000000,
                "fork":"journal","run_ref":"ses_x","status":"completed"}"#,
        )
        .unwrap();
        assert_eq!(full.worktree.as_deref(), Some(std::path::Path::new("/w")));
        assert_eq!(full.context_tokens, Some(1234));
        assert_eq!(full.context_window, Some(1_000_000));
        assert_eq!(full.status.as_deref(), Some("completed"));
    }
}
