//! High-level lab API: [`Lab`], [`DeviceBuilder`], [`NatMode`], [`Impair`], [`ObservedAddr`].

use std::{
    collections::HashMap,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::Path,
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use ipnet::{Ipv4Net, Ipv6Net};
use serde::Deserialize;
use tracing::{debug, debug_span, Instrument as _};

use crate::core::{
    apply_impair_in, apply_nat, apply_nat_v6, cleanup_netns, run_closure_in_namespace, run_nft_in,
    setup_device_async, setup_root_ns_async, setup_router_async, spawn_command_in_namespace,
    CoreConfig, DownstreamPool, IfaceBuild, NetworkCore, NodeId, RouterSetupData, TaskHandle,
};

pub(crate) static LAB_COUNTER: AtomicU64 = AtomicU64::new(0);

// ─────────────────────────────────────────────
// Public types
// ─────────────────────────────────────────────

/// NAT mode for a router.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, strum::EnumIter, strum::Display,
)]
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

/// IPv6 NAT mode for a router.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NatV6Mode {
    /// No translation — devices use global unicast directly.
    #[default]
    None,
    /// RFC 6296 stateless prefix translation (1:1 prefix mapping).
    Nptv6,
    /// Stateful masquerade (useful for testing symmetric behaviour on IPv6).
    Masquerade,
}

/// Selects which IP address families a router supports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IpSupport {
    /// IPv4 only (default, backwards-compatible).
    #[default]
    V4Only,
    /// IPv6 only.
    V6Only,
    /// Both IPv4 and IPv6.
    DualStack,
}

impl IpSupport {
    /// Returns `true` when IPv4 is enabled.
    pub fn has_v4(self) -> bool {
        matches!(self, IpSupport::V4Only | IpSupport::DualStack)
    }
    /// Returns `true` when IPv6 is enabled.
    pub fn has_v6(self) -> bool {
        matches!(self, IpSupport::V6Only | IpSupport::DualStack)
    }
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

/// High-level lab API built on top of `NetworkCore`.
///
/// `Lab` wraps `Arc<Mutex<LabInner>>` and is cheaply cloneable. All methods
/// take `&self` and use interior mutability through the mutex.
#[derive(Clone)]
pub struct Lab {
    pub(crate) inner: Arc<Mutex<LabInner>>,
}

pub(crate) struct LabInner {
    /// Short process-unique prefix used on root-namespace interface names.
    pub(crate) prefix: String,
    /// (from_region, to_region, latency_ms) pairs; applied as tc netem during build.
    pub(crate) region_latencies: Vec<(String, String, u32)>,

    /// Background tasks spawned by the lab (reflectors, commands).
    children: Vec<ChildTask>,

    /// Low-level topology model.
    pub(crate) core: NetworkCore,

    /// Tracing span for this lab instance; used as parent for router/device/worker spans.
    pub(crate) span: tracing::Span,
}

enum ChildTask {
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
        let root_ns = format!("lab{lab_seq}-root");
        let bridge_tag = format!("p{}{}", pid_tag, uniq);
        let ix_gw = Ipv4Addr::new(203, 0, 113, 1);
        let lab_span = debug_span!("lab", id = lab_seq);
        let _lab_enter = lab_span.enter();
        debug!(prefix = %prefix, "lab: created");
        let core = NetworkCore::new(CoreConfig {
            lab_id: lab_seq,
            prefix: prefix.clone(),
            root_ns,
            bridge_tag,
            ix_br: format!("br-p{}{}-1", pid_tag, uniq),
            ix_gw,
            ix_cidr: "203.0.113.0/24".parse().expect("valid ix cidr"),
            private_cidr: "10.0.0.0/16".parse().expect("valid private cidr"),
            public_cidr: "203.0.113.0/24".parse().expect("valid public cidr"),
            ix_gw_v6: "2001:db8::1".parse().expect("valid ix gw v6"),
            ix_cidr_v6: "2001:db8::/32".parse().expect("valid ix cidr v6"),
            private_cidr_v6: "fd10::/48".parse().expect("valid private cidr v6"),
            span: lab_span.clone(),
        });
        drop(_lab_enter);
        Self {
            inner: Arc::new(Mutex::new(LabInner {
                prefix,
                region_latencies: vec![],
                children: vec![],
                core,
                span: lab_span,
            })),
        }
    }

    /// Returns the unique resource prefix associated with this lab instance.
    pub fn prefix(&self) -> String {
        self.inner.lock().unwrap().prefix.clone()
    }

    /// Returns the dedicated lab root namespace name.
    pub fn root_namespace_name(&self) -> String {
        self.inner.lock().unwrap().core.root_ns().to_string()
    }

