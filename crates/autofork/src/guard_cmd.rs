//! `autofork guard`: the fork guard's command-line face.
//!
//! - `guard eval` is what the harness adapters call at the tool boundary
//!   (JSON in on stdin, a verdict out on stdout). It is synchronous and
//!   daemon-free: the fork file is read and evaluated here, so a guard
//!   keeps working when the daemon is restarting.
//! - `guard check` and `guard analyse` are for humans: what would this
//!   fork say to this command, and what does the analyser see in it.

use std::io::Read;
use std::path::{Path, PathBuf};

use autofork_core::config::Paths;
use autofork_core::frontmatter::{parse_fork_file, ForkParse};
use autofork_core::guard::{patch_paths, shell, Guard, ToolCall, Verdict};
use serde::Deserialize;

/// The fork's name from its path, the way discovery names it: the file
/// stem, or the parent directory for `FORK.md`.
pub fn fork_name_from_path(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("fork");
    if stem.eq_ignore_ascii_case("fork") {
        if let Some(parent) = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
        {
            return parent.to_string();
        }
    }
    stem.to_string()
}

/// Load a fork file and return its guard, if it declares one.
pub fn load_guard(path: &Path) -> Result<Option<Guard>, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let name = fork_name_from_path(path);
    match parse_fork_file(&name, &content) {
        ForkParse::Fork(parsed) => Ok(parsed.def.guard),
        ForkParse::NotFork { .. } => Ok(None),
        ForkParse::Invalid => Err(format!("{} has invalid frontmatter", path.display())),
    }
}

/// What an adapter sends to `guard eval`.
#[derive(Debug, Deserialize)]
pub struct EvalInput {
    /// Absolute path to the fork's `.md`.
    pub fork_path: PathBuf,
    /// Fork name for messages (defaults to the name derived from the path).
    #[serde(default)]
    pub fork: Option<String>,
    /// Which harness's tool vocabulary `tool`/`args` use: `opencode`,
    /// `claude-code`, or `generic` (tool ∈ shell/edit/read/web/task).
    #[serde(default)]
    pub client: Option<String>,
    pub tool: String,
    #[serde(default)]
    pub args: serde_json::Value,
    /// The session's working directory.
    pub cwd: PathBuf,
}

