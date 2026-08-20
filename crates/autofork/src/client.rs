//! Daemon client: connect over the unix socket, auto-spawning the daemon
//! when needed (flock-serialized against racing siblings), with per-call
//! timeouts so hook paths never blow their budgets.

use autofork_core::config::Paths;
use autofork_core::protocol::{encode, Event, Request, RequestBody, Response, ResponseBody};
use autofork_core::PROTO_VERSION;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

pub struct Client {
    stream: UnixStream,
    next_id: u64,
}

#[derive(Debug)]
pub enum ClientError {
    NotRunning,
    Io(std::io::Error),
    Protocol(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::NotRunning => write!(f, "daemon not running"),
            ClientError::Io(e) => write!(f, "io: {e}"),
            ClientError::Protocol(m) => write!(f, "protocol: {m}"),
        }
    }
}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        ClientError::Io(e)
    }
}

impl Client {
    /// Connect without spawning.
    pub fn connect(paths: &Paths, timeout: Duration) -> Result<Self, ClientError> {
        let socket = paths.socket();
        let stream = UnixStream::connect(&socket).map_err(|_| ClientError::NotRunning)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        Ok(Self { stream, next_id: 1 })
    }

    /// Connect, auto-spawning the daemon when it's down. `budget` bounds the
    /// whole operation (connect + spawn + reconnect polling).
    pub fn connect_or_spawn(paths: &Paths, budget: Duration) -> Result<Self, ClientError> {
        let deadline = Instant::now() + budget;
        if let Ok(c) = Self::connect(paths, budget) {
            return Ok(c);
        }
        spawn_daemon_locked(paths, deadline)?;
        loop {
            match Self::connect(paths, Duration::from_secs(30)) {
                Ok(c) => return Ok(c),
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// The asyncRewake Stop hook's long poll: the daemon may hold the response
    /// for a long time (until forks are due or the wait is cancelled), so the
    /// read timeout is widened to the hook's own 4h budget. A closed socket
    /// (daemon retiring mid-poll) surfaces as an error the caller treats as a
    /// silent exit-0.
    pub fn stop_wait(&mut self, ev: Event) -> Result<ResponseBody, ClientError> {
        self.stream
            .set_read_timeout(Some(Duration::from_secs(4 * 3600)))?;
        self.request(RequestBody::StopWait(ev))
    }

    pub fn request(&mut self, body: RequestBody) -> Result<ResponseBody, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let req = Request {
            proto: PROTO_VERSION,
            id,
            body,
        };
        let line = encode(&req).map_err(|e| ClientError::Protocol(e.to_string()))?;
        self.stream.write_all(line.as_bytes())?;
        let mut reader = BufReader::new(self.stream.try_clone()?);
        let mut resp_line = String::new();
        reader.read_line(&mut resp_line)?;
        if resp_line.is_empty() {
            return Err(ClientError::Protocol("connection closed".into()));
        }
        let resp: Response = serde_json::from_str(resp_line.trim())
            .map_err(|e| ClientError::Protocol(e.to_string()))?;
        Ok(resp.body)
    }

    /// Version handshake: when this CLI is newer than the daemon (or protos
    /// mismatch), retire the daemon (drain) and spawn the current binary.
    /// Call only on paths with time slack (never UserPromptSubmit).
    pub fn ensure_current_version(mut self, paths: &Paths) -> Result<Client, ClientError> {
        let mine = env!("CARGO_PKG_VERSION");
        let outdated = match self.request(RequestBody::Hello {
            version: mine.to_string(),
        }) {
            Ok(ResponseBody::HelloInfo { version }) => semver_lt(&version, mine),
            Ok(ResponseBody::Error { .. }) | Err(_) => true,
            Ok(_) => false,
        };
        if !outdated {
            return Ok(self);
        }
        tracing::info!("retiring outdated daemon");
        let _ = self.request(RequestBody::Shutdown { drain: true });
        drop(self);
        // Wait for the old daemon to release its lock, then respawn.
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            if try_flock(&paths.daemon_lock()).is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Client::connect_or_spawn(paths, Duration::from_secs(10))
    }
}

/// `a < b` for `x.y.z` version strings (missing/invalid parts count as 0).
fn semver_lt(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> [u64; 3] {
        let mut out = [0u64; 3];
        for (i, part) in s.trim().split('.').take(3).enumerate() {
            out[i] = part
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0);
        }
        out
    };
    parse(a) < parse(b)
}

/// Acquire (and immediately hold) a non-blocking exclusive flock; None when
/// another process holds it. Dropping the file releases it.
pub(crate) fn try_flock(path: &Path) -> Option<std::fs::File> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ok()?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (rc == 0).then_some(file)
}

/// Blocking flock with a deadline (poll-based, since flock has no timeout).
fn flock_until(path: &Path, deadline: Instant) -> Option<std::fs::File> {
    loop {
        if let Some(f) = try_flock(path) {
            return Some(f);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The daemon binary: `autofork-daemon` next to the current executable.
fn daemon_binary() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join("autofork-daemon");
    candidate.is_file().then_some(candidate)
}

/// Spawn the daemon, serialized against racing CLIs via the spawn lock.
/// Fire-and-forget variant: does not wait for the socket.
pub fn spawn_daemon_detached(paths: &Paths) {
    let deadline = Instant::now() + Duration::from_millis(500);
    let _ = spawn_daemon_locked(paths, deadline);
}

fn spawn_daemon_locked(paths: &Paths, deadline: Instant) -> Result<(), ClientError> {
    let Some(_spawn_lock) = flock_until(&paths.spawn_lock(), deadline) else {
        // Someone else is spawning; treat as success and let the caller's
        // reconnect loop find the socket.
        return Ok(());
    };
    // Re-check: the race winner may have brought the daemon up while we
    // waited on the lock.
    if UnixStream::connect(paths.socket()).is_ok() {
        return Ok(());
    }
    // Staleness: the daemon holds its lock for life; acquirable = dead.
    {
        let Some(_daemon_lock) = try_flock(&paths.daemon_lock()) else {
            // A daemon lives but its socket didn't answer — maybe still
            // booting. Nothing to do but let the caller retry.
            return Ok(());
        };
        let _ = std::fs::remove_file(paths.socket());
        // Lock released here so the spawned daemon can take it.
    }

    let Some(bin) = daemon_binary() else {
        return Err(ClientError::Protocol(
            "autofork-daemon binary not found next to the CLI".into(),
        ));
    };
    let log_path = paths.daemon_log();
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log2 = log.try_clone()?;

    let mut cmd = std::process::Command::new(bin);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log2));
    // Detach into its own session so it outlives the hook process.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    cmd.spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::semver_lt;

    #[test]
    fn semver_ordering() {
        assert!(semver_lt("0.1.0", "0.2.0"));
        assert!(semver_lt("0.1.9", "0.1.10"));
        assert!(!semver_lt("0.2.0", "0.1.9"));
        assert!(!semver_lt("1.0.0", "1.0.0"));
        assert!(semver_lt("garbage", "0.0.1"));
    }
}