    /// Parses `lab.toml`, builds the network, and returns a ready-to-use lab.
    pub async fn load(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path).context("read lab config")?;
        let cfg: crate::config::LabConfig = toml::from_str(&text).context("parse lab config")?;
        Self::from_config(cfg).await
    }

    /// Builds a `Lab` from a parsed config, creating all namespaces and links.
    pub async fn from_config(cfg: crate::config::LabConfig) -> Result<Self> {
        let lab = Self::new();

        // Region latency pairs.
        if let Some(regions) = &cfg.region {
            let mut inner = lab.inner.lock().unwrap();
            for (from, rcfg) in regions {
                for (to, &ms) in &rcfg.latencies {
                    inner.region_latencies.push((from.clone(), to.clone(), ms));
                }
            }
        }

        // Routers: topological sort — process any router whose upstream is already resolved.
        let mut pending: HashMap<&str, &crate::config::RouterConfig> =
            cfg.router.iter().map(|r| (r.name.as_str(), r)).collect();
        loop {
            let ready: Vec<&str> = pending
                .keys()
                .copied()
                .filter(|&name| {
                    pending[name]
                        .upstream
                        .as_deref()
                        .map(|up| !pending.contains_key(up))
                        .unwrap_or(true)
                })
                .collect();
            if ready.is_empty() {
                break;
            }
            // Sort for deterministic order (parent before child, stable within same depth).
            let mut ready = ready;
            ready.sort();
            for name in ready {
                let rcfg = pending.remove(name).unwrap();
                let upstream = {
                    let inner = lab.inner.lock().unwrap();
                    rcfg.upstream
                        .as_deref()
                        .and_then(|n| inner.core.router_id_by_name(n))
                };
                let mut rb = lab
                    .add_router(&rcfg.name)
                    .nat(rcfg.nat)
                    .ip_support(rcfg.ip_support)
                    .nat_v6(rcfg.nat_v6);
                if let Some(r) = &rcfg.region {
                    rb = rb.region(r);
                }
                if let Some(u) = upstream {
                    rb = rb.upstream(u);
                }
                rb.build().await?;
            }
        }
        if !pending.is_empty() {
            let mut names: Vec<_> = pending.keys().copied().collect();
            names.sort();
            bail!(
                "unresolvable router upstreams (cycle?): {}",
                names.join(", ")
            );
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
                    let router_id = lab
                        .inner
                        .lock()
                        .unwrap()
                        .core
                        .router_id_by_name(gw_name)
                        .ok_or_else(|| {
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
            builder.build().await?;
        }

        // Apply inter-region latency rules now that all routers are built.
        lab.apply_region_latencies()?;

        Ok(lab)
    }

    /// Applies stored region latency rules to IX-connected routers' "ix" interfaces.
    fn apply_region_latencies(&self) -> Result<()> {
        let inner = self.inner.lock().unwrap();
        if inner.region_latencies.is_empty() {
            return Ok(());
        }

        // Build region → target CIDRs map from IX-connected routers.
        let mut region_targets: HashMap<String, Vec<ipnet::IpNet>> = HashMap::new();
        for router in inner.core.all_routers() {
            let Some(uplink) = router.uplink else {
                continue;
            };
            if uplink != inner.core.ix_sw() {
                continue;
            }
            let Some(region) = router.region.as_ref() else {
                continue;
            };
            // v4 targets
            if let Some(ix_ip) = router.upstream_ip {
                if let Ok(cidr) = ipnet::Ipv4Net::new(ix_ip, 32) {
                    region_targets
                        .entry(region.clone())
                        .or_default()
                        .push(cidr.into());
                }
            }
            if router.cfg.downstream_pool == crate::core::DownstreamPool::Public {
                if let Some(cidr) = router.downstream_cidr {
                    region_targets
                        .entry(region.clone())
                        .or_default()
                        .push(cidr.into());
                }
            }
            // v6 targets
            if let Some(ix_ip_v6) = router.upstream_ip_v6 {
                if let Ok(cidr) = ipnet::Ipv6Net::new(ix_ip_v6, 128) {
                    region_targets
                        .entry(region.clone())
                        .or_default()
                        .push(cidr.into());
                }
            }
            if router.cfg.downstream_pool == crate::core::DownstreamPool::Public {
                if let Some(cidr6) = router.downstream_cidr_v6 {
                    region_targets
                        .entry(region.clone())
                        .or_default()
                        .push(cidr6.into());
                }
            }
        }

        // Apply tc netem filters on each IX-connected router's "ix" interface.
        for router in inner.core.all_routers() {
            let Some(uplink) = router.uplink else {
                continue;
            };
            if uplink != inner.core.ix_sw() {
                continue;
            }
            let Some(region) = router.region.as_ref() else {
                continue;
            };
            let mut filters = Vec::new();
            for (from, to, latency) in &inner.region_latencies {
                if from != region {
                    continue;
                }
                if let Some(targets) = region_targets.get(to) {
                    for cidr in targets {
                        filters.push((*cidr, *latency));
                    }
                }
            }
            if !filters.is_empty() {
                crate::qdisc::apply_region_latency_dual(&router.ns, "ix", &filters)?;
            }
        }
        Ok(())
    }

    // ── Builder methods (sync — just populate data structures) ──────────

    /// Begins building a router; returns a [`RouterBuilder`] to configure options.
    ///
    /// Call [`.nat()`][RouterBuilder::nat], [`.region()`][RouterBuilder::region], and/or
    /// [`.upstream()`][RouterBuilder::upstream] as needed, then
    /// [`.build()`][RouterBuilder::build] to finalise.
    ///
    /// Default NAT mode is [`NatMode::None`] (public DC-style router, IX-connected).
    pub fn add_router(&self, name: &str) -> RouterBuilder {
        let inner = self.inner.lock().unwrap();
        let lab_span = inner.span.clone();
        if inner.core.router_id_by_name(name).is_some() {
            return RouterBuilder {
                inner: Arc::clone(&self.inner),
                lab_span,
                name: name.to_string(),
                region: None,
                upstream: None,
                nat: NatMode::None,
                ip_support: IpSupport::V4Only,
                nat_v6: NatV6Mode::None,
                result: Err(anyhow!("router '{}' already exists", name)),
            };
        }
        RouterBuilder {
            inner: Arc::clone(&self.inner),
            lab_span,
            name: name.to_string(),
            region: None,
            upstream: None,
            nat: NatMode::None,
            ip_support: IpSupport::V4Only,
            nat_v6: NatV6Mode::None,
            result: Ok(()),
        }
    }

    /// Begins building a device; returns a [`DeviceBuilder`] to configure interfaces.
    ///
    /// Call [`.iface()`][DeviceBuilder::iface] one or more times to attach network
    /// interfaces, then [`.build()`][DeviceBuilder::build] to finalize.
    pub fn add_device(&self, name: &str) -> DeviceBuilder {
        let mut inner = self.inner.lock().unwrap();
        let lab_span = inner.span.clone();
        if inner.core.device_id_by_name(name).is_some() {
            return DeviceBuilder {
                inner: Arc::clone(&self.inner),
                lab_span,
                id: NodeId(u64::MAX),
                result: Err(anyhow!("device '{}' already exists", name)),
            };
        }
        let id = inner.core.add_device(name);
        DeviceBuilder {
            inner: Arc::clone(&self.inner),
            lab_span,
            id,
            result: Ok(()),
        }
    }

    // ── build ────────────────────────────────────────────────────────────

    // ── User-facing API ─────────────────────────────────────────────────

    /// Adds a one-way inter-region latency in milliseconds.
    ///
    /// If IX-connected routers are already built, the latency rules are applied
    /// immediately. Otherwise they are deferred until all routers are ready.
    pub fn set_region_latency(&self, from: &str, to: &str, latency_ms: u32) {
        debug!(from = %from, to = %to, latency_ms, "lab: set_region_latency");
        self.inner.lock().unwrap().region_latencies.push((
            from.to_string(),
            to.to_string(),
            latency_ms,
        ));
        // Best-effort immediate application; no-op if routers aren't built yet.
        let _ = self.apply_region_latencies();
    }

    /// Builds a map of `NETSIM_*` environment variables from the current lab state.
    pub fn env_vars(&self) -> std::collections::HashMap<String, String> {
        let inner = self.inner.lock().unwrap();
        let mut map = std::collections::HashMap::new();
        for dev in inner.core.all_devices() {
            let norm = normalize_env_name(&dev.name);
            if let Some(ip) = dev.default_iface().ip {
                map.insert(format!("NETSIM_IP_{}", norm), ip.to_string());
            }
            for iface in &dev.interfaces {
                if let Some(ip) = iface.ip {
                    let ifnorm = normalize_env_name(&iface.ifname);
                    map.insert(format!("NETSIM_IP_{}_{}", norm, ifnorm), ip.to_string());
                }
            }
        }
        map
    }

    // ── Reflector / probe helpers (mainly for tests) ─────────────────────

    /// Spawns a UDP reflector in the lab root namespace (IX bridge side).
    pub fn spawn_reflector_on_ix(&self, bind: SocketAddr) -> Result<TaskHandle> {
        let root_ns = self.inner.lock().unwrap().core.root_ns().to_string();
        let (handle, join) = crate::test_utils::spawn_reflector_in(&root_ns, bind)?;
        self.inner.lock().unwrap().children.push(ChildTask::Thread {
            handle: handle.clone(),
            join,
        });
        Ok(handle)
    }

    // ── Lookup helpers ───────────────────────────────────────────────────

    /// Returns the IX gateway IP (203.0.113.1).
    pub fn ix_gw(&self) -> Ipv4Addr {
        self.inner.lock().unwrap().core.ix_gw()
    }

    /// Safety-net cleanup: drops fd-registry entries for this lab's prefix.
    /// Normal cleanup happens in `NetworkCore::drop`.
    pub fn cleanup(&self) {
        let prefix = self.prefix();
        crate::netns::cleanup_registry_prefix(&prefix);
    }

    // ── Dynamic operations ────────────────────────────────────────────────

    /// Applies or removes impairment on the link between two directly connected nodes.
    ///
    /// For **Device ↔ Router**: applies impairment on the device's interface in the
    /// device namespace (affecting both upload and download on that link).
    ///
    /// For **Router ↔ Router**: applies impairment on the downstream router's WAN
    /// interface (either "ix" for IX-connected or "wan" for sub-routers).
    ///
    /// The order of `from` and `to` does not matter — the method resolves the
    /// connected pair in either direction.
    pub fn impair_link(&self, a: NodeId, b: NodeId, impair: Option<Impair>) -> Result<()> {
        debug!(a = ?a, b = ?b, impair = ?impair, "lab: impair_link");
        let inner = self.inner.lock().unwrap();

        // Try Device(a) ↔ Router(b) or Device(b) ↔ Router(a).
        if let Some(dev) = inner.core.device(a) {
            if let Some(router) = inner.core.router(b) {
                let downlink_sw = router
                    .downlink
                    .ok_or_else(|| anyhow!("router '{}' has no downstream switch", router.name))?;
                let iface = dev
                    .interfaces
                    .iter()
                    .find(|i| i.uplink == downlink_sw)
                    .ok_or_else(|| {
                        anyhow!(
                            "device '{}' is not connected to router '{}'",
                            dev.name,
                            router.name
                        )
                    })?;
                let ns = dev.ns.clone();
                let ifname = iface.ifname.clone();
                drop(inner);
                match impair {
                    Some(imp) => apply_impair_in(&ns, &ifname, imp),
                    None => crate::qdisc::remove_qdisc(&ns, &ifname),
                }
                return Ok(());
            }
        }
        if let Some(dev) = inner.core.device(b) {
            if let Some(router) = inner.core.router(a) {
                let downlink_sw = router
                    .downlink
                    .ok_or_else(|| anyhow!("router '{}' has no downstream switch", router.name))?;
                let iface = dev
                    .interfaces
                    .iter()
                    .find(|i| i.uplink == downlink_sw)
                    .ok_or_else(|| {
                        anyhow!(
                            "device '{}' is not connected to router '{}'",
                            dev.name,
                            router.name
                        )
                    })?;
                let ns = dev.ns.clone();
                let ifname = iface.ifname.clone();
                drop(inner);
                match impair {
                    Some(imp) => apply_impair_in(&ns, &ifname, imp),
                    None => crate::qdisc::remove_qdisc(&ns, &ifname),
                }
                return Ok(());
            }
        }

        // Try Router(a) ↔ Router(b) — one must be upstream of the other.
        if let (Some(ra), Some(rb)) = (inner.core.router(a), inner.core.router(b)) {
            // Check if b is downstream of a (b.uplink points to a's downlink switch).
            if let Some(a_downlink) = ra.downlink {
                if rb.uplink == Some(a_downlink) {
                    let ns = rb.ns.clone();
                    let wan_if = rb.wan_ifname(inner.core.ix_sw());
                    drop(inner);
                    match impair {
                        Some(imp) => apply_impair_in(&ns, wan_if, imp),
                        None => crate::qdisc::remove_qdisc(&ns, wan_if),
                    }
                    return Ok(());
                }
            }
            // Check if a is downstream of b.
            if let Some(b_downlink) = rb.downlink {
                if ra.uplink == Some(b_downlink) {
                    let ns = ra.ns.clone();
                    let wan_if = ra.wan_ifname(inner.core.ix_sw());
                    drop(inner);
                    match impair {
                        Some(imp) => apply_impair_in(&ns, wan_if, imp),
                        None => crate::qdisc::remove_qdisc(&ns, wan_if),
                    }
                    return Ok(());
                }
            }
            bail!(
                "routers '{}' and '{}' are not directly connected",
                ra.name,
                rb.name
            );
        }

        bail!(
            "nodes {:?} and {:?} are not a connected device-router or router-router pair",
            a,
            b
        )
    }

    /// Applies or removes impairment on a router's downlink bridge, affecting
    /// download-direction traffic to all downstream devices.
    pub fn impair_router_downlink(&self, router: NodeId, impair: Option<Impair>) -> Result<()> {
        debug!(router = ?router, impair = ?impair, "lab: impair_router_downlink");
        let inner = self.inner.lock().unwrap();
        let r = inner.core.router(router).context("unknown router id")?;
        let ns = r.ns.clone();
        let bridge = r.downlink_bridge.clone();
        drop(inner);
        match impair {
            Some(imp) => apply_impair_in(&ns, &bridge, imp),
            None => crate::qdisc::remove_qdisc(&ns, &bridge),
        }
        Ok(())
    }

    /// Like [`crate::test_utils::probe_in_ns`] but with an explicit bind address.
    pub fn probe_in_ns_from(
        ns: &str,
        reflector: SocketAddr,
        bind: SocketAddr,
        timeout: Duration,
    ) -> Result<ObservedAddr> {
        use std::net::UdpSocket;
        let ns_name = ns.to_string();
        run_closure_in_namespace(&ns_name, move || {
            let sock = UdpSocket::bind(bind)?;
            sock.set_read_timeout(Some(timeout))?;
            let mut buf = [0u8; 512];
            for _attempt in 1..=3 {
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
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            Err(anyhow!("probe timed out after 3 attempts"))
        })
    }
}

impl Default for Lab {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for LabInner {
    fn drop(&mut self) {
        for child in self.children.drain(..) {
            let ChildTask::Thread { handle, join } = child;
            handle.stop();
            let _ = join.join();
        }
        for ns_name in self.core.all_ns_names() {
            cleanup_netns(&ns_name);
        }
    }
}

// ─────────────────────────────────────────────
// RouterBuilder
// ─────────────────────────────────────────────

/// Builder for a router node; returned by [`Lab::add_router`].
pub struct RouterBuilder {
    inner: Arc<Mutex<LabInner>>,
    lab_span: tracing::Span,
    name: String,
    region: Option<String>,
    upstream: Option<NodeId>,
    nat: NatMode,
    ip_support: IpSupport,
    nat_v6: NatV6Mode,
    result: Result<()>,
}

impl RouterBuilder {
    /// Sets the region tag used for inter-region latency rules.
    pub fn region(mut self, region: &str) -> Self {
        if self.result.is_ok() {
            self.region = Some(region.to_string());
        }
        self
    }

    /// Connects this router as a subscriber behind `parent`'s downstream switch.
    ///
    /// Without this, the router attaches directly to the IX switch.
    pub fn upstream(mut self, parent: NodeId) -> Self {
        if self.result.is_ok() {
            self.upstream = Some(parent);
        }
        self
    }

    /// Sets the NAT mode. Defaults to [`NatMode::None`] (no NAT, public addressing).
    pub fn nat(mut self, mode: NatMode) -> Self {
        if self.result.is_ok() {
            self.nat = mode;
        }
        self
    }

    /// Sets which IP address families this router supports. Defaults to [`IpSupport::V4Only`].
    pub fn ip_support(mut self, support: IpSupport) -> Self {
        if self.result.is_ok() {
            self.ip_support = support;
        }
        self
    }

    /// Sets the IPv6 NAT mode. Defaults to [`NatV6Mode::None`].
    pub fn nat_v6(mut self, mode: NatV6Mode) -> Self {
        if self.result.is_ok() {
            self.nat_v6 = mode;
        }
        self
    }

    /// Finalises the router, creates its namespace and links, and returns a [`Router`] handle.
    pub async fn build(self) -> Result<Router> {
        self.result?;

        // Phase 1: Lock → register topology + extract snapshot → unlock.
        let (id, setup_data, netns, need_root_setup) = {
            let mut inner = self.inner.lock().unwrap();
            let nat = self.nat;
            let downstream_pool = if nat == NatMode::None {
                DownstreamPool::Public
            } else {
                DownstreamPool::Private
            };
            let id = inner.core.add_router(
                &self.name,
                nat,
                downstream_pool,
                self.region,
                self.ip_support,
                self.nat_v6,
            );
            let has_v4 = self.ip_support.has_v4();
            let has_v6 = self.ip_support.has_v6();
            let sub_switch =
                inner
                    .core
                    .add_switch(&format!("{}-sub", self.name), None, None, None, None);
            inner.core.connect_router_downlink(id, sub_switch)?;
            match self.upstream {
                None => {
                    let ix_ip = if has_v4 {
                        Some(inner.core.alloc_ix_ip_low())
                    } else {
                        None
                    };
                    let ix_ip_v6 = if has_v6 {
                        Some(inner.core.alloc_ix_ip_v6_low())
                    } else {
                        None
                    };
                    let ix_sw = inner.core.ix_sw();
                    inner
                        .core
                        .connect_router_uplink(id, ix_sw, ix_ip, ix_ip_v6)?;
                }
                Some(parent_id) => {
                    let parent_downlink = inner
                        .core
                        .router(parent_id)
                        .and_then(|r| r.downlink)
                        .ok_or_else(|| anyhow!("parent router missing downlink switch"))?;
                    let uplink_ip_v4 = if has_v4 {
                        Some(inner.core.alloc_from_switch(parent_downlink)?)
                    } else {
                        None
                    };
                    let uplink_ip_v6 = if has_v6 {
                        Some(inner.core.alloc_from_switch_v6(parent_downlink)?)
                    } else {
                        None
                    };
                    inner.core.connect_router_uplink(
                        id,
                        parent_downlink,
                        uplink_ip_v4,
                        uplink_ip_v6,
                    )?;
                }
            }

            // Extract snapshot for async setup.
            let router = inner.core.router(id).unwrap().clone();
            let cfg = &inner.core.cfg;
            let ix_sw = inner.core.ix_sw();

            // Upstream info for sub-routers.
            let (
                upstream_owner_ns,
                upstream_bridge,
                upstream_gw,
                upstream_cidr_prefix,
                upstream_gw_v6,
                upstream_cidr_prefix_v6,
            ) = if let Some(uplink) = router.uplink {
                if uplink != ix_sw {
                    let sw = inner.core.switch(uplink).unwrap();
                    let owner = sw.owner_router.unwrap();
                    let owner_ns = inner.core.router(owner).unwrap().ns.clone();
                    let bridge = sw.bridge.clone().unwrap_or_else(|| "br-lan".to_string());
                    let gw = sw.gw;
                    let prefix = sw.cidr.map(|c| c.prefix_len());
                    let gw_v6 = sw.gw_v6;
                    let prefix_v6 = sw.cidr_v6.map(|c| c.prefix_len());
                    (Some(owner_ns), Some(bridge), gw, prefix, gw_v6, prefix_v6)
                } else {
                    (None, None, None, None, None, None)
                }
            } else {
                (None, None, None, None, None, None)
            };

            // Downlink bridge info.
            let downlink_bridge = router.downlink.and_then(|sw_id| {
                let sw = inner.core.switch(sw_id)?;
                let br = sw.bridge.clone().unwrap_or_else(|| "br-lan".to_string());
                let v4 = sw.gw.and_then(|gw| Some((gw, sw.cidr?.prefix_len())));
                Some((br, v4))
            });
            let downlink_bridge_v6 = router.downlink.and_then(|sw_id| {
                let sw = inner.core.switch(sw_id)?;
                Some((sw.gw_v6?, sw.cidr_v6?.prefix_len()))
            });

            // Return route for public downstreams.
            let return_route = if router.uplink == Some(ix_sw)
                && router.cfg.downstream_pool == DownstreamPool::Public
            {
                if let (Some(cidr), Some(via)) = (router.downstream_cidr, router.upstream_ip) {
                    Some((cidr.addr(), cidr.prefix_len(), via))
                } else {
                    None
                }
            } else {
                None
            };
            let mut return_route_v6 = if router.uplink == Some(ix_sw) {
                // IX-level router: return route via this router's IX IP.
                if router.cfg.downstream_pool == DownstreamPool::Public {
                    if let (Some(cidr6), Some(via6)) =
                        (router.downstream_cidr_v6, router.upstream_ip_v6)
                    {
                        Some((cidr6.addr(), cidr6.prefix_len(), via6))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };

            // For sub-routers with NatV6Mode::None: add routes so that return
            // traffic for the sub-router's ULA subnet can reach it.
            let parent_route_v6 = if router.uplink.is_some()
                && router.uplink != Some(ix_sw)
                && router.cfg.nat_v6 == NatV6Mode::None
            {
                let uplink_sw = router.uplink.unwrap();
                let parent_id = inner.core.switch(uplink_sw).and_then(|sw| sw.owner_router);
                // Route in the parent router's ns: sub-router's LAN via sub-router's WAN IP.
                let parent_rt = if let (Some(cidr6), Some(via6), Some(ref owner_ns)) = (
                    router.downstream_cidr_v6,
                    router.upstream_ip_v6,
                    &upstream_owner_ns,
                ) {
                    Some((owner_ns.clone(), cidr6.addr(), cidr6.prefix_len(), via6))
                } else {
                    None
                };
                // Also need a root-ns route via the IX-level ancestor's IX IP.
                if parent_rt.is_some() {
                    if let Some(pid) = parent_id {
                        if let Some(parent_router) = inner.core.router(pid) {
                            if parent_router.uplink == Some(ix_sw) {
                                // Parent is IX-level; use its IX IP as the root-ns next-hop.
                                if let Some(parent_ix_v6) = parent_router.upstream_ip_v6 {
                                    if let Some(cidr6) = router.downstream_cidr_v6 {
                                        // Overwrite return_route_v6 for root ns
                                        return_route_v6 =
                                            Some((cidr6.addr(), cidr6.prefix_len(), parent_ix_v6));
                                    }
                                }
                            }
                        }
                    }
                }
                parent_rt
            } else {
                None
            };

            let has_v6 = router.cfg.ip_support.has_v6();
            let setup_data = RouterSetupData {
                router,
                root_ns: cfg.root_ns.clone(),
                prefix: cfg.prefix.clone(),
                ix_sw,
                ix_br: cfg.ix_br.clone(),
                ix_gw: cfg.ix_gw,
                ix_cidr_prefix: cfg.ix_cidr.prefix_len(),
                upstream_owner_ns,
                upstream_bridge,
                upstream_gw,
                upstream_cidr_prefix,
                return_route,
                downlink_bridge,
                ix_gw_v6: if has_v6 { Some(cfg.ix_gw_v6) } else { None },
                ix_cidr_v6_prefix: if has_v6 {
                    Some(cfg.ix_cidr_v6.prefix_len())
                } else {
                    None
                },
                upstream_gw_v6,
                upstream_cidr_prefix_v6,
                return_route_v6,
                downlink_bridge_v6,
                parent_route_v6,
            };

            let netns = Arc::clone(&inner.core.netns);
            let need_root = !inner.core.root_ns_initialized;
            (id, setup_data, netns, need_root)
        }; // lock released

        // Phase 2: Async network setup (no lock held).
        async {
            if need_root_setup {
                let cfg = {
                    let inner = self.inner.lock().unwrap();
                    inner.core.cfg.clone()
                };
                setup_root_ns_async(&cfg, &netns).await?;
                {
                    let mut inner = self.inner.lock().unwrap();
                    if !inner.core.root_ns_initialized {
                        inner.core.own_netns.push(cfg.root_ns.clone());
                        inner.core.root_ns_initialized = true;
                    }
                }
            }

            setup_router_async(&netns, &setup_data).await
        }
        .instrument(self.lab_span.clone())
        .await?;

        // Phase 3: Lock → bookkeeping → unlock.
        {
            let mut inner = self.inner.lock().unwrap();
            inner.core.own_netns.push(setup_data.router.ns.clone());
        }

        // Apply any pending region latency rules now that this router is ready.
        let lab_handle = Lab {
            inner: Arc::clone(&self.inner),
        };
        let _ = lab_handle.apply_region_latencies();

        let lab = Arc::clone(&self.inner);
        Ok(Router { id, lab })
    }
}

// ─────────────────────────────────────────────
// DeviceBuilder
// ─────────────────────────────────────────────

/// Builder for a device node; returned by [`Lab::add_device`].
pub struct DeviceBuilder {
    inner: Arc<Mutex<LabInner>>,
    lab_span: tracing::Span,
    id: NodeId,
    result: Result<()>,
}

impl DeviceBuilder {
    /// Attach `ifname` inside the device namespace to `router`'s downstream switch.
    pub fn iface(mut self, ifname: &str, router: NodeId, impair: Option<Impair>) -> Self {
        if self.result.is_ok() {
            self.result = self
                .inner
                .lock()
                .unwrap()
                .core
                .add_device_iface(self.id, ifname, router, impair)
                .map(|_| ());
        }
        self
    }

    /// Attach to `router`'s downstream switch with auto-named interfaces (eth0, eth1, ...).
    pub fn uplink(mut self, router: NodeId) -> Self {
        if self.result.is_ok() {
            let idx = {
                let inner = self.inner.lock().unwrap();
                inner
                    .core
                    .device(self.id)
                    .map(|d| d.interfaces.len())
                    .unwrap_or(0)
            };
            let ifname = format!("eth{}", idx);
            self.result = self
                .inner
                .lock()
                .unwrap()
                .core
                .add_device_iface(self.id, &ifname, router, None)
                .map(|_| ());
        }
        self
    }

    /// Overrides which interface carries the default route.
    pub fn default_via(mut self, ifname: &str) -> Self {
        if self.result.is_ok() {
            self.result = self
                .inner
                .lock()
                .unwrap()
                .core
                .set_device_default_via(self.id, ifname);
        }
        self
    }

    /// Finalizes the device, creates its namespace and links, and returns a [`Device`] handle.
    pub async fn build(self) -> Result<Device> {
        self.result?;

        // Phase 1: Lock → extract snapshot → unlock.
        let (dev, ifaces, prefix, root_ns, netns, need_root_setup) = {
            let inner = self.inner.lock().unwrap();
            let dev = inner
                .core
                .device(self.id)
                .ok_or_else(|| anyhow!("unknown device id"))?
                .clone();

            let mut iface_data = Vec::new();
            for iface in &dev.interfaces {
                let sw = inner.core.switch(iface.uplink).ok_or_else(|| {
                    anyhow!(
                        "device '{}' iface '{}' switch missing",
                        dev.name,
                        iface.ifname
                    )
                })?;
                let gw_router = sw.owner_router.ok_or_else(|| {
                    anyhow!(
                        "device '{}' iface '{}' switch missing owner",
                        dev.name,
                        iface.ifname
                    )
                })?;
                let gw_br = sw.bridge.clone().unwrap_or_else(|| "br-lan".to_string());
                let gw_ns = inner.core.router(gw_router).unwrap().ns.clone();
                iface_data.push(IfaceBuild {
                    dev_ns: dev.ns.clone(),
                    gw_ns,
                    gw_ip: sw.gw,
                    gw_br,
                    dev_ip: iface.ip,
                    prefix_len: sw.cidr.map(|c| c.prefix_len()).unwrap_or(24),
                    gw_ip_v6: sw.gw_v6,
                    dev_ip_v6: iface.ip_v6,
                    prefix_len_v6: sw.cidr_v6.map(|c| c.prefix_len()).unwrap_or(64),
                    impair: iface.impair,
                    ifname: iface.ifname.clone(),
                    is_default: iface.ifname == dev.default_via,
                    idx: iface.idx,
                });
            }

            let prefix = inner.core.cfg.prefix.clone();
            let root_ns = inner.core.cfg.root_ns.clone();
            let netns = Arc::clone(&inner.core.netns);
            let need_root = !inner.core.root_ns_initialized;
            (dev, iface_data, prefix, root_ns, netns, need_root)
        }; // lock released

        // Phase 2: Async network setup (no lock held).
        async {
            if need_root_setup {
                let cfg = {
                    let inner = self.inner.lock().unwrap();
                    inner.core.cfg.clone()
                };
                setup_root_ns_async(&cfg, &netns).await?;
                {
                    let mut inner = self.inner.lock().unwrap();
                    if !inner.core.root_ns_initialized {
                        inner.core.own_netns.push(cfg.root_ns.clone());
                        inner.core.root_ns_initialized = true;
                    }
                }
            }

            setup_device_async(&netns, &prefix, &root_ns, &dev, ifaces).await
        }
        .instrument(self.lab_span.clone())
        .await?;

        // Phase 3: Lock → bookkeeping → unlock.
        {
            let mut inner = self.inner.lock().unwrap();
            inner.core.own_netns.push(dev.ns.clone());
        }

        let lab = Arc::clone(&self.inner);
        Ok(Device { id: self.id, lab })
    }
}

// ─────────────────────────────────────────────
// Device / Router / DeviceIface handles
// ─────────────────────────────────────────────

/// Owned snapshot of a single device network interface.
///
/// Returned by [`Device::iface`], [`Device::default_iface`], and
/// [`Device::interfaces`]. This is a lightweight value type — no `Arc`.
#[derive(Clone, Debug)]
pub struct DeviceIface {
    ifname: String,
    ip: Ipv4Addr,
    ip_v6: Option<Ipv6Addr>,
    impair: Option<Impair>,
}

impl DeviceIface {
    /// Returns the interface name (e.g. `"eth0"`).
    pub fn name(&self) -> &str {
        &self.ifname
    }

    /// Returns the assigned IPv4 address.
    pub fn ip(&self) -> Ipv4Addr {
        self.ip
    }

    /// Returns the assigned IPv6 address, if any.
    pub fn ip6(&self) -> Option<Ipv6Addr> {
        self.ip_v6
    }

    /// Returns the impairment profile, if any.
    pub fn impair(&self) -> Option<Impair> {
        self.impair
    }
}

/// Cloneable handle to a device in the lab topology.
///
/// Holds a `NodeId` and an `Arc` to the lab interior. All accessor methods
/// briefly lock the mutex, read a value, and return owned data.
pub struct Device {
    id: NodeId,
    lab: Arc<Mutex<LabInner>>,
}

impl Clone for Device {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            lab: Arc::clone(&self.lab),
        }
    }
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device").field("id", &self.id).finish()
    }
}

impl Device {
    /// Returns the node identifier.
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// Returns the device name.
    pub fn name(&self) -> String {
        let inner = self.lab.lock().unwrap();
        inner
            .core
            .device(self.id)
            .map(|d| d.name.clone())
            .unwrap_or_default()
    }

    /// Returns the network namespace name for this device.
    pub fn ns(&self) -> String {
        let inner = self.lab.lock().unwrap();
        inner
            .core
            .device(self.id)
            .map(|d| d.ns.clone())
            .unwrap_or_default()
    }

    /// Returns the IP address of the default interface.
    pub fn ip(&self) -> Ipv4Addr {
        let inner = self.lab.lock().unwrap();
        inner
            .core
            .device(self.id)
            .and_then(|d| d.default_iface().ip)
            .unwrap_or(Ipv4Addr::UNSPECIFIED)
    }

    /// Returns the IPv6 address of the default interface, if assigned.
    pub fn ip6(&self) -> Option<Ipv6Addr> {
        let inner = self.lab.lock().unwrap();
        inner
            .core
            .device(self.id)
            .and_then(|d| d.default_iface().ip_v6)
    }

    /// Returns a snapshot of the named interface, if it exists.
    pub fn iface(&self, name: &str) -> Option<DeviceIface> {
        let inner = self.lab.lock().unwrap();
        let dev = inner.core.device(self.id)?;
        let iface = dev.iface(name)?;
        Some(DeviceIface {
            ifname: iface.ifname.clone(),
            ip: iface.ip.unwrap_or(Ipv4Addr::UNSPECIFIED),
            ip_v6: iface.ip_v6,
            impair: iface.impair,
        })
    }

    /// Returns a snapshot of the default interface.
    pub fn default_iface(&self) -> DeviceIface {
        let inner = self.lab.lock().unwrap();
        let dev = inner
            .core
            .device(self.id)
            .expect("device handle has valid id");
        let iface = dev.default_iface();
        DeviceIface {
            ifname: iface.ifname.clone(),
            ip: iface.ip.unwrap_or(Ipv4Addr::UNSPECIFIED),
            ip_v6: iface.ip_v6,
            impair: iface.impair,
        }
    }

    /// Returns snapshots of all interfaces.
    pub fn interfaces(&self) -> Vec<DeviceIface> {
        let inner = self.lab.lock().unwrap();
        let dev = match inner.core.device(self.id) {
            Some(d) => d,
            None => return vec![],
        };
        dev.interfaces
            .iter()
            .map(|iface| DeviceIface {
                ifname: iface.ifname.clone(),
                ip: iface.ip.unwrap_or(Ipv4Addr::UNSPECIFIED),
                ip_v6: iface.ip_v6,
                impair: iface.impair,
            })
            .collect()
    }

    // ── Dynamic operations ──────────────────────────────────────────────

    /// Brings an interface administratively down.
    pub async fn link_down(&self, ifname: &str) -> Result<()> {
        let (ns, netns) = {
            let inner = self.lab.lock().unwrap();
            let dev = inner
                .core
                .device(self.id)
                .ok_or_else(|| anyhow!("unknown device id"))?;
            (dev.ns.clone(), Arc::clone(&inner.core.netns))
        };
        let ifname = ifname.to_string();
        netns
            .spawn_netlink_task_in(
                &ns,
                move |nl| async move { nl.set_link_down(&ifname).await },
            )
            .await
            .map_err(|_| anyhow!("netns task cancelled"))?
    }

    /// Brings an interface administratively up.
    ///
    /// Linux removes routes via an interface when it goes admin-down, so we
    /// re-add the default route if `ifname` is the device's current `default_via`.
    pub async fn link_up(&self, ifname: &str) -> Result<()> {
        let (ns, uplink, is_default_via, netns) = {
            let inner = self.lab.lock().unwrap();
            let dev = inner
                .core
                .device(self.id)
                .ok_or_else(|| anyhow!("unknown device id"))?;
            let iface = dev
                .iface(ifname)
                .ok_or_else(|| anyhow!("interface '{}' not found", ifname))?;
            (
                dev.ns.clone(),
                iface.uplink,
                dev.default_via == ifname,
                Arc::clone(&inner.core.netns),
            )
        };
        let ifname_owned = ifname.to_string();
        netns
            .spawn_netlink_task_in(&ns, {
                let ifname_owned = ifname_owned.clone();
                move |nl| async move { nl.set_link_up(&ifname_owned).await }
            })
            .await
            .map_err(|_| anyhow!("netns task cancelled"))??;
        if is_default_via {
            let gw_ip = self
                .lab
                .lock()
                .unwrap()
                .core
                .router_downlink_gw_for_switch(uplink)?;
            netns
                .spawn_netlink_task_in(&ns, move |nl| async move {
                    nl.replace_default_route_v4(&ifname_owned, gw_ip).await
                })
                .await
                .map_err(|_| anyhow!("netns task cancelled"))??;
        }
        Ok(())
    }

    /// Switches the active default route to a different interface.
    pub async fn switch_route(&self, to: &str) -> Result<()> {
        let (ns, uplink, impair, netns) = {
            let inner = self.lab.lock().unwrap();
            let dev = inner
                .core
                .device(self.id)
                .ok_or_else(|| anyhow!("unknown device id"))?;
            let iface = dev
                .iface(to)
                .ok_or_else(|| anyhow!("interface '{}' not found", to))?;
            (
                dev.ns.clone(),
                iface.uplink,
                iface.impair,
                Arc::clone(&inner.core.netns),
            )
        };
        let gw_ip = self
            .lab
            .lock()
            .unwrap()
            .core
            .router_downlink_gw_for_switch(uplink)?;
        let to_owned = to.to_string();
        netns
            .spawn_netlink_task_in(&ns, move |nl| async move {
                nl.replace_default_route_v4(&to_owned, gw_ip).await
            })
            .await
            .map_err(|_| anyhow!("netns task cancelled"))??;
        match impair {
            Some(imp) => apply_impair_in(&ns, to, imp),
            None => crate::qdisc::remove_qdisc(&ns, to),
        }
        self.lab
            .lock()
            .unwrap()
            .core
            .set_device_default_via(self.id, to)?;
        Ok(())
    }

    /// Applies or removes a link-layer impairment on the named interface.
    ///
    /// If `ifname` is `None`, applies to the default interface.
    pub fn set_impair(&self, ifname: &str, impair: Option<Impair>) -> Result<()> {
        let mut inner = self.lab.lock().unwrap();
        let (ns, resolved_ifname) = {
            let dev = inner
                .core
                .device(self.id)
                .ok_or_else(|| anyhow!("unknown device id"))?;
            let iname = ifname.to_string();
            if dev.iface(&iname).is_none() {
                bail!("interface '{}' not found", iname);
            }
            (dev.ns.clone(), iname)
        };
        match impair {
            Some(imp) => apply_impair_in(&ns, &resolved_ifname, imp),
            None => crate::qdisc::remove_qdisc(&ns, &resolved_ifname),
        }
        if let Some(dev) = inner.core.device_mut(self.id) {
            if let Some(iface) = dev.iface_mut(&resolved_ifname) {
                iface.impair = impair;
            }
        }
        Ok(())
    }

    // ── Spawn ───────────────────────────────────────────────────────────

    /// Spawns an async task in this device's network namespace.
    ///
    /// The closure receives a cloned [`Device`] handle. Returns a
    /// `TaskHandle` that resolves to the task's output.
    pub fn spawn<F, Fut, T>(&self, f: F) -> crate::netns::TaskHandle<T>
    where
        F: FnOnce(Device) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = T> + 'static,
        T: Send + 'static,
    {
        let ns = {
            let inner = self.lab.lock().unwrap();
            inner
                .core
                .device(self.id)
                .expect("device handle has valid id")
                .ns
                .clone()
        };
        let handle = self.clone();
        crate::netns::spawn_task_in_netns(&ns, move || f(handle))
    }

    /// Spawns a raw command in this device's network namespace.
    pub fn spawn_command(&self, cmd: Command) -> Result<std::process::Child> {
        let ns = {
            let inner = self.lab.lock().unwrap();
            inner
                .core
                .device(self.id)
                .ok_or_else(|| anyhow!("unknown device id"))?
                .ns
                .clone()
        };
        spawn_command_in_namespace(&ns, cmd)
    }

    /// Probes the NAT mapping seen by a reflector from this device.
    pub fn probe_udp_mapping(&self, reflector: SocketAddr) -> Result<ObservedAddr> {
        let ns = self.ns();
        let base = 40000u16;
        let port = base + ((self.id.0 % 20000) as u16);
        crate::test_utils::probe_in_ns(&ns, reflector, Duration::from_millis(500), Some(port))
    }

    /// Spawns a UDP reflector in this device's network namespace.
    pub fn spawn_reflector(&self, bind: SocketAddr) -> Result<TaskHandle> {
        let ns = self.ns();
        let (handle, join) = crate::test_utils::spawn_reflector_in(&ns, bind)?;
        self.lab.lock().unwrap().children.push(ChildTask::Thread {
            handle: handle.clone(),
            join,
        });
        Ok(handle)
    }

    /// Moves one of this device's interfaces to a different router's downstream
    /// network, simulating a WiFi handoff or network switch.
    ///
    /// The interface name is preserved but the IP address changes (allocated from
    /// the new router's pool). The old veth pair is torn down and a fresh one is
    /// created.
    pub async fn switch_uplink(&self, ifname: &str, to_router: NodeId) -> Result<()> {
        use crate::core::{self, IfaceBuild};

        // Phase 1: Lock → extract data + allocate from new router's pool → unlock
        let (iface_build, old_idx, netns, prefix, root_ns) = {
            let mut inner = self.lab.lock().unwrap();
            let dev = inner
                .core
                .device(self.id)
                .ok_or_else(|| anyhow!("unknown device id"))?
                .clone();
            let iface = dev
                .interfaces
                .iter()
                .find(|i| i.ifname == ifname)
                .ok_or_else(|| anyhow!("device '{}' has no interface '{}'", dev.name, ifname))?;
            let old_idx = iface.idx;

            let target_router = inner
                .core
                .router(to_router)
                .ok_or_else(|| anyhow!("unknown target router id"))?
                .clone();
            let downlink_sw = target_router.downlink.ok_or_else(|| {
                anyhow!(
                    "target router '{}' has no downstream switch",
                    target_router.name
                )
            })?;
            let sw = inner
                .core
                .switch(downlink_sw)
                .ok_or_else(|| anyhow!("target router's downlink switch missing"))?
                .clone();
            let gw_br = sw.bridge.clone().unwrap_or_else(|| "br-lan".to_string());
            let new_ip = if sw.cidr.is_some() {
                Some(inner.core.alloc_from_switch(downlink_sw)?)
            } else {
                None
            };
            let new_ip_v6 = if sw.cidr_v6.is_some() {
                Some(inner.core.alloc_from_switch_v6(downlink_sw)?)
            } else {
                None
            };
            let prefix_len = sw.cidr.map(|c| c.prefix_len()).unwrap_or(24);

            let netns = Arc::clone(&inner.core.netns);
            let prefix = inner.core.cfg.prefix.clone();
            let root_ns = inner.core.cfg.root_ns.clone();

            let build = IfaceBuild {
                dev_ns: dev.ns.clone(),
                gw_ns: target_router.ns.clone(),
                gw_ip: sw.gw,
                gw_br,
                dev_ip: new_ip,
                prefix_len,
                gw_ip_v6: sw.gw_v6,
                dev_ip_v6: new_ip_v6,
                prefix_len_v6: sw.cidr_v6.map(|c| c.prefix_len()).unwrap_or(64),
                impair: iface.impair,
                ifname: ifname.to_string(),
                is_default: ifname == dev.default_via,
                idx: old_idx,
            };
            (build, old_idx, netns, prefix, root_ns)
        };

        // Phase 2: Delete old veth pair (from root NS)
        let old_root_gw = format!("{}g{}", prefix, old_idx);
        let old_root_dev = format!("{}e{}", prefix, old_idx);
        core::nl_run(&netns, &root_ns, {
            let old_root_gw = old_root_gw.clone();
            let old_root_dev = old_root_dev.clone();
            async move |h| {
                h.ensure_link_deleted(&old_root_gw).await.ok();
                h.ensure_link_deleted(&old_root_dev).await.ok();
                Ok(())
            }
        })
        .await?;

        // Phase 3: Wire new interface (reuses existing wiring logic)
        let new_ip = iface_build.dev_ip;
        let new_ip_v6 = iface_build.dev_ip_v6;
        let new_uplink = {
            let inner = self.lab.lock().unwrap();
            inner.core.router(to_router).unwrap().downlink.unwrap()
        };
        core::wire_iface_async(&netns, &prefix, &root_ns, iface_build).await?;

        // Phase 4: Lock → update internal records → unlock
        {
            let mut inner = self.lab.lock().unwrap();
            let dev = inner
                .core
                .device_mut(self.id)
                .ok_or_else(|| anyhow!("device disappeared"))?;
            if let Some(iface) = dev.interfaces.iter_mut().find(|i| i.ifname == ifname) {
                iface.uplink = new_uplink;
                iface.ip = new_ip;
                iface.ip_v6 = new_ip_v6;
            }
        }

        Ok(())
    }
}

/// Cloneable handle to a router in the lab topology.
///
/// Same pattern as [`Device`]: holds `NodeId` + `Arc<Mutex<LabInner>>`.
pub struct Router {
    id: NodeId,
    lab: Arc<Mutex<LabInner>>,
}

impl Clone for Router {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            lab: Arc::clone(&self.lab),
        }
    }
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router").field("id", &self.id).finish()
    }
}