/// Map a harness tool call onto the guard's vocabulary. Returns the
/// call plus owned strings it borrows from.
fn map_call<'a>(
    client: &str,
    tool: &'a str,
    args: &'a serde_json::Value,
    cwd: &'a Path,
    buf: &'a mut MapBuf,
) -> ToolCall<'a> {
    let s = |k: &str| args.get(k).and_then(|v| v.as_str());
    match client {
        "opencode" => match tool {
            "bash" => {
                buf.command = s("command").unwrap_or("").to_string();
                buf.cwd = s("workdir")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| cwd.to_path_buf());
                ToolCall::Shell {
                    command: &buf.command,
                    cwd: &buf.cwd,
                }
            }
            "edit" | "write" | "multiedit" => {
                buf.path = s("filePath")
                    .or(s("file_path"))
                    .or(s("path"))
                    .map(PathBuf::from)
                    .unwrap_or_default();
                ToolCall::Edit {
                    path: &buf.path,
                    cwd,
                }
            }
            // `apply_patch` (and the older `patch`) take no path argument:
            // the files are named inside the patch body (`patchText`), so
            // the guard reads them out of it. Falling through to an empty
            // path would have the guard judge the cwd instead — which is
            // both too strict (a brain handover from a job directory) and
            // too loose (a patch reaching outside from inside).
            "patch" | "apply_patch" => match s("filePath").or(s("file_path")).or(s("path")) {
                Some(p) => {
                    buf.path = PathBuf::from(p);
                    ToolCall::Edit {
                        path: &buf.path,
                        cwd,
                    }
                }
                None => {
                    buf.paths = patch_paths(
                        s("patchText")
                            .or(s("patch_text"))
                            .or(s("patch"))
                            .unwrap_or(""),
                    );
                    ToolCall::Patch {
                        paths: &buf.paths,
                        cwd,
                    }
                }
            },
            "read" | "glob" | "grep" | "list" | "ls" | "lsp" | "codesearch" => ToolCall::Read,
            "webfetch" | "websearch" => ToolCall::Web {
                target: s("url").or(s("query")),
            },
            "task" => ToolCall::Task,
            other if OPENCODE_BUILTINS.contains(&other) => ToolCall::Other { name: other },
            // opencode names MCP tools `<server>_<tool>`; anything that is
            // not a known built-in is one.
            other => ToolCall::Mcp { name: other },
        },
        "claude-code" | "claude" => match tool {
            "Bash" => {
                buf.command = s("command").unwrap_or("").to_string();
                buf.cwd = cwd.to_path_buf();
                ToolCall::Shell {
                    command: &buf.command,
                    cwd: &buf.cwd,
                }
            }
            "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
                buf.path = s("file_path")
                    .or(s("notebook_path"))
                    .map(PathBuf::from)
                    .unwrap_or_default();
                ToolCall::Edit {
                    path: &buf.path,
                    cwd,
                }
            }
            "Read" | "Glob" | "Grep" | "LS" => ToolCall::Read,
            "WebFetch" | "WebSearch" => ToolCall::Web {
                target: s("url").or(s("query")),
            },
            "Agent" | "Task" => ToolCall::Task,
            other if other.starts_with("mcp__") => ToolCall::Mcp { name: other },
            other => ToolCall::Other { name: other },
        },
        _ => match tool {
            "shell" | "bash" => {
                buf.command = s("command").unwrap_or("").to_string();
                buf.cwd = s("cwd")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| cwd.to_path_buf());
                ToolCall::Shell {
                    command: &buf.command,
                    cwd: &buf.cwd,
                }
            }
            "edit" | "write" => {
                buf.path = s("path").map(PathBuf::from).unwrap_or_default();
                ToolCall::Edit {
                    path: &buf.path,
                    cwd,
                }
            }
            "patch" => {
                buf.paths = patch_paths(s("patch").unwrap_or(""));
                ToolCall::Patch {
                    paths: &buf.paths,
                    cwd,
                }
            }
            "read" => ToolCall::Read,
            "web" => ToolCall::Web { target: s("url") },
            "task" => ToolCall::Task,
            "mcp" => ToolCall::Mcp {
                name: s("name").unwrap_or("mcp"),
            },
            other => ToolCall::Other { name: other },
        },
    }
}

/// opencode's own tools outside the five families. Everything else the
/// plugin reports is an MCP tool.
const OPENCODE_BUILTINS: [&str; 10] = [
    "todowrite",
    "todoread",
    "skill",
    "lsp",
    "question",
    "invalid",
    "batch",
    "toolsearch",
    "plan",
    "notebook",
];

#[derive(Default)]
struct MapBuf {
    command: String,
    cwd: PathBuf,
    path: PathBuf,
    paths: Vec<PathBuf>,
}

/// Evaluate one call. `None` guard = allow.
pub fn evaluate(paths: &Paths, input: &EvalInput) -> Result<Verdict, String> {
    let Some(guard) = load_guard(&input.fork_path)? else {
        return Ok(Verdict::Allow);
    };
    let name = input
        .fork
        .clone()
        .unwrap_or_else(|| fork_name_from_path(&input.fork_path));
    let client = input.client.as_deref().unwrap_or("generic");
    let mut buf = MapBuf::default();
    let call = map_call(client, &input.tool, &input.args, &input.cwd, &mut buf);
    let verdict = guard.evaluate(&name, &call, &paths.base.join("tmp"));
    log_verdict(paths, &name, &input.tool, &call, &verdict);
    Ok(verdict)
}

fn log_verdict(paths: &Paths, fork: &str, tool: &str, call: &ToolCall, verdict: &Verdict) {
    use std::io::Write;
    let what = match call {
        ToolCall::Shell { command, .. } => command
            .chars()
            .take(160)
            .collect::<String>()
            .replace('\n', " "),
        ToolCall::Edit { path, .. } => path.display().to_string(),
        ToolCall::Patch { paths, .. } => paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(" "),
        ToolCall::Web { target } => target.unwrap_or("").to_string(),
        _ => String::new(),
    };
    let action = match verdict {
        Verdict::Allow => "allow",
        Verdict::Deny { .. } => "deny",
        Verdict::Sandbox { .. } => "sandbox",
    };
    let dir = paths.base.join("logs");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("guard.log"))
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // One write per line: concurrent forks append to this file, and a
        // `writeln!` is several writes that interleave mid-line.
        let line = format!("{now} {action} fork={fork} tool={tool} {what}\n");
        let _ = f.write_all(line.as_bytes());
    }
}

