//! patchbay — Linux network-namespace lab for NAT/routing experiments.
//!
//! Each router and device lives in its own Linux network namespace with real
//! kernel networking (veth pairs, nftables NAT, tc netem impairment). The
//! library handles namespace creation, IP allocation, and teardown.
//!
//! # Quick start (from TOML)
//! ```no_run
//! # use patchbay::Lab;
//! # use std::process::Command;
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> anyhow::Result<()> {
//! let lab = Lab::load("lab.toml").await?;
//! let dev = lab.device_by_name("home-eu1").unwrap();
//! let mut cmd = Command::new("ping");
//! cmd.args(["-c1", "8.8.8.8"]);
//! dev.spawn_command(cmd)?;
//! # Ok(())
//! # }
//! ```
//!
//! # Builder API
//! ```no_run
//! # use patchbay::{Lab, Nat};
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> anyhow::Result<()> {
//! let lab = Lab::new().await;
//! let isp = lab
//!     .add_router("isp1")
//!     .nat(Nat::Cgnat)
//!     .build()
//!     .await?;
//! let home = lab
//!     .add_router("home1")
//!     .upstream(isp.id())
//!     .nat(Nat::Home)
//!     .build()
//!     .await?;
//! lab.add_device("dev1")
//!     .iface("eth0", home.id(), None)
//!     .build()
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! Namespace transitions are executed inside dedicated worker threads; callers
//! can use any Tokio runtime flavor.

use anyhow::{anyhow, bail, Context, Result};

/// Defines TOML configuration structures used by [`Lab::load`].
pub mod config;
pub(crate) mod core;
pub(crate) mod firewall;
pub(crate) mod handles;
mod lab;
pub(crate) mod nat;
mod netlink;
mod netns;
mod qdisc;
/// Probe and reflector helpers for integration tests.
pub mod test_utils;
#[cfg(test)]
mod tests;
mod userns;
/// Shared string sanitizers.
pub mod util;

pub use lab::{
    ConntrackTimeouts, DefaultRegions, Device, DeviceBuilder, DeviceIface, Firewall,
    FirewallConfig, FirewallConfigBuilder, IpSupport, Ix, Lab, LinkCondition, LinkLimits, Nat,
    NatConfig, NatConfigBuilder, NatFiltering, NatMapping, NatV6Mode, ObservedAddr, Region,
    RegionLink, Router, RouterBuilder,
};

pub use crate::{
    core::NodeId,
    userns::{init_userns, init_userns_for_ctor},
};

pub use ipnet::Ipv4Net;

/// Verifies the process has enough privileges to manage namespaces, routes, and raw sockets.
///
/// Checks for `CAP_NET_ADMIN`, `CAP_NET_RAW`, and `CAP_SYS_ADMIN` in the
/// effective capability set. Returns `Ok(())` if the process is root or all
/// capabilities are present.
///
/// # Errors
///
/// Returns an error listing the missing capabilities.
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
