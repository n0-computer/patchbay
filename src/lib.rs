//! netsim-rs — Linux network-namespace lab for NAT/routing experiments.
//!
//! # Quick start (from TOML)
//! ```no_run
//! # use netsim::Lab;
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
//! # use netsim::{Gateway, Lab, NatMode};
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> anyhow::Result<()> {
//! let mut lab = Lab::new();
//! let isp  = lab.add_isp("isp1", "eu", false, None)?;
//! let home = lab.add_home("home1", isp, NatMode::DestinationIndependent)?;
//! lab.add_device("dev1", Gateway::Lan(home), None)?;
//! lab.build().await?;
//! # Ok(())
//! # }
//! ```
//!
//! **Important**: `build()` uses `setns(2)` which is thread-local.
//! Always call it (and any test using it) on a `current_thread` Tokio runtime.

#![allow(dead_code)]

use anyhow::{anyhow, bail, Context, Result};
use nix::unistd::Pid;
use serde::Deserialize;
use std::{
    collections::HashMap,
    io::ErrorKind,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    path::Path,
    process::ExitStatus,
    thread,
    time::{Duration, Instant},
};
use tracing::debug;

pub mod core;
mod qdisc;
use crate::core::{
    cleanup_netns, resources, run_in_netns, spawn_in_netns, spawn_in_netns_thread,
    with_netns_thread, CoreConfig, DownstreamPool, LabCore, RouterConfig, TaskHandle,
};

/// Stable identifier for devices/routers/switches in the lab.
pub use crate::core::NodeId;

// ─────────────────────────────────────────────
// Public types
// ─────────────────────────────────────────────

/// NAT mapping behaviour at a home router.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NatMode {
    /// Endpoint-independent mapping: same external port regardless of destination.
    DestinationIndependent,
    /// Endpoint-dependent (symmetric-ish): different port per destination.
    DestinationDependent,
}

/// Link-layer impairment profile applied via `tc netem`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Impair {
    /// ~20 ms delay, 5 ms jitter.
    Wifi,
    /// ~50 ms delay, 20 ms jitter, 1 % loss.
    Mobile,
    /// Custom impairment settings.
    Manual {
        /// Rate limit in kbit/s.
        rate: u32,
        /// Packet loss percentage (0.0 - 100.0).
        loss: f32,
        /// One-way latency in milliseconds.
        latency: u32,
    },
}

impl<'de> Deserialize<'de> for Impair {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Preset(String),
            Manual { rate: u32, loss: f32, latency: u32 },
        }

        match Repr::deserialize(deserializer)? {
            Repr::Preset(s) => match s.as_str() {
                "wifi" => Ok(Impair::Wifi),
                "mobile" => Ok(Impair::Mobile),
                _ => Err(serde::de::Error::custom("unknown impair preset")),
            },
            Repr::Manual {
                rate,
                loss,
                latency,
            } => Ok(Impair::Manual {
                rate,
                loss,
                latency,
            }),
        }
    }
}

/// Where a device is attached (high-level API).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gateway {
    /// Device sits behind a home router LAN.
    Lan(NodeId),
    /// Device lives inside a DC namespace (server/relay).
    Dc(NodeId),
    /// Device connects directly to an ISP (e.g. mobile phone with SIM).
    Isp(NodeId),
}

/// Observed external address as reported by a STUN-like reflector.
#[derive(Clone, Debug)]
pub struct ObservedAddr {
    /// External socket address observed by the reflector.
    pub observed: SocketAddr,
}

// ─────────────────────────────────────────────
// Lab
// ─────────────────────────────────────────────

/// High-level lab API built on top of `LabCore`.
///
/// The `dc/home/isp` notions are convenience shorthands for tests and configs.
pub struct Lab {
    /// Short process-unique prefix used on root-namespace interface names.
    prefix: String,
    bridge_tag: String,
    bridge_counter: u32,
    ns_counter: u32,
    isp_by_name: HashMap<String, NodeId>,
    dc_by_name: HashMap<String, NodeId>,
    home_by_name: HashMap<String, NodeId>,
    device_by_name: HashMap<String, NodeId>,
    /// (from_region, to_region, latency_ms) pairs; applied as tc netem during build.
    region_latencies: Vec<(String, String, u32)>,

    /// Background tasks spawned by the lab (reflectors, commands).
    children: Vec<ChildTask>,

    /// Low-level topology model.
    core: LabCore,
}

enum ChildTask {
    Process(std::process::Child),
    Thread {
        handle: TaskHandle,
        join: thread::JoinHandle<Result<()>>,
    },
}

impl Lab {
    // ── Constructors ────────────────────────────────────────────────────

    /// Create a new lab with default address ranges and IX settings.
    pub fn new() -> Self {
        let pid = std::process::id();
        let pid_tag = pid % 9999 + 1;
        let prefix = format!("lab-p{}", pid_tag); // e.g. "lab-p1234"
        let bridge_tag = format!("p{}", pid_tag);
        let ix_gw = Ipv4Addr::new(203, 0, 113, 1);
        resources().register_prefix(&prefix);
        let core = LabCore::new(CoreConfig {
            prefix: prefix.clone(),
            ix_br: format!("br-{}-1", bridge_tag),
            ix_gw,
            ix_cidr: "203.0.113.0/24".parse().expect("valid ix cidr"),
            private_cidr: "10.0.0.0/16".parse().expect("valid private cidr"),
            public_cidr: "203.0.113.0/24".parse().expect("valid public cidr"),
        });
        resources().register_prefix(&format!("br-{}-", bridge_tag));
        Self {
            prefix,
            bridge_tag,
            bridge_counter: 2,
            ns_counter: 1,
            isp_by_name: HashMap::new(),
            dc_by_name: HashMap::new(),
            home_by_name: HashMap::new(),
            device_by_name: HashMap::new(),
            region_latencies: vec![],
            children: vec![],
            core,
        }
    }

