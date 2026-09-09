//! The one end-to-end check that runs on every platform, Windows included:
//! the real CLI spawns the real daemon over the real endpoint (a Unix socket
//! here, a named pipe there), registers a session through the hook path,
//! reads it back, and retires the daemon. Everything the platform layer
//! abstracts — home resolution, the daemon lock, process detaching, the
//! transport — is on the wire here.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Env {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    socket: PathBuf,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        let home = base.join("af");
        let project = base.join("proj");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(".autofork/forks")).unwrap();
        std::fs::write(
            project.join(".autofork/forks/journal.md"),
            "---\nfork: true\ndescription: keep the journal\nrun_on: [idle: 4m]\n---\nUpdate the journal.\n",
        )
        .unwrap();
        std::fs::write(home.join("config.toml"), "quiet_period = \"1h\"\n").unwrap();
        Self {
            // Short: a Unix socket path is capped around 100 bytes.
            socket: base.join("d.sock"),
            _tmp: tmp,
            home,
            project,
        }
    }

    fn cli(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_autofork"));
        cmd.env("AUTOFORK_HOME", &self.home)
            .env("AUTOFORK_SOCKET", &self.socket)
            // Keep the developer's real ~/.claude out of discovery.
            .env("AUTOFORK_CLAUDE_DIR", self.home.join("claude"))
            .env("AUTOFORK_AGENTS_DIR", self.home.join("agents"))
            .current_dir(&self.project)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    fn run(&self, args: &[&str]) -> (Option<i32>, String, String) {
        let out = self.cli().args(args).output().unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    }

    fn hook(&self, event: &str, input: &serde_json::Value) -> (Option<i32>, String, String) {
        let mut child = self
            .cli()
            .args(["hook", event])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    }

    fn daemon_log(&self) -> String {
        std::fs::read_to_string(self.home.join("logs/daemon.log")).unwrap_or_default()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.run(&["stop-daemon"]);
    }
}

fn daemon_binary_present() -> bool {
    let exe = Path::new(env!("CARGO_BIN_EXE_autofork"));
    exe.parent()
        .map(|p| p.join(format!("autofork-daemon{}", std::env::consts::EXE_SUFFIX)))
        .map(|p| p.is_file())
        .unwrap_or(false)
}

/// The daemon is spawned by a hook that is itself several processes below
/// the session (Claude Code → shell → hook). On Windows every inheritable
/// handle travels down that chain, so a daemon spawned naively would hold
/// the session's pipes for its whole life. Run `status` through a shell
/// wrapper with a piped stdout: if the daemon inherits that pipe, this
/// `wait_with_output` never returns.
#[test]
fn daemon_spawned_through_a_wrapper_does_not_hold_its_pipes() {
    if !daemon_binary_present() {
        eprintln!("skipping: autofork-daemon binary not built next to the CLI");
        return;
    }
    let env = Env::new();
    let cli = env!("CARGO_BIN_EXE_autofork");
    #[cfg(windows)]
    let mut wrapper = {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("cmd");
        // cmd wants the payload verbatim; Rust's quoting would wrap it again.
        c.arg("/C").raw_arg(format!("\"{cli}\" status"));
        c
    };
    #[cfg(not(windows))]
    let mut wrapper = {
        let mut c = Command::new("sh");
        c.args(["-c", &format!("'{cli}' status")]);
        c
    };
    wrapper
        .env("AUTOFORK_HOME", &env.home)
        .env("AUTOFORK_SOCKET", &env.socket)
        .env("AUTOFORK_CLAUDE_DIR", env.home.join("claude"))
        .env("AUTOFORK_AGENTS_DIR", env.home.join("agents"))
        .current_dir(&env.project)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = wrapper.spawn().unwrap();
    // A watchdog turns a leak into a failure instead of a hung test run.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let out = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the wrapper's stdout never closed: the daemon inherited the pipe")
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("autofork daemon v"));
}

#[test]
fn cli_spawns_daemon_registers_a_session_and_retires_it() {
    if !daemon_binary_present() {
        // `cargo test -p autofork` alone does not build the daemon crate's
        // binary; the workspace test run does. Nothing to check without it.
        eprintln!("skipping: autofork-daemon binary not built next to the CLI");
        return;
    }
    let env = Env::new();

    // `status` auto-spawns the daemon and talks to it.
    let (code, out, err) = env.run(&["status"]);
    assert_eq!(code, Some(0), "status failed: {err}\n{}", env.daemon_log());
    assert!(
        out.contains("autofork daemon v"),
        "unexpected status output: {out}"
    );

    // A session registers through the hook path (the plugin's SessionStart).
    let input = serde_json::json!({
        "session_id": "rtsess01",
        "transcript_path": env.project.join("t.jsonl"),
        "cwd": env.project,
        "hook_event_name": "SessionStart",
        "source": "startup",
    });
    let (code, _, err) = env.hook("session-start", &input);
    assert_eq!(code, Some(0), "session-start hook failed: {err}");

    let (code, out, _) = env.run(&["status"]);
    assert_eq!(code, Some(0));
    assert!(out.contains("rtsess01"), "session not listed:\n{out}");

    // Discovery through the daemon finds the project's fork.
    let (code, out, err) = env.run(&["forks"]);
    assert_eq!(code, Some(0), "forks failed: {err}");
    assert!(out.contains("journal"), "fork not discovered:\n{out}");

    // The daemon answers a shutdown and lets go of its lock.
    let (code, out, _) = env.run(&["stop-daemon"]);
    assert_eq!(code, Some(0));
    assert!(out.contains("asked to exit"), "{out}");
    let start = Instant::now();
    loop {
        if autofork_core::sys::try_lock_file(&env.home.join("run/daemon.lock")).is_some() {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "daemon never released its lock:\n{}",
            env.daemon_log()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let (_, out, _) = env.run(&["stop-daemon"]);
    assert!(out.contains("not running"), "{out}");
}
