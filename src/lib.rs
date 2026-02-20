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
//! # use netsim::{Lab, NatMode};
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> anyhow::Result<()> {
//! let mut lab = Lab::new();
//! let isp  = lab.add_router("isp1",  Some("eu"), None,      NatMode::Cgnat)?;
//! let home = lab.add_router("home1", None,        Some(isp), NatMode::DestinationIndependent)?;
//! lab.add_device("dev1").iface("eth0", home, None).build()?;
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

/// Verify the process has enough privileges to manage namespaces, routes, and raw sockets.
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

// ─────────────────────────────────────────────
// Public types
// ─────────────────────────────────────────────

/// NAT mode for a router.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NatMode {
    /// No NAT — downstream addresses are publicly routable (DC behaviour).
    #[default]
    None,
    /// CGNAT — SNAT subscriber traffic on the IX-facing interface.
    Cgnat,
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
pub struct Lab {
    /// Short process-unique prefix used on root-namespace interface names.
    prefix: String,
    bridge_tag: String,
    bridge_counter: u32,
    ns_counter: u32,
    router_by_name: HashMap<String, NodeId>,
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
        let root_ns = format!("{prefix}-root");
        let bridge_tag = format!("p{}", pid_tag);
        let ix_gw = Ipv4Addr::new(203, 0, 113, 1);
        resources().register_prefix(&prefix);
        let core = LabCore::new(CoreConfig {
            prefix: prefix.clone(),
            root_ns,
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
            router_by_name: HashMap::new(),
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

    /// Initialize tracing for this crate (idempotent).
    ///
    /// Honors `RUST_LOG`; defaults to `netsim=debug` if unset.
    pub fn init_tracing() {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("netsim=debug"));
        let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
    }

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

        // Routers: multi-pass until all upstream references resolve.
        let mut remaining: Vec<&config::RouterCfg> = cfg.router.iter().collect();
        let mut changed = true;
        while changed && !remaining.is_empty() {
            changed = false;
            let mut next = Vec::new();
            for rcfg in remaining {
                let upstream = match &rcfg.upstream {
                    None => None,
                    Some(parent_name) => match lab.router_by_name.get(parent_name).copied() {
                        Some(id) => Some(id),
                        None => {
                            next.push(rcfg);
                            continue;
                        }
                    },
                };
                lab.add_router(&rcfg.name, rcfg.region.as_deref(), upstream, rcfg.nat)?;
                changed = true;
            }
            remaining = next;
        }
        if !remaining.is_empty() {
            let names: Vec<_> = remaining.iter().map(|r| r.name.as_str()).collect();
            bail!("unresolvable router upstreams: {}", names.join(", "));
        }

        // Devices — pre-resolve router IDs before taking the mutable borrow via add_device.
        for dev_cfg in &cfg.device {
            let router_id = lab
                .router_by_name
                .get(&dev_cfg.router)
                .copied()
                .ok_or_else(|| {
                    anyhow!(
                        "device '{}' references unknown router '{}'",
                        dev_cfg.name,
                        dev_cfg.router
                    )
                })?;
            lab.add_device(&dev_cfg.name)
                .iface("eth0", router_id, dev_cfg.impair)
                .build()?;
        }

        Ok(lab)
    }

    // ── Builder methods (sync — just populate data structures) ──────────

    /// Add a router to the lab.
    ///
    /// - `region`: optional region tag used for inter-region latency rules.
    /// - `upstream`: if `None`, the router attaches directly to the IX switch;
    ///   if `Some(parent)`, it attaches to `parent`'s downstream switch as a
    ///   subscriber router (home/CPE style).
    /// - `nat`: NAT mode.  Use [`NatMode::None`] for public DC-style routers,
    ///   [`NatMode::Cgnat`] for ISP-style CGNAT, or
    ///   [`NatMode::DestinationIndependent`] / [`NatMode::DestinationDependent`]
    ///   for home NAT behind an upstream router.
    pub fn add_router(
        &mut self,
        name: &str,
        region: Option<&str>,
        upstream: Option<NodeId>,
        nat: NatMode,
    ) -> Result<NodeId> {
        if self.router_by_name.contains_key(name) {
            bail!("router '{}' already exists", name);
        }
        let ns = self.ns_name();
        let downlink_bridge = self.next_bridge_name();

        let (cgnat, downstream_pool) = match nat {
            NatMode::None => (false, DownstreamPool::Public),
            NatMode::Cgnat => (true, DownstreamPool::Private),
            NatMode::DestinationIndependent | NatMode::DestinationDependent => {
                (false, DownstreamPool::Private)
            }
        };
        let nat_cfg = match nat {
            NatMode::DestinationIndependent | NatMode::DestinationDependent => Some(nat),
            _ => None,
        };
        let cfg = RouterConfig {
            nat: nat_cfg,
            cgnat,
            downlink_bridge,
            downstream_pool,
        };

        let id = self
            .core
            .add_router(name, ns, cfg, region.map(|s| s.to_string()));
        let sub_switch = self.core.add_switch(&format!("{name}-sub"), None, None);
        let _ = self.core.connect_router_downlink(id, sub_switch)?;

        match upstream {
            None => {
                let ix_ip = self.core.alloc_ix_ip_low();
                let _ = self
                    .core
                    .connect_router_uplink(id, self.core.ix_sw(), Some(ix_ip))?;
            }
            Some(parent_id) => {
                let parent_downlink = self
                    .core
                    .router(parent_id)
                    .and_then(|r| r.downlink)
                    .ok_or_else(|| anyhow!("parent router missing downlink switch"))?;
                let _ = self.core.connect_router_uplink(id, parent_downlink, None)?;
            }
        }

        self.router_by_name.insert(name.to_string(), id);
        Ok(id)
    }

    /// Begin building a device; returns a [`DeviceBuilder`] to configure interfaces.
    ///
    /// Call [`.iface()`][DeviceBuilder::iface] one or more times to attach network
    /// interfaces, then [`.build()`][DeviceBuilder::build] to finalize.
    pub fn add_device(&mut self, name: &str) -> DeviceBuilder<'_> {
        if self.device_by_name.contains_key(name) {
            return DeviceBuilder {
                lab: self,
                id: NodeId(u64::MAX),
                result: Err(anyhow!("device '{}' already exists", name)),
            };
        }
        let ns = self.ns_name();
        let id = self.core.add_device(name, ns);
        self.device_by_name.insert(name.to_string(), id);
        DeviceBuilder {
            lab: self,
            id,
            result: Ok(()),
        }
    }

    // ── build ────────────────────────────────────────────────────────────

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
    pub fn run_in<F, R>(ns_name: &str, f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        with_netns_thread(ns_name, f)
    }

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

    /// Spawn a UDP reflector in a named device/router namespace.
    pub fn spawn_reflector(&mut self, ns_name: &str, bind: SocketAddr) -> Result<TaskHandle> {
        let (handle, join) = spawn_reflector_in(ns_name, bind)?;
        self.children.push(ChildTask::Thread {
            handle: handle.clone(),
            join,
        });
        Ok(handle)
    }

    /// Spawn a UDP reflector in the lab root namespace (IX bridge side).
    pub fn spawn_reflector_on_ix(&mut self, bind: SocketAddr) -> Result<TaskHandle> {
        let (handle, join) = spawn_reflector_in(self.core.root_ns(), bind)?;
        self.children.push(ChildTask::Thread {
            handle: handle.clone(),
            join,
        });
        Ok(handle)
    }

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

    /// Return the assigned IP of a device's default interface.
    pub fn device_ip(&self, id: NodeId) -> Result<Ipv4Addr> {
        self.core
            .device(id)
            .map(|dev| dev.default_iface().ip)
            .ok_or_else(|| anyhow!("unknown device id"))?
            .ok_or_else(|| anyhow!("device default interface missing ip"))
    }

    /// Resolve a router name to its [`NodeId`].
    pub fn router_id(&self, name: &str) -> Option<NodeId> {
        self.router_by_name.get(name).copied()
    }

    /// Resolve a device name to its [`NodeId`].
    pub fn device_id(&self, name: &str) -> Option<NodeId> {
        self.device_by_name.get(name).copied()
    }

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
// DeviceBuilder
// ─────────────────────────────────────────────

/// Builder for a device node; returned by [`Lab::add_device`].
///
/// Chain [`.iface()`][DeviceBuilder::iface] calls to attach one or more
/// network interfaces, then call [`.build()`][DeviceBuilder::build] to
/// finalize the device and obtain its [`NodeId`].
pub struct DeviceBuilder<'lab> {
    lab: &'lab mut Lab,
    id: NodeId,
    result: Result<()>,
}

