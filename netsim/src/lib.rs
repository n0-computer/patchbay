//! netsim — sim runner library + CLI.

pub use netsim_core as core;
pub use netsim_core::{
    check_caps,
    config::{LabConfig, RegionConfig, RouterConfig},
    init_userns, DeviceBuilder, LinkCondition, Lab, Nat, NodeId, ObservedAddr,
};
pub use netsim_utils::assets::BinaryOverride;

mod init;
pub(crate) mod sim;

use std::path::PathBuf;

use anyhow::Result;

/// Run one or more simulations from sim-file paths.
///
/// This is a thin adapter that delegates to the internal `sim::runner`.
pub async fn run_sims(
    sim_inputs: Vec<PathBuf>,
    work_dir: PathBuf,
    binary_overrides: Vec<String>,
    verbose: bool,
    project_root: Option<PathBuf>,
    no_build: bool,
) -> Result<()> {
    sim::run_sims(
        sim_inputs,
        work_dir,
        binary_overrides,
        verbose,
        project_root,
        no_build,
    )
    .await
}

/// Build / fetch binaries declared in sim files without executing steps.
pub async fn prepare_sims(
    sim_inputs: Vec<PathBuf>,
    work_dir: PathBuf,
    binary_overrides: Vec<String>,
    project_root: Option<PathBuf>,
    no_build: bool,
) -> Result<()> {
    sim::prepare_sims(
        sim_inputs,
        work_dir,
        binary_overrides,
        project_root,
        no_build,
    )
    .await
}
