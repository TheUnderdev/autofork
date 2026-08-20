//! CLI hook-path tests for `autofork hook <event>`.
//!
//! The stop-wait long poll is exercised against a *mock* daemon socket (so we
//! can assert exit codes and stderr precisely) plus one real-daemon end-to-end
//! wake. The fast events are covered against the real auto-spawned daemon.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use autofork_core::protocol::{encode, Request, RequestBody, Response, ResponseBody};
use autofork_core::PROTO_VERSION;

struct Env {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    socket: PathBuf,
    project: PathBuf,
}

impl Env {
    fn new(idle: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        let home = base.join("fsan");
        let project = base.join("proj");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(".autofork/forks")).unwrap();
        std::fs::write(
            home.join("config.toml"),
            // These tests assert the subagent (exit-2 wake) behavior; the
            // shipped default is headless, so pin the runner explicitly.
            format!("default_idle_deadline = \"{idle}\"\nquiet_period = \"1h\"\nwake_debounce = \"0\"\nfork_runner = \"subagent\"\n"),
        )
        .unwrap();
        Self {
            socket: base.join("d.sock"),
            _tmp: tmp,
            home,
            project,
        }
    }

    fn hook_input(&self, session: &str) -> serde_json::Value {
        serde_json::json!({
            "session_id": session,
            "transcript_path": self.project.join("t.jsonl"),
            "cwd": self.project,
            "hook_event_name": "whatever",
        })
    }

    /// Run `autofork hook <event>` to completion; returns (exit_code, stdout, stderr).
    fn hook(&self, event: &str, stdin_json: &serde_json::Value) -> (Option<i32>, String, String) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_autofork"))
            .args(["hook", event])
            .env("AUTOFORK_HOME", &self.home)
            .env("AUTOFORK_SOCKET", &self.socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin_json.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    }

    fn kill_daemon(&self) {
        if let Ok(mut s) = UnixStream::connect(&self.socket) {
            let _ = s.write_all(b"{\"proto\":1,\"id\":1,\"type\":\"shutdown\",\"drain\":false}\n");
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.kill_daemon();
    }
}

/// A mock daemon that answers Hello (as a very new version, so no retire) and
/// answers the first StopWait with `stop_wait_response`, then stops.
fn mock_daemon(socket: PathBuf, stop_wait_response: ResponseBody) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let listener = UnixListener::bind(&socket).unwrap();
        for conn in listener.incoming() {
            let Ok(stream) = conn else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut line = String::new();
            let mut done = false;
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                let req: Request = match serde_json::from_str(line.trim()) {
                    Ok(r) => r,
                    Err(_) => break,
                };
                let body = match req.body {
                    RequestBody::Hello { .. } => ResponseBody::HelloInfo {
                        version: "999.0.0".into(),
                    },
                    RequestBody::StopWait(_) => {
                        done = true;
                        stop_wait_response.clone()
                    }
                    _ => ResponseBody::Ack,
                };
                let resp = Response {
                    proto: PROTO_VERSION,
                    id: req.id,
                    body,
                };
                let _ = writer.write_all(encode(&resp).unwrap().as_bytes());
                line.clear();
                if done {
                    break;
                }
            }
            break;
        }
    })
}

fn wait_for_socket(path: &std::path::Path) {
    let start = Instant::now();
    while !path.exists() {
        assert!(start.elapsed() < Duration::from_secs(5), "mock never bound");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn stop_wait_wake_exits_2_with_payload_on_stderr() {
    let env = Env::new("1h");
    let handle = mock_daemon(
        env.socket.clone(),
        ResponseBody::Wake {
            payload: "WAKE_PAYLOAD_MARKER".into(),
            forks: None,
        },
    );
    wait_for_socket(&env.socket);

    let (code, stdout, stderr) = env.hook("stop-wait", &env.hook_input("s1"));
    assert_eq!(code, Some(2), "wake must exit 2");
    assert!(
        stderr.contains("WAKE_PAYLOAD_MARKER"),
        "payload not on stderr: {stderr}"
    );
    assert!(stdout.trim().is_empty(), "unexpected stdout: {stdout}");
    let _ = handle.join();
}

#[test]
fn stop_wait_waited_exits_0() {
    let env = Env::new("1h");
    let handle = mock_daemon(env.socket.clone(), ResponseBody::Waited);
    wait_for_socket(&env.socket);

    let (code, _stdout, _stderr) = env.hook("stop-wait", &env.hook_input("s1"));
    assert_eq!(code, Some(0), "a cancelled wait must exit 0 silently");
    let _ = handle.join();
}

#[test]
fn stop_wait_closed_socket_exits_0() {
    // Mock that binds, answers Hello, then closes the connection mid-poll (as a
    // retiring daemon would). The hook must exit 0, never wedge.
    let env = Env::new("1h");
    let socket = env.socket.clone();
    let handle = thread::spawn(move || {
        let listener = UnixListener::bind(&socket).unwrap();
        if let Some(Ok(stream)) = listener.incoming().next() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut line = String::new();
            // Answer Hello, then drop on the StopWait.
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                let req: Request = serde_json::from_str(line.trim()).unwrap();
                match req.body {
                    RequestBody::Hello { .. } => {
                        let resp = Response {
                            proto: PROTO_VERSION,
                            id: req.id,
                            body: ResponseBody::HelloInfo {
                                version: "999.0.0".into(),
                            },
                        };
                        let _ = writer.write_all(encode(&resp).unwrap().as_bytes());
                    }
                    RequestBody::StopWait(_) => break, // close without answering
                    _ => {}
                }
                line.clear();
            }
        }
    });
    wait_for_socket(&env.socket);

    let (code, _o, _e) = env.hook("stop-wait", &env.hook_input("s1"));
    assert_eq!(code, Some(0), "closed socket mid-poll must exit 0");
    let _ = handle.join();
}

