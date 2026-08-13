//! The fork definition format: a markdown file with YAML frontmatter.
//!
//! Since v0.5 a fork MUST carry an explicit `fork: true` marker in its
//! frontmatter. `.autofork/forks/` may hold arbitrary companion `.md` files
//! (reference notes a fork body reads, etc.); only files marked `fork: true`
//! are forks. Files without the marker are skipped — silently for plain
//! notes, with a migration warning when they carry fork-like frontmatter.
//!
//! Supported top-level keys: `fork`, `description`, `run_on`, `throttle`,
//! `after`, `priority`, `overlap`, `tags`, `chain`, `chain_limit`, `gate`.
//! Unknown keys are ignored for
//! forward compatibility; invalid values warn and fall back rather than
//! dropping the fork. The keys `delivery`, `model`, `allowed_tools`,
//! `permission_mode` are parsed-and-ignored with a deprecation warning (v0.5
//! forks inherit the session's permissions and model; report delivery is
//! native). There is deliberately no RAG surface in this format.

use crate::duration::parse_duration_yaml;
use serde::Deserialize;

/// A parsed fork definition (frontmatter only; the body is the prompt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkDef {
    /// Human documentation only — autofork never feeds this to a model or an
    /// embedding index.
    pub description: Option<String>,
    /// The fork moments this fork fires at.
    pub run_on: Vec<ForkRunOn>,
    /// Minimum seconds between two runs of this fork within a session.
    pub throttle_secs: Option<u64>,
    /// Sequencing: run after these forks finish at the same moment (empty =
    /// independent).
    pub after: Vec<String>,
    /// Ordering weight across forks due at the same moment (z-index-like):
    /// forks run in ascending priority waves — a wave starts once every fork
    /// of the lower waves finished — and forks sharing a priority run in
    /// parallel. `after` wins over priority: a dependent never runs before
    /// its predecessors, whatever the numbers say. Default 0.
    pub priority: i64,
    /// Whether two runs of this fork may overlap in time. Off by default.
    pub overlap: bool,
    /// Free-form tags used by the per-session enable/disable filter. Empty
    /// when unset.
    pub tags: Vec<String>,
    /// `chain: true`: each run of this fork may request another by ending its
    /// report with the continue sentinel — the fork itself decides, per run,
    /// whether its goal needs more work. Off by default; without it the
    /// sentinel is ignored and the fork is never told about it.
    pub chain: bool,
    /// Maximum chain runs within one pause (safety cap). `None` falls back to
    /// the config default.
    pub chain_limit: Option<u64>,
    /// `gate: true`: while this fork is running (and, with `chain: true`,
    /// while its chain is unsettled), every other idle-triggered fork of the
    /// session is held back; when it settles, the pause baseline resets so
    /// the held forks' idle deadlines measure from that moment. The "goal
    /// fork" key: goal work first, consolidation forks after.
    pub gate: bool,
}

impl Default for ForkDef {
    fn default() -> Self {
        Self {
            description: None,
            run_on: default_run_on(),
            throttle_secs: None,
            after: Vec::new(),
            priority: 0,
            overlap: false,
            tags: Vec::new(),
            chain: false,
            chain_limit: None,
            gate: false,
        }
    }
}

/// The default fork moment when `run_on` is absent: every idle pause at the
/// default idle deadline. (Before v0.5 this also included `compact`, which no
/// longer exists.)
pub fn default_run_on() -> Vec<ForkRunOn> {
    vec![ForkRunOn::Idle { after_secs: None }]
}

/// A fork moment a fork fires at (`run_on`).
///
/// Only `Idle` and the three `Context*` variants are *supported* in v0.5 (see
/// [`ForkRunOn::is_supported`]); the rest are recognized (so a migration
/// warning can be produced) but never fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkRunOn {
    /// An idle pause. With `after_secs` unset, fires at the configured
    /// default idle deadline; with it set (`- idle: 20m`), fires once the
    /// session has been idle that long.
    Idle { after_secs: Option<u64> },
    /// The session's prompt token count reached this absolute value.
    ContextTokens(u64),
    /// The session used at least this percentage of the model's context
    /// window.
    ContextUsedPct(u8),
    /// At most this many tokens remain in the model's context window.
    ContextLeft(u64),
    /// At least this many seconds since the fork's last run (or, before the
    /// first run, since the session began) — evaluated at every turn
    /// boundary, and on opencode also mid-run. Unlike `idle`, it does not
    /// wait for a pause: the first evaluation point past the interval fires.
    Every { interval_secs: u64 },
    /// Context compaction. Not supported since v0.5 (never fires).
    Compact,
    /// A new session starting. Not supported since v0.5 (never fires).
    SessionStart,
    /// The session ending. Not supported since v0.5 (never fires).
    SessionEnd,
    /// The session ending while recently active. Not supported since v0.5.
    ManualStop,
    /// Daemon startup found this session. Not supported since v0.5.
    Boot,
}

