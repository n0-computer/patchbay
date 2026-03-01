//! High-level lab API: [`Lab`], [`DeviceBuilder`], [`Nat`], [`LinkCondition`], [`ObservedAddr`].

use std::{
    collections::HashMap,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::Path,
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use anyhow::{anyhow, bail, Context, Result};
use ipnet::{Ipv4Net, Ipv6Net};
use serde::Deserialize;
use tracing::{debug, debug_span, Instrument as _};

use crate::{
    core::{
        self, apply_or_remove_impair, setup_device_async, setup_root_ns_async, setup_router_async,
        CoreConfig, DownstreamPool, IfaceBuild, LabInner, NetworkCore, NodeId, RouterSetupData,
    },
    netlink::Netlink,
};

pub use crate::qdisc::LinkLimits;

pub(crate) static LAB_COUNTER: AtomicU64 = AtomicU64::new(0);

// ── address construction helpers ─────────────────────────────────────

/// Constructs a /`prefix` network from components, e.g. `net4(198, 18, 0, 0, 24)`.
fn net4(a: u8, b: u8, c: u8, d: u8, prefix: u8) -> Ipv4Net {
    Ipv4Net::new(Ipv4Addr::new(a, b, c, d), prefix).expect("valid prefix len")
}

pub(crate) fn net6(addr: Ipv6Addr, prefix: u8) -> Ipv6Net {
    Ipv6Net::new(addr, prefix).expect("valid prefix len")
}

/// Base address for a region's /20 block: `198.18.{idx*16}.0`.
fn region_base(idx: u8) -> Ipv4Addr {
    Ipv4Addr::new(
        198,
        18,
        idx.checked_mul(16).expect("region idx overflow"),
        0,
    )
}

pub use crate::firewall::{Firewall, FirewallConfig, FirewallConfigBuilder};
pub use crate::handles::{Device, DeviceIface, Ix, Router};
pub use crate::nat::{
    ConntrackTimeouts, IpSupport, Nat, NatConfig, NatConfigBuilder, NatFiltering, NatMapping,
    NatV6Mode,
};

/// Link-layer impairment profile applied via `tc netem`.
///
/// Named presets model common last-mile conditions. Use [`LinkCondition::Manual`]
/// with [`LinkLimits`] for full control over all `tc netem` parameters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LinkCondition {
    /// Wired LAN (1G Ethernet). No impairment.
    ///
    /// Use for datacenter-local, same-rack communication.
    Lan,
    /// Good WiFi — 5 GHz band, close to AP, low contention.
    ///
    /// 5 ms one-way delay, 2 ms jitter, 0.1 % loss.
    Wifi,
    /// Congested WiFi — 2.4 GHz, far from AP, interference.
    ///
    /// 40 ms one-way delay, 15 ms jitter, 2 % loss, 20 Mbit.
    WifiBad,
    /// 4G/LTE good signal.
    ///
    /// 25 ms one-way delay, 8 ms jitter, 0.5 % loss.
    Mobile4G,
    /// 3G or degraded 4G.
    ///
    /// 100 ms one-way delay, 30 ms jitter, 2 % loss, 2 Mbit.
    Mobile3G,
    /// LEO satellite (Starlink-class).
    ///
    /// 40 ms one-way delay, 7 ms jitter, 1 % loss.
    Satellite,
    /// GEO satellite (HughesNet/Viasat).
    ///
    /// 300 ms one-way delay, 20 ms jitter, 0.5 % loss, 25 Mbit.
    SatelliteGeo,
    /// Fully custom impairment parameters.
    Manual(LinkLimits),
}

impl<'de> Deserialize<'de> for LinkCondition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Preset(String),
            Manual(LinkLimits),
        }

        match Repr::deserialize(deserializer)? {
            Repr::Preset(s) => match s.as_str() {
                "lan" => Ok(LinkCondition::Lan),
                "wifi" => Ok(LinkCondition::Wifi),
                "wifi-bad" => Ok(LinkCondition::WifiBad),
                "mobile-4g" | "mobile" => Ok(LinkCondition::Mobile4G),
                "mobile-3g" => Ok(LinkCondition::Mobile3G),
                "satellite" => Ok(LinkCondition::Satellite),
                "satellite-geo" => Ok(LinkCondition::SatelliteGeo),
                _ => Err(serde::de::Error::custom(format!(
                    "unknown link condition preset '{s}'"
                ))),
            },
            Repr::Manual(limits) => Ok(LinkCondition::Manual(limits)),
        }
    }
}

impl LinkCondition {
    /// Converts this preset (or manual config) into concrete [`LinkLimits`].
    pub fn to_limits(self) -> LinkLimits {
        match self {
            LinkCondition::Lan => LinkLimits::default(),
            LinkCondition::Wifi => LinkLimits {
                latency_ms: 5,
                jitter_ms: 2,
                loss_pct: 0.1,
                ..Default::default()
            },
            LinkCondition::WifiBad => LinkLimits {
                latency_ms: 40,
                jitter_ms: 15,
                loss_pct: 2.0,
                rate_kbit: 20_000,
                ..Default::default()
            },
            LinkCondition::Mobile4G => LinkLimits {
                latency_ms: 25,
                jitter_ms: 8,
                loss_pct: 0.5,
                ..Default::default()
            },
            LinkCondition::Mobile3G => LinkLimits {
                latency_ms: 100,
                jitter_ms: 30,
                loss_pct: 2.0,
                rate_kbit: 2_000,
                ..Default::default()
            },
            LinkCondition::Satellite => LinkLimits {
                latency_ms: 40,
                jitter_ms: 7,
                loss_pct: 1.0,
                ..Default::default()
            },
            LinkCondition::SatelliteGeo => LinkLimits {
                latency_ms: 300,
                jitter_ms: 20,
                loss_pct: 0.5,
                rate_kbit: 25_000,
                ..Default::default()
            },
            LinkCondition::Manual(limits) => limits,
        }
    }
}

/// Observed external address as reported by a STUN-like UDP reflector.
///
/// This is the `ip:port` pair that the reflector sees after NAT translation.
/// Alias for [`SocketAddr`].
pub type ObservedAddr = SocketAddr;

// ─────────────────────────────────────────────
// Region
// ─────────────────────────────────────────────

/// Handle for a network region backed by a real router namespace.
///
/// Regions model geographic proximity: routers within a region share a bridge,
/// and inter-region traffic flows over veths with configurable netem impairment.
#[derive(Clone)]
pub struct Region {
    name: String,
    idx: u8,
    router_id: NodeId,
}

impl Region {
    /// Region name (e.g. "us", "eu").
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The underlying region router's node ID.
    pub fn router_id(&self) -> NodeId {
        self.router_id
    }
}