impl<'lab> DeviceBuilder<'lab> {
    /// Attach `ifname` inside the device namespace to `router`'s downstream switch.
    ///
    /// The first interface added becomes the default-route interface unless
    /// overridden by [`default_via`][DeviceBuilder::default_via].
    pub fn iface(mut self, ifname: &str, router: NodeId, impair: Option<Impair>) -> Self {
        if self.result.is_ok() {
            self.result = self
                .lab
                .core
                .add_device_iface(self.id, ifname, router, impair)
                .map(|_| ());
        }
        self
    }

    /// Override which interface carries the default route.
    ///
    /// By default this is the first interface added via [`iface`][DeviceBuilder::iface].
    pub fn default_via(mut self, ifname: &str) -> Self {
        if self.result.is_ok() {
            self.result = self.lab.core.set_device_default_via(self.id, ifname);
        }
        self
    }

    /// Finalize the device and return its [`NodeId`].
    pub fn build(self) -> Result<NodeId> {
        self.result?;
        Ok(self.id)
    }
}

// ─────────────────────────────────────────────
// TOML config types
// ─────────────────────────────────────────────

mod config {
    use super::{Impair, NatMode};
    use serde::Deserialize;
    use std::collections::HashMap;

    /// Parsed lab configuration from TOML.
    #[derive(Deserialize)]
    pub struct LabConfig {
        /// Optional region-latency map.
        pub region: Option<HashMap<String, RegionConfig>>,
        /// Router entries.
        #[serde(default)]
        pub router: Vec<RouterCfg>,
        /// Device entries.
        #[serde(default)]
        pub device: Vec<DeviceCfg>,
    }