impl Router {
    /// Returns the node identifier.
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// Returns the router name.
    pub fn name(&self) -> String {
        let inner = self.lab.lock().unwrap();
        inner
            .core
            .router(self.id)
            .map(|r| r.name.clone())
            .unwrap_or_default()
    }

    /// Returns the network namespace name for this router.
    pub fn ns(&self) -> String {
        let inner = self.lab.lock().unwrap();
        inner
            .core
            .router(self.id)
            .map(|r| r.ns.clone())
            .unwrap_or_default()
    }

    /// Returns the region label, if set.
    pub fn region(&self) -> Option<String> {
        let inner = self.lab.lock().unwrap();
        inner.core.router(self.id).and_then(|r| r.region.clone())
    }

    /// Returns the NAT mode.
    pub fn nat_mode(&self) -> NatMode {
        let inner = self.lab.lock().unwrap();
        inner
            .core
            .router(self.id)
            .map(|r| r.cfg.nat)
            .unwrap_or_default()
    }

    /// Returns the uplink (WAN-side) IP, if connected.
    pub fn uplink_ip(&self) -> Option<Ipv4Addr> {
        let inner = self.lab.lock().unwrap();
        inner.core.router(self.id).and_then(|r| r.upstream_ip)
    }

    /// Returns the downstream subnet CIDR, if allocated.
    pub fn downstream_cidr(&self) -> Option<Ipv4Net> {
        let inner = self.lab.lock().unwrap();
        inner.core.router(self.id).and_then(|r| r.downstream_cidr)
    }