/// Parameters for an inter-region link passed to [`Lab::link_regions`].
#[derive(Clone, Debug)]
pub struct RegionLink {
    /// One-way latency in milliseconds (RTT = 2x).
    pub latency_ms: u32,
    /// Jitter in milliseconds (uniform distribution around `latency_ms`).
    pub jitter_ms: u32,
    /// Packet loss percentage (0.0–100.0).
    pub loss_pct: f64,
    /// Rate limit in Mbit/s (0 = unlimited).
    pub rate_mbit: u32,
}

impl RegionLink {
    /// Good inter-region link: only latency, no jitter or loss.
    pub fn good(latency_ms: u32) -> Self {
        Self {
            latency_ms,
            jitter_ms: 0,
            loss_pct: 0.0,
            rate_mbit: 0,
        }
    }

    /// Degraded link: jitter = latency/10, 0.5% loss, no rate limit.
    pub fn degraded(latency_ms: u32) -> Self {
        Self {
            latency_ms,
            jitter_ms: latency_ms / 10,
            loss_pct: 0.5,
            rate_mbit: 0,
        }
    }
}

/// Pre-built regions from [`Lab::add_default_regions`].
pub struct DefaultRegions {
    /// US region (198.18.0.0/20).
    pub us: Region,
    /// EU region (198.18.16.0/20).
    pub eu: Region,
    /// Asia region (198.18.32.0/20).
    pub asia: Region,
}

// ─────────────────────────────────────────────
// Lab
// ─────────────────────────────────────────────

/// High-level lab API built on top of `NetworkCore`.
///
/// `Lab` wraps `Arc<LabInner>` and is cheaply cloneable. All methods
/// take `&self` and use interior mutability through the mutex.
#[derive(Clone)]
pub struct Lab {
    pub(crate) inner: Arc<LabInner>,
}

impl Lab {
    // ── Constructors ────────────────────────────────────────────────────

