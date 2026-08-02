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

    fn write_fork(&self, rel: &str, content: &str) {
        let path = self.project.join(".autofork/forks").join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
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
            .env("RUST_LOG", "debug")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(ms) = self.poll_grace_ms {
            cmd.env("AUTOFORK_POLL_LOSS_GRACE_MS", ms.to_string());
        }
        if let Some(secs) = self.wake_grace_secs {
            cmd.env("AUTOFORK_WAKE_GRACE_SECS", secs.to_string());
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
            model: None,
            enable_tags: None,
            disable_tags: None,
            waking: None,
            notif_tool_use_id: None,
            notif_task_id: None,
            notif_status: None,
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

    /// Append a background-task completion notification to the transcript, as
    /// the relay turn's user entry would contain.
    fn append_completion_notification(&self, tool_use_id: &str, status: &str) {
        let content = format!(
            "<task-notification>\n<task-id>t-{tool_use_id}</task-id>\n\
             <tool-use-id>{tool_use_id}</tool-use-id>\n<status>{status}</status>\n\
             <summary>Agent \"x\" finished</summary>\n<result>report</result>"
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