    /// Returns the downstream gateway address, if allocated.
    pub fn downstream_gw(&self) -> Option<Ipv4Addr> {
        let inner = self.lab.lock().unwrap();
        inner.core.router(self.id).and_then(|r| r.downstream_gw)
    }

    /// Returns which IP address families this router supports.
    pub fn ip_support(&self) -> IpSupport {
        let inner = self.lab.lock().unwrap();
        inner
            .core
            .router(self.id)
            .map(|r| r.cfg.ip_support)
            .unwrap_or_default()
    }

    /// Returns the uplink (WAN-side) IPv6 address, if connected.
    pub fn uplink_ip_v6(&self) -> Option<Ipv6Addr> {
        let inner = self.lab.lock().unwrap();
        inner.core.router(self.id).and_then(|r| r.upstream_ip_v6)
    }

    /// Returns the downstream IPv6 subnet CIDR, if allocated.
    pub fn downstream_cidr_v6(&self) -> Option<Ipv6Net> {
        let inner = self.lab.lock().unwrap();
        inner
            .core
            .router(self.id)
            .and_then(|r| r.downstream_cidr_v6)
    }

    /// Returns the downstream IPv6 gateway address, if allocated.
    pub fn downstream_gw_v6(&self) -> Option<Ipv6Addr> {
        let inner = self.lab.lock().unwrap();
        inner.core.router(self.id).and_then(|r| r.downstream_gw_v6)
    }

