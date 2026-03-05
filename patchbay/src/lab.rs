//! High-level lab API: [`Lab`], [`DeviceBuilder`], [`Nat`], [`LinkCondition`], [`ObservedAddr`].

use std::{
    collections::HashMap,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
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

pub use crate::qdisc::LinkLimits;
use crate::{
    core::{
        self, apply_or_remove_impair, setup_device_async, setup_root_ns_async, setup_router_async,
        CoreConfig, DeviceSetupData, DownstreamPool, IfaceBuild, LabInner, NetworkCore, NodeId,
        RouterSetupData, RA_DEFAULT_ENABLED, RA_DEFAULT_INTERVAL_SECS, RA_DEFAULT_LIFETIME_SECS,
    },
    event::{DeviceState, LabEvent, LabEventKind, RouterState},
    netlink::Netlink,
};

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

pub use crate::{
    firewall::{Firewall, FirewallConfig, FirewallConfigBuilder},
    handles::{Device, DeviceIface, Ix, Router, RouterIface},
    nat::{
        ConntrackTimeouts, IpSupport, Nat, NatConfig, NatConfigBuilder, NatFiltering, NatMapping,
        NatV6Mode,
    },
};

/// Link-layer impairment profile applied via `tc netem`.
///
/// Named presets model common last-mile conditions. Use [`LinkCondition::Manual`]
/// with [`LinkLimits`] for full control over all `tc netem` parameters.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
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
    name: Arc<str>,
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

/// Options for constructing a [`Lab`].
///
/// Use the builder methods to configure output directory and label, then pass
/// to [`Lab::with_opts`].
///
/// # Example
/// ```no_run
/// # use patchbay::{Lab, LabOpts};
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> anyhow::Result<()> {
/// let lab = Lab::with_opts(
///     LabOpts::default()
///         .outdir(OutDir::Nested("/tmp/patchbay-out".into()))
///         .label("my-test"),
/// )
/// .await?;
/// # Ok(())
/// # }
/// ```
#[derive(Default)]
pub struct LabOpts {
    outdir: Option<OutDir>,
    label: Option<String>,
    ipv6_dad_mode: Ipv6DadMode,
    ipv6_provisioning_mode: Ipv6ProvisioningMode,
}

/// Where the lab writes event logs and state files.
#[derive(Clone, Debug)]
pub enum OutDir {
    /// Parent directory — lab creates a timestamped subdirectory inside it.
    Nested(PathBuf),
    /// Exact directory — lab writes directly here, no subdirectory created.
    Exact(PathBuf),
}

/// Controls IPv6 duplicate address detection behavior in created namespaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ipv6DadMode {
    /// Keep kernel default behavior, DAD enabled.
    Enabled,
    /// Disable DAD for deterministic fast tests.
    #[default]
    Disabled,
}

/// Controls how IPv6 routes are provisioned for hosts and routers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ipv6ProvisioningMode {
    /// Install routes directly from patchbay wiring logic.
    #[default]
    Static,
    /// RA-driven route provisioning mode.
    ///
    /// This mode follows RA and RS semantics for route installation and
    /// emits structured RA and RS events into patchbay logs. It does not
    /// emit raw ICMPv6 Router Advertisement or Router Solicitation packets.
    RaDriven,
}

/// IPv6 behavior profile for a lab, controlling DAD and route provisioning.
///
/// `Deterministic` keeps tests fast and reproducible by disabling DAD and
/// wiring routes statically. `Realistic` enables DAD and RA/RS-driven
/// provisioning, matching how real networks operate. Use `Realistic` when
/// your application depends on RA timing, default-route installation
/// order, or link-local gateway behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ipv6Profile {
    /// DAD disabled, static route wiring. Fast and reproducible for tests.
    Deterministic,
    /// DAD enabled, RA/RS-driven route provisioning. Matches real-world
    /// network behavior where routers announce prefixes and hosts learn
    /// routes through Router Advertisements.
    Realistic,
}

impl Ipv6Profile {
    fn modes(self) -> (Ipv6DadMode, Ipv6ProvisioningMode) {
        match self {
            Self::Deterministic => (Ipv6DadMode::Disabled, Ipv6ProvisioningMode::Static),
            Self::Realistic => (Ipv6DadMode::Enabled, Ipv6ProvisioningMode::RaDriven),
        }
    }
}

impl LabOpts {
    /// Sets the output directory for event log and state files.
    pub fn outdir(mut self, outdir: OutDir) -> Self {
        self.outdir = Some(outdir);
        self
    }

    /// Sets a human-readable label for this lab (used in output directory naming).
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Reads the output directory from the `PATCHBAY_OUTDIR` environment variable
    /// as [`OutDir::Nested`]. Does nothing if the variable is absent.
    pub fn outdir_from_env(mut self) -> Self {
        if let Ok(v) = std::env::var("PATCHBAY_OUTDIR") {
            self.outdir = Some(OutDir::Nested(v.into()));
        }
        self
    }

    /// Sets IPv6 duplicate address detection behavior.
    pub fn ipv6_dad_mode(mut self, mode: Ipv6DadMode) -> Self {
        self.ipv6_dad_mode = mode;
        self
    }

