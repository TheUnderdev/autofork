mod daemon;
mod flush;
mod hooks;
mod ipc;
mod liveness;
mod planner;
mod server;
mod sweep;
mod transcript;
mod watch;

use autofork_core::config::Paths;
use autofork_core::store::Store;
use daemon::Daemon;

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

    // Single instance: the daemon holds this lock for its whole life; a
    // client that can acquire it knows the daemon is dead.
    let Some(_lock) = autofork_core::sys::try_lock_file(&paths.daemon_lock()) else {
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
    // We hold the daemon lock, so any existing endpoint is stale.
    let listener = match ipc::Listener::bind(&paths) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, socket = %paths.socket().display(), "cannot bind endpoint");
            std::process::exit(1);
        }
    };
    tracing::info!(
        version = Daemon::version(),
        socket = %listener.describe(),
        "autofork daemon up"
    );

    let daemon = Daemon::new(paths, store);
    let paths = daemon.paths.clone();

    // Close sessions that timed out (crashed without a SessionEnd).
    let sweeper = daemon.clone();
    tokio::spawn(async move { sweep::session_reaper(sweeper).await });

    // Close sessions whose client process is gone (the OS-level answer, for
    // every exit a SessionEnd hook or a parked poll didn't cover).
    let liveness = daemon.clone();
    tokio::spawn(async move { liveness::harness_reaper(liveness).await });

    let reaper = daemon.clone();
    tokio::spawn(async move { reaper.quiet_reaper().await });

    // Sweep the paths `changed:` triggers watch.
    let watcher = daemon.clone();
    tokio::spawn(async move { watch::watch_loop(watcher).await });

    let serve_daemon = daemon.clone();
    tokio::select! {
        _ = server::serve(serve_daemon, listener) => {}
        _ = daemon.shutdown.notified() => {}
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("interrupted");
        }
    }

    ipc::Listener::cleanup(&paths);
    tracing::info!("autofork daemon down");
}
