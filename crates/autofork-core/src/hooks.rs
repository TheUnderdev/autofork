//! Lifecycle hooks: small shell commands the daemon runs at session
//! lifecycle moments — no model, no fork, no context. Built for resource
//! integrations (workspace leases, seat locks, scratch allocations) that need
//! to follow a session's life: acquire on start, renew on activity, release
//! on end, park after an idle timeout.
//!
//! A hook is a markdown file with YAML frontmatter, discovered from
//! `.autofork/hooks/` trees (per ancestor directory, nearest first, then the
//! user level `~/.autofork/hooks/`). Like forks, two layouts are supported:
//! a bare `<name>.md` or a `<name>/HOOK.md` folder. The body is documentation
//! only — autofork never feeds it to anything.
//!
//! ```markdown
//! ---
//! hook: true
//! description: keep the workspace lease alive
//! on: [session_start, activity, "idle: 5m", session_end]
//! command: lease-tool touch --session "$AUTOFORK_SESSION_ID"
//! timeout: 30s
//! ---
//! ```
//!
//! Events (stable names): `session_start` (a session registered; also covers
//! resumes — `AUTOFORK_SOURCE` says which), `resume` (only the resume case),
//! `activity` (genuine user activity), `idle`/`idle: <dur>` (the session has
//! been idle that long — fires once per pause, while the session stays open),
//! `session_end` (the session ended; `AUTOFORK_END_REASON` distinguishes a
//! clean end from `lost`/`pruned`/`timeout`). No event can cover SIGKILL,
//! crashes, or power loss — keep a lease TTL as the crash fallback.

use crate::duration::parse_duration_yaml;
use crate::frontmatter::split_frontmatter;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Cap on organizational-subfolder nesting below a hooks root.
const MAX_HOOK_NESTING_DEPTH: usize = 8;

/// Default command timeout when the frontmatter sets none (seconds).
pub const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 60;

/// A lifecycle moment a hook fires at (`on`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookOn {
    /// A session registered with the daemon (startup, resume, clear — any
    /// event that opens a session row).
    SessionStart,
    /// A session registered with `source: resume` specifically.
    Resume,
    /// Genuine user activity (each real user prompt).
    Activity,
    /// The session has been idle this long (once per pause; the session
    /// stays open). `after_secs` unset = the configured default idle
    /// deadline.
    Idle { after_secs: Option<u64> },
    /// The session ended — gracefully or via the daemon's liveness fallbacks
    /// (`AUTOFORK_END_REASON` says which).
    SessionEnd,
}

impl HookOn {
    /// Stable label (documentation, listings).
    pub fn label(&self) -> String {
        match self {
            HookOn::SessionStart => "session_start".into(),
            HookOn::Resume => "resume".into(),
            HookOn::Activity => "activity".into(),
            HookOn::Idle { after_secs: None } => "idle".into(),
            HookOn::Idle {
                after_secs: Some(s),
            } => format!("idle:{s}"),
            HookOn::SessionEnd => "session_end".into(),
        }
    }
}

/// A parsed hook definition (frontmatter only; the body is documentation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookDef {
    pub description: Option<String>,
    /// The lifecycle moments this hook fires at.
    pub on: Vec<HookOn>,
    /// The shell command to run (`sh -c`), with the context in
    /// `AUTOFORK_*` environment variables.
    pub command: String,
    /// Kill the command after this long (seconds).
    pub timeout_secs: u64,
}

#[derive(Deserialize, Default)]
struct RawHook {
    // The marker (`hook: true`). Absent or `false` = not a hook.
    #[serde(default)]
    hook: Option<serde_yaml::Value>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    on: Option<serde_yaml::Value>,
    #[serde(default)]
    command: Option<serde_yaml::Value>,
    #[serde(default)]
    timeout: Option<serde_yaml::Value>,
}

/// The outcome of parsing a `.md` file in a hooks tree.
#[derive(Debug, Clone)]
pub enum HookParse {
    Hook(ParsedHook),
    /// Not a hook (no `hook: true`). `hook_like` is true when the frontmatter
    /// nevertheless carries hook keys — a likely forgotten marker.
    NotHook {
        hook_like: bool,
    },
    /// A frontmatter block is present but is not valid YAML.
    Invalid,
}

#[derive(Debug, Clone)]
pub struct ParsedHook {
    pub def: HookDef,
    pub warnings: Vec<String>,
}

