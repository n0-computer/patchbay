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
//! **Important**: namespace transitions are executed inside dedicated worker
//! threads in the netns manager; callers can use any Tokio runtime flavor.

use anyhow::{anyhow, bail, Context, Result};
use nix::unistd::Pid;
use serde::Deserialize;
use std::{
    collections::HashMap,
    io::ErrorKind,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    path::Path,
    process::{Command, ExitStatus},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};
use tracing::debug;

/// Shared binary/source path parsing and target shortcut resolution helpers.
pub mod assets;
/// Shared URL-binary cache helpers.
pub mod binary_cache;
/// Exposes low-level topology and namespace construction primitives.
pub mod core;
mod netlink;
mod netns;
mod qdisc;
/// Embedded UI HTTP serving helpers.
pub mod serve;
mod userns;
/// Shared string sanitizers.
pub mod util;
use crate::core::{
    apply_impair_in, cleanup_netns, resources, run_closure_in_namespace, run_command_in_namespace,
    spawn_closure_in_namespace_thread, spawn_command_in_namespace, CoreConfig, DownstreamPool,
    LabCore, RouterConfig, TaskHandle,
};

static LAB_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Stable identifier for devices/routers/switches in the lab.
pub use crate::core::NodeId;

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

/// Bootstraps an unprivileged user namespace and maps current UID/GID to root.
///
/// Call this once before spawning threads or starting Tokio when running as a
/// non-root user. The function is a no-op when already running as UID 0.
#[cfg(target_os = "linux")]
pub fn bootstrap_userns() -> Result<()> {
    use nix::sched::{unshare, CloneFlags};

    if nix::unistd::Uid::effective().is_root() {
        return Ok(());
    }

    let uid = nix::unistd::Uid::current().as_raw();
    let gid = nix::unistd::Gid::current().as_raw();

    unshare(CloneFlags::CLONE_NEWUSER)
        .context("unshare(CLONE_NEWUSER) failed; ensure user namespaces are enabled and no threads are running yet")?;

    std::fs::write("/proc/self/setgroups", "deny\n").context("write /proc/self/setgroups")?;
    std::fs::write("/proc/self/uid_map", format!("0 {uid} 1\n"))
        .context("write /proc/self/uid_map")?;
    std::fs::write("/proc/self/gid_map", format!("0 {gid} 1\n"))
        .context("write /proc/self/gid_map")?;

    if nix::unistd::Uid::effective().is_root() {
        Ok(())
    } else {
        bail!("userns bootstrap finished without UID 0 mapping")
    }
}

