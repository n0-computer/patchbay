//! Runs the `netsim` CLI entrypoint.

mod caps;
mod sim;
mod vm;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use netsim::check_caps;

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
    },
    /// Run one or more sims in the local QEMU VM.
    RunVm {
        /// One or more sim TOML files or directories containing `*.toml`.
        #[arg(required = true)]
        sims: Vec<PathBuf>,

        /// Work directory for logs, binaries, and results.
        #[arg(long, default_value = ".netsim-work")]
        work_dir: PathBuf,

        /// Binary override in `<name>:<mode>:<value>` form.
        #[arg(long = "binary")]
        binary_overrides: Vec<String>,

        /// Recreate VM if running with different mount paths.
        #[arg(long)]
        recreate: bool,
    },
    /// Apply capabilities to this binary and required system tools.
    SetupCaps,
    /// Clean leaked labs by prefix and stop the local VM if running.
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
        } => {
            check_caps()?;
            install_signal_cleanup_handler(vec![], false)?;
            sim::run_sims(sims, work_dir, binary_overrides).await
        }
        Command::RunVm {
            sims,
            work_dir,
            binary_overrides,
            recreate,
        } => {
            install_signal_cleanup_handler(vec![], true)?;
            vm::run_sims_in_vm(vm::RunVmArgs {
                sim_inputs: sims,
                work_dir,
                binary_overrides,
                recreate,
            })
            .await
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
    perform_cleanup(&use_prefixes, true)
}

fn perform_cleanup(prefixes: &[String], stop_vm: bool) -> Result<()> {
    if prefixes.is_empty() {
        println!("netsim cleanup: starting (prefixes: registered)");
    } else {
        println!(
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
    if stop_vm {
        println!("netsim cleanup: stopping local VM (if running)");
        vm::stop_vm_if_running()?;
    }
    println!("netsim cleanup: done");
    Ok(())
}

fn install_signal_cleanup_handler(prefixes: Vec<String>, stop_vm: bool) -> Result<()> {
    ctrlc::set_handler(move || {
        eprintln!("netsim: received interrupt, running cleanup...");
        let _ = perform_cleanup(&prefixes, stop_vm);
        // SAFETY: immediate process termination after best-effort cleanup in signal path.
        unsafe { nix::libc::_exit(130) };
    })
    .context("install Ctrl-C cleanup handler")
}