    /// Creates a new lab with default address ranges and IX settings.
    ///
    /// Sets up the root network namespace and IX bridge before returning.
    pub async fn new() -> Result<Self> {
        let pid = std::process::id();
        let pid_tag = pid % 9999 + 1;
        let lab_seq = LAB_COUNTER.fetch_add(1, Ordering::Relaxed);
        let uniq = format!("{lab_seq:x}");
        let prefix = format!("lab-p{}{}", pid_tag, uniq); // e.g. "lab-p12340"
        let root_ns = format!("lab{lab_seq}-root");
        let bridge_tag = format!("p{}{}", pid_tag, uniq);
        let ix_gw = Ipv4Addr::new(198, 18, 0, 1);
        let lab_span = debug_span!("lab", id = lab_seq);
        {
            let _enter = lab_span.enter();
            debug!(prefix = %prefix, "lab: created");
        }
        let core = NetworkCore::new(CoreConfig {
            lab_id: lab_seq,
            prefix: prefix.clone(),
            root_ns,
            bridge_tag,
            ix_br: format!("br-p{}{}-1", pid_tag, uniq),
            ix_gw,
            ix_cidr: net4(198, 18, 0, 0, 24),
            private_cidr: net4(10, 0, 0, 0, 16),
            public_cidr: net4(198, 18, 1, 0, 24),
            ix_gw_v6: Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1),
            ix_cidr_v6: net6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
            private_cidr_v6: net6(Ipv6Addr::new(0xfd10, 0, 0, 0, 0, 0, 0, 0), 48),
            span: lab_span,
        })
        .context("failed to create DNS entries directory")?;
        let netns = Arc::new(crate::netns::NetnsManager::new());
        let cancel = tokio_util::sync::CancellationToken::new();
        let lab = Self {
            inner: Arc::new(LabInner {
                core: std::sync::Mutex::new(core),
                netns: Arc::clone(&netns),
                cancel,
            }),
        };
        // Initialize root namespace and IX bridge eagerly — no lazy-init race.
        let cfg = lab.inner.core.lock().unwrap().cfg.clone();
        setup_root_ns_async(&cfg, &netns)
            .await
            .context("failed to set up root namespace")?;
        Ok(lab)
    }

    /// Returns the unique resource prefix associated with this lab instance.
    pub fn prefix(&self) -> String {
        self.inner.core.lock().unwrap().cfg.prefix.clone()
    }

    /// Parses `lab.toml`, builds the network, and returns a ready-to-use lab.
    pub async fn load(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path).context("read lab config")?;
        let cfg: crate::config::LabConfig = toml::from_str(&text).context("parse lab config")?;
        Self::from_config(cfg).await
    }

    /// Builds a `Lab` from a parsed config, creating all namespaces and links.
    pub async fn from_config(cfg: crate::config::LabConfig) -> Result<Self> {
        let lab = Self::new().await?;

        // Region latency pairs from TOML config are ignored in the new region API.
        // TODO: support regions in TOML config via add_region / link_regions.

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
                    let inner = lab.inner.core.lock().unwrap();
                    rcfg.upstream
                        .as_deref()
                        .and_then(|n| inner.router_id_by_name(n))
                };
                let mut rb = lab
                    .add_router(&rcfg.name)
                    .nat(rcfg.nat)
                    .ip_support(rcfg.ip_support)
                    .nat_v6(rcfg.nat_v6);
                // TODO: support region assignment from TOML config via add_region.
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
            ifaces: Vec<(String, NodeId, Option<LinkCondition>)>,
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
                        .core
                        .lock()
                        .unwrap()
                        .router_id_by_name(gw_name)
                        .ok_or_else(|| {
                            anyhow!(
                                "device '{}' iface '{}' references unknown router '{}'",
                                dev_name,
                                ifname,
                                gw_name
                            )
                        })?;
                    let impair: Option<LinkCondition> = match iface_table.get("impair") {
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

        Ok(lab)
    }

    // ── Builder methods (sync — just populate data structures) ──────────

    /// Begins building a router; returns a [`RouterBuilder`] to configure options.
    ///
    /// Call [`.nat()`][RouterBuilder::nat], [`.region()`][RouterBuilder::region], and/or
    /// [`.upstream()`][RouterBuilder::upstream] as needed, then
    /// [`.build()`][RouterBuilder::build] to finalize.
    ///
    /// Default NAT mode is [`Nat::None`] (public DC-style router, IX-connected).
    pub fn add_router(&self, name: &str) -> RouterBuilder {
        let inner = self.inner.core.lock().unwrap();
        let lab_span = inner.cfg.span.clone();
        if name.starts_with("region_") {
            return RouterBuilder::error(
                Arc::clone(&self.inner),
                lab_span,
                name,
                anyhow!("router names starting with 'region_' are reserved"),
            );
        }
        if inner.router_id_by_name(name).is_some() {
            return RouterBuilder::error(
                Arc::clone(&self.inner),
                lab_span,
                name,
                anyhow!("router '{}' already exists", name),
            );
        }
        RouterBuilder {
            inner: Arc::clone(&self.inner),
            lab_span,
            name: name.to_string(),
            region: None,
            upstream: None,
            nat: Nat::None,
            ip_support: IpSupport::V4Only,
            nat_v6: NatV6Mode::None,
            downstream_cidr: None,
            downlink_condition: None,
            mtu: None,
            block_icmp_frag_needed: false,
            firewall: Firewall::None,
            result: Ok(()),
        }
    }

    /// Begins building a device; returns a [`DeviceBuilder`] to configure interfaces.
    ///
    /// Call [`.iface()`][DeviceBuilder::iface] one or more times to attach network
    /// interfaces, then [`.build()`][DeviceBuilder::build] to finalize.
    pub fn add_device(&self, name: &str) -> DeviceBuilder {
        let mut inner = self.inner.core.lock().unwrap();
        let lab_span = inner.cfg.span.clone();
        if inner.device_id_by_name(name).is_some() {
            return DeviceBuilder {
                inner: Arc::clone(&self.inner),
                lab_span,
                id: NodeId(u64::MAX),
                mtu: None,
                result: Err(anyhow!("device '{}' already exists", name)),
            };
        }
        let id = inner.add_device(name);
        DeviceBuilder {
            inner: Arc::clone(&self.inner),
            lab_span,
            id,
            mtu: None,
            result: Ok(()),
        }
    }

    // ── removal ──────────────────────────────────────────────────────────

    /// Removes a device from the lab, destroying its namespace and all interfaces.
    ///
    /// The kernel automatically destroys veth pairs when the namespace closes.
    pub fn remove_device(&self, id: NodeId) -> Result<()> {
        let ns = {
            let mut inner = self.inner.core.lock().unwrap();
            let dev = inner
                .device(id)
                .ok_or_else(|| anyhow!("unknown device id {:?}", id))?;
            let ns = dev.ns.clone();
            inner.remove_device(id);
            ns
        };
        self.inner.netns.remove_worker(&ns);
        Ok(())
    }

    /// Removes a router from the lab, destroying its namespace and all interfaces.
    ///
    /// Fails if any devices are still connected to this router's downstream switch.
    /// Remove or replug those devices first.
    pub fn remove_router(&self, id: NodeId) -> Result<()> {
        let ns = {
            let mut inner = self.inner.core.lock().unwrap();
            let router = inner
                .router(id)
                .ok_or_else(|| anyhow!("unknown router id {:?}", id))?;
            let ns = router.ns.clone();

            // Check that no devices are connected to this router's downstream switch.
            if let Some(sw_id) = router.downlink {
                for dev in inner.all_devices() {
                    for iface in &dev.interfaces {
                        if iface.uplink == sw_id {
                            bail!(
                                "cannot remove router '{}': device '{}' is still connected",
                                router.name,
                                dev.name
                            );
                        }
                    }
                }
            }

            inner.remove_router(id);
            ns
        };
        self.inner.netns.remove_worker(&ns);
        Ok(())
    }

    // ── build ────────────────────────────────────────────────────────────

    // ── User-facing API ─────────────────────────────────────────────────

    // ── Region API ────────────────────────────────────────────────────

    /// Creates a new network region backed by a real router namespace.
    ///
    /// Each region gets a /20 block from 198.18.0.0/15. Routers added with
    /// `.region(&region)` connect to the region's bridge as sub-routers.
    /// Inter-region latency is configured separately via [`link_regions`](Self::link_regions).
    pub async fn add_region(&self, name: &str) -> Result<Region> {
        if name.is_empty() {
            bail!("region name must not be empty");
        }
        let region_router_name = format!("region_{name}");

        // Phase 1: Lock → register topology → unlock.
        let (id, setup_data, idx) = {
            let mut inner = self.inner.core.lock().unwrap();
            if inner.regions.contains_key(name) {
                bail!("region '{name}' already exists");
            }
            let idx = inner.alloc_region_idx()?;

            // Region router: Nat::None, public downstream, no region tag (it IS the region).
            let id = inner.add_router(
                &region_router_name,
                Nat::None,
                DownstreamPool::Public,
                None,
                IpSupport::V4Only,
                NatV6Mode::None,
            );

            // Downstream switch: region's first /24 as override CIDR.
            let region_bridge_cidr = net4(198, 18, idx * 16, 0, 24);
            let sub_switch =
                inner.add_switch(&format!("{region_router_name}-sub"), None, None, None, None);
            inner.connect_router_downlink(id, sub_switch, Some(region_bridge_cidr))?;

            // Set next_host to 10 so sub-routers get .10, .11, ...
            if let Some(sw) = inner.switch_mut(sub_switch) {
                sw.next_host = 10;
            }

            // IX uplink: region router gets an IX IP.
            let ix_ip = inner.alloc_ix_ip_low()?;
            let ix_sw = inner.ix_sw();
            inner.connect_router_uplink(id, ix_sw, Some(ix_ip), None)?;

            // Store region info.
            inner.regions.insert(
                name.to_string(),
                crate::core::RegionInfo {
                    idx,
                    router_id: id,
                    next_downstream: 1,
                },
            );

            // Extract snapshot for async setup.
            let router = inner.router(id).unwrap().clone();
            let cfg = &inner.cfg;
            let ix_sw_id = inner.ix_sw();

            // Region router has a return route for its bridge /24 via its IX IP.
            // But it also needs the /20 aggregate in root NS.
            // The per-/24 return route for the bridge subnet is handled by the
            // standard return_route mechanism.
            let return_route =
                if let (Some(cidr), Some(via)) = (router.downstream_cidr, router.upstream_ip) {
                    Some((cidr.addr(), cidr.prefix_len(), via))
                } else {
                    None
                };

            let downlink_bridge = router.downlink.and_then(|sw_id| {
                let sw = inner.switch(sw_id)?;
                let br = sw.bridge.clone().unwrap_or_else(|| "br-lan".to_string());
                let v4 = sw.gw.and_then(|gw| Some((gw, sw.cidr?.prefix_len())));
                Some((br, v4))
            });

            let setup_data = RouterSetupData {
                router,
                root_ns: cfg.root_ns.clone(),
                prefix: cfg.prefix.clone(),
                ix_sw: ix_sw_id,
                ix_br: cfg.ix_br.clone(),
                ix_gw: cfg.ix_gw,
                ix_cidr_prefix: cfg.ix_cidr.prefix_len(),
                upstream_owner_ns: None,
                upstream_bridge: None,
                upstream_gw: None,
                upstream_cidr_prefix: None,
                return_route,
                downlink_bridge,
                ix_gw_v6: None,
                ix_cidr_v6_prefix: None,
                upstream_gw_v6: None,
                upstream_cidr_prefix_v6: None,
                return_route_v6: None,
                downlink_bridge_v6: None,
                parent_route_v6: None,
                parent_route_v4: None,
            };

            (id, setup_data, idx)
        }; // lock released

        // Phase 2: Async network setup (no lock held).
        let netns = &self.inner.netns;
        setup_router_async(netns, &setup_data).await?;

        // Phase 3: Add /20 aggregate route in root NS for the region.
        let region_net = region_base(idx);
        let via = setup_data
            .router
            .upstream_ip
            .context("region router has no IX IP")?;
        let root_ns = setup_data.root_ns.clone();
        core::nl_run(netns, &root_ns, move |h: Netlink| async move {
            h.add_route_v4(region_net, 20, via).await.ok();
            Ok(())
        })
        .await?;

        Ok(Region {
            name: name.to_string(),
            idx,
            router_id: id,
        })
    }

    /// Links two regions with a veth pair and applies netem impairment.
    ///
    /// Creates a point-to-point veth between the two region router namespaces,
    /// assigns /30 addresses from 203.0.113.0/24, applies tc netem on both ends,
    /// and adds /20 routes so each region can reach the other.
    pub async fn link_regions(&self, a: &Region, b: &Region, link: RegionLink) -> Result<()> {
        let (a_ns, b_ns, a_idx, b_idx, root_ns);
        let (ip_a, ip_b);
        let link_key;
        {
            let mut inner = self.inner.core.lock().unwrap();

            // Validate regions exist and aren't already linked.
            let a_name = a.name.clone();
            let b_name = b.name.clone();
            link_key = if a_name < b_name {
                (a_name.clone(), b_name.clone())
            } else {
                (b_name.clone(), a_name.clone())
            };
            if inner.region_links.contains_key(&link_key) {
                bail!("regions '{}' and '{}' are already linked", a.name, b.name);
            }

            let a_info = inner
                .regions
                .get(&a.name)
                .ok_or_else(|| anyhow!("region '{}' not found", a.name))?
                .clone();
            let b_info = inner
                .regions
                .get(&b.name)
                .ok_or_else(|| anyhow!("region '{}' not found", b.name))?
                .clone();

            a_ns = inner.router(a_info.router_id).unwrap().ns.clone();
            b_ns = inner.router(b_info.router_id).unwrap().ns.clone();
            a_idx = a_info.idx;
            b_idx = b_info.idx;
            root_ns = inner.cfg.root_ns.clone();

            // Allocate /30 from 203.0.113.0/24.
            let (ipa, ipb) = inner.alloc_interregion_ips()?;
            ip_a = ipa;
            ip_b = ipb;

            // Store IPs in sorted key order: ip_a belongs to link_key.0, ip_b to link_key.1.
            let (stored_ip_a, stored_ip_b) = if a.name < b.name {
                (ip_a, ip_b)
            } else {
                (ip_b, ip_a)
            };
            inner.region_links.insert(
                link_key.clone(),
                crate::core::RegionLinkData {
                    ip_a: stored_ip_a,
                    ip_b: stored_ip_b,
                    broken: false,
                },
            );
        } // lock released

        let netns = &self.inner.netns;
        let veth_a = format!("vr-{}-{}", a.name, b.name);
        let veth_b = format!("vr-{}-{}", b.name, a.name);

        // Create veth pair in root NS, then move ends to region router NSes.
        let veth_a2 = veth_a.clone();
        let veth_b2 = veth_b.clone();
        let a_ns_fd = netns.ns_fd(&a_ns)?;
        let b_ns_fd = netns.ns_fd(&b_ns)?;
        core::nl_run(netns, &root_ns, move |h: Netlink| async move {
            h.ensure_link_deleted(&veth_a2).await.ok();
            h.add_veth(&veth_a2, &veth_b2).await?;
            h.move_link_to_netns(&veth_a2, &a_ns_fd).await?;
            h.move_link_to_netns(&veth_b2, &b_ns_fd).await?;
            Ok(())
        })
        .await?;

        // Configure side A: assign IP, bring up, add route to B's /20.
        let veth_a3 = veth_a.clone();
        let b_region_net = region_base(b_idx);
        core::nl_run(netns, &a_ns, move |h: Netlink| async move {
            h.add_addr4(&veth_a3, ip_a, 30).await?;
            h.set_link_up(&veth_a3).await?;
            h.add_route_v4(b_region_net, 20, ip_b).await?;
            Ok(())
        })
        .await?;

        // Configure side B: assign IP, bring up, add route to A's /20.
        let veth_b3 = veth_b.clone();
        let a_region_net = region_base(a_idx);
        core::nl_run(netns, &b_ns, move |h: Netlink| async move {
            h.add_addr4(&veth_b3, ip_b, 30).await?;
            h.set_link_up(&veth_b3).await?;
            h.add_route_v4(a_region_net, 20, ip_a).await?;
            Ok(())
        })
        .await?;

        // Apply netem impairment on both veth ends.
        if link.latency_ms > 0 || link.jitter_ms > 0 || link.loss_pct > 0.0 {
            let limits = LinkLimits {
                latency_ms: link.latency_ms,
                jitter_ms: link.jitter_ms,
                loss_pct: link.loss_pct as f32,
                rate_kbit: if link.rate_mbit > 0 {
                    link.rate_mbit * 1000
                } else {
                    0
                },
                ..Default::default()
            };
            let veth_a4 = veth_a.clone();
            let limits_a = limits;
            let rt_a = netns.rt_handle_for(&a_ns)?;
            rt_a.spawn(async move { crate::qdisc::apply_impair(&veth_a4, limits_a).await })
                .await
                .context("tc impair task panicked")??;
            let veth_b4 = veth_b.clone();
            let rt_b = netns.rt_handle_for(&b_ns)?;
            rt_b.spawn(async move { crate::qdisc::apply_impair(&veth_b4, limits).await })
                .await
                .context("tc impair task panicked")??;
        }

        Ok(())
    }

    /// Breaks the direct link between two regions, rerouting through an intermediate.
    ///
    /// Finds a third region `m` that has non-broken links to both `a` and `b`,
    /// and replaces the direct routes with routes through `m`. Traffic will
    /// traverse two inter-region hops instead of one.
    pub fn break_region_link(&self, a: &Region, b: &Region) -> Result<()> {
        let inner = self.inner.core.lock().unwrap();

        let link_key = Self::region_link_key(&a.name, &b.name);
        let link = inner
            .region_links
            .get(&link_key)
            .ok_or_else(|| anyhow!("no link between '{}' and '{}'", a.name, b.name))?;
        if link.broken {
            bail!(
                "link between '{}' and '{}' is already broken",
                a.name,
                b.name
            );
        }

        // Find intermediate region m with non-broken links to both a and b.
        let m_name = inner
            .regions
            .keys()
            .find(|name| {
                if *name == &a.name || *name == &b.name {
                    return false;
                }
                let key_ma = Self::region_link_key(name, &a.name);
                let key_mb = Self::region_link_key(name, &b.name);
                let link_ma = inner.region_links.get(&key_ma);
                let link_mb = inner.region_links.get(&key_mb);
                matches!((link_ma, link_mb), (Some(la), Some(lb)) if !la.broken && !lb.broken)
            })
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "no intermediate region found to reroute '{}'↔'{}'",
                    a.name,
                    b.name
                )
            })?;

        // Get the veth IPs for m↔a and m↔b links.
        let key_ma = Self::region_link_key(&m_name, &a.name);
        let link_ma = inner.region_links.get(&key_ma).unwrap();
        // m's IP on the m↔a veth: if key is (a, m) then ip_b is m's side, else ip_a.
        let m_ip_on_ma = if key_ma.0 == a.name {
            link_ma.ip_b
        } else {
            link_ma.ip_a
        };

        let key_mb = Self::region_link_key(&m_name, &b.name);
        let link_mb = inner.region_links.get(&key_mb).unwrap();
        let m_ip_on_mb = if key_mb.0 == b.name {
            link_mb.ip_b
        } else {
            link_mb.ip_a
        };

        let a_ns = inner.router(a.router_id).unwrap().ns.clone();
        let b_ns = inner.router(b.router_id).unwrap().ns.clone();
        drop(inner);
        let netns = &self.inner.netns;

        // On region_a: replace route to b's /20 via m (on a↔m veth)
        let b_net = region_base(b.idx);
        let a_via = m_ip_on_ma;
        netns.run_closure_in(&a_ns, move || {
            let status = Command::new("ip")
                .args([
                    "route",
                    "replace",
                    &format!("{b_net}/20"),
                    "via",
                    &a_via.to_string(),
                ])
                .status()
                .context("ip route replace")?;
            if !status.success() {
                bail!("ip route replace failed");
            }
            Ok(())
        })?;

        // On region_b: replace route to a's /20 via m (on b↔m veth)
        let a_net = region_base(a.idx);
        let b_via = m_ip_on_mb;
        netns.run_closure_in(&b_ns, move || {
            let status = Command::new("ip")
                .args([
                    "route",
                    "replace",
                    &format!("{a_net}/20"),
                    "via",
                    &b_via.to_string(),
                ])
                .status()
                .context("ip route replace")?;
            if !status.success() {
                bail!("ip route replace failed");
            }
            Ok(())
        })?;

        // Mark link as broken.
        self.inner
            .core
            .lock()
            .unwrap()
            .region_links
            .get_mut(&link_key)
            .unwrap()
            .broken = true;
        Ok(())
    }

    /// Restores a previously broken direct link between two regions.
    ///
    /// Reverses [`break_region_link`](Self::break_region_link): replaces the
    /// indirect route through the intermediate region with the original direct
    /// veth route.
    ///
    /// # Errors
    ///
    /// Returns an error if the link is not currently broken or if the regions
    /// are not connected.
    pub fn restore_region_link(&self, a: &Region, b: &Region) -> Result<()> {
        let link_key = Self::region_link_key(&a.name, &b.name);
        let (a_ns, b_ns, link_ip_a, link_ip_b) = {
            let inner = self.inner.core.lock().unwrap();
            let link = inner
                .region_links
                .get(&link_key)
                .ok_or_else(|| anyhow!("no link between '{}' and '{}'", a.name, b.name))?;
            if !link.broken {
                bail!("link between '{}' and '{}' is not broken", a.name, b.name);
            }
            let a_ns = inner.router(a.router_id).unwrap().ns.clone();
            let b_ns = inner.router(b.router_id).unwrap().ns.clone();
            (a_ns, b_ns, link.ip_a, link.ip_b)
        };
        let netns = &self.inner.netns;

        // Direct route on a: b's /20 via b's IP on the a↔b veth.
        let b_net = region_base(b.idx);
        let b_direct_ip = if link_key.0 == a.name {
            link_ip_b
        } else {
            link_ip_a
        };
        netns.run_closure_in(&a_ns, move || {
            let status = Command::new("ip")
                .args([
                    "route",
                    "replace",
                    &format!("{b_net}/20"),
                    "via",
                    &b_direct_ip.to_string(),
                ])
                .status()
                .context("ip route replace")?;
            if !status.success() {
                bail!("ip route replace failed");
            }
            Ok(())
        })?;

        // Direct route on b: a's /20 via a's IP on the a↔b veth.
        let a_net = region_base(a.idx);
        let a_direct_ip = if link_key.0 == a.name {
            link_ip_a
        } else {
            link_ip_b
        };
        netns.run_closure_in(&b_ns, move || {
            let status = Command::new("ip")
                .args([
                    "route",
                    "replace",
                    &format!("{a_net}/20"),
                    "via",
                    &a_direct_ip.to_string(),
                ])
                .status()
                .context("ip route replace")?;
            if !status.success() {
                bail!("ip route replace failed");
            }
            Ok(())
        })?;

        // Mark link as not broken.
        self.inner
            .core
            .lock()
            .unwrap()
            .region_links
            .get_mut(&link_key)
            .unwrap()
            .broken = false;
        Ok(())
    }

    /// Creates three default regions (us, eu, asia) with typical one-way latencies.
    ///
    /// One-way latencies (RTT = 2×):
    /// - us↔eu: 40ms (RTT ~80ms, real-world 70–100ms)
    /// - us↔asia: 95ms (RTT ~190ms, real-world 170–220ms US East↔East Asia)
    /// - eu↔asia: 120ms (RTT ~240ms, real-world 210–250ms EU↔East Asia)
    pub async fn add_default_regions(&self) -> Result<DefaultRegions> {
        let us = self.add_region("us").await?;
        let eu = self.add_region("eu").await?;
        let asia = self.add_region("asia").await?;
        self.link_regions(&us, &eu, RegionLink::good(40)).await?;
        self.link_regions(&us, &asia, RegionLink::good(95)).await?;
        self.link_regions(&eu, &asia, RegionLink::good(120)).await?;
        Ok(DefaultRegions { us, eu, asia })
    }

    fn region_link_key(a: &str, b: &str) -> (String, String) {
        if a < b {
            (a.to_string(), b.to_string())
        } else {
            (b.to_string(), a.to_string())
        }
    }

    /// No-op stub — the old per-CIDR tc filter approach has been removed.
    /// Use [`add_region`](Self::add_region) + [`link_regions`](Self::link_regions) instead.
    #[deprecated(note = "use add_region + link_regions instead")]
    pub fn set_region_latency(&self, _from: &str, _to: &str, _latency_ms: u32) {}

    /// Builds a map of `NETSIM_*` environment variables from the current lab state.
    ///
    /// Keys follow the pattern `NETSIM_IP_{DEVICE}` for the default interface
    /// and `NETSIM_IP_{DEVICE}_{IFACE}` for all interfaces. Names are
    /// uppercased with hyphens replaced by underscores.
    pub fn env_vars(&self) -> std::collections::HashMap<String, String> {
        let inner = self.inner.core.lock().unwrap();
        let mut map = std::collections::HashMap::new();
        for dev in inner.all_devices() {
            let norm = crate::handles::normalize_env_name(&dev.name);
            if let Some(ip) = dev.default_iface().ip {
                map.insert(format!("NETSIM_IP_{}", norm), ip.to_string());
            }
            for iface in &dev.interfaces {
                if let Some(ip) = iface.ip {
                    let ifnorm = crate::handles::normalize_env_name(&iface.ifname);
                    map.insert(format!("NETSIM_IP_{}_{}", norm, ifnorm), ip.to_string());
                }
            }
        }
        map
    }

    /// Returns a handle to the IX (Internet Exchange) root namespace.
    pub fn ix(&self) -> Ix {
        Ix::new(Arc::clone(&self.inner))
    }

    /// Safety-net cleanup: drops fd-registry entries for this lab's prefix.
    /// Normal cleanup happens in `NetworkCore::drop`.
    pub fn cleanup(&self) {
        let prefix = self.inner.core.lock().unwrap().cfg.prefix.clone();
        self.inner.netns.cleanup_prefix(&prefix);
    }

    // ── DNS entries ───────────────────────────────────────────────────────

    /// Adds a hosts entry visible to all devices.
    ///
    /// The entry is written to each device's hosts file overlay. Worker threads
    /// (sync, async, and tokio blocking pool) have `/etc/hosts` bind-mounted, so
    /// glibc picks up changes on the next `getaddrinfo()` via mtime check.
    pub fn dns_entry(&self, name: &str, ip: std::net::IpAddr) -> Result<()> {
        let mut inner = self.inner.core.lock().unwrap();
        inner.dns.global.push((name.to_string(), ip));
        let ids: Vec<_> = inner.all_device_ids();
        inner.dns.write_all_hosts_files(&ids)?;
        Ok(())
    }

    /// Resolves a name from the lab-wide DNS entries (in-memory, no syscall).
    pub fn resolve(&self, name: &str) -> Option<std::net::IpAddr> {
        let inner = self.inner.core.lock().unwrap();
        inner.dns.resolve(None, name)
    }

    /// Sets the nameserver for all devices (writes `/etc/resolv.conf` overlay).
    ///
    /// Worker threads have `/etc/resolv.conf` bind-mounted, so glibc picks up
    /// changes on the next resolver call.
    pub fn set_nameserver(&self, server: std::net::IpAddr) -> Result<()> {
        let mut inner = self.inner.core.lock().unwrap();
        inner.dns.nameserver = Some(server);
        inner.dns.write_resolv_conf()
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
    pub async fn set_link_condition(
        &self,
        a: NodeId,
        b: NodeId,
        impair: Option<LinkCondition>,
    ) -> Result<()> {
        debug!(a = ?a, b = ?b, impair = ?impair, "lab: set_link_condition");

        // Extract (ns, ifname) under the lock, then apply impairment after dropping it.
        let target: (String, String) = {
            let inner = self.inner.core.lock().unwrap();

            // Try Device ↔ Router in both orderings.
            let mut found = None;
            for (dev_id, router_id) in [(a, b), (b, a)] {
                if let (Some(dev), Some(router)) = (inner.device(dev_id), inner.router(router_id)) {
                    let downlink_sw = router.downlink.ok_or_else(|| {
                        anyhow!("router '{}' has no downstream switch", router.name)
                    })?;
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
                    found = Some((dev.ns.clone(), iface.ifname.clone()));
                    break;
                }
            }

            if let Some(t) = found {
                t
            } else if let (Some(ra), Some(rb)) = (inner.router(a), inner.router(b)) {
                // Router(a) ↔ Router(b) — one must be upstream of the other.
                if let Some(a_downlink) = ra.downlink {
                    if rb.uplink == Some(a_downlink) {
                        let wan_if = rb.wan_ifname(inner.ix_sw()).to_string();
                        (rb.ns.clone(), wan_if)
                    } else if let Some(b_downlink) = rb.downlink {
                        if ra.uplink == Some(b_downlink) {
                            let wan_if = ra.wan_ifname(inner.ix_sw()).to_string();
                            (ra.ns.clone(), wan_if)
                        } else {
                            bail!(
                                "routers '{}' and '{}' are not directly connected",
                                ra.name,
                                rb.name
                            );
                        }
                    } else {
                        bail!(
                            "routers '{}' and '{}' are not directly connected",
                            ra.name,
                            rb.name
                        );
                    }
                } else if let Some(b_downlink) = rb.downlink {
                    if ra.uplink == Some(b_downlink) {
                        let wan_if = ra.wan_ifname(inner.ix_sw()).to_string();
                        (ra.ns.clone(), wan_if)
                    } else {
                        bail!(
                            "routers '{}' and '{}' are not directly connected",
                            ra.name,
                            rb.name
                        );
                    }
                } else {
                    bail!(
                        "routers '{}' and '{}' are not directly connected",
                        ra.name,
                        rb.name
                    );
                }
            } else {
                bail!(
                    "nodes {:?} and {:?} are not a connected device-router or router-router pair",
                    a,
                    b
                );
            }
        };

        apply_or_remove_impair(&self.inner.netns, &target.0, &target.1, impair).await;
        Ok(())
    }

    // ── Lookup helpers ───────────────────────────────────────────────────

    /// Returns a device handle by id, or `None` if the id is not a device.
    pub fn device(&self, id: NodeId) -> Option<Device> {
        let inner = self.inner.core.lock().unwrap();
        let d = inner.device(id)?;
        Some(Device::new(
            id,
            d.name.as_str().into(),
            d.ns.as_str().into(),
            Arc::clone(&self.inner),
        ))
    }

    /// Returns a router handle by id, or `None` if the id is not a router.
    pub fn router(&self, id: NodeId) -> Option<Router> {
        let inner = self.inner.core.lock().unwrap();
        let r = inner.router(id)?;
        Some(Router::new(
            id,
            r.name.as_str().into(),
            r.ns.as_str().into(),
            Arc::clone(&self.inner),
        ))
    }

    /// Looks up a device by name and returns a handle.
    pub fn device_by_name(&self, name: &str) -> Option<Device> {
        let inner = self.inner.core.lock().unwrap();
        let id = inner.device_id_by_name(name)?;
        let d = inner.device(id)?;
        Some(Device::new(
            id,
            d.name.as_str().into(),
            d.ns.as_str().into(),
            Arc::clone(&self.inner),
        ))
    }

    /// Looks up a router by name and returns a handle.
    pub fn router_by_name(&self, name: &str) -> Option<Router> {
        let inner = self.inner.core.lock().unwrap();
        let id = inner.router_id_by_name(name)?;
        let r = inner.router(id)?;
        Some(Router::new(
            id,
            r.name.as_str().into(),
            r.ns.as_str().into(),
            Arc::clone(&self.inner),
        ))
    }

    /// Returns handles for all devices.
    pub fn devices(&self) -> Vec<Device> {
        let inner = self.inner.core.lock().unwrap();
        inner
            .all_device_ids()
            .into_iter()
            .filter_map(|id| {
                let d = inner.device(id)?;
                Some(Device::new(
                    id,
                    d.name.as_str().into(),
                    d.ns.as_str().into(),
                    Arc::clone(&self.inner),
                ))
            })
            .collect()
    }

    /// Returns handles for all routers.
    pub fn routers(&self) -> Vec<Router> {
        let inner = self.inner.core.lock().unwrap();
        inner
            .all_router_ids()
            .into_iter()
            .filter_map(|id| {
                let r = inner.router(id)?;
                Some(Router::new(
                    id,
                    r.name.as_str().into(),
                    r.ns.as_str().into(),
                    Arc::clone(&self.inner),
                ))
            })
            .collect()
    }
}

