//! The session's *credential environment* — the env vars that decide which
//! account (and which endpoint) a fork run talks to — carried from the
//! client's own process to whatever ends up spawning the fork.
//!
//! Why this exists: a fork child normally inherits the environment of the
//! process that spawns it, and for the in-session paths that is nearly
//! right — the parked Stop hook is a child of the user's Claude Code, so
//! `ANTHROPIC_BASE_URL`, a corporate proxy's `HTTPS_PROXY` and friends are
//! already there. The one exception is `CLAUDE_CODE_OAUTH_TOKEN`, which
//! Claude Code scrubs from every child it spawns, hooks included; see
//! [`OAUTH_TOKEN_OVERRIDE`] for the way around it. The daemon is the other
//! exception. It is spawned ONCE, by whichever client's hook first found no
//! daemon running, and then serves every session on the machine for hours. On a
//! machine with several harnesses (Claude Code, opencode, codex) or several
//! auth methods, the daemon's env is therefore an arbitrary snapshot of some
//! *other* session's shell:
//!
//! - a daemon started from opencode has no `CLAUDE_CODE_OAUTH_TOKEN` at all,
//!   so the `flush_on_close` end-runner it spawns runs `claude -p` unauthenticated
//!   (or falls through to a stale keychain login), and every consolidation
//!   fork of a closing session fails;
//! - worse, a daemon that *does* carry a token from a shell that has since
//!   rotated it — or from a different account than the session being flushed
//!   — authenticates as the wrong identity, which no fallback can detect.
//!
//! So the client sends its own credential env with every event
//! ([`Event::env`](crate::protocol::Event::env)), the daemon keeps the latest
//! snapshot per session **in memory only** (never in `state.db`: a token
//! belongs in the keychain or the user's rc file, not in autofork's state),
//! and every process the daemon spawns on that session's behalf gets it
//! applied [authoritatively](Snapshot::apply): each carried name is first
//! removed and then re-set from the snapshot, so a var the session does NOT
//! have can never survive by inheritance from the daemon's own env.
//!
//! When there is no snapshot (a daemon that restarted and never saw an event
//! from the session it is now flushing) nothing is touched and the child
//! inherits, which is the pre-v0.28 behavior.

use serde::{Deserialize, Serialize};

/// The Claude Code OAuth token autofork's own `claude` children run with.
///
/// Claude Code removes `CLAUDE_CODE_OAUTH_TOKEN` from the environment of
/// everything it spawns — hooks, Bash tool shells — while every other
/// credential var (`ANTHROPIC_API_KEY`, `CLAUDE_CONFIG_DIR`, the AWS ones…)
/// passes through. A session that authenticates *only* with that token (no
/// keychain login, no `apiKeyHelper` in its config dir) therefore leaves
/// autofork nothing to authenticate its fork runs with, and every headless or
/// flush-on-close fork dies with "Not logged in".
///
/// Exporting the same token under this name as well gets it past the scrub:
/// every `claude` process autofork spawns gets it as
/// `CLAUDE_CODE_OAUTH_TOKEN`, and the [`Snapshot`] carries it to the daemon's
/// children the same way. It wins over a plain `CLAUDE_CODE_OAUTH_TOKEN`
/// when both are present — it is the one you set for autofork on purpose. An
/// empty value counts as unset.
pub const OAUTH_TOKEN_OVERRIDE: &str = "AUTOFORK_CLAUDE_CODE_OAUTH_TOKEN";

/// The name Claude Code reads the token from.
pub const OAUTH_TOKEN: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// The [`OAUTH_TOKEN_OVERRIDE`] value in this process's env, if set.
pub fn oauth_token_override() -> Option<String> {
    std::env::var(OAUTH_TOKEN_OVERRIDE)
        .ok()
        .filter(|v| !v.is_empty())
}

/// Point a `claude` child at the [`OAUTH_TOKEN_OVERRIDE`] token, if one is
/// set. Called on every `claude` autofork spawns itself.
pub fn apply_oauth_override(cmd: &mut std::process::Command) {
    if let Some(token) = oauth_token_override() {
        cmd.env(OAUTH_TOKEN, token);
    }
}