#[test]
fn real_daemon_stop_wait_wakes_on_idle() {
    let env = Env::new("1s");
    std::fs::write(
        env.project.join(".autofork/forks/journal.md"),
        "---\nfork: true\nrun_on: [idle]\n---\nJOURNAL BODY",
    )
    .unwrap();

    // session-start auto-spawns the real daemon and registers the session.
    let (code, _o, _e) = env.hook("session-start", &env.hook_input("s1"));
    assert_eq!(code, Some(0));

    // stop-wait blocks until the 1s idle deadline, then wakes (exit 2).
    let (code, _stdout, stderr) = env.hook("stop-wait", &env.hook_input("s1"));
    assert_eq!(code, Some(2), "real daemon should wake at idle");
    assert!(stderr.contains("source: autofork"));
    assert!(stderr.contains("due: journal"));
    assert!(stderr.contains("subagent_type \"fork\""));
}

#[test]
fn disable_tags_env_filters_fork() {
    let env = Env::new("1s");
    std::fs::write(
        env.project.join(".autofork/forks/tagged.md"),
        "---\nfork: true\nrun_on: [idle]\ntags: [ci]\n---\nTAGGED BODY",
    )
    .unwrap();

    let run = |event: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_autofork"))
            .args(["hook", event])
            .env("AUTOFORK_HOME", &env.home)
            .env("AUTOFORK_SOCKET", &env.socket)
            .env("AUTOFORK_DISABLE_TAGS", "ci")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(env.hook_input("s1").to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };

    assert_eq!(run("session-start").status.code(), Some(0));
    // The disabled fork never comes due; the wait parks. Give it a moment past
    // the 1s deadline, then a prompt cancels it → exit 0 (never a wake/exit 2).
    let home = env.home.clone();
    let socket = env.socket.clone();
    let stdin = env.hook_input("s1").to_string();
    let handle = thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1800));
        let mut child = Command::new(env!("CARGO_BIN_EXE_autofork"))
            .args(["hook", "user-prompt-submit"])
            .env("AUTOFORK_HOME", &home)
            .env("AUTOFORK_SOCKET", &socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let _ = child.wait();
    });
    let out = run("stop-wait");
    assert_eq!(out.status.code(), Some(0), "disabled fork woke the session");
    let _ = handle.join();
}

#[test]
fn fork_env_guard_short_circuits_hook() {
    let env = Env::new("1h");
    std::fs::write(
        env.project.join(".autofork/forks/journal.md"),
        "---\nfork: true\nrun_on: [idle]\n---\nBODY",
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_autofork"))
        .args(["hook", "stop-wait"])
        .env("AUTOFORK_HOME", &env.home)
        .env("AUTOFORK_SOCKET", &env.socket)
        .env("AUTOFORK_FORK", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(env.hook_input("s1").to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "guarded hook must exit 0");
    assert!(out.stdout.is_empty(), "guarded hook produced stdout");
    assert!(out.stderr.is_empty(), "guarded hook produced stderr");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !env.socket.exists(),
        "a daemon was spawned despite the fork guard"
    );
}

#[test]
fn concurrent_session_starts_race_to_one_daemon() {
    let env = Env::new("1h");
    let mut children = Vec::new();
    for i in 0..5 {
        let mut child = Command::new(env!("CARGO_BIN_EXE_autofork"))
            .args(["hook", "session-start"])
            .env("AUTOFORK_HOME", &env.home)
            .env("AUTOFORK_SOCKET", &env.socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(env.hook_input(&format!("s{i}")).to_string().as_bytes())
            .unwrap();
        children.push(child);
    }
    for mut c in children {
        assert!(c.wait().unwrap().success());
    }
    let out = Command::new(env!("CARGO_BIN_EXE_autofork"))
        .arg("status")
        .env("AUTOFORK_HOME", &env.home)
        .env("AUTOFORK_SOCKET", &env.socket)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("sessions: 5"), "status was: {stdout}");
}

#[test]
fn hook_never_fails_on_garbage_stdin() {
    let env = Env::new("1h");
    for event in [
        "session-start",
        "user-prompt-submit",
        "stop-wait",
        "session-end",
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_autofork"))
            .args(["hook", event])
            .env("AUTOFORK_HOME", &env.home)
            .env("AUTOFORK_SOCKET", &env.socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"not json").unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "hook {event} broke on garbage stdin"
        );
    }
}

