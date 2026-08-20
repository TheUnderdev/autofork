//! End-to-end daemon tests: spawn the real daemon binary and drive it over the
//! unix socket with protocol frames. v0.5 forks are never subprocesses — the
//! daemon answers a parked `StopWait` long poll with a wake payload — so these
//! tests assert the *answers* (payload text and timing) rather than any spawn.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use autofork_core::protocol::{
    encode, Event, EventKind, Request, RequestBody, Response, ResponseBody,
};
use autofork_core::PROTO_VERSION;

struct Harness {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    socket: PathBuf,
    project: PathBuf,
    daemon: Option<Child>,
    poll_grace_ms: Option<u64>,
    wake_grace_secs: Option<u64>,
    gate_grace_secs: Option<u64>,
    chain_grace_secs: Option<u64>,
}

impl Harness {
    fn new(idle_deadline: &str, wake_debounce: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        let home = base.join("fsan");
        let project = base.join("proj");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(".autofork/forks")).unwrap();
        std::fs::write(
            home.join("config.toml"),
            format!(
                "default_idle_deadline = \"{idle_deadline}\"\nquiet_period = \"1h\"\nwake_debounce = \"{wake_debounce}\"\n",
            ),
        )
        .unwrap();
        Self {
            socket: base.join("d.sock"),
            _tmp: tmp,
            home,
            project,
            daemon: None,
            poll_grace_ms: None,
            wake_grace_secs: None,
            gate_grace_secs: None,
            chain_grace_secs: None,
        }
    }

    fn poll_grace_ms(mut self, ms: u64) -> Self {
        self.poll_grace_ms = Some(ms);
        self
    }

    fn wake_grace_secs(mut self, secs: u64) -> Self {
        self.wake_grace_secs = Some(secs);
        self
    }

    fn gate_grace_secs(mut self, secs: u64) -> Self {
        self.gate_grace_secs = Some(secs);
        self
    }

    fn chain_grace_secs(mut self, secs: u64) -> Self {
        self.chain_grace_secs = Some(secs);
        self
    }

    /// Append one raw line to the daemon's user config (call before
    /// `start_daemon`).
    fn append_config(&self, line: &str) {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(self.home.join("config.toml"))
            .unwrap();
        writeln!(f, "{line}").unwrap();
    }

    fn write_fork(&self, rel: &str, content: &str) {
        let path = self.project.join(".autofork/forks").join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// Write a lifecycle hook whose command appends one
    /// `event|source|reason|idle_secs|session` line per firing to the
    /// returned log file.
    fn write_logging_hook(&self, rel: &str, on: &str) -> PathBuf {
        let log = self.project.join(format!("{}.log", rel.replace('/', "_")));
        let path = self.project.join(".autofork/hooks").join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            format!(
                "---\nhook: true\non: {on}\n\
                 command: printf '%s\\n' \"$AUTOFORK_EVENT|${{AUTOFORK_SOURCE:-}}|${{AUTOFORK_END_REASON:-}}|${{AUTOFORK_IDLE_SECS:-}}|$AUTOFORK_SESSION_ID\" >> \"{}\"\n\
                 ---\nlease-keeper documentation\n",
                log.display()
            ),
        )
        .unwrap();
        log
    }

    /// Poll `log` until it holds at least `n` lines (hook commands run
    /// asynchronously); panics after `timeout`.
    fn wait_for_hook_lines(&self, log: &PathBuf, n: usize, timeout: Duration) -> Vec<String> {
        let start = Instant::now();
        loop {
            let lines: Vec<String> = std::fs::read_to_string(log)
                .unwrap_or_default()
                .lines()
                .map(|l| l.to_string())
                .collect();
            if lines.len() >= n {
                return lines;
            }
            assert!(
                start.elapsed() < timeout,
                "expected {n} hook lines, have {lines:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn write_transcript(&self, tokens: u64) -> PathBuf {
        let path = self.project.join("transcript.jsonl");
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"assistant\",\"message\":{{\"model\":\"m\",\"usage\":{{\"input_tokens\":{tokens},\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}}}}\n"
            ),
        )
        .unwrap();
        path
    }

    /// Append a further assistant turn to the transcript (the gauge is
    /// byte-offset tracked, so growth must be real appended lines).
    fn append_transcript(&self, tokens: u64) {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(self.project.join("transcript.jsonl"))
            .unwrap();
        writeln!(
            f,
            "{{\"type\":\"assistant\",\"message\":{{\"model\":\"m\",\"usage\":{{\"input_tokens\":{tokens},\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}}}}"
        )
        .unwrap();
    }

    fn start_daemon(&mut self) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_autofork-daemon"));
        cmd.env("AUTOFORK_HOME", &self.home)
            .env("AUTOFORK_SOCKET", &self.socket)
            // Keep the developer's real ~/.claude out of test discovery.
            .env("AUTOFORK_CLAUDE_DIR", self.home.join("claude"))
            .env("AUTOFORK_AGENTS_DIR", self.home.join("agents"))
            .env("RUST_LOG", "debug")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(ms) = self.poll_grace_ms {
            cmd.env("AUTOFORK_POLL_LOSS_GRACE_MS", ms.to_string());
        }
        if let Some(secs) = self.wake_grace_secs {
            cmd.env("AUTOFORK_WAKE_GRACE_SECS", secs.to_string());
        }
        if let Some(secs) = self.gate_grace_secs {
            cmd.env("AUTOFORK_GATE_GRACE_SECS", secs.to_string());
        }
        if let Some(secs) = self.chain_grace_secs {
            cmd.env("AUTOFORK_CHAIN_GRACE_SECS", secs.to_string());
        }
        let child = cmd.spawn().unwrap();
        self.daemon = Some(child);
        let start = Instant::now();
        loop {
            if UnixStream::connect(&self.socket).is_ok() {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "daemon never came up"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn kill_daemon(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn event(&self, kind: EventKind, session: &str) -> Event {
        Event {
            event: kind,
            session_id: session.to_string(),
            transcript_path: None,
            cwd: self.project.clone(),
            project_root: self.project.clone(),
            source: None,
            reason: None,
            model: None,
            enable_tags: None,
            disable_tags: None,
            waking: None,
            notif_tool_use_id: None,
            notif_task_id: None,
            notif_status: None,
            notif_continue: None,
            context_tokens: None,
            context_window: None,
            client: None,
            busy: None,
        }
    }

    /// A waking (`Some(true)`) or non-waking (`Some(false)`) PromptSubmit.
    fn prompt_submit(&self, session: &str, waking: bool) -> Event {
        let mut ev = self.event(EventKind::PromptSubmit, session);
        ev.waking = Some(waking);
        ev
    }

    /// A PromptSubmit carrying a task-notification envelope, as the CLI sends
    /// for `<task-notification>` prompts (coarse `waking: false` plus the ids
    /// the daemon classifies against its spawn registry). Carries the
    /// transcript path, as the real hook does — classification ingests the
    /// transcript delta first.
    fn prompt_submit_notif(&self, session: &str, tool_use_id: &str, status: &str) -> Event {
        let mut ev = self.event_t(EventKind::PromptSubmit, session);
        ev.waking = Some(false);
        ev.notif_tool_use_id = Some(tool_use_id.to_string());
        ev.notif_status = Some(status.to_string());
        ev.notif_continue = Some(false);
        ev
    }

    /// Like [`prompt_submit_notif`], for a report that ended with the chain
    /// sentinel (the CLI sets `notif_continue` from the `<result>` scan).
    fn prompt_submit_notif_cont(&self, session: &str, tool_use_id: &str) -> Event {
        let mut ev = self.prompt_submit_notif(session, tool_use_id, "completed");
        ev.notif_continue = Some(true);
        ev
    }

    /// An event pointing at the project transcript (needed whenever the test
    /// exercises transcript ingestion: spawns, completions, the gauge).
    fn event_t(&self, kind: EventKind, session: &str) -> Event {
        let mut ev = self.event(kind, session);
        ev.transcript_path = Some(self.project.join("transcript.jsonl"));
        ev
    }

    /// Append a raw JSONL line to the transcript.
    fn append_transcript_line(&self, line: &str) {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.project.join("transcript.jsonl"))
            .unwrap();
        writeln!(f, "{line}").unwrap();
    }

    /// Append a fork-spawn Agent tool_use (with the spawn-prompt fingerprint)
    /// to the transcript, as the wake turn would produce.
    fn append_fork_spawn(&self, tool_use_id: &str, fork: &str) {
        let prompt = format!(
            "Read the file /x/{fork}.md and follow the instructions in its body. \
             Context for this run: fork '{fork}', trigger 'idle', parent session s, \
             conversation c, project root /p."
        );
        let line = serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                { "type": "tool_use", "id": tool_use_id, "name": "Agent",
                  "input": { "subagent_type": "fork", "prompt": prompt } },
            ] }
        });
        self.append_transcript_line(&line.to_string());
    }

    /// Append the tool_result of a `run_in_background` Bash launch: the turn
    /// ended, but the session is waiting on work that is still running.
    fn append_background_launch(&self, tool_use_id: &str, task_id: &str) {
        let line = serde_json::json!({
            "type": "user",
            "message": { "content": [
                { "type": "tool_result", "tool_use_id": tool_use_id, "content": [
                    { "type": "text", "text": format!(
                        "Command running in background with ID: {task_id}. Output is being \
                         written to: /tmp/{task_id}.output. You will be notified when it \
                         completes.") },
                ] },
            ] }
        });
        self.append_transcript_line(&line.to_string());
    }

    /// Append a background-task completion notification to the transcript, as
    /// the relay turn's user entry would contain.
    fn append_completion_notification(&self, tool_use_id: &str, status: &str) {
        self.append_completion_notification_result(tool_use_id, status, "report");
    }

    /// Same, with a custom `<result>` payload (e.g. a report ending with the
    /// chain sentinel).
    fn append_completion_notification_result(&self, tool_use_id: &str, status: &str, result: &str) {
        let content = format!(
            "<task-notification>\n<task-id>t-{tool_use_id}</task-id>\n\
             <tool-use-id>{tool_use_id}</tool-use-id>\n<status>{status}</status>\n\
             <summary>Agent \"x\" finished</summary>\n<result>{result}</result>"
        );
        let line = serde_json::json!({
            "type": "user",
            "message": { "content": content }
        });
        self.append_transcript_line(&line.to_string());
    }

    /// One-shot request/response over a fresh connection.
    fn request(&self, body: RequestBody) -> ResponseBody {
        let stream = UnixStream::connect(&self.socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut writer = stream.try_clone().unwrap();
        let req = Request {
            proto: PROTO_VERSION,
            id: 1,
            body,
        };
        writer.write_all(encode(&req).unwrap().as_bytes()).unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str::<Response>(line.trim()).unwrap().body
    }

    fn send_event(&self, ev: Event) -> ResponseBody {
        self.request(RequestBody::Event(ev))
    }

    /// Park a StopWait on its own connection/thread; the result arrives on the
    /// returned channel once the daemon answers (Wake or Waited).
    fn park_stop_wait(&self, ev: Event) -> mpsc::Receiver<ResponseBody> {
        let socket = self.socket.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let stream = UnixStream::connect(&socket).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(60)))
                .unwrap();
            let mut writer = stream.try_clone().unwrap();
            let req = Request {
                proto: PROTO_VERSION,
                id: 1,
                body: RequestBody::StopWait(ev),
            };
            writer.write_all(encode(&req).unwrap().as_bytes()).unwrap();
            let mut line = String::new();
            let body = match BufReader::new(stream).read_line(&mut line) {
                Ok(n) if n > 0 => serde_json::from_str::<Response>(line.trim()).unwrap().body,
                // Socket closed (e.g. daemon exited): model it as Waited.
                _ => ResponseBody::Waited,
            };
            let _ = tx.send(body);
        });
        rx
    }

    fn status_recent_runs(&self) -> usize {
        match self.request(RequestBody::Status) {
            ResponseBody::StatusInfo(info) => info.recent_runs.len(),
            other => panic!("unexpected: {other:?}"),
        }
    }

    fn open_sessions(&self) -> Vec<autofork_core::protocol::SessionInfo> {
        match self.request(RequestBody::Status) {
            ResponseBody::StatusInfo(info) => info.sessions,
            other => panic!("unexpected: {other:?}"),
        }
    }

    fn has_open_session(&self, session: &str) -> bool {
        self.open_sessions().iter().any(|s| s.session_id == session)
    }

    /// Park a StopWait, then drop the connection WITHOUT reading a response —
    /// simulating the Claude process (and its hook subprocess) dying.
    fn drop_stop_wait(&self, ev: Event) {
        let stream = UnixStream::connect(&self.socket).unwrap();
        let mut writer = stream.try_clone().unwrap();
        let req = Request {
            proto: PROTO_VERSION,
            id: 1,
            body: RequestBody::StopWait(ev),
        };
        writer.write_all(encode(&req).unwrap().as_bytes()).unwrap();
        // Give the daemon a moment to read the request and park before we close.
        std::thread::sleep(Duration::from_millis(150));
        drop(writer);
        drop(stream);
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.kill_daemon();
    }
}

