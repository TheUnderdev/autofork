//! Configuration: `<project_root>/.autofork/config.toml` overrides
//! `~/.autofork/config.toml` overrides built-in defaults. Keys that only make
//! sense globally are ignored (with a warning) when set at project level.
//!
//! v0.5 removed a batch of keys tied to the old subprocess runner
//! (`claude_bin`, `concurrency`, `isolation`, `permission_mode`,
//! `run_timeout`/`fork_timeout`, `context_window`, `[models]`, and the
//! report/poll budgets). They are accepted-and-warned for backward
//! compatibility, then ignored — an old config file never hard-errors.

use crate::duration::parse_duration_str;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Effective configuration after layering.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Idle deadline for bare `idle` triggers (seconds). 0 disables them.
    pub default_idle_deadline_secs: u64,
    /// A session idle longer than this is closed by the session reaper.
    pub session_timeout_secs: u64,
    /// The daemon exits after this long with nothing to do (seconds).
    pub quiet_period_secs: u64,
    /// After the first fork comes due for a parked session, wait this long
    /// before answering so near-simultaneous forks batch into one wake
    /// (seconds). 0 answers immediately.
    pub wake_debounce_secs: u64,
    /// Default enable (whitelist) tag filter; a session's own filter (from
    /// the hook env vars) overrides it. `None` = unset.
    pub enable_tags: Option<Vec<String>>,
    /// Default disable (blocklist) tag filter; a session's own filter
    /// overrides it. `None` = unset.
    pub disable_tags: Option<Vec<String>>,
    /// Per-tag shared throttle (seconds): a minimum gap between wakes of any
    /// fork carrying that tag, across the project.
    pub tag_throttles: BTreeMap<String, u64>,
    /// Default cap on chain runs of one fork within one pause, for `chain:
    /// true` forks that don't set their own `chain_limit`. The belt against a
    /// fork that never stops asking for another run.
    pub chain_limit: u64,
    /// Runaway breaker: max wakes of one fork per session per rolling hour,
    /// counted against wall-clock — immune to pause resets, session resumes,
    /// and duplicated client event streams (the failure modes that defeat the
    /// per-pause counters). `every:` triggers are exempt (their interval is an
    /// explicit contract). 0 disables. Tripping it is loud (a warn log), never
    /// silent.
    pub runaway_limit: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_idle_deadline_secs: 600,
            session_timeout_secs: 12 * 3600,
            quiet_period_secs: 20 * 60,
            wake_debounce_secs: 5,
            enable_tags: None,
            disable_tags: None,
            tag_throttles: BTreeMap::new(),
            chain_limit: 25,
            runaway_limit: 30,
        }
    }
}

/// Raw TOML shape; every key optional so layers can be sparse. Deprecated
/// keys are still declared so their presence can be warned about.
#[derive(Debug, Clone, Default, Deserialize)]
struct RawConfig {
    default_idle_deadline: Option<toml::Value>,
    session_timeout: Option<toml::Value>,
    quiet_period: Option<toml::Value>,
    wake_debounce: Option<toml::Value>,
    enable_tags: Option<Vec<String>>,
    disable_tags: Option<Vec<String>>,
    #[serde(default)]
    tag_throttles: BTreeMap<String, toml::Value>,
    chain_limit: Option<toml::Value>,
    runaway_limit: Option<toml::Value>,
    // ---- deprecated since v0.5: accepted, warned, ignored ----
    concurrency: Option<toml::Value>,
    fork_timeout: Option<toml::Value>,
    run_timeout: Option<toml::Value>,
    claude_bin: Option<toml::Value>,
    context_window: Option<toml::Value>,
    #[serde(default)]
    models: BTreeMap<String, toml::Value>,
    report_ttl: Option<toml::Value>,
    poll_budget_chars: Option<toml::Value>,
    permission_mode: Option<toml::Value>,
    isolation: Option<toml::Value>,
}

/// Keys ignored at project level (global-only concerns).
const GLOBAL_ONLY: &[&str] = &["quiet_period"];

fn parse_toml_duration(v: &toml::Value, key: &str, warnings: &mut Vec<String>) -> Option<u64> {
    let parsed = match v {
        toml::Value::Integer(n) if *n >= 0 => Some(*n as u64),
        toml::Value::String(s) => parse_duration_str(s),
        _ => None,
    };
    if parsed.is_none() {
        warnings.push(format!("invalid duration for '{key}', ignoring"));
    }
    parsed
}