impl ForkRunOn {
    /// Stable label used in wake payloads and (for context triggers) as the
    /// once-per-session latch key.
    pub fn label(&self) -> String {
        match self {
            ForkRunOn::Idle { after_secs: None } => "idle".into(),
            ForkRunOn::Idle {
                after_secs: Some(s),
            } => format!("idle:{s}"),
            ForkRunOn::ContextTokens(n) => format!("context_tokens:{n}"),
            ForkRunOn::ContextUsedPct(p) => format!("context_used:{p}%"),
            ForkRunOn::ContextLeft(n) => format!("context_left:{n}"),
            ForkRunOn::Every { interval_secs } => format!("every:{interval_secs}"),
            ForkRunOn::Compact => "compact".into(),
            ForkRunOn::SessionStart => "session_start".into(),
            ForkRunOn::SessionEnd => "session_end".into(),
            ForkRunOn::ManualStop => "manual_stop".into(),
            ForkRunOn::Boot => "boot".into(),
        }
    }

    /// Whether this trigger can fire under the v0.5 wake-and-spawn model.
    pub fn is_supported(&self) -> bool {
        matches!(
            self,
            ForkRunOn::Idle { .. }
                | ForkRunOn::ContextTokens(_)
                | ForkRunOn::ContextUsedPct(_)
                | ForkRunOn::ContextLeft(_)
                | ForkRunOn::Every { .. }
        )
    }
}

fn parse_percent(v: &serde_yaml::Value) -> Option<u8> {
    let n = match v {
        serde_yaml::Value::Number(n) => n.as_u64()?,
        serde_yaml::Value::String(s) => s.trim().trim_end_matches('%').trim().parse().ok()?,
        _ => return None,
    };
    (1..=100).contains(&n).then_some(n as u8)
}