fn assert_ack(body: ResponseBody) {
    assert!(
        matches!(body, ResponseBody::Ack),
        "expected ack, got {body:?}"
    );
}

fn wake_payload(body: ResponseBody) -> String {
    match body {
        ResponseBody::Wake { payload, .. } => payload,
        other => panic!("expected a Wake, got {other:?}"),
    }
}

/// The structured due-fork specs riding on a Wake (for opencode clients).
fn wake_forks(body: ResponseBody) -> Vec<autofork_core::protocol::WakeFork> {
    match body {
        ResponseBody::Wake { forks, .. } => forks.expect("wake carries structured forks"),
        other => panic!("expected a Wake, got {other:?}"),
    }
}

#[test]
fn idle_wake_names_the_fork() {
    let mut h = Harness::new("1s", "0");
    h.write_fork(
        "journal.md",
        "---\nfork: true\nrun_on: [idle]\n---\nwrite the journal now",
    );
    h.start_daemon();

    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));

    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("source: autofork"));
    assert!(payload.contains("due: journal (trigger: idle)"));
    assert!(payload.contains("subagent_type \"fork\""));
    assert!(payload.contains("journal.md"));
    assert!(payload.contains("parent session s1"));
    assert!(payload.contains(&format!("project root {}", h.project.display())));
    assert!(payload.contains("Do not read that file yourself"));
    // overlap default false → skip-if-running line.
    assert!(payload.contains("skip spawning it"));
    // A wake was recorded (throttle stamp at issuance).
    assert_eq!(h.status_recent_runs(), 1);
}

#[test]
fn wake_debounce_zero_is_immediate() {
    let mut h = Harness::new("1s", "0");
    h.write_fork("j.md", "---\nfork: true\nrun_on: [idle]\n---\nbody");
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let start = Instant::now();
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(5)).unwrap());
    assert!(payload.contains("due: j"));
    // ~1s idle + no debounce; comfortably under 4s.
    assert!(
        start.elapsed() < Duration::from_secs(4),
        "too slow: {:?}",
        start.elapsed()
    );
}

#[test]
fn prompt_submit_cancels_parked_wait_without_stamping() {
    let mut h = Harness::new("1s", "3");
    h.write_fork("j.md", "---\nfork: true\nrun_on: [idle]\n---\nbody");
    h.start_daemon();

    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    // Let the fork come due (1s) and enter the 3s debounce, then prompt.
    std::thread::sleep(Duration::from_millis(1500));
    assert_ack(h.send_event(h.event(EventKind::PromptSubmit, "s1")));

    let body = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        matches!(body, ResponseBody::Waited),
        "expected Waited, got {body:?}"
    );
    // Cancellation during debounce must not stamp the throttle.
    assert_eq!(h.status_recent_runs(), 0, "throttle stamped despite cancel");
}

#[test]
fn shutdown_resolves_parked_wait() {
    // Long idle so the wait stays parked with nothing due.
    let mut h = Harness::new("1h", "0");
    h.write_fork("j.md", "---\nfork: true\nrun_on: [idle]\n---\nbody");
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    std::thread::sleep(Duration::from_millis(300));
    assert_ack(h.request(RequestBody::Shutdown { drain: false }));
    let body = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        matches!(body, ResponseBody::Waited),
        "expected Waited, got {body:?}"
    );
}

#[test]
fn disable_tag_filters_fork_but_untagged_wakes() {
    let mut h = Harness::new("1s", "0");
    h.write_fork(
        "tagged.md",
        "---\nfork: true\nrun_on: [idle]\ntags: [ci]\n---\nTAGGED",
    );
    h.write_fork("plain.md", "---\nfork: true\nrun_on: [idle]\n---\nPLAIN");
    h.start_daemon();

    let mut start_ev = h.event(EventKind::SessionStart, "s1");
    start_ev.disable_tags = Some(vec!["ci".into()]);
    assert_ack(h.send_event(start_ev));
    let mut stop = h.event(EventKind::Stop, "s1");
    stop.disable_tags = Some(vec!["ci".into()]);
    let rx = h.park_stop_wait(stop);

    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: plain"));
    assert!(
        !payload.contains("due: tagged"),
        "disabled fork leaked: {payload}"
    );
}

#[test]
fn enable_list_excludes_untagged_fork() {
    let mut h = Harness::new("1s", "0");
    h.write_fork(
        "tagged.md",
        "---\nfork: true\nrun_on: [idle]\ntags: [ci]\n---\nTAGGED",
    );
    h.write_fork("plain.md", "---\nfork: true\nrun_on: [idle]\n---\nPLAIN");
    h.start_daemon();

    let mut start_ev = h.event(EventKind::SessionStart, "s1");
    start_ev.enable_tags = Some(vec!["ci".into()]);
    assert_ack(h.send_event(start_ev));
    let mut stop = h.event(EventKind::Stop, "s1");
    stop.enable_tags = Some(vec!["ci".into()]);
    let rx = h.park_stop_wait(stop);

    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: tagged"));
    assert!(
        !payload.contains("due: plain"),
        "untagged fork ran despite whitelist: {payload}"
    );
}

#[test]
fn throttle_suppresses_second_wake() {
    let mut h = Harness::new("1s", "0");
    h.write_fork(
        "j.md",
        "---\nfork: true\nrun_on: [idle]\nthrottle: 1h\n---\nbody",
    );
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));

    // First turn wakes and stamps the throttle.
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let _ = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());

    // Second turn: within the throttle window, nothing is due; the wait parks,
    // then a prompt cancels it (Waited).
    let rx2 = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    std::thread::sleep(Duration::from_millis(1500));
    assert_ack(h.send_event(h.event(EventKind::PromptSubmit, "s1")));
    let body = rx2.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        matches!(body, ResponseBody::Waited),
        "throttled fork woke again: {body:?}"
    );
}

#[test]
fn tag_throttle_suppresses_group_but_other_tag_wakes() {
    let mut h = Harness::new("1s", "0");
    // A ci-throttle of 1h; two ci forks and one docs fork.
    std::fs::write(
        h.home.join("config.toml"),
        "default_idle_deadline = \"1s\"\nquiet_period = \"1h\"\nwake_debounce = \"0\"\n[tag_throttles]\nci = \"1h\"\n",
    )
    .unwrap();
    h.write_fork(
        "a.md",
        "---\nfork: true\nrun_on: [idle]\ntags: [ci]\n---\nA",
    );
    h.write_fork(
        "b.md",
        "---\nfork: true\nrun_on: [idle]\ntags: [ci]\n---\nB",
    );
    h.write_fork(
        "c.md",
        "---\nfork: true\nrun_on: [idle]\ntags: [docs]\n---\nC",
    );
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));

    // First wake: all three fire (no prior ci run yet), stamping the ci group.
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: a") && payload.contains("due: b") && payload.contains("due: c"));

    // A new pause (real user activity) releases the once-per-pause latches, so
    // the second turn is decided by the tag throttle alone: the ci group is
    // still suppressed (throttle holds across pauses); the docs fork (c) wakes.
    assert_ack(h.send_event(h.prompt_submit("s1", true)));
    let rx2 = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let payload = wake_payload(rx2.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(
        payload.contains("due: c"),
        "docs fork should still wake: {payload}"
    );
    assert!(
        !payload.contains("due: a"),
        "ci fork a not throttled: {payload}"
    );
    assert!(
        !payload.contains("due: b"),
        "ci fork b not throttled: {payload}"
    );
}

#[test]
fn after_dependent_held_until_predecessor_completes() {
    let mut h = Harness::new("1s", "0");
    h.write_fork("alpha.md", "---\nfork: true\nrun_on: [idle]\n---\nALPHA");
    h.write_fork(
        "beta.md",
        "---\nfork: true\nrun_on: [idle]\nafter: alpha\n---\nBETA",
    );
    h.start_daemon();
    h.write_transcript(100);
    assert_ack(h.send_event(h.event_t(EventKind::SessionStart, "s1")));

    // Wake 1: alpha spawns now; beta is held by the daemon, not the model.
    let rx = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: alpha"), "{payload}");
    assert!(payload.contains("held back by autofork"), "{payload}");
    assert!(payload.contains("'beta' (after 'alpha')"), "{payload}");
    assert!(!payload.contains("due: beta"), "{payload}");

    // The wake turn spawns alpha; its Stop parks a new poll (which ingests
    // the spawn from the transcript) and stays parked — nothing else is due.
    h.append_fork_spawn("toolu_alpha", "alpha");
    let rx2 = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    std::thread::sleep(Duration::from_millis(400));

    // alpha finishes: its completion notification lands in the transcript and
    // the relay turn's continuation cancels the parked poll.
    h.append_completion_notification("toolu_alpha", "completed");
    assert_ack(h.send_event(h.prompt_submit("s1", false)));
    assert!(matches!(
        rx2.recv_timeout(Duration::from_secs(5)).unwrap(),
        ResponseBody::Waited
    ));

    // The relay turn's own Stop is answered immediately with beta's release.
    let rx3 = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    let release = wake_payload(rx3.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(
        release.contains("due: beta (trigger: idle) — released, 'alpha' finished"),
        "{release}"
    );
    assert!(release.contains("Read the file"), "{release}");
    assert!(release.contains("beta.md"), "{release}");
    assert!(
        release.contains("append the report(s) 'alpha' returned"),
        "{release}"
    );

    // The release is one-shot: the release turn's Stop parks quietly.
    let rx4 = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    assert!(
        rx4.recv_timeout(Duration::from_millis(2500)).is_err(),
        "release fired twice"
    );
}

#[test]
fn priority_layers_forks_into_waves() {
    let mut h = Harness::new("1s", "0");
    // Adversarial naming: the high-priority fork sorts FIRST lexically, so a
    // pass here can't come from incidental roster order.
    h.write_fork(
        "aaa-last.md",
        "---\nfork: true\nrun_on: [idle]\npriority: 10\n---\nLAST",
    );
    h.write_fork(
        "zzz-first.md",
        "---\nfork: true\nrun_on: [idle]\n---\nFIRST",
    );
    h.start_daemon();
    h.write_transcript(100);
    assert_ack(h.send_event(h.event_t(EventKind::SessionStart, "s1")));

    // Wake 1: only the priority-0 fork spawns; the 10 is held for ordering.
    let rx = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    let body = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let payload = wake_payload(body.clone());
    assert!(payload.contains("due: zzz-first"), "{payload}");
    assert!(!payload.contains("due: aaa-last"), "{payload}");
    assert!(payload.contains("held back by autofork"), "{payload}");
    assert!(
        payload.contains("'aaa-last' (after 'zzz-first')"),
        "{payload}"
    );
    let forks = wake_forks(body);
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0].name, "zzz-first");

    // zzz-first completes → aaa-last releases, with ordering wording (no
    // report piping: the priority gate is order-only).
    h.append_fork_spawn("toolu_first", "zzz-first");
    let rx2 = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    std::thread::sleep(Duration::from_millis(400));
    h.append_completion_notification("toolu_first", "completed");
    assert_ack(h.send_event(h.prompt_submit("s1", false)));
    assert!(matches!(
        rx2.recv_timeout(Duration::from_secs(5)).unwrap(),
        ResponseBody::Waited
    ));
    let rx3 = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    let body = rx3.recv_timeout(Duration::from_secs(10)).unwrap();
    let release = wake_payload(body.clone());
    assert!(
        release.contains("due: aaa-last (trigger: idle) — released, earlier forks finished"),
        "{release}"
    );
    assert!(
        release.contains("The forks ordered before this one have finished."),
        "{release}"
    );
    assert!(!release.contains("append the report(s)"), "{release}");
    // The structured spec carries no report preds either.
    let forks = wake_forks(body);
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0].name, "aaa-last");
    assert!(forks[0].after.is_empty());
}