fn parse_on_entry(v: &serde_yaml::Value, warnings: &mut Vec<String>) -> Option<HookOn> {
    if let serde_yaml::Value::String(s) = v {
        // Accept both `- idle: 5m` (a YAML map) and `- "idle: 5m"` (a quoted
        // string), since the latter reads naturally in flow lists.
        if let Some(rest) = s.strip_prefix("idle:") {
            return match parse_duration_yaml(&serde_yaml::Value::String(rest.trim().into())) {
                Some(secs) => Some(HookOn::Idle {
                    after_secs: Some(secs),
                }),
                None => {
                    warnings.push(format!("invalid idle duration '{}', skipping", s.trim()));
                    None
                }
            };
        }
        return match s.as_str() {
            "session_start" => Some(HookOn::SessionStart),
            "resume" => Some(HookOn::Resume),
            "activity" => Some(HookOn::Activity),
            "idle" => Some(HookOn::Idle { after_secs: None }),
            "session_end" => Some(HookOn::SessionEnd),
            other => {
                warnings.push(format!("unknown hook event '{other}', skipping"));
                None
            }
        };
    }
    if let serde_yaml::Value::Mapping(m) = v {
        if m.len() == 1 {
            if let Some((serde_yaml::Value::String(key), val)) = m.iter().next() {
                if key == "idle" {
                    let parsed = parse_duration_yaml(val).map(|s| HookOn::Idle {
                        after_secs: Some(s),
                    });
                    if parsed.is_none() {
                        warnings.push("invalid idle duration in 'on', skipping".into());
                    }
                    return parsed;
                }
                warnings.push(format!("unknown hook event '{key}', skipping"));
                return None;
            }
        }
    }
    warnings.push("malformed 'on' entry, skipping".into());
    None
}

/// Parse a hook file's full content. `name` is used only for warning text.
pub fn parse_hook_file(name: &str, content: &str) -> HookParse {
    let (front, _body) = split_frontmatter(content);
    let raw: RawHook = match front {
        None => return HookParse::NotHook { hook_like: false },
        Some(yaml) => match serde_yaml::from_str(yaml) {
            Ok(raw) => raw,
            Err(e) => {
                tracing::debug!(hook = name, error = %e, "invalid hook frontmatter YAML");
                return HookParse::Invalid;
            }
        },
    };

    if !matches!(&raw.hook, Some(serde_yaml::Value::Bool(true))) {
        return HookParse::NotHook {
            hook_like: !matches!(&raw.hook, Some(serde_yaml::Value::Bool(false)))
                && (raw.on.is_some() || raw.command.is_some()),
        };
    }

    let mut warnings = Vec::new();

    let command = match &raw.command {
        Some(serde_yaml::Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            warnings.push(format!(
                "hook '{name}': missing or empty 'command'; it can never run"
            ));
            String::new()
        }
    };

    let on: Vec<HookOn> = match &raw.on {
        None => {
            warnings.push(format!("hook '{name}': no 'on' events; it will never fire"));
            Vec::new()
        }
        Some(serde_yaml::Value::Sequence(seq)) => seq
            .iter()
            .filter_map(|e| parse_on_entry(e, &mut warnings))
            .collect(),
        Some(other) => parse_on_entry(other, &mut warnings).into_iter().collect(),
    };
    if raw.on.is_some() && on.is_empty() {
        warnings.push(format!(
            "hook '{name}': 'on' has no valid events; it will never fire"
        ));
    }

    let timeout_secs = match &raw.timeout {
        None => DEFAULT_HOOK_TIMEOUT_SECS,
        Some(v) => match parse_duration_yaml(v).filter(|s| *s > 0) {
            Some(s) => s,
            None => {
                warnings.push(format!("hook '{name}': invalid timeout, using the default"));
                DEFAULT_HOOK_TIMEOUT_SECS
            }
        },
    };

    for w in &warnings {
        tracing::warn!(hook = name, "{w}");
    }

    HookParse::Hook(ParsedHook {
        def: HookDef {
            description: raw.description.filter(|d| !d.trim().is_empty()),
            on,
            command,
            timeout_secs,
        },
        warnings,
    })
}

/// A discovered hook definition.
#[derive(Debug, Clone)]
pub struct HookEntry {
    pub name: String,
    /// Absolute path of the definition file (`…/<name>.md` or `…/HOOK.md`).
    pub path: PathBuf,
    pub parsed: ParsedHook,
}