    fn next_bridge_name(&mut self) -> String {
        let name = format!("br-{}-{}", self.bridge_tag, self.bridge_counter);
        self.bridge_counter = self.bridge_counter.saturating_add(1);
        name
    }

    /// Initialize tracing for this crate (idempotent). Honors `RUST_LOG`;
    /// defaults to `netsim=debug` if unset.
    /// Initialize tracing for this crate (idempotent).
    ///
    /// Honors `RUST_LOG`; defaults to `netsim=debug` if unset.
    pub fn init_tracing() {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("netsim=debug"));
        let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
    }

    /// Parse `lab.toml`, instantiate the lab, run `build()`, and return the
    /// ready-to-use lab.  Must be called on a `current_thread` Tokio runtime.
    /// Parse `lab.toml`, build the network, and return a ready-to-use lab.
    ///
    /// Must be called on a `current_thread` Tokio runtime.
    pub async fn load(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path).context("read lab config")?;
        let cfg: config::LabConfig = toml::from_str(&text).context("parse lab config")?;
        let mut lab = Self::from_config(cfg)?;
        lab.build().await?;
        Ok(lab)
    }

    /// Build a `Lab` from a parsed config without building the network yet.
    fn from_config(cfg: config::LabConfig) -> Result<Self> {
        let mut lab = Self::new();

        // Region latency pairs.
        if let Some(regions) = &cfg.region {
            for (from, rcfg) in regions {
                for (to, &ms) in &rcfg.latencies {
                    lab.region_latencies.push((from.clone(), to.clone(), ms));
                }
            }
        }

        for isp_cfg in &cfg.isp {
            let cgnat = isp_cfg.nat == Some(config::IspNat::Cgnat);
            lab.add_isp(
                &isp_cfg.name,
                &isp_cfg.region,
                cgnat,
                isp_cfg.impair_downstream.as_ref().map(|i| i.latency),
            )?;
        }
        for dc_cfg in &cfg.dc {
            lab.add_dc(&dc_cfg.name, &dc_cfg.region)?;
        }
        for lan_cfg in &cfg.lan {
            let isp_id = lab.isp_by_name.get(&lan_cfg.isp).copied().ok_or_else(|| {
                anyhow!(
                    "lan '{}' references unknown isp '{}'",
                    lan_cfg.name,
                    lan_cfg.isp
                )
            })?;
            lab.add_home(&lan_cfg.name, isp_id, lan_cfg.nat)?;
        }
        for dev_cfg in &cfg.device {
            let gw = lab.gateway_from_name(&dev_cfg.gateway)?;
            lab.add_device(&dev_cfg.name, gw, dev_cfg.impair.clone())?;
        }
        Ok(lab)
    }

    // ── Builder methods (sync — just populate data structures) ──────────

    /// Add an ISP router.
    ///
    /// If `cgnat` is true the ISP uses CGNAT and assigns private downstream IPs.
    pub fn add_isp(
        &mut self,
        name: &str,
        region: &str,
        cgnat: bool,
        impair_downstream_ms: Option<u32>,
    ) -> Result<NodeId> {
        if self.isp_by_name.contains_key(name) {
            bail!("isp '{}' already exists", name);
        }
        let ns = self.ns_name();
        let downstream_pool = if cgnat {
            DownstreamPool::Private
        } else {
            DownstreamPool::Public
        };
        let downlink_bridge = self.next_bridge_name();
        let cfg = RouterConfig {
            nat: None,
            cgnat,
            downlink_bridge,
            downstream_pool,
        };
        let id = self
            .core
            .add_router(name, ns, cfg, Some(region.to_string()));
        let sub_switch = self.core.add_switch(&format!("{name}-sub"), None, None);
        let _ = self.core.connect_router_downlink(id, sub_switch)?;
        let ix_ip = self.core.alloc_ix_ip_low();
        let _ = self
            .core
            .connect_router_uplink(id, self.core.ix_sw(), Some(ix_ip))?;
        let _ = region;
        let _ = impair_downstream_ms;
        self.isp_by_name.insert(name.to_string(), id);
        Ok(id)
    }

    /// Add a DC router with public downstream addressing.
    pub fn add_dc(&mut self, name: &str, region: &str) -> Result<NodeId> {
        if self.dc_by_name.contains_key(name) {
            bail!("dc '{}' already exists", name);
        }
        let ns = self.ns_name();
        let downlink_bridge = self.next_bridge_name();
        let cfg = RouterConfig {
            nat: None,
            cgnat: false,
            downlink_bridge,
            downstream_pool: DownstreamPool::Public,
        };
        let id = self
            .core
            .add_router(name, ns, cfg, Some(region.to_string()));
        let lan_switch = self.core.add_switch(&format!("{name}-lan"), None, None);
        let _ = self.core.connect_router_downlink(id, lan_switch)?;
        let ix_ip = self.core.alloc_ix_ip_high();
        let _ = self
            .core
            .connect_router_uplink(id, self.core.ix_sw(), Some(ix_ip))?;
        let _ = region;
        self.dc_by_name.insert(name.to_string(), id);
        Ok(id)
    }

    /// Add a home router connected to an ISP.
    pub fn add_home(&mut self, name: &str, isp: NodeId, nat: NatMode) -> Result<NodeId> {
        if self.home_by_name.contains_key(name) {
            bail!("home '{}' already exists", name);
        }
        let ns = self.ns_name();
        let downlink_bridge = self.next_bridge_name();
        let cfg = RouterConfig {
            nat: Some(nat),
            cgnat: false,
            downlink_bridge,
            downstream_pool: DownstreamPool::Private,
        };
        let id = self.core.add_router(name, ns, cfg, None);
        let lan_switch = self.core.add_switch(&format!("{name}-lan"), None, None);
        let _ = self.core.connect_router_downlink(id, lan_switch)?;

        let isp_downlink = self
            .core
            .router(isp)
            .and_then(|r| r.downlink)
            .ok_or_else(|| anyhow!("isp router missing downlink"))?;
        let _ = self.core.connect_router_uplink(id, isp_downlink, None)?;
        self.home_by_name.insert(name.to_string(), id);
        Ok(id)
    }

    /// Add a device and attach it to a gateway router.
    pub fn add_device(
        &mut self,
        name: &str,
        gateway: Gateway,
        impair: Option<Impair>,
    ) -> Result<NodeId> {
        if self.device_by_name.contains_key(name) {
            bail!("device '{}' already exists", name);
        }
        let ns = self.ns_name();
        let id = self.core.add_device(name, ns, impair);
        let gw_router = match gateway {
            Gateway::Lan(id) | Gateway::Dc(id) | Gateway::Isp(id) => id,
        };
        if self.core.router(gw_router).is_none() {
            bail!("unknown gateway router id");
        }
        let _ = self.core.connect_device_to_router(id, gw_router)?;
        self.device_by_name.insert(name.to_string(), id);
        Ok(id)
    }

    // ── build ────────────────────────────────────────────────────────────

    /// Create all namespaces, links, addresses, routes, and NAT rules.
    ///
    /// Must be called on a `current_thread` Tokio runtime because `setns(2)`
    /// is thread-local and we must ensure all netlink operations happen in the
    /// correct namespace on the same OS thread.
    /// Create all namespaces, links, addresses, routes, and NAT rules.
    ///
    /// Must be called on a `current_thread` Tokio runtime because `setns(2)`
    /// is thread-local and we must ensure all netlink operations happen in the
    /// correct namespace on the same OS thread.
    pub async fn build(&mut self) -> Result<()> {
        self.core.build(&self.region_latencies).await
    }

    // ── User-facing API ─────────────────────────────────────────────────

    /// Add a one-way inter-region latency in milliseconds.
    /// Add a one-way inter-region latency in milliseconds.
    pub fn add_region_latency(&mut self, from: &str, to: &str, latency_ms: u32) {
        self.region_latencies
            .push((from.to_string(), to.to_string(), latency_ms));
    }

    /// Run a command inside a device namespace (blocks until it exits).
    ///
    /// ```no_run
    /// # use netsim::Lab;
    /// # use std::process::Command;
    /// # fn main() -> anyhow::Result<()> {
    /// # let lab = Lab::new();
    /// let mut cmd = Command::new("ping");
    /// cmd.args(["-c1", "1.1.1.1"]);
    /// lab.run_on("home-eu1", cmd)?;
    /// # Ok(())
    /// # }
    /// ```
    /// Run a command inside a device namespace (blocks until it exits).
    pub fn run_on(&self, name: &str, cmd: std::process::Command) -> Result<ExitStatus> {
        let id = self
            .device_by_name
            .get(name)
            .copied()
            .ok_or_else(|| anyhow!("unknown device '{}'", name))?;
        let ns = self.core.device_ns(id)?;
        run_in_netns(ns, cmd)
    }

    /// Run a closure inside a named network namespace.
    ///
    /// The closure runs on a dedicated OS thread so you can coordinate with
    /// channels or build a current-thread runtime inside it.
    ///
    /// Note: `ns_name` is the namespace name (e.g. from `lab.node_ns(id)`).
    /// Run a closure inside a named network namespace.
    ///
    /// The closure runs on a dedicated OS thread so you can coordinate with
    /// channels or build a current-thread runtime inside it.
    pub fn run_in<F, R>(ns_name: &str, f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        with_netns_thread(ns_name, f)
    }

    /// Spawn a thread that enters `ns_name`, runs `f`, restores the namespace,
    /// and returns its result via the join handle.
    /// Spawn a thread that enters `ns_name`, runs `f`, restores the namespace,
    /// and returns its result via the join handle.
    pub fn run_in_thread<F, R>(ns_name: &str, f: F) -> thread::JoinHandle<Result<R>>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        spawn_in_netns_thread(ns_name.to_string(), f)
    }

    /// Spawn a long-running process inside a device namespace and return its PID.
    /// The process is killed when the `Lab` is dropped.
    /// Spawn a long-running process inside a device namespace and return its PID.
    /// The process is killed when the `Lab` is dropped.
    pub fn spawn_on(&mut self, name: &str, cmd: std::process::Command) -> Result<Pid> {
        let id = self
            .device_by_name
            .get(name)
            .copied()
            .ok_or_else(|| anyhow!("unknown device '{}'", name))?;
        let ns = self.core.device_ns(id)?.to_string();
        let child = spawn_in_netns(&ns, cmd)?;
        let pid = Pid::from_raw(child.id() as i32);
        self.children.push(ChildTask::Process(child));
        Ok(pid)
    }

    // ── Reflector / probe helpers (mainly for tests) ─────────────────────

    /// Spawn a UDP reflector in a named device/DC/ISP namespace.
    ///
    /// Use `dc_ix_ip(dc)` or `isp_public_ip(isp)` to pick a bind address.
    /// Spawn a UDP reflector in a named device/DC/ISP namespace.
    ///
    /// Use `dc_ix_ip(dc)` or `isp_public_ip(isp)` to pick a bind address.
    pub fn spawn_reflector(&mut self, ns_name: &str, bind: SocketAddr) -> Result<TaskHandle> {
        let (handle, join) = spawn_reflector_in(Some(ns_name), bind)?;
        self.children.push(ChildTask::Thread {
            handle: handle.clone(),
            join,
        });
        Ok(handle)
    }

    /// Spawn a UDP reflector in the root namespace (IX bridge side).
    /// Spawn a UDP reflector in the root namespace (IX bridge side).
    pub fn spawn_reflector_on_ix(&mut self, bind: SocketAddr) -> Result<TaskHandle> {
        let (handle, join) = spawn_reflector_in(None, bind)?;
        self.children.push(ChildTask::Thread {
            handle: handle.clone(),
            join,
        });
        Ok(handle)
    }

    /// Probe the NAT mapping seen by a reflector from a named device.
    /// Probe the NAT mapping seen by a reflector from a named device.
    pub fn probe_udp_mapping(&self, device: &str, reflector: SocketAddr) -> Result<ObservedAddr> {
        let id = self
            .device_by_name
            .get(device)
            .copied()
            .ok_or_else(|| anyhow!("unknown device '{}'", device))?;
        let ns = self.core.device_ns(id)?;
        let base = 40000u16;
        let port = base + ((id.0 % 20000) as u16);
        probe_in_ns(ns, reflector, Duration::from_millis(500), Some(port))
    }

    // ── Lookup helpers ───────────────────────────────────────────────────

    /// Return the ISP's public IX address.
    pub fn isp_public_ip(&self, isp: NodeId) -> Result<IpAddr> {
        let r = self.core.router(isp).context("unknown isp id")?;
        Ok(IpAddr::V4(r.upstream_ip.context("missing ix ip")?))
    }

    /// Return the DC's IX address.
    pub fn dc_ix_ip(&self, dc: NodeId) -> Result<Ipv4Addr> {
        let r = self.core.router(dc).context("unknown dc id")?;
        Ok(r.upstream_ip.context("missing ix ip")?)
    }

    /// Return the network namespace name for a node.
    pub fn node_ns(&self, id: NodeId) -> Result<&str> {
        if let Some(r) = self.core.router(id) {
            return Ok(&r.ns);
        }
        if let Some(d) = self.core.device(id) {
            return Ok(&d.ns);
        }
        Err(anyhow!("unknown node id"))
    }

    /// Return the router's downstream gateway IP.
    pub fn router_downlink_gw(&self, id: NodeId) -> Result<Ipv4Addr> {
        self.core
            .router(id)
            .and_then(|rt| rt.downstream_gw)
            .ok_or_else(|| anyhow!("router missing downstream gw"))
    }

    /// Return the router's uplink IP.
    pub fn router_uplink_ip(&self, id: NodeId) -> Result<Ipv4Addr> {
        self.core
            .router(id)
            .and_then(|rt| rt.upstream_ip)
            .ok_or_else(|| anyhow!("router missing upstream ip"))
    }

    /// Return the device's assigned IP.
    pub fn device_ip(&self, id: NodeId) -> Result<Ipv4Addr> {
        self.core
            .device(id)
            .and_then(|dev| dev.ip)
            .ok_or_else(|| anyhow!("device missing ip"))
    }

    /// Resolve an ISP name to its `NodeId`.
    pub fn isp_id(&self, name: &str) -> Option<NodeId> {
        self.isp_by_name.get(name).copied()
    }
    /// Resolve a DC name to its `NodeId`.
    pub fn dc_id(&self, name: &str) -> Option<NodeId> {
        self.dc_by_name.get(name).copied()
    }
    /// Resolve a home name to its `NodeId`.
    pub fn home_id(&self, name: &str) -> Option<NodeId> {
        self.home_by_name.get(name).copied()
    }
    /// Resolve a device name to its `NodeId`.
    pub fn device_id(&self, name: &str) -> Option<NodeId> {
        self.device_by_name.get(name).copied()
    }

    /// The IX gateway IP (203.0.113.1) — useful for binding a root-ns reflector.
    /// Return the IX gateway IP (203.0.113.1).
    pub fn ix_gw(&self) -> Ipv4Addr {
        self.core.ix_gw()
    }

    /// Remove any known lab resources created by this process.
    pub fn cleanup(&self) {
        resources().cleanup_all();
    }

    /// Aggressively remove any resources whose names match the lab prefix.
    ///
    /// This is useful if a previous run crashed before it could clean up.
    pub fn cleanup_everything() {
        resources().cleanup_everything();
    }

    // ── Private helpers ──────────────────────────────────────────────────

    fn ns_name(&mut self) -> String {
        let id = self.ns_counter;
        self.ns_counter = self.ns_counter.saturating_add(1);
        format!("{}-{}", self.prefix, id)
    }

    fn gateway_from_name(&self, name: &str) -> Result<Gateway> {
        if let Some(&id) = self.home_by_name.get(name) {
            return Ok(Gateway::Lan(id));
        }
        if let Some(&id) = self.dc_by_name.get(name) {
            return Ok(Gateway::Dc(id));
        }
        if let Some(&id) = self.isp_by_name.get(name) {
            return Ok(Gateway::Isp(id));
        }
        bail!("unknown gateway '{}'", name)
    }
}