#[test]
fn after_wins_over_priority_and_reports_still_pipe() {
    let mut h = Harness::new("1s", "0");
    // beta declares a LOWER priority than alpha but runs `after: alpha` —
    // the lift keeps it behind alpha, and its release still pipes the report.
    h.write_fork("alpha.md", "---\nfork: true\nrun_on: [idle]\n---\nA");
    h.write_fork(
        "beta.md",
        "---\nfork: true\nrun_on: [idle]\nafter: alpha\npriority: -5\n---\nB",
    );
    h.start_daemon();
    h.write_transcript(100);
    assert_ack(h.send_event(h.event_t(EventKind::SessionStart, "s1")));

    let rx = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: alpha"), "{payload}");
    assert!(!payload.contains("due: beta"), "{payload}");

    h.append_fork_spawn("toolu_alpha", "alpha");
    let rx2 = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    std::thread::sleep(Duration::from_millis(400));
    h.append_completion_notification("toolu_alpha", "completed");
    assert_ack(h.send_event(h.prompt_submit("s1", false)));
    assert!(matches!(
        rx2.recv_timeout(Duration::from_secs(5)).unwrap(),
        ResponseBody::Waited
    ));
    let rx3 = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    let release = wake_payload(rx3.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(
        release.contains("append the report(s) 'alpha' returned"),
        "{release}"
    );
}

#[test]
fn skill_attached_fork_wake_tells_the_fork_to_load_the_skill() {
    let mut h = Harness::new("1s", "0");
    let skill_dir = h.project.join(".claude/skills/feedback");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: feedback\ndescription: d\n---\nskill body",
    )
    .unwrap();
    std::fs::write(
        skill_dir.join("FORK.md"),
        "---\nfork: true\nrun_on: [idle]\n---\napply the skill",
    )
    .unwrap();
    h.start_daemon();
    h.write_transcript(100);
    assert_ack(h.send_event(h.event_t(EventKind::SessionStart, "s1")));

    let rx = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: feedback"), "{payload}");
    assert!(payload.contains("belongs to the skill at"), "{payload}");
    assert!(payload.contains("SKILL.md"), "{payload}");
    assert!(payload.contains("not already in your context"), "{payload}");
}

#[test]
fn foreign_task_completion_starts_a_new_pause() {
    // A background task the daemon didn't spawn finishes → the session picks
    // real work back up → the next pause must re-fire idle forks (this was the
    // "handover never fires again after a background job" bug).
    let mut h = Harness::new("1s", "0").wake_grace_secs(0);
    h.write_fork("journal.md", "---\nfork: true\nrun_on: [idle]\n---\nJ");
    h.start_daemon();
    h.write_transcript(100);
    assert_ack(h.send_event(h.event_t(EventKind::SessionStart, "s1")));

    let rx = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());

    // Wake turn's Stop re-parks; the fork is latched for this pause.
    let rx2 = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    std::thread::sleep(Duration::from_millis(400));

    // A completion notification for a task that is NOT one of our spawns.
    assert_ack(h.send_event(h.prompt_submit_notif("s1", "toolu_users_build", "completed")));
    assert!(matches!(
        rx2.recv_timeout(Duration::from_secs(5)).unwrap(),
        ResponseBody::Waited
    ));

    // New pause: the idle fork fires again.
    let rx3 = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    let payload = wake_payload(rx3.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: journal"), "{payload}");
}

#[test]
fn own_fork_completion_matches_even_without_an_intervening_stop() {
    // Observed live (v0.8.0): the spawn's tool_use was on disk but no
    // stop-wait had ingested it when the completion notification arrived, so
    // the registry was empty, the fork's own completion classified as foreign
    // activity, and the idle fork re-fired every pause — once per fork run,
    // forever. Classification must refresh the registry from the transcript
    // before deciding.
    let mut h = Harness::new("1s", "0").wake_grace_secs(0);
    h.write_fork("journal.md", "---\nfork: true\nrun_on: [idle]\n---\nJ");
    h.start_daemon();
    h.write_transcript(100);
    assert_ack(h.send_event(h.event_t(EventKind::SessionStart, "s1")));

    let rx = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());

    // The spawn lands in the transcript, but NO Stop poll reads it before the
    // fork's completion notification arrives.
    h.append_fork_spawn("toolu_j", "journal");
    assert_ack(h.send_event(h.prompt_submit_notif("s1", "toolu_j", "completed")));

    // Same pause: the relay turn's Stop parks quietly, no re-fire.
    let rx2 = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    assert!(
        rx2.recv_timeout(Duration::from_millis(2500)).is_err(),
        "own fork completion re-fired the idle fork without an intervening Stop"
    );
}

#[test]
fn own_fork_completion_does_not_restart_the_pause() {
    // The counterpart guard: a completion notification for a fork the daemon
    // itself spawned stays a continuation of the same pause — even with the
    // post-wake grace window disabled — so wakes can never feed back.
    let mut h = Harness::new("1s", "0").wake_grace_secs(0);
    h.write_fork("journal.md", "---\nfork: true\nrun_on: [idle]\n---\nJ");
    h.start_daemon();
    h.write_transcript(100);
    assert_ack(h.send_event(h.event_t(EventKind::SessionStart, "s1")));

    let rx = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());

    // The wake turn spawns the fork; the next poll ingests the spawn.
    h.append_fork_spawn("toolu_j", "journal");
    let rx2 = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    std::thread::sleep(Duration::from_millis(400));

    // The fork's own completion notification arrives.
    assert_ack(h.send_event(h.prompt_submit_notif("s1", "toolu_j", "completed")));
    assert!(matches!(
        rx2.recv_timeout(Duration::from_secs(5)).unwrap(),
        ResponseBody::Waited
    ));

    // Same pause: the relay turn's Stop parks quietly, no re-fire.
    let rx3 = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    assert!(
        rx3.recv_timeout(Duration::from_millis(2500)).is_err(),
        "own fork completion re-fired the idle fork"
    );
}

#[test]
fn context_threshold_wakes_and_latches_once() {
    let mut h = Harness::new("1h", "0"); // long idle: only context can fire
    h.write_fork(
        "ctx.md",
        "---\nfork: true\nrun_on:\n  - context_tokens: 1000\n---\ncontext filling",
    );
    h.start_daemon();
    let transcript = h.write_transcript(2000);

    let mut start = h.event(EventKind::SessionStart, "s1");
    start.transcript_path = Some(transcript.clone());
    assert_ack(h.send_event(start));

    let mut stop = h.event(EventKind::Stop, "s1");
    stop.transcript_path = Some(transcript.clone());
    let rx = h.park_stop_wait(stop);
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(
        payload.contains("due: ctx (trigger: context_tokens:1000)"),
        "{payload}"
    );

    // Second turn: latched, must not re-fire → parks (cancelled by a prompt).
    let mut stop2 = h.event(EventKind::Stop, "s1");
    stop2.transcript_path = Some(transcript);
    let rx2 = h.park_stop_wait(stop2);
    std::thread::sleep(Duration::from_millis(400));
    assert_ack(h.send_event(h.event(EventKind::PromptSubmit, "s1")));
    let body = rx2.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        matches!(body, ResponseBody::Waited),
        "context re-fired: {body:?}"
    );
}

#[test]
fn context_used_respects_1m_model_window() {
    let mut h = Harness::new("1h", "0"); // long idle: only context can fire
    h.write_fork(
        "ctx75.md",
        "---\nfork: true\nrun_on:\n  - context_used: 75%\n---\nnearly full",
    );
    h.start_daemon();
    // 300k tokens: over 75% of the default 200k window, well under 75% of 1M.
    let transcript = h.write_transcript(300_000);

    let mut start = h.event(EventKind::SessionStart, "s1");
    start.transcript_path = Some(transcript.clone());
    start.model = Some("claude-opus-4-8[1m]".to_string());
    assert_ack(h.send_event(start));

    // Must NOT wake on a 1M session at 30% usage → parks until cancelled.
    let mut stop = h.event(EventKind::Stop, "s1");
    stop.transcript_path = Some(transcript.clone());
    let rx = h.park_stop_wait(stop);
    std::thread::sleep(Duration::from_millis(400));
    assert_ack(h.send_event(h.prompt_submit("s1", true)));
    let body = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        matches!(body, ResponseBody::Waited),
        "context fired at 30% of a 1M window: {body:?}"
    );

    // Past 75% of 1M the trigger fires.
    h.append_transcript(800_000);
    let mut stop2 = h.event(EventKind::Stop, "s1");
    stop2.transcript_path = Some(transcript);
    let rx2 = h.park_stop_wait(stop2);
    let payload = wake_payload(rx2.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(
        payload.contains("due: ctx75 (trigger: context_used:75%)"),
        "{payload}"
    );
}

#[test]
fn oversized_gauge_bumps_unmarked_window() {
    let mut h = Harness::new("1h", "0");
    h.write_fork(
        "ctx75.md",
        "---\nfork: true\nrun_on:\n  - context_used: 75%\n---\nnearly full",
    );
    h.start_daemon();
    // No model marker anywhere, but the gauge already exceeds 200k: the
    // window must bump to the 1M tier instead of firing at "150%".
    let transcript = h.write_transcript(300_000);

    let mut start = h.event(EventKind::SessionStart, "s1");
    start.transcript_path = Some(transcript.clone());
    assert_ack(h.send_event(start));

    let mut stop = h.event(EventKind::Stop, "s1");
    stop.transcript_path = Some(transcript);
    let rx = h.park_stop_wait(stop);
    std::thread::sleep(Duration::from_millis(400));
    assert_ack(h.send_event(h.prompt_submit("s1", true)));
    let body = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        matches!(body, ResponseBody::Waited),
        "context fired despite oversized-gauge bump: {body:?}"
    );
}

#[test]
fn debounce_batches_forks_across_the_window() {
    // Two idle deadlines 1s apart; a 2s debounce that both land inside.
    let mut h = Harness::new("1s", "2");
    h.write_fork("a.md", "---\nfork: true\nrun_on:\n  - idle: 1\n---\nA");
    h.write_fork("b.md", "---\nfork: true\nrun_on:\n  - idle: 2\n---\nB");
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));

    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    // Both forks in ONE answer, with a single acknowledgment line.
    assert!(payload.contains("due: a (trigger: idle:1)"), "{payload}");
    assert!(payload.contains("due: b (trigger: idle:2)"), "{payload}");
    assert_eq!(payload.matches("After spawning all forks above").count(), 1);
    // Two wakes stamped in one issuance.
    assert_eq!(h.status_recent_runs(), 2);
}

#[test]
fn idle_fork_fires_at_most_once_per_pause() {
    let mut h = Harness::new("1s", "0");
    h.write_fork("j.md", "---\nfork: true\nrun_on: [idle]\n---\nbody");
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));

    // Pause 1: the idle deadline wakes fork j.
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: j"));
    assert_eq!(h.status_recent_runs(), 1);

    // The wake turn runs and ends: a non-waking continuation prompt, then its
    // own Stop re-parks. j is latched for this pause — no second wake, even
    // after the idle deadline elapses again.
    assert_ack(h.send_event(h.prompt_submit("s1", false)));
    let rx2 = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        rx2.try_recv().is_err(),
        "fork re-fired within the same pause"
    );
    // Cancel the still-parked wait (another non-waking prompt).
    assert_ack(h.send_event(h.prompt_submit("s1", false)));
    assert!(matches!(
        rx2.recv_timeout(Duration::from_secs(5)).unwrap(),
        ResponseBody::Waited
    ));
    // Only the single first wake was ever issued.
    assert_eq!(h.status_recent_runs(), 1);

    // Genuine user activity starts a new pause: j is due again.
    assert_ack(h.send_event(h.prompt_submit("s1", true)));
    let rx3 = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let payload = wake_payload(rx3.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(
        payload.contains("due: j"),
        "new pause did not re-arm the fork"
    );
    assert_eq!(h.status_recent_runs(), 2);
}

#[test]
fn ambiguous_prompt_within_grace_is_treated_as_continuation() {
    // The daemon-side belt: a PromptSubmit with no `waking` flag arriving right
    // after a wake is assumed to be a continuation (no epoch advance).
    let mut h = Harness::new("1s", "0");
    h.write_fork("j.md", "---\nfork: true\nrun_on: [idle]\n---\nbody");
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));

    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let _ = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());

    // Ambiguous prompt (waking = None) inside the grace window → non-waking.
    assert_ack(h.send_event(h.event(EventKind::PromptSubmit, "s1")));

    let rx2 = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        rx2.try_recv().is_err(),
        "belt failed: ambiguous prompt advanced the pause"
    );
    assert_ack(h.send_event(h.prompt_submit("s1", false)));
    assert!(matches!(
        rx2.recv_timeout(Duration::from_secs(5)).unwrap(),
        ResponseBody::Waited
    ));
}