/// The env vars carried from a session to its fork runs.
///
/// Deliberately a fixed list, not "everything": the daemon would otherwise
/// hand each child a stale copy of an unrelated shell's `PATH`, `PWD`,
/// terminal state and so on. Extend it per-machine with `AUTOFORK_CARRY_ENV`
/// (comma- or space-separated names) when a setup needs a var not listed
/// here — it is read from the client's env, so the extension travels with the
/// snapshot and needs no daemon-side config.
pub const NAMES: &[&str] = &[
    // Anthropic / Claude Code credentials and endpoint.
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_CUSTOM_HEADERS",
    "CLAUDE_CODE_OAUTH_TOKEN",
    OAUTH_TOKEN_OVERRIDE,
    "CLAUDE_CONFIG_DIR",
    // Which provider Claude Code talks to at all.
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    // Bedrock / Vertex credentials, for the two providers above.
    "AWS_ACCESS_KEY_ID",
    "AWS_BEARER_TOKEN_BEDROCK",
    "AWS_DEFAULT_REGION",
    "AWS_PROFILE",
    "AWS_REGION",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "CLOUD_ML_REGION",
    "GOOGLE_APPLICATION_CREDENTIALS",
    // codex / OpenAI.
    "CODEX_API_KEY",
    "CODEX_HOME",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    // opencode.
    "OPENCODE_API_KEY",
    "OPENCODE_CONFIG",
    // The network path to the API: corporate proxies and custom CA bundles
    // are as load-bearing as the token itself, and just as likely to be set
    // per-shell rather than machine-wide.
    "ALL_PROXY",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
    "all_proxy",
    "https_proxy",
    "http_proxy",
    "no_proxy",
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_DIR",
    "SSL_CERT_FILE",
];