impl Default for Lab {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        resources().cleanup_all();
        for child in self.children.drain(..) {
            match child {
                ChildTask::Process(mut proc) => {
                    let _ = proc.kill();
                    let _ = proc.wait();
                }
                ChildTask::Thread { handle, join } => {
                    handle.stop();
                    let _ = join.join();
                }
            }
        }
        for ns_name in self.core.all_ns_names() {
            cleanup_netns(&ns_name);
        }
    }
}

// ─────────────────────────────────────────────
// TOML config types
// ─────────────────────────────────────────────

mod config {
    use super::{Impair, NatMode};
    use serde::Deserialize;
    use std::collections::HashMap;

    #[derive(Deserialize)]
    /// Parsed lab configuration from TOML.
    pub struct LabConfig {
        /// Optional region-latency map.
        pub region: Option<HashMap<String, RegionConfig>>,
        /// ISP entries.
        #[serde(default)]
        pub isp: Vec<IspConfig>,
        /// DC entries.
        #[serde(default)]
        pub dc: Vec<DcConfig>,
        /// LAN entries (homes).
        #[serde(default)]
        pub lan: Vec<LanConfig>,
        /// Device entries.
        #[serde(default)]
        pub device: Vec<DeviceConfig>,
    }

    #[derive(Deserialize)]
    /// Per-region latency configuration.
    pub struct RegionConfig {
        /// Map of target-region name → one-way latency in ms.
        #[serde(default)]
        pub latencies: HashMap<String, u32>,
    }