#[test]
fn throttle_holds_across_pauses() {
    let mut h = Harness::new("1s", "0");
    h.write_fork(
        "j.md",
        "---\nfork: true\nrun_on: [idle]\nthrottle: 1h\n---\nbody",
    );
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));

    // Pause 1: wake, stamping the 1h throttle.
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let _ = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());

    // A real user prompt starts a fresh pause — but the throttle still holds.
    assert_ack(h.send_event(h.prompt_submit("s1", true)));
    let rx2 = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        rx2.try_recv().is_err(),
        "throttle didn't hold across pauses"
    );
    assert_ack(h.send_event(h.prompt_submit("s1", false)));
    assert!(matches!(
        rx2.recv_timeout(Duration::from_secs(5)).unwrap(),
        ResponseBody::Waited
    ));
}

#[test]
fn lost_poll_closes_session_after_grace() {
    let mut h = Harness::new("1h", "0").poll_grace_ms(400);
    h.write_fork("j.md", "---\nfork: true\nrun_on: [idle]\n---\nbody");
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    assert!(h.has_open_session("s1"));

    // The Claude process dies: its parked poll drops unanswered.
    h.drop_stop_wait(h.event(EventKind::Stop, "s1"));
    // Within the grace it is still open...
    std::thread::sleep(Duration::from_millis(150));
    assert!(h.has_open_session("s1"), "closed before the grace elapsed");
    // ...and after the grace with no fresh event, it is closed.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !h.has_open_session("s1"),
        "lost poll did not close the session"
    );

    // A later event re-opens it via the normal upsert path.
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    assert!(
        h.has_open_session("s1"),
        "a later event did not re-open the session"
    );
}

#[test]
fn event_within_grace_keeps_session_open() {
    let mut h = Harness::new("1h", "0").poll_grace_ms(700);
    h.write_fork("j.md", "---\nfork: true\nrun_on: [idle]\n---\nbody");
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));

    h.drop_stop_wait(h.event(EventKind::Stop, "s1"));
    // A fresh event arrives inside the grace window.
    std::thread::sleep(Duration::from_millis(200));
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    // Past the original grace: the session stays open.
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        h.has_open_session("s1"),
        "grace-close fired despite a fresh event"
    );
}

#[test]
fn answered_poll_never_triggers_grace_close() {
    let mut h = Harness::new("1s", "0").poll_grace_ms(400);
    h.write_fork("j.md", "---\nfork: true\nrun_on: [idle]\n---\nbody");
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));

    // A normally-answered Wake closes its connection afterward — that must NOT
    // count as a lost poll.
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let _ = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    std::thread::sleep(Duration::from_millis(600)); // > grace
    assert!(
        h.has_open_session("s1"),
        "an answered poll wrongly closed the session"
    );
}

#[test]
fn stale_annotation_for_idle_open_session_without_poll() {
    let mut h = Harness::new("1s", "0"); // 2×deadline = 2s
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    // No parked poll; wait comfortably past 2× the idle deadline (whole-second
    // timestamps mean the difference must clear 2 full seconds).
    std::thread::sleep(Duration::from_millis(3300));
    let stale = h
        .open_sessions()
        .into_iter()
        .find(|s| s.session_id == "s1")
        .map(|s| s.stale)
        .unwrap_or(false);
    assert!(
        stale,
        "an old open session with no poll should be flagged stale"
    );
}

#[test]
fn list_forks_marks_only_marked_files_and_status_and_shutdown() {
    let mut h = Harness::new("1h", "0");
    h.write_fork(
        "info/FORK.md",
        "---\nfork: true\ndescription: nested fork\nrun_on: [idle]\nthrottle: 5m\n---\nbody",
    );
    // A companion note with fork-like keys but no marker → warned, not a fork.
    h.write_fork("oops.md", "---\nrun_on: [idle]\n---\nnope");
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));

    match h.request(RequestBody::Status) {
        ResponseBody::StatusInfo(info) => {
            assert_eq!(info.daemon_proto, PROTO_VERSION);
            assert_eq!(info.sessions.len(), 1);
        }
        other => panic!("unexpected: {other:?}"),
    }

    match h.request(RequestBody::ListForks {
        project_root: h.project.clone(),
        cwd: h.project.clone(),
    }) {
        ResponseBody::ForkList { items } => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].name, "info");
            assert_eq!(items[0].throttle_secs, Some(300));
            // The unmarked companion produces a migration warning somewhere.
            let has_warn = items
                .iter()
                .any(|f| f.warnings.iter().any(|w| w.contains("no `fork: true`")));
            assert!(has_warn, "missing fork-like warning: {items:?}");
        }
        other => panic!("unexpected: {other:?}"),
    }

    assert_ack(h.request(RequestBody::Shutdown { drain: true }));
    let start = Instant::now();
    loop {
        if UnixStream::connect(&h.socket).is_err() {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "daemon didn't exit"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    if let Some(mut child) = h.daemon.take() {
        let _ = child.wait();
    }
}

#[test]
fn prune_closes_stale_sessions_only() {
    // 1s idle deadline → stale after >2s idle with no parked poll.
    let mut h = Harness::new("1s", "0");
    h.start_daemon();

    // s1 will go stale: an event, then silence with no parked poll (its
    // Claude process "died mid-turn").
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    // s3 idles just as long but keeps a parked poll → never stale.
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s3")));
    let parked = h.park_stop_wait(h.event(EventKind::Stop, "s3"));
    std::thread::sleep(Duration::from_millis(3200));
    // s2 is freshly active.
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s2")));

    let stale: Vec<String> = h
        .open_sessions()
        .into_iter()
        .filter(|s| s.stale)
        .map(|s| s.session_id)
        .collect();
    assert_eq!(stale, vec!["s1".to_string()], "status stale annotation");

    match h.request(RequestBody::Prune) {
        ResponseBody::Pruned { sessions } => {
            assert_eq!(sessions.len(), 1, "pruned: {sessions:?}");
            assert_eq!(sessions[0].session_id, "s1");
            assert_eq!(sessions[0].status, "closed");
        }
        other => panic!("unexpected: {other:?}"),
    }
    assert!(!h.has_open_session("s1"), "stale session still open");
    assert!(h.has_open_session("s2"), "active session was pruned");
    assert!(h.has_open_session("s3"), "parked session was pruned");

    // Idempotent: nothing left to prune.
    match h.request(RequestBody::Prune) {
        ResponseBody::Pruned { sessions } => assert!(sessions.is_empty(), "{sessions:?}"),
        other => panic!("unexpected: {other:?}"),
    }

    // A later event re-opens a pruned session via the normal upsert path.
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    assert!(h.has_open_session("s1"), "event did not re-open");

    // Unpark s3's poll so its thread ends cleanly.
    assert_ack(h.send_event(h.prompt_submit("s3", true)));
    let _ = parked.recv_timeout(Duration::from_secs(5));
}

// ---- opencode client flow (no transcript; explicit gauge, spawns and
// completions reported as protocol frames; wakes consumed structured) ----

/// An event as the opencode plugin's hook sends it: no transcript, explicit
/// client tag, gauge and model riding on the event itself.
fn oc_event(h: &Harness, kind: EventKind, session: &str) -> Event {
    let mut ev = h.event(kind, session);
    ev.client = Some("opencode".to_string());
    ev
}

#[test]
fn opencode_wake_carries_structured_forks() {
    let mut h = Harness::new("1s", "0");
    h.write_fork(
        "journal.md",
        "---\nfork: true\nrun_on: [idle]\n---\nwrite the journal now",
    );
    h.start_daemon();

    assert_ack(h.send_event(oc_event(&h, EventKind::SessionStart, "oc1")));
    let rx = h.park_stop_wait(oc_event(&h, EventKind::Stop, "oc1"));

    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks.len(), 1);
    let f = &forks[0];
    assert_eq!(f.name, "journal");
    assert_eq!(f.trigger, "idle");
    assert!(!f.overlap);
    assert!(f.after.is_empty());
    assert!(f.path.ends_with("journal.md"), "{}", f.path);
    assert!(f.prompt.contains("Read the file"), "{}", f.prompt);
    assert!(f.prompt.contains(&f.path), "{}", f.prompt);
    assert!(f.prompt.contains("parent session oc1"), "{}", f.prompt);
    assert!(
        f.prompt.contains("Your final message is your report"),
        "{}",
        f.prompt
    );
    // Issued-run bookkeeping works the same as for Claude Code sessions.
    assert_eq!(h.status_recent_runs(), 1);
}

#[test]
fn opencode_context_gauge_rides_on_the_event() {
    let mut h = Harness::new("1h", "0"); // long idle: only context can fire
    h.write_fork(
        "distill.md",
        "---\nfork: true\nrun_on:\n  - context_used: 50%\n---\nDISTILL",
    );
    h.start_daemon();

    assert_ack(h.send_event(oc_event(&h, EventKind::SessionStart, "oc1")));

    // Gauge under the threshold: nothing is due; the poll parks. Cancel it.
    let mut low = oc_event(&h, EventKind::Stop, "oc1");
    low.context_tokens = Some(10_000);
    let rx = h.park_stop_wait(low);
    std::thread::sleep(Duration::from_millis(400));
    assert_ack(h.send_event({
        let mut ev = oc_event(&h, EventKind::PromptSubmit, "oc1");
        ev.waking = Some(true);
        ev
    }));
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        ResponseBody::Waited
    ));

    // Gauge over the threshold (default 200k window): the same poll wakes.
    let mut high = oc_event(&h, EventKind::Stop, "oc1");
    high.context_tokens = Some(150_000);
    let rx = h.park_stop_wait(high);
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0].name, "distill");
    assert_eq!(forks[0].trigger, "context_used:50%");
}

#[test]
fn opencode_reported_window_governs_context_thresholds() {
    // The 1M regression: opencode model ids never carry the `[1m]` marker,
    // so without a reported window the 200k default judged `context_used:
    // 75%` at 150k — 15% of the real 1M window.
    let mut h = Harness::new("1h", "0"); // long idle: only context can fire
    h.write_fork(
        "distill.md",
        "---\nfork: true\nrun_on:\n  - context_used: 75%\n---\nDISTILL",
    );
    h.start_daemon();

    assert_ack(h.send_event(oc_event(&h, EventKind::SessionStart, "oc1")));

    // 150k gauge on a reported 1M window is 15% used: nothing fires, even
    // though it clears 75% of the 200k the heuristic would assume. Cancel
    // the parked poll with a genuine prompt.
    let mut low = oc_event(&h, EventKind::Stop, "oc1");
    low.model = Some("claude-sonnet-4-5".to_string());
    low.context_tokens = Some(150_000);
    low.context_window = Some(1_000_000);
    let rx = h.park_stop_wait(low);
    std::thread::sleep(Duration::from_millis(400));
    assert_ack(h.send_event({
        let mut ev = oc_event(&h, EventKind::PromptSubmit, "oc1");
        ev.waking = Some(true);
        ev
    }));
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        ResponseBody::Waited
    ));

    // 800k of 1M is past the threshold: the poll wakes. The window rides on
    // the session row too, so this poll could even omit it.
    let mut high = oc_event(&h, EventKind::Stop, "oc1");
    high.model = Some("claude-sonnet-4-5".to_string());
    high.context_tokens = Some(800_000);
    high.context_window = Some(1_000_000);
    let rx = h.park_stop_wait(high);
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0].name, "distill");
    assert_eq!(forks[0].trigger, "context_used:75%");
}

#[test]
fn opencode_fork_completion_releases_after_dependent() {
    let mut h = Harness::new("1s", "0");
    h.write_fork("alpha.md", "---\nfork: true\nrun_on: [idle]\n---\nALPHA");
    h.write_fork(
        "beta.md",
        "---\nfork: true\nrun_on: [idle]\nafter: alpha\n---\nBETA",
    );
    h.start_daemon();

    assert_ack(h.send_event(oc_event(&h, EventKind::SessionStart, "oc1")));

    // Wake 1: alpha is the structured root; beta is held daemon-side.
    let rx = h.park_stop_wait(oc_event(&h, EventKind::Stop, "oc1"));
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0].name, "alpha");

    // The plugin forks the session, prompts the copy, and reports the spawn.
    assert_ack(h.request(RequestBody::ForkSpawned {
        session_id: "oc1".into(),
        fork: "alpha".into(),
        run_ref: "ses_fork_alpha".into(),
    }));

    // The session stays idle, so the plugin re-parks immediately.
    let parked = h.park_stop_wait(oc_event(&h, EventKind::Stop, "oc1"));
    std::thread::sleep(Duration::from_millis(400));

    // alpha's fork session finishes; the completion frame nudges the parked
    // poll (resolved Waited) so the plugin re-parks and picks up the release.
    assert_ack(h.request(RequestBody::ForkCompleted {
        session_id: "oc1".into(),
        fork: "alpha".into(),
        run_ref: "ses_fork_alpha".into(),
        status: "completed".into(),
        cont: None,
    }));
    assert!(matches!(
        parked.recv_timeout(Duration::from_secs(5)).unwrap(),
        ResponseBody::Waited
    ));

    // The re-park is answered immediately with beta's release, `after`
    // naming the finished predecessor whose report the plugin appends.
    let rx = h.park_stop_wait(oc_event(&h, EventKind::Stop, "oc1"));
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0].name, "beta");
    assert_eq!(forks[0].after, vec!["alpha".to_string()]);
    assert!(forks[0].prompt.contains("beta.md"));
}