/// The hooks roots relevant to `dir`: each ancestor's `.autofork/hooks`
/// (including `dir` itself), nearest first, then `user_hooks_root`
/// (`~/.autofork/hooks`) if not already among them.
pub fn hook_roots(dir: &Path, user_hooks_root: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let start = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let mut cur = Some(start.as_path());
    while let Some(d) = cur {
        let candidate = d.join(".autofork").join("hooks");
        if candidate.is_dir() {
            roots.push(candidate);
        }
        cur = d.parent();
    }
    if let Some(user) = user_hooks_root {
        let c = user.canonicalize().unwrap_or_else(|_| user.to_path_buf());
        if c.is_dir() && !roots.contains(&c) {
            roots.push(c);
        }
    }
    roots
}

/// Discover all hooks visible from `dir` (nearest-project-first, then
/// user-level; first-discovered wins name collisions). Returns entries in
/// discovery order plus warnings.
pub fn discover_hooks(dir: &Path, user_hooks_root: Option<&Path>) -> (Vec<HookEntry>, Vec<String>) {
    let mut entries: Vec<HookEntry> = Vec::new();
    let mut warnings = Vec::new();
    for root in hook_roots(dir, user_hooks_root) {
        scan_hooks_dir(&root, 0, &mut entries, &mut warnings);
    }
    (entries, warnings)
}

fn insert_entry(
    entries: &mut Vec<HookEntry>,
    warnings: &mut Vec<String>,
    name: String,
    path: PathBuf,
) {
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            warnings.push(format!("failed to read {}: {e}", path.display()));
            return;
        }
    };
    let parsed = match parse_hook_file(&name, &content) {
        HookParse::Hook(p) => p,
        HookParse::NotHook { hook_like: true } => {
            warnings.push(format!(
                "{} has hook-like frontmatter but no `hook: true` — not treated as a hook",
                path.display()
            ));
            return;
        }
        HookParse::NotHook { hook_like: false } => return,
        HookParse::Invalid => {
            warnings.push(format!(
                "hook '{name}' at {} has invalid frontmatter YAML, skipped",
                path.display()
            ));
            return;
        }
    };
    if let Some(existing) = entries.iter().find(|e| e.name == name) {
        if existing.path != path {
            warnings.push(format!(
                "hook '{name}' at {} shadowed by {}",
                path.display(),
                existing.path.display()
            ));
        }
        return;
    }
    entries.push(HookEntry { name, path, parsed });
}