    /// `nat = "cgnat"` on an ISP entry.
    #[derive(Deserialize, PartialEq)]
    #[serde(rename_all = "lowercase")]
    /// ISP NAT mode.
    pub enum IspNat {
        Cgnat,
    }

    #[derive(Deserialize)]
    /// ISP configuration entry.
    pub struct IspConfig {
        /// ISP name.
        pub name: String,
        /// ISP region.
        pub region: String,
        /// Set to `"cgnat"` to enable CGNAT on this ISP.
        pub nat: Option<IspNat>,
        /// Optional impairment applied to downstream links.
        pub impair_downstream: Option<ImpairCfg>,
    }

    #[derive(Deserialize)]
    /// Impairment configuration.
    pub struct ImpairCfg {
        /// One-way latency in milliseconds.
        pub latency: u32, // milliseconds added to downstream links
    }

    #[derive(Deserialize)]
    /// Data center configuration entry.
    pub struct DcConfig {
        /// DC name.
        pub name: String,
        /// DC region.
        pub region: String,
    }

    #[derive(Deserialize)]
    /// Home/LAN configuration entry.
    pub struct LanConfig {
        /// LAN name.
        pub name: String,
        /// Name of an `[[isp]]` entry.
        pub isp: String,
        /// `"destination-independent"` or `"destination-dependent"`.
        pub nat: NatMode,
    }