    /// Returns the IPv6 NAT mode.
    pub fn nat_v6_mode(&self) -> NatV6Mode {
        let inner = self.lab.lock().unwrap();
        inner
            .core
            .router(self.id)
            .map(|r| r.cfg.nat_v6)
            .unwrap_or_default()
    }

    // ── Dynamic operations ──────────────────────────────────────────────

    /// Replaces NAT rules on this router at runtime.
    ///
    /// Flushes the `ip nat` table then re-applies the new rules.
    pub async fn set_nat_mode(&self, mode: NatMode) -> Result<()> {
        let (ns, lan_if, wan_if, wan_ip) =
            self.lab.lock().unwrap().core.router_nat_params(self.id)?;
        run_nft_in(&ns, "flush table ip nat").await.ok();
        apply_nat(&ns, mode, &lan_if, &wan_if, wan_ip).await?;
        self.lab
            .lock()
            .unwrap()
            .core
            .set_router_nat_mode(self.id, mode)
    }

    /// Replaces IPv6 NAT rules on this router at runtime.
    pub async fn set_nat_v6_mode(&self, mode: NatV6Mode) -> Result<()> {
        let (ns, wan_if, lan_prefix, wan_prefix) = {
            let inner = self.lab.lock().unwrap();
            let router = inner
                .core
                .router(self.id)
                .ok_or_else(|| anyhow!("unknown router id"))?;
            let wan_if = router.wan_ifname(inner.core.ix_sw()).to_string();
            let lan_prefix = router
                .downstream_cidr_v6
                .unwrap_or_else(|| "fd10::/64".parse().unwrap());
            let wan_prefix = {
                let up_ip = router.upstream_ip_v6.unwrap_or(Ipv6Addr::UNSPECIFIED);
                let up_prefix = if router.uplink == Some(inner.core.ix_sw()) {
                    inner.core.cfg.ix_cidr_v6.prefix_len()
                } else {
                    router
                        .uplink
                        .and_then(|sw| inner.core.switch(sw))
                        .and_then(|sw| sw.cidr_v6)
                        .map(|c| c.prefix_len())
                        .unwrap_or(64)
                };
                Ipv6Net::new(up_ip, up_prefix).unwrap_or_else(|_| Ipv6Net::new(up_ip, 128).unwrap())
            };
            (router.ns.clone(), wan_if, lan_prefix, wan_prefix)
        };
        run_nft_in(&ns, "flush table ip6 nat").await.ok();
        apply_nat_v6(&ns, mode, &wan_if, lan_prefix, wan_prefix).await?;
        {
            let mut inner = self.lab.lock().unwrap();
            let router = inner
                .core
                .router_mut(self.id)
                .ok_or_else(|| anyhow!("unknown router id"))?;
            router.cfg.nat_v6 = mode;
        }
        Ok(())
    }