// ─────────────────────────────────────────────
// RouterBuilder
// ─────────────────────────────────────────────

/// Builder for a router node; returned by [`Lab::add_router`].
pub struct RouterBuilder {
    inner: Arc<LabInner>,
    lab_span: tracing::Span,
    name: String,
    region: Option<String>,
    upstream: Option<NodeId>,
    nat: Nat,
    ip_support: IpSupport,
    nat_v6: NatV6Mode,
    downstream_cidr: Option<Ipv4Net>,
    downlink_condition: Option<LinkCondition>,
    mtu: Option<u32>,
    block_icmp_frag_needed: bool,
    firewall: Firewall,
    result: Result<()>,
}

impl RouterBuilder {
    /// Creates a builder in an error state; `build()` will return this error.
    fn error(
        inner: Arc<LabInner>,
        lab_span: tracing::Span,
        name: &str,
        err: anyhow::Error,
    ) -> Self {
        Self {
            inner,
            lab_span,
            name: name.to_string(),
            region: None,
            upstream: None,
            nat: Nat::None,
            ip_support: IpSupport::V4Only,
            nat_v6: NatV6Mode::None,
            downstream_cidr: None,
            downlink_condition: None,
            mtu: None,
            block_icmp_frag_needed: false,
            firewall: Firewall::None,
            result: Err(err),
        }
    }