/// Bootstraps user namespaces on Linux; no-op on other platforms.
#[cfg(not(target_os = "linux"))]
pub fn bootstrap_userns() -> Result<()> {
    Ok(())
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

    /// Creates a new lab with default address ranges and IX settings.
    pub fn new() -> Self {
        let pid = std::process::id();
        let pid_tag = pid % 9999 + 1;
        let lab_seq = LAB_COUNTER.fetch_add(1, Ordering::Relaxed);
        let uniq = format!("{lab_seq:x}");
        let prefix = format!("lab-p{}{}", pid_tag, uniq); // e.g. "lab-p12340"
        let root_ns = format!("{prefix}-root");
        let bridge_tag = format!("p{}{}", pid_tag, uniq);
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

    /// Initializes tracing for this crate (idempotent).
    ///
    /// Honors `RUST_LOG`; defaults to `netsim=debug` if unset.
    pub fn init_tracing() {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("netsim=info"));
        let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
    }

    /// Returns the unique resource prefix associated with this lab instance.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Returns the dedicated lab root namespace name.
    pub fn root_namespace_name(&self) -> &str {
        self.core.root_ns()
    }

    /// Parses `lab.toml`, builds the network, and returns a ready-to-use lab.
    pub async fn load(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path).context("read lab config")?;
        let cfg: config::LabConfig = toml::from_str(&text).context("parse lab config")?;
        let mut lab = Self::from_config(cfg)?;
        lab.build().await?;
        Ok(lab)
    }

    /// Builds a `Lab` from a parsed config without building the network yet.
    pub fn from_config(cfg: config::LabConfig) -> Result<Self> {
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

        // Devices — parse raw TOML, pre-resolve router IDs, then build.
        struct ParsedDev {
            name: String,
            default_via: Option<String>,
            ifaces: Vec<(String, NodeId, Option<Impair>)>,
        }

        let dev_data: Vec<ParsedDev> = {
            let mut dev_names: Vec<&String> = cfg.device.keys().collect();
            dev_names.sort();
            let mut result = Vec::new();
            for dev_name in dev_names {
                let dev_val = &cfg.device[dev_name];
                let dev_table = dev_val
                    .as_table()
                    .ok_or_else(|| anyhow!("device '{}' must be a TOML table", dev_name))?;
                let default_via = dev_table
                    .get("default_via")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let count = match dev_table.get("count") {
                    None => 1usize,
                    Some(v) => {
                        let n = v.as_integer().ok_or_else(|| {
                            anyhow!("device '{}' count must be an integer", dev_name)
                        })?;
                        if n < 1 {
                            bail!("device '{}' count must be >= 1", dev_name);
                        }
                        usize::try_from(n)
                            .map_err(|_| anyhow!("device '{}' count out of range", dev_name))?
                    }
                };
                // Interface sub-tables: table-valued keys, excluding scalar device-level keys.
                let mut iface_keys: Vec<&String> = dev_table
                    .keys()
                    .filter(|k| dev_table[*k].is_table())
                    .collect();
                iface_keys.sort();
                if iface_keys.is_empty() {
                    bail!("device '{}' has no interface sub-tables", dev_name);
                }
                let mut ifaces = Vec::new();
                for ifname in iface_keys {
                    let iface_table = dev_table[ifname].as_table().ok_or_else(|| {
                        anyhow!("device '{}' iface '{}' must be a table", dev_name, ifname)
                    })?;
                    let gw_name = iface_table
                        .get("gateway")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            anyhow!("device '{}' iface '{}' missing 'gateway'", dev_name, ifname)
                        })?;
                    let router_id = lab.router_by_name.get(gw_name).copied().ok_or_else(|| {
                        anyhow!(
                            "device '{}' iface '{}' references unknown router '{}'",
                            dev_name,
                            ifname,
                            gw_name
                        )
                    })?;
                    let impair: Option<Impair> = match iface_table.get("impair") {
                        None => None,
                        Some(v) => Some(v.clone().try_into().map_err(|e: toml::de::Error| {
                            anyhow!(
                                "device '{}' iface '{}' invalid impair: {}",
                                dev_name,
                                ifname,
                                e
                            )
                        })?),
                    };
                    ifaces.push((ifname.clone(), router_id, impair));
                }
                if dev_table.contains_key("count") {
                    for idx in 0..count {
                        result.push(ParsedDev {
                            name: format!("{dev_name}-{idx}"),
                            default_via: default_via.clone(),
                            ifaces: ifaces.clone(),
                        });
                    }
                } else {
                    result.push(ParsedDev {
                        name: dev_name.clone(),
                        default_via,
                        ifaces,
                    });
                }
            }
            result
        };
        for dev in dev_data {
            let mut builder = lab.add_device(&dev.name);
            for (ifname, router_id, impair) in dev.ifaces {
                builder = builder.iface(&ifname, router_id, impair);
            }
            if let Some(via) = dev.default_via {
                builder = builder.default_via(&via);
            }
            builder.build()?;
        }

        Ok(lab)
    }

    // ── Builder methods (sync — just populate data structures) ──────────

    /// Adds a router to the lab.
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

        let downstream_pool = match nat {
            NatMode::None => DownstreamPool::Public,
            NatMode::Cgnat => DownstreamPool::Private,
            NatMode::DestinationIndependent | NatMode::DestinationDependent => {
                DownstreamPool::Private
            }
        };
        let cfg = RouterConfig {
            nat,
            downlink_bridge,
            downstream_pool,
        };

        let id = self
            .core
            .add_router(name, ns, cfg, region.map(|s| s.to_string()));
        let sub_switch = self.core.add_switch(&format!("{name}-sub"), None, None);
        self.core.connect_router_downlink(id, sub_switch)?;

        match upstream {
            None => {
                let ix_ip = self.core.alloc_ix_ip_low();
                self.core
                    .connect_router_uplink(id, self.core.ix_sw(), Some(ix_ip))?;
            }
            Some(parent_id) => {
                let parent_downlink = self
                    .core
                    .router(parent_id)
                    .and_then(|r| r.downlink)
                    .ok_or_else(|| anyhow!("parent router missing downlink switch"))?;
                self.core.connect_router_uplink(id, parent_downlink, None)?;
            }
        }

        self.router_by_name.insert(name.to_string(), id);
        Ok(id)
    }

    /// Begins building a device; returns a [`DeviceBuilder`] to configure interfaces.
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

    /// Creates all namespaces, links, addresses, routes, and NAT rules.
    pub async fn build(&mut self) -> Result<()> {
        self.core.build(&self.region_latencies).await
    }

    // ── User-facing API ─────────────────────────────────────────────────

    /// Adds a one-way inter-region latency in milliseconds.
    pub fn add_region_latency(&mut self, from: &str, to: &str, latency_ms: u32) {
        self.region_latencies
            .push((from.to_string(), to.to_string(), latency_ms));
    }

    /// Runs a command inside a device namespace and waits for exit.
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
    pub fn run_on(&self, name: &str, cmd: Command) -> Result<ExitStatus> {
        let id = self.resolve_device(name)?;
        let ns = self.core.device_ns(id)?;
        run_command_in_namespace(ns, cmd)
    }

    /// Runs a closure inside a named network namespace.
    pub fn run_in_namespace<F, R>(ns_name: &str, f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        run_closure_in_namespace(ns_name, f)
    }

    /// Spawns a thread-backed task that runs `f` in `ns_name`.
    pub fn run_in_namespace_thread<F, R>(ns_name: &str, f: F) -> thread::JoinHandle<Result<R>>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        spawn_closure_in_namespace_thread(ns_name.to_string(), f)
    }

    /// Spawns a long-running process inside a device namespace and returns its PID.
    pub fn spawn_on(&mut self, name: &str, cmd: Command) -> Result<Pid> {
        let id = self.resolve_device(name)?;
        let ns = self.core.device_ns(id)?.to_string();
        let child = spawn_command_in_namespace(&ns, cmd)?;
        let pid = Pid::from_raw(child.id() as i32);
        self.children.push(ChildTask::Process(child));
        Ok(pid)
    }

    /// Spawns an unmanaged command in a device namespace and returns the raw `Child`.
    pub fn spawn_unmanaged_on(&self, device: &str, cmd: Command) -> Result<std::process::Child> {
        let id = self.resolve_device(device)?;
        let ns = self.core.device_ns(id)?.to_string();
        spawn_command_in_namespace(&ns, cmd)
    }

    /// Returns the network namespace name for a device by name.
    pub fn device_ns_name(&self, device: &str) -> Result<String> {
        let id = self.resolve_device(device)?;
        Ok(self.core.device_ns(id)?.to_string())
    }

    /// Returns the network namespace name for a router by name.
    pub fn router_ns_name(&self, router: &str) -> Result<String> {
        let id = self.resolve_router(router)?;
        Ok(self.core.router_ns(id)?.to_string())
    }

    /// Builds a map of `NETSIM_*` environment variables from the current lab state.
    ///
    /// Keys use device/interface names normalised to uppercase with `-` → `_`.
    pub fn env_vars(&self) -> std::collections::HashMap<String, String> {
        let mut map = std::collections::HashMap::new();
        for (name, &id) in &self.device_by_name {
            let norm = normalize_env_name(name);
            if let Some(dev) = self.core.device(id) {
                // Default-via IP
                if let Some(ip) = dev.default_iface().ip {
                    map.insert(format!("NETSIM_IP_{}", norm), ip.to_string());
                }
                // Per-interface IPs
                for iface in &dev.interfaces {
                    if let Some(ip) = iface.ip {
                        let ifnorm = normalize_env_name(&iface.ifname);
                        map.insert(format!("NETSIM_IP_{}_{}", norm, ifnorm), ip.to_string());
                    }
                }
                // Namespace name
                map.insert(format!("NETSIM_NS_{}", norm), dev.ns.clone());
            }
        }
        map
    }

    // ── Reflector / probe helpers (mainly for tests) ─────────────────────

    /// Spawns a UDP reflector in a named device/router namespace.
    pub fn spawn_reflector(&mut self, ns_name: &str, bind: SocketAddr) -> Result<TaskHandle> {
        let (handle, join) = spawn_reflector_in(ns_name, bind)?;
        self.children.push(ChildTask::Thread {
            handle: handle.clone(),
            join,
        });
        Ok(handle)
    }

    /// Spawns a UDP reflector in the lab root namespace (IX bridge side).
    pub fn spawn_reflector_on_ix(&mut self, bind: SocketAddr) -> Result<TaskHandle> {
        let (handle, join) = spawn_reflector_in(self.core.root_ns(), bind)?;
        self.children.push(ChildTask::Thread {
            handle: handle.clone(),
            join,
        });
        Ok(handle)
    }

    /// Probes the NAT mapping seen by a reflector from a named device.
    pub fn probe_udp_mapping(&self, device: &str, reflector: SocketAddr) -> Result<ObservedAddr> {
        let id = self.resolve_device(device)?;
        let ns = self.core.device_ns(id)?;
        let base = 40000u16;
        let port = base + ((id.0 % 20000) as u16);
        probe_in_ns(ns, reflector, Duration::from_millis(500), Some(port))
    }

    // ── Lookup helpers ───────────────────────────────────────────────────

    /// Returns the network namespace name for a node.
    pub fn node_ns(&self, id: NodeId) -> Result<&str> {
        if let Some(r) = self.core.router(id) {
            return Ok(&r.ns);
        }
        if let Some(d) = self.core.device(id) {
            return Ok(&d.ns);
        }
        Err(anyhow!("unknown node id"))
    }

    /// Returns the router's downstream gateway IP.
    pub fn router_downlink_gw(&self, id: NodeId) -> Result<Ipv4Addr> {
        self.core
            .router(id)
            .and_then(|rt| rt.downstream_gw)
            .ok_or_else(|| anyhow!("router missing downstream gw"))
    }

    /// Returns the router's uplink IP.
    pub fn router_uplink_ip(&self, id: NodeId) -> Result<Ipv4Addr> {
        self.core
            .router(id)
            .and_then(|rt| rt.upstream_ip)
            .ok_or_else(|| anyhow!("router missing upstream ip"))
    }

    /// Returns the assigned IP of a device's default interface.
    pub fn device_ip(&self, id: NodeId) -> Result<Ipv4Addr> {
        self.core
            .device(id)
            .map(|dev| dev.default_iface().ip)
            .ok_or_else(|| anyhow!("unknown device id"))?
            .ok_or_else(|| anyhow!("device default interface missing ip"))
    }

    /// Resolves a router name to its [`NodeId`].
    pub fn router_id(&self, name: &str) -> Option<NodeId> {
        self.router_by_name.get(name).copied()
    }

    /// Resolves a device name to its [`NodeId`].
    pub fn device_id(&self, name: &str) -> Option<NodeId> {
        self.device_by_name.get(name).copied()
    }

    /// Returns the IX gateway IP (203.0.113.1).
    pub fn ix_gw(&self) -> Ipv4Addr {
        self.core.ix_gw()
    }

    /// Removes any known lab resources created by this process.
    pub fn cleanup(&self) {
        resources().cleanup_registered();
    }

    /// Removes any resources whose names match the lab prefix.
    ///
    /// This is useful if a previous run crashed before it could clean up.
    pub fn cleanup_everything() {
        resources().cleanup_registered_prefixes();
    }

    // ── Private helpers ──────────────────────────────────────────────────

    fn ns_name(&mut self) -> String {
        let id = self.ns_counter;
        self.ns_counter = self.ns_counter.saturating_add(1);
        format!("{}-{}", self.prefix, id)
    }

    fn dev_ns(&self, device: &str) -> Result<String> {
        let id = self.resolve_device(device)?;
        Ok(self.core.device_ns(id)?.to_string())
    }

    fn resolve_device(&self, name: &str) -> Result<NodeId> {
        self.device_by_name
            .get(name)
            .copied()
            .ok_or_else(|| anyhow!("unknown device '{}'", name))
    }

    fn resolve_router(&self, name: &str) -> Result<NodeId> {
        self.router_by_name
            .get(name)
            .copied()
            .ok_or_else(|| anyhow!("unknown router '{}'", name))
    }

    // ── Dynamic operations ────────────────────────────────────────────────

    /// Applies or removes a link-layer impairment on a device interface.
    ///
    /// `ifname = None` targets the `default_via` interface.
    /// `impair = None` removes any existing qdisc.
    pub fn set_impair(
        &mut self,
        device: &str,
        ifname: Option<&str>,
        impair: Option<Impair>,
    ) -> Result<()> {
        let id = self.resolve_device(device)?;
        let (ns, resolved_ifname) = {
            let dev = self
                .core
                .device(id)
                .ok_or_else(|| anyhow!("unknown device id"))?;
            let iname = ifname.unwrap_or(&dev.default_via).to_string();
            if dev.iface(&iname).is_none() {
                bail!("interface '{}' not found on device '{}'", iname, device);
            }
            (dev.ns.clone(), iname)
        };
        match impair {
            Some(imp) => apply_impair_in(&ns, &resolved_ifname, imp),
            None => qdisc::remove_qdisc(&ns, &resolved_ifname),
        }
        // Update stored impair so switch_route can re-apply it correctly.
        if let Some(dev) = self.core.device_mut(id) {
            if let Some(iface) = dev.iface_mut(&resolved_ifname) {
                iface.impair = impair;
            }
        }
        Ok(())
    }

    /// Brings a device interface administratively down.
    pub fn link_down(&mut self, device: &str, ifname: &str) -> Result<()> {
        let ns = self.dev_ns(device)?;
        self.core.set_link_state_in_namespace(&ns, ifname, false)?;
        Ok(())
    }

    /// Brings a device interface administratively up.
    pub fn link_up(&mut self, device: &str, ifname: &str) -> Result<()> {
        let ns = self.dev_ns(device)?;
        self.core.set_link_state_in_namespace(&ns, ifname, true)?;
        Ok(())
    }

    /// Switches the active default route to a different interface.
    ///
    /// `to` is the interface name (e.g. `"eth1"`).  The impairment configured
    /// for the new interface is re-applied after the route change.
    pub fn switch_route(&mut self, device: &str, to: &str) -> Result<()> {
        let id = self.resolve_device(device)?;
        let (ns, uplink, impair) = {
            let dev = self
                .core
                .device(id)
                .ok_or_else(|| anyhow!("unknown device id"))?;
            let iface = dev
                .iface(to)
                .ok_or_else(|| anyhow!("interface '{}' not found on device '{}'", to, device))?;
            (dev.ns.clone(), iface.uplink, iface.impair)
        };
        let gw_ip = self.core.router_downlink_gw_for_switch(uplink)?;
        self.core
            .replace_default_route_in_namespace(&ns, to, gw_ip)?;
        match impair {
            Some(imp) => apply_impair_in(&ns, to, imp),
            None => qdisc::remove_qdisc(&ns, to),
        }
        self.core.set_device_default_via(id, to)?;
        Ok(())
    }
}

