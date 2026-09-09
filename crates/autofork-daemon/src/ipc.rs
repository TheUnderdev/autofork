//! The daemon's listening endpoint: a Unix domain socket, or a named pipe on
//! Windows. Both speak the same JSONL protocol over the same
//! `AsyncRead + AsyncWrite` surface; only how a connection is *accepted*
//! differs, and that difference lives here.

use autofork_core::config::Paths;
use tokio::io::{AsyncRead, AsyncWrite};

/// One accepted connection, whatever the transport.
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Stream for T {}

#[cfg(unix)]
pub use unix::Listener;
#[cfg(windows)]
pub use windows::Listener;

#[cfg(unix)]
mod unix {
    use super::Paths;
    use std::path::PathBuf;
    use tokio::net::{UnixListener, UnixStream};

    pub struct Listener {
        inner: UnixListener,
        path: PathBuf,
    }

    impl Listener {
        /// Bind the socket. The caller holds the daemon lock, so any existing
        /// socket file is stale and is replaced.
        pub fn bind(paths: &Paths) -> std::io::Result<Self> {
            let path = paths.socket();
            let _ = std::fs::remove_file(&path);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let inner = UnixListener::bind(&path)?;
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
            Ok(Self { inner, path })
        }

        pub async fn accept(&mut self) -> std::io::Result<UnixStream> {
            let (stream, _) = self.inner.accept().await?;
            Ok(stream)
        }

        /// The endpoint, for logs.
        pub fn describe(&self) -> String {
            self.path.display().to_string()
        }

        /// Remove the socket file so a client never connects to a corpse.
        pub fn cleanup(paths: &Paths) {
            let _ = std::fs::remove_file(paths.socket());
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::Paths;
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    /// A named pipe server. Windows has no listen/accept: each client
    /// connects to its *own* pipe instance, so the listener always holds one
    /// unconnected instance ready and creates the next the moment a client
    /// takes it — the pattern tokio documents for a pipe server.
    pub struct Listener {
        name: String,
        server: NamedPipeServer,
    }

    impl Listener {
        /// Create the first instance. `first_pipe_instance` makes this fail
        /// if another process already serves the name — the pipe-world twin
        /// of the daemon lock, so two daemons can never share an endpoint.
        pub fn bind(paths: &Paths) -> std::io::Result<Self> {
            let name = paths.pipe_name();
            let server = ServerOptions::new()
                .first_pipe_instance(true)
                .create(&name)?;
            Ok(Self { name, server })
        }

        pub async fn accept(&mut self) -> std::io::Result<NamedPipeServer> {
            self.server.connect().await?;
            let next = ServerOptions::new().create(&self.name)?;
            Ok(std::mem::replace(&mut self.server, next))
        }

        pub fn describe(&self) -> String {
            self.name.clone()
        }

        /// Nothing to remove: a pipe name vanishes with its last handle.
        pub fn cleanup(_paths: &Paths) {}
    }
}