#[test]
fn opencode_fork_run_sessions_are_never_scheduled() {
    // The breeding-loop guard: a fork-run session that slips past the
    // plugin's eligibility check (lost title marker, duplicate plugin
    // instance, event race at creation) reports itself as a real session —
    // the daemon must refuse to register it or schedule forks on it, or its
    // own idle forks would fork it again every deadline.
    let mut h = Harness::new("1s", "0");
    h.write_fork(
        "journal.md",
        "---\nfork: true\nrun_on: [idle]\n---\nJOURNAL",
    );
    h.start_daemon();

    assert_ack(h.send_event(oc_event(&h, EventKind::SessionStart, "oc1")));
    let rx = h.park_stop_wait(oc_event(&h, EventKind::Stop, "oc1"));
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks[0].name, "journal");
    assert_ack(h.request(RequestBody::ForkSpawned {
        session_id: "oc1".into(),
        fork: "journal".into(),
        run_ref: "ses_fork_run".into(),
    }));

    // A confused plugin instance registers the fork session and parks a poll
    // for it. SessionStart is dropped; the poll resolves Waited immediately
    // instead of firing the idle fork on the fork session.
    assert_ack(h.send_event(oc_event(&h, EventKind::SessionStart, "ses_fork_run")));
    assert!(
        !h.has_open_session("ses_fork_run"),
        "fork run was registered"
    );
    let rx = h.park_stop_wait(oc_event(&h, EventKind::Stop, "ses_fork_run"));
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        ResponseBody::Waited
    ));
    assert!(
        !h.has_open_session("ses_fork_run"),
        "fork run was registered"
    );
    // No run was issued beyond the parent's original wake.
    assert_eq!(h.status_recent_runs(), 1);

    // A finished fork run stays unschedulable (the registry keeps terminal
    // rows), and the parent itself is unaffected.
    assert_ack(h.request(RequestBody::ForkCompleted {
        session_id: "oc1".into(),
        fork: "journal".into(),
        run_ref: "ses_fork_run".into(),
        status: "completed".into(),
        cont: None,
    }));
    let rx = h.park_stop_wait(oc_event(&h, EventKind::Stop, "ses_fork_run"));
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        ResponseBody::Waited
    ));
    assert!(h.has_open_session("oc1"), "parent must stay registered");
}

// ---- `every:` interval trigger (turn-boundary and mid-run busy polls) ----

#[test]
fn every_fires_at_turn_boundary_without_a_long_idle() {
    // Idle deadline far away: only `every` can fire this poll.
    let mut h = Harness::new("1h", "0");
    h.write_fork(
        "periodic.md",
        "---\nfork: true\nrun_on:\n  - every: 1s\n---\nPERIODIC",
    );
    h.start_daemon();

    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(
        payload.contains("due: periodic (trigger: every:1)"),
        "{payload}"
    );

    // Within the same quiet pause the interval must NOT re-fire — a quiet
    // session is not a cron. The re-parked poll just stays parked.
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    assert!(
        rx.recv_timeout(Duration::from_millis(2500)).is_err(),
        "every re-fired during a quiet pause"
    );

    // Genuine activity starts a new pause; the next turn boundary fires
    // again once the interval (measured from the last run) has elapsed.
    assert_ack(h.send_event(h.prompt_submit("s1", true)));
    let _ = rx.recv_timeout(Duration::from_secs(5)); // cancelled poll resolves
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(
        payload.contains("due: periodic (trigger: every:1)"),
        "{payload}"
    );
}

#[test]
fn busy_poll_fires_every_but_never_idle() {
    let mut h = Harness::new("1s", "0");
    h.write_fork(
        "idler.md",
        "---\nfork: true\nrun_on:\n  - idle: 1s\n---\nIDLER",
    );
    h.write_fork(
        "periodic.md",
        "---\nfork: true\nrun_on:\n  - every: 1s\n---\nPERIODIC",
    );
    h.start_daemon();

    assert_ack(h.send_event(oc_event(&h, EventKind::SessionStart, "oc1")));
    // A busy (mid-run) poll: idle deadlines must not arm even though the
    // idle fork's 1s deadline would elapse well within the wait.
    let mut ev = oc_event(&h, EventKind::Stop, "oc1");
    ev.busy = Some(true);
    let rx = h.park_stop_wait(ev);
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(
        forks.len(),
        1,
        "only the every fork may fire on a busy poll"
    );
    assert_eq!(forks[0].name, "periodic");
    assert_eq!(forks[0].trigger, "every:1");

    // A subsequent idle poll still fires the idle fork normally.
    let rx = h.park_stop_wait(oc_event(&h, EventKind::Stop, "oc1"));
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(forks.iter().any(|f| f.name == "idler"), "{forks:?}");
}

// ---- chain forks (`chain: true` + the continue sentinel) and gate forks ----

#[test]
fn chain_continue_rearms_the_fork_within_the_pause() {
    let mut h = Harness::new("1s", "0").wake_grace_secs(0);
    h.write_fork(
        "goal.md",
        "---\nfork: true\nrun_on: [idle]\nchain: true\n---\nGOAL",
    );
    h.start_daemon();
    h.write_transcript(100);
    assert_ack(h.send_event(h.event_t(EventKind::SessionStart, "s1")));

    // Wake 1: the spawn prompt teaches the sentinel.
    let rx = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: goal"), "{payload}");
    assert!(payload.contains("<<autofork:continue>>"), "{payload}");

    // The run's report ends with the sentinel (seen via the transcript scan).
    h.append_fork_spawn("toolu_g1", "goal");
    h.append_completion_notification_result(
        "toolu_g1",
        "completed",
        "goal not met, queued more work\n<<autofork:continue>>",
    );
    assert_ack(h.send_event(h.prompt_submit_notif_cont("s1", "toolu_g1")));

    // Same pause: the relay turn's Stop re-fires the fork immediately.
    let rx2 = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    let payload = wake_payload(rx2.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: goal"), "{payload}");

    // Run 2 ends WITHOUT the sentinel: the chain is over, no third wake.
    h.append_fork_spawn("toolu_g2", "goal");
    h.append_completion_notification("toolu_g2", "completed");
    assert_ack(h.send_event(h.prompt_submit_notif("s1", "toolu_g2", "completed")));
    let rx3 = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    assert!(
        rx3.recv_timeout(Duration::from_millis(2500)).is_err(),
        "chain re-fired without the sentinel"
    );
}

#[test]
fn chain_continue_via_prompt_ids_without_transcript_notification() {
    // The notification can reach the PromptSubmit hook before it is flushed
    // to the transcript: the forwarded notif_continue alone must re-arm.
    let mut h = Harness::new("1s", "0").wake_grace_secs(0);
    h.write_fork(
        "goal.md",
        "---\nfork: true\nrun_on: [idle]\nchain: true\n---\nGOAL",
    );
    h.start_daemon();
    h.write_transcript(100);
    assert_ack(h.send_event(h.event_t(EventKind::SessionStart, "s1")));

    let rx = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());

    // Spawn on disk, completion notification NOT on disk yet.
    h.append_fork_spawn("toolu_g1", "goal");
    assert_ack(h.send_event(h.prompt_submit_notif_cont("s1", "toolu_g1")));

    let rx2 = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    let payload = wake_payload(rx2.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: goal"), "{payload}");
}

#[test]
fn chain_sentinel_ignored_without_optin() {
    let mut h = Harness::new("1s", "0").wake_grace_secs(0);
    h.write_fork("journal.md", "---\nfork: true\nrun_on: [idle]\n---\nJ");
    h.start_daemon();
    h.write_transcript(100);
    assert_ack(h.send_event(h.event_t(EventKind::SessionStart, "s1")));

    // A non-chain fork is never taught the sentinel.
    let rx = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(!payload.contains("<<autofork:continue>>"), "{payload}");

    // Even a report that ends with the sentinel must not re-arm it.
    h.append_fork_spawn("toolu_j1", "journal");
    h.append_completion_notification_result(
        "toolu_j1",
        "completed",
        "quoting the docs\n<<autofork:continue>>",
    );
    assert_ack(h.send_event(h.prompt_submit_notif_cont("s1", "toolu_j1")));
    let rx2 = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    assert!(
        rx2.recv_timeout(Duration::from_millis(2500)).is_err(),
        "sentinel re-armed a fork without chain: true"
    );
}

#[test]
fn chain_limit_caps_refires() {
    let mut h = Harness::new("1s", "0").wake_grace_secs(0);
    h.write_fork(
        "goal.md",
        "---\nfork: true\nrun_on: [idle]\nchain: true\nchain_limit: 2\n---\nGOAL",
    );
    h.start_daemon();
    h.write_transcript(100);
    assert_ack(h.send_event(h.event_t(EventKind::SessionStart, "s1")));

    // Wake 1 (run 1) → continue → wake 2 (run 2).
    let rx = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    h.append_fork_spawn("toolu_g1", "goal");
    h.append_completion_notification_result("toolu_g1", "completed", "more\n<<autofork:continue>>");
    assert_ack(h.send_event(h.prompt_submit_notif_cont("s1", "toolu_g1")));
    let rx2 = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    wake_payload(rx2.recv_timeout(Duration::from_secs(10)).unwrap());

    // Run 2 also asks to continue, but the limit (2 runs this pause) is spent.
    h.append_fork_spawn("toolu_g2", "goal");
    h.append_completion_notification_result("toolu_g2", "completed", "more\n<<autofork:continue>>");
    assert_ack(h.send_event(h.prompt_submit_notif_cont("s1", "toolu_g2")));
    let rx3 = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    assert!(
        rx3.recv_timeout(Duration::from_millis(2500)).is_err(),
        "chain exceeded its chain_limit"
    );
}

#[test]
fn opencode_continue_field_rearms_and_settles() {
    let mut h = Harness::new("1s", "0").wake_grace_secs(0);
    h.write_fork(
        "goal.md",
        "---\nfork: true\nrun_on: [idle]\nchain: true\n---\nGOAL",
    );
    h.start_daemon();
    assert_ack(h.send_event(oc_event(&h, EventKind::SessionStart, "oc1")));

    // Wake 1: the structured spec carries the chain flag and the sentinel.
    let rx = h.park_stop_wait(oc_event(&h, EventKind::Stop, "oc1"));
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks[0].name, "goal");
    assert!(forks[0].chain, "structured spec must carry chain");
    assert!(forks[0].prompt.contains("<<autofork:continue>>"));
    assert_ack(h.request(RequestBody::ForkSpawned {
        session_id: "oc1".into(),
        fork: "goal".into(),
        run_ref: "ses_g1".into(),
    }));

    // The completion carries `continue`: the parked poll is nudged and the
    // re-park is answered with the same fork again.
    let parked = h.park_stop_wait(oc_event(&h, EventKind::Stop, "oc1"));
    std::thread::sleep(Duration::from_millis(400));
    assert_ack(h.request(RequestBody::ForkCompleted {
        session_id: "oc1".into(),
        fork: "goal".into(),
        run_ref: "ses_g1".into(),
        status: "completed".into(),
        cont: Some(true),
    }));
    assert!(matches!(
        parked.recv_timeout(Duration::from_secs(5)).unwrap(),
        ResponseBody::Waited
    ));
    let rx2 = h.park_stop_wait(oc_event(&h, EventKind::Stop, "oc1"));
    let forks = wake_forks(rx2.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks[0].name, "goal");

    // Run 2 settles (no continue): the chain is over.
    assert_ack(h.request(RequestBody::ForkSpawned {
        session_id: "oc1".into(),
        fork: "goal".into(),
        run_ref: "ses_g2".into(),
    }));
    assert_ack(h.request(RequestBody::ForkCompleted {
        session_id: "oc1".into(),
        fork: "goal".into(),
        run_ref: "ses_g2".into(),
        status: "completed".into(),
        cont: None,
    }));
    let rx3 = h.park_stop_wait(oc_event(&h, EventKind::Stop, "oc1"));
    assert!(
        rx3.recv_timeout(Duration::from_millis(2500)).is_err(),
        "chain re-fired after settling"
    );
}