    /// Per-region latency configuration.
    #[derive(Deserialize)]
    pub struct RegionConfig {
        /// Map of target-region name → one-way latency in ms.
        #[serde(default)]
        pub latencies: HashMap<String, u32>,
    }

    /// Router configuration entry.
    #[derive(Deserialize)]
    pub struct RouterCfg {
        /// Router name.
        pub name: String,
        /// Optional region tag (used for inter-region latency rules).
        pub region: Option<String>,
        /// Name of the upstream router.  If absent the router attaches to the IX switch.
        pub upstream: Option<String>,
        /// NAT mode.  Defaults to `"none"` (public downstream, no NAT).
        #[serde(default)]
        pub nat: NatMode,
    }

    /// Device configuration entry.
    #[derive(Deserialize)]
    pub struct DeviceCfg {
        /// Device name.
        pub name: String,
        /// Name of the router to connect to (via `eth0`).
        pub router: String,
        /// Optional link impairment: `"wifi"`, `"mobile"`, or `{ rate, loss, latency }`.
        pub impair: Option<Impair>,
    }
}

// ─────────────────────────────────────────────
// STUN-like reflector + probe
// ─────────────────────────────────────────────

/// Spawn a UDP reflector that echoes "OBSERVED <peer_ip>:<peer_port>" back to
/// each sender inside the named netns.
fn spawn_reflector_in(
    ns: &str,
    bind: SocketAddr,
) -> Result<(TaskHandle, thread::JoinHandle<Result<()>>)> {
    let ns = ns.to_string();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let join = spawn_in_netns_thread(ns, move || {
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
    });
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
        check_caps()?;
        let mut lab = Lab::new();
        let isp = lab.add_router("isp1", Some("eu"), None, NatMode::None)?;
        let dc = lab.add_router("dc1", Some("eu"), None, NatMode::None)?;
        let home = lab.add_router("home1", None, Some(isp), NatMode::DestinationIndependent)?;
        lab.add_device("dev1").iface("eth0", home, None).build()?;
        lab.build().await?;

        // Reflector in DC namespace.
        let dc_ip = lab.router_uplink_ip(dc)?;
        let r1 = SocketAddr::new(IpAddr::V4(dc_ip), 3478);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, r1)?;

        // Reflector on IX bridge (lab-root ns).
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
        check_caps()?;
        let mut lab = Lab::new();
        let isp = lab.add_router("isp1", Some("eu"), None, NatMode::None)?;
        let dc = lab.add_router("dc1", Some("eu"), None, NatMode::None)?;
        let home = lab.add_router("home1", None, Some(isp), NatMode::DestinationDependent)?;
        lab.add_device("dev1").iface("eth0", home, None).build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r1 = SocketAddr::new(IpAddr::V4(dc_ip), 4478);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, r1)?;

        let r2 = SocketAddr::new(IpAddr::V4(lab.ix_gw()), 4479);
        lab.spawn_reflector_on_ix(r2)?;

        tokio::time::sleep(Duration::from_millis(250)).await;

        let o1 = lab.probe_udp_mapping("dev1", r1)?;
        let o2 = lab.probe_udp_mapping("dev1", r2)?;
        println!("o1 {o1:?}");
        println!("o2 {o2:?}");

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
        check_caps()?;
        let mut lab = Lab::new();
        let isp = lab.add_router("isp1", Some("eu"), None, NatMode::Cgnat)?;
        let dc = lab.add_router("dc1", Some("eu"), None, NatMode::None)?;
        let home = lab.add_router("home1", None, Some(isp), NatMode::DestinationIndependent)?;
        lab.add_device("dev1").iface("eth0", home, None).build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r = SocketAddr::new(IpAddr::V4(dc_ip), 5478);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, r)?;

        tokio::time::sleep(Duration::from_millis(250)).await;

        let o = lab.probe_udp_mapping("dev1", r)?;
        let isp_public = IpAddr::V4(lab.router_uplink_ip(isp)?);

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
        check_caps()?;
        // Minimal inline TOML so the test is self-contained.
        let toml = r#"
