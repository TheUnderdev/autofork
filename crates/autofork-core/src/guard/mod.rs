//! Fork guards: what a fork run may touch, decided from its `guard:`
//! frontmatter and enforced at the harness's tool boundary.
//!
//! A fork inherits the parent's whole conversation, and a cheap model
//! routinely keeps executing the parent's last instruction instead of the
//! fork's own job — pushing the parent's branch, commenting on its PR,
//! building in its workspace. The guard turns that into a wall the fork
//! hits *with an explanation*, so even a weak model course-corrects instead
//! of flailing.
//!
//! The author states only what the fork may **change** (`write:`), and
//! everything else follows from it:
//!
//! - an edit/write tool call is allowed iff its path is inside the write set;
//! - a shell command is run through the static analyser in [`shell`], which
//!   follows `cd`, resolves relative paths, expands literal variables,
//!   recurses into `bash -c`, `eval`, `xargs`, `find -exec`, `source` and
//!   scripts it can read, and classifies every simple command as a reader,
//!   a writer of specific paths, a network/publish action, or *unknown*;
//! - a command whose effects the analyser can prove stay inside the write
//!   set runs as is; one that provably leaves it is refused with the
//!   author's message; one the analyser cannot prove (an interpreter, a
//!   build tool, an unresolvable path) runs inside an OS sandbox
//!   ([`sandbox`]) that allows writes only under the write set and no
//!   network at all — so `python3 -c` that stays home works and one that
//!   wanders fails, and the failure is explained the same way.
//!
//! The evaluator here is harness-neutral; each runner maps its tool names
//! onto [`ToolCall`] and its result onto a hook throw, a permission rule,
//! or a sandbox flag.

pub mod sandbox;
pub mod shell;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::sys;

/// Tool families a fork may be limited to (`guard.tools`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolFamily {
    /// Reading files and searching: read, glob, grep, list.
    Read,
    /// The shell tool.
    Shell,
    /// Editing and writing files.
    Edit,
    /// Web fetch and search.
    Web,
    /// Spawning subagents.
    Task,
}

impl ToolFamily {
    pub const ALL: [ToolFamily; 5] = [
        ToolFamily::Read,
        ToolFamily::Shell,
        ToolFamily::Edit,
        ToolFamily::Web,
        ToolFamily::Task,
    ];

    pub fn parse(s: &str) -> Option<ToolFamily> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "read" => ToolFamily::Read,
            "shell" | "bash" => ToolFamily::Shell,
            "edit" | "write" => ToolFamily::Edit,
            "web" => ToolFamily::Web,
            "task" | "agent" => ToolFamily::Task,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            ToolFamily::Read => "read",
            ToolFamily::Shell => "shell",
            ToolFamily::Edit => "edit",
            ToolFamily::Web => "web",
            ToolFamily::Task => "task",
        }
    }
}

/// A fork's guard, as parsed from its frontmatter. Paths are absolute
/// (tilde already expanded).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Guard {
    /// The only places the fork may change anything. Empty = read-only.
    #[serde(default)]
    pub write: Vec<PathBuf>,
    /// Tool families the fork may use at all. `None` = every family.
    /// `Some(vec![])` = no tool at all (a pure reviewer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolFamily>>,
    /// Whether generic network tools (curl, wget, ssh, gh, …) and the web
    /// tools are allowed. Off by default. Git remote operations are gated
    /// on the repository being inside the write set instead, so a fork
    /// that owns a repo may still sync and push it.
    #[serde(default)]
    pub network: bool,
    /// Command patterns refused on sight, wherever they appear (top level,
    /// inside `bash -c`, behind `xargs` or a wrapper): `ssh`, `git push*`,
    /// `rm -rf *`. Tokens match arguments in order, `*` in a token matches
    /// any text, a trailing `*` token matches the rest of the line.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    /// The author's course-correction text, quoted back to the fork
    /// whenever it hits the guard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// One tool call, harness-neutral.
#[derive(Debug, Clone)]
pub enum ToolCall<'a> {
    Shell { command: &'a str, cwd: &'a Path },
    Edit { path: &'a Path, cwd: &'a Path },
    Read,
    Web { target: Option<&'a str> },
    Task,
    Other { name: &'a str },
}

impl ToolCall<'_> {
    pub fn family(&self) -> Option<ToolFamily> {
        match self {
            ToolCall::Shell { .. } => Some(ToolFamily::Shell),
            ToolCall::Edit { .. } => Some(ToolFamily::Edit),
            ToolCall::Read => Some(ToolFamily::Read),
            ToolCall::Web { .. } => Some(ToolFamily::Web),
            ToolCall::Task => Some(ToolFamily::Task),
            ToolCall::Other { .. } => None,
        }
    }
}