    #[derive(Deserialize)]
    /// Device configuration entry.
    pub struct DeviceConfig {
        /// Device name.
        pub name: String,
        /// Name of a `[[lan]]`, `[[dc]]`, or `[[isp]]` entry.
        pub gateway: String,
        /// Optional link impairment: `"wifi"`, `"mobile"`, or `{ rate, loss, latency }`.
        pub impair: Option<Impair>,
    }
}

// ─────────────────────────────────────────────
// STUN-like reflector + probe
// ─────────────────────────────────────────────

/// Spawn a UDP reflector that echoes "OBSERVED <peer_ip>:<peer_port>" back to
/// each sender.  Pass `ns = Some(name)` to run inside a named netns, or
/// `None` for the root namespace.
fn spawn_reflector_in(
    ns: Option<&str>,
    bind: SocketAddr,
) -> Result<(TaskHandle, thread::JoinHandle<Result<()>>)> {
    let ns = ns.map(|s| s.to_string());
    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let join = match ns {
        Some(ns) => spawn_in_netns_thread(ns, move || {
            let sock = UdpSocket::bind(bind).context("reflector bind")?;
            let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
            let mut buf = [0u8; 512];
            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                match sock.recv_from(&mut buf) {
                    Ok((_, peer)) => {
                        let msg = format!("OBSERVED {}", peer);
                        let _ = sock.send_to(msg.as_bytes(), peer);
                    }
                    Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                        continue;
                    }
                    Err(_) => break,
                }
            }
            Ok(())
        }),
        None => thread::spawn(move || {
            let sock = UdpSocket::bind(bind).context("reflector bind")?;
            let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
            let mut buf = [0u8; 512];
            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                match sock.recv_from(&mut buf) {
                    Ok((_, peer)) => {
                        let msg = format!("OBSERVED {}", peer);
                        let _ = sock.send_to(msg.as_bytes(), peer);
                    }
                    Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                        continue;
                    }
                    Err(_) => break,
                }
            }
            Ok(())
        }),
    };
    Ok((TaskHandle::new(stop_tx), join))
}

