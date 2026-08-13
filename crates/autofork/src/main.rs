mod client;
mod commands;
mod hook;
mod opencode;

use autofork_core::config::Paths;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "autofork",
    version,
    about = "Forks for Claude Code: throwaway forked-context runs at lifecycle moments"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Claude Code hook entrypoint (reads the hook JSON on stdin).
    #[command(hide = true)]
    Hook {
        #[arg(value_enum)]
        event: hook::HookKind,
    },
    /// Daemon, session, and fork-run status.
    Status,
    /// List the forks visible from the current (or given) directory.
    Forks {
        /// Project directory (defaults to the current directory).
        #[arg(long)]
        project: Option<std::path::PathBuf>,
    },
    /// List the lifecycle hooks visible from the current (or given) directory.
    ///
    /// Lifecycle hooks are daemon-run shell commands (no model, no fork) fired
    /// at session moments — session_start, resume, activity, idle, session_end
    /// — for resource integrations like workspace leases.
    Hooks {
        /// Project directory (defaults to the current directory).
        #[arg(long)]
        project: Option<std::path::PathBuf>,
    },
    /// Print the spawn instruction for a fork (by name, or every fork carrying
    /// `--tag`) to paste into an interactive Claude Code session. v0.5 forks
    /// run as fork subagents, so autofork can no longer spawn them itself.
    Run {
        /// Fork name (omit when using --tag).
        name: Option<String>,
        /// Select every fork carrying this tag instead of one by name.
        #[arg(long, conflicts_with = "name")]
        tag: Option<String>,
    },
    /// Show the daemon log.
    Logs {
        /// Keep following the log.
        #[arg(short, long)]
        follow: bool,
    },
    /// Close the sessions `status` marks [stale?] now.
    ///
    /// Stale = open with no parked poll and long idle — a Claude process that
    /// died mid-turn. Harmless (a stale session can never fire a fork) but they
    /// clutter `status` until the session timeout (default 12h) reaps them.
    Prune,
    /// Check the installation and report problems.
    Doctor,
    /// opencode integration: install the plugin, or serve as its hook.
    Opencode {
        #[command(subcommand)]
        command: OpencodeCommand,
    },
    /// Ask the daemon to exit (it restarts on the next hook event).
    StopDaemon {
        /// Wait for in-flight fork runs to finish first.
        #[arg(long, default_value_t = true)]
        drain: bool,
    },
}

#[derive(Subcommand)]
enum OpencodeCommand {
    /// Install (or refresh) the autofork plugin into opencode's global
    /// plugin directory. Restart opencode afterwards.
    Install {
        /// Print the plugin source to stdout instead of installing it.
        #[arg(long)]
        print: bool,
    },
    /// Remove the installed plugin.
    Uninstall,
    /// The opencode plugin's hook entrypoint (reads JSON on stdin).
    #[command(hide = true)]
    Hook {
        #[arg(value_enum)]
        event: opencode::OcHookKind,
    },
}

fn main() {
    let cli = Cli::parse();
    let Some(paths) = Paths::from_env() else {
        eprintln!("autofork: cannot determine home directory");
        std::process::exit(1);
    };

    match cli.command {
        Command::Hook { event } => hook::run_hook(event),
        Command::Status => exit_on_err(commands::status(&paths)),
        Command::Forks { project } => exit_on_err(commands::list_forks(&paths, project)),
        Command::Hooks { project } => exit_on_err(commands::list_hooks(&paths, project)),
        Command::Run { name, tag } => exit_on_err(commands::run_fork(&paths, name, tag)),
        Command::Logs { follow } => exit_on_err(commands::logs(&paths, follow)),
        Command::Prune => exit_on_err(commands::prune(&paths)),
        Command::Doctor => exit_on_err(commands::doctor(&paths)),
        Command::Opencode { command } => match command {
            OpencodeCommand::Install { print } => exit_on_err(opencode::install(print)),
            OpencodeCommand::Uninstall => exit_on_err(opencode::uninstall()),
            OpencodeCommand::Hook { event } => opencode::run_hook(event),
        },
        Command::StopDaemon { drain } => exit_on_err(stop_daemon(&paths, drain)),
    }
}

fn stop_daemon(paths: &Paths, drain: bool) -> Result<(), String> {
    use autofork_core::protocol::RequestBody;
    match client::Client::connect(paths, std::time::Duration::from_secs(5)) {
        Ok(mut c) => {
            let _ = c
                .request(RequestBody::Shutdown { drain })
                .map_err(|e| e.to_string())?;
            println!("daemon asked to exit");
            Ok(())
        }
        Err(_) => {
            println!("daemon not running");
            Ok(())
        }
    }
}

fn exit_on_err(result: Result<(), String>) {
    if let Err(e) = result {
        eprintln!("autofork: {e}");
        std::process::exit(1);
    }
}