/// The guard's decision on one tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum Verdict {
    Allow,
    /// Refuse the call; `message` is what the model reads instead of a
    /// tool result.
    Deny {
        message: String,
    },
    /// Run the shell command, but inside an OS sandbox confined to the
    /// write set with no network. `command` is the wrapped command line;
    /// `message` is what to append to the output if it fails on the
    /// sandbox's rule.
    Sandbox {
        command: String,
        message: String,
    },
}

impl Guard {
    /// Whether `path` (absolute) may be changed by this fork. Symlinks in
    /// the existing prefix are resolved so a link out of the write set
    /// does not count as inside it.
    pub fn writable(&self, path: &Path) -> bool {
        if self.write.is_empty() {
            return false;
        }
        let real = canonical_prefix(path);
        self.write.iter().any(|w| {
            let w = canonical_prefix(w);
            real == w || real.starts_with(&w)
        })
    }

    pub fn allows_family(&self, family: ToolFamily) -> bool {
        match &self.tools {
            None => true,
            Some(list) => list.contains(&family),
        }
    }

    /// Evaluate one tool call. `tmp_dir` is where a sandbox profile may be
    /// written (the autofork tmp dir); `fork_name` is quoted in messages.
    pub fn evaluate(&self, fork_name: &str, call: &ToolCall, tmp_dir: &Path) -> Verdict {
        let family = call.family();
        if let Some(f) = family {
            if !self.allows_family(f) {
                let what = match call {
                    ToolCall::Shell { command, .. } => {
                        format!("the shell (`{}`)", excerpt(command))
                    }
                    ToolCall::Edit { path, .. } => format!("editing `{}`", path.display()),
                    ToolCall::Read => "reading files".to_string(),
                    ToolCall::Web { .. } => "the web".to_string(),
                    ToolCall::Task => "subagents".to_string(),
                    ToolCall::Other { name } => format!("the `{name}` tool"),
                };
                return Verdict::Deny {
                    message: self.compose(
                        fork_name,
                        &[format!(
                            "{what} is not available to this fork: it may use {}",
                            self.tools_phrase()
                        )],
                    ),
                };
            }
        }
        match call {
            ToolCall::Read => Verdict::Allow,
            ToolCall::Task => Verdict::Allow,
            ToolCall::Other { name } => {
                if self.tools.as_ref().is_some_and(|t| t.is_empty()) {
                    Verdict::Deny {
                        message: self.compose(
                            fork_name,
                            &[format!("the `{name}` tool is not available to this fork")],
                        ),
                    }
                } else {
                    Verdict::Allow
                }
            }
            ToolCall::Web { target } => {
                if self.network {
                    Verdict::Allow
                } else {
                    let t = target
                        .map(|t| format!(" (`{}`)", excerpt(t)))
                        .unwrap_or_default();
                    Verdict::Deny {
                        message: self.compose(
                            fork_name,
                            &[format!(
                                "fetching from the web{t} is not allowed: this fork has no network"
                            )],
                        ),
                    }
                }
            }
            ToolCall::Edit { path, cwd } => {
                let abs = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    cwd.join(path)
                };
                let abs = normalize(&abs);
                if self.writable(&abs) {
                    Verdict::Allow
                } else {
                    Verdict::Deny {
                        message: self.compose(
                            fork_name,
                            &[format!("editing `{}` is outside the list", abs.display())],
                        ),
                    }
                }
            }
            ToolCall::Shell { command, cwd } => {
                self.evaluate_shell(fork_name, command, cwd, tmp_dir)
            }
        }
    }

    fn evaluate_shell(
        &self,
        fork_name: &str,
        command: &str,
        cwd: &Path,
        tmp_dir: &Path,
    ) -> Verdict {
        let analysis = shell::analyse(command, cwd, &self.write, &self.deny);
        let mut denies: Vec<String> = Vec::new();
        let mut unknowns: Vec<String> = Vec::new();
        for e in &analysis.effects {
            match e {
                shell::Effect::Write { path, by } => {
                    if !self.writable(path) {
                        denies.push(format!(
                            "`{by}` writes `{}`, outside the list",
                            path.display()
                        ));
                    }
                }
                shell::Effect::Exec { path, by } => {
                    if !self.writable(path) {
                        unknowns.push(format!("`{by}` runs a script autofork cannot analyse"));
                    }
                }
                shell::Effect::GitRemote { repo, by, publish } => {
                    if !self.writable(repo) {
                        let verb = if *publish { "publishes" } else { "syncs" };
                        denies.push(format!(
                            "`{by}` {verb} the repository at `{}`, outside the list",
                            repo.display()
                        ));
                    }
                }
                shell::Effect::Network { by } => {
                    if !self.network {
                        denies.push(format!(
                            "`{by}` uses the network, which this fork does not have"
                        ));
                    }
                }
                shell::Effect::Publish { by, what } => {
                    if !self.network {
                        denies.push(format!("`{by}` {what}, which this fork may not do"));
                    }
                }
                shell::Effect::Denied { by, pattern } => {
                    denies.push(format!(
                        "`{by}` matches this fork's deny list (`{pattern}`)"
                    ));
                }
                shell::Effect::Escalate { by } => {
                    denies.push(format!("`{by}` escalates privileges, which no fork may do"));
                }
                shell::Effect::Disrupt { by } => {
                    denies.push(format!("`{by}` stops or reconfigures processes or the system, which no fork may do"));
                }
                shell::Effect::Unknown { by, why } => {
                    unknowns.push(format!("`{by}` {why}"));
                }
            }
        }
        if !denies.is_empty() {
            return Verdict::Deny {
                message: self.compose(fork_name, &denies),
            };
        }
        if unknowns.is_empty() {
            return Verdict::Allow;
        }
        match sandbox::wrap(command, cwd, self, tmp_dir) {
            Ok(wrapped) => {
                let mut reasons = vec![format!(
                    "the command ran inside a sandbox because {}; it may only write under the list and has no network, and it broke that rule",
                    unknowns.join("; ")
                )];
                reasons.truncate(1);
                Verdict::Sandbox {
                    command: wrapped,
                    message: self.compose(fork_name, &reasons),
                }
            }
            Err(why) => {
                let mut reasons = unknowns;
                reasons.push(format!("autofork cannot confine it either ({why}), so it is refused; express the work as plain shell commands on explicit paths"));
                Verdict::Deny {
                    message: self.compose(fork_name, &reasons),
                }
            }
        }
    }

    fn tools_phrase(&self) -> String {
        match &self.tools {
            None => "every tool".to_string(),
            Some(t) if t.is_empty() => "no tool at all: it decides from the conversation it inherited and writes its report".to_string(),
            Some(t) => {
                let names: Vec<&str> = t.iter().map(|f| f.name()).collect();
                format!("only: {}", names.join(", "))
            }
        }
    }

    fn scope_phrase(&self) -> String {
        if self.write.is_empty() {
            "This fork may not change any file.".to_string()
        } else {
            let list: Vec<String> = self
                .write
                .iter()
                .map(|p| format!("`{}`", p.display()))
                .collect();
            format!(
                "This fork may only change files under: {}.",
                list.join(", ")
            )
        }
    }

    /// The text a model reads when it hits the guard. Written for the
    /// weakest model that will ever read it: what was blocked, why, what
    /// the author wants, and the one thing not to do next.
    pub fn compose(&self, fork_name: &str, reasons: &[String]) -> String {
        let mut out = format!(
            "autofork guard — fork `{fork_name}`. {}",
            self.scope_phrase()
        );
        let shown: Vec<&String> = reasons.iter().take(3).collect();
        if !shown.is_empty() {
            out.push_str("\nBlocked: ");
            out.push_str(
                &shown
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join("; "),
            );
            out.push('.');
        }
        if let Some(m) = &self.message {
            let m = m.trim();
            if !m.is_empty() {
                out.push('\n');
                out.push_str(m);
            }
        }
        out.push_str(
            "\nThis rule is enforced at the tool boundary. Do not retry through another tool, \
             another shell, an interpreter, xargs, eval, a script, or a different path: every \
             route is checked the same way and will fail the same way. If this action belonged \
             to the session you were forked from, it is not yours to take — leave it, finish \
             your own job, and say in your report what you did not do.",
        );
        out
    }

    /// One-line display for `autofork forks`.
    pub fn display(&self) -> String {
        let mut parts = Vec::new();
        if self.write.is_empty() {
            parts.push("write: none".to_string());
        } else {
            let list: Vec<String> = self.write.iter().map(|p| p.display().to_string()).collect();
            parts.push(format!("write: [{}]", list.join(", ")));
        }
        match &self.tools {
            None => {}
            Some(t) if t.is_empty() => parts.push("tools: none".to_string()),
            Some(t) => {
                let names: Vec<&str> = t.iter().map(|f| f.name()).collect();
                parts.push(format!("tools: [{}]", names.join(", ")));
            }
        }
        if self.network {
            parts.push("network: yes".to_string());
        }
        if !self.deny.is_empty() {
            parts.push(format!("deny: [{}]", self.deny.join(", ")));
        }
        parts.join(" | ")
    }

    /// The hosts of every git remote of every repository inside the write
    /// set — what a network allowlist derived from `write:` must contain
    /// for the fork to push its own repositories.
    pub fn remote_hosts(&self) -> Vec<String> {
        let mut hosts = Vec::new();
        for w in &self.write {
            for repo in git_repos_under(w) {
                for h in git_remote_hosts(&repo) {
                    if !hosts.contains(&h) {
                        hosts.push(h);
                    }
                }
            }
        }
        hosts
    }
}

