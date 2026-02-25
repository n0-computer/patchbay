//! netsim-core — Linux network-namespace lab for NAT/routing experiments.
//!
//! # Quick start (from TOML)
//! ```no_run
//! # use netsim_core::Lab;
//! # use std::process::Command;
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> anyhow::Result<()> {
//! let lab = Lab::load("lab.toml").await?;
//! let mut cmd = Command::new("ping");
//! cmd.args(["-c1", "8.8.8.8"]);
//! lab.run_on("home-eu1", cmd)?;
//! # Ok(())
//! # }
//! ```
//!
//! # Builder API
//! ```no_run
//! # use netsim_core::{Lab, NatMode};
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> anyhow::Result<()> {
//! let mut lab = Lab::new();
//! let isp  = lab.add_router("isp1").region("eu").nat(NatMode::Cgnat).build()?;
//! let home = lab.add_router("home1").upstream(isp).nat(NatMode::DestinationIndependent).build()?;
//! lab.add_device("dev1").iface("eth0", home, None).build()?;
//! lab.build().await?;
//! # Ok(())
//! # }
//! ```
//!
//! namespace transitions are executed inside dedicated worker
//! threads in the netns manager; callers can use any Tokio runtime flavor.

use anyhow::{anyhow, bail, Context, Result};

/// Defines TOML configuration structures used by [`Lab::load`].
pub mod config;
/// Exposes low-level topology and namespace construction primitives.
pub mod core;
mod lab;
mod netlink;
mod netns;
mod qdisc;
/// Probe and reflector helpers for integration tests.
pub mod test_utils;
mod userns;
/// Shared string sanitizers.
pub mod util;

pub use crate::core::NodeId;
pub use crate::userns::{init_userns, init_userns_for_ctor};
pub use lab::{DeviceBuilder, Impair, Lab, NatMode, ObservedAddr, RouterBuilder};

/// Verifies the process has enough privileges to manage namespaces, routes, and raw sockets.
pub fn check_caps() -> Result<()> {
    if nix::unistd::Uid::effective().is_root() {
        return Ok(());
    }
    let status = std::fs::read_to_string("/proc/self/status").context("read /proc/self/status")?;
    let cap_eff = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:\t"))
        .ok_or_else(|| anyhow!("missing CapEff in /proc/self/status"))?;
    let cap_eff = u64::from_str_radix(cap_eff.trim(), 16).context("parse CapEff")?;
    const CAP_NET_ADMIN: u64 = 12;
    const CAP_NET_RAW: u64 = 13;
    const CAP_SYS_ADMIN: u64 = 21;
    let need = [
        ("CAP_NET_ADMIN", CAP_NET_ADMIN),
        ("CAP_NET_RAW", CAP_NET_RAW),
        ("CAP_SYS_ADMIN", CAP_SYS_ADMIN),
    ];
    let missing: Vec<&str> = need
        .into_iter()
        .filter(|(_, bit)| (cap_eff & (1u64 << bit)) == 0)
        .map(|(name, _)| name)
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        bail!("missing capabilities: {}", missing.join(", "))
    }
}