    /// Flushes the conntrack table, forcing all active NAT mappings to expire.
    ///
    /// Subsequent flows get new external port assignments. Requires `conntrack-tools`.
    pub fn rebind_nats(&self) -> Result<()> {
        let ns = self
            .lab
            .lock()
            .unwrap()
            .core
            .router_ns(self.id)?
            .to_string();
        run_closure_in_namespace(&ns, || {
            let st = std::process::Command::new("conntrack").arg("-F").status()?;
            if !st.success() {
                bail!("conntrack -F failed: {st}");
            }
            Ok(())
        })
    }

    // ── Spawn ───────────────────────────────────────────────────────────

    /// Spawns an async task in this router's network namespace.
    ///
    /// The closure receives a cloned [`Router`] handle.
    pub fn spawn<F, Fut, T>(&self, f: F) -> crate::netns::TaskHandle<T>
    where
        F: FnOnce(Router) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = T> + 'static,
        T: Send + 'static,
    {
        let ns = {
            let inner = self.lab.lock().unwrap();
            inner
                .core
                .router(self.id)
                .expect("router handle has valid id")
                .ns
                .clone()
        };
        let handle = self.clone();
        crate::netns::spawn_task_in_netns(&ns, move || f(handle))
    }

    /// Spawns a raw command in this router's network namespace.
    pub fn spawn_command(&self, cmd: Command) -> Result<std::process::Child> {
        let ns = {
            let inner = self.lab.lock().unwrap();
            inner
                .core
                .router(self.id)
                .ok_or_else(|| anyhow!("unknown router id"))?
                .ns
                .clone()
        };
        spawn_command_in_namespace(&ns, cmd)
    }