#[test]
fn runaway_breaker_stops_an_epoch_pumped_chain() {
    // The incident replay: duplicated opencode session loops (an interrupted
    // stream leaves a zombie loop behind) report autofork's own chain turns
    // as genuine user activity. Every such report bumps the pause epoch —
    // minting a fresh idle latch and resetting the per-pause chain counter —
    // so the goal fork re-fires forever with zero user input, surviving even
    // session close + resume. The wall-clock runaway breaker must stop it.
    let mut h = Harness::new("1s", "0")
        .wake_grace_secs(0)
        .chain_grace_secs(0);
    h.append_config("runaway_limit = 2");
    h.write_fork(
        "goal.md",
        "---\nfork: true\nrun_on: [idle]\nchain: true\n---\nGOAL",
    );
    h.start_daemon();
    assert_ack(h.send_event(oc_event(&h, EventKind::SessionStart, "oc1")));

    let pump = |h: &Harness| {
        // The zombie loop's busy transition: a waking PromptSubmit the chain
        // grace is disabled from downgrading (this test exercises the breaker
        // alone).
        let mut ev = oc_event(h, EventKind::PromptSubmit, "oc1");
        ev.waking = Some(true);
        assert_ack(h.send_event(ev));
    };

    // Cycle 1: wake → run → continue → pump.
    let rx = h.park_stop_wait(oc_event(&h, EventKind::Stop, "oc1"));
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks[0].name, "goal");
    assert_ack(h.request(RequestBody::ForkSpawned {
        session_id: "oc1".into(),
        fork: "goal".into(),
        run_ref: "ses_g1".into(),
    }));
    assert_ack(h.request(RequestBody::ForkCompleted {
        session_id: "oc1".into(),
        fork: "goal".into(),
        run_ref: "ses_g1".into(),
        status: "completed".into(),
        cont: Some(true),
    }));
    pump(&h);

    // Cycle 2: the pumped epoch re-fires the fork (the per-pause counters
    // have been defeated — this is the runaway in motion).
    let rx2 = h.park_stop_wait(oc_event(&h, EventKind::Stop, "oc1"));
    let forks = wake_forks(rx2.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks[0].name, "goal");
    assert_ack(h.request(RequestBody::ForkSpawned {
        session_id: "oc1".into(),
        fork: "goal".into(),
        run_ref: "ses_g2".into(),
    }));
    assert_ack(h.request(RequestBody::ForkCompleted {
        session_id: "oc1".into(),
        fork: "goal".into(),
        run_ref: "ses_g2".into(),
        status: "completed".into(),
        cont: Some(true),
    }));
    pump(&h);

    // Cycle 3: two runs inside the window — the breaker refuses a third no
    // matter how many fresh pauses the pump mints.
    let rx3 = h.park_stop_wait(oc_event(&h, EventKind::Stop, "oc1"));
    assert!(
        rx3.recv_timeout(Duration::from_millis(2500)).is_err(),
        "runaway breaker failed: the epoch-pumped chain re-fired past the cap"
    );
}

#[test]
fn chain_grace_downgrades_duplicate_activity_reports() {
    // A duplicated observer (second plugin instance / duplicated session
    // loop) reports the chain's own injected turn as `waking: true`. Inside
    // the chain grace window that report must be downgraded, so the pause —
    // and with it the per-pause chain limit — survives the duplicate.
    let mut h = Harness::new("1s", "0").wake_grace_secs(0);
    h.write_fork(
        "goal.md",
        "---\nfork: true\nrun_on: [idle]\nchain: true\nchain_limit: 2\n---\nGOAL",
    );
    h.start_daemon();
    assert_ack(h.send_event(oc_event(&h, EventKind::SessionStart, "oc1")));

    let duplicate_report = |h: &Harness| {
        let mut ev = oc_event(h, EventKind::PromptSubmit, "oc1");
        ev.waking = Some(true);
        assert_ack(h.send_event(ev));
    };

    // Cycle 1: wake → run → continue → duplicate waking report (downgraded).
    let rx = h.park_stop_wait(oc_event(&h, EventKind::Stop, "oc1"));
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks[0].name, "goal");
    assert_ack(h.request(RequestBody::ForkSpawned {
        session_id: "oc1".into(),
        fork: "goal".into(),
        run_ref: "ses_g1".into(),
    }));
    assert_ack(h.request(RequestBody::ForkCompleted {
        session_id: "oc1".into(),
        fork: "goal".into(),
        run_ref: "ses_g1".into(),
        status: "completed".into(),
        cont: Some(true),
    }));
    duplicate_report(&h);

    // Cycle 2: the chain re-fires within the SAME pause.
    let rx2 = h.park_stop_wait(oc_event(&h, EventKind::Stop, "oc1"));
    let forks = wake_forks(rx2.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks[0].name, "goal");
    assert_ack(h.request(RequestBody::ForkSpawned {
        session_id: "oc1".into(),
        fork: "goal".into(),
        run_ref: "ses_g2".into(),
    }));
    assert_ack(h.request(RequestBody::ForkCompleted {
        session_id: "oc1".into(),
        fork: "goal".into(),
        run_ref: "ses_g2".into(),
        status: "completed".into(),
        cont: Some(true),
    }));
    duplicate_report(&h);

    // chain_limit (2 per pause) now binds, because the duplicates were
    // downgraded and the pause was never reset. Without the grace the second
    // duplicate would have minted a fresh pause and the chain would re-fire.
    let rx3 = h.park_stop_wait(oc_event(&h, EventKind::Stop, "oc1"));
    assert!(
        rx3.recv_timeout(Duration::from_millis(2500)).is_err(),
        "duplicate activity report reset the pause and defeated chain_limit"
    );
}

#[test]
fn overlap_false_holds_across_pause_resets() {
    // Daemon-side overlap gate: with a run of the fork still in flight, a new
    // pause (fresh epoch, fresh idle latch) must not fire it again. The
    // client-side overlap gates live in plugin-instance memory and multiply
    // with duplicated instances; the spawn registry is the copy that counts.
    let mut h = Harness::new("1s", "0").wake_grace_secs(0);
    h.write_fork("journal.md", "---\nfork: true\nrun_on: [idle]\n---\nJ");
    h.start_daemon();
    assert_ack(h.send_event(oc_event(&h, EventKind::SessionStart, "oc1")));

    // Wake 1, run in flight (spawned, not completed).
    let rx = h.park_stop_wait(oc_event(&h, EventKind::Stop, "oc1"));
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks[0].name, "journal");
    assert_ack(h.request(RequestBody::ForkSpawned {
        session_id: "oc1".into(),
        fork: "journal".into(),
        run_ref: "ses_j1".into(),
    }));

    // Genuine user activity starts a new pause; the fresh latch would
    // normally let the fork fire again — the live spawn must block it.
    let mut ev = oc_event(&h, EventKind::PromptSubmit, "oc1");
    ev.waking = Some(true);
    assert_ack(h.send_event(ev));
    let rx2 = h.park_stop_wait(oc_event(&h, EventKind::Stop, "oc1"));
    assert!(
        rx2.recv_timeout(Duration::from_millis(2500)).is_err(),
        "overlap: false fork re-fired while its run was still in flight"
    );

    // The run settles: the next pause fires it again.
    assert_ack(h.request(RequestBody::ForkCompleted {
        session_id: "oc1".into(),
        fork: "journal".into(),
        run_ref: "ses_j1".into(),
        status: "completed".into(),
        cont: None,
    }));
    let mut ev = oc_event(&h, EventKind::PromptSubmit, "oc1");
    ev.waking = Some(true);
    assert_ack(h.send_event(ev));
    let rx3 = h.park_stop_wait(oc_event(&h, EventKind::Stop, "oc1"));
    let forks = wake_forks(rx3.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks[0].name, "journal");
}

