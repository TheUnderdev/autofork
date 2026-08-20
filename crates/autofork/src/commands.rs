//! User-facing commands: status, forks, run, logs, prune, doctor.

use crate::client::Client;
use autofork_core::config::Paths;
use autofork_core::project::project_root;
use autofork_core::protocol::{RequestBody, ResponseBody, StatusInfo};
use autofork_core::wake::{build_wake_payload, DueFork};
use std::time::Duration;

fn connect(paths: &Paths) -> Result<Client, String> {
    Client::connect_or_spawn(paths, Duration::from_secs(5)).map_err(|e| e.to_string())
}

/// Claude Code version at which the `fork` subagent type is enabled by default
/// in interactive sessions.
const FORK_DEFAULT_VERSION: [u64; 3] = [2, 1, 161];
/// Version at which the `fork` subagent type first exists (gated behind
/// `CLAUDE_CODE_FORK_SUBAGENT=1` until [`FORK_DEFAULT_VERSION`]).
const FORK_GATED_VERSION: [u64; 3] = [2, 1, 117];

/// The doctor hint printed for a fork-capable version: whether the force-enable
/// env is set, plus the confirmed remedy for a current version that still lacks
/// the fork type because of the staged server-side rollout.
fn fork_enable_hint() -> Vec<String> {
    fork_enable_hint_lines(std::env::var_os("CLAUDE_CODE_FORK_SUBAGENT").is_some())
}

fn fork_enable_hint_lines(env_set: bool) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(if env_set {
        "        CLAUDE_CODE_FORK_SUBAGENT is set (fork subagent force-enabled)".to_string()
    } else {
        "        note: CLAUDE_CODE_FORK_SUBAGENT is not set".to_string()
    });
    lines.push(
        "        a current version can still lack the fork type due to a staged server-side"
            .to_string(),
    );
    lines.push(
        "        rollout; force-enable it by adding {\"env\": {\"CLAUDE_CODE_FORK_SUBAGENT\": \"1\"}}"
            .to_string(),
    );
    lines.push(
        "        to ~/.claude/settings.json (persistent; preferred over a shell export)"
            .to_string(),
    );
    lines
}

/// Impostor `fork` agent definitions: a custom `fork.md` under `.claude/agents/`
/// (user-level and/or project-level) shadows the built-in fork subagent type
/// but does NOT inherit the conversation, so forks would silently lose context.
/// Returns the existing offenders.
fn impostor_agent_files(
    home: Option<&std::path::Path>,
    project_root: &std::path::Path,
) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Some(h) = home {
        let p = h.join(".claude/agents/fork.md");
        if p.is_file() {
            out.push(p);
        }
    }
    let p = project_root.join(".claude/agents/fork.md");
    if p.is_file() && !out.contains(&p) {
        out.push(p);
    }
    out
}

/// Extract the first `x.y.z` triple from `claude --version` output (which may
/// be just the number or include trailing text). `None` if none is found.
fn parse_version(s: &str) -> Option<[u64; 3]> {
    for tok in s.split(|c: char| c.is_whitespace() || c == '(' || c == ')') {
        let mut it = tok.split('.');
        let a = it.next().and_then(|x| x.parse::<u64>().ok());
        let b = it.next().and_then(|x| x.parse::<u64>().ok());
        let c = it.next().and_then(|x| {
            let digits: String = x.chars().take_while(|ch| ch.is_ascii_digit()).collect();
            digits.parse::<u64>().ok()
        });
        if let (Some(a), Some(b), Some(c)) = (a, b, c) {
            return Some([a, b, c]);
        }
    }
    None
}