[[router]]
name   = "isp1"
region = "eu"

[[router]]
name   = "dc1"
region = "eu"

[[router]]
name     = "lan1"
upstream = "isp1"
nat      = "destination-independent"

[[device]]
name   = "dev1"
router = "lan1"
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
        check_caps()?;
        let mut lab = Lab::new();
        let isp = lab.add_router("isp1", Some("eu"), None, NatMode::None)?;
        let home = lab.add_router("home1", None, Some(isp), NatMode::DestinationIndependent)?;
        lab.add_device("dev1").iface("eth0", home, None).build()?;
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
        check_caps()?;
        let mut lab = Lab::new();
        let isp = lab.add_router("isp1", Some("eu"), None, NatMode::None)?;
        let dc = lab.add_router("dc1", Some("eu"), None, NatMode::None)?;
        let home = lab.add_router("home1", None, Some(isp), NatMode::DestinationIndependent)?;
        lab.add_device("dev1").iface("eth0", home, None).build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
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
        check_caps()?;
        let mut lab = Lab::new();
        let isp = lab.add_router("isp1", Some("eu"), None, NatMode::None)?;
        let dc = lab.add_router("dc1", Some("eu"), None, NatMode::None)?;
        let home = lab.add_router("home1", None, Some(isp), NatMode::DestinationIndependent)?;
        lab.add_device("dev1").iface("eth0", home, None).build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
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
        check_caps()?;
        let mut lab = Lab::new();
        let isp = lab.add_router("isp1", Some("eu"), None, NatMode::None)?;
        let home = lab.add_router("home1", None, Some(isp), NatMode::DestinationIndependent)?;
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
        check_caps()?;
        let mut lab = Lab::new();
        let isp = lab.add_router("isp1", Some("eu"), None, NatMode::None)?;
        let dc = lab.add_router("dc1", Some("eu"), None, NatMode::None)?;
        lab.build().await?;

        let isp_ns = lab.node_ns(isp)?.to_string();
        ping_in_ns(&isp_ns, &lab.ix_gw().to_string())?;
        let dc_ip = lab.router_uplink_ip(dc)?;
        ping_in_ns(&isp_ns, &dc_ip.to_string())?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn smoke_device_to_device_same_lan() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        let isp = lab.add_router("isp1", Some("eu"), None, NatMode::None)?;
        let home = lab.add_router("home1", None, Some(isp), NatMode::DestinationIndependent)?;
        let dev1 = lab.add_device("dev1").iface("eth0", home, None).build()?;
        let dev2 = lab.add_device("dev2").iface("eth0", home, None).build()?;
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
        check_caps()?;
        let mut lab = Lab::new();
        lab.add_region_latency("eu", "us", 30);
        lab.add_region_latency("us", "eu", 70);
        let dc_eu = lab.add_router("dc-eu", Some("eu"), None, NatMode::None)?;
        let dc_us = lab.add_router("dc-us", Some("us"), None, NatMode::None)?;
        let dev_eu = lab.add_device("dev-eu").iface("eth0", dc_eu, None).build()?;
        let dev_us = lab.add_device("dev-us").iface("eth0", dc_us, None).build()?;
        lab.build().await?;

        let dc_us_ip = lab.router_uplink_ip(dc_us)?;
        let r_us = SocketAddr::new(IpAddr::V4(dc_us_ip), 9010);
        let dc_us_ns = lab.node_ns(dc_us)?.to_string();
        lab.spawn_reflector(&dc_us_ns, r_us)?;

        let dc_eu_ip = lab.router_uplink_ip(dc_eu)?;
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
        let diff = rtt_eu_to_us.abs_diff(rtt_us_to_eu);
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
        check_caps()?;
        let mut lab = Lab::new();
        lab.add_region_latency("eu", "us", 50);
        lab.add_region_latency("us", "eu", 50);
        let dc_eu = lab.add_router("dc-eu", Some("eu"), None, NatMode::None)?;
        let dc_us = lab.add_router("dc-us", Some("us"), None, NatMode::None)?;
        lab.add_device("dev1").iface("eth0", dc_eu, None).build()?;
        lab.build().await?;

        let dc_us_ip = lab.router_uplink_ip(dc_us)?;
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
        check_caps()?;

        async fn measure(impair: Option<Impair>) -> Result<Duration> {
            let mut lab = Lab::new();
            lab.add_region_latency("eu", "us", 40);
            lab.add_region_latency("us", "eu", 40);
            let dc_eu = lab.add_router("dc-eu", Some("eu"), None, NatMode::None)?;
            let dc_us = lab.add_router("dc-us", Some("us"), None, NatMode::None)?;
            lab.add_device("dev1").iface("eth0", dc_eu, impair).build()?;
            lab.build().await?;

            let dc_us_ip = lab.router_uplink_ip(dc_us)?;
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
        check_caps()?;
        let mut lab = Lab::new();
        let dc_eu = lab.add_router("dc-eu", Some("eu"), None, NatMode::None)?;
        let dc_us = lab.add_router("dc-us", Some("us"), None, NatMode::None)?;
        lab.add_region_latency("eu", "us", 20);
        lab.add_region_latency("us", "eu", 20);
        let dev = lab
            .add_device("dev1")
            .iface(
                "eth0",
                dc_eu,
                Some(Impair::Manual {
                    rate: 10_000,
                    loss: 0.0,
                    latency: 60,
                }),
            )
            .build()?;
        lab.build().await?;

        let dc_us_ip = lab.router_uplink_ip(dc_us)?;
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
        check_caps()?;
        let mut lab = Lab::new();
        let isp_public = lab.add_router("isp-public", Some("eu"), None, NatMode::None)?;
        let isp_cgnat = lab.add_router("isp-cgnat", Some("eu"), None, NatMode::Cgnat)?;
        let home_public = lab.add_router(
            "home-public",
            None,
            Some(isp_public),
            NatMode::DestinationIndependent,
        )?;
        let home_cgnat = lab.add_router(
            "home-cgnat",
            None,
            Some(isp_cgnat),
            NatMode::DestinationIndependent,
        )?;
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
[[router]]
name = "dc1"
region = "eu"

[[device]]
name = "dev1"
router = "dc1"
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