/// Canonicalize the longest existing prefix of `path` and re-append the
/// rest, so a path that does not exist yet still resolves the symlinks
/// above it.
pub fn canonical_prefix(path: &Path) -> PathBuf {
    let path = normalize(path);
    let mut existing = path.clone();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if existing.exists() {
            break;
        }
        match (existing.file_name(), existing.parent()) {
            (Some(n), Some(p)) => {
                rest.push(n.to_os_string());
                existing = p.to_path_buf();
            }
            _ => break,
        }
    }
    let mut out = sys::canonical(&existing);
    for r in rest.iter().rev() {
        out.push(r);
    }
    out
}

/// Lexically normalize: collapse `.` and `..`, keep it absolute.
pub fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn excerpt(s: &str) -> String {
    let s = s.trim();
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 120 {
        let cut: String = one.chars().take(117).collect();
        format!("{cut}...")
    } else {
        one
    }
}

/// Git repositories at or below `root` (shallow: `root` itself, then one
/// level of children — a write set names a repo or a folder of repos).
fn git_repos_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    // `root` itself, or a repo above it.
    let mut p = Some(root.to_path_buf());
    while let Some(cur) = p {
        if cur.join(".git").exists() {
            out.push(cur);
            break;
        }
        p = cur.parent().map(|x| x.to_path_buf());
    }
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let path = e.path();
            if path.join(".git").exists() && !out.contains(&path) {
                out.push(path);
            }
        }
    }
    out
}