/// `guard eval`: JSON on stdin, verdict JSON on stdout. Never fails the
/// caller: a broken input is reported as a deny with the error, because
/// a guard that cannot decide must not wave the call through.
pub fn eval_stdin(paths: &Paths) {
    let mut raw = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut raw) {
        print_verdict(&Verdict::Deny {
            message: format!("autofork guard could not read its input: {e}"),
        });
        return;
    }
    let input: EvalInput = match serde_json::from_str(&raw) {
        Ok(i) => i,
        Err(e) => {
            print_verdict(&Verdict::Deny {
                message: format!("autofork guard could not parse its input: {e}"),
            });
            return;
        }
    };
    match evaluate(paths, &input) {
        Ok(v) => print_verdict(&v),
        Err(e) => print_verdict(&Verdict::Deny {
            message: format!("autofork guard could not load the fork: {e}"),
        }),
    }
}

/// `guard hook`: the Claude Code `PreToolUse` face of the guard, for the
/// headless runner's forks. Claude Code's hook JSON on stdin (`tool_name`,
/// `tool_input`, `cwd`), a `hookSpecificOutput` decision on stdout.
///
/// A deny is reported as a `permissionDecision` of `deny` with the guard's
/// message as the reason, which Claude Code hands the model as the tool's
/// result — the same words the opencode plugin throws as the tool's error.
/// An allow prints an empty object, so the call goes on through the normal
/// permission flow (the mode, the settings rules, the native sandbox). A
/// `sandbox` verdict — a command the analyser cannot prove — is also let
/// through: on this client every Bash command already runs inside Claude
/// Code's own sandbox, confined to the same write set, so autofork's
/// wrapper would only nest one sandbox in another.
///
/// Fails closed: an input that cannot be read or parsed, or a fork file
/// that cannot be loaded, is a deny — never a silent allow. Exit status is
/// 0 either way; Claude Code honours the JSON decision as long as it parses.
pub fn claude_hook(paths: &Paths, fork_path: &Path) {
    let mut raw = String::new();
    let decision = match std::io::stdin().read_to_string(&mut raw) {
        Err(e) => Some(format!("autofork guard could not read its input: {e}")),
        Ok(_) => match serde_json::from_str::<ClaudeHookInput>(&raw) {
            Err(e) => Some(format!("autofork guard could not parse its input: {e}")),
            Ok(input) => {
                let eval = EvalInput {
                    fork_path: fork_path.to_path_buf(),
                    fork: None,
                    client: Some("claude-code".to_string()),
                    tool: input.tool_name,
                    args: input.tool_input,
                    cwd: input
                        .cwd
                        .or_else(|| std::env::current_dir().ok())
                        .unwrap_or_else(|| PathBuf::from("/")),
                };
                match evaluate(paths, &eval) {
                    Ok(v) => hook_denial(&v),
                    Err(e) => Some(format!("autofork guard could not load the fork: {e}")),
                }
            }
        },
    };
    println!("{}", claude_hook_output(decision.as_deref()));
}

/// What Claude Code's `PreToolUse` JSON carries that the guard needs.
#[derive(Debug, Deserialize)]
struct ClaudeHookInput {
    tool_name: String,
    #[serde(default)]
    tool_input: serde_json::Value,
    #[serde(default)]
    cwd: Option<PathBuf>,
}

/// The reason to deny with, if the verdict is a denial.
fn hook_denial(v: &Verdict) -> Option<String> {
    match v {
        Verdict::Allow | Verdict::Sandbox { .. } => None,
        Verdict::Deny { message } => Some(message.clone()),
    }
}

/// The `PreToolUse` hook's stdout: a deny with its reason, or `{}`.
fn claude_hook_output(deny_reason: Option<&str>) -> String {
    let v = match deny_reason {
        Some(reason) => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        }),
        None => serde_json::json!({}),
    };
    v.to_string()
}

fn print_verdict(v: &Verdict) {
    println!(
        "{}",
        serde_json::to_string(v).unwrap_or_else(|_| {
            "{\"action\":\"deny\",\"message\":\"autofork guard: internal error\"}".into()
        })
    );
}