/// Extra names to carry, from the client's `AUTOFORK_CARRY_ENV`.
fn extra_names() -> Vec<String> {
    std::env::var("AUTOFORK_CARRY_ENV")
        .ok()
        .map(|raw| {
            raw.split([',', ' ', '\t', ';'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// One session's credential env: the names considered, and the values it
/// actually had.
///
/// `names` travels with the values so the receiving side can be authoritative
/// without knowing the sender's `AUTOFORK_CARRY_ENV`: everything in `names`
/// is cleared from the child's env, then `vars` is applied. A name in `names`
/// but not in `vars` means "this session does not have it" — the strongest
/// statement we can make, and the one that keeps a daemon's stale token from
/// leaking into a session that authenticates some other way.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// Every name considered (built-in [`NAMES`] plus `AUTOFORK_CARRY_ENV`).
    #[serde(default)]
    pub names: Vec<String>,
    /// The subset that was actually set, as `(name, value)` pairs.
    #[serde(default)]
    pub vars: Vec<(String, String)>,
}

/// Values are secrets: never print them, not even under `--debug`. `Event`
/// derives `Debug` and is logged whole in a few places, so this impl is what
/// keeps a token out of `~/.autofork/logs/daemon.log`.
impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field(
                "set",
                &self
                    .vars
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("considered", &self.names.len())
            .finish()
    }
}

impl Snapshot {
    /// Capture the current process's credential env. Called in the client's
    /// hook — a direct child of the user's Claude Code / opencode / codex, so
    /// its env IS the session's env.
    pub fn capture() -> Self {
        let mut names: Vec<String> = NAMES.iter().map(|s| s.to_string()).collect();
        for extra in extra_names() {
            if !names.iter().any(|n| n == &extra) {
                names.push(extra);
            }
        }
        Self::build(names, |n| std::env::var(n).ok())
    }

    /// [`Self::capture`] over an arbitrary lookup, so tests need not mutate
    /// the process env. An [`OAUTH_TOKEN_OVERRIDE`] value replaces whatever
    /// `CLAUDE_CODE_OAUTH_TOKEN` the lookup had: the daemon's children then
    /// authenticate with it even though the hook never saw the real name.
    fn build(names: Vec<String>, get: impl Fn(&str) -> Option<String>) -> Self {
        let mut vars: Vec<(String, String)> = names
            .iter()
            .filter_map(|n| get(n).map(|v| (n.clone(), v)))
            .collect();
        if let Some(token) = get(OAUTH_TOKEN_OVERRIDE).filter(|v| !v.is_empty()) {
            vars.retain(|(k, _)| k != OAUTH_TOKEN);
            vars.push((OAUTH_TOKEN.to_string(), token));
        }
        Self { names, vars }
    }

    /// Whether this snapshot carries nothing at all — the common case for a
    /// keychain-authenticated Claude Code with no proxy. Still worth sending:
    /// "this session has none of these" is the fact that stops a daemon's
    /// stale token from being inherited.
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    /// The names to clear before applying [`Self::vars`]: everything
    /// considered, plus any value-carrying name the sender did not list
    /// (a newer client talking to an older... or simply a hand-built one).
    fn to_clear(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.names.iter().map(String::as_str).collect();
        for (k, _) in &self.vars {
            if !out.contains(&k.as_str()) {
                out.push(k);
            }
        }
        out
    }

    /// Apply authoritatively to a `std::process::Command`.
    pub fn apply(&self, cmd: &mut std::process::Command) {
        for name in self.to_clear() {
            cmd.env_remove(name);
        }
        for (k, v) in &self.vars {
            cmd.env(k, v);
        }
    }

    /// The same, as a `(clear, set)` pair, for callers whose command type is
    /// not `std::process::Command` (the daemon's lifecycle hooks run on
    /// `tokio::process::Command`).
    pub fn plan(&self) -> (Vec<String>, Vec<(String, String)>) {
        (
            self.to_clear().into_iter().map(str::to_string).collect(),
            self.vars.clone(),
        )
    }

    /// The names that are set, for logging and `autofork doctor`.
    pub fn set_names(&self) -> Vec<&str> {
        self.vars.iter().map(|(k, _)| k.as_str()).collect()
    }
}

/// Capture the current credential env, or `None` when there is nothing worth
/// sending *and* nothing worth stating. Kept as a helper so hook entrypoints
/// read as one line; it always returns `Some` today (an empty snapshot is
/// itself information — see [`Snapshot::is_empty`]) but keeps the option open
/// for a client that wants to opt out.
pub fn capture() -> Option<Snapshot> {
    if std::env::var_os("AUTOFORK_NO_CARRY_ENV").is_some() {
        return None;
    }
    Some(Snapshot::capture())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_clears_a_name_the_session_does_not_have() {
        let snap = Snapshot {
            names: vec!["CLAUDE_CODE_OAUTH_TOKEN".into(), "ANTHROPIC_API_KEY".into()],
            vars: vec![("ANTHROPIC_API_KEY".into(), "sk-session".into())],
        };
        let (clear, set) = snap.plan();
        // The daemon may hold a stale token; the session does not have one,
        // so the child must not either.
        assert!(clear.contains(&"CLAUDE_CODE_OAUTH_TOKEN".to_string()));
        assert_eq!(set, vec![("ANTHROPIC_API_KEY".into(), "sk-session".into())]);
    }

    #[test]
    fn a_value_without_its_name_is_still_cleared_first() {
        let snap = Snapshot {
            names: vec![],
            vars: vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "tok".into())],
        };
        let (clear, _) = snap.plan();
        assert_eq!(clear, vec!["CLAUDE_CODE_OAUTH_TOKEN".to_string()]);
    }

    #[test]
    fn debug_never_prints_a_value() {
        let snap = Snapshot {
            names: vec!["CLAUDE_CODE_OAUTH_TOKEN".into()],
            vars: vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "sk-ant-secret".into())],
        };
        let rendered = format!("{snap:?}");
        assert!(!rendered.contains("sk-ant-secret"), "{rendered}");
        assert!(rendered.contains("CLAUDE_CODE_OAUTH_TOKEN"), "{rendered}");
    }

    fn names() -> Vec<String> {
        NAMES.iter().map(|s| s.to_string()).collect()
    }

    fn value<'a>(snap: &'a Snapshot, name: &str) -> Option<&'a str> {
        snap.vars
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn the_override_is_carried_as_the_oauth_token() {
        // Claude Code scrubbed CLAUDE_CODE_OAUTH_TOKEN from the hook's env;
        // only the override got through.
        let snap = Snapshot::build(names(), |n| {
            (n == OAUTH_TOKEN_OVERRIDE).then(|| "sk-ant-oat-session".to_string())
        });
        assert_eq!(value(&snap, OAUTH_TOKEN), Some("sk-ant-oat-session"));
        assert_eq!(
            value(&snap, OAUTH_TOKEN_OVERRIDE),
            Some("sk-ant-oat-session")
        );
        assert_eq!(
            snap.vars.iter().filter(|(k, _)| k == OAUTH_TOKEN).count(),
            1,
            "{snap:?}"
        );
    }

    #[test]
    fn the_override_wins_over_a_plain_token() {
        let snap = Snapshot::build(names(), |n| match n {
            OAUTH_TOKEN => Some("stale".into()),
            OAUTH_TOKEN_OVERRIDE => Some("fresh".into()),
            _ => None,
        });
        assert_eq!(value(&snap, OAUTH_TOKEN), Some("fresh"));
        assert_eq!(
            snap.vars.iter().filter(|(k, _)| k == OAUTH_TOKEN).count(),
            1,
            "{snap:?}"
        );
    }

    #[test]
    fn an_empty_override_is_ignored() {
        let snap = Snapshot::build(names(), |n| match n {
            OAUTH_TOKEN => Some("plain".into()),
            OAUTH_TOKEN_OVERRIDE => Some(String::new()),
            _ => None,
        });
        assert_eq!(value(&snap, OAUTH_TOKEN), Some("plain"));
    }

    #[test]
    fn capture_takes_only_carried_names() {
        // A name that is never carried stays out, whatever the environment.
        let snap = Snapshot::capture();
        assert!(!snap.set_names().contains(&"PATH"));
        assert!(snap.names.iter().any(|n| n == "CLAUDE_CODE_OAUTH_TOKEN"));
    }
}