#[test]
fn gate_holds_idle_forks_until_chain_settles() {
    let mut h = Harness::new("1s", "0").wake_grace_secs(0);
    h.write_fork(
        "goal.md",
        "---\nfork: true\nrun_on: [idle: 0s]\nchain: true\ngate: true\n---\nGOAL",
    );
    h.write_fork("handover.md", "---\nfork: true\nrun_on: [idle: 1s]\n---\nH");
    h.start_daemon();
    h.write_transcript(100);
    assert_ack(h.send_event(h.event_t(EventKind::SessionStart, "s1")));

    // Wake 1: the goal fork fires right at the Stop; handover is not due yet.
    let rx = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: goal"), "{payload}");
    assert!(!payload.contains("due: handover"), "{payload}");

    // Handover's deadline elapses mid-run, but the gate holds it.
    h.append_fork_spawn("toolu_g1", "goal");
    let rx2 = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    assert!(
        rx2.recv_timeout(Duration::from_millis(2500)).is_err(),
        "gate failed to hold the idle fork mid-chain"
    );

    // Run 1 continues the chain: the next wake is the goal fork again, and
    // handover stays held even though its deadline has long elapsed.
    h.append_completion_notification_result("toolu_g1", "completed", "more\n<<autofork:continue>>");
    assert_ack(h.send_event(h.prompt_submit_notif_cont("s1", "toolu_g1")));
    let rx3 = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    let payload = wake_payload(rx3.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: goal"), "{payload}");
    assert!(!payload.contains("due: handover"), "{payload}");

    // Run 2 settles the chain: the gate clears, the pause baseline resets,
    // and handover fires at its own deadline measured from the next Stop.
    h.append_fork_spawn("toolu_g2", "goal");
    h.append_completion_notification("toolu_g2", "completed");
    assert_ack(h.send_event(h.prompt_submit_notif("s1", "toolu_g2", "completed")));
    let rx4 = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    let payload = wake_payload(rx4.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: handover"), "{payload}");
    assert!(!payload.contains("due: goal"), "{payload}");
}

#[test]
fn gate_belt_lifts_a_fumbled_wake() {
    // A gate wake the model never acted on (no spawn observed) must not
    // silence the other idle forks for the whole pause: the belt lifts the
    // gate after the grace window, from the parked poll itself.
    let mut h = Harness::new("1s", "0")
        .wake_grace_secs(0)
        .gate_grace_secs(1);
    h.write_fork(
        "goal.md",
        "---\nfork: true\nrun_on: [idle: 0s]\nchain: true\ngate: true\n---\nGOAL",
    );
    h.write_fork("handover.md", "---\nfork: true\nrun_on: [idle: 1s]\n---\nH");
    h.start_daemon();
    h.write_transcript(100);
    assert_ack(h.send_event(h.event_t(EventKind::SessionStart, "s1")));

    let rx = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: goal"), "{payload}");

    // No spawn ever lands. The next poll first holds handover, then the
    // grace expires and the belt lifts the gate — handover fires.
    let rx2 = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    let payload = wake_payload(rx2.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: handover"), "{payload}");
    assert!(!payload.contains("due: goal"), "{payload}");
}

#[test]
fn lifecycle_hooks_fire_across_the_session_life() {
    // Long default idle deadline so no fork machinery interferes; the hook
    // carries its own explicit idle duration.
    let mut h = Harness::new("30m", "0");
    let log = h.write_logging_hook(
        "lease.md",
        "[session_start, activity, \"idle: 1s\", session_end]",
    );
    h.start_daemon();

    let mut start = h.event(EventKind::SessionStart, "s1");
    start.source = Some("startup".into());
    assert_ack(h.send_event(start));
    let lines = h.wait_for_hook_lines(&log, 1, Duration::from_secs(5));
    assert_eq!(lines[0], "session_start|startup|||s1");

    // A repeat SessionStart for the same open session is not a new edge.
    let mut again = h.event(EventKind::SessionStart, "s1");
    again.source = Some("compact".into());
    assert_ack(h.send_event(again));

    assert_ack(h.send_event(h.prompt_submit("s1", true)));
    let lines = h.wait_for_hook_lines(&log, 2, Duration::from_secs(5));
    assert_eq!(lines[1], "activity||||s1");

    // Park the idle poll: the idle hook fires after ~1s while the session
    // stays open — the poll must NOT resolve (no forks are due).
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let lines = h.wait_for_hook_lines(&log, 3, Duration::from_secs(10));
    assert_eq!(lines[2], "idle|||1|s1");
    assert!(
        rx.try_recv().is_err(),
        "idle hook firing must not resolve the parked poll"
    );

    // A clean SessionEnd carries the client-reported reason.
    let mut end = h.event(EventKind::SessionEnd, "s1");
    end.reason = Some("logout".into());
    assert_ack(h.send_event(end));
    let lines = h.wait_for_hook_lines(&log, 4, Duration::from_secs(5));
    assert_eq!(lines[3], "session_end||logout||s1");

    // A second SessionEnd is not a new transition: no fifth line.
    assert_ack(h.send_event(h.event(EventKind::SessionEnd, "s1")));
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        h.wait_for_hook_lines(&log, 4, Duration::from_secs(1)).len(),
        4
    );
}

#[test]
fn idle_hook_fires_once_per_pause_and_rearms_on_activity() {
    let mut h = Harness::new("30m", "0");
    let log = h.write_logging_hook("park.md", "[\"idle: 1s\"]");
    h.start_daemon();

    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let lines = h.wait_for_hook_lines(&log, 1, Duration::from_secs(10));
    assert_eq!(lines.len(), 1);

    // A non-waking continuation (a wake turn ending) re-parks without a new
    // pause: the hook is latched and must not fire again.
    assert_ack(h.send_event(h.prompt_submit("s1", false)));
    let _ = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let _rx2 = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    std::thread::sleep(Duration::from_millis(1800));
    assert_eq!(
        h.wait_for_hook_lines(&log, 1, Duration::from_secs(1)).len(),
        1
    );

    // Genuine activity starts a new pause: the next idle fires the hook again.
    assert_ack(h.send_event(h.prompt_submit("s1", true)));
    let _rx3 = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let lines = h.wait_for_hook_lines(&log, 2, Duration::from_secs(10));
    assert_eq!(lines.len(), 2);
}

#[test]
fn poll_loss_close_fires_session_end_with_reason_lost() {
    let mut h = Harness::new("1s", "0").poll_grace_ms(300);
    let log = h.write_logging_hook("cleanup.md", "[session_end]");
    h.start_daemon();

    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    // The Claude process dies: its parked poll drops unanswered. After the
    // grace the daemon closes the session — the crash-adjacent fallback path
    // (a true SIGKILL/power loss may fire nothing at all; lease TTLs stay
    // the last line of defense).
    h.drop_stop_wait(h.event(EventKind::Stop, "s1"));
    let lines = h.wait_for_hook_lines(&log, 1, Duration::from_secs(10));
    assert_eq!(lines[0], "session_end||lost||s1");
}

#[test]
fn resume_hook_fires_only_for_the_resume_source() {
    let mut h = Harness::new("30m", "0");
    let log = h.write_logging_hook("rejoin.md", "[resume]");
    h.start_daemon();

    let mut startup = h.event(EventKind::SessionStart, "s1");
    startup.source = Some("startup".into());
    assert_ack(h.send_event(startup));
    // A resumed session arrives as a NEW session id with source: resume.
    let mut resume = h.event(EventKind::SessionStart, "s2");
    resume.source = Some("resume".into());
    assert_ack(h.send_event(resume));

    let lines = h.wait_for_hook_lines(&log, 1, Duration::from_secs(5));
    assert_eq!(lines, vec!["resume|resume|||s2".to_string()]);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        h.wait_for_hook_lines(&log, 1, Duration::from_secs(1)).len(),
        1
    );
}

// ---- codex client flow (same wire shape as opencode: no transcript,
// explicit gauge, fork frames; the waiter consumes wakes structured) ----

/// An event as the codex waiter/hooks send it: client tag "codex", UUIDv7
/// session ids, gauge and window riding on the event.
fn cx_event(h: &Harness, kind: EventKind, session: &str) -> Event {
    let mut ev = h.event(kind, session);
    ev.client = Some("codex".to_string());
    ev
}

#[test]
fn codex_wake_carries_structured_forks_and_gauge() {
    let mut h = Harness::new("1s", "0");
    h.write_fork(
        "journal.md",
        "---\nfork: true\nrun_on: [idle]\n---\nwrite the journal now",
    );
    h.write_fork(
        "distill.md",
        "---\nfork: true\nrun_on:\n  - context_used: 50%\n---\nDISTILL",
    );
    h.start_daemon();

    let sid = "01a01f24-3113-76c3-a00a-74ac3948e630";
    assert_ack(h.send_event(cx_event(&h, EventKind::SessionStart, sid)));
    // The waiter's idle poll carries the rollout-derived gauge and codex's
    // own model_context_window; both trigger kinds resolve off it.
    let mut ev = cx_event(&h, EventKind::Stop, sid);
    ev.context_tokens = Some(160_000);
    ev.context_window = Some(258_400);
    // 160k of a 258.4k reported window is past 50%: the context fork fires
    // on the very poll that reported the gauge, ahead of the idle deadline.
    let rx = h.park_stop_wait(ev.clone());
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0].name, "distill");
    assert_eq!(forks[0].trigger, "context_used:50%");
    // The waiter re-parks; the idle fork fires at its deadline.
    let rx = h.park_stop_wait(ev);
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0].name, "journal");
    assert!(forks[0].prompt.contains(&format!("parent session {sid}")));
    assert_eq!(h.status_recent_runs(), 2);
}

#[test]
fn codex_chain_grace_downgrades_duplicate_activity() {
    // The chain-grace downgrade is keyed on native-execution clients, not on
    // opencode alone: a codex queue-drained report turn that fires a waking
    // UserPromptSubmit (sniff missed, duplicated observer) inside the grace
    // window must not reset the pause, or chain_limit never binds.
    let mut h = Harness::new("1s", "0").wake_grace_secs(0);
    h.write_fork(
        "goal.md",
        "---\nfork: true\nrun_on: [idle]\nchain: true\nchain_limit: 2\n---\nGOAL",
    );
    h.start_daemon();
    let sid = "01a01f24-aaaa-76c3-a00a-74ac3948e630";
    assert_ack(h.send_event(cx_event(&h, EventKind::SessionStart, sid)));

    let duplicate_report = |h: &Harness| {
        let mut ev = cx_event(h, EventKind::PromptSubmit, sid);
        ev.waking = Some(true);
        assert_ack(h.send_event(ev));
    };

    for run in [
        "01a01f30-0001-7000-8000-000000000001",
        "01a01f30-0002-7000-8000-000000000002",
    ] {
        let rx = h.park_stop_wait(cx_event(&h, EventKind::Stop, sid));
        let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
        assert_eq!(forks[0].name, "goal");
        assert_ack(h.request(RequestBody::ForkSpawned {
            session_id: sid.into(),
            fork: "goal".into(),
            run_ref: run.into(),
        }));
        assert_ack(h.request(RequestBody::ForkCompleted {
            session_id: sid.into(),
            fork: "goal".into(),
            run_ref: run.into(),
            status: "completed".into(),
            cont: Some(true),
        }));
        duplicate_report(&h);
    }

    // chain_limit (2 per pause) binds because the duplicates were downgraded.
    let rx = h.park_stop_wait(cx_event(&h, EventKind::Stop, sid));
    assert!(
        rx.recv_timeout(Duration::from_millis(2500)).is_err(),
        "duplicate activity report reset the pause and defeated chain_limit"
    );
}

#[test]
fn codex_fork_run_sessions_are_never_scheduled() {
    // The recursion env guard on the `codex exec fork` child is the primary
    // defense; the daemon's spawn registry is the backstop that survives a
    // waiter restart or a hook environment that lost the guard vars.
    let mut h = Harness::new("1s", "0");
    h.write_fork("journal.md", "---\nfork: true\nrun_on: [idle]\n---\nJ");
    h.start_daemon();

    let sid = "01a01f24-bbbb-76c3-a00a-74ac3948e630";
    let run = "01a01f30-cccc-7000-8000-000000000001";
    assert_ack(h.send_event(cx_event(&h, EventKind::SessionStart, sid)));
    let rx = h.park_stop_wait(cx_event(&h, EventKind::Stop, sid));
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks[0].name, "journal");
    assert_ack(h.request(RequestBody::ForkSpawned {
        session_id: sid.into(),
        fork: "journal".into(),
        run_ref: run.into(),
    }));

    // The fork child's own SessionStart hook fires (env guard lost): the
    // daemon must drop it and answer its polls Waited immediately.
    assert_ack(h.send_event(cx_event(&h, EventKind::SessionStart, run)));
    assert!(!h.has_open_session(run), "fork run was registered");
    let rx = h.park_stop_wait(cx_event(&h, EventKind::Stop, run));
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        ResponseBody::Waited
    ));
    assert_eq!(h.status_recent_runs(), 1);
}

#[test]
fn wake_forks_carry_resolved_model_and_mode() {
    // Fork frontmatter wins; config [fork_models]/[fork_modes] fills per
    // client; a client the map doesn't name inherits (None).
    let mut h = Harness::new("1s", "0");
    h.append_config("[fork_models]");
    h.append_config("codex = \"gpt-5.1-codex-mini\"");
    h.append_config("\"claude-code\" = \"haiku\"");
    h.write_fork(
        "journal.md",
        "---\nfork: true\nrun_on: [idle]\nmodel:\n  opencode: anthropic/claude-haiku-4-5\nmode:\n  codex: workspace-write\n---\nJ",
    );
    h.start_daemon();

    // opencode session: frontmatter names its model; no mode → None.
    assert_ack(h.send_event(oc_event(&h, EventKind::SessionStart, "oc-m")));
    let rx = h.park_stop_wait(oc_event(&h, EventKind::Stop, "oc-m"));
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(
        forks[0].model.as_deref(),
        Some("anthropic/claude-haiku-4-5")
    );
    assert_eq!(forks[0].mode, None);

    // codex session: frontmatter has no codex model → config fallback; mode
    // from frontmatter.
    let sid = "01a01f24-dddd-76c3-a00a-74ac3948e630";
    assert_ack(h.send_event(cx_event(&h, EventKind::SessionStart, sid)));
    let rx = h.park_stop_wait(cx_event(&h, EventKind::Stop, sid));
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks[0].model.as_deref(), Some("gpt-5.1-codex-mini"));
    assert_eq!(forks[0].mode.as_deref(), Some("workspace-write"));

    // Claude Code session (no client tag): config fallback for its model.
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "cc1")));
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "cc1"));
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks[0].model.as_deref(), Some("haiku"));
}

#[test]
fn codex_peek_due_runs_the_goal_fast_path() {
    // PeekDue selects-and-stamps only chain forks due at idle:0s; the
    // non-chain idle:0s fork stays unstamped for the parked poll. A cont
    // completion re-arms the chain for the next PeekDue (the loop); a
    // settle leaves the next PeekDue empty.
    let mut h = Harness::new("1h", "0"); // long default idle: only idle:0s fires
    h.write_fork(
        "goal.md",
        "---\nfork: true\nrun_on:\n  - idle: 0s\nchain: true\n---\nGOAL",
    );
    h.write_fork(
        "note.md",
        "---\nfork: true\nrun_on:\n  - idle: 0s\n---\nNOTE",
    );
    h.start_daemon();
    let sid = "01a01f24-eeee-76c3-a00a-74ac3948e630";
    assert_ack(h.send_event(cx_event(&h, EventKind::SessionStart, sid)));

    // Iteration 1.
    let ResponseBody::Due { forks } = h.request(RequestBody::PeekDue {
        session_id: sid.into(),
    }) else {
        panic!("expected Due");
    };
    assert_eq!(forks.len(), 1, "only the chain fork rides the fast path");
    assert_eq!(forks[0].name, "goal");
    assert!(forks[0].chain);
    assert_ack(h.request(RequestBody::ForkSpawned {
        session_id: sid.into(),
        fork: "goal".into(),
        run_ref: "01a01f30-aaaa-7000-8000-000000000001".into(),
    }));
    assert_ack(h.request(RequestBody::ForkCompleted {
        session_id: sid.into(),
        fork: "goal".into(),
        run_ref: "01a01f30-aaaa-7000-8000-000000000001".into(),
        status: "completed".into(),
        cont: Some(true),
    }));

    // Iteration 2: the cont re-armed the latch.
    let ResponseBody::Due { forks } = h.request(RequestBody::PeekDue {
        session_id: sid.into(),
    }) else {
        panic!("expected Due");
    };
    assert_eq!(forks.len(), 1);
    assert_ack(h.request(RequestBody::ForkSpawned {
        session_id: sid.into(),
        fork: "goal".into(),
        run_ref: "01a01f30-aaaa-7000-8000-000000000002".into(),
    }));
    assert_ack(h.request(RequestBody::ForkCompleted {
        session_id: sid.into(),
        fork: "goal".into(),
        run_ref: "01a01f30-aaaa-7000-8000-000000000002".into(),
        status: "completed".into(),
        cont: None, // settle
    }));

    // Settled: nothing due on the fast path any more this pause.
    let ResponseBody::Due { forks } = h.request(RequestBody::PeekDue {
        session_id: sid.into(),
    }) else {
        panic!("expected Due");
    };
    assert!(forks.is_empty(), "settled chain must not re-fire");

    // The non-chain idle:0s fork was never stamped: the regular parked poll
    // still fires it.
    let rx = h.park_stop_wait(cx_event(&h, EventKind::Stop, sid));
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0].name, "note");
}