/// `guard check`: what this fork's guard says to a command or an edit.
pub fn check(
    paths: &Paths,
    fork: &Path,
    cwd: Option<PathBuf>,
    path: Option<PathBuf>,
    command: Vec<String>,
) -> Result<(), String> {
    let cwd = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
    let Some(guard) = load_guard(fork)? else {
        println!(
            "{} declares no guard: everything is allowed",
            fork.display()
        );
        return Ok(());
    };
    let name = fork_name_from_path(fork);
    let verdict = if let Some(p) = path {
        guard.evaluate(
            &name,
            &ToolCall::Edit {
                path: &p,
                cwd: &cwd,
            },
            &paths.base.join("tmp"),
        )
    } else if !command.is_empty() {
        let cmd = command.join(" ");
        guard.evaluate(
            &name,
            &ToolCall::Shell {
                command: &cmd,
                cwd: &cwd,
            },
            &paths.base.join("tmp"),
        )
    } else {
        return Err("give a command (after --) or --path".into());
    };
    match verdict {
        Verdict::Allow => println!("allow"),
        Verdict::Deny { message } => println!("deny\n\n{message}"),
        Verdict::Sandbox { command, message } => {
            println!("sandbox\n\n{command}\n\non failure:\n{message}")
        }
    }
    Ok(())
}

/// `guard analyse`: the analyser's view of a command line.
pub fn analyse(cwd: Option<PathBuf>, command: Vec<String>) -> Result<(), String> {
    let cwd = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
    if command.is_empty() {
        return Err("give a command after --".into());
    }
    let cmd = command.join(" ");
    let a = shell::analyse(&cmd, &cwd, &[], &[]);
    if a.effects.is_empty() {
        println!("clean: reads only");
    }
    for e in &a.effects {
        match e {
            shell::Effect::Write { path, by } => println!("write    {}    ← {by}", path.display()),
            shell::Effect::Exec { path, by } => println!("exec     {}    ← {by}", path.display()),
            shell::Effect::GitRemote { repo, by, publish } => println!(
                "{} {}    ← {by}",
                if *publish { "publish " } else { "sync    " },
                repo.display()
            ),
            shell::Effect::Network { by } => println!("network  ← {by}"),
            shell::Effect::Publish { by, what } => println!("publish  {what}    ← {by}"),
            shell::Effect::Denied { by, pattern } => println!("denied   {pattern}    ← {by}"),
            shell::Effect::Escalate { by } => println!("escalate ← {by}"),
            shell::Effect::Disrupt { by } => println!("disrupt  ← {by}"),
            shell::Effect::Unknown { by, why } => println!("unknown  {why}    ← {by}"),
        }
    }
    if a.parse_error {
        println!("(the line did not fully parse as shell)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_hook_denies_with_the_guard_message_as_the_reason() {
        // The reason is what Claude Code hands the model as the tool
        // result, so it carries the guard's own words.
        let out = claude_hook_output(Some("fork 'x' may only change files under `/tmp/brain`"));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(
            v["hookSpecificOutput"]["permissionDecisionReason"],
            "fork 'x' may only change files under `/tmp/brain`"
        );
    }

    #[test]
    fn claude_hook_lets_allow_and_sandbox_through_untouched() {
        // `{}` = no decision: the call goes on through the mode, the rules
        // and Claude Code's own sandbox. A `sandbox` verdict is not a deny
        // here — the native sandbox already confines every Bash command.
        assert_eq!(claude_hook_output(None), "{}");
        assert_eq!(hook_denial(&Verdict::Allow), None);
        assert_eq!(
            hook_denial(&Verdict::Sandbox {
                command: "x".into(),
                message: "y".into()
            }),
            None
        );
        assert_eq!(
            hook_denial(&Verdict::Deny {
                message: "no".into()
            }),
            Some("no".to_string())
        );
    }

    #[test]
    fn claude_hook_input_takes_claude_codes_field_names() {
        let raw = r#"{"session_id":"s","hook_event_name":"PreToolUse","tool_name":"Write",
            "tool_input":{"file_path":"/etc/hosts","content":"x"},"cwd":"/work"}"#;
        let i: ClaudeHookInput = serde_json::from_str(raw).unwrap();
        assert_eq!(i.tool_name, "Write");
        assert_eq!(i.tool_input["file_path"], "/etc/hosts");
        assert_eq!(i.cwd.as_deref(), Some(Path::new("/work")));
    }
}
