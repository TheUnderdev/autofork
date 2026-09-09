//! Daemon client: connect over the daemon endpoint (a Unix socket, or a named
//! pipe on Windows), auto-spawning the daemon when needed (lock-serialized
//! against racing siblings), with per-call timeouts so hook paths never blow
//! their budgets.

use autofork_core::config::Paths;
use autofork_core::protocol::{encode, Event, Request, RequestBody, Response, ResponseBody};
use autofork_core::sys;
use autofork_core::PROTO_VERSION;
use std::path::Path;
use std::time::{Duration, Instant};

pub struct Client {
    conn: Conn,
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
        let conn = Conn::connect(paths, timeout).map_err(|_| ClientError::NotRunning)?;
        Ok(Self { conn, next_id: 1 })
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
    /// read timeout is widened to the hook's own 4h budget. A closed
    /// connection (daemon retiring mid-poll) surfaces as an error the caller
    /// treats as a silent exit-0.
    pub fn stop_wait(&mut self, ev: Event) -> Result<ResponseBody, ClientError> {
        self.conn.set_read_timeout(Duration::from_secs(4 * 3600))?;
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
        self.conn.write_all(line.as_bytes())?;
        let resp_line = self.conn.read_line()?;
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

/// Acquire (and immediately hold) a non-blocking exclusive lock; None when
/// another process holds it. Dropping the file releases it.
pub(crate) fn try_flock(path: &Path) -> Option<std::fs::File> {
    sys::try_lock_file(path)
}

/// Blocking lock with a deadline (poll-based, since the lock has no timeout).
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
    let candidate = exe
        .parent()?
        .join(format!("autofork-daemon{}", sys::EXE_SUFFIX));
    candidate.is_file().then_some(candidate)
}

/// Spawn the daemon, serialized against racing CLIs via the spawn lock.
/// Fire-and-forget variant: does not wait for the endpoint.
pub fn spawn_daemon_detached(paths: &Paths) {
    let deadline = Instant::now() + Duration::from_millis(500);
    let _ = spawn_daemon_locked(paths, deadline);
}

fn spawn_daemon_locked(paths: &Paths, deadline: Instant) -> Result<(), ClientError> {
    let Some(_spawn_lock) = flock_until(&paths.spawn_lock(), deadline) else {
        // Someone else is spawning; treat as success and let the caller's
        // reconnect loop find the endpoint.
        return Ok(());
    };
    // Re-check: the race winner may have brought the daemon up while we
    // waited on the lock.
    if Conn::probe(paths) {
        return Ok(());
    }
    // Staleness: the daemon holds its lock for life; acquirable = dead.
    {
        let Some(_daemon_lock) = try_flock(&paths.daemon_lock()) else {
            // A daemon lives but its endpoint didn't answer — maybe still
            // booting. Nothing to do but let the caller retry.
            return Ok(());
        };
        #[cfg(unix)]
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
    // Detach so it outlives the hook process.
    sys::detach(&mut cmd);
    cmd.spawn()?;
    Ok(())
}

/// The executable path of this process's PARENT — the harness that spawned
/// the hook (Claude Code, codex, opencode). Fork children must run the SAME
/// binary the user's session runs: multi-install machines (a standalone
/// opencode under an aliased wrapper, several claude/codex checkouts) make
/// PATH lookup resolve a different program than the parent. `None` when the
/// platform lookup fails — callers fall back to the PATH name.
pub(crate) fn parent_exe() -> Option<std::path::PathBuf> {
    sys::exe_path(sys::parent_pid())
}

// ---------------------------------------------------------------------------
// The connection itself
// ---------------------------------------------------------------------------

#[cfg(unix)]
use unix::Conn;
#[cfg(windows)]
use windows::Conn;

#[cfg(unix)]
mod unix {
    use super::Paths;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    /// A Unix-socket connection with kernel read/write timeouts.
    pub struct Conn {
        stream: UnixStream,
        // Reads go through a buffered clone of the same socket; SO_RCVTIMEO
        // is a property of the socket, so the timeout set on `stream` holds
        // for the clone too.
        reader: BufReader<UnixStream>,
    }

    impl Conn {
        pub fn connect(paths: &Paths, timeout: Duration) -> std::io::Result<Self> {
            let stream = UnixStream::connect(paths.socket())?;
            stream.set_read_timeout(Some(timeout))?;
            stream.set_write_timeout(Some(timeout))?;
            let reader = BufReader::new(stream.try_clone()?);
            Ok(Self { stream, reader })
        }

        /// Whether anything answers at the endpoint right now.
        pub fn probe(paths: &Paths) -> bool {
            UnixStream::connect(paths.socket()).is_ok()
        }

        pub fn set_read_timeout(&mut self, d: Duration) -> std::io::Result<()> {
            self.stream.set_read_timeout(Some(d))
        }

        pub fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
            self.stream.write_all(bytes)
        }

        /// One response line (empty on EOF).
        pub fn read_line(&mut self) -> std::io::Result<String> {
            let mut line = String::new();
            self.reader.read_line(&mut line)?;
            Ok(line)
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::Paths;
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};

    /// `ERROR_PIPE_BUSY`: every server instance is taken this instant. The
    /// daemon creates the next instance the moment one is claimed, so the
    /// window is short; a client retries rather than failing.
    const PIPE_BUSY: i32 = 231;

    /// A named-pipe connection. A blocking `std::fs::File` on a pipe has no
    /// read timeout, and the hook paths live on their timeouts — so the
    /// client drives tokio's pipe client on a private single-thread runtime
    /// and wraps every operation in `tokio::time::timeout`.
    pub struct Conn {
        rt: tokio::runtime::Runtime,
        io: BufReader<NamedPipeClient>,
        timeout: Duration,
    }

    impl Conn {
        pub fn connect(paths: &Paths, timeout: Duration) -> std::io::Result<Self> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let name = paths.pipe_name();
            let deadline = Instant::now() + timeout;
            let pipe = rt.block_on(async {
                loop {
                    match ClientOptions::new().open(&name) {
                        Ok(c) => break Ok(c),
                        Err(e)
                            if e.raw_os_error() == Some(PIPE_BUSY) && Instant::now() < deadline =>
                        {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                        Err(e) => break Err(e),
                    }
                }
            })?;
            Ok(Self {
                rt,
                io: BufReader::new(pipe),
                timeout,
            })
        }

        pub fn probe(paths: &Paths) -> bool {
            Self::connect(paths, Duration::from_millis(500)).is_ok()
        }

        pub fn set_read_timeout(&mut self, d: Duration) -> std::io::Result<()> {
            self.timeout = d;
            Ok(())
        }

        pub fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
            let Self { rt, io, timeout } = self;
            rt.block_on(async {
                tokio::time::timeout(*timeout, io.write_all(bytes))
                    .await
                    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "write"))?
            })
        }

        pub fn read_line(&mut self) -> std::io::Result<String> {
            let Self { rt, io, timeout } = self;
            let mut line = String::new();
            rt.block_on(async {
                tokio::time::timeout(*timeout, io.read_line(&mut line))
                    .await
                    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "read"))?
            })?;
            Ok(line)
        }
    }
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
