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
use autofork_core::guard::{shell, Guard, ToolCall, Verdict};
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
            "edit" | "write" | "patch" | "multiedit" | "apply_patch" => {
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
        let _ = writeln!(f, "{now} {action} fork={fork} tool={tool} {what}");
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