    /// Spawns a UDP reflector in this router's network namespace.
    pub fn spawn_reflector(&self, bind: SocketAddr) -> Result<TaskHandle> {
        let ns = self.ns();
        let (handle, join) = crate::test_utils::spawn_reflector_in(&ns, bind)?;
        self.lab.lock().unwrap().children.push(ChildTask::Thread {
            handle: handle.clone(),
            join,
        });
        Ok(handle)
    }
}

// ─────────────────────────────────────────────
// Lab lookup methods
// ─────────────────────────────────────────────

impl Lab {
    /// Returns a device handle by id, or `None` if the id is not a device.
    pub fn device(&self, id: NodeId) -> Option<Device> {
        let inner = self.inner.lock().unwrap();
        inner.core.device(id).map(|_| Device {
            id,
            lab: Arc::clone(&self.inner),
        })
    }

    /// Returns a router handle by id, or `None` if the id is not a router.
    pub fn router(&self, id: NodeId) -> Option<Router> {
        let inner = self.inner.lock().unwrap();
        inner.core.router(id).map(|_| Router {
            id,
            lab: Arc::clone(&self.inner),
        })
    }

    /// Looks up a device by name and returns a handle.
    pub fn device_by_name(&self, name: &str) -> Option<Device> {
        let inner = self.inner.lock().unwrap();
        inner.core.device_id_by_name(name).map(|id| Device {
            id,
            lab: Arc::clone(&self.inner),
        })
    }

    /// Looks up a router by name and returns a handle.
    pub fn router_by_name(&self, name: &str) -> Option<Router> {
        let inner = self.inner.lock().unwrap();
        inner.core.router_id_by_name(name).map(|id| Router {
            id,
            lab: Arc::clone(&self.inner),
        })
    }

    /// Returns handles for all devices.
    pub fn devices(&self) -> Vec<Device> {
        let inner = self.inner.lock().unwrap();
        inner
            .core
            .all_device_ids()
            .into_iter()
            .map(|id| Device {
                id,
                lab: Arc::clone(&self.inner),
            })
            .collect()
    }

    /// Returns handles for all routers.
    pub fn routers(&self) -> Vec<Router> {
        let inner = self.inner.lock().unwrap();
        inner
            .core
            .all_router_ids()
            .into_iter()
            .map(|id| Router {
                id,
                lab: Arc::clone(&self.inner),
            })
            .collect()
    }
}

// ─────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────

/// Normalise a device/interface name for use in an environment variable name.
fn normalize_env_name(s: &str) -> String {
    s.to_uppercase().replace('-', "_")
}
