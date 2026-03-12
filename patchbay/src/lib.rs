#![warn(missing_docs)]
#![warn(unreachable_pub)]
#![warn(unused_qualifications)]
#![deny(unsafe_op_in_unsafe_fn)]

//! Realistic network topologies from Linux network namespaces.
//!
//! patchbay gives you routers, devices, NAT, firewalls, and link conditions
//! backed by real kernel networking. Each node lives in its own network
//! namespace with veth pairs, nftables rules, and tc netem qdiscs, so the
//! code running inside sees exactly what it would see on a separate machine.
//! No root required: the library bootstraps into an unprivileged user
//! namespace before Tokio starts.
//!
//! # Building a topology
//!
//! A [`Lab`] owns a root namespace with an IX (Internet Exchange) bridge.
//! [`Lab::add_router`] and [`Lab::add_device`] return builders that configure
//! the node and wire it into the topology. Once built, [`Device`], [`Router`],
//! and [`Ix`] are lightweight cloneable handles for running code inside the
//! namespace.
//!
//! ```no_run
//! # use patchbay::*;
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> anyhow::Result<()> {
//! let lab = Lab::new().await?;
//!
//! // A datacenter router: devices get globally routable IPs, no NAT.
//! let dc = lab
//!     .add_router("dc")
//!     .preset(RouterPreset::Public)
//!     .build()
//!     .await?;
//!
//! // A home router behind the IX with residential NAT and a firewall.
//! let home = lab
//!     .add_router("home")
//!     .preset(RouterPreset::Home)
//!     .build()
//!     .await?;
//!
//! // A laptop on the home network with a lossy WiFi link.
//! let laptop = lab
//!     .add_device("laptop")
//!     .iface("wlan0", home.id(), Some(LinkCondition::Wifi))
//!     .build()
//!     .await?;
//!
//! // A server in the datacenter with a clean link.
//! let server = lab
//!     .add_device("server")
//!     .iface("eth0", dc.id(), None)
//!     .build()
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Running code in namespaces
//!
//! Every handle provides several ways to execute code inside its namespace.
//! Namespace transitions happen on dedicated worker threads, so callers never
//! call `setns` directly and can use any Tokio runtime flavor.
//!
//! ```no_run
//! # use patchbay::*;
//! # async fn example(dev: Device, server: Device) -> anyhow::Result<()> {
//! // Async task on the namespace's single-threaded tokio runtime.
//! let addr = server.ip().unwrap();
//! let jh = dev.spawn(async move |_dev| {
//!     let stream = tokio::net::TcpStream::connect((addr, 8080)).await?;
//!     anyhow::Ok(stream.local_addr()?)
//! })?;
//! let local_addr = jh.await??;
//!
//! // OS command (tokio::process::Command variant).
//! let mut child = dev.spawn_command({
//!     let mut cmd = tokio::process::Command::new("ping");
//!     cmd.args(["-c1", &server.ip().unwrap().to_string()]);
//!     cmd
//! })?;
//! child.wait().await?;
//!
//! // Blocking closure on the sync worker (fast, non-I/O work).
//! let lo_addr = dev.run_sync(|| {
//!     let sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
//!     Ok(sock.local_addr()?)
//! })?;
//! # Ok(())
//! # }
//! ```
//!
//! # Router presets
//!
//! [`RouterPreset`] bundles NAT, firewall, IP support, and address-pool
//! settings to match real-world deployments in a single call. Individual
//! builder methods called after [`RouterBuilder::preset`] override preset
//! values.
//!
//! | Preset | NAT | Firewall | IP | Use case |
//! |--------|-----|----------|----|----------|
//! | [`Home`](RouterPreset::Home) | EIM + APDF | Block inbound | Dual | Consumer router (FritzBox, UniFi) |
//! | [`Public`](RouterPreset::Public) | None | None | Dual | Datacenter switch, ISP handoff |
//! | [`IspCgnat`](RouterPreset::IspCgnat) | CGNAT (EIM) | None | Dual | Carrier with shared IPv4 |
//! | [`IspV6`](RouterPreset::IspV6) | NAT64 | Block inbound | V6 | T-Mobile, Jio-style IPv6-only |
//! | [`Corporate`](RouterPreset::Corporate) | Symmetric | TCP 80/443 only | Dual | Enterprise firewall |
//! | [`Hotel`](RouterPreset::Hotel) | Symmetric | No UDP | V4 | Guest WiFi |
//! | [`Cloud`](RouterPreset::Cloud) | Symmetric | None | Dual | AWS/GCP NAT gateway |
//!
//! # Link conditions
//!
//! [`LinkCondition`] presets model common last-mile networks via `tc netem`
//! and `tc tbf`. Apply them at build time through the device builder or
//! change them dynamically with [`Device::set_link_condition`].
//!
//! ```no_run
//! # use patchbay::*;
//! # async fn example(dev: Device) -> anyhow::Result<()> {
//! // Switch from WiFi to a degraded 3G link at runtime.
//! dev.set_link_condition("wlan0", Some(LinkCondition::Mobile3G))
//!     .await?;
//!
//! // Or use fully custom parameters.
//! dev.set_link_condition(
//!     "wlan0",
//!     Some(LinkCondition::Manual(LinkLimits {
//!         latency_ms: 200,
//!         loss_pct: 5.0,
//!         rate_kbit: 500,
//!         ..Default::default()
//!     })),
//! )
//! .await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Multi-region routing
//!
//! Routers can be placed in regions with simulated inter-region latency.
//! Traffic between routers in different regions flows through dedicated
//! region-router namespaces with configurable impairment.
//!
//! ```no_run
//! # use patchbay::*;
//! # async fn example(lab: Lab) -> anyhow::Result<()> {
//! let eu = lab.add_region("eu").await?;
//! let us = lab.add_region("us").await?;
//! lab.link_regions(&eu, &us, RegionLink::good(80)).await?;
//!
//! let dc_eu = lab.add_router("dc-eu").region(&eu).build().await?;
//! let dc_us = lab.add_router("dc-us").region(&us).build().await?;
//! // Traffic between dc-eu and dc-us now carries ~80 ms of added latency.
//!
//! // Fault injection: break and restore the link at runtime.
//! lab.break_region_link(&eu, &us)?;
//! lab.restore_region_link(&eu, &us)?;
//! # Ok(())
//! # }
//! ```
//!
//! # Dynamic operations
//!
//! The topology is not static after setup. Devices can switch uplinks, change
//! default routes, toggle links, and routers can swap NAT modes, all at
//! runtime:
//!
//! ```no_run
//! # use patchbay::*;
//! # async fn example(dev: Device, router: Router, other: Router) -> anyhow::Result<()> {
//! dev.replug_iface("wlan0", other.id()).await?;
//! dev.set_default_route("eth0").await?;
//! dev.link_down("wlan0").await?;
//! dev.link_up("wlan0").await?;
//! router.set_nat_mode(Nat::Corporate).await?;
//! router.flush_nat_state().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # TOML configuration
//!
//! Labs can also be loaded from TOML files with [`Lab::load`]. See the
//! [TOML reference](https://n0-computer.github.io/patchbay/reference/toml-reference.html)
//! for the full syntax.
//!
//! # Requirements
//!
//! Linux with unprivileged user namespaces enabled, plus `nft` and `tc` in
//! PATH. No root needed. Call [`init_userns`] before spawning any threads
//! to enter the user namespace.

use anyhow::{anyhow, bail, Context, Result};

/// TOML configuration structures used by [`Lab::load`].
pub mod config;
/// Shared filename constants for the run output directory.
pub mod consts;
pub(crate) mod core;
/// Lab event system: typed events, state reducer, file writer.
pub mod event;
pub(crate) mod firewall;
pub(crate) mod handles;
mod lab;
pub(crate) mod nat;
pub(crate) mod nat64;
mod netlink;
mod netns;
#[path = "tracing.rs"]
mod ns_tracing;
mod qdisc;
#[allow(dead_code)]
pub(crate) mod test_utils;
#[cfg(test)]
mod tests;
mod userns;
/// String sanitizers for filenames and environment variable names.
pub mod util;
pub(crate) mod writer;

pub use firewall::PortPolicy;
pub use ipnet::Ipv4Net;
pub use lab::{
    ConntrackTimeouts, DefaultRegions, Device, DeviceBuilder, DeviceIface, Firewall,
    FirewallConfig, FirewallConfigBuilder, IpSupport, Ipv6DadMode, Ipv6Profile,
    Ipv6ProvisioningMode, Ix, Lab, LabOpts, LinkCondition, LinkLimits, Nat, NatConfig,
    NatConfigBuilder, NatFiltering, NatMapping, NatV6Mode, OutDir, Region, RegionLink, Router,
    RouterBuilder, RouterIface, RouterPreset,
};

pub use crate::{
    core::{NodeId, ReflectorGuard},
    event::{IfaceCounters, IfaceSnapshot, LabEvent, LabEventKind, LabState},
    userns::{init_userns, init_userns_for_ctor},
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