fn parse_token_count(v: &serde_yaml::Value) -> Option<u64> {
    match v {
        serde_yaml::Value::Number(n) => n.as_u64(),
        serde_yaml::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Parse one `run_on` entry: a plain string (`idle`, `compact`, …) or a
/// single-key map (`idle: 20m`, `context_tokens: 150000`, …).
fn parse_run_on_entry(v: &serde_yaml::Value, warnings: &mut Vec<String>) -> Option<ForkRunOn> {
    if let serde_yaml::Value::String(s) = v {
        return match s.as_str() {
            "idle" => Some(ForkRunOn::Idle { after_secs: None }),
            "compact" | "compaction" => Some(ForkRunOn::Compact),
            "session_start" => Some(ForkRunOn::SessionStart),
            "session_end" => Some(ForkRunOn::SessionEnd),
            "manual_stop" => Some(ForkRunOn::ManualStop),
            "boot" => Some(ForkRunOn::Boot),
            other => {
                warnings.push(format!("unknown run_on trigger '{other}', skipping"));
                None
            }
        };
    }
    if let serde_yaml::Value::Mapping(m) = v {
        if m.len() == 1 {
            if let Some((serde_yaml::Value::String(key), val)) = m.iter().next() {
                let parsed = match key.as_str() {
                    "idle" => parse_duration_yaml(val).map(|s| ForkRunOn::Idle {
                        after_secs: Some(s),
                    }),
                    "every" => parse_duration_yaml(val)
                        .filter(|&s| s > 0)
                        .map(|s| ForkRunOn::Every { interval_secs: s }),
                    "context_tokens" => parse_token_count(val).map(ForkRunOn::ContextTokens),
                    "context_used" => parse_percent(val).map(ForkRunOn::ContextUsedPct),
                    "context_left" => parse_token_count(val).map(ForkRunOn::ContextLeft),
                    other => {
                        warnings.push(format!("unknown run_on trigger '{other}', skipping"));
                        return None;
                    }
                };
                if parsed.is_none() {
                    warnings.push(format!("invalid run_on value for '{key}', skipping"));
                }
                return parsed;
            }
        }
    }
    warnings.push("malformed run_on entry, skipping".into());
    None
}

/// Parse one `after` entry: a plain fork-name string, or a map
/// `{fork: <name>}` (`skill:` accepted as an alias for the key, for
/// definitions shared with other tools). A `context:` sub-key is accepted for
/// backward compatibility but ignored with a warning (v0.5 dependents inherit
/// the parent session, never a predecessor's).
fn parse_after_entry(
    v: &serde_yaml::Value,
    name: &str,
    warnings: &mut Vec<String>,
) -> Option<String> {
    match v {
        serde_yaml::Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        serde_yaml::Value::Mapping(m) => {
            let get = |k: &str| {
                m.get(serde_yaml::Value::String(k.into()))
                    .and_then(|v| v.as_str())
            };
            if get("context").is_some() {
                warnings.push(format!(
                    "fork '{name}': after 'context' is ignored since v0.5 (dependents inherit the parent session)"
                ));
            }
            let dep = get("fork")
                .or_else(|| get("skill"))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            if dep.is_none() {
                warnings.push(format!(
                    "fork '{name}': after entry is missing 'fork', ignoring"
                ));
            }
            dep
        }
        _ => {
            warnings.push(format!("fork '{name}': malformed after value, ignoring"));
            None
        }
    }
}

/// Parse the full `after` value: one entry, or a list of entries. Duplicates
/// and self-references are dropped.
fn parse_after(v: &serde_yaml::Value, name: &str, warnings: &mut Vec<String>) -> Vec<String> {
    let entries: Vec<String> = match v {
        serde_yaml::Value::Sequence(seq) => seq
            .iter()
            .filter_map(|e| parse_after_entry(e, name, warnings))
            .collect(),
        other => parse_after_entry(other, name, warnings)
            .into_iter()
            .collect(),
    };
    let mut out: Vec<String> = Vec::new();
    for dep in entries {
        if dep == name {
            warnings.push(format!("fork '{name}': after references itself, ignoring"));
            continue;
        }
        if out.iter().any(|e| e == &dep) {
            warnings.push(format!(
                "fork '{name}': duplicate after entry '{dep}', ignoring"
            ));
            continue;
        }
        out.push(dep);
    }
    out
}

/// Parse the `tags` value: a scalar string (comma-split) or a list of
/// strings (each still comma-split for convenience). Entries are trimmed,
/// empties dropped, and duplicates removed with the first occurrence kept.
fn parse_tags(v: &serde_yaml::Value, name: &str, warnings: &mut Vec<String>) -> Vec<String> {
    fn push_split(s: &str, out: &mut Vec<String>) {
        for piece in s.split(',') {
            let t = piece.trim();
            if !t.is_empty() && !out.iter().any(|e| e == t) {
                out.push(t.to_string());
            }
        }
    }
    let mut out: Vec<String> = Vec::new();
    match v {
        serde_yaml::Value::String(s) => push_split(s, &mut out),
        serde_yaml::Value::Sequence(seq) => {
            for entry in seq {
                match entry {
                    serde_yaml::Value::String(s) => push_split(s, &mut out),
                    _ => warnings.push(format!(
                        "fork '{name}': tags entry is not a string, skipping"
                    )),
                }
            }
        }
        serde_yaml::Value::Null => {}
        _ => warnings.push(format!(
            "fork '{name}': tags must be a string or a list of strings, ignoring"
        )),
    }
    out
}

#[derive(Deserialize, Default)]
struct RawFork {
    // The v0.5 fork marker (`fork: true`). Absent or `false` = not a fork.
    #[serde(default)]
    fork: Option<serde_yaml::Value>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    run_on: Option<Vec<serde_yaml::Value>>,
    #[serde(default)]
    throttle: Option<serde_yaml::Value>,
    #[serde(default)]
    after: Option<serde_yaml::Value>,
    #[serde(default)]
    priority: Option<serde_yaml::Value>,
    #[serde(default)]
    overlap: Option<serde_yaml::Value>,
    #[serde(default)]
    tags: Option<serde_yaml::Value>,
    #[serde(default)]
    chain: Option<serde_yaml::Value>,
    #[serde(default)]
    chain_limit: Option<serde_yaml::Value>,
    #[serde(default)]
    gate: Option<serde_yaml::Value>,
    // Deprecated since v0.5: parsed only to warn, then ignored.
    #[serde(default)]
    delivery: Option<serde_yaml::Value>,
    #[serde(default)]
    model: Option<serde_yaml::Value>,
    #[serde(default)]
    allowed_tools: Option<serde_yaml::Value>,
    #[serde(default)]
    permission_mode: Option<serde_yaml::Value>,
    // Recognized-and-rejected: the format has no RAG surface.
    #[serde(default)]
    rag: Option<serde_yaml::Value>,
    #[serde(default)]
    triggers: Option<serde_yaml::Value>,
}

impl RawFork {
    /// True if `fork: true`.
    fn is_marked(&self) -> bool {
        matches!(&self.fork, Some(serde_yaml::Value::Bool(true)))
    }

    /// True if the frontmatter carries any key that only a fork would set — a
    /// likely `fork: true` migration mistake when the marker is absent.
    fn has_fork_like_keys(&self) -> bool {
        self.description.is_some()
            || self.run_on.is_some()
            || self.throttle.is_some()
            || self.after.is_some()
            || self.priority.is_some()
            || self.overlap.is_some()
            || self.tags.is_some()
            || self.chain.is_some()
            || self.chain_limit.is_some()
            || self.gate.is_some()
            || self.delivery.is_some()
            || self.model.is_some()
            || self.allowed_tools.is_some()
            || self.permission_mode.is_some()
    }
}

/// The result of parsing a fork definition (frontmatter only; the body is the
/// prompt), plus any warnings produced by fallbacks.
#[derive(Debug, Clone)]
pub struct ParsedFork {
    pub def: ForkDef,
    pub body: String,
    pub warnings: Vec<String>,
}

/// The outcome of parsing a `.md` file in a forks tree.
#[derive(Debug, Clone)]
pub enum ForkParse {
    /// A real fork (frontmatter carried `fork: true`).
    Fork(ParsedFork),
    /// Not a fork: no `fork: true`. `fork_like` is true when the frontmatter
    /// nevertheless carries fork keys (a likely migration mistake worth
    /// warning about); false for a plain companion note or explicit
    /// `fork: false`.
    NotFork { fork_like: bool },
    /// A frontmatter block is present but is not valid YAML.
    Invalid,
}

/// Split a markdown file into its `---`-delimited YAML frontmatter and body.
/// Files without frontmatter are valid: their whole content is the body.
/// (Shared with the lifecycle-hook definition parser.)
pub(crate) fn split_frontmatter(content: &str) -> (Option<&str>, &str) {
    let Some(rest) = content.strip_prefix("---") else {
        return (None, content);
    };
    let Some(rest) = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))
    else {
        return (None, content);
    };
    let mut search_from = 0;
    while let Some(pos) = rest[search_from..].find("\n---") {
        let abs = search_from + pos;
        let front = rest[..abs].strip_suffix('\r').unwrap_or(&rest[..abs]);
        let tail = &rest[abs + 4..];
        let tail = tail.strip_prefix('\r').unwrap_or(tail);
        if tail.is_empty() {
            return (Some(front), "");
        }
        if let Some(body) = tail.strip_prefix('\n') {
            return (Some(front), body);
        }
        search_from = abs + 4;
    }
    (None, content)
}