fn scan_hooks_dir(
    dir: &Path,
    depth: usize,
    entries: &mut Vec<HookEntry>,
    warnings: &mut Vec<String>,
) {
    if depth > MAX_HOOK_NESTING_DEPTH {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut items: Vec<_> = read.filter_map(|e| e.ok()).collect();
    items.sort_by_key(|e| e.file_name());
    for item in items {
        let path = item.path();
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if file_name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            let hook_md = path.join("HOOK.md");
            if hook_md.is_file() {
                insert_entry(entries, warnings, file_name.to_string(), hook_md);
            } else {
                scan_hooks_dir(&path, depth + 1, entries, warnings);
            }
        } else if let Some(stem) = file_name.strip_suffix(".md") {
            if !stem.is_empty() {
                insert_entry(entries, warnings, stem.to_string(), path.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn parse(content: &str) -> ParsedHook {
        match parse_hook_file("test", content) {
            HookParse::Hook(p) => p,
            other => panic!("expected a hook, got {other:?}"),
        }
    }

    #[test]
    fn marker_required() {
        assert!(matches!(
            parse_hook_file("n", "just notes\n"),
            HookParse::NotHook { hook_like: false }
        ));
        assert!(matches!(
            parse_hook_file("n", "---\ntitle: x\n---\nbody"),
            HookParse::NotHook { hook_like: false }
        ));
        // Hook keys without the marker: a likely forgotten marker.
        assert!(matches!(
            parse_hook_file("n", "---\non: [session_end]\ncommand: x\n---\n"),
            HookParse::NotHook { hook_like: true }
        ));
        // Explicit opt-out is silent.
        assert!(matches!(
            parse_hook_file("n", "---\nhook: false\ncommand: x\n---\n"),
            HookParse::NotHook { hook_like: false }
        ));
        assert!(matches!(
            parse_hook_file("n", "---\n: [oops\n---\n"),
            HookParse::Invalid
        ));
    }

    #[test]
    fn full_definition_parses() {
        let p = parse(
            "---\nhook: true\ndescription: lease keeper\n\
             on:\n  - session_start\n  - resume\n  - activity\n  - idle: 5m\n  - session_end\n\
             command: lease-tool touch\ntimeout: 30s\n---\nbody",
        );
        assert_eq!(p.def.description.as_deref(), Some("lease keeper"));
        assert_eq!(
            p.def.on,
            vec![
                HookOn::SessionStart,
                HookOn::Resume,
                HookOn::Activity,
                HookOn::Idle {
                    after_secs: Some(300)
                },
                HookOn::SessionEnd,
            ]
        );
        assert_eq!(p.def.command, "lease-tool touch");
        assert_eq!(p.def.timeout_secs, 30);
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
    }

    #[test]
    fn quoted_idle_and_bare_idle() {
        // Flow-list string form: `on: [activity, "idle: 90s"]`.
        let p = parse("---\nhook: true\non: [activity, \"idle: 90s\"]\ncommand: c\n---\n");
        assert_eq!(
            p.def.on,
            vec![
                HookOn::Activity,
                HookOn::Idle {
                    after_secs: Some(90)
                }
            ]
        );
        // Bare `idle` falls back to the configured default deadline.
        let p = parse("---\nhook: true\non: [idle]\ncommand: c\n---\n");
        assert_eq!(p.def.on, vec![HookOn::Idle { after_secs: None }]);
        // A scalar (non-list) `on` works too.
        let p = parse("---\nhook: true\non: session_end\ncommand: c\n---\n");
        assert_eq!(p.def.on, vec![HookOn::SessionEnd]);
    }

    #[test]
    fn missing_command_or_on_warns() {
        let p = parse("---\nhook: true\non: [activity]\n---\n");
        assert!(p.warnings.iter().any(|w| w.contains("command")));
        let p = parse("---\nhook: true\ncommand: c\n---\n");
        assert!(p.warnings.iter().any(|w| w.contains("never fire")));
        let p = parse("---\nhook: true\non: [flarp]\ncommand: c\n---\n");
        assert!(p.warnings.iter().any(|w| w.contains("flarp")));
        assert!(p.warnings.iter().any(|w| w.contains("no valid events")));
    }

    #[test]
    fn timeout_defaults_and_validates() {
        assert_eq!(
            parse("---\nhook: true\non: [activity]\ncommand: c\n---\n")
                .def
                .timeout_secs,
            DEFAULT_HOOK_TIMEOUT_SECS
        );
        let p = parse("---\nhook: true\non: [activity]\ncommand: c\ntimeout: soon\n---\n");
        assert_eq!(p.def.timeout_secs, DEFAULT_HOOK_TIMEOUT_SECS);
        assert!(p.warnings.iter().any(|w| w.contains("timeout")));
        assert_eq!(
            parse("---\nhook: true\non: [activity]\ncommand: c\ntimeout: 120\n---\n")
                .def
                .timeout_secs,
            120
        );
    }

    #[test]
    fn discovery_layouts_and_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path().join("outer");
        let inner = outer.join("inner");
        let user = tmp.path().join("user-hooks");
        let write = |p: &Path, c: &str| {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, c).unwrap();
        };
        write(
            &inner.join(".autofork/hooks/lease.md"),
            "---\nhook: true\non: [activity]\ncommand: inner\n---\n",
        );
        write(
            &outer.join(".autofork/hooks/lease/HOOK.md"),
            "---\nhook: true\non: [activity]\ncommand: outer\n---\n",
        );
        write(
            &user.join("cleanup.md"),
            "---\nhook: true\non: [session_end]\ncommand: user\n---\n",
        );
        // Companion notes are skipped silently.
        write(&inner.join(".autofork/hooks/notes.md"), "reference\n");

        let (entries, warnings) = discover_hooks(&inner, Some(&user));
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["lease", "cleanup"]);
        // Nearest root won the collision, with a shadow warning.
        assert_eq!(entries[0].parsed.def.command, "inner");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("shadowed"));
    }

    #[test]
    fn no_hooks_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let (entries, warnings) = discover_hooks(tmp.path(), None);
        assert!(entries.is_empty());
        assert!(warnings.is_empty());
    }
}