    /// Places this router in a region, connecting it to the region's bridge.
    ///
    /// The router becomes a sub-router of the region router. For `Nat::None`
    /// routers, a return route is added in the region router's namespace.
    pub fn region(mut self, region: &Region) -> Self {
        if self.result.is_ok() {
            self.region = Some(region.name.clone());
            self.upstream = Some(region.router_id);
        }
        self
    }

    /// Connects this router as a sub-router behind `parent`'s downstream switch.
    ///
    /// Without this, the router attaches directly to the IX switch.
    pub fn upstream(mut self, parent: NodeId) -> Self {
        if self.result.is_ok() {
            self.upstream = Some(parent);
        }
        self
    }

    /// Sets the NAT mode. Defaults to [`Nat::None`] (no NAT, public addressing).
    pub fn nat(mut self, mode: Nat) -> Self {
        if self.result.is_ok() {
            self.nat = mode;
        }
        self
    }

    /// Sets an impairment condition on this router's downlink bridge, affecting
    /// download-direction traffic to all downstream devices.
    ///
    /// Equivalent to calling [`Router::set_downlink_condition`] after build.
    pub fn downlink_condition(mut self, condition: LinkCondition) -> Self {
        if self.result.is_ok() {
            self.downlink_condition = Some(condition);
        }
        self
    }