fn apply_layer(cfg: &mut Config, raw: RawConfig, project_level: bool, warnings: &mut Vec<String>) {
    let mut dur = |v: &Option<toml::Value>, key: &str| -> Option<u64> {
        if project_level && GLOBAL_ONLY.contains(&key) && v.is_some() {
            warnings.push(format!("'{key}' is global-only; ignoring project value"));
            return None;
        }
        v.as_ref()
            .and_then(|v| parse_toml_duration(v, key, warnings))
    };
    if let Some(s) = dur(&raw.default_idle_deadline, "default_idle_deadline") {
        cfg.default_idle_deadline_secs = s;
    }
    if let Some(s) = dur(&raw.session_timeout, "session_timeout") {
        cfg.session_timeout_secs = s;
    }
    if let Some(s) = dur(&raw.quiet_period, "quiet_period") {
        cfg.quiet_period_secs = s;
    }
    if let Some(s) = dur(&raw.wake_debounce, "wake_debounce") {
        cfg.wake_debounce_secs = s;
    }
    if let Some(v) = raw.enable_tags {
        cfg.enable_tags = Some(v);
    }
    if let Some(v) = raw.disable_tags {
        cfg.disable_tags = Some(v);
    }
    if let Some(v) = &raw.chain_limit {
        match v {
            toml::Value::Integer(n) if *n > 0 => cfg.chain_limit = *n as u64,
            _ => warnings.push("'chain_limit' must be a positive integer, ignoring".into()),
        }
    }
    if let Some(v) = &raw.runaway_limit {
        match v {
            // 0 is legal: it disables the breaker.
            toml::Value::Integer(n) if *n >= 0 => cfg.runaway_limit = *n as u64,
            _ => warnings.push("'runaway_limit' must be a non-negative integer, ignoring".into()),
        }
    }
    // Like `models` once did: per-key extend so a project layer overrides the
    // home layer for the tags it names and inherits the rest.
    for (tag, v) in raw.tag_throttles {
        let key = format!("tag_throttles.{tag}");
        if let Some(secs) = parse_toml_duration(&v, &key, warnings) {
            cfg.tag_throttles.insert(tag, secs);
        }
    }

    // Deprecated keys: warn once each, ignore the value.
    let mut deprecated = |present: bool, key: &str| {
        if present {
            warnings.push(format!(
                "'{key}' is no longer used since v0.5 (forks run as fork subagents), ignoring"
            ));
        }
    };
    deprecated(raw.concurrency.is_some(), "concurrency");
    deprecated(raw.fork_timeout.is_some(), "fork_timeout");
    deprecated(raw.run_timeout.is_some(), "run_timeout");
    deprecated(raw.claude_bin.is_some(), "claude_bin");
    deprecated(raw.context_window.is_some(), "context_window");
    deprecated(!raw.models.is_empty(), "models");
    deprecated(raw.report_ttl.is_some(), "report_ttl");
    deprecated(raw.poll_budget_chars.is_some(), "poll_budget_chars");
    deprecated(raw.permission_mode.is_some(), "permission_mode");
    deprecated(raw.isolation.is_some(), "isolation");
}

fn load_layer(path: &Path, warnings: &mut Vec<String>) -> Option<RawConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    match toml::from_str(&content) {
        Ok(raw) => Some(raw),
        Err(e) => {
            warnings.push(format!("invalid config at {}: {e}", path.display()));
            None
        }
    }
}

/// Load the effective config for a project. `home` is the user's home dir.
pub fn load_config(project_root: Option<&Path>, home: Option<&Path>) -> (Config, Vec<String>) {
    let mut cfg = Config::default();
    let mut warnings = Vec::new();
    if let Some(home) = home {
        if let Some(raw) = load_layer(&home.join(".autofork/config.toml"), &mut warnings) {
            apply_layer(&mut cfg, raw, false, &mut warnings);
        }
    }
    if let Some(root) = project_root {
        let path = root.join(".autofork/config.toml");
        // Don't double-apply when the "project" root is the home dir itself.
        if home.map(|h| h.join(".autofork/config.toml")) != Some(path.clone()) {
            if let Some(raw) = load_layer(&path, &mut warnings) {
                apply_layer(&mut cfg, raw, true, &mut warnings);
            }
        }
    }
    for w in &warnings {
        tracing::warn!("config: {w}");
    }
    (cfg, warnings)
}