/// Parse a fork file's full content. `name` is used only for warning text.
pub fn parse_fork_file(name: &str, content: &str) -> ForkParse {
    let (front, body) = split_frontmatter(content);
    let raw: RawFork = match front {
        // No frontmatter at all: never a fork, and never fork-like.
        None => return ForkParse::NotFork { fork_like: false },
        Some(yaml) => match serde_yaml::from_str(yaml) {
            Ok(raw) => raw,
            Err(e) => {
                tracing::debug!(fork = name, error = %e, "invalid fork frontmatter YAML");
                return ForkParse::Invalid;
            }
        },
    };

    // The v0.5 marker gate: only `fork: true` files are forks.
    if !raw.is_marked() {
        return ForkParse::NotFork {
            // `fork: false` is an explicit opt-out (no warning); an absent
            // marker on a file with fork keys is a likely migration mistake.
            fork_like: !matches!(&raw.fork, Some(serde_yaml::Value::Bool(false)))
                && raw.has_fork_like_keys(),
        };
    }

    let mut warnings = Vec::new();

    if raw.rag.is_some() || raw.triggers.is_some() {
        warnings.push(format!(
            "fork '{name}': RAG keys are not part of the fork format, ignoring"
        ));
    }

    // Deprecated-and-ignored keys.
    if raw.delivery.is_some() || raw.model.is_some() {
        warnings.push(format!(
            "fork '{name}': v0.5 ignores 'delivery' and 'model' — delivery is native \
             and the fork inherits the session's model"
        ));
    }
    if raw.allowed_tools.is_some() || raw.permission_mode.is_some() {
        warnings.push(format!(
            "fork '{name}': v0.5 ignores 'allowed_tools' and 'permission_mode' — a fork \
             inherits the session's permissions"
        ));
    }

    let throttle_secs = match &raw.throttle {
        None => None,
        Some(v) => {
            let parsed = parse_duration_yaml(v).filter(|s| *s > 0);
            if parsed.is_none() {
                warnings.push(format!("fork '{name}': invalid throttle value, ignoring"));
            }
            parsed
        }
    };

    let run_on = match raw.run_on {
        None => default_run_on(),
        Some(entries) => {
            let parsed: Vec<ForkRunOn> = entries
                .iter()
                .filter_map(|e| parse_run_on_entry(e, &mut warnings))
                .collect();
            if parsed.is_empty() {
                warnings.push(format!(
                    "fork '{name}': run_on has no valid triggers; using the defaults"
                ));
                default_run_on()
            } else {
                parsed
            }
        }
    };

    // Warn about unsupported moments; a fork with only unsupported ones will
    // never fire.
    let mut seen_unsupported: Vec<String> = Vec::new();
    for trigger in &run_on {
        if !trigger.is_supported() {
            let label = trigger.label();
            if !seen_unsupported.iter().any(|l| l == &label) {
                warnings.push(format!(
                    "fork '{name}': run_on '{label}' is not supported since v0.5, ignoring"
                ));
                seen_unsupported.push(label);
            }
        }
    }
    if !run_on.iter().any(|t| t.is_supported()) {
        warnings.push(format!(
            "fork '{name}': no supported run_on moments (idle / context_*); it will never fire"
        ));
    }

    let after = raw
        .after
        .as_ref()
        .map(|v| parse_after(v, name, &mut warnings))
        .unwrap_or_default();

    let priority = match &raw.priority {
        None => 0,
        Some(serde_yaml::Value::Number(n)) if n.as_i64().is_some() => n.as_i64().unwrap(),
        Some(serde_yaml::Value::String(s)) if s.trim().parse::<i64>().is_ok() => {
            s.trim().parse().unwrap()
        }
        Some(_) => {
            warnings.push(format!(
                "fork '{name}': priority must be an integer; using 0"
            ));
            0
        }
    };

    let overlap = match &raw.overlap {
        None => false,
        Some(serde_yaml::Value::Bool(b)) => *b,
        Some(_) => {
            warnings.push(format!(
                "fork '{name}': overlap must be true or false; using false"
            ));
            false
        }
    };

    let chain = match &raw.chain {
        None => false,
        Some(serde_yaml::Value::Bool(b)) => *b,
        Some(_) => {
            warnings.push(format!(
                "fork '{name}': chain must be true or false; using false"
            ));
            false
        }
    };

    let chain_limit = match &raw.chain_limit {
        None => None,
        Some(v) => {
            let parsed = parse_token_count(v).filter(|n| *n > 0);
            if parsed.is_none() {
                warnings.push(format!(
                    "fork '{name}': chain_limit must be a positive integer, ignoring"
                ));
            }
            parsed
        }
    };
    if chain_limit.is_some() && !chain {
        warnings.push(format!(
            "fork '{name}': chain_limit without chain: true has no effect"
        ));
    }

    let gate = match &raw.gate {
        None => false,
        Some(serde_yaml::Value::Bool(b)) => *b,
        Some(_) => {
            warnings.push(format!(
                "fork '{name}': gate must be true or false; using false"
            ));
            false
        }
    };

    let tags = raw
        .tags
        .as_ref()
        .map(|v| parse_tags(v, name, &mut warnings))
        .unwrap_or_default();

    for w in &warnings {
        tracing::warn!(fork = name, "{w}");
    }

    ForkParse::Fork(ParsedFork {
        def: ForkDef {
            description: raw.description.filter(|d| !d.trim().is_empty()),
            run_on,
            throttle_secs,
            after,
            priority,
            overlap,
            tags,
            chain,
            chain_limit,
            gate,
        },
        body: body.to_string(),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> ParsedFork {
        match parse_fork_file("test", content) {
            ForkParse::Fork(p) => p,
            other => panic!("expected a fork, got {other:?}"),
        }
    }

    #[test]
    fn marker_required_absent_plain_note_is_silent() {
        // A companion note with no frontmatter is silently not a fork.
        assert!(matches!(
            parse_fork_file("note", "just reference material\n"),
            ForkParse::NotFork { fork_like: false }
        ));
        // Frontmatter without the marker and without fork-like keys: silent.
        assert!(matches!(
            parse_fork_file("note", "---\ntitle: notes\n---\nbody"),
            ForkParse::NotFork { fork_like: false }
        ));
    }

    #[test]
    fn marker_absent_with_fork_like_keys_warns() {
        assert!(matches!(
            parse_fork_file("j", "---\nrun_on: [idle]\n---\nbody"),
            ForkParse::NotFork { fork_like: true }
        ));
        assert!(matches!(
            parse_fork_file("j", "---\ndescription: hi\nthrottle: 1h\n---\nb"),
            ForkParse::NotFork { fork_like: true }
        ));
    }

    #[test]
    fn fork_false_is_silent_optout() {
        assert!(matches!(
            parse_fork_file("j", "---\nfork: false\nrun_on: [idle]\n---\nbody"),
            ForkParse::NotFork { fork_like: false }
        ));
    }

    #[test]
    fn marker_present_is_a_fork() {
        let p = parse("---\nfork: true\ndescription: hi\n---\nbody");
        assert_eq!(p.def.description.as_deref(), Some("hi"));
        assert_eq!(p.def.run_on, default_run_on());
        assert_eq!(p.body, "body");
    }

    #[test]
    fn default_run_on_is_just_idle() {
        assert_eq!(default_run_on(), vec![ForkRunOn::Idle { after_secs: None }]);
        let p = parse("---\nfork: true\n---\n");
        assert_eq!(p.def.run_on, vec![ForkRunOn::Idle { after_secs: None }]);
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn full_config() {
        let p = parse(
            "---\n\
             fork: true\n\
             description: d\n\
             run_on:\n  - idle: 10m\n  - context_used: 80%\n\
             throttle: 30m\n\
             after: journal\n\
             tags: [ci, review]\n\
             overlap: true\n\
             ---\nbody",
        );
        assert_eq!(
            p.def.run_on,
            vec![
                ForkRunOn::Idle {
                    after_secs: Some(600)
                },
                ForkRunOn::ContextUsedPct(80),
            ]
        );
        assert_eq!(p.def.throttle_secs, Some(1800));
        assert_eq!(p.def.after, vec!["journal".to_string()]);
        assert_eq!(p.def.tags, vec!["ci".to_string(), "review".to_string()]);
        assert!(p.def.overlap);
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn unsupported_moment_warns_and_only_unsupported_never_fires() {
        // idle stays supported; compact warns but the fork still fires on idle.
        let p = parse("---\nfork: true\nrun_on: [idle, compact]\n---\n");
        assert_eq!(
            p.def.run_on,
            vec![ForkRunOn::Idle { after_secs: None }, ForkRunOn::Compact]
        );
        assert!(p.warnings.iter().any(|w| w.contains("compact")));
        assert!(!p.warnings.iter().any(|w| w.contains("never fire")));

        // Only unsupported moments: the "never fire" warning appears.
        let p = parse("---\nfork: true\nrun_on: [session_end, boot]\n---\n");
        assert!(p.warnings.iter().any(|w| w.contains("never fire")));
    }

    #[test]
    fn context_thresholds() {
        let p = parse(
            "---\nfork: true\nrun_on:\n  - context_tokens: 150000\n  - context_used: 80%\n  - context_left: 20000\n---\n",
        );
        assert_eq!(
            p.def.run_on,
            vec![
                ForkRunOn::ContextTokens(150000),
                ForkRunOn::ContextUsedPct(80),
                ForkRunOn::ContextLeft(20000),
            ]
        );
        assert_eq!(p.def.run_on[1].label(), "context_used:80%");
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn deprecated_keys_warn_and_are_ignored() {
        let p = parse(
            "---\nfork: true\nmodel: haiku\ndelivery: discard\nallowed_tools: [Write]\npermission_mode: acceptEdits\n---\n",
        );
        assert!(p.warnings.iter().any(|w| w.contains("delivery")));
        assert!(p.warnings.iter().any(|w| w.contains("allowed_tools")));
    }

    #[test]
    fn after_forms_names_only() {
        let p = parse("---\nfork: true\nafter: [a, {fork: b}, c]\n---\n");
        assert_eq!(p.def.after, vec!["a", "b", "c"]);
        assert!(p.warnings.is_empty());
        // `skill:` alias; `context:` is ignored with a warning.
        let p = parse("---\nfork: true\nafter: {skill: journal, context: fork}\n---\n");
        assert_eq!(p.def.after, vec!["journal".to_string()]);
        assert!(p.warnings.iter().any(|w| w.contains("context")));
    }

    #[test]
    fn after_self_and_dup_dropped() {
        // Duplicate 'a' dropped (name is "test", so neither entry is a self-ref).
        let p = parse("---\nfork: true\nafter: [a, b, a]\n---\n");
        assert_eq!(p.def.after, vec!["a".to_string(), "b".to_string()]);
        // Self-reference dropped, keeping the rest.
        let p = match parse_fork_file("me", "---\nfork: true\nafter: [other, me]\n---\n") {
            ForkParse::Fork(p) => p,
            _ => panic!(),
        };
        assert_eq!(p.def.after, vec!["other".to_string()]);
        assert_eq!(p.warnings.len(), 1);
    }

    #[test]
    fn tags_scalar_and_list() {
        assert_eq!(
            parse("---\nfork: true\ntags: ci\n---\n").def.tags,
            vec!["ci"]
        );
        assert_eq!(
            parse("---\nfork: true\ntags: ci, review ,docs\n---\n")
                .def
                .tags,
            vec!["ci", "review", "docs"]
        );
        assert_eq!(
            parse("---\nfork: true\ntags: [ci, review]\n---\n").def.tags,
            vec!["ci", "review"]
        );
    }

    #[test]
    fn overlap_parsing() {
        assert!(!parse("---\nfork: true\ndescription: d\n---\n").def.overlap);
        assert!(parse("---\nfork: true\noverlap: true\n---\n").def.overlap);
        let p = parse("---\nfork: true\noverlap: sometimes\n---\n");
        assert!(!p.def.overlap);
        assert!(p.warnings.iter().any(|w| w.contains("overlap")));
    }

    #[test]
    fn priority_parsing() {
        assert_eq!(parse("---\nfork: true\n---\n").def.priority, 0);
        assert_eq!(
            parse("---\nfork: true\npriority: 100\n---\n").def.priority,
            100
        );
        assert_eq!(
            parse("---\nfork: true\npriority: -5\n---\n").def.priority,
            -5
        );
        assert_eq!(
            parse("---\nfork: true\npriority: \"7\"\n---\n")
                .def
                .priority,
            7
        );
        let p = parse("---\nfork: true\npriority: high\n---\n");
        assert_eq!(p.def.priority, 0);
        assert!(p.warnings.iter().any(|w| w.contains("priority")));
    }

    #[test]
    fn chain_parsing() {
        assert!(!parse("---\nfork: true\n---\n").def.chain);
        let p = parse("---\nfork: true\nchain: true\nchain_limit: 10\n---\n");
        assert!(p.def.chain);
        assert_eq!(p.def.chain_limit, Some(10));
        assert!(p.warnings.is_empty());
        // Invalid values warn and fall back.
        let p = parse("---\nfork: true\nchain: always\n---\n");
        assert!(!p.def.chain);
        assert!(p.warnings.iter().any(|w| w.contains("chain must be")));
        let p = parse("---\nfork: true\nchain: true\nchain_limit: 0\n---\n");
        assert_eq!(p.def.chain_limit, None);
        assert!(p.warnings.iter().any(|w| w.contains("chain_limit")));
        // chain_limit without chain: true is called out.
        let p = parse("---\nfork: true\nchain_limit: 5\n---\n");
        assert!(p.warnings.iter().any(|w| w.contains("has no effect")));
    }

    #[test]
    fn chain_alone_is_fork_like() {
        assert!(matches!(
            parse_fork_file("x", "---\nchain: true\n---\n"),
            ForkParse::NotFork { fork_like: true }
        ));
    }

    #[test]
    fn gate_parsing() {
        assert!(!parse("---\nfork: true\n---\n").def.gate);
        let p = parse("---\nfork: true\nchain: true\ngate: true\n---\n");
        assert!(p.def.gate);
        assert!(p.warnings.is_empty());
        // gate without chain is valid: it holds others until the run finishes.
        let p = parse("---\nfork: true\ngate: true\n---\n");
        assert!(p.def.gate);
        assert!(p.warnings.is_empty());
        let p = parse("---\nfork: true\ngate: maybe\n---\n");
        assert!(!p.def.gate);
        assert!(p.warnings.iter().any(|w| w.contains("gate must be")));
    }

    #[test]
    fn priority_alone_is_fork_like() {
        // A file with only `priority:` and no marker should trip the
        // forgotten-marker guard rail.
        assert!(matches!(
            parse_fork_file("x", "---\npriority: 3\n---\n"),
            ForkParse::NotFork { fork_like: true }
        ));
    }

    #[test]
    fn invalid_throttle_ignored() {
        let p = parse("---\nfork: true\nthrottle: soon\n---\n");
        assert_eq!(p.def.throttle_secs, None);
        assert!(p.warnings.iter().any(|w| w.contains("throttle")));
    }

    #[test]
    fn unknown_run_on_falls_back_to_defaults() {
        let p = parse("---\nfork: true\nrun_on: [flarp]\n---\n");
        assert_eq!(p.def.run_on, default_run_on());
        assert!(p.warnings.iter().any(|w| w.contains("flarp")));
    }

    #[test]
    fn rag_keys_rejected_with_warning() {
        let p = parse("---\nfork: true\ntriggers: [x]\n---\n");
        assert!(p.warnings.iter().any(|w| w.contains("RAG")));
    }

    #[test]
    fn unknown_keys_ignored() {
        let p = parse("---\nfork: true\nfuture_key: whatever\ndescription: d\n---\nb");
        assert!(p.warnings.is_empty());
        assert_eq!(p.def.description.as_deref(), Some("d"));
    }

    #[test]
    fn invalid_yaml_is_invalid() {
        assert!(matches!(
            parse_fork_file("x", "---\n: [unclosed\n---\nb"),
            ForkParse::Invalid
        ));
    }

    #[test]
    fn frontmatter_closed_at_eof() {
        let p = parse("---\nfork: true\ndescription: d\n---");
        assert_eq!(p.def.description.as_deref(), Some("d"));
        assert_eq!(p.body, "");
    }

    #[test]
    fn every_trigger_parses() {
        let p = match parse_fork_file(
            "periodic",
            "---\nfork: true\nrun_on:\n  - every: 1h\n  - idle: 4m\n---\nbody",
        ) {
            ForkParse::Fork(p) => p,
            other => panic!("expected fork, got {other:?}"),
        };
        assert_eq!(
            p.def.run_on[0],
            ForkRunOn::Every {
                interval_secs: 3600
            }
        );
        assert_eq!(p.def.run_on[0].label(), "every:3600");
        assert!(p.def.run_on[0].is_supported());

        // Zero intervals are rejected with a warning.
        let z = match parse_fork_file("zero", "---\nfork: true\nrun_on:\n  - every: 0s\n---\nbody")
        {
            ForkParse::Fork(p) => p,
            other => panic!("expected fork, got {other:?}"),
        };
        assert!(z.def.run_on.is_empty() || z.def.run_on[0] == ForkRunOn::Idle { after_secs: None });
        assert!(z.warnings.iter().any(|w| w.contains("every")));
    }
}