    /// Sets the MTU on this router's WAN and LAN bridge interfaces.
    ///
    /// Useful for simulating VPN tunnels (e.g. 1420 for WireGuard) or
    /// constrained paths.
    pub fn mtu(mut self, mtu: u32) -> Self {
        if self.result.is_ok() {
            self.mtu = Some(mtu);
        }
        self
    }

    /// Blocks ICMP "fragmentation needed" (type 3, code 4) in the forward chain.
    ///
    /// Simulates a PMTU blackhole middlebox — devices behind this router
    /// will not receive path MTU discovery feedback.
    pub fn block_icmp_frag_needed(mut self) -> Self {
        if self.result.is_ok() {
            self.block_icmp_frag_needed = true;
        }
        self
    }

    /// Sets a firewall preset for this router.
    pub fn firewall(mut self, fw: Firewall) -> Self {
        if self.result.is_ok() {
            self.firewall = fw;
        }
        self
    }

    /// Configures a custom firewall via a builder closure.
    ///
    /// # Example
    /// ```ignore
    /// lab.add_router("fw")
    ///     .firewall_custom(|f| f.allow_tcp(&[80, 443]).allow_udp(&[53]).block_udp())
    ///     .build().await?;
    /// ```
    pub fn firewall_custom(
        mut self,
        f: impl FnOnce(&mut FirewallConfigBuilder) -> &mut FirewallConfigBuilder,
    ) -> Self {
        if self.result.is_ok() {
            let mut builder = FirewallConfigBuilder::default();
            f(&mut builder);
            self.firewall = Firewall::Custom(builder.build());
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

    /// Overrides the downstream subnet instead of auto-allocating from the pool.
    ///
    /// The gateway address is the `.1` host of the given CIDR. Device addresses
    /// are allocated sequentially starting at `.2`.
    pub fn downstream_cidr(mut self, cidr: Ipv4Net) -> Self {
        if self.result.is_ok() {
            self.downstream_cidr = Some(cidr);
        }
        self
    }

    /// Finalizes the router, creates its namespace and links, and returns a [`Router`] handle.
    pub async fn build(self) -> Result<Router> {
        self.result?;

        // Phase 1: Lock → register topology + extract snapshot → unlock.
        let (id, setup_data) = {
            let mut inner = self.inner.core.lock().unwrap();
            let nat = self.nat;
            let downstream_pool = if nat == Nat::None {
                DownstreamPool::Public
            } else {
                DownstreamPool::Private
            };
            let id = inner.add_router(
                &self.name,
                nat,
                downstream_pool,
                self.region,
                self.ip_support,
                self.nat_v6,
            );
            // Apply builder-level config to the registered RouterData.
            if let Some(r) = inner.router_mut(id) {
                r.cfg.mtu = self.mtu;
                r.cfg.block_icmp_frag_needed = self.block_icmp_frag_needed;
                r.cfg.firewall = self.firewall.clone();
            }
            let has_v4 = self.ip_support.has_v4();
            let has_v6 = self.ip_support.has_v6();
            let sub_switch =
                inner.add_switch(&format!("{}-sub", self.name), None, None, None, None);
            // For Nat::None sub-routers in a region, allocate downstream /24
            // from the region's pool instead of the global pool.
            let downstream_cidr = if self.downstream_cidr.is_some() {
                self.downstream_cidr
            } else if downstream_pool == DownstreamPool::Public {
                if let Some(region_name) = inner.router(id).and_then(|r| r.region.clone()) {
                    if inner.regions.contains_key(&region_name) {
                        Some(inner.alloc_region_public_cidr(&region_name)?)
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };
            inner.connect_router_downlink(id, sub_switch, downstream_cidr)?;
            match self.upstream {
                None => {
                    let ix_ip = if has_v4 {
                        Some(inner.alloc_ix_ip_low()?)
                    } else {
                        None
                    };
                    let ix_ip_v6 = if has_v6 {
                        Some(inner.alloc_ix_ip_v6_low()?)
                    } else {
                        None
                    };
                    let ix_sw = inner.ix_sw();
                    inner.connect_router_uplink(id, ix_sw, ix_ip, ix_ip_v6)?;
                }
                Some(parent_id) => {
                    let parent_downlink = inner
                        .router(parent_id)
                        .and_then(|r| r.downlink)
                        .ok_or_else(|| anyhow!("parent router missing downlink switch"))?;
                    let uplink_ip_v4 = if has_v4 {
                        Some(inner.alloc_from_switch(parent_downlink)?)
                    } else {
                        None
                    };
                    let uplink_ip_v6 = if has_v6 {
                        Some(inner.alloc_from_switch_v6(parent_downlink)?)
                    } else {
                        None
                    };
                    inner.connect_router_uplink(id, parent_downlink, uplink_ip_v4, uplink_ip_v6)?;
                }
            }

            // Extract snapshot for async setup.
            let router = inner.router(id).unwrap().clone();
            let cfg = &inner.cfg;
            let ix_sw = inner.ix_sw();

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
                    let sw = inner.switch(uplink).unwrap();
                    let owner = sw.owner_router.unwrap();
                    let owner_ns = inner.router(owner).unwrap().ns.clone();
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
                let sw = inner.switch(sw_id)?;
                let br = sw.bridge.clone().unwrap_or_else(|| "br-lan".to_string());
                let v4 = sw.gw.and_then(|gw| Some((gw, sw.cidr?.prefix_len())));
                Some((br, v4))
            });
            let downlink_bridge_v6 = router.downlink.and_then(|sw_id| {
                let sw = inner.switch(sw_id)?;
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
                let parent_id = inner.switch(uplink_sw).and_then(|sw| sw.owner_router);
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
                        if let Some(parent_router) = inner.router(pid) {
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

            // For sub-routers with public downstream: add return route in
            // parent router's NS (e.g. region router) so return traffic can
            // reach this sub-router's downstream /24.
            let parent_route_v4 = if router.uplink.is_some()
                && router.uplink != Some(ix_sw)
                && router.cfg.downstream_pool == DownstreamPool::Public
            {
                if let (Some(cidr), Some(via), Some(ref owner_ns)) = (
                    router.downstream_cidr,
                    router.upstream_ip,
                    &upstream_owner_ns,
                ) {
                    Some((owner_ns.clone(), cidr.addr(), cidr.prefix_len(), via))
                } else {
                    None
                }
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
                parent_route_v4,
            };

            (id, setup_data)
        }; // lock released

        // Phase 2: Async network setup (no lock held).
        let netns = &self.inner.netns;
        async { setup_router_async(netns, &setup_data).await }
            .instrument(self.lab_span.clone())
            .await?;

        let router = {
            let inner = self.inner.core.lock().unwrap();
            let r = inner.router(id).unwrap();
            Router::new(
                id,
                r.name.as_str().into(),
                r.ns.as_str().into(),
                Arc::clone(&self.inner),
            )
        };
        if let Some(cond) = self.downlink_condition {
            router.set_downlink_condition(Some(cond)).await?;
        }
        Ok(router)
    }
}

// ─────────────────────────────────────────────
// DeviceBuilder
// ─────────────────────────────────────────────

/// Builder for a device node; returned by [`Lab::add_device`].
pub struct DeviceBuilder {
    inner: Arc<LabInner>,
    lab_span: tracing::Span,
    id: NodeId,
    mtu: Option<u32>,
    result: Result<()>,
}

impl DeviceBuilder {
    /// Sets the MTU on all interfaces of this device.
    pub fn mtu(mut self, mtu: u32) -> Self {
        if self.result.is_ok() {
            self.mtu = Some(mtu);
        }
        self
    }

    /// Attach `ifname` inside the device namespace to `router`'s downstream switch.
    pub fn iface(mut self, ifname: &str, router: NodeId, impair: Option<LinkCondition>) -> Self {
        if self.result.is_ok() {
            self.result = self
                .inner
                .core
                .lock()
                .unwrap()
                .add_device_iface(self.id, ifname, router, impair)
                .map(|_| ());
        }
        self
    }

    /// Attach to `router`'s downstream switch with auto-named interfaces (eth0, eth1, ...).
    pub fn uplink(mut self, router: NodeId) -> Self {
        if self.result.is_ok() {
            let idx = {
                let inner = self.inner.core.lock().unwrap();
                inner
                    .device(self.id)
                    .map(|d| d.interfaces.len())
                    .unwrap_or(0)
            };
            let ifname = format!("eth{}", idx);
            self.result = self
                .inner
                .core
                .lock()
                .unwrap()
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
                .core
                .lock()
                .unwrap()
                .set_device_default_via(self.id, ifname);
        }
        self
    }

    /// Finalizes the device, creates its namespace and links, and returns a [`Device`] handle.
    pub async fn build(self) -> Result<Device> {
        self.result?;

        // Phase 1: Lock → extract snapshot + DNS overlay → unlock.
        let (dev, ifaces, prefix, root_ns, dns_overlay) = {
            let mut inner = self.inner.core.lock().unwrap();
            // Apply builder-level config before snapshot.
            if let Some(d) = inner.device_mut(self.id) {
                d.mtu = self.mtu;
            }
            let dev = inner
                .device(self.id)
                .ok_or_else(|| anyhow!("unknown device id"))?
                .clone();

            let mut iface_data = Vec::new();
            for iface in &dev.interfaces {
                let sw = inner.switch(iface.uplink).ok_or_else(|| {
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
                let gw_ns = inner.router(gw_router).unwrap().ns.clone();
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

            // Prepare DNS overlay: ensure the hosts file exists and build paths.
            inner.dns.ensure_hosts_file(self.id)?;
            let overlay = crate::netns::DnsOverlay {
                hosts_path: inner.dns.hosts_path_for(self.id),
                resolv_path: inner.dns.resolv_path(),
            };

            let prefix = inner.cfg.prefix.clone();
            let root_ns = inner.cfg.root_ns.clone();
            (dev, iface_data, prefix, root_ns, overlay)
        }; // lock released

        // Phase 2: Async network setup (no lock held).
        // The DNS overlay is passed to create_named_netns so worker threads
        // get /etc/hosts and /etc/resolv.conf bind-mounted at startup.
        let netns = &self.inner.netns;
        async {
            setup_device_async(netns, &prefix, &root_ns, &dev, ifaces, Some(dns_overlay)).await
        }
        .instrument(self.lab_span.clone())
        .await?;

        Ok(Device::new(
            self.id,
            dev.name.as_str().into(),
            dev.ns.as_str().into(),
            Arc::clone(&self.inner),
        ))
    }
}