impl Default for Lab {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        resources().cleanup_registered();
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

    /// Overrides which interface carries the default route.
    ///
    /// By default this is the first interface added via [`iface`][DeviceBuilder::iface].
    pub fn default_via(mut self, ifname: &str) -> Self {
        if self.result.is_ok() {
            self.result = self.lab.core.set_device_default_via(self.id, ifname);
        }
        self
    }

    /// Finalizes the device and returns its [`NodeId`].
    pub fn build(self) -> Result<NodeId> {
        self.result?;
        Ok(self.id)
    }
}

// ─────────────────────────────────────────────
// TOML config types
// ─────────────────────────────────────────────

/// Defines TOML configuration structures used by `Lab::load`.
pub mod config {
    use super::NatMode;
    use serde::Deserialize;
    use std::collections::HashMap;

    /// Parsed lab configuration from TOML.
    #[derive(Deserialize, Clone, Default)]
    pub struct LabConfig {
        /// Optional region-latency map.
        pub region: Option<HashMap<String, RegionConfig>>,
        /// Router entries.
        #[serde(default)]
        pub router: Vec<RouterCfg>,
        /// Raw device tables; post-processed by [`Lab::from_config`].
        ///
        /// Format: `[device.<name>.<ifname>]` with a `gateway` field.
        /// Device-level settings (e.g. `default_via`) live in `[device.<name>]`.
        #[serde(default)]
        pub device: HashMap<String, toml::Value>,
    }