/// A mock daemon for the headless runner: answers Hello, the FIRST StopWait
/// with the given wake, every later StopWait with Waited (ends the loop), and
/// records every other frame it receives.
fn mock_headless_daemon(
    socket: PathBuf,
    wake: ResponseBody,
) -> (
    thread::JoinHandle<()>,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) {
    let frames = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = frames.clone();
    let handle = thread::spawn(move || {
        let listener = UnixListener::bind(&socket).unwrap();
        // The hook holds its poll connection open while the runner opens
        // short-lived frame connections concurrently: thread per connection.
        let woke = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let deadline = Instant::now() + Duration::from_secs(30);
        listener.set_nonblocking(true).unwrap();
        let mut workers = Vec::new();
        while Instant::now() < deadline && !done.load(std::sync::atomic::Ordering::SeqCst) {
            let Ok((stream, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            };
            stream.set_nonblocking(false).unwrap();
            let captured = captured.clone();
            let wake = wake.clone();
            let woke = woke.clone();
            let done = done.clone();
            workers.push(thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut writer = stream;
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    let req: Request = match serde_json::from_str(line.trim()) {
                        Ok(r) => r,
                        Err(_) => break,
                    };
                    let body = match &req.body {
                        RequestBody::Hello { .. } => ResponseBody::HelloInfo {
                            version: "999.0.0".into(),
                        },
                        RequestBody::StopWait(_) => {
                            if woke.swap(true, std::sync::atomic::Ordering::SeqCst) {
                                done.store(true, std::sync::atomic::Ordering::SeqCst);
                                ResponseBody::Waited
                            } else {
                                wake.clone()
                            }
                        }
                        other => {
                            captured
                                .lock()
                                .unwrap()
                                .push(serde_json::to_string(other).unwrap());
                            ResponseBody::Ack
                        }
                    };
                    let resp = Response {
                        proto: PROTO_VERSION,
                        id: req.id,
                        body,
                    };
                    let _ = writer.write_all(encode(&resp).unwrap().as_bytes());
                    line.clear();
                }
            }));
        }
        for w in workers {
            let _ = w.join();
        }
    });
    (handle, frames)
}

#[test]
fn headless_wake_runs_forks_and_spools_reports() {
    // The default runner: a wake is consumed by the hook itself — no exit 2,
    // no stderr payload — the fork runs as a subprocess (stubbed here), its
    // report is spooled, and the completion frame follows.
    let env = Env::new("1h");
    // Flip this Env to the headless default and stub the claude binary.
    std::fs::write(
        env.home.join("config.toml"),
        "default_idle_deadline = \"1h\"\nquiet_period = \"1h\"\n",
    )
    .unwrap();
    let stub = env.project.join("claude-stub.sh");
    std::fs::write(
        &stub,
        "#!/bin/sh\nprintf '{\"session_id\":\"fork-1\",\"result\":\"STUB REPORT\",\"is_error\":false}'\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let wake = ResponseBody::Wake {
        payload: "unused by the headless runner".into(),
        forks: Some(vec![autofork_core::protocol::WakeFork {
            name: "journal".into(),
            path: "/x/journal.md".into(),
            trigger: "idle".into(),
            overlap: false,
            after: Vec::new(),
            chain: false,
            model: Some("stub-model".into()),
            model_fallbacks: Vec::new(),
            mode: None,
            prompt: "Read the file /x/journal.md".into(),
        }]),
    };
    let (daemon, frames) = mock_headless_daemon(env.socket.clone(), wake);
    wait_for_socket(&env.socket);

    let mut child = Command::new(env!("CARGO_BIN_EXE_autofork"))
        .args(["hook", "stop-wait"])
        .env("AUTOFORK_HOME", &env.home)
        .env("AUTOFORK_SOCKET", &env.socket)
        .env("AUTOFORK_CLAUDE_BIN", &stub)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(env.hook_input("s-headless").to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "headless never exits 2");
    assert!(
        String::from_utf8_lossy(&out.stderr).trim().is_empty(),
        "headless prints no wake payload"
    );
    daemon.join().unwrap();

    let frames = frames.lock().unwrap().join("\n");
    assert!(frames.contains("\"fork_spawned\""), "{frames}");
    assert!(
        frames.contains("\"spool_report\"") && frames.contains("STUB REPORT"),
        "{frames}"
    );
    assert!(
        frames.contains("\"fork_completed\"") && frames.contains("completed"),
        "{frames}"
    );
}