/// Host names in a repo's remotes, read straight from `.git/config` (no
/// git process: this runs inside a tool hook).
fn git_remote_hosts(repo: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let cfg = match std::fs::read_to_string(repo.join(".git/config")) {
        Ok(c) => c,
        Err(_) => return out,
    };
    for line in cfg.lines() {
        let line = line.trim();
        let Some(url) = line.strip_prefix("url") else {
            continue;
        };
        let Some(url) = url.trim_start().strip_prefix('=') else {
            continue;
        };
        if let Some(h) = remote_url_host(url.trim()) {
            if !out.contains(&h) {
                out.push(h);
            }
        }
    }
    out
}

/// The host of a git remote URL: `https://h/x`, `ssh://git@h:port/x`,
/// `git@h:x`, `h:x`.
pub fn remote_url_host(url: &str) -> Option<String> {
    if let Some(idx) = url.find("://") {
        let rest = &url[idx + 3..];
        let auth = rest.split('/').next()?;
        let host = auth.rsplit('@').next()?;
        let host = host.split(':').next()?;
        return (!host.is_empty()).then(|| host.to_string());
    }
    // scp-like: [user@]host:path
    let (auth, _) = url.split_once(':')?;
    if auth.contains('/') {
        return None;
    }
    let host = auth.rsplit('@').next()?;
    (!host.is_empty()).then(|| host.to_string())
}