    /// Per-region latency configuration.
    #[derive(Deserialize, Clone)]
    pub struct RegionConfig {
        /// Map of target-region name → one-way latency in ms.
        #[serde(default)]
        pub latencies: HashMap<String, u32>,
    }

    /// Router configuration entry.
    #[derive(Deserialize, Clone)]
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
}

// ─────────────────────────────────────────────
// STUN-like reflector + probe
// ─────────────────────────────────────────────

/// Spawns a UDP reflector that echoes "OBSERVED <peer_ip>:<peer_port>" back to
/// each sender inside the named netns.
fn spawn_reflector_in(
    ns: &str,
    bind: SocketAddr,
) -> Result<(TaskHandle, thread::JoinHandle<Result<()>>)> {
    let ns = ns.to_string();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let join = spawn_closure_in_namespace_thread(ns, move || {
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

/// Sends a UDP probe from inside `ns` and returns the observed external address.
pub fn probe_in_ns(
    ns: &str,
    reflector: SocketAddr,
    timeout: Duration,
    bind_port: Option<u16>,
) -> Result<ObservedAddr> {
    let ns_name = ns.to_string();
    let ns_for_log = ns_name.clone();
    run_closure_in_namespace(&ns_name, move || {
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

/// Normalise a device/interface name for use in an environment variable name.
/// Converts to uppercase and replaces `-` with `_`.
fn normalize_env_name(s: &str) -> String {
    s.to_uppercase().replace('-', "_")
}

/// Returns the observed external address from a one-shot UDP probe in `ns`.
pub fn udp_roundtrip_in_ns(ns: &str, reflector: SocketAddr) -> Result<ObservedAddr> {
    probe_in_ns(ns, reflector, Duration::from_millis(500), None)
}

/// Returns UDP round-trip time from `ns` to `reflector`.
pub fn udp_rtt_in_ns(ns: &str, reflector: SocketAddr) -> Result<Duration> {
    run_closure_in_namespace(ns, move || {
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
    use tracing::debug;

    use super::*;

    fn ping_in_ns(ns: &str, addr: &str) -> Result<()> {
        let mut cmd = std::process::Command::new("ping");
        cmd.args(["-c", "1", "-W", "1", addr]);
        let status = run_command_in_namespace(ns, cmd)?;
        if !status.success() {
            bail!("ping {} failed with status {}", addr, status);
        }
        Ok(())
    }

    fn ping_fails_in_ns(ns: &str, addr: &str) -> Result<()> {
        let mut cmd = std::process::Command::new("ping");
        cmd.args(["-c", "1", "-W", "1", addr]);
        let status = run_command_in_namespace(ns, cmd)?;
        if status.success() {
            bail!("ping {} unexpectedly succeeded", addr);
        }
        Ok(())
    }

    #[derive(Clone, Copy, Debug)]
    enum UplinkWiring {
        DirectIx,
        ViaPublicIsp,
        ViaCgnatIsp,
    }

    impl UplinkWiring {
        fn label(self) -> &'static str {
            match self {
                Self::DirectIx => "direct-ix",
                Self::ViaPublicIsp => "via-public-isp",
                Self::ViaCgnatIsp => "via-cgnat-isp",
            }
        }
    }

    async fn build_single_nat_case(
        nat_mode: NatMode,
        wiring: UplinkWiring,
        port_base: u16,
    ) -> Result<(Lab, String, SocketAddr, SocketAddr, Ipv4Addr)> {
        let mut lab = Lab::new();
        let dc = lab.add_router("dc", Some("eu"), None, NatMode::None)?;
        let upstream = match wiring {
            UplinkWiring::DirectIx => None,
            UplinkWiring::ViaPublicIsp => {
                Some(lab.add_router("isp", Some("eu"), None, NatMode::None)?)
            }
            UplinkWiring::ViaCgnatIsp => {
                Some(lab.add_router("isp", Some("eu"), None, NatMode::Cgnat)?)
            }
        };
        let nat = lab.add_router("nat", None, upstream, nat_mode)?;
        let dev = lab.add_device("dev").iface("eth0", nat, None).build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r_dc = SocketAddr::new(IpAddr::V4(dc_ip), port_base);
        let r_ix = SocketAddr::new(IpAddr::V4(lab.ix_gw()), port_base + 1);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, r_dc)?;
        lab.spawn_reflector_on_ix(r_ix)?;
        tokio::time::sleep(Duration::from_millis(200)).await;

        let dev_ns = lab.node_ns(dev)?.to_string();
        let expected_ip = match (nat_mode, wiring) {
            (_, UplinkWiring::ViaCgnatIsp) => {
                let isp = lab.router_id("isp").context("missing isp")?;
                lab.router_uplink_ip(isp)?
            }
            (NatMode::None, _) => lab.device_ip(dev)?,
            _ => lab.router_uplink_ip(nat)?,
        };
        Ok((lab, dev_ns, r_dc, r_ix, expected_ip))
    }

    fn spawn_tcp_echo_in(ns: &str, bind: SocketAddr) -> thread::JoinHandle<Result<()>> {
        Lab::run_in_namespace_thread(ns, move || {
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
        run_closure_in_namespace(ns, move || {
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

    fn current_netns_inode() -> Result<String> {
        let link = std::fs::read_link("/proc/self/ns/net").context("read host netns inode")?;
        Ok(link.to_string_lossy().to_string())
    }

    fn netns_inode(ns: &str) -> Result<String> {
        let ns = ns.to_string();
        let ns_for_msg = ns.clone();
        run_closure_in_namespace(&ns, move || {
            let link = std::fs::read_link("/proc/thread-self/ns/net")
                .or_else(|_| std::fs::read_link("/proc/self/ns/net"))
                .with_context(|| format!("read netns inode in '{ns_for_msg}'"))?;
            Ok(link.to_string_lossy().to_string())
        })
    }

    fn run_cmd_output_in_ns(
        ns: &str,
        program: &str,
        args: &[&str],
    ) -> Result<std::process::Output> {
        let ns = ns.to_string();
        let ns_for_msg = ns.clone();
        let program = program.to_string();
        let args: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        run_closure_in_namespace(&ns, move || {
            let mut cmd = std::process::Command::new(&program);
            cmd.args(&args);
            cmd.output()
                .with_context(|| format!("run '{program} {}' in ns '{ns_for_msg}'", args.join(" ")))
        })
    }

    fn dump_ns_state(ns: &str, phase: &str) {
        eprintln!("diag[{phase}] ns={ns}");
        match netns_inode(ns) {
            Ok(ino) => eprintln!("diag[{phase}] ns={ns} inode={ino}"),
            Err(err) => eprintln!("diag[{phase}] ns={ns} inode_error={err:#}"),
        }
        for (label, args) in [
            ("links", vec!["-o", "link", "show"]),
            ("addrs", vec!["-4", "addr", "show"]),
            ("routes", vec!["-4", "route", "show"]),
        ] {
            match run_cmd_output_in_ns(ns, "ip", &args) {
                Ok(out) => {
                    eprintln!("diag[{phase}] ns={ns} {label} status={}", out.status);
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    if !stdout.trim().is_empty() {
                        eprintln!("diag[{phase}] ns={ns} {label} stdout:\n{stdout}");
                    }
                    if !stderr.trim().is_empty() {
                        eprintln!("diag[{phase}] ns={ns} {label} stderr:\n{stderr}");
                    }
                }
                Err(err) => eprintln!("diag[{phase}] ns={ns} {label} error={err:#}"),
            }
        }
    }

    // ── Builder-API NAT tests ────────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn smoke_debug_netns_exit_trace() -> Result<()> {
        check_caps()?;
        let host_inode_before = current_netns_inode()?;
        debug!(host_inode_before = %host_inode_before, "diag: host inode before build");

        let mut lab = Lab::new();
        let isp = lab.add_router("isp1", Some("eu"), None, NatMode::None)?;
        let home = lab.add_router("home1", None, Some(isp), NatMode::DestinationIndependent)?;
        lab.add_device("dev1").iface("eth0", home, None).build()?;

        let ns_plan = lab.core.all_ns_names();
        eprintln!("diag[pre-build] host_inode={}", current_netns_inode()?);
        for ns in &ns_plan {
            dump_ns_state(ns, "pre-build");
        }

        if let Err(err) = lab.build().await {
            eprintln!("diag[build-error] host_inode={}", current_netns_inode()?);
            eprintln!("diag[build-error] build_err={err:#}");
            for ns in &ns_plan {
                dump_ns_state(ns, "build-error");
            }
            return Err(err).context("smoke_debug_netns_exit_trace build failed");
        }

        let ns_after = lab.core.all_ns_names();
        eprintln!("diag[post-build] host_inode={}", current_netns_inode()?);
        for ns in &ns_after {
            dump_ns_state(ns, "post-build");
        }

        let dev_id = lab.device_id("dev1").context("missing dev1")?;
        let dev_ns = lab.node_ns(dev_id)?.to_string();
        let lan_gw = lab.router_downlink_gw(home)?;
        ping_in_ns(&dev_ns, &lan_gw.to_string())?;

        let host_inode_after = current_netns_inode()?;
        debug!(host_inode_after = %host_inode_after, "diag: host inode after smoke");
        eprintln!("diag[done] host_inode_after={host_inode_after}");
        Ok(())
    }

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

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn iroh_nat_like_nodes_report_public_203_mapped_addrs() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        let dc = lab.add_router("dc", Some("eu"), None, NatMode::None)?;
        let isp = lab.add_router("isp", Some("eu"), None, NatMode::Cgnat)?;
        let lan_provider = lab.add_router(
            "lan-provider",
            None,
            Some(isp),
            NatMode::DestinationIndependent,
        )?;
        let lan_fetcher = lab.add_router(
            "lan-fetcher",
            None,
            Some(isp),
            NatMode::DestinationIndependent,
        )?;
        lab.add_device("provider")
            .iface("eth0", lan_provider, None)
            .build()?;
        lab.add_device("fetcher")
            .iface("eth0", lan_fetcher, None)
            .build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 6478);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, reflector)?;
        tokio::time::sleep(Duration::from_millis(250)).await;

        let provider_obs = lab.probe_udp_mapping("provider", reflector)?;
        let fetcher_obs = lab.probe_udp_mapping("fetcher", reflector)?;
        let isp_public = lab.router_uplink_ip(isp)?;

        let provider_ip = match provider_obs.observed.ip() {
            IpAddr::V4(ip) => ip,
            IpAddr::V6(ip) => bail!("expected provider observed IPv4 address, got {ip}"),
        };
        let fetcher_ip = match fetcher_obs.observed.ip() {
            IpAddr::V4(ip) => ip,
            IpAddr::V6(ip) => bail!("expected fetcher observed IPv4 address, got {ip}"),
        };

        assert_eq!(
            provider_ip.octets()[0],
            203,
            "provider STUN report should be public 203.* mapped IP, got {}",
            provider_obs.observed
        );
        assert_eq!(
            fetcher_ip.octets()[0],
            203,
            "fetcher STUN report should be public 203.* mapped IP, got {}",
            fetcher_obs.observed
        );
        assert_eq!(
            provider_ip, isp_public,
            "provider should be mapped behind ISP public address"
        );
        assert_eq!(
            fetcher_ip, isp_public,
            "fetcher should be mapped behind ISP public address"
        );
        assert_ne!(
            provider_obs.observed.port(),
            0,
            "provider mapped port should be non-zero"
        );
        assert_ne!(
            fetcher_obs.observed.port(),
            0,
            "fetcher mapped port should be non-zero"
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

[device.dev1.eth0]
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
    async fn smoke_nat_homes_can_ping_public_relay_device() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();

        let dc = lab.add_router("dc", None, None, NatMode::None)?;
        let lan_provider =
            lab.add_router("lan-provider", None, None, NatMode::DestinationIndependent)?;
        let lan_fetcher =
            lab.add_router("lan-fetcher", None, None, NatMode::DestinationIndependent)?;

        let relay = lab.add_device("relay").iface("eth0", dc, None).build()?;
        let provider = lab
            .add_device("provider")
            .iface("eth0", lan_provider, None)
            .build()?;
        let fetcher = lab
            .add_device("fetcher")
            .iface("eth0", lan_fetcher, None)
            .build()?;

        lab.build().await?;

        let relay_ip = lab.device_ip(relay)?;
        let provider_ns = lab.node_ns(provider)?.to_string();
        let fetcher_ns = lab.node_ns(fetcher)?.to_string();

        ping_in_ns(&provider_ns, &relay_ip.to_string())?;
        ping_in_ns(&fetcher_ns, &relay_ip.to_string())?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn nat_matrix_public_connectivity_and_reflexive_ip() -> Result<()> {
        check_caps()?;
        let cases = [
            (NatMode::None, UplinkWiring::DirectIx),
            (NatMode::Cgnat, UplinkWiring::DirectIx),
            (NatMode::DestinationIndependent, UplinkWiring::DirectIx),
            (NatMode::DestinationIndependent, UplinkWiring::ViaPublicIsp),
            (NatMode::DestinationIndependent, UplinkWiring::ViaCgnatIsp),
            (NatMode::DestinationDependent, UplinkWiring::DirectIx),
            (NatMode::DestinationDependent, UplinkWiring::ViaPublicIsp),
            (NatMode::DestinationDependent, UplinkWiring::ViaCgnatIsp),
        ];

        let mut case_idx = 0u16;
        for (mode, wiring) in cases {
            let port_base = 10000 + case_idx * 10;
            case_idx = case_idx.saturating_add(1);
            let (lab, dev_ns, r_dc, _r_ix, expected_ip) =
                build_single_nat_case(mode, wiring, port_base).await?;

            ping_in_ns(&dev_ns, &r_dc.ip().to_string())?;
            let _ = udp_roundtrip_in_ns(&dev_ns, r_dc)?;
            let observed = lab.probe_udp_mapping("dev", r_dc)?;
            assert_eq!(
                observed.observed.ip(),
                IpAddr::V4(expected_ip),
                "unexpected reflexive IP for mode={mode:?} wiring={}",
                wiring.label()
            );
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn nat_mapping_port_behavior_by_mode_and_wiring() -> Result<()> {
        check_caps()?;
        let modes = [
            NatMode::DestinationIndependent,
            NatMode::DestinationDependent,
        ];
        let wirings = [
            UplinkWiring::DirectIx,
            UplinkWiring::ViaPublicIsp,
            UplinkWiring::ViaCgnatIsp,
        ];

        let mut case_idx = 0u16;
        for mode in modes {
            for wiring in wirings {
                let port_base = 11000 + case_idx * 10;
                case_idx = case_idx.saturating_add(1);
                let (lab, _dev_ns, r_dc, r_ix, expected_ip) =
                    build_single_nat_case(mode, wiring, port_base).await?;
                let o1 = lab.probe_udp_mapping("dev", r_dc)?;
                let o2 = lab.probe_udp_mapping("dev", r_ix)?;

                assert_eq!(
                    o1.observed.ip(),
                    IpAddr::V4(expected_ip),
                    "unexpected reflexive IP for mode={mode:?} wiring={}",
                    wiring.label()
                );
                assert_eq!(
                    o2.observed.ip(),
                    IpAddr::V4(expected_ip),
                    "unexpected reflexive IP for mode={mode:?} wiring={}",
                    wiring.label()
                );

                match mode {
                    NatMode::DestinationIndependent => assert_eq!(
                        o1.observed.port(),
                        o2.observed.port(),
                        "expected stable external port for mode={mode:?} wiring={}",
                        wiring.label()
                    ),
                    NatMode::DestinationDependent => assert_ne!(
                        o1.observed.port(),
                        o2.observed.port(),
                        "expected destination-dependent external port for mode={mode:?} wiring={}",
                        wiring.label()
                    ),
                    _ => unreachable!("only destination modes are tested here"),
                }
            }
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn nat_private_reachability_isolated_public_reachable() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        let dc = lab.add_router("dc", Some("eu"), None, NatMode::None)?;
        let nat_a = lab.add_router("nat-a", None, None, NatMode::DestinationIndependent)?;
        let nat_b = lab.add_router("nat-b", None, None, NatMode::DestinationIndependent)?;

        let relay = lab.add_device("relay").iface("eth0", dc, None).build()?;
        let a1 = lab.add_device("a1").iface("eth0", nat_a, None).build()?;
        let a2 = lab.add_device("a2").iface("eth0", nat_a, None).build()?;
        let b1 = lab.add_device("b1").iface("eth0", nat_b, None).build()?;
        lab.build().await?;

        let a1_ns = lab.node_ns(a1)?.to_string();
        let b1_ns = lab.node_ns(b1)?.to_string();
        let a2_ip = lab.device_ip(a2)?;
        let b1_ip = lab.device_ip(b1)?;
        let a1_ip = lab.device_ip(a1)?;
        let relay_ip = lab.device_ip(relay)?;

        ping_in_ns(&a1_ns, &a2_ip.to_string())?;
        ping_fails_in_ns(&a1_ns, &b1_ip.to_string())?;
        ping_fails_in_ns(&b1_ns, &a1_ip.to_string())?;

        ping_in_ns(&a1_ns, &relay_ip.to_string())?;
        ping_in_ns(&b1_ns, &relay_ip.to_string())?;

        let nat_a_public = lab.router_uplink_ip(nat_a)?;
        let nat_b_public = lab.router_uplink_ip(nat_b)?;
        ping_in_ns(&a1_ns, &nat_b_public.to_string())?;
        ping_in_ns(&b1_ns, &nat_a_public.to_string())?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 12000);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, reflector)?;
        tokio::time::sleep(Duration::from_millis(200)).await;

        let a1_map = lab.probe_udp_mapping("a1", reflector)?;
        let a2_map = lab.probe_udp_mapping("a2", reflector)?;
        let b1_map = lab.probe_udp_mapping("b1", reflector)?;
        assert_eq!(a1_map.observed.ip(), IpAddr::V4(nat_a_public));
        assert_eq!(a2_map.observed.ip(), IpAddr::V4(nat_a_public));
        assert_eq!(b1_map.observed.ip(), IpAddr::V4(nat_b_public));
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
        let dev_eu = lab
            .add_device("dev-eu")
            .iface("eth0", dc_eu, None)
            .build()?;
        let dev_us = lab
            .add_device("dev-us")
            .iface("eth0", dc_us, None)
            .build()?;
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
            lab.add_device("dev1")
                .iface("eth0", dc_eu, impair)
                .build()?;
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

    // ── Dynamic-ops tests ────────────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn dynamic_set_impair_changes_rtt() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        let dc = lab.add_router("dc1", Some("eu"), None, NatMode::None)?;
        let dev = lab.add_device("dev1").iface("eth0", dc, None).build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r = SocketAddr::new(IpAddr::V4(dc_ip), 9100);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, r)?;
        tokio::time::sleep(Duration::from_millis(250)).await;

        let dev_ns = lab.node_ns(dev)?.to_string();
        let base_rtt = udp_rtt_in_ns(&dev_ns, r)?;

        // Apply Mobile impair (+50 ms one-way latency).
        lab.set_impair("dev1", None, Some(Impair::Mobile))?;
        let impaired_rtt = udp_rtt_in_ns(&dev_ns, r)?;
        assert!(
            impaired_rtt >= base_rtt + Duration::from_millis(40),
            "expected impaired RTT >= base + 40ms, base={base_rtt:?} impaired={impaired_rtt:?}"
        );

        // Remove impair — RTT should drop back near baseline.
        lab.set_impair("dev1", None, None)?;
        let recovered_rtt = udp_rtt_in_ns(&dev_ns, r)?;
        assert!(
            recovered_rtt < base_rtt + Duration::from_millis(30),
            "expected recovered RTT close to base, base={base_rtt:?} recovered={recovered_rtt:?}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn dynamic_link_down_up_connectivity() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        let dc = lab.add_router("dc1", Some("eu"), None, NatMode::None)?;
        let dev = lab.add_device("dev1").iface("eth0", dc, None).build()?;
        lab.build().await?;

        let gw = lab.router_downlink_gw(dc)?;
        let dev_ns = lab.node_ns(dev)?.to_string();

        // Connectivity should be OK initially.
        ping_in_ns(&dev_ns, &gw.to_string())?;

        // Bring interface down — ping must fail.
        lab.link_down("dev1", "eth0")?;
        let result = ping_in_ns(&dev_ns, &gw.to_string());
        assert!(result.is_err(), "expected ping to fail after link_down");

        // Bring interface back up — connectivity must be restored.
        lab.link_up("dev1", "eth0")?;
        ping_in_ns(&dev_ns, &gw.to_string())?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    #[traced_test]
    async fn dynamic_switch_route_changes_path() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        // Two IX-attached public routers.
        let dc = lab.add_router("dc1", Some("eu"), None, NatMode::None)?;
        let isp = lab.add_router("isp1", Some("eu"), None, NatMode::None)?;
        // Device: eth0 → dc (fast, no impair); eth1 → isp (slow, Mobile = +50 ms).
        let dev = lab
            .add_device("dev1")
            .iface("eth0", dc, None)
            .iface("eth1", isp, Some(Impair::Mobile))
            .default_via("eth0")
            .build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r = SocketAddr::new(IpAddr::V4(dc_ip), 9200);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, r)?;
        tokio::time::sleep(Duration::from_millis(250)).await;

        let dev_ns = lab.node_ns(dev)?.to_string();
        let fast_rtt = udp_rtt_in_ns(&dev_ns, r)?;

        // Switch to the Mobile path (eth1 → isp → ix → dc).
        lab.switch_route("dev1", "eth1")?;
        let slow_rtt = udp_rtt_in_ns(&dev_ns, r)?;

        assert!(
            slow_rtt >= fast_rtt + Duration::from_millis(80),
            "expected slow RTT >= fast + 80ms, fast={fast_rtt:?} slow={slow_rtt:?}"
        );
        Ok(())
    }

    #[test]
    fn manual_impair_deserialize() -> Result<()> {
        let cfg = r#"
[[router]]
name = "dc1"
region = "eu"

[device.dev1.eth0]
gateway = "dc1"
impair = { rate = 5000, loss = 1.5, latency = 40 }
"#;
        let parsed: config::LabConfig = toml::from_str(cfg)?;
        let dev1 = parsed.device.get("dev1").context("missing dev1")?;
        let eth0 = dev1.get("eth0").context("missing eth0")?;
        let impair: Impair = eth0
            .get("impair")
            .context("missing impair")?
            .clone()
            .try_into()
            .map_err(|e: toml::de::Error| anyhow!("{}", e))?;
        match impair {
            Impair::Manual {
                rate,
                loss,
                latency,
            } => {
                assert_eq!(rate, 5000);
                assert!((loss - 1.5).abs() < f32::EPSILON);
                assert_eq!(latency, 40);
            }
            other => bail!("unexpected impair: {:?}", other),
        }
        Ok(())
    }

    #[test]
    fn from_config_expands_count_devices() -> Result<()> {
        let cfg = r#"
[[router]]
name = "dc1"

[device.fetcher]
count = 2
default_via = "eth0"

[device.fetcher.eth0]
gateway = "dc1"
"#;
        let parsed: config::LabConfig = toml::from_str(cfg)?;
        let lab = Lab::from_config(parsed)?;
        assert!(lab.device_id("fetcher-0").is_some());
        assert!(lab.device_id("fetcher-1").is_some());
        assert!(lab.device_id("fetcher").is_none());
        Ok(())
    }
}