fn fmt_ago(now: i64, ts: i64) -> String {
    let d = (now - ts).max(0);
    match d {
        0..=59 => format!("{d}s ago"),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86399 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn status(paths: &Paths) -> Result<(), String> {
    let mut client = connect(paths)?;
    let ResponseBody::StatusInfo(info) = client
        .request(RequestBody::Status)
        .map_err(|e| e.to_string())?
    else {
        return Err("unexpected response".into());
    };
    println!("autofork daemon v{} (pid {})", info.version, info.pid);
    print_sessions_header(&info);
    Ok(())
}

/// A display form of a session id that stays distinguishable: Claude Code's
/// UUIDs are random from the first character, so 8 chars identify them; but
/// opencode ids (`ses_<time-ordered>`) share their leading characters across
/// sessions created near in time — truncating them made every session of a
/// stretch display identically ("the same session twice"). Show those whole.
/// Codex ids are UUIDv7 — time-ordered too (the leading 48 bits are a
/// timestamp), so they get the same treatment; the version nibble tells the
/// two UUID kinds apart.
fn display_session_id(id: &str) -> &str {
    let uuid_v7 = id.len() >= 15 && id.as_bytes().get(14) == Some(&b'7');
    if id.contains('-') && !uuid_v7 {
        &id[..id.len().min(8)]
    } else {
        id
    }
}

fn print_sessions_header(info: &StatusInfo) {
    let t = now();
    // Belt: the store keys sessions by id (PRIMARY KEY), so duplicates
    // shouldn't exist — but if a daemon ever hands back two rows for one id,
    // show only the most recently active (the list is ordered by activity).
    let mut seen = std::collections::HashSet::new();
    let sessions: Vec<_> = info
        .sessions
        .iter()
        .filter(|s| seen.insert(s.session_id.as_str()))
        .collect();
    println!("sessions: {}", sessions.len());
    for s in sessions {
        let tokens = s
            .prompt_tokens
            .map(|n| format!(", ~{n} prompt tokens"))
            .unwrap_or_default();
        let stale = if s.stale { " [stale?]" } else { "" };
        println!(
            "  session {} [{}]{stale} {} (active {}{tokens})",
            display_session_id(&s.session_id),
            s.status,
            s.project_root.display(),
            fmt_ago(t, s.last_activity),
        );
    }
    if !info.running.is_empty() {
        println!(
            "fork runs in flight (closing their session loses their reports; \
             opencode runs die with the instance, other clients' runs finish their work):"
        );
        for r in &info.running {
            println!(
                "  {} — session {} — started {}",
                r.fork,
                display_session_id(&r.session_id),
                fmt_ago(t, r.started_at),
            );
        }
    }
    if !info.recent_runs.is_empty() {
        println!("recent wakes:");
        for r in &info.recent_runs {
            println!(
                "  {} ({}) [{}] {}",
                r.fork,
                r.trigger,
                r.state,
                fmt_ago(t, r.started_at),
            );
        }
    }
}

pub fn prune(paths: &Paths) -> Result<(), String> {
    let mut client = connect(paths)?;
    let ResponseBody::Pruned { sessions } = client
        .request(RequestBody::Prune)
        .map_err(|e| e.to_string())?
    else {
        return Err("unexpected response".into());
    };
    if sessions.is_empty() {
        println!("no stale sessions");
        return Ok(());
    }
    let t = now();
    println!(
        "closed {} stale session{}:",
        sessions.len(),
        if sessions.len() == 1 { "" } else { "s" }
    );
    for s in &sessions {
        println!(
            "  session {} {} (last active {})",
            display_session_id(&s.session_id),
            s.project_root.display(),
            fmt_ago(t, s.last_activity),
        );
    }
    Ok(())
}

pub fn list_forks(paths: &Paths, project: Option<std::path::PathBuf>) -> Result<(), String> {
    let cwd = project
        .or_else(|| std::env::current_dir().ok())
        .ok_or("cannot resolve cwd")?;
    let root = project_root(&cwd);
    let mut client = connect(paths)?;
    let ResponseBody::ForkList { items } = client
        .request(RequestBody::ListForks {
            project_root: root.clone(),
            cwd,
        })
        .map_err(|e| e.to_string())?
    else {
        return Err("unexpected response".into());
    };
    if items.is_empty() {
        println!("no forks discovered (looked for .autofork/forks/ up from here and user-level)");
        return Ok(());
    }
    println!("forks visible from {} :", root.display());
    for f in &items {
        println!(
            "  {} — {}",
            f.name,
            f.description.as_deref().unwrap_or("(no description)")
        );
        let mut details = vec![format!("runs on: {}", f.triggers.join(", "))];
        if let Some(t) = f.throttle_secs {
            details.push(format!("throttle: {t}s"));
        }
        if !f.after.is_empty() {
            details.push(format!("after: {}", f.after.join(", ")));
        }
        if f.priority != 0 {
            details.push(format!("priority: {}", f.priority));
        }
        if f.chain {
            details.push("chain".into());
        }
        if f.gate {
            details.push("gate".into());
        }
        if let Some(m) = &f.model {
            details.push(format!("model: {m}"));
        }
        if let Some(m) = &f.mode {
            details.push(format!("mode: {m}"));
        }
        if !f.tags.is_empty() {
            details.push(format!("tags: {}", f.tags.join(", ")));
        }
        if f.overlap {
            details.push("overlap allowed".into());
        }
        println!("      {}", details.join(" | "));
        println!("      {}", f.path.display());
        if let Some(skill) = &f.skill {
            println!("      skill: {}", skill.display());
        }
        for w in &f.warnings {
            println!("      warning: {w}");
        }
    }
    Ok(())
}

/// List the lifecycle hooks visible from a directory. Pure filesystem
/// discovery — no daemon round trip (the daemon re-discovers per firing
/// anyway, so there is no daemon-side state to consult).
pub fn list_hooks(paths: &Paths, project: Option<std::path::PathBuf>) -> Result<(), String> {
    let cwd = project
        .or_else(|| std::env::current_dir().ok())
        .ok_or("cannot resolve cwd")?;
    let user_hooks = paths.base.join("hooks");
    let (entries, warnings) = autofork_core::hooks::discover_hooks(&cwd, Some(&user_hooks));
    if entries.is_empty() && warnings.is_empty() {
        println!(
            "no lifecycle hooks discovered (looked for .autofork/hooks/ up from here and {})",
            user_hooks.display()
        );
        return Ok(());
    }
    println!("lifecycle hooks visible from {} :", cwd.display());
    for h in &entries {
        println!(
            "  {} — {}",
            h.name,
            h.parsed
                .def
                .description
                .as_deref()
                .unwrap_or("(no description)")
        );
        let on: Vec<String> = h.parsed.def.on.iter().map(|o| o.label()).collect();
        println!(
            "      on: {} | timeout: {}s",
            if on.is_empty() {
                "(nothing — never fires)".to_string()
            } else {
                on.join(", ")
            },
            h.parsed.def.timeout_secs
        );
        println!("      command: {}", h.parsed.def.command);
        println!("      {}", h.path.display());
        for w in &h.parsed.warnings {
            println!("      warning: {w}");
        }
    }
    for w in &warnings {
        println!("  warning: {w}");
    }
    Ok(())
}

/// Manual runs can no longer spawn anything (forks are subagents of an
/// interactive session). Instead we print the wake-style spawn instruction to
/// paste into an interactive Claude Code session.
pub fn run_fork(paths: &Paths, name: Option<String>, tag: Option<String>) -> Result<(), String> {
    if name.is_none() && tag.is_none() {
        return Err("provide a fork name or --tag <tag>".into());
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let root = project_root(&cwd);
    let user_forks = paths.base.join("forks");
    let claude_dir = std::env::var_os("AUTOFORK_CLAUDE_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".claude")));
    let agents_dir = std::env::var_os("AUTOFORK_AGENTS_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".agents")));
    let (entries, _) = autofork_core::discovery::discover_forks(
        &cwd,
        Some(&user_forks),
        claude_dir.as_deref(),
        agents_dir.as_deref(),
    );

    let picked: Vec<_> = match (&name, &tag) {
        (Some(name), _) => match entries.into_iter().find(|e| &e.name == name) {
            Some(e) => vec![e],
            None => {
                return Err(format!(
                    "no fork named '{name}' visible from {}",
                    cwd.display()
                ));
            }
        },
        (None, Some(tag)) => {
            let matched: Vec<_> = entries
                .into_iter()
                .filter(|e| e.parsed.def.tags.iter().any(|t| t == tag))
                .collect();
            if matched.is_empty() {
                return Err(format!(
                    "no forks with tag '{tag}' visible from {}",
                    cwd.display()
                ));
            }
            matched
        }
        (None, None) => unreachable!(),
    };

    let due: Vec<DueFork> = picked
        .iter()
        .map(|e| DueFork {
            name: e.name.clone(),
            path: e.path.to_string_lossy().into_owned(),
            trigger: "manual".to_string(),
            overlap: e.parsed.def.overlap,
            after: Vec::new(),
            skill: autofork_core::discovery::skill_sibling(&e.path)
                .map(|p| p.to_string_lossy().into_owned()),
            chain: e.parsed.def.chain,
            // The manual print targets the Claude Code subagent path, which
            // cannot apply model/mode overrides.
            model: None,
            model_fallbacks: Vec::new(),
            mode: None,
        })
        .collect();
    let payload = build_wake_payload(
        "(the current session)",
        "(the current conversation)",
        &root.to_string_lossy(),
        &due,
        &[],
    );

    println!(
        "autofork can no longer spawn forks itself — forks run as fork subagents of an\n\
         interactive session. Paste the following into an interactive Claude Code session\n\
         to run the selected fork(s) now:\n"
    );
    println!("{payload}");
    Ok(())
}

pub fn logs(paths: &Paths, follow: bool) -> Result<(), String> {
    let path = paths.daemon_log();
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let tail: Vec<&str> = content.lines().rev().take(100).collect();
    for line in tail.iter().rev() {
        println!("{line}");
    }
    if follow {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
        let mut pos = file.metadata().map_err(|e| e.to_string())?.len();
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let len = file.metadata().map_err(|e| e.to_string())?.len();
            if len < pos {
                pos = 0;
            }
            if len > pos {
                file.seek(SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
                let mut buf = String::new();
                file.read_to_string(&mut buf).map_err(|e| e.to_string())?;
                print!("{buf}");
                pos = len;
            }
        }
    }
    Ok(())
}

pub fn doctor(paths: &Paths) -> Result<(), String> {
    let mut problems = 0;
    let ok = |msg: &str| println!("  ok: {msg}");
    println!("autofork doctor");

    // Binaries.
    match std::env::current_exe() {
        Ok(exe) => {
            ok(&format!(
                "cli: {} (v{})",
                exe.display(),
                env!("CARGO_PKG_VERSION")
            ));
            let daemon_bin = exe.parent().map(|p| p.join("autofork-daemon"));
            match daemon_bin {
                Some(p) if p.is_file() => ok(&format!("daemon binary: {}", p.display())),
                _ => {
                    problems += 1;
                    println!("  PROBLEM: autofork-daemon not found next to the CLI");
                }
            }
        }
        Err(e) => {
            problems += 1;
            println!("  PROBLEM: cannot resolve current exe: {e}");
        }
    }

    // Daemon liveness + version.
    match Client::connect(paths, Duration::from_secs(2)) {
        Ok(mut client) => match client.request(RequestBody::Hello {
            version: env!("CARGO_PKG_VERSION").to_string(),
        }) {
            Ok(ResponseBody::HelloInfo { version }) => {
                ok(&format!(
                    "daemon answering at {} (v{version})",
                    paths.socket().display()
                ));
            }
            other => {
                problems += 1;
                println!("  PROBLEM: daemon handshake failed: {other:?}");
            }
        },
        Err(_) => {
            println!("  note: daemon not running (it auto-starts on the next hook event)");
        }
    }

    // State db.
    if paths.db().is_file() {
        ok(&format!("state db: {}", paths.db().display()));
    } else {
        println!("  note: no state db yet at {}", paths.db().display());
    }

    // Claude Code version gating for the `fork` subagent type:
    //   >= 2.1.161            enabled by default in interactive sessions
    //   2.1.117 ..= 2.1.160   exists but gated behind CLAUDE_CODE_FORK_SUBAGENT=1
    //   < 2.1.117             no fork subagent — too old for autofork v0.5
    match std::process::Command::new("claude")
        .arg("--version")
        .output()
    {
        Ok(out) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout);
            let raw = raw.trim();
            match parse_version(raw) {
                Some(v) if v >= FORK_DEFAULT_VERSION => {
                    ok(&format!("claude: {raw}"));
                    for line in fork_enable_hint() {
                        println!("{line}");
                    }
                }
                Some(v) if v >= FORK_GATED_VERSION => {
                    println!("  WARN: claude {raw}: the fork subagent is gated on this version —");
                    println!("        export CLAUDE_CODE_FORK_SUBAGENT=1 or upgrade to >= 2.1.161");
                }
                Some(_) => {
                    problems += 1;
                    println!(
                        "  PROBLEM: claude {raw} is too old for autofork v0.5 (needs the fork \
                         subagent, >= 2.1.117)"
                    );
                }
                None => println!("  note: could not parse 'claude --version' output ({raw:?})"),
            }
        }
        _ => println!("  note: could not run 'claude --version' to check fork subagent support"),
    }

    // opencode integration (only reported when opencode or the plugin is
    // present — a Claude-Code-only install stays quiet).
    for line in crate::opencode::doctor_lines() {
        if line.contains("plugin installed") {
            ok(&line);
        } else {
            println!("  WARN: {line}");
        }
    }

    // Codex CLI integration (likewise quiet unless codex or our hooks exist).
    for line in crate::codex::doctor_lines() {
        if line.contains("hooks installed (") {
            ok(&line);
        } else {
            println!("  WARN: {line}");
        }
    }

    // Impostor `fork` agent definitions (a context-less shadow of the built-in
    // type — see the wake payload's own prohibition against creating one).
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let proot = std::env::current_dir()
        .ok()
        .map(|c| project_root(&c))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    for f in impostor_agent_files(home.as_deref(), &proot) {
        problems += 1;
        println!(
            "  PROBLEM: custom agent 'fork' at {} shadows/impersonates the",
            f.display()
        );
        println!("           built-in fork subagent type — autofork forks would silently lose");
        println!("           conversation context; delete it.");
    }

    println!(
        "  note: v0.5 forks run as fork subagents of your interactive session — no headless\n\
         \x20       fork subprocesses, and no separate fork-session transcripts to prune."
    );

    if problems == 0 {
        println!("all good");
        Ok(())
    } else {
        Err(format!("{problems} problem(s) found"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parsing_and_gating() {
        assert_eq!(parse_version("2.1.161"), Some([2, 1, 161]));
        assert_eq!(parse_version("2.1.161 (Claude Code)"), Some([2, 1, 161]));
        assert_eq!(parse_version("v2.1.117-beta"), None); // leading 'v' breaks the first token
        assert_eq!(parse_version("2.1.117-beta"), Some([2, 1, 117]));
        assert_eq!(parse_version("nonsense output"), None);

        // Gating thresholds.
        assert!(parse_version("2.1.161").unwrap() >= FORK_DEFAULT_VERSION);
        assert!(parse_version("2.2.0").unwrap() >= FORK_DEFAULT_VERSION);
        let gated = parse_version("2.1.150").unwrap();
        assert!(gated < FORK_DEFAULT_VERSION && gated >= FORK_GATED_VERSION);
        assert!(parse_version("2.1.116").unwrap() < FORK_GATED_VERSION);
        assert!(parse_version("2.0.999").unwrap() < FORK_GATED_VERSION);
    }

    #[test]
    fn fork_enable_hint_reflects_env_and_recommends_settings_pin() {
        let set = fork_enable_hint_lines(true).join("\n");
        assert!(set.contains("CLAUDE_CODE_FORK_SUBAGENT is set"));
        assert!(set.contains("force-enabled"));

        let unset = fork_enable_hint_lines(false).join("\n");
        assert!(unset.contains("CLAUDE_CODE_FORK_SUBAGENT is not set"));
        assert!(unset.contains("staged server-side"));
        assert!(unset.contains(r#"{"env": {"CLAUDE_CODE_FORK_SUBAGENT": "1"}}"#));
        assert!(unset.contains("~/.claude/settings.json"));
    }

    #[test]
    fn detects_impostor_fork_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(project.join(".autofork")).unwrap();

        // Nothing yet.
        assert!(impostor_agent_files(Some(&home), &project).is_empty());

        // A user-level impostor.
        std::fs::create_dir_all(home.join(".claude/agents")).unwrap();
        std::fs::write(home.join(".claude/agents/fork.md"), "impostor").unwrap();
        let found = impostor_agent_files(Some(&home), &project);
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with(".claude/agents/fork.md"));

        // Plus a project-level impostor → both reported.
        std::fs::create_dir_all(project.join(".claude/agents")).unwrap();
        std::fs::write(project.join(".claude/agents/fork.md"), "impostor").unwrap();
        assert_eq!(impostor_agent_files(Some(&home), &project).len(), 2);

        // Same dir as home and project (dedup): no double-count.
        assert_eq!(impostor_agent_files(Some(&project), &project).len(), 1);
    }
}