/// Send a UDP probe from inside `ns` to `reflector`, parse the "OBSERVED …"
/// reply, and return the observed external address.
pub fn probe_in_ns(
    ns: &str,
    reflector: SocketAddr,
    timeout: Duration,
    bind_port: Option<u16>,
) -> Result<ObservedAddr> {
    let ns_name = ns.to_string();
    let ns_for_log = ns_name.clone();
    with_netns_thread(&ns_name, move || {
        let bind_addr = match bind_port {
            Some(port) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port),
            None => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        };
        let sock = UdpSocket::bind(bind_addr)?;
        sock.set_read_timeout(Some(timeout))?;
        let mut buf = [0u8; 512];
        for attempt in 1..=3 {
            sock.send_to(b"PROBE", reflector)?;
            match sock.recv_from(&mut buf) {
                Ok((n, _)) => {
                    let s = std::str::from_utf8(&buf[..n])?;
                    let addr_str = s
                        .strip_prefix("OBSERVED ")
                        .ok_or_else(|| anyhow!("unexpected reflector reply: {:?}", s))?;
                    return Ok(ObservedAddr {
                        observed: addr_str.parse()?,
                    });
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    debug!(
                        ns = %ns_for_log,
                        attempt,
                        "probe timeout waiting for reflector reply"
                    );
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(anyhow!("probe timed out after 3 attempts"))
    })
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────
//
pub fn udp_roundtrip_in_ns(ns: &str, reflector: SocketAddr) -> Result<ObservedAddr> {
    probe_in_ns(ns, reflector, Duration::from_millis(500), None)
}

pub fn udp_rtt_in_ns(ns: &str, reflector: SocketAddr) -> Result<Duration> {
    with_netns_thread(ns, move || {
        let sock = UdpSocket::bind("0.0.0.0:0")?;
        sock.set_read_timeout(Some(Duration::from_secs(2)))?;
        let mut buf = [0u8; 256];
        let start = Instant::now();
        sock.send_to(b"PING", reflector)?;
        let _ = sock.recv_from(&mut buf)?;
        Ok(start.elapsed())
    })
}

#[cfg(test)]
mod tests {
    use n0_tracing_test::traced_test;
    use serial_test::serial;
    use std::io::{Read, Write};

    use super::*;

    fn require_root() {
        if !nix::unistd::Uid::effective().is_root() {
            panic!("test requires root / CAP_NET_ADMIN — run: sudo -E cargo test -- --nocapture");
        }
    }

    fn ping_in_ns(ns: &str, addr: &str) -> Result<()> {
        let mut cmd = std::process::Command::new("ping");
        cmd.args(["-c", "1", "-W", "1", addr]);
        run_in_netns(ns, cmd).map(|_| ())
    }

    fn spawn_tcp_echo_in(ns: &str, bind: SocketAddr) -> thread::JoinHandle<Result<()>> {
        Lab::run_in_thread(ns, move || {
            let listener = std::net::TcpListener::bind(bind).context("tcp echo bind")?;
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 64];
                let n = stream.read(&mut buf)?;
                stream.write_all(&buf[..n])?;
            }
            Ok(())
        })
    }

    fn tcp_roundtrip_in_ns(ns: &str, target: SocketAddr) -> Result<()> {
        with_netns_thread(ns, move || {
            let timeout = Duration::from_millis(500);
            let mut stream = std::net::TcpStream::connect_timeout(&target, timeout)?;
            stream.set_read_timeout(Some(timeout))?;
            stream.set_write_timeout(Some(timeout))?;
            let payload = b"ping";
            stream.write_all(payload)?;
            let mut buf = [0u8; 4];
            stream.read_exact(&mut buf)?;
            if &buf != payload {
                bail!("tcp echo mismatch: {:?}", buf);
            }
            Ok(())
        })
    }

    // ── Builder-API NAT tests ────────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn nat_dest_independent_keeps_port() -> Result<()> {
        require_root();
        let mut lab = Lab::new();
        let isp = lab.add_isp("isp1", "eu", false, None)?;
        let dc = lab.add_dc("dc1", "eu")?;
        let home = lab.add_home("home1", isp, NatMode::DestinationIndependent)?;
        lab.add_device("dev1", Gateway::Lan(home), None)?;
        lab.build().await?;

        // Reflector in DC namespace.
        let dc_ip = lab.dc_ix_ip(dc)?;
        let r1 = SocketAddr::new(IpAddr::V4(dc_ip), 3478);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, r1)?;

        // Reflector on IX bridge (root ns).
        let r2 = SocketAddr::new(IpAddr::V4(lab.ix_gw()), 3479);
        lab.spawn_reflector_on_ix(r2)?;

        tokio::time::sleep(Duration::from_millis(250)).await;

        let o1 = lab.probe_udp_mapping("dev1", r1)?;
        let o2 = lab.probe_udp_mapping("dev1", r2)?;

        assert_eq!(o1.observed.ip(), o2.observed.ip(), "external IP differs");
        assert_eq!(
            o1.observed.port(),
            o2.observed.port(),
            "EIM: external port must be stable across destinations",
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn nat_dest_dependent_changes_port() -> Result<()> {
        require_root();
        let mut lab = Lab::new();
        let isp = lab.add_isp("isp1", "eu", false, None)?;
        let dc = lab.add_dc("dc1", "eu")?;
        let home = lab.add_home("home1", isp, NatMode::DestinationDependent)?;
        lab.add_device("dev1", Gateway::Lan(home), None)?;
        lab.build().await?;

        let dc_ip = lab.dc_ix_ip(dc)?;
        let r1 = SocketAddr::new(IpAddr::V4(dc_ip), 4478);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, r1)?;

        let r2 = SocketAddr::new(IpAddr::V4(lab.ix_gw()), 4479);
        lab.spawn_reflector_on_ix(r2)?;

        tokio::time::sleep(Duration::from_millis(250)).await;

        let o1 = lab.probe_udp_mapping("dev1", r1)?;
        let o2 = lab.probe_udp_mapping("dev1", r2)?;
        println!("o1 {o1:?}");
        println!("o2 {o1:?}");

        assert_eq!(o1.observed.ip(), o2.observed.ip(), "external IP differs");
        assert_ne!(
            o1.observed.port(),
            o2.observed.port(),
            "EDM: external port must change per destination",
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn cgnat_hides_behind_isp_public_ip() -> Result<()> {
        require_root();
        let mut lab = Lab::new();
        let isp = lab.add_isp("isp1", "eu", true /* cgnat */, None)?;
        let dc = lab.add_dc("dc1", "eu")?;
        let home = lab.add_home("home1", isp, NatMode::DestinationIndependent)?;
        lab.add_device("dev1", Gateway::Lan(home), None)?;
        lab.build().await?;

        let dc_ip = lab.dc_ix_ip(dc)?;
        let r = SocketAddr::new(IpAddr::V4(dc_ip), 5478);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, r)?;

        tokio::time::sleep(Duration::from_millis(250)).await;

        let o = lab.probe_udp_mapping("dev1", r)?;
        let isp_public = lab.isp_public_ip(isp)?;

        assert_eq!(
            o.observed.ip(),
            isp_public,
            "with CGNAT the observed IP must be the ISP's IX IP",
        );
        Ok(())
    }

    // ── Lab::load test ───────────────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    async fn load_from_toml() -> Result<()> {
        require_root();
        // Minimal inline TOML so the test is self-contained.
        let toml = r#"
[[isp]]
name   = "isp1"
region = "eu"

[[dc]]
name   = "dc1"
region = "eu"

[[lan]]
name    = "lan1"
isp     = "isp1"
nat     = "destination-independent"

[[device]]
name    = "dev1"
gateway = "lan1"
"#;
        let tmp = std::env::temp_dir().join("netsim_test_lab.toml");
        std::fs::write(&tmp, toml)?;

        let lab = Lab::load(&tmp).await?;
        assert!(lab.device_id("dev1").is_some());
        Ok(())
    }

    // ── Smoke tests ─────────────────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn smoke_ping_gateway() -> Result<()> {
        require_root();
        let mut lab = Lab::new();
        let isp = lab.add_isp("isp1", "eu", false, None)?;
        let home = lab.add_home("home1", isp, NatMode::DestinationIndependent)?;
        lab.add_device("dev1", Gateway::Lan(home), None)?;
        lab.build().await?;

        let dev_id = lab.device_id("dev1").expect("dev1 exists");
        let dev_ns = lab.node_ns(dev_id)?.to_string();
        let lan_gw = lab.router_downlink_gw(home)?;
        ping_in_ns(&dev_ns, &lan_gw.to_string())?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn smoke_udp_dc_roundtrip() -> Result<()> {
        require_root();
        let mut lab = Lab::new();
        let isp = lab.add_isp("isp1", "eu", false, None)?;
        let dc = lab.add_dc("dc1", "eu")?;
        let home = lab.add_home("home1", isp, NatMode::DestinationIndependent)?;
        lab.add_device("dev1", Gateway::Lan(home), None)?;
        lab.build().await?;

        let dc_ip = lab.dc_ix_ip(dc)?;
        let r = SocketAddr::new(IpAddr::V4(dc_ip), 3478);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, r)?;

        tokio::time::sleep(Duration::from_millis(250)).await;

        let dev_id = lab.device_id("dev1").expect("dev1 exists");
        let dev_ns = lab.node_ns(dev_id)?.to_string();
        let _ = udp_roundtrip_in_ns(&dev_ns, r)?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn smoke_tcp_dc_roundtrip() -> Result<()> {
        require_root();
        let mut lab = Lab::new();
        let isp = lab.add_isp("isp1", "eu", false, None)?;
        let dc = lab.add_dc("dc1", "eu")?;
        let home = lab.add_home("home1", isp, NatMode::DestinationIndependent)?;
        lab.add_device("dev1", Gateway::Lan(home), None)?;
        lab.build().await?;

        let dc_ip = lab.dc_ix_ip(dc)?;
        let bind = SocketAddr::new(IpAddr::V4(dc_ip), 9000);
        let dc_ns = lab.node_ns(dc)?.to_string();
        let join = spawn_tcp_echo_in(&dc_ns, bind);

        tokio::time::sleep(Duration::from_millis(250)).await;

        let dev_id = lab.device_id("dev1").expect("dev1 exists");
        let dev_ns = lab.node_ns(dev_id)?.to_string();
        tcp_roundtrip_in_ns(&dev_ns, bind)?;
        match join.join() {
            Ok(res) => res?,
            Err(_) => bail!("tcp echo thread panicked"),
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn smoke_ping_home_to_isp() -> Result<()> {
        require_root();
        let mut lab = Lab::new();
        let isp = lab.add_isp("isp1", "eu", false, None)?;
        let home = lab.add_home("home1", isp, NatMode::DestinationIndependent)?;
        lab.build().await?;

        let home_ns = lab.node_ns(home)?.to_string();
        let isp_wan_ip = lab.router_downlink_gw(isp)?;
        ping_in_ns(&home_ns, &isp_wan_ip.to_string())?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn smoke_ping_isp_to_ix_and_dc() -> Result<()> {
        require_root();
        let mut lab = Lab::new();
        let isp = lab.add_isp("isp1", "eu", false, None)?;
        let dc = lab.add_dc("dc1", "eu")?;
        lab.build().await?;

        let isp_ns = lab.node_ns(isp)?.to_string();
        ping_in_ns(&isp_ns, &lab.ix_gw().to_string())?;
        let dc_ip = lab.dc_ix_ip(dc)?;
        ping_in_ns(&isp_ns, &dc_ip.to_string())?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn smoke_device_to_device_same_lan() -> Result<()> {
        require_root();
        let mut lab = Lab::new();
        let isp = lab.add_isp("isp1", "eu", false, None)?;
        let home = lab.add_home("home1", isp, NatMode::DestinationIndependent)?;
        let dev1 = lab.add_device("dev1", Gateway::Lan(home), None)?;
        let dev2 = lab.add_device("dev2", Gateway::Lan(home), None)?;
        lab.build().await?;

        let dev1_ns = lab.node_ns(dev1)?.to_string();
        let dev2_ip = lab.device_ip(dev2)?;
        ping_in_ns(&dev1_ns, &dev2_ip.to_string())?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn latency_directional_between_regions() -> Result<()> {
        require_root();
        let mut lab = Lab::new();
        lab.add_region_latency("eu", "us", 30);
        lab.add_region_latency("us", "eu", 70);
        let dc_eu = lab.add_dc("dc-eu", "eu")?;
        let dc_us = lab.add_dc("dc-us", "us")?;
        let dev_eu = lab.add_device("dev-eu", Gateway::Dc(dc_eu), None)?;
        let dev_us = lab.add_device("dev-us", Gateway::Dc(dc_us), None)?;
        lab.build().await?;

        let dc_us_ip = lab.dc_ix_ip(dc_us)?;
        let r_us = SocketAddr::new(IpAddr::V4(dc_us_ip), 9010);
        let dc_us_ns = lab.node_ns(dc_us)?.to_string();
        lab.spawn_reflector(&dc_us_ns, r_us)?;

        let dc_eu_ip = lab.dc_ix_ip(dc_eu)?;
        let r_eu = SocketAddr::new(IpAddr::V4(dc_eu_ip), 9011);
        let dc_eu_ns = lab.node_ns(dc_eu)?.to_string();
        lab.spawn_reflector(&dc_eu_ns, r_eu)?;

        tokio::time::sleep(Duration::from_millis(250)).await;

        let dev_eu_ns = lab.node_ns(dev_eu)?.to_string();
        let dev_us_ns = lab.node_ns(dev_us)?.to_string();
        let rtt_eu_to_us = udp_rtt_in_ns(&dev_eu_ns, r_us)?;
        let rtt_us_to_eu = udp_rtt_in_ns(&dev_us_ns, r_eu)?;
        let expected = Duration::from_millis(100);

        assert!(
            rtt_eu_to_us >= expected - Duration::from_millis(10),
            "expected eu->us RTT >= 90ms, got {rtt_eu_to_us:?}"
        );
        assert!(
            rtt_us_to_eu >= expected - Duration::from_millis(10),
            "expected us->eu RTT >= 90ms, got {rtt_us_to_eu:?}"
        );
        let diff = if rtt_eu_to_us > rtt_us_to_eu {
            rtt_eu_to_us - rtt_us_to_eu
        } else {
            rtt_us_to_eu - rtt_eu_to_us
        };
        assert!(
            diff <= Duration::from_millis(20),
            "expected RTTs to be close; eu->us={rtt_eu_to_us:?} us->eu={rtt_us_to_eu:?}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn latency_inter_region_dc_to_dc() -> Result<()> {
        require_root();
        let mut lab = Lab::new();
        lab.add_region_latency("eu", "us", 50);
        lab.add_region_latency("us", "eu", 50);
        let dc_eu = lab.add_dc("dc-eu", "eu")?;
        let dc_us = lab.add_dc("dc-us", "us")?;
        lab.add_device("dev1", Gateway::Dc(dc_eu), None)?;
        lab.build().await?;

        let dc_us_ip = lab.dc_ix_ip(dc_us)?;
        let r = SocketAddr::new(IpAddr::V4(dc_us_ip), 9000);
        let dc_us_ns = lab.node_ns(dc_us)?.to_string();
        lab.spawn_reflector(&dc_us_ns, r)?;
        tokio::time::sleep(Duration::from_millis(250)).await;

        let dev_id = lab.device_id("dev1").context("missing dev1")?;
        let dev_ns = lab.node_ns(dev_id)?.to_string();
        let rtt = udp_rtt_in_ns(&dev_ns, r)?;
        assert!(
            rtt >= Duration::from_millis(90),
            "expected inter-region RTT >= 90ms, got {rtt:?}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn latency_device_impair_adds_delay() -> Result<()> {
        require_root();

        async fn measure(impair: Option<Impair>) -> Result<Duration> {
            let mut lab = Lab::new();
            lab.add_region_latency("eu", "us", 40);
            lab.add_region_latency("us", "eu", 40);
            let dc_eu = lab.add_dc("dc-eu", "eu")?;
            let dc_us = lab.add_dc("dc-us", "us")?;
            lab.add_device("dev1", Gateway::Dc(dc_eu), impair)?;
            lab.build().await?;

            let dc_us_ip = lab.dc_ix_ip(dc_us)?;
            let r = SocketAddr::new(IpAddr::V4(dc_us_ip), 9001);
            let dc_us_ns = lab.node_ns(dc_us)?.to_string();
            lab.spawn_reflector(&dc_us_ns, r)?;
            tokio::time::sleep(Duration::from_millis(250)).await;

            let dev_id = lab.device_id("dev1").context("missing dev1")?;
            let dev_ns = lab.node_ns(dev_id)?.to_string();
            udp_rtt_in_ns(&dev_ns, r)
        }

        let base = measure(None).await?;
        let impaired = measure(Some(Impair::Mobile)).await?;
        assert!(
            impaired >= base + Duration::from_millis(30),
            "expected impaired RTT >= base + 30ms, base={base:?} impaired={impaired:?}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn latency_manual_impair_applies() -> Result<()> {
        require_root();
        let mut lab = Lab::new();
        let dc_eu = lab.add_dc("dc-eu", "eu")?;
        let dc_us = lab.add_dc("dc-us", "us")?;
        lab.add_region_latency("eu", "us", 20);
        lab.add_region_latency("us", "eu", 20);
        let dev = lab.add_device(
            "dev1",
            Gateway::Dc(dc_eu),
            Some(Impair::Manual {
                rate: 10_000,
                loss: 0.0,
                latency: 60,
            }),
        )?;
        lab.build().await?;

        let dc_us_ip = lab.dc_ix_ip(dc_us)?;
        let r = SocketAddr::new(IpAddr::V4(dc_us_ip), 9020);
        let dc_us_ns = lab.node_ns(dc_us)?.to_string();
        lab.spawn_reflector(&dc_us_ns, r)?;
        tokio::time::sleep(Duration::from_millis(250)).await;

        let dev_ns = lab.node_ns(dev)?.to_string();
        let rtt = udp_rtt_in_ns(&dev_ns, r)?;
        assert!(
            rtt >= Duration::from_millis(90),
            "expected manual latency >= 90ms RTT, got {rtt:?}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn isp_home_wan_pool_selection() -> Result<()> {
        require_root();
        let mut lab = Lab::new();
        let isp_public = lab.add_isp("isp-public", "eu", false, None)?;
        let isp_cgnat = lab.add_isp("isp-cgnat", "eu", true, None)?;
        let home_public =
            lab.add_home("home-public", isp_public, NatMode::DestinationIndependent)?;
        let home_cgnat = lab.add_home("home-cgnat", isp_cgnat, NatMode::DestinationIndependent)?;
        lab.build().await?;

        let wan_public = lab.router_uplink_ip(home_public)?;
        let wan_cgnat = lab.router_uplink_ip(home_cgnat)?;

        let is_private_10 = |ip: Ipv4Addr| ip.octets()[0] == 10;
        assert!(
            !is_private_10(wan_public),
            "expected public WAN for non-CGNAT home, got {wan_public}"
        );
        assert!(
            is_private_10(wan_cgnat),
            "expected private WAN for CGNAT home, got {wan_cgnat}"
        );
        Ok(())
    }

    #[test]
    fn manual_impair_deserialize() -> Result<()> {
        let cfg = r#"
[[dc]]
name = "dc1"
region = "eu"

[[device]]
name = "dev1"
gateway = "dc1"
impair = { rate = 5000, loss = 1.5, latency = 40 }
"#;
        let parsed: config::LabConfig = toml::from_str(cfg)?;
        let dev = parsed.device.first().context("missing device")?;
        match dev.impair {
            Some(Impair::Manual {
                rate,
                loss,
                latency,
            }) => {
                assert_eq!(rate, 5000);
                assert!((loss - 1.5).abs() < f32::EPSILON);
                assert_eq!(latency, 40);
            }
            other => bail!("unexpected impair: {:?}", other),
        }
        Ok(())
    }
}
