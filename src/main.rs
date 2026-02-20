//! Runs the `netsim` CLI entrypoint.

mod caps;
mod sim;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use netsim::check_caps;
use netsim::serve::start_ui_server;

#[derive(Parser)]
#[command(name = "netsim", about = "Run a netsim simulation")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run one or more sims locally.
    Run {
        /// One or more sim TOML files or directories containing `*.toml`.
        #[arg(required = true)]
        sims: Vec<PathBuf>,

        /// Work directory for logs, binaries, and results.
        #[arg(long, default_value = ".netsim-work")]
        work_dir: PathBuf,

        /// Binary override in `<name>:<mode>:<value>` form.
        #[arg(long = "binary")]
        binary_overrides: Vec<String>,

        /// Start embedded UI server and open browser.
        #[arg(long, default_value_t = false)]
        open: bool,

        /// Bind address for embedded UI server.
        #[arg(long, default_value = "127.0.0.1:0")]
        bind: String,
    },
    /// Serve embedded UI + work directory over HTTP.
    Serve {
        /// Work directory containing run outputs.
        #[arg(long, default_value = ".netsim-work")]
        work_dir: PathBuf,
        /// Bind address for HTTP server.
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: String,
        /// Open browser after server start.
        #[arg(long, default_value_t = false)]
        open: bool,
    },
    /// Apply capabilities to this binary and required system tools.
    SetupCaps,
    /// Clean leaked labs by prefix.
    Cleanup {
        /// Resource name prefix to clean (repeatable).
        ///
        /// Defaults to `lab-p` and `br-p` when omitted.
        #[arg(long = "prefix")]
        prefixes: Vec<String>,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    netsim::Lab::init_tracing();
    let cli = Cli::parse();
    match cli.command {
        Command::Run {
            sims,
            work_dir,
            binary_overrides,
            open,
            bind,
        } => {
            check_caps()?;
            install_signal_cleanup_handler(vec![])?;
            let _server = if open {
                let srv = start_ui_server(work_dir.clone(), &bind)?;
                println!("netsim UI: {}", srv.url());
                srv.open_browser()?;
                Some(srv)
            } else {
                None
            };
            let res = sim::run_sims(sims, work_dir, binary_overrides).await;
            if open && res.is_ok() {
                println!("run finished; UI server still running (Ctrl-C to exit)");
                loop {
                    std::thread::sleep(Duration::from_secs(60));
                }
            }
            res
        }
        Command::Serve {
            work_dir,
            bind,
            open,
        } => {
            let _server = start_ui_server(work_dir, &bind)?;
            println!("netsim UI: {}", _server.url());
            if open {
                _server.open_browser()?;
            }
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
        Command::SetupCaps => caps::setup_caps_for_self_and_tools(),
        Command::Cleanup { prefixes } => cleanup_command(prefixes),
    }
}

fn default_cleanup_prefixes() -> Vec<String> {
    vec!["lab-p".to_string(), "br-p".to_string()]
}

fn cleanup_command(prefixes: Vec<String>) -> Result<()> {
    check_caps().context(
        "cleanup requires CAP_NET_ADMIN, CAP_SYS_ADMIN, and CAP_NET_RAW; run `netsim setup-caps` first",
    )?;
    let use_prefixes = if prefixes.is_empty() {
        default_cleanup_prefixes()
    } else {
        prefixes
    };
    perform_cleanup(&use_prefixes)
}

fn perform_cleanup(prefixes: &[String]) -> Result<()> {
    if prefixes.is_empty() {
        tracing::debug!("netsim cleanup: starting (prefixes: registered)");
    } else {
        tracing::debug!(
            "netsim cleanup: starting (prefixes: {})",
            prefixes.join(", ")
        );
    }
    let resources = netsim::core::resources();
    if !prefixes.is_empty() {
        for prefix in prefixes {
            resources.cleanup_everything_with_prefix(prefix);
        }
    } else {
        resources.cleanup_all();
    }
    if !prefixes.is_empty() {
        resources.cleanup_everything();
    }
    tracing::debug!("netsim cleanup: done");
    Ok(())
}

fn install_signal_cleanup_handler(prefixes: Vec<String>) -> Result<()> {
    ctrlc::set_handler(move || {
        eprintln!("netsim: received interrupt, running cleanup...");
        let _ = perform_cleanup(&prefixes);
        // SAFETY: immediate process termination after best-effort cleanup in signal path.
        unsafe { nix::libc::_exit(130) };
    })
    .context("install Ctrl-C cleanup handler")
}