/// The user's home directory, honoring the `AUTOFORK_HOME` test/dev override
/// (which replaces the *`~/.autofork`* base, not `$HOME` itself).
pub fn autofork_home_from_env() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("AUTOFORK_HOME") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return Some(dir);
        }
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".autofork"))
}

/// The daemon's home base: `~/.autofork`.
pub fn autofork_home(home: &Path) -> PathBuf {
    home.join(".autofork")
}

/// Paths under the autofork base dir (`~/.autofork` or `$AUTOFORK_HOME`).
pub struct Paths {
    pub base: PathBuf,
}

impl Paths {
    pub fn new(base: PathBuf) -> Self {
        Self { base }
    }

    /// From the environment (`AUTOFORK_HOME` override, else `$HOME/.autofork`).
    pub fn from_env() -> Option<Self> {
        autofork_home_from_env().map(Self::new)
    }

    /// The daemon socket path: `$AUTOFORK_SOCKET` override, else
    /// `$XDG_RUNTIME_DIR/autofork.sock` when set (kept short — the platform
    /// caps `sun_path` around 100 bytes), else `<base>/run/daemon.sock`.
    pub fn socket(&self) -> PathBuf {
        if let Some(p) = std::env::var_os("AUTOFORK_SOCKET") {
            let p = PathBuf::from(p);
            if p.is_absolute() {
                return p;
            }
        }
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            let dir = PathBuf::from(dir);
            if dir.is_absolute() {
                return dir.join("autofork.sock");
            }
        }
        self.base.join("run/daemon.sock")
    }

    /// Held (flocked) by the daemon for its whole life; acquirable = dead.
    pub fn daemon_lock(&self) -> PathBuf {
        self.base.join("run/daemon.lock")
    }

    /// Serializes daemon auto-spawn between racing CLIs.
    pub fn spawn_lock(&self) -> PathBuf {
        self.base.join("run/spawn.lock")
    }

    pub fn db(&self) -> PathBuf {
        self.base.join("state.db")
    }

    pub fn daemon_log(&self) -> PathBuf {
        self.base.join("logs/daemon.log")
    }

    /// The user-level config layer lives at `<base>/config.toml`; the
    /// `load_config` home parameter expects the directory *containing*
    /// `.autofork`, so this exposes the base for direct loading.
    pub fn user_config(&self) -> PathBuf {
        self.base.join("config.toml")
    }
}

