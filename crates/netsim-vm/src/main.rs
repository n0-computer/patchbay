mod util;
mod vm;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use netsim::serve::start_ui_server;

#[derive(Parser)]
#[command(name = "netsim-vm", about = "Standalone VM runner for netsim")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Boot or reuse VM and ensure mounts.
    Up {
        #[arg(long)]
        recreate: bool,
    },
    /// Stop VM and helper processes.
    Down,
    /// Show VM running status.
    Status,
    /// Best-effort cleanup of VM helper artifacts/processes.
    Cleanup,
    /// Execute command over guest SSH.
    Ssh {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },
    /// Run one or more sims in VM using guest netsim binary.
    Run {
        #[arg(required = true)]
        sims: Vec<PathBuf>,
        #[arg(long, default_value = ".netsim-work")]
        work_dir: PathBuf,
        #[arg(long = "binary")]
        binary_overrides: Vec<String>,
        #[arg(long)]
        recreate: bool,
        #[arg(long, default_value = "latest")]
        netsim_version: String,
        #[arg(long, default_value_t = false)]
        open: bool,
        #[arg(long, default_value = "127.0.0.1:0")]
        bind: String,
    },
    /// Serve embedded UI + work directory over HTTP.
    Serve {
        #[arg(long, default_value = ".netsim-work")]
        work_dir: PathBuf,
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: String,
        #[arg(long, default_value_t = false)]
        open: bool,
    },
    /// Build and run tests in VM (replaces legacy test-vm flow).
    Test {
        #[arg(long, default_value = "x86_64-unknown-linux-musl")]
        target: String,
        #[arg(long = "package")]
        packages: Vec<String>,
        #[arg(long = "test")]
        tests: Vec<String>,
        #[arg(long)]
        recreate: bool,
        #[arg(last = true)]
        cargo_args: Vec<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Up { recreate } => vm::up_cmd(recreate),
        Command::Down => vm::down_cmd(),
        Command::Status => vm::status_cmd(),
        Command::Cleanup => vm::cleanup_cmd(),
        Command::Ssh { cmd } => vm::ssh_cmd_cli(cmd),
        Command::Run {
            sims,
            work_dir,
            binary_overrides,
            recreate,
            netsim_version,
            open,
            bind,
        } => {
            let _server = if open {
                let srv = start_ui_server(work_dir.clone(), &bind)?;
                println!("netsim UI: {}", srv.url());
                srv.open_browser()?;
                Some(srv)
            } else {
                None
            };
            let res = vm::run_sims_in_vm(vm::RunVmArgs {
                sim_inputs: sims,
                work_dir,
                binary_overrides,
                recreate,
                netsim_version,
            });
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
        Command::Test {
            target,
            packages,
            tests,
            recreate,
            cargo_args,
        } => vm::run_tests_in_vm(vm::TestVmArgs {
            target,
            packages,
            tests,
            recreate,
            cargo_args,
        }),
    }
}
