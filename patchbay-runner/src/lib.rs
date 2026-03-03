//! patchbay-runner — sim runner library + CLI.
//!
//! This crate requires Linux. On other platforms, it provides an empty stub.

#[cfg(target_os = "linux")]
pub use patchbay as core;
#[cfg(target_os = "linux")]
pub use patchbay::{
    check_caps,
    config::{LabConfig, RegionConfig, RouterConfig},
    init_userns, DeviceBuilder, Lab, LinkCondition, Nat, NodeId, ObservedAddr,
};
#[cfg(target_os = "linux")]
pub use patchbay_utils::assets::BinaryOverride;

#[cfg(target_os = "linux")]
mod init;
#[cfg(target_os = "linux")]
pub(crate) mod sim;

#[cfg(target_os = "linux")]
use std::path::PathBuf;

#[cfg(target_os = "linux")]
use anyhow::Result;

/// Run one or more simulations from sim-file paths.
///
/// This is a thin adapter that delegates to the internal `sim::runner`.
#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
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
