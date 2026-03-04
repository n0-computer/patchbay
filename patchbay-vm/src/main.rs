mod util;
mod vm;

fn default_test_target() -> String {
    if std::env::consts::ARCH == "aarch64" {
        "aarch64-unknown-linux-musl".to_string()
    } else {
        "x86_64-unknown-linux-musl".to_string()
    }
}

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use patchbay_server::DEFAULT_UI_BIND;

#[derive(Parser)]
#[command(name = "patchbay-vm", about = "Standalone VM runner for patchbay")]
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
    /// Run one or more sims in VM using guest patchbay binary.
    Run {
        #[arg(required = true)]
        sims: Vec<PathBuf>,
        #[arg(long, default_value = ".patchbay-work")]
        work_dir: PathBuf,
        #[arg(long = "binary")]
        binary_overrides: Vec<String>,
        #[arg(short = 'v', long, default_value_t = false)]
        verbose: bool,
        #[arg(long)]
        recreate: bool,
        #[arg(long, default_value = "latest")]
        patchbay_version: String,
        #[arg(long, default_value_t = false)]
        open: bool,
        #[arg(long, default_value = DEFAULT_UI_BIND)]
        bind: String,
    },
    /// Serve embedded UI + work directory over HTTP.
    Serve {
        #[arg(long, default_value = ".patchbay-work")]
        work_dir: PathBuf,
        #[arg(long, default_value = DEFAULT_UI_BIND)]
        bind: String,
        #[arg(long, default_value_t = false)]
        open: bool,
    },
    /// Build and run tests in VM (replaces legacy test-vm flow).
    Test {
        #[arg(long, default_value_t = default_test_target())]
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

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    patchbay_utils::init_tracing();
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
            verbose,
            recreate,
            patchbay_version,
            open,
            bind,
        } => {
            if open {
                let url = format!("http://{bind}");
                println!("patchbay UI: {url}");
                let _ = open::that(&url);
                let work = work_dir.clone();
                tokio::spawn(async move {
                    if let Err(e) = patchbay_server::serve(work, &bind).await {
                        tracing::error!("server error: {e}");
                    }
                });
            }
            let res = vm::run_sims_in_vm(vm::RunVmArgs {
                sim_inputs: sims,
                work_dir,
                binary_overrides,
                verbose,
                recreate,
                patchbay_version,
            });
            if open && res.is_ok() {
                println!("run finished; server still running (Ctrl-C to exit)");
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                }
            }
            res
        }
        Command::Serve {
            work_dir,
            bind,
            open,
        } => {
            if open {
                let url = format!("http://{bind}");
                println!("patchbay UI: {url}");
                let _ = open::that(&url);
            }
            patchbay_server::serve(work_dir, &bind).await
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