#[test]
fn spooled_reports_deliver_once_in_order() {
    let mut h = Harness::new("1h", "0");
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "cc-spool")));
    assert_ack(h.request(RequestBody::SpoolReport {
        session_id: "cc-spool".into(),
        fork: "journal".into(),
        text: "first".into(),
    }));
    assert_ack(h.request(RequestBody::SpoolReport {
        session_id: "cc-spool".into(),
        fork: "notes".into(),
        text: "second".into(),
    }));
    let ResponseBody::Reports { blocks } = h.request(RequestBody::TakeReports {
        session_id: "cc-spool".into(),
    }) else {
        panic!("expected Reports");
    };
    assert_eq!(blocks, vec!["first".to_string(), "second".to_string()]);
    let ResponseBody::Reports { blocks } = h.request(RequestBody::TakeReports {
        session_id: "cc-spool".into(),
    }) else {
        panic!("expected Reports");
    };
    assert!(blocks.is_empty(), "taking clears the spool");
}

#[test]
fn codex_poll_reserves_idle_zero_chain_forks_for_peek_due() {
    // The waiter's parked poll and the Stop hook's PeekDue both fire at the
    // pause's first Stop; the poll must leave `idle: 0s` chain forks to the
    // fast path (a poll win would strand the goal loop on the queue path).
    let mut h = Harness::new("1h", "0");
    h.write_fork(
        "goal.md",
        "---\nfork: true\nrun_on:\n  - idle: 0s\nchain: true\n---\nGOAL",
    );
    h.start_daemon();
    let sid = "01a01f24-ffff-76c3-a00a-74ac3948e630";
    assert_ack(h.send_event(cx_event(&h, EventKind::SessionStart, sid)));

    // The parked poll must NOT fire the goal fork...
    let rx = h.park_stop_wait(cx_event(&h, EventKind::Stop, sid));
    assert!(
        rx.recv_timeout(Duration::from_millis(2500)).is_err(),
        "the poll grabbed a fast-path fork"
    );
    // ...and the latch is untouched, so PeekDue still gets it.
    let ResponseBody::Due { forks } = h.request(RequestBody::PeekDue {
        session_id: sid.into(),
    }) else {
        panic!("expected Due");
    };
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0].name, "goal");

    // opencode sessions are NOT reserved (no fast path there): same fork
    // definition fires on the poll.
    assert_ack(h.send_event(oc_event(&h, EventKind::SessionStart, "oc-goal")));
    let rx = h.park_stop_wait(oc_event(&h, EventKind::Stop, "oc-goal"));
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks[0].name, "goal");
}

#[test]
fn take_final_runs_flushes_only_unrun_idle_forks_in_order() {
    // flush_on_close: forks that already fired this pause stay fired; the
    // rest come back stamped, topologically ordered, with report-piping
    // `after` preds filled in. A second take returns nothing.
    let mut h = Harness::new("1s", "0");
    h.write_fork("early.md", "---\nfork: true\nrun_on:\n  - idle: 1s\n---\nE");
    h.write_fork(
        "journal.md",
        "---\nfork: true\nrun_on:\n  - idle: 30m\n---\nJ",
    );
    h.write_fork(
        "handover.md",
        "---\nfork: true\nrun_on:\n  - idle: 30m\nafter: [journal]\n---\nH",
    );
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "cc-flush")));

    // The 1s fork fires normally and is latched for this pause.
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "cc-flush"));
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0].name, "early");

    // Close-time flush: only the unrun 30m forks, journal before its
    // dependent, the dependent carrying the report-piping pred.
    let ResponseBody::Due { forks } = h.request(RequestBody::TakeFinalRuns {
        session_id: "cc-flush".into(),
    }) else {
        panic!("expected Due");
    };
    assert_eq!(
        forks.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
        vec!["journal", "handover"]
    );
    assert!(forks[0].after.is_empty());
    assert_eq!(forks[1].after, vec!["journal".to_string()]);
    assert!(
        forks[0].trigger.contains("at close"),
        "{}",
        forks[0].trigger
    );

    // Everything is stamped now: a second take is empty.
    let ResponseBody::Due { forks } = h.request(RequestBody::TakeFinalRuns {
        session_id: "cc-flush".into(),
    }) else {
        panic!("expected Due");
    };
    assert!(forks.is_empty(), "final runs must stamp what they hand out");
}

#[test]
fn wake_forks_carry_model_fallback_lists() {
    let mut h = Harness::new("1s", "0");
    h.append_config("[fork_models]");
    h.append_config("codex = [\"gpt-5.6-luna\", \"gpt-5.5\"]");
    h.write_fork(
        "journal.md",
        "---\nfork: true\nrun_on: [idle]\nmodel:\n  opencode: [github-copilot/gemini-3.7-flash, anthropic/claude-haiku-4-5]\n---\nJ",
    );
    h.start_daemon();

    // Frontmatter list on opencode.
    assert_ack(h.send_event(oc_event(&h, EventKind::SessionStart, "oc-fb")));
    let rx = h.park_stop_wait(oc_event(&h, EventKind::Stop, "oc-fb"));
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(
        forks[0].model.as_deref(),
        Some("github-copilot/gemini-3.7-flash")
    );
    assert_eq!(
        forks[0].model_fallbacks,
        vec!["anthropic/claude-haiku-4-5".to_string()]
    );

    // Config array on codex.
    let sid = "01a01f24-abcd-76c3-a00a-74ac3948e630";
    assert_ack(h.send_event(cx_event(&h, EventKind::SessionStart, sid)));
    let rx = h.park_stop_wait(cx_event(&h, EventKind::Stop, sid));
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks[0].model.as_deref(), Some("gpt-5.6-luna"));
    assert_eq!(forks[0].model_fallbacks, vec!["gpt-5.5".to_string()]);
}

#[test]
fn fork_models_resolve_by_parent_model_with_default_catchall() {
    // [fork_models.<client>] can be a table keyed by the SESSION's own
    // (parent) model, with "default" as the catch-all for a parent model
    // with no explicit entry -- lets a big/expensive parent point fork runs
    // at a different (cheaper) model than a small/cheap parent does.
    let mut h = Harness::new("1s", "0");
    h.append_config("[fork_models.\"claude-code\"]");
    h.append_config("opus = \"sonnet\"");
    h.append_config("fable = [\"sonnet\", \"haiku\"]");
    h.append_config("default = \"haiku\"");
    h.write_fork("journal.md", "---\nfork: true\nrun_on: [idle]\n---\nJ");
    h.start_daemon();

    // Claude Code session (no client tag) reporting "opus" as its model:
    // exact match in the by-parent-model table.
    let mut start = h.event(EventKind::SessionStart, "cc-opus");
    start.model = Some("opus".to_string());
    assert_ack(h.send_event(start));
    let mut stop = h.event(EventKind::Stop, "cc-opus");
    stop.model = Some("opus".to_string());
    let rx = h.park_stop_wait(stop);
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks[0].model.as_deref(), Some("sonnet"));
    assert!(forks[0].model_fallbacks.is_empty());

    // "fable" parent: fallback list.
    let mut start = h.event(EventKind::SessionStart, "cc-fable");
    start.model = Some("fable".to_string());
    assert_ack(h.send_event(start));
    let mut stop = h.event(EventKind::Stop, "cc-fable");
    stop.model = Some("fable".to_string());
    let rx = h.park_stop_wait(stop);
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks[0].model.as_deref(), Some("sonnet"));
    assert_eq!(forks[0].model_fallbacks, vec!["haiku".to_string()]);

    // Unlisted parent model: falls through to "default".
    let mut start = h.event(EventKind::SessionStart, "cc-other");
    start.model = Some("some-future-model".to_string());
    assert_ack(h.send_event(start));
    let mut stop = h.event(EventKind::Stop, "cc-other");
    stop.model = Some("some-future-model".to_string());
    let rx = h.park_stop_wait(stop);
    let forks = wake_forks(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert_eq!(forks[0].model.as_deref(), Some("haiku"));
}

// ---- background work holds the idle clock ----

#[test]
fn background_work_holds_the_idle_clock_until_it_finishes() {
    let mut h = Harness::new("1s", "0").wake_grace_secs(0);
    h.write_fork(
        "goal.md",
        "---\nfork: true\nrun_on:\n  - idle: 0s\n---\nGOAL",
    );
    h.start_daemon();
    h.write_transcript(100);
    assert_ack(h.send_event(h.event_t(EventKind::SessionStart, "s1")));

    // The turn ended with a `run_in_background` command still running: Claude
    // Code calls that a Stop, but the session is waiting, not idle.
    h.append_background_launch("toolu_bg", "bg1");
    let rx = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    assert!(
        rx.recv_timeout(Duration::from_millis(2500)).is_err(),
        "an idle:0s fork fired while background work was still running"
    );

    // The command finishes and its notification lands; the next Stop is the
    // first genuinely idle one, and the fork fires there.
    h.append_completion_notification("toolu_bg", "completed");
    assert_ack(h.send_event(h.prompt_submit("s1", false)));
    let rx = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: goal"), "{payload}");
}

#[test]
fn background_hold_expires_so_unfinished_work_cannot_silence_forks() {
    let mut h = Harness::new("1s", "0").wake_grace_secs(0);
    h.append_config("background_hold_timeout = \"1s\"");
    h.write_fork(
        "journal.md",
        "---\nfork: true\nrun_on:\n  - idle: 0s\n---\nJOURNAL",
    );
    h.start_daemon();
    h.write_transcript(100);
    assert_ack(h.send_event(h.event_t(EventKind::SessionStart, "s1")));

    // A server left running: its completion notification never comes.
    h.append_background_launch("toolu_server", "srv1");
    let rx = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    assert!(rx.recv_timeout(Duration::from_millis(1200)).is_err());

    // Past the hold timeout the task stops counting and idle forks resume.
    std::thread::sleep(Duration::from_millis(1200));
    assert_ack(h.send_event(h.prompt_submit("s1", false)));
    let rx = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: journal"), "{payload}");
}

#[test]
fn background_hold_can_be_switched_off() {
    let mut h = Harness::new("1s", "0").wake_grace_secs(0);
    h.append_config("background_hold = false");
    h.write_fork(
        "journal.md",
        "---\nfork: true\nrun_on:\n  - idle: 0s\n---\nJOURNAL",
    );
    h.start_daemon();
    h.write_transcript(100);
    assert_ack(h.send_event(h.event_t(EventKind::SessionStart, "s1")));

    h.append_background_launch("toolu_bg", "bg1");
    let rx = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: journal"), "{payload}");
}

#[test]
fn own_fork_spawns_never_count_as_background_work() {
    // autofork's own fork subagent is background work in Claude Code's eyes,
    // but it must not make the session look busy to autofork's own scheduler
    // (the gate/after/overlap machinery is what orders fork runs).
    let mut h = Harness::new("1s", "0").wake_grace_secs(0);
    h.write_fork(
        "second.md",
        "---\nfork: true\nrun_on:\n  - idle: 0s\n---\nSECOND",
    );
    h.start_daemon();
    h.write_transcript(100);
    assert_ack(h.send_event(h.event_t(EventKind::SessionStart, "s1")));

    // A fork spawn, still running (no completion notification).
    h.append_fork_spawn("toolu_f1", "first");
    h.append_transcript_line(
        &serde_json::json!({
            "type": "user",
            "message": { "content": [
                { "type": "tool_result", "tool_use_id": "toolu_f1", "content": [
                    { "type": "text", "text": "Async agent launched successfully.\nagentId: a1" },
                ] },
            ] }
        })
        .to_string(),
    );
    let rx = h.park_stop_wait(h.event_t(EventKind::Stop, "s1"));
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: second"), "{payload}");
}
