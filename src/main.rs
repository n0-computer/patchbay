//! Runs the `netsim` CLI entrypoint.

mod sim;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use netsim::check_caps;

#[derive(Parser)]
#[command(name = "netsim", about = "Run a netsim simulation")]
struct Cli {
    /// Path to the sim TOML file.
    sim: PathBuf,

    /// Work directory for logs, binaries, and results.
    #[arg(long, default_value = ".netsim-work")]
    work_dir: PathBuf,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    netsim::Lab::init_tracing();
    check_caps()?;

    let cli = Cli::parse();
    sim::run_sim(cli.sim, cli.work_dir).await
}
