mod daemon;
mod flush;
mod hooks;
mod liveness;
mod planner;
mod server;
mod sweep;
mod transcript;

use autofork_core::config::Paths;
use autofork_core::store::Store;
use daemon::Daemon;
use std::fs::OpenOptions;
use std::os::fd::AsRawFd;

/// Hold an exclusive flock on `path` for the process lifetime. Returns the
/// open file (dropping it releases the lock) or None if another process
/// holds it.
fn try_lock(path: &std::path::Path) -> Option<std::fs::File> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ok()?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (rc == 0).then_some(file)
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let Some(paths) = Paths::from_env() else {
        eprintln!("autofork-daemon: cannot determine home directory");
        std::process::exit(1);
    };

    // Single instance: the daemon holds this flock for its whole life; a
    // client that can acquire it knows the daemon is dead.
    let Some(_lock) = try_lock(&paths.daemon_lock()) else {
        tracing::info!("another daemon holds the lock, exiting");
        std::process::exit(0);
    };

    let store = match Store::open(&paths.db()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "cannot open state db");
            std::process::exit(1);
        }
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async_main(paths, store));
}

async fn async_main(paths: Paths, store: Store) {
    let socket = paths.socket();
    // We hold the daemon lock, so any existing socket file is stale.
    let _ = std::fs::remove_file(&socket);
    if let Some(parent) = socket.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let listener = match tokio::net::UnixListener::bind(&socket) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, socket = %socket.display(), "cannot bind socket");
            std::process::exit(1);
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
    }
    tracing::info!(
        version = Daemon::version(),
        socket = %socket.display(),
        "autofork daemon up"
    );

    let daemon = Daemon::new(paths, store);

    // Close sessions that timed out (crashed without a SessionEnd).
    let sweeper = daemon.clone();
    tokio::spawn(async move { sweep::session_reaper(sweeper).await });

    // Close sessions whose client process is gone (the OS-level answer, for
    // every exit a SessionEnd hook or a parked poll didn't cover).
    let liveness = daemon.clone();
    tokio::spawn(async move { liveness::harness_reaper(liveness).await });

    let reaper = daemon.clone();
    tokio::spawn(async move { reaper.quiet_reaper().await });

    let serve_daemon = daemon.clone();
    tokio::select! {
        _ = server::serve(serve_daemon, listener) => {}
        _ = daemon.shutdown.notified() => {}
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("interrupted");
        }
    }

    let _ = std::fs::remove_file(&socket);
    tracing::info!("autofork daemon down");
}