    /// Sets IPv6 provisioning behavior.
    pub fn ipv6_provisioning_mode(mut self, mode: Ipv6ProvisioningMode) -> Self {
        self.ipv6_provisioning_mode = mode;
        self
    }

    /// Applies a deployment profile that sets both DAD and v6 provisioning mode.
    pub fn ipv6_profile(mut self, profile: Ipv6Profile) -> Self {
        let (dad, provisioning) = profile.modes();
        self.ipv6_dad_mode = dad;
        self.ipv6_provisioning_mode = provisioning;
        self
    }
}

impl Lab {
    // ── Constructors ────────────────────────────────────────────────────

    /// Creates a new lab with default address ranges and IX settings.
    ///
    /// Reads `PATCHBAY_OUTDIR` from the environment for event output.
    /// Use [`Lab::with_opts`] for explicit configuration.
    pub async fn new() -> Result<Self> {
        Self::with_opts(LabOpts::default().outdir_from_env()).await
    }

    /// Creates a new lab with the given options.
    pub async fn with_opts(opts: LabOpts) -> Result<Self> {
        let pid = std::process::id();
        let pid_tag = pid % 9999 + 1;
        let lab_seq = LAB_COUNTER.fetch_add(1, Ordering::Relaxed);
        let uniq = format!("{lab_seq:x}");
        let prefix = format!("lab-p{}{}", pid_tag, uniq); // e.g. "lab-p12340"
        let root_ns = format!("lab{lab_seq}-root");
        let bridge_tag = format!("p{}{}", pid_tag, uniq);
        let ix_gw = Ipv4Addr::new(198, 18, 0, 1);
        let label: Option<Arc<str>> = opts.label.map(|s| Arc::from(s.as_str()));
        let lab_span = debug_span!("lab", id = lab_seq);
        {
            let _enter = lab_span.enter();
            debug!(prefix = %prefix, "lab: created");
        }
        let core = NetworkCore::new(CoreConfig {
            lab_id: lab_seq,
            prefix: prefix.clone().into(),
            root_ns: root_ns.into(),
            bridge_tag: bridge_tag.into(),
            ix_br: format!("br-p{}{}-1", pid_tag, uniq).into(),
            ix_gw,
            ix_cidr: net4(198, 18, 0, 0, 24),
            private_cidr: net4(10, 0, 0, 0, 16),
            public_cidr: net4(198, 18, 1, 0, 24),
            ix_gw_v6: Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1),
            ix_cidr_v6: net6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 64),
            private_cidr_v6: net6(Ipv6Addr::new(0xfd10, 0, 0, 0, 0, 0, 0, 0), 48),
            public_cidr_v6: net6(Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 0), 48),
            span: lab_span,
        })
        .context("failed to create DNS entries directory")?;

        // Compute run_dir before constructing LabInner (needed for writer + tracing).
        let run_dir = opts.outdir.map(|od| match od {
            OutDir::Exact(p) => p,
            OutDir::Nested(base) => {
                let label_or_prefix = label
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| prefix.clone());
                let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
                base.join(format!("{ts}-{label_or_prefix}"))
            }
        });

        let mut netns_mgr = crate::netns::NetnsManager::new();
        if let Some(ref rd) = run_dir {
            netns_mgr.set_run_dir(rd.clone());
        }
        let netns = Arc::new(netns_mgr);
        let cancel = tokio_util::sync::CancellationToken::new();
        let (events_tx, _rx) = tokio::sync::broadcast::channel::<LabEvent>(256);
        drop(_rx);

        let lab = Self {
            inner: Arc::new(LabInner {
                core: std::sync::Mutex::new(core),
                netns: Arc::clone(&netns),
                cancel,
                opid: AtomicU64::new(0),
                events_tx,
                label: label.clone(),
                ns_to_name: std::sync::Mutex::new(HashMap::new()),
                run_dir: run_dir.clone(),
                ipv6_dad_mode: opts.ipv6_dad_mode,
                ipv6_provisioning_mode: opts.ipv6_provisioning_mode,
            }),
        };
        // Initialize root namespace and IX bridge eagerly — no lazy-init race.
        let cfg = lab.inner.core.lock().unwrap().cfg.clone();
        setup_root_ns_async(&cfg, &netns, opts.ipv6_dad_mode)
            .await
            .context("failed to set up root namespace")?;

        // Spawn file writer if outdir is configured — subscribe before emitting
        // initial events so the writer captures LabCreated and IxCreated.
        if let Some(ref run_dir) = run_dir {
            crate::writer::spawn_writer(run_dir.clone(), lab.inner.events_tx.subscribe(), lab.inner.cancel.clone());
        }

        // Emit lifecycle events.
        lab.inner.emit(LabEventKind::LabCreated {
            lab_prefix: cfg.prefix.to_string(),
            label: label.as_ref().map(|s| s.to_string()),
        });
        lab.inner.emit(LabEventKind::IxCreated {
            bridge: cfg.ix_br.to_string(),
            cidr: cfg.ix_cidr,
            gw: cfg.ix_gw,
            cidr_v6: cfg.ix_cidr_v6,
            gw_v6: cfg.ix_gw_v6,
        });

        Ok(lab)
    }

    /// Returns the unique resource prefix associated with this lab instance.
    pub fn prefix(&self) -> String {
        self.inner.core.lock().unwrap().cfg.prefix.to_string()
    }

    /// Subscribe to the lab event stream.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<LabEvent> {
        self.inner.events_tx.subscribe()
    }

    /// Returns the resolved run output directory, if outdir was configured.
    ///
    /// This is the `{base}/{timestamp}-{label}` subdirectory where events, state,
    /// and per-namespace tracing logs are written.
    pub fn run_dir(&self) -> Option<&Path> {
        self.inner.run_dir.as_deref()
    }

    /// Returns the human-readable label, if one was set at construction.
    pub fn label(&self) -> Option<&str> {
        self.inner.label.as_deref()
    }

    /// Parses `lab.toml`, builds the network, and returns a ready-to-use lab.
    pub async fn load(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path).context("read lab config")?;
        let cfg: crate::config::LabConfig = toml::from_str(&text).context("parse lab config")?;
        Self::from_config(cfg).await
    }

    /// Builds a `Lab` from a parsed config, creating all namespaces and links.
    pub async fn from_config(cfg: crate::config::LabConfig) -> Result<Self> {
        Self::from_config_with_opts(cfg, LabOpts::default().outdir_from_env()).await
    }

    /// Builds a `Lab` from a parsed config with explicit options.
    pub async fn from_config_with_opts(
        cfg: crate::config::LabConfig,
        opts: LabOpts,
    ) -> Result<Self> {
        let lab = Self::with_opts(opts).await?;

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
                if let Some(enabled) = rcfg.ra_enabled {
                    rb = rb.ra_enabled(enabled);
                }
                if let Some(interval) = rcfg.ra_interval_secs {
                    rb = rb.ra_interval_secs(interval);
                }
                if let Some(lifetime) = rcfg.ra_lifetime_secs {
                    rb = rb.ra_lifetime_secs(lifetime);
                }
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
            downstream_pool: None,
            downstream_cidr: None,
            downlink_condition: None,
            mtu: None,
            block_icmp_frag_needed: false,
            firewall: Firewall::None,
            ra_enabled: RA_DEFAULT_ENABLED,
            ra_interval_secs: RA_DEFAULT_INTERVAL_SECS,
            ra_lifetime_secs: RA_DEFAULT_LIFETIME_SECS,
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
                provisioning_mode: None,
                result: Err(anyhow!("device '{}' already exists", name)),
            };
        }
        let id = inner.add_device(name);
        DeviceBuilder {
            inner: Arc::clone(&self.inner),
            lab_span,
            id,
            mtu: None,
            provisioning_mode: None,
            result: Ok(()),
        }
    }

    // ── removal ──────────────────────────────────────────────────────────

    /// Removes a device from the lab, destroying its namespace and all interfaces.
    ///
    /// The kernel automatically destroys veth pairs when the namespace closes.
    pub fn remove_device(&self, id: NodeId) -> Result<()> {
        let dev = self.inner.core.lock().unwrap().remove_device(id)?;
        self.inner.netns.remove_worker(&dev.ns);
        self.inner.emit(LabEventKind::DeviceRemoved {
            name: dev.name.to_string(),
        });
        Ok(())
    }

    /// Removes a router from the lab, destroying its namespace and all interfaces.
    ///
    /// Fails if any devices are still connected to this router's downstream switch.
    /// Remove or replug those devices first.
    pub fn remove_router(&self, id: NodeId) -> Result<()> {
        let router = self.inner.core.lock().unwrap().remove_router(id)?;
        self.inner.netns.remove_worker(&router.ns);
        self.inner.emit(LabEventKind::RouterRemoved {
            name: router.name.to_string(),
        });
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
            // DualStack so it can forward v6 traffic from sub-routers.
            let id = inner.add_router(
                &region_router_name,
                Nat::None,
                DownstreamPool::Public,
                None,
                IpSupport::DualStack,
                NatV6Mode::None,
            );

            // Downstream switch: region's first /24 as override CIDR.
            // v6 /64 is auto-allocated by connect_router_downlink since region router is DualStack.
            let region_bridge_cidr = net4(198, 18, idx * 16, 0, 24);
            let sub_switch =
                inner.add_switch(&format!("{region_router_name}-sub"), None, None, None, None);
            inner.connect_router_downlink(id, sub_switch, Some(region_bridge_cidr))?;

            // Set next_host to 10 so sub-routers get .10, .11, ...
            if let Some(sw) = inner.switch_mut(sub_switch) {
                sw.next_host = 10;
                sw.next_host_v6 = 10;
            }

            // IX uplink: region router gets an IX IP (v4 + v6).
            let ix_ip = inner.alloc_ix_ip_low()?;
            let ix_ip_v6 = inner.alloc_ix_ip_v6_low()?;
            let ix_sw = inner.ix_sw();
            inner.connect_router_uplink(id, ix_sw, Some(ix_ip), Some(ix_ip_v6))?;

            // Store region info.
            inner.regions.insert(
                Arc::<str>::from(name),
                core::RegionInfo {
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
                let br = sw.bridge.clone().unwrap_or_else(|| "br-lan".into());
                let v4 = sw.gw.and_then(|gw| Some((gw, sw.cidr?.prefix_len())));
                Some((br, v4))
            });
            let downlink_bridge_v6 = router.downlink.and_then(|sw_id| {
                let sw = inner.switch(sw_id)?;
                Some((sw.gw_v6?, sw.cidr_v6?.prefix_len()))
            });
            let ra_enabled = router.cfg.ra_enabled;

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
                ix_gw_v6: Some(cfg.ix_gw_v6),
                ix_cidr_v6_prefix: Some(cfg.ix_cidr_v6.prefix_len()),
                upstream_gw_v6: None,
                upstream_cidr_prefix_v6: None,
                return_route_v6: None,
                downlink_bridge_v6,
                parent_route_v6: None,
                parent_route_v4: None,
                cancel: self.inner.cancel.clone(),
                dad_mode: self.inner.ipv6_dad_mode,
                provisioning_mode: self.inner.ipv6_provisioning_mode,
                ra_enabled,
            };

            (id, setup_data, idx)
        }; // lock released

        // Phase 2: Async network setup (no lock held).
        let netns = &self.inner.netns;
        setup_router_async(netns, &setup_data).await?;

        // Phase 3: Add /20 aggregate route in root NS for the region (v4 + v6).
        let region_net = region_base(idx);
        let via = setup_data
            .router
            .upstream_ip
            .context("region router has no IX IP")?;
        let via_v6 = setup_data.router.upstream_ip_v6;
        let downstream_cidr_v6 = setup_data.router.downstream_cidr_v6;
        let root_ns = setup_data.root_ns.clone();
        core::nl_run(netns, &root_ns, move |h: Netlink| async move {
            h.add_route_v4(region_net, 20, via).await.ok();
            if let (Some(via6), Some(cidr6)) = (via_v6, downstream_cidr_v6) {
                h.add_route_v6(cidr6.addr(), cidr6.prefix_len(), via6)
                    .await
                    .ok();
            }
            Ok(())
        })
        .await?;

        self.inner.emit(LabEventKind::RegionAdded {
            name: name.to_string(),
            router: region_router_name,
        });

        Ok(Region {
            name: Arc::from(name),
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
        let s = self
            .inner
            .core
            .lock()
            .unwrap()
            .prepare_link_regions(&a.name, &b.name)?;

        let netns = &self.inner.netns;
        let veth_a = format!("vr-{}-{}", a.name, b.name);
        let veth_b = format!("vr-{}-{}", b.name, a.name);

        // Create veth pair in root NS, then move ends to region router NSes.
        let veth_a2 = veth_a.clone();
        let veth_b2 = veth_b.clone();
        let a_ns_fd = netns.ns_fd(&s.a.ns)?;
        let b_ns_fd = netns.ns_fd(&s.b.ns)?;
        core::nl_run(netns, &s.root_ns, move |h: Netlink| async move {
            h.ensure_link_deleted(&veth_a2).await.ok();
            h.add_veth(&veth_a2, &veth_b2).await?;
            h.move_link_to_netns(&veth_a2, &a_ns_fd).await?;
            h.move_link_to_netns(&veth_b2, &b_ns_fd).await?;
            Ok(())
        })
        .await?;

        // Copy out IP fields used by both closures.
        let (a_ip, a_ip6) = (s.a.ip, s.a.ip6);
        let (b_ip, b_ip6) = (s.b.ip, s.b.ip6);

        // Configure side A: assign IP, bring up, add route to B's /20.
        let veth_a3 = veth_a.clone();
        let b_region_net = region_base(s.b.idx);
        let b_sub_v6 = s.b.sub_v6;
        core::nl_run(netns, &s.a.ns, move |h: Netlink| async move {
            h.add_addr4(&veth_a3, a_ip, 30).await?;
            h.add_addr6(&veth_a3, a_ip6, 126).await?;
            h.set_link_up(&veth_a3).await?;
            h.add_route_v4(b_region_net, 20, b_ip).await?;
            if let Some(v6) = b_sub_v6 {
                h.add_route_v6(v6.addr(), v6.prefix_len(), b_ip6).await?;
            }
            Ok(())
        })
        .await?;

        // Configure side B: assign IP, bring up, add route to A's /20.
        let veth_b3 = veth_b.clone();
        let a_region_net = region_base(s.a.idx);
        let a_sub_v6 = s.a.sub_v6;
        core::nl_run(netns, &s.b.ns, move |h: Netlink| async move {
            h.add_addr4(&veth_b3, b_ip, 30).await?;
            h.add_addr6(&veth_b3, b_ip6, 126).await?;
            h.set_link_up(&veth_b3).await?;
            h.add_route_v4(a_region_net, 20, a_ip).await?;
            if let Some(v6) = a_sub_v6 {
                h.add_route_v6(v6.addr(), v6.prefix_len(), a_ip6).await?;
            }
            Ok(())
        })
        .await?;

        // Emit RegionLinkAdded event.
        {
            let inner = self.inner.core.lock().unwrap();
            let ra = inner
                .router(a.router_id)
                .map(|r| r.name.to_string())
                .unwrap_or_default();
            let rb = inner
                .router(b.router_id)
                .map(|r| r.name.to_string())
                .unwrap_or_default();
            drop(inner);
            self.inner.emit(LabEventKind::RegionLinkAdded {
                router_a: ra,
                router_b: rb,
            });
        }

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
            let rt_a = netns.rt_handle_for(&s.a.ns)?;
            rt_a.spawn(async move { crate::qdisc::apply_impair(&veth_a4, limits_a).await })
                .await
                .context("tc impair task panicked")??;
            let veth_b4 = veth_b.clone();
            let rt_b = netns.rt_handle_for(&s.b.ns)?;
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
        let s = self
            .inner
            .core
            .lock()
            .unwrap()
            .prepare_break_region_link(&a.name, &b.name)?;

        let netns = &self.inner.netns;

        // On region_a: replace route to b's /20 via m (on a↔m veth)
        let b_net = region_base(b.idx);
        let a_via = s.m_ip_on_ma;
        netns.run_closure_in(&s.a_ns, move || {
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
        let b_via = s.m_ip_on_mb;
        netns.run_closure_in(&s.b_ns, move || {
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
            .set_region_link_broken(&s.link_key, true);

        // Emit event.
        {
            let inner = self.inner.core.lock().unwrap();
            let ra = inner
                .router(a.router_id)
                .map(|r| r.name.to_string())
                .unwrap_or_default();
            let rb = inner
                .router(b.router_id)
                .map(|r| r.name.to_string())
                .unwrap_or_default();
            drop(inner);
            self.inner.emit(LabEventKind::RegionLinkBroken {
                router_a: ra,
                router_b: rb,
                condition: None,
            });
        }

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
        let s = self
            .inner
            .core
            .lock()
            .unwrap()
            .prepare_restore_region_link(&a.name, &b.name)?;

        let netns = &self.inner.netns;

        // Direct route on a: b's /20 via b's IP on the a↔b veth.
        let b_net = region_base(b.idx);
        let b_direct_ip = s.b_direct_ip;
        netns.run_closure_in(&s.a_ns, move || {
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
        let a_direct_ip = s.a_direct_ip;
        netns.run_closure_in(&s.b_ns, move || {
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

        // Mark link as restored.
        self.inner
            .core
            .lock()
            .unwrap()
            .set_region_link_broken(&s.link_key, false);

        // Emit event.
        {
            let inner = self.inner.core.lock().unwrap();
            let ra = inner
                .router(a.router_id)
                .map(|r| r.name.to_string())
                .unwrap_or_default();
            let rb = inner
                .router(b.router_id)
                .map(|r| r.name.to_string())
                .unwrap_or_default();
            drop(inner);
            self.inner.emit(LabEventKind::RegionLinkRestored {
                router_a: ra,
                router_b: rb,
            });
        }

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

    /// Builds a map of `NETSIM_*` environment variables from the current lab state.
    ///
    /// Keys follow the pattern `NETSIM_IP_{DEVICE}` for the default interface
    /// and `NETSIM_IP_{DEVICE}_{IFACE}` for all interfaces. Names are
    /// uppercased with hyphens replaced by underscores.
    pub fn env_vars(&self) -> HashMap<String, String> {
        let inner = self.inner.core.lock().unwrap();
        let mut map = HashMap::new();
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
        self.inner.core.lock().unwrap().add_dns_entry(name, ip)
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
        let (ns, ifname) = self.inner.core.lock().unwrap().resolve_link_target(a, b)?;
        apply_or_remove_impair(&self.inner.netns, &ns, &ifname, impair).await;
        Ok(())
    }

    // ── Lookup helpers ───────────────────────────────────────────────────

    /// Returns a device handle by id, or `None` if the id is not a device.
    pub fn device(&self, id: NodeId) -> Option<Device> {
        let inner = self.inner.core.lock().unwrap();
        let d = inner.device(id)?;
        Some(Device::new(
            id,
            d.name.clone(),
            d.ns.clone(),
            Arc::clone(&self.inner),
        ))
    }

    /// Returns a router handle by id, or `None` if the id is not a router.
    pub fn router(&self, id: NodeId) -> Option<Router> {
        let inner = self.inner.core.lock().unwrap();
        let r = inner.router(id)?;
        Some(Router::new(
            id,
            r.name.clone(),
            r.ns.clone(),
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
            d.name.clone(),
            d.ns.clone(),
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
            r.name.clone(),
            r.ns.clone(),
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
                    d.name.clone(),
                    d.ns.clone(),
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
                    r.name.clone(),
                    r.ns.clone(),
                    Arc::clone(&self.inner),
                ))
            })
            .collect()
    }
}

// ─────────────────────────────────────────────
// RouterPreset
// ─────────────────────────────────────────────

/// Pre-built router configurations that match common real-world deployments.
///
/// Each preset configures NAT mode, firewall policy, IP address family, and
/// downstream address pool as a single unit. Methods called after `.preset()`
/// on the [`RouterBuilder`] override the preset's defaults, so you can start
/// from a known configuration and adjust only what your test needs.
///
/// The ISP presets (`IspCgnat`, `IspV6`) cover both fixed-line and mobile
/// carriers. Most mobile networks (T-Mobile, Vodafone, AT&T) use the same
/// CGNAT or NAT64 infrastructure as their fixed-line counterparts, and
/// real-world measurements confirm that hole-punching succeeds on the
/// majority of them.
///
/// # Example
///
/// ```ignore
/// let home = lab.add_router("home")
///     .preset(RouterPreset::Home)
///     .build().await?;
///
/// // Override NAT while keeping the rest of the Home preset:
/// let home = lab.add_router("home")
///     .preset(RouterPreset::Home)
///     .nat(Nat::FullCone)
///     .build().await?;
/// ```
#[derive(Clone, Copy, Debug)]
pub enum RouterPreset {
    /// Residential home router.
    ///
    /// Models the standard consumer setup: a FritzBox, UniFi, TP-Link, or
    /// similar device where every LAN host gets an RFC 1918 IPv4 address
    /// behind NAT and a ULA IPv6 address behind a stateful firewall. The
    /// NAT is endpoint-independent mapping with address-and-port-dependent
    /// filtering (EIM+APDF), which preserves the external port and allows
    /// UDP hole-punching. The firewall blocks unsolicited inbound
    /// connections on both address families (RFC 6092 CE router behavior).
    ///
    /// Dual-stack, private downstream pool.
    Home,

    /// Public-IP router with no NAT or firewall.
    ///
    /// Downstream devices receive globally routable addresses on both
    /// address families. Use this for datacenter switches, ISP handoff
    /// points, VPS hosts, and any topology where devices need direct
    /// reachability without translation or filtering.
    ///
    /// Dual-stack, public downstream pool.
    Public,

    /// IPv4-only variant of [`Public`](Self::Public).
    ///
    /// Same behavior — no NAT, no firewall, public downstream — but
    /// without IPv6. Models legacy ISPs and v4-only VPS providers.
    ///
    /// V4-only, public downstream pool.
    PublicV4,

    /// ISP or mobile carrier with carrier-grade NAT.
    ///
    /// Models any provider that shares a pool of public IPv4 addresses
    /// across subscribers via CGNAT: budget fiber, fixed-wireless,
    /// satellite (Starlink), and dual-stack mobile carriers (Vodafone, O2,
    /// AT&T). The CGNAT uses endpoint-independent mapping and filtering
    /// per RFC 6888, so hole-punching works — inbound packets reach
    /// mapped ports. No additional firewall beyond the NAT. IPv6 addresses
    /// are globally routable.
    ///
    /// Dual-stack, private downstream pool.
    IspCgnat,

    /// IPv6-only ISP or mobile carrier with NAT64.
    ///
    /// Models T-Mobile US, Jio, NTT Docomo, and other providers that run
    /// pure IPv6 networks. The device has no IPv4 address. A userspace
    /// SIIT translator on the router converts between IPv6 and IPv4 via
    /// the well-known prefix `64:ff9b::/96`, and nftables masquerade
    /// handles port mapping on the IPv4 side. A `BlockInbound` firewall
    /// prevents unsolicited connections.
    ///
    /// V6-only, public downstream pool.
    IspV6,

    /// Enterprise gateway with restrictive outbound filtering.
    ///
    /// Models a Cisco ASA, Palo Alto, or Fortinet appliance. Symmetric NAT
    /// (endpoint-dependent mapping) makes STUN useless — the external port
    /// changes with every new destination, so the reflexive address learned
    /// from a STUN server does not work for other peers. The `Corporate`
    /// firewall restricts outbound traffic to TCP 80/443 and UDP 53,
    /// blocking all other UDP. Applications behind this preset must fall
    /// back to TURN-over-TLS on port 443.
    ///
    /// Dual-stack, private downstream pool.
    Corporate,

    /// Hotel, airport, or conference guest WiFi.
    ///
    /// Symmetric NAT with a `CaptivePortal` firewall that allows TCP on
    /// any port but blocks all non-DNS UDP. This kills QUIC and prevents
    /// direct P2P, but TURN-over-TCP on non-standard ports can still work
    /// — unlike the stricter `Corporate` preset. IPv4-only, because most
    /// guest networks still do not offer IPv6.
    ///
    /// V4-only, private downstream pool.
    Hotel,

    /// Cloud NAT gateway.
    ///
    /// Models AWS NAT Gateway, Azure NAT Gateway, and GCP Cloud NAT. VPC
    /// instances get private addresses, and the NAT gateway handles
    /// public-facing translation with symmetric mapping. Timeouts are
    /// longer than residential NAT (350 seconds for UDP) to accommodate
    /// long-lived cloud workloads. No firewall — security groups and
    /// NACLs are a separate concern in cloud environments.
    ///
    /// Dual-stack, private downstream pool.
    Cloud,
}

impl RouterPreset {
    fn nat(self) -> Nat {
        match self {
            Self::Home => Nat::Home,
            Self::Public | Self::PublicV4 | Self::IspV6 => Nat::None,
            Self::IspCgnat => Nat::Cgnat,
            Self::Corporate | Self::Hotel => Nat::Corporate,
            Self::Cloud => Nat::CloudNat,
        }
    }

    fn nat_v6(self) -> NatV6Mode {
        match self {
            Self::IspV6 => NatV6Mode::Nat64,
            _ => NatV6Mode::None,
        }
    }

    fn firewall(self) -> Firewall {
        match self {
            Self::Home | Self::IspV6 => Firewall::BlockInbound,
            Self::Public | Self::PublicV4 | Self::IspCgnat | Self::Cloud => Firewall::None,
            Self::Corporate => Firewall::Corporate,
            Self::Hotel => Firewall::CaptivePortal,
        }
    }

    fn ip_support(self) -> IpSupport {
        match self {
            Self::PublicV4 | Self::Hotel => IpSupport::V4Only,
            Self::IspV6 => IpSupport::V6Only,
            _ => IpSupport::DualStack,
        }
    }

    fn downstream_pool(self) -> DownstreamPool {
        match self {
            Self::Public | Self::PublicV4 | Self::IspV6 => DownstreamPool::Public,
            _ => DownstreamPool::Private,
        }
    }

    /// Returns the recommended IPv6 profile for this preset.
    ///
    /// All presets return [`Ipv6Profile::Realistic`]. Use
    /// [`LabOpts::ipv6_profile`] with [`Ipv6Profile::Deterministic`] to
    /// override for fast, reproducible tests.
    pub fn recommended_ipv6_profile(self) -> Ipv6Profile {
        Ipv6Profile::Realistic
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
    region: Option<Arc<str>>,
    upstream: Option<NodeId>,
    nat: Nat,
    ip_support: IpSupport,
    nat_v6: NatV6Mode,
    downstream_pool: Option<DownstreamPool>,
    downstream_cidr: Option<Ipv4Net>,
    downlink_condition: Option<LinkCondition>,
    mtu: Option<u32>,
    block_icmp_frag_needed: bool,
    firewall: Firewall,
    ra_enabled: bool,
    ra_interval_secs: u64,
    ra_lifetime_secs: u64,
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
            downstream_pool: None,
            downstream_cidr: None,
            downlink_condition: None,
            mtu: None,
            block_icmp_frag_needed: false,
            firewall: Firewall::None,
            ra_enabled: RA_DEFAULT_ENABLED,
            ra_interval_secs: RA_DEFAULT_INTERVAL_SECS,
            ra_lifetime_secs: RA_DEFAULT_LIFETIME_SECS,
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

    /// Applies a [`RouterPreset`] that sets NAT, firewall, IP support, and
    /// address pool to match a real-world deployment pattern.
    ///
    /// Individual methods (`.nat()`, `.firewall()`, etc.) called **after**
    /// `preset()` override the preset's values.
    ///
    /// # Example
    /// ```ignore
    /// // Home router with full-cone NAT instead of default port-restricted:
    /// lab.add_router("home")
    ///     .preset(RouterPreset::Home)
    ///     .nat(Nat::FullCone)
    ///     .build().await?;
    /// ```
    pub fn preset(mut self, p: RouterPreset) -> Self {
        if self.result.is_ok() {
            self.nat = p.nat();
            self.nat_v6 = p.nat_v6();
            self.firewall = p.firewall();
            self.ip_support = p.ip_support();
            self.downstream_pool = Some(p.downstream_pool());
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

    /// Enables or disables router advertisement emission in RA-driven mode.
    ///
    /// In the current implementation, this controls structured RA events and
    /// default-route behavior, not raw ICMPv6 packet emission.
    pub fn ra_enabled(mut self, enabled: bool) -> Self {
        if self.result.is_ok() {
            self.ra_enabled = enabled;
        }
        self
    }

    /// Sets the RA interval in seconds, clamped to at least 1 second.
    ///
    /// This interval drives patchbay's RA event cadence in RA-driven mode.
    pub fn ra_interval_secs(mut self, secs: u64) -> Self {
        if self.result.is_ok() {
            self.ra_interval_secs = secs.max(1);
        }
        self
    }

    /// Sets Router Advertisement lifetime in seconds.
    ///
    /// A value of `0` advertises default-router withdrawal semantics in
    /// patchbay's RA-driven route model.
    pub fn ra_lifetime_secs(mut self, secs: u64) -> Self {
        if self.result.is_ok() {
            self.ra_lifetime_secs = secs;
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
            let downstream_pool = self.downstream_pool.unwrap_or(if nat == Nat::None {
                DownstreamPool::Public
            } else {
                DownstreamPool::Private
            });
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
                r.cfg.ra_enabled = self.ra_enabled;
                r.cfg.ra_interval_secs = self.ra_interval_secs.max(1);
                r.cfg.ra_lifetime_secs = self.ra_lifetime_secs;
                r.ra_runtime.set_enabled(self.ra_enabled);
                r.ra_runtime.set_interval_secs(self.ra_interval_secs);
                r.ra_runtime.set_lifetime_secs(self.ra_lifetime_secs);
            }
            let has_v4 = self.ip_support.has_v4();
            let has_v6 = self.ip_support.has_v6();
            // NAT64 needs v4 on the uplink even when IpSupport::V6Only,
            // but downstream devices stay v6-only (no v4 CIDR for them).
            let uplink_needs_v4 = has_v4 || self.nat_v6 == NatV6Mode::Nat64;
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
                    let ix_ip = if uplink_needs_v4 {
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
                    let uplink_ip_v4 = if uplink_needs_v4 {
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
                    let bridge = sw.bridge.clone().unwrap_or_else(|| "br-lan".into());
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
                let br = sw.bridge.clone().unwrap_or_else(|| "br-lan".into());
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
                if let (Some(cidr6), Some(via6)) =
                    (router.downstream_cidr_v6, router.upstream_ip_v6)
                {
                    Some((cidr6.addr(), cidr6.prefix_len(), via6))
                } else {
                    None
                }
            } else {
                None
            };

            // For sub-routers with NatV6Mode::None: add routes so that return
            // traffic for the sub-router's ULA subnet can reach it.
            let parent_route_v6 = if let Some(uplink_sw) = router
                .uplink
                .filter(|&u| u != ix_sw && router.cfg.nat_v6 == NatV6Mode::None)
            {
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
            let ra_enabled = router.cfg.ra_enabled;
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
                cancel: self.inner.cancel.clone(),
                dad_mode: self.inner.ipv6_dad_mode,
                provisioning_mode: self.inner.ipv6_provisioning_mode,
                ra_enabled,
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
            let ix_sw = inner.ix_sw();

            // Resolve upstream router name.
            let upstream_name = r.uplink.and_then(|sw_id| {
                if sw_id == ix_sw {
                    return None;
                }
                let sw = inner.switch(sw_id)?;
                let owner = sw.owner_router?;
                Some(inner.router(owner)?.name.to_string())
            });

            // Resolve downstream bridge name.
            let ds_bridge = r
                .downlink
                .and_then(|sw_id| inner.switch(sw_id)?.bridge.as_ref().map(|b| b.to_string()))
                .unwrap_or_default();

            // Emit RouterAdded event.
            let router_state = RouterState::from_router_data(r, upstream_name, ds_bridge);
            self.inner.emit(LabEventKind::RouterAdded {
                name: r.name.to_string(),
                state: Box::new(router_state),
            });

            // Register ns → name mapping.
            self.inner
                .ns_to_name
                .lock()
                .unwrap()
                .insert(r.ns.to_string(), r.name.to_string());

            Router::new(id, r.name.clone(), r.ns.clone(), Arc::clone(&self.inner))
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
    provisioning_mode: Option<Ipv6ProvisioningMode>,
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

    /// Overrides IPv6 provisioning mode for this device only.
    pub fn ipv6_provisioning_mode(mut self, mode: Ipv6ProvisioningMode) -> Self {
        if self.result.is_ok() {
            self.provisioning_mode = Some(mode);
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
        let (dev, ifaces, prefix, root_ns, dns_overlay, provisioning_mode) = {
            let mut inner = self.inner.core.lock().unwrap();
            // Apply builder-level config before snapshot.
            if let Some(d) = inner.device_mut(self.id) {
                d.mtu = self.mtu;
                d.provisioning_mode = self.provisioning_mode;
            }
            let dev = inner
                .device(self.id)
                .ok_or_else(|| anyhow!("unknown device id"))?
                .clone();
            let provisioning_mode = dev
                .provisioning_mode
                .unwrap_or(self.inner.ipv6_provisioning_mode);

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
                let gw_br = sw.bridge.clone().unwrap_or_else(|| "br-lan".into());
                let gw_ns = inner.router(gw_router).unwrap().ns.clone();
                let gw_ip_v6 = if provisioning_mode == Ipv6ProvisioningMode::RaDriven {
                    None
                } else {
                    sw.gw_v6
                };
                let gw_ll_v6 = inner.router(gw_router).and_then(|r| {
                    if provisioning_mode == Ipv6ProvisioningMode::RaDriven {
                        r.active_downstream_ll_v6()
                    } else {
                        r.downstream_ll_v6
                    }
                });
                iface_data.push(IfaceBuild {
                    dev_ns: dev.ns.clone(),
                    gw_ns,
                    gw_ip: sw.gw,
                    gw_br,
                    dev_ip: iface.ip,
                    prefix_len: sw.cidr.map(|c| c.prefix_len()).unwrap_or(24),
                    gw_ip_v6,
                    dev_ip_v6: iface.ip_v6,
                    gw_ll_v6,
                    dev_ll_v6: iface.ll_v6,
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
            (dev, iface_data, prefix, root_ns, overlay, provisioning_mode)
        }; // lock released

        // Phase 2: Async network setup (no lock held).
        // The DNS overlay is passed to create_named_netns so worker threads
        // get /etc/hosts and /etc/resolv.conf bind-mounted at startup.
        let netns = &self.inner.netns;
        async {
            setup_device_async(
                netns,
                DeviceSetupData {
                    prefix,
                    root_ns,
                    dev: dev.clone(),
                    ifaces,
                    dns_overlay: Some(dns_overlay),
                    dad_mode: self.inner.ipv6_dad_mode,
                    provisioning_mode,
                },
            )
            .await
        }
        .instrument(self.lab_span.clone())
        .await?;

        // Emit DeviceAdded event.
        {
            let inner = self.inner.core.lock().unwrap();
            let d = inner.device(self.id).unwrap();
            let device_state = DeviceState::from_device_data(d, &inner);

            self.inner.emit(LabEventKind::DeviceAdded {
                name: d.name.to_string(),
                state: device_state,
            });

            // Register ns → name mapping.
            self.inner
                .ns_to_name
                .lock()
                .unwrap()
                .insert(d.ns.to_string(), d.name.to_string());
        }

        Ok(Device::new(
            self.id,
            dev.name.clone(),
            dev.ns.clone(),
            Arc::clone(&self.inner),
        ))
    }
}
