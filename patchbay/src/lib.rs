#![warn(missing_docs)]
#![warn(unreachable_pub)]
#![warn(unused_qualifications)]
#![deny(unsafe_op_in_unsafe_fn)]

//! Linux network-namespace lab for NAT, routing, and link-condition experiments.
//!
//! patchbay builds realistic network topologies from Linux network namespaces.
//! Each router and device lives in its own namespace with real kernel networking:
//! veth pairs, nftables NAT, and tc netem link conditions. The library handles
//! namespace creation, IP allocation, and teardown automatically.
//!
//! # Platform support
//!
//! The full functionality requires Linux. On other platforms (macOS, Windows),
//! stub types are provided so that dependent crates like `patchbay-server` and
//! `patchbay-vm` can compile. Cross-compilation to Linux works from any platform.
//!
//! # How it works
//!
//! A [`Lab`] owns a root namespace with an IX (Internet Exchange) bridge.
//! Routers connect to the IX (or to each other as sub-routers) and devices
//! connect to routers via downstream bridges. Each namespace gets a dedicated
//! async worker thread (single-threaded tokio runtime) and a lazy sync worker.
//! [`Device`], [`Router`], and [`Ix`] are lightweight cloneable handles that
//! dispatch work to these workers, so callers never call `setns` directly.
//!
//! NAT is configured per-router via [`Nat`] presets (`Home`, `Corporate`,
//! `CloudNat`, `FullCone`, `Cgnat`) or custom [`NatConfig`] values. Link
//! conditions use [`LinkCondition`] presets (`Wifi`, `Mobile4G`, etc.) or
//! custom [`LinkLimits`]. Both can be changed at runtime.
//!
//! The whole thing runs unprivileged. Call [`init_userns`] before spawning
//! any threads to bootstrap into a user namespace with full networking
//! capabilities.
//!
//! # Quick start (from TOML)
//!
//! ```no_run
//! # #[cfg(target_os = "linux")]
//! # use patchbay::Lab;
//! # use std::process::Command;
//! # #[cfg(target_os = "linux")]
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> anyhow::Result<()> {
//! # let lab = Lab::load("lab.toml").await?;
//! # let dev = lab.device_by_name("home-eu1").unwrap();
//! # let mut cmd = Command::new("ping");
//! # cmd.args(["-c1", "8.8.8.8"]);
//! # dev.spawn_command_sync(cmd)?;
//! # Ok(())
//! # }
//! # #[cfg(not(target_os = "linux"))]
//! # fn main() {}
//! ```
//!
//! # Builder API
//!
//! ```no_run
//! # #[cfg(target_os = "linux")]
//! # use patchbay::{Lab, Nat};
//! # #[cfg(target_os = "linux")]
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> anyhow::Result<()> {
//! # let lab = Lab::new().await?;
//! # let isp = lab.add_router("isp1").nat(Nat::Cgnat).build().await?;
//! # let home = lab
//! #     .add_router("home1")
//! #     .upstream(isp.id())
//! #     .nat(Nat::Home)
//! #     .build()
//! #     .await?;
//! # lab.add_device("dev1")
//! #     .iface("eth0", home.id(), None)
//! #     .build()
//! #     .await?;
//! # Ok(())
//! # }
//! # #[cfg(not(target_os = "linux"))]
//! # fn main() {}
//! ```
//!
//! Namespace transitions are executed inside dedicated worker threads, so
//! callers can use any Tokio runtime flavor.

// ─────────────────────────────────────────────────────────────────────────────
// Cross-platform modules (always compiled)
// ─────────────────────────────────────────────────────────────────────────────

/// TOML configuration structures used by [`Lab::load`].
pub mod config;
/// Shared filename constants for the run output directory.
pub mod consts;
/// Lab event system: typed events, state reducer, file writer.
pub mod event;
/// String sanitizers for filenames and environment variable names.
pub mod util;
/// Event file writer and run discovery.
pub mod writer;

// Cross-platform type definitions (firewall, nat, qdisc types)
#[cfg(target_os = "linux")]
pub(crate) mod firewall;
#[cfg(target_os = "linux")]
pub(crate) mod nat;
#[cfg(target_os = "linux")]
mod qdisc;

#[cfg(not(target_os = "linux"))]
#[path = "types_portable.rs"]
mod types_portable;

// ─────────────────────────────────────────────────────────────────────────────
// Linux-only modules
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
pub(crate) mod core;
#[cfg(target_os = "linux")]
pub(crate) mod handles;
#[cfg(target_os = "linux")]
mod lab;
#[cfg(target_os = "linux")]
pub(crate) mod nat64;
#[cfg(target_os = "linux")]
mod netlink;
#[cfg(target_os = "linux")]
mod netns;
#[cfg(target_os = "linux")]
#[path = "tracing.rs"]
mod ns_tracing;
#[cfg(target_os = "linux")]
mod userns;

/// Probe and reflector helpers for integration tests.
#[cfg(target_os = "linux")]
pub mod test_utils;
#[cfg(all(target_os = "linux", test))]
mod tests;

// ─────────────────────────────────────────────────────────────────────────────
// Stub module for non-Linux platforms
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(not(target_os = "linux"))]
mod stubs;

// ─────────────────────────────────────────────────────────────────────────────
// Public exports
// ─────────────────────────────────────────────────────────────────────────────

pub use ipnet::Ipv4Net;

// Linux: export real types
#[cfg(target_os = "linux")]
pub use firewall::PortPolicy;
#[cfg(target_os = "linux")]
pub use lab::{
    ConntrackTimeouts, DefaultRegions, Device, DeviceBuilder, DeviceIface, Firewall,
    FirewallConfig, FirewallConfigBuilder, IpSupport, Ix, Lab, LabOpts, LinkCondition, LinkLimits,
    Nat, NatConfig, NatConfigBuilder, NatFiltering, NatMapping, NatV6Mode, ObservedAddr, Region,
    RegionLink, Router, RouterBuilder, RouterPreset,
};
#[cfg(target_os = "linux")]
pub use crate::core::NodeId;
#[cfg(target_os = "linux")]
pub use crate::userns::{init_userns, init_userns_for_ctor};

// Non-Linux: export stubs and portable types
#[cfg(not(target_os = "linux"))]
pub use stubs::{
    check_caps, init_userns, init_userns_for_ctor, DefaultRegions, Device, DeviceBuilder,
    DeviceIface, Ix, Lab, LabOpts, NodeId, ObservedAddr, Region, RegionLink, Router, RouterBuilder,
};
#[cfg(not(target_os = "linux"))]
pub use types_portable::{
    ConntrackTimeouts, Firewall, FirewallConfig, FirewallConfigBuilder, IpSupport, LinkCondition,
    LinkLimits, Nat, NatConfig, NatConfigBuilder, NatFiltering, NatMapping, NatV6Mode, PortPolicy,
    RouterPreset,
};

// Cross-platform event types
pub use crate::{
    event::{IfaceCounters, IfaceSnapshot, LabEvent, LabEventKind, LabState},
    writer::{discover_runs, RunInfo},
};

/// Verifies the process has enough privileges to manage namespaces, routes, and raw sockets.
///
/// Checks for `CAP_NET_ADMIN`, `CAP_NET_RAW`, and `CAP_SYS_ADMIN` in the
/// effective capability set. Returns `Ok(())` if the process is root or all
/// capabilities are present.
///
/// # Errors
///
/// Returns an error listing the missing capabilities.
#[cfg(target_os = "linux")]
pub fn check_caps() -> anyhow::Result<()> {
    use anyhow::{anyhow, bail, Context};

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