/// Load the effective config from explicit layer paths (daemon-side variant
/// of `load_config` that works with a `Paths` base).
pub fn load_config_at(project_root: Option<&Path>, user_config: &Path) -> (Config, Vec<String>) {
    let mut cfg = Config::default();
    let mut warnings = Vec::new();
    if let Some(raw) = load_layer(user_config, &mut warnings) {
        apply_layer(&mut cfg, raw, false, &mut warnings);
    }
    if let Some(root) = project_root {
        let path = root.join(".autofork/config.toml");
        if path != user_config {
            if let Some(raw) = load_layer(&path, &mut warnings) {
                apply_layer(&mut cfg, raw, true, &mut warnings);
            }
        }
    }
    for w in &warnings {
        tracing::warn!("config: {w}");
    }
    (cfg, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn layering_and_global_only() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let proj = tmp.path().join("proj");
        fs::create_dir_all(home.join(".autofork")).unwrap();
        fs::create_dir_all(proj.join(".autofork")).unwrap();
        fs::write(
            home.join(".autofork/config.toml"),
            "default_idle_deadline = \"5m\"\nquiet_period = \"30m\"\n",
        )
        .unwrap();
        fs::write(
            proj.join(".autofork/config.toml"),
            "default_idle_deadline = 120\nquiet_period = \"1h\"\n",
        )
        .unwrap();

        let (cfg, warnings) = load_config(Some(&proj), Some(&home));
        assert_eq!(cfg.default_idle_deadline_secs, 120);
        assert_eq!(cfg.quiet_period_secs, 1800); // project-level ignored
        assert!(warnings.iter().any(|w| w.contains("quiet_period")));
    }

    #[test]
    fn deprecated_keys_warn_but_dont_error() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        fs::create_dir_all(home.join(".autofork")).unwrap();
        fs::write(
            home.join(".autofork/config.toml"),
            "claude_bin = \"claude-x\"\nconcurrency = 4\nrun_timeout = \"10m\"\nisolation = \"open\"\ncontext_window = 500000\n[models]\n\"m1\" = 100\n",
        )
        .unwrap();
        let (cfg, warnings) = load_config(None, Some(home));
        // Nothing applied; all deprecated keys warned.
        assert_eq!(cfg, Config::default());
        for key in [
            "claude_bin",
            "concurrency",
            "run_timeout",
            "isolation",
            "context_window",
            "models",
        ] {
            assert!(
                warnings.iter().any(|w| w.contains(key)),
                "missing warning for {key}"
            );
        }
    }

    #[test]
    fn tag_filter_keys_layer_project_over_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let proj = tmp.path().join("proj");
        fs::create_dir_all(home.join(".autofork")).unwrap();
        fs::create_dir_all(proj.join(".autofork")).unwrap();
        fs::write(
            home.join(".autofork/config.toml"),
            "enable_tags = [\"home\"]\ndisable_tags = [\"noisy\"]\n",
        )
        .unwrap();
        fs::write(
            proj.join(".autofork/config.toml"),
            "enable_tags = [\"ci\", \"review\"]\n",
        )
        .unwrap();

        let (cfg, _) = load_config(Some(&proj), Some(&home));
        assert_eq!(
            cfg.enable_tags,
            Some(vec!["ci".to_string(), "review".to_string()])
        );
        assert_eq!(cfg.disable_tags, Some(vec!["noisy".to_string()]));
    }

    #[test]
    fn tag_throttles_layer_per_key() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let proj = tmp.path().join("proj");
        fs::create_dir_all(home.join(".autofork")).unwrap();
        fs::create_dir_all(proj.join(".autofork")).unwrap();
        fs::write(
            home.join(".autofork/config.toml"),
            "[tag_throttles]\nci = \"1h\"\nreview = \"30m\"\n",
        )
        .unwrap();
        fs::write(
            proj.join(".autofork/config.toml"),
            "[tag_throttles]\nci = 120\ndocs = \"10m\"\n",
        )
        .unwrap();

        let (cfg, warnings) = load_config(Some(&proj), Some(&home));
        assert_eq!(cfg.tag_throttles.get("ci"), Some(&120));
        assert_eq!(cfg.tag_throttles.get("review"), Some(&1800));
        assert_eq!(cfg.tag_throttles.get("docs"), Some(&600));
        assert!(warnings.is_empty());
    }

    #[test]
    fn wake_debounce_parses_and_defaults() {
        assert_eq!(Config::default().wake_debounce_secs, 5);
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        fs::create_dir_all(home.join(".autofork")).unwrap();
        fs::write(home.join(".autofork/config.toml"), "wake_debounce = 0\n").unwrap();
        let (cfg, warnings) = load_config(None, Some(home));
        assert_eq!(cfg.wake_debounce_secs, 0);
        assert!(warnings.is_empty());
    }

    #[test]
    fn runaway_limit_parses_and_zero_disables() {
        assert_eq!(Config::default().runaway_limit, 30);
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        fs::create_dir_all(home.join(".autofork")).unwrap();
        fs::write(home.join(".autofork/config.toml"), "runaway_limit = 0\n").unwrap();
        let (cfg, warnings) = load_config(None, Some(home));
        assert_eq!(cfg.runaway_limit, 0);
        assert!(warnings.is_empty());
        fs::write(home.join(".autofork/config.toml"), "runaway_limit = -3\n").unwrap();
        let (cfg, warnings) = load_config(None, Some(home));
        assert_eq!(cfg.runaway_limit, 30);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn defaults_when_nothing_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, warnings) = load_config(Some(tmp.path()), Some(tmp.path()));
        assert_eq!(cfg, Config::default());
        assert!(warnings.is_empty());
    }

    #[test]
    fn invalid_values_warn_and_keep_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        fs::create_dir_all(home.join(".autofork")).unwrap();
        fs::write(
            home.join(".autofork/config.toml"),
            "default_idle_deadline = \"soon\"\n",
        )
        .unwrap();
        let (cfg, warnings) = load_config(None, Some(home));
        assert_eq!(cfg.default_idle_deadline_secs, 600);
        assert_eq!(warnings.len(), 1);
    }
}
