//! OS sandboxes for shell commands the analyser cannot vouch for.
//!
//! The rule is derived from the guard alone: the process may write only
//! under the write set (plus the scratch places every program needs:
//! `$TMPDIR`, `/tmp`, `/dev/null`, its tty) and has no network at all. A
//! `python3 -c` that stays inside the write set works normally; one that
//! reaches outside fails at the syscall, and the runner appends the guard's
//! explanation to the output so the model reads why.
//!
//! macOS: Seatbelt through `/usr/bin/sandbox-exec` (deprecated in name,
//! shipped and used by Apple's own tooling). Linux: bubblewrap (`bwrap`),
//! the same tool Claude Code's sandbox uses. Elsewhere, or when the tool is
//! missing, there is no sandbox and the guard refuses the command instead.

use std::path::{Path, PathBuf};

use super::Guard;

/// Why no sandbox can be produced on this machine.
pub fn availability() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        if Path::new("/usr/bin/sandbox-exec").exists() {
            Ok(())
        } else {
            Err("/usr/bin/sandbox-exec is missing".into())
        }
    }
    #[cfg(target_os = "linux")]
    {
        if which("bwrap").is_some() {
            Ok(())
        } else {
            Err("bubblewrap (`bwrap`) is not installed".into())
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err("no sandbox on this platform".into())
    }
}

/// The shell command line that runs `command` from `cwd` inside a sandbox
/// derived from `guard`. A profile file may be written under `tmp_dir`.
pub fn wrap(command: &str, cwd: &Path, guard: &Guard, tmp_dir: &Path) -> Result<String, String> {
    availability()?;
    let writes = write_roots(guard);
    #[cfg(target_os = "macos")]
    {
        let profile = seatbelt_profile(&writes);
        let _ = std::fs::create_dir_all(tmp_dir);
        let name = format!("guard-{:016x}.sb", crate::sys::fnv1a64(&profile));
        let path = tmp_dir.join(name);
        if !path.exists() {
            std::fs::write(&path, &profile)
                .map_err(|e| format!("cannot write sandbox profile: {e}"))?;
        }
        let _ = cwd;
        Ok(format!(
            "/usr/bin/sandbox-exec -f {} /bin/bash -c {}",
            shell_quote(&path.display().to_string()),
            shell_quote(command)
        ))
    }
    #[cfg(target_os = "linux")]
    {
        let _ = tmp_dir;
        let mut args = bwrap_args(&writes, cwd);
        args.push("/bin/bash".into());
        args.push("-c".into());
        args.push(command.to_string());
        Ok(args
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" "))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (command, cwd, tmp_dir, writes);
        Err("no sandbox on this platform".into())
    }
}

/// Whether a sandboxed command's output looks like it hit the sandbox's
/// rule rather than failing on its own terms.
pub fn looks_like_violation(output: &str) -> bool {
    let o = output.to_ascii_lowercase();
    o.contains("operation not permitted")
        || o.contains("eperm")
        || o.contains("permission denied")
        || o.contains("read-only file system")
        || o.contains("erofs")
        || o.contains("network is unreachable")
        || o.contains("could not resolve host")
        || o.contains("temporary failure in name resolution")
        || o.contains("nodename nor servname provided")
        || o.contains("connection refused")
        || o.contains("socket: operation not permitted")
        || o.contains("sandbox")
}

/// The directories the sandbox lets the command write: the write set,
/// existing paths only (a sandbox cannot bind what does not exist), plus
/// the scratch places.
fn write_roots(guard: &Guard) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for w in &guard.write {
        let p = super::canonical_prefix(w);
        if p.exists() && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

fn scratch_roots() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = vec![
        PathBuf::from("/tmp"),
        PathBuf::from("/private/tmp"),
        PathBuf::from("/var/tmp"),
        PathBuf::from("/private/var/tmp"),
        PathBuf::from("/var/folders"),
        PathBuf::from("/private/var/folders"),
        PathBuf::from("/dev/shm"),
    ];
    if let Some(t) = std::env::var_os("TMPDIR") {
        let t = PathBuf::from(t);
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// A Seatbelt profile: everything allowed except writing outside the
/// roots and any network use.
pub fn seatbelt_profile(writes: &[PathBuf]) -> String {
    let mut p = String::new();
    p.push_str("(version 1)\n(allow default)\n(deny network*)\n(deny file-write*)\n");
    p.push_str("(allow file-write*\n");
    for w in writes.iter().chain(scratch_roots().iter()) {
        p.push_str(&format!(
            "  (subpath {})\n",
            sb_quote(&w.display().to_string())
        ));
    }
    p.push_str("  (literal \"/dev/null\")\n  (literal \"/dev/zero\")\n  (literal \"/dev/stdout\")\n  (literal \"/dev/stderr\")\n");
    p.push_str("  (regex #\"^/dev/tty\")\n  (regex #\"^/dev/fd/\")\n  (regex #\"^/dev/pts/\")\n  (literal \"/dev/ptmx\")\n");
    p.push_str(")\n");
    p
}

/// bubblewrap arguments: the whole filesystem read-only, the roots bound
/// writable, no network.
pub fn bwrap_args(writes: &[PathBuf], cwd: &Path) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "bwrap".into(),
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        "--dev".into(),
        "/dev".into(),
        "--proc".into(),
        "/proc".into(),
        "--tmpfs".into(),
        "/tmp".into(),
        "--unshare-net".into(),
        "--die-with-parent".into(),
    ];
    for w in writes.iter().chain(scratch_roots().iter()) {
        if w == Path::new("/tmp") || !w.exists() {
            continue;
        }
        a.push("--bind".into());
        a.push(w.display().to_string());
        a.push(w.display().to_string());
    }
    a.push("--chdir".into());
    a.push(cwd.display().to_string());
    a.push("--".into());
    a
}

fn sb_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Single-quote for a POSIX shell.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(target_os = "linux")]
fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_lists_roots_and_denies_network() {
        let p = seatbelt_profile(&[PathBuf::from("/Users/x/brain"), PathBuf::from("/a b/\"q\"")]);
        assert!(p.contains("(deny network*)"));
        assert!(p.contains("(deny file-write*)"));
        assert!(p.contains("(subpath \"/Users/x/brain\")"));
        assert!(p.contains("(subpath \"/a b/\\\"q\\\"\")"));
        assert!(p.contains("(literal \"/dev/null\")"));
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("abc"), "abc");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn violation_detection() {
        assert!(looks_like_violation(
            "PermissionError: [Errno 1] Operation not permitted: 'x'"
        ));
        assert!(looks_like_violation(
            "curl: (6) Could not resolve host: example.com"
        ));
        assert!(!looks_like_violation("error: no such file or directory"));
    }
}