// The analyser reasons in POSIX paths (a guard governs a POSIX shell);
// on Windows the same PathBufs display with backslashes, so the string
// assertions here are Unix-only. The logic itself compiles everywhere.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn guard(write: &[&str]) -> Guard {
        Guard {
            write: write.iter().map(PathBuf::from).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn writable_is_prefix_based() {
        let g = guard(&["/tmp/brain"]);
        assert!(g.writable(Path::new("/tmp/brain")));
        assert!(g.writable(Path::new("/tmp/brain/knowledge/x.md")));
        assert!(!g.writable(Path::new("/tmp/brainstorm")));
        assert!(!g.writable(Path::new("/tmp")));
        assert!(!guard(&[]).writable(Path::new("/tmp/brain")));
    }

    #[test]
    fn edit_outside_is_denied_with_message() {
        let g = Guard {
            message: Some("Only the brain.".into()),
            ..guard(&["/tmp/brain"])
        };
        let v = g.evaluate(
            "handover",
            &ToolCall::Edit {
                path: Path::new("src/main.rs"),
                cwd: Path::new("/work/proj"),
            },
            Path::new("/tmp"),
        );
        match v {
            Verdict::Deny { message } => {
                assert!(message.contains("/work/proj/src/main.rs"), "{message}");
                assert!(message.contains("Only the brain."));
                assert!(message.contains("fork `handover`"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            g.evaluate(
                "handover",
                &ToolCall::Edit {
                    path: Path::new("/tmp/brain/a.md"),
                    cwd: Path::new("/work")
                },
                Path::new("/tmp")
            ),
            Verdict::Allow
        );
    }

    #[test]
    fn tools_none_denies_everything() {
        let g = Guard {
            tools: Some(vec![]),
            ..Default::default()
        };
        assert!(matches!(
            g.evaluate("r", &ToolCall::Read, Path::new("/tmp")),
            Verdict::Deny { .. }
        ));
        assert!(matches!(
            g.evaluate(
                "r",
                &ToolCall::Shell {
                    command: "ls",
                    cwd: Path::new("/")
                },
                Path::new("/tmp")
            ),
            Verdict::Deny { .. }
        ));
        assert!(matches!(
            g.evaluate(
                "r",
                &ToolCall::Other { name: "todowrite" },
                Path::new("/tmp")
            ),
            Verdict::Deny { .. }
        ));
    }

    #[test]
    fn web_needs_network() {
        let g = guard(&[]);
        assert!(matches!(
            g.evaluate(
                "f",
                &ToolCall::Web {
                    target: Some("https://x")
                },
                Path::new("/tmp")
            ),
            Verdict::Deny { .. }
        ));
        let g = Guard {
            network: true,
            ..guard(&[])
        };
        assert_eq!(
            g.evaluate("f", &ToolCall::Web { target: None }, Path::new("/tmp")),
            Verdict::Allow
        );
    }

    #[test]
    fn deny_list_refuses_on_sight() {
        let g = Guard {
            network: true,
            deny: vec!["ssh".into(), "git push*".into()],
            ..guard(&["/tmp/brain"])
        };
        let tmp = Path::new("/tmp");
        for c in [
            "ssh host ls",
            "nohup ssh host",
            "bash -c 'ssh host'",
            "cd /tmp/brain && git push origin main",
            "echo x | xargs ssh",
        ] {
            match g.evaluate(
                "f",
                &ToolCall::Shell {
                    command: c,
                    cwd: Path::new("/tmp/brain"),
                },
                tmp,
            ) {
                Verdict::Deny { message } => {
                    assert!(message.contains("deny list"), "{c}: {message}")
                }
                other => panic!("{c}: {other:?}"),
            }
        }
        // `git pull` of the writable repo is not on the list.
        assert_eq!(
            g.evaluate(
                "f",
                &ToolCall::Shell {
                    command: "git pull",
                    cwd: Path::new("/tmp/brain")
                },
                tmp
            ),
            Verdict::Allow
        );
    }

    #[test]
    fn remote_url_hosts() {
        assert_eq!(
            remote_url_host("https://github.com/a/b.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            remote_url_host("ssh://git@git.example.org:2222/m/brain.git").as_deref(),
            Some("git.example.org")
        );
        assert_eq!(
            remote_url_host("git@github.com:a/b.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(remote_url_host("/local/path"), None);
    }

    #[test]
    fn normalize_collapses_dots() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
    }
}
