//! High-level lab API: [`Lab`], [`DeviceBuilder`], [`Nat`], [`LinkCondition`] (aka `LinkCondition`), [`ObservedAddr`].

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

use crate::{
    core::{
        self, apply_nat_for_router, apply_nat_v6, apply_or_remove_impair,
        run_nft_in, setup_device_async, setup_root_ns_async, setup_router_async, CoreConfig,
        DownstreamPool, IfaceBuild, NetworkCore, NodeId, RouterSetupData,
    },
    netlink::Netlink,
};

pub use crate::qdisc::LinkLimits;

pub(crate) static LAB_COUNTER: AtomicU64 = AtomicU64::new(0);

// ─────────────────────────────────────────────
// Public types
// ─────────────────────────────────────────────

/// NAT behavior preset for common real-world equipment.
///
/// Abbreviations used in variant docs:
/// - EIM: Endpoint-Independent Mapping (same external port for all destinations)
/// - EDM: Endpoint-Dependent Mapping (different port per destination, "symmetric")
/// - EIF: Endpoint-Independent Filtering (any host can reach the mapped port)
/// - APDF: Address-and-Port-Dependent Filtering (only contacted host:port can reply)
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, strum::EnumIter, strum::Display,
)]
#[serde(rename_all = "kebab-case")]
pub enum Nat {
    /// No NAT - addresses are publicly routable.
    ///
    /// Use for datacenter racks, cloud VMs with elastic IPs,
    /// or any host that needs a stable public address.
    #[default]
    None,

    /// Home router - the most common consumer NAT.
    ///
    /// EIM + APDF. Port-preserving. No hairpin. UDP timeout 300s.
    /// This is what Linux `snat to <ip>` produces.
    ///
    /// Observed on: FritzBox, Unifi (default), TP-Link Archer, ASUS RT-AX,
    /// OpenWRT default masquerade.
    ///
    /// Hole-punching works with simultaneous open (both sides must send).
    /// This is the RFC 4787 REQ-1 compliant "port-restricted cone" NAT.
    Home,

    /// Corporate firewall - symmetric NAT.
    ///
    /// EDM + APDF. Random ports. No hairpin. UDP timeout 120s.
    /// Produces a different external port per (dst_ip, dst_port) 4-tuple.
    ///
    /// Observed on: Cisco ASA (PAT), Palo Alto NGFW (DIPP), Fortinet
    /// FortiGate, Juniper SRX. AWS/Azure/GCP NAT gateways behave identically.
    ///
    /// Hole-punching is impossible without relay (TURN/DERP).
    #[serde(alias = "destination-dependent")]
    Corporate,

    /// Carrier-grade NAT per RFC 6888.
    ///
    /// EIM + EIF. Port-preserving. No hairpin. UDP timeout 300s.
    /// Applied on the ISP/IX-facing interface (stacks with home NAT).
    ///
    /// Observed on: A10 Thunder CGN, Cisco ASR CGNAT, Juniper MX MS-MPC.
    /// Mobile carriers (T-Mobile, Vodafone, O2) use this for LTE/5G subscribers.
    /// RFC 6888 mandates EIM to preserve P2P traversal at the ISP layer.
    Cgnat,

    /// Cloud NAT gateway - symmetric NAT with randomized ports.
    ///
    /// EDM + APDF. Random ports. No hairpin. UDP timeout 350s.
    ///
    /// Observed on: AWS NAT Gateway, Azure NAT Gateway, GCP Cloud NAT
    /// (default dynamic port allocation mode).
    ///
    /// Functionally identical to Corporate but with longer timeouts
    /// matching documented cloud provider behavior.
    CloudNat,

    /// Full cone - most permissive NAT for testing.
    ///
    /// EIM + EIF. Port-preserving. Hairpin on. UDP timeout 300s.
    /// Any external host can send to the mapped port after first outbound packet.
    ///
    /// Observed on: older FritzBox firmware, some CGNAT with full-cone policy.
    /// Hole-punching always succeeds.
    FullCone,

    /// Custom NAT configuration built from [`NatConfig`].
    ///
    /// Use this when the named presets don't match your scenario.
    /// The router gets private addressing (like [`Nat::Home`]).
    ///
    /// # Example
    /// ```no_run
    /// # use netsim_core::*;
    /// let custom = Nat::Custom(NatConfig::builder()
    ///     .mapping(NatMapping::EndpointIndependent)
    ///     .filtering(NatFiltering::EndpointIndependent)
    ///     .udp_stream_timeout(120)
    ///     .build());
    /// ```
    #[serde(skip)]
    #[strum(disabled)]
    Custom(NatConfig),
}

impl From<NatConfig> for Nat {
    fn from(config: NatConfig) -> Self {
        Nat::Custom(config)
    }
}

/// NAT mapping behavior per RFC 4787 Section 4.1.
///
/// Controls how the NAT assigns external ports when translating outbound packets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NatMapping {
    /// Same external port for all destinations (EIM).
    ///
    /// Port-preserving: the NAT reuses the internal source port when free.
    /// nftables: `snat to <ip>`.
    EndpointIndependent,
    /// Different external port per destination IP+port (symmetric/EDM).
    ///
    /// Port randomized per 4-tuple. nftables: `masquerade random,fully-random`.
    EndpointDependent,
}

/// NAT filtering behavior per RFC 4787 Section 5.
///
/// Controls which inbound packets the NAT allows through to the internal host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NatFiltering {
    /// Any external host can send to the mapped port (full cone).
    ///
    /// nftables: fullcone DNAT map in prerouting.
    EndpointIndependent,
    /// Only the exact (IP, port) the internal endpoint contacted can reply.
    ///
    /// nftables: conntrack-only (no prerouting DNAT).
    AddressAndPortDependent,
}

/// Conntrack timeout configuration for a NAT profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConntrackTimeouts {
    /// Timeout for a single unreplied UDP packet (seconds).
    pub udp: u32,
    /// Timeout for a UDP "stream" (bidirectional traffic seen, seconds).
    pub udp_stream: u32,
    /// Timeout for an established TCP connection (seconds).
    pub tcp_established: u32,
}

impl Default for ConntrackTimeouts {
    fn default() -> Self {
        Self {
            udp: 30,
            udp_stream: 300,
            tcp_established: 7200,
        }
    }
}

/// Expanded NAT configuration produced from a [`Nat`] preset or the builder API.
///
/// This carries all parameters needed to generate nftables rules and
/// conntrack settings for a router's NAT. The presets ([`Nat::Home`],
/// [`Nat::Corporate`], etc.) each expand to a specific `NatConfig` via
/// [`Nat::to_config`]. Custom configurations can be built directly.
///
/// # Example
/// ```
/// # use netsim_core::{NatConfig, NatMapping, NatFiltering};
/// // A home router with shorter UDP timeouts:
/// let cfg = NatConfig::builder()
///     .mapping(NatMapping::EndpointIndependent)
///     .filtering(NatFiltering::AddressAndPortDependent)
///     .udp_stream_timeout(120)
///     .build();
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NatConfig {
    /// How outbound port mapping works.
    pub mapping: NatMapping,
    /// Which inbound packets are forwarded.
    pub filtering: NatFiltering,
    /// Conntrack timeout settings.
    pub timeouts: ConntrackTimeouts,
    /// Whether LAN devices can reach each other via the router's public IP.
    pub hairpin: bool,
}

impl NatConfig {
    /// Returns a builder for constructing a custom NAT configuration.
    pub fn builder() -> NatConfigBuilder {
        NatConfigBuilder::default()
    }
}

/// Builder for [`NatConfig`].
///
/// Defaults to EIM + APDF with standard home-router timeouts.
#[derive(Clone, Debug)]
pub struct NatConfigBuilder {
    mapping: NatMapping,
    filtering: NatFiltering,
    timeouts: ConntrackTimeouts,
    hairpin: bool,
}

impl Default for NatConfigBuilder {
    fn default() -> Self {
        Self {
            mapping: NatMapping::EndpointIndependent,
            filtering: NatFiltering::AddressAndPortDependent,
            timeouts: ConntrackTimeouts::default(),
            hairpin: false,
        }
    }
}

impl NatConfigBuilder {
    /// Sets the mapping behavior.
    pub fn mapping(mut self, mapping: NatMapping) -> Self {
        self.mapping = mapping;
        self
    }

    /// Sets the filtering behavior.
    pub fn filtering(mut self, filtering: NatFiltering) -> Self {
        self.filtering = filtering;
        self
    }

    /// Sets the UDP single-packet timeout (seconds). Default: 30.
    pub fn udp_timeout(mut self, secs: u32) -> Self {
        self.timeouts.udp = secs;
        self
    }

    /// Sets the UDP stream timeout (seconds). Default: 300.
    pub fn udp_stream_timeout(mut self, secs: u32) -> Self {
        self.timeouts.udp_stream = secs;
        self
    }

    /// Sets the TCP established timeout (seconds). Default: 7200.
    pub fn tcp_established_timeout(mut self, secs: u32) -> Self {
        self.timeouts.tcp_established = secs;
        self
    }

    /// Enables or disables NAT hairpinning. Default: false.
    ///
    /// When enabled, LAN devices can reach each other via the router's
    /// public IP (e.g. using a reflexive address learned via STUN).
    pub fn hairpin(mut self, enabled: bool) -> Self {
        self.hairpin = enabled;
        self
    }

    /// Builds the [`NatConfig`].
    pub fn build(self) -> NatConfig {
        NatConfig {
            mapping: self.mapping,
            filtering: self.filtering,
            timeouts: self.timeouts,
            hairpin: self.hairpin,
        }
    }
}

impl Nat {
    /// Expands a preset into its full [`NatConfig`].
    ///
    /// Returns `None` for [`Nat::None`] and [`Nat::Cgnat`] (which use
    /// different code paths — no NAT and ISP-level masquerade respectively).
    pub fn to_config(self) -> Option<NatConfig> {
        match self {
            Nat::None | Nat::Cgnat => None,
            Nat::Home => Some(NatConfig {
                mapping: NatMapping::EndpointIndependent,
                filtering: NatFiltering::AddressAndPortDependent,
                timeouts: ConntrackTimeouts {
                    udp: 30,
                    udp_stream: 300,
                    tcp_established: 7200,
                },
                hairpin: false,
            }),
            Nat::FullCone => Some(NatConfig {
                mapping: NatMapping::EndpointIndependent,
                filtering: NatFiltering::EndpointIndependent,
                timeouts: ConntrackTimeouts {
                    udp: 30,
                    udp_stream: 300,
                    tcp_established: 7200,
                },
                hairpin: true,
            }),
            Nat::Corporate => Some(NatConfig {
                mapping: NatMapping::EndpointDependent,
                filtering: NatFiltering::AddressAndPortDependent,
                timeouts: ConntrackTimeouts {
                    udp: 30,
                    udp_stream: 120,
                    tcp_established: 3600,
                },
                hairpin: false,
            }),
            // CloudNat and Corporate use the same nftables rules (masquerade random)
            // and the same mapping/filtering (EDM + APDF). The only difference is
            // timeout tuning: cloud NAT gateways (AWS, Azure, GCP) use longer UDP
            // stream timeouts (~350s) than corporate firewalls (~120s).
            // See: https://tailscale.com/blog/nat-traversal-improvements-pt-2-cloud-environments
            Nat::CloudNat => Some(NatConfig {
                mapping: NatMapping::EndpointDependent,
                filtering: NatFiltering::AddressAndPortDependent,
                timeouts: ConntrackTimeouts {
                    udp: 30,
                    udp_stream: 350,
                    tcp_established: 3600,
                },
                hairpin: false,
            }),
            Nat::Custom(config) => Some(config),
        }
    }
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

/// Firewall preset for a router's forward chain.
///
/// Firewall rules restrict which traffic can traverse the router.
/// They are applied as nftables rules in a separate `ip fw` table
/// (priority 10, after NAT filter rules at priority 0).
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Firewall {
    /// No filtering beyond NAT (default).
    #[default]
    None,

    /// Corporate/enterprise firewall.
    ///
    /// Allows TCP 80, 443 and UDP 53 (DNS). Blocks all other TCP and UDP.
    /// STUN/ICE fails, must use TURN-over-TLS on port 443.
    ///
    /// Observed on: Cisco ASA, Palo Alto, Fortinet in enterprise deployments.
    Corporate,

    /// Hotel/airport captive-portal style firewall.
    ///
    /// Allows TCP 80, 443, 53 and UDP 53. Blocks all other UDP.
    /// TCP to other ports is allowed (unlike Corporate).
    ///
    /// Observed on: hotel/airport guest WiFi after captive portal auth.
    CaptivePortal,

    /// Fully custom firewall configuration.
    Custom(FirewallConfig),
}

/// Custom firewall configuration for per-port allow/block rules.
///
/// # Example
/// ```
/// # use netsim_core::FirewallConfig;
/// let cfg = FirewallConfig::builder()
///     .allow_tcp(&[80, 443, 8443])
///     .allow_udp(&[53])
///     .block_udp()
///     .build();
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FirewallConfig {
    /// Allowed outbound TCP destination ports. Empty + block_tcp = block all TCP.
    pub allow_tcp: Vec<u16>,
    /// Allowed outbound UDP destination ports. Empty + block_udp = block all UDP.
    pub allow_udp: Vec<u16>,
    /// If true, block TCP not in `allow_tcp`.
    pub block_tcp: bool,
    /// If true, block UDP not in `allow_udp`.
    pub block_udp: bool,
}

impl Firewall {
    /// Expands a preset to a [`FirewallConfig`], or returns `None` for [`Firewall::None`].
    pub fn to_config(&self) -> Option<FirewallConfig> {
        match self {
            Firewall::None => None,
            Firewall::Corporate => Some(FirewallConfig {
                allow_tcp: vec![80, 443],
                allow_udp: vec![53],
                block_tcp: true,
                block_udp: true,
            }),
            Firewall::CaptivePortal => Some(FirewallConfig {
                allow_tcp: vec![80, 443, 53],
                allow_udp: vec![53],
                block_tcp: false,
                block_udp: true,
            }),
            Firewall::Custom(cfg) => Some(cfg.clone()),
        }
    }
}

impl FirewallConfig {
    /// Returns a builder for constructing a custom firewall configuration.
    pub fn builder() -> FirewallConfigBuilder {
        FirewallConfigBuilder::default()
    }
}

/// Builder for [`FirewallConfig`].
#[derive(Clone, Debug, Default)]
pub struct FirewallConfigBuilder {
    allow_tcp: Vec<u16>,
    allow_udp: Vec<u16>,
    block_tcp: bool,
    block_udp: bool,
}

impl FirewallConfigBuilder {
    /// Allow outbound TCP to these destination ports.
    pub fn allow_tcp(&mut self, ports: &[u16]) -> &mut Self {
        self.allow_tcp.extend_from_slice(ports);
        self
    }

    /// Allow outbound UDP to these destination ports.
    pub fn allow_udp(&mut self, ports: &[u16]) -> &mut Self {
        self.allow_udp.extend_from_slice(ports);
        self
    }

    /// Block all outbound TCP not in the allow list.
    pub fn block_tcp(&mut self) -> &mut Self {
        self.block_tcp = true;
        self
    }

    /// Block all outbound UDP not in the allow list.
    pub fn block_udp(&mut self) -> &mut Self {
        self.block_udp = true;
        self
    }

    /// Builds the [`FirewallConfig`].
    pub fn build(&self) -> FirewallConfig {
        FirewallConfig {
            allow_tcp: self.allow_tcp.clone(),
            allow_udp: self.allow_udp.clone(),
            block_tcp: self.block_tcp,
            block_udp: self.block_udp,
        }
    }
}

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
            Manual {
                #[serde(default)]
                rate_kbit: u32,
                #[serde(default, alias = "rate")]
                rate_kbit_alias: Option<u32>,
                #[serde(default)]
                loss_pct: f32,
                #[serde(default, alias = "loss")]
                loss_pct_alias: Option<f32>,
                #[serde(default)]
                latency_ms: u32,
                #[serde(default, alias = "latency")]
                latency_ms_alias: Option<u32>,
                #[serde(default)]
                jitter_ms: u32,
                #[serde(default)]
                reorder_pct: f32,
                #[serde(default)]
                duplicate_pct: f32,
                #[serde(default)]
                corrupt_pct: f32,
            },
        }

        match Repr::deserialize(deserializer)? {
            Repr::Preset(s) => match s.as_str() {
                "lan" => Ok(LinkCondition::Lan),
                "wifi" => Ok(LinkCondition::Wifi),
                "wifi_bad" | "wifi-bad" => Ok(LinkCondition::WifiBad),
                "mobile_4g" | "mobile-4g" | "mobile" => Ok(LinkCondition::Mobile4G),
                "mobile_3g" | "mobile-3g" => Ok(LinkCondition::Mobile3G),
                "satellite" => Ok(LinkCondition::Satellite),
                "satellite_geo" | "satellite-geo" => Ok(LinkCondition::SatelliteGeo),
                _ => Err(serde::de::Error::custom(format!(
                    "unknown impair preset '{s}'"
                ))),
            },
            Repr::Manual {
                rate_kbit,
                rate_kbit_alias,
                loss_pct,
                loss_pct_alias,
                latency_ms,
                latency_ms_alias,
                jitter_ms,
                reorder_pct,
                duplicate_pct,
                corrupt_pct,
            } => Ok(LinkCondition::Manual(LinkLimits {
                rate_kbit: rate_kbit_alias.unwrap_or(rate_kbit),
                loss_pct: loss_pct_alias.unwrap_or(loss_pct),
                latency_ms: latency_ms_alias.unwrap_or(latency_ms),
                jitter_ms,
                reorder_pct,
                duplicate_pct,
                corrupt_pct,
            })),
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

/// Observed external address as reported by a STUN-like reflector.
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
    lab: Arc<Mutex<NetworkCore>>,
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

/// Parameters for an inter-region link.
#[derive(Clone, Debug)]
pub struct RegionLink {
    pub latency_ms: u32,
    pub jitter_ms: u32,
    pub loss_pct: f64,
    pub rate_mbit: u32,
}

impl RegionLink {
    /// Good inter-region link with just latency.
    pub fn good(latency_ms: u32) -> Self {
        Self {
            latency_ms,
            jitter_ms: 0,
            loss_pct: 0.0,
            rate_mbit: 0,
        }
    }

    /// Degraded link with jitter and some loss.
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
    pub us: Region,
    pub eu: Region,
    pub asia: Region,
}

// ─────────────────────────────────────────────
// Lab
// ─────────────────────────────────────────────

/// High-level lab API built on top of `NetworkCore`.
///
/// `Lab` wraps `Arc<Mutex<NetworkCore>>` and is cheaply cloneable. All methods
/// take `&self` and use interior mutability through the mutex.
#[derive(Clone)]
pub struct Lab {
    pub(crate) inner: Arc<Mutex<NetworkCore>>,
}

impl Lab {
    // ── Constructors ────────────────────────────────────────────────────

    /// Creates a new lab with default address ranges and IX settings.
    ///
    /// Sets up the root network namespace and IX bridge before returning.
    pub async fn new() -> Self {
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
            prefix,
            root_ns,
            bridge_tag,
            ix_br: format!("br-p{}{}-1", pid_tag, uniq),
            ix_gw,
            ix_cidr: "198.18.0.0/24".parse().expect("valid ix cidr"),
            private_cidr: "10.0.0.0/16".parse().expect("valid private cidr"),
            public_cidr: "198.18.1.0/24".parse().expect("valid public cidr"),
            ix_gw_v6: "2001:db8::1".parse().expect("valid ix gw v6"),
            ix_cidr_v6: "2001:db8::/32".parse().expect("valid ix cidr v6"),
            private_cidr_v6: "fd10::/48".parse().expect("valid private cidr v6"),
            span: lab_span,
        })
        .expect("Lab::new: failed to create DNS entries directory");
        let lab = Self {
            inner: Arc::new(Mutex::new(core)),
        };
        // Initialize root namespace and IX bridge eagerly — no lazy-init race.
        let (cfg, netns) = {
            let inner = lab.inner.lock().unwrap();
            (inner.cfg.clone(), Arc::clone(&inner.netns))
        };
        setup_root_ns_async(&cfg, &netns)
            .await
            .expect("Lab::new: failed to set up root namespace");
        lab
    }

    /// Returns the unique resource prefix associated with this lab instance.
    pub fn prefix(&self) -> String {
        self.inner.lock().unwrap().cfg.prefix.clone()
    }

    /// Parses `lab.toml`, builds the network, and returns a ready-to-use lab.
    pub async fn load(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path).context("read lab config")?;
        let cfg: crate::config::LabConfig = toml::from_str(&text).context("parse lab config")?;
        Self::from_config(cfg).await
    }

    /// Builds a `Lab` from a parsed config, creating all namespaces and links.
    pub async fn from_config(cfg: crate::config::LabConfig) -> Result<Self> {
        let lab = Self::new().await;

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
                    let inner = lab.inner.lock().unwrap();
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
    /// [`.build()`][RouterBuilder::build] to finalise.
    ///
    /// Default NAT mode is [`Nat::None`] (public DC-style router, IX-connected).
    pub fn add_router(&self, name: &str) -> RouterBuilder {
        let inner = self.inner.lock().unwrap();
        let lab_span = inner.cfg.span.clone();
        if name.starts_with("region_") {
            return RouterBuilder {
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
                result: Err(anyhow!(
                    "router names starting with 'region_' are reserved"
                )),
            };
        }
        if inner.router_id_by_name(name).is_some() {
            return RouterBuilder {
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
                result: Err(anyhow!("router '{}' already exists", name)),
            };
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
        let mut inner = self.inner.lock().unwrap();
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
        let (ns, netns) = {
            let mut inner = self.inner.lock().unwrap();
            let dev = inner
                .device(id)
                .ok_or_else(|| anyhow!("unknown device id {:?}", id))?;
            let ns = dev.ns.clone();
            let netns = Arc::clone(&inner.netns);
            inner.remove_device(id);
            (ns, netns)
        };
        netns.remove_worker(&ns);
        Ok(())
    }

    /// Removes a router from the lab, destroying its namespace and all interfaces.
    ///
    /// Fails if any devices are still connected to this router's downstream switch.
    /// Remove or replug those devices first.
    pub fn remove_router(&self, id: NodeId) -> Result<()> {
        let (ns, netns) = {
            let mut inner = self.inner.lock().unwrap();
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

            let netns = Arc::clone(&inner.netns);
            inner.remove_router(id);
            (ns, netns)
        };
        netns.remove_worker(&ns);
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
        let (id, setup_data, netns, idx) = {
            let mut inner = self.inner.lock().unwrap();
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
            let region_bridge_cidr: Ipv4Net = format!(
                "198.18.{}.0/24",
                idx as u16 * 16
            )
            .parse()
            .context("region bridge CIDR")?;
            let sub_switch = inner.add_switch(
                &format!("{region_router_name}-sub"),
                None,
                None,
                None,
                None,
            );
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
                    next_host: 10,
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
            let return_route = if let (Some(cidr), Some(via)) =
                (router.downstream_cidr, router.upstream_ip)
            {
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

            let netns = Arc::clone(&inner.netns);
            (id, setup_data, netns, idx)
        }; // lock released

        // Phase 2: Async network setup (no lock held).
        setup_router_async(&netns, &setup_data).await?;

        // Phase 3: Add /20 aggregate route in root NS for the region.
        let region_net: Ipv4Addr = format!("198.18.{}.0", idx as u16 * 16)
            .parse()
            .context("region aggregate net")?;
        let via = setup_data.router.upstream_ip.context("region router has no IX IP")?;
        let root_ns = setup_data.root_ns.clone();
        core::nl_run(&netns, &root_ns, move |h: Netlink| async move {
            h.add_route_v4(region_net, 20, via).await.ok();
            Ok(())
        })
        .await?;

        Ok(Region {
            name: name.to_string(),
            idx,
            router_id: id,
            lab: Arc::clone(&self.inner),
        })
    }

    /// Links two regions with a veth pair and applies netem impairment.
    ///
    /// Creates a point-to-point veth between the two region router namespaces,
    /// assigns /30 addresses from 203.0.113.0/24, applies tc netem on both ends,
    /// and adds /20 routes so each region can reach the other.
    pub async fn link_regions(&self, a: &Region, b: &Region, link: RegionLink) -> Result<()> {
        let (a_ns, b_ns, a_idx, b_idx, netns, root_ns);
        let (ip_a, ip_b);
        let link_key;
        {
            let mut inner = self.inner.lock().unwrap();

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
            netns = Arc::clone(&inner.netns);

            // Allocate /30 from 203.0.113.0/24.
            let (ipa, ipb) = inner.alloc_interregion_ips()?;
            ip_a = ipa;
            ip_b = ipb;

            let ifname_a = format!("vr-{}-{}", a.name, b.name);
            let ifname_b = format!("vr-{}-{}", b.name, a.name);
            // Store IPs in sorted key order: ip_a belongs to link_key.0, ip_b to link_key.1.
            let (stored_ip_a, stored_ip_b) = if a.name < b.name {
                (ip_a, ip_b)
            } else {
                (ip_b, ip_a)
            };
            let (stored_if_a, stored_if_b) = if a.name < b.name {
                (ifname_a.clone(), ifname_b.clone())
            } else {
                (ifname_b.clone(), ifname_a.clone())
            };
            inner.region_links.insert(
                link_key.clone(),
                crate::core::RegionLinkData {
                    ifname_a: stored_if_a,
                    ifname_b: stored_if_b,
                    ip_a: stored_ip_a,
                    ip_b: stored_ip_b,
                    broken: false,
                },
            );
        } // lock released

        let veth_a = format!("vr-{}-{}", a.name, b.name);
        let veth_b = format!("vr-{}-{}", b.name, a.name);

        // Create veth pair in root NS, then move ends to region router NSes.
        let veth_a2 = veth_a.clone();
        let veth_b2 = veth_b.clone();
        let a_ns_fd = netns.ns_fd(&a_ns)?;
        let b_ns_fd = netns.ns_fd(&b_ns)?;
        core::nl_run(&netns, &root_ns, move |h: Netlink| async move {
            h.ensure_link_deleted(&veth_a2).await.ok();
            h.add_veth(&veth_a2, &veth_b2).await?;
            h.move_link_to_netns(&veth_a2, &a_ns_fd).await?;
            h.move_link_to_netns(&veth_b2, &b_ns_fd).await?;
            Ok(())
        })
        .await?;

        // Configure side A: assign IP, bring up, add route to B's /20.
        let veth_a3 = veth_a.clone();
        let b_region_net: Ipv4Addr = format!("198.18.{}.0", b_idx as u16 * 16).parse()?;
        core::nl_run(&netns, &a_ns, move |h: Netlink| async move {
            h.add_addr4(&veth_a3, ip_a, 30).await?;
            h.set_link_up(&veth_a3).await?;
            h.add_route_v4(b_region_net, 20, ip_b).await?;
            Ok(())
        })
        .await?;

        // Configure side B: assign IP, bring up, add route to A's /20.
        let veth_b3 = veth_b.clone();
        let a_region_net: Ipv4Addr = format!("198.18.{}.0", a_idx as u16 * 16).parse()?;
        core::nl_run(&netns, &b_ns, move |h: Netlink| async move {
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
            let limits_a = limits.clone();
            netns.run_closure_in(&a_ns, move || {
                crate::qdisc::apply_impair(&veth_a4, limits_a)
            })?;
            let veth_b4 = veth_b.clone();
            netns.run_closure_in(&b_ns, move || {
                crate::qdisc::apply_impair(&veth_b4, limits)
            })?;
        }

        Ok(())
    }

    /// Breaks the direct link between two regions, rerouting through an intermediate.
    ///
    /// Finds a third region `m` that has non-broken links to both `a` and `b`,
    /// and replaces the direct routes with routes through `m`. Traffic will
    /// traverse two inter-region hops instead of one.
    pub fn break_region_link(&self, a: &Region, b: &Region) -> Result<()> {
        let inner = self.inner.lock().unwrap();

        let link_key = Self::region_link_key(&a.name, &b.name);
        let link = inner
            .region_links
            .get(&link_key)
            .ok_or_else(|| anyhow!("no link between '{}' and '{}'", a.name, b.name))?;
        if link.broken {
            bail!("link between '{}' and '{}' is already broken", a.name, b.name);
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
        let netns = Arc::clone(&inner.netns);

        // On region_a: replace route to b's /20 via m (on a↔m veth)
        let b_net: Ipv4Addr = format!("198.18.{}.0", b.idx as u16 * 16).parse()?;
        let a_via = m_ip_on_ma;
        netns.run_closure_in(&a_ns, move || {
            let status = Command::new("ip")
                .args(["route", "replace", &format!("{b_net}/20"), "via", &a_via.to_string()])
                .status()
                .context("ip route replace")?;
            if !status.success() {
                bail!("ip route replace failed");
            }
            Ok(())
        })?;

        // On region_b: replace route to a's /20 via m (on b↔m veth)
        let a_net: Ipv4Addr = format!("198.18.{}.0", a.idx as u16 * 16).parse()?;
        let b_via = m_ip_on_mb;
        netns.run_closure_in(&b_ns, move || {
            let status = Command::new("ip")
                .args(["route", "replace", &format!("{a_net}/20"), "via", &b_via.to_string()])
                .status()
                .context("ip route replace")?;
            if !status.success() {
                bail!("ip route replace failed");
            }
            Ok(())
        })?;

        // Mark link as broken.
        drop(inner);
        self.inner
            .lock()
            .unwrap()
            .region_links
            .get_mut(&link_key)
            .unwrap()
            .broken = true;
        Ok(())
    }

    /// Restores a previously broken direct link between two regions.
    pub fn restore_region_link(&self, a: &Region, b: &Region) -> Result<()> {
        let inner = self.inner.lock().unwrap();

        let link_key = Self::region_link_key(&a.name, &b.name);
        let link = inner
            .region_links
            .get(&link_key)
            .ok_or_else(|| anyhow!("no link between '{}' and '{}'", a.name, b.name))?;
        if !link.broken {
            bail!("link between '{}' and '{}' is not broken", a.name, b.name);
        }

        // Restore direct routes using the a↔b veth IPs.
        let a_ns = inner.router(a.router_id).unwrap().ns.clone();
        let b_ns = inner.router(b.router_id).unwrap().ns.clone();
        let netns = Arc::clone(&inner.netns);

        // Direct route on a: b's /20 via b's IP on the a↔b veth.
        let b_net: Ipv4Addr = format!("198.18.{}.0", b.idx as u16 * 16).parse()?;
        let b_direct_ip = if link_key.0 == a.name {
            link.ip_b
        } else {
            link.ip_a
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
        let a_net: Ipv4Addr = format!("198.18.{}.0", a.idx as u16 * 16).parse()?;
        let a_direct_ip = if link_key.0 == a.name {
            link.ip_a
        } else {
            link.ip_b
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
        drop(inner);
        self.inner
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
    pub fn env_vars(&self) -> std::collections::HashMap<String, String> {
        let inner = self.inner.lock().unwrap();
        let mut map = std::collections::HashMap::new();
        for dev in inner.all_devices() {
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

    /// Returns a handle to the IX (Internet Exchange) root namespace.
    pub fn ix(&self) -> Ix {
        Ix {
            lab: Arc::clone(&self.inner),
        }
    }

    /// Safety-net cleanup: drops fd-registry entries for this lab's prefix.
    /// Normal cleanup happens in `NetworkCore::drop`.
    pub fn cleanup(&self) {
        let inner = self.inner.lock().unwrap();
        inner.netns.cleanup_prefix(&inner.cfg.prefix);
    }

    // ── DNS entries ───────────────────────────────────────────────────────

    /// Adds a hosts entry visible to all devices.
    ///
    /// The entry is written to each device's hosts file overlay. Worker threads
    /// (sync, async, and tokio blocking pool) have `/etc/hosts` bind-mounted, so
    /// glibc picks up changes on the next `getaddrinfo()` via mtime check.
    pub fn dns_entry(&self, name: &str, ip: std::net::IpAddr) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.dns.global.push((name.to_string(), ip));
        let ids: Vec<_> = inner.all_device_ids();
        inner.dns.write_all_hosts_files(&ids)?;
        Ok(())
    }

    /// Resolves a name from the lab-wide DNS entries (in-memory, no syscall).
    pub fn resolve(&self, name: &str) -> Option<std::net::IpAddr> {
        let inner = self.inner.lock().unwrap();
        inner.dns.resolve(None, name)
    }

    /// Sets the nameserver for all devices (writes `/etc/resolv.conf` overlay).
    ///
    /// Worker threads have `/etc/resolv.conf` bind-mounted, so glibc picks up
    /// changes on the next resolver call.
    pub fn set_nameserver(&self, server: std::net::IpAddr) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
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
    pub fn set_link_condition(&self, a: NodeId, b: NodeId, impair: Option<LinkCondition>) -> Result<()> {
        debug!(a = ?a, b = ?b, impair = ?impair, "lab: set_link_condition");
        let inner = self.inner.lock().unwrap();
        let netns = Arc::clone(&inner.netns);

        // Try Device ↔ Router in both orderings.
        for (dev_id, router_id) in [(a, b), (b, a)] {
            if let (Some(dev), Some(router)) = (inner.device(dev_id), inner.router(router_id)) {
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
                apply_or_remove_impair(&netns, &dev.ns, &iface.ifname, impair);
                return Ok(());
            }
        }

        // Try Router(a) ↔ Router(b) — one must be upstream of the other.
        if let (Some(ra), Some(rb)) = (inner.router(a), inner.router(b)) {
            // Check if b is downstream of a (b.uplink points to a's downlink switch).
            if let Some(a_downlink) = ra.downlink {
                if rb.uplink == Some(a_downlink) {
                    let wan_if = rb.wan_ifname(inner.ix_sw()).to_string();
                    apply_or_remove_impair(&netns, &rb.ns, &wan_if, impair);
                    return Ok(());
                }
            }
            // Check if a is downstream of b.
            if let Some(b_downlink) = rb.downlink {
                if ra.uplink == Some(b_downlink) {
                    let wan_if = ra.wan_ifname(inner.ix_sw()).to_string();
                    apply_or_remove_impair(&netns, &ra.ns, &wan_if, impair);
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


    // ── Lookup helpers ───────────────────────────────────────────────────

    /// Returns a device handle by id, or `None` if the id is not a device.
    pub fn device(&self, id: NodeId) -> Option<Device> {
        let inner = self.inner.lock().unwrap();
        inner.device(id).map(|_| Device {
            id,
            lab: Arc::clone(&self.inner),
        })
    }

    /// Returns a router handle by id, or `None` if the id is not a router.
    pub fn router(&self, id: NodeId) -> Option<Router> {
        let inner = self.inner.lock().unwrap();
        inner.router(id).map(|_| Router {
            id,
            lab: Arc::clone(&self.inner),
        })
    }

    /// Looks up a device by name and returns a handle.
    pub fn device_by_name(&self, name: &str) -> Option<Device> {
        let inner = self.inner.lock().unwrap();
        inner.device_id_by_name(name).map(|id| Device {
            id,
            lab: Arc::clone(&self.inner),
        })
    }

    /// Looks up a router by name and returns a handle.
    pub fn router_by_name(&self, name: &str) -> Option<Router> {
        let inner = self.inner.lock().unwrap();
        inner.router_id_by_name(name).map(|id| Router {
            id,
            lab: Arc::clone(&self.inner),
        })
    }

    /// Returns handles for all devices.
    pub fn devices(&self) -> Vec<Device> {
        let inner = self.inner.lock().unwrap();
        inner
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
// RouterBuilder
// ─────────────────────────────────────────────

/// Builder for a router node; returned by [`Lab::add_router`].
pub struct RouterBuilder {
    inner: Arc<Mutex<NetworkCore>>,
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

    /// Connects this router as a subscriber behind `parent`'s downstream switch.
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

    /// Finalises the router, creates its namespace and links, and returns a [`Router`] handle.
    pub async fn build(self) -> Result<Router> {
        self.result?;

        // Phase 1: Lock → register topology + extract snapshot → unlock.
        let (id, setup_data, netns) = {
            let mut inner = self.inner.lock().unwrap();
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

            let netns = Arc::clone(&inner.netns);
            (id, setup_data, netns)
        }; // lock released

        // Phase 2: Async network setup (no lock held).
        async {
            setup_router_async(&netns, &setup_data).await
        }
        .instrument(self.lab_span.clone())
        .await?;

        let router = Router {
            id,
            lab: Arc::clone(&self.inner),
        };
        if let Some(cond) = self.downlink_condition {
            router.set_downlink_condition(Some(cond))?;
        }
        Ok(router)
    }
}

// ─────────────────────────────────────────────
// DeviceBuilder
// ─────────────────────────────────────────────

/// Builder for a device node; returned by [`Lab::add_device`].
pub struct DeviceBuilder {
    inner: Arc<Mutex<NetworkCore>>,
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
                let inner = self.inner.lock().unwrap();
                inner
                    .device(self.id)
                    .map(|d| d.interfaces.len())
                    .unwrap_or(0)
            };
            let ifname = format!("eth{}", idx);
            self.result = self
                .inner
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
        let (dev, ifaces, prefix, root_ns, netns, dns_overlay) = {
            let mut inner = self.inner.lock().unwrap();
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
            let netns = Arc::clone(&inner.netns);
            (dev, iface_data, prefix, root_ns, netns, overlay)
        }; // lock released

        // Phase 2: Async network setup (no lock held).
        // The DNS overlay is passed to create_named_netns so worker threads
        // get /etc/hosts and /etc/resolv.conf bind-mounted at startup.
        async {
            setup_device_async(&netns, &prefix, &root_ns, &dev, ifaces, Some(dns_overlay)).await
        }
        .instrument(self.lab_span.clone())
        .await?;

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
    ip: Option<Ipv4Addr>,
    ip_v6: Option<Ipv6Addr>,
    impair: Option<LinkCondition>,
}

impl DeviceIface {
    /// Returns the interface name (e.g. `"eth0"`).
    pub fn name(&self) -> &str {
        &self.ifname
    }

    /// Returns the assigned IPv4 address, if any.
    pub fn ip(&self) -> Option<Ipv4Addr> {
        self.ip
    }

    /// Returns the assigned IPv6 address, if any.
    pub fn ip6(&self) -> Option<Ipv6Addr> {
        self.ip_v6
    }

    /// Returns the impairment profile, if any.
    pub fn impair(&self) -> Option<LinkCondition> {
        self.impair
    }
}

/// Cloneable handle to a device in the lab topology.
///
/// Holds a `NodeId` and an `Arc` to the lab interior. All accessor methods
/// briefly lock the mutex, read a value, and return owned data.
pub struct Device {
    id: NodeId,
    lab: Arc<Mutex<NetworkCore>>,
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
            .device(self.id)
            .map(|d| d.name.clone())
            .unwrap_or_default()
    }

    /// Returns the network namespace name for this device.
    pub fn ns(&self) -> String {
        let inner = self.lab.lock().unwrap();
        inner
            .device(self.id)
            .map(|d| d.ns.clone())
            .unwrap_or_default()
    }

    /// Returns the IPv4 address of the default interface, if assigned.
    pub fn ip(&self) -> Option<Ipv4Addr> {
        let inner = self.lab.lock().unwrap();
        inner.device(self.id).and_then(|d| d.default_iface().ip)
    }

    /// Returns the IPv6 address of the default interface, if assigned.
    pub fn ip6(&self) -> Option<Ipv6Addr> {
        let inner = self.lab.lock().unwrap();
        inner.device(self.id).and_then(|d| d.default_iface().ip_v6)
    }

    /// Returns the configured MTU, if set.
    pub fn mtu(&self) -> Option<u32> {
        let inner = self.lab.lock().unwrap();
        inner.device(self.id).and_then(|d| d.mtu)
    }

    /// Returns a snapshot of the named interface, if it exists.
    pub fn iface(&self, name: &str) -> Option<DeviceIface> {
        let inner = self.lab.lock().unwrap();
        let dev = inner.device(self.id)?;
        let iface = dev.iface(name)?;
        Some(DeviceIface {
            ifname: iface.ifname.clone(),
            ip: iface.ip,
            ip_v6: iface.ip_v6,
            impair: iface.impair,
        })
    }

    /// Returns a snapshot of the default interface.
    pub fn default_iface(&self) -> DeviceIface {
        let inner = self.lab.lock().unwrap();
        let dev = inner.device(self.id).expect("device handle has valid id");
        let iface = dev.default_iface();
        DeviceIface {
            ifname: iface.ifname.clone(),
            ip: iface.ip,
            ip_v6: iface.ip_v6,
            impair: iface.impair,
        }
    }

    /// Returns snapshots of all interfaces.
    pub fn interfaces(&self) -> Vec<DeviceIface> {
        let inner = self.lab.lock().unwrap();
        let dev = match inner.device(self.id) {
            Some(d) => d,
            None => return vec![],
        };
        dev.interfaces
            .iter()
            .map(|iface| DeviceIface {
                ifname: iface.ifname.clone(),
                ip: iface.ip,
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
                .device(self.id)
                .ok_or_else(|| anyhow!("unknown device id"))?;
            (dev.ns.clone(), Arc::clone(&inner.netns))
        };
        let ifname = ifname.to_string();
        core::nl_run(&netns, &ns, move |nl: Netlink| async move {
            nl.set_link_down(&ifname).await
        })
        .await
    }

    /// Brings an interface administratively up.
    ///
    /// Linux removes routes via an interface when it goes admin-down, so we
    /// re-add the default route if `ifname` is the device's current `default_via`.
    pub async fn link_up(&self, ifname: &str) -> Result<()> {
        let (ns, uplink, is_default_via, netns) = {
            let inner = self.lab.lock().unwrap();
            let dev = inner
                .device(self.id)
                .ok_or_else(|| anyhow!("unknown device id"))?;
            let iface = dev
                .iface(ifname)
                .ok_or_else(|| anyhow!("interface '{}' not found", ifname))?;
            (
                dev.ns.clone(),
                iface.uplink,
                dev.default_via == ifname,
                Arc::clone(&inner.netns),
            )
        };
        let ifname_owned = ifname.to_string();
        core::nl_run(&netns, &ns, {
            let ifname_owned = ifname_owned.clone();
            move |nl: Netlink| async move { nl.set_link_up(&ifname_owned).await }
        })
        .await?;
        if is_default_via {
            let gw_ip = self
                .lab
                .lock()
                .unwrap()
                .router_downlink_gw_for_switch(uplink)?;
            core::nl_run(&netns, &ns, move |nl: Netlink| async move {
                nl.replace_default_route_v4(&ifname_owned, gw_ip).await
            })
            .await?;
        }
        Ok(())
    }

    /// Sets the active default route to a different interface.
    pub async fn set_default_route(&self, to: &str) -> Result<()> {
        let (ns, uplink, impair, netns) = {
            let inner = self.lab.lock().unwrap();
            let dev = inner
                .device(self.id)
                .ok_or_else(|| anyhow!("unknown device id"))?;
            let iface = dev
                .iface(to)
                .ok_or_else(|| anyhow!("interface '{}' not found", to))?;
            (
                dev.ns.clone(),
                iface.uplink,
                iface.impair,
                Arc::clone(&inner.netns),
            )
        };
        let gw_ip = self
            .lab
            .lock()
            .unwrap()
            .router_downlink_gw_for_switch(uplink)?;
        let to_owned = to.to_string();
        core::nl_run(&netns, &ns, move |nl: Netlink| async move {
            nl.replace_default_route_v4(&to_owned, gw_ip).await
        })
        .await?;
        apply_or_remove_impair(&netns, &ns, to, impair);
        self.lab
            .lock()
            .unwrap()
            .set_device_default_via(self.id, to)?;
        Ok(())
    }


    /// Applies or removes a link-layer impairment on the named interface.
    pub fn set_link_condition(&self, ifname: &str, impair: Option<LinkCondition>) -> Result<()> {
        let mut inner = self.lab.lock().unwrap();
        let (ns, resolved_ifname, netns) = {
            let dev = inner
                .device(self.id)
                .ok_or_else(|| anyhow!("unknown device id"))?;
            let iname = ifname.to_string();
            if dev.iface(&iname).is_none() {
                bail!("interface '{}' not found", iname);
            }
            (dev.ns.clone(), iname, Arc::clone(&inner.netns))
        };
        apply_or_remove_impair(&netns, &ns, &resolved_ifname, impair);
        if let Some(dev) = inner.device_mut(self.id) {
            if let Some(iface) = dev.iface_mut(&resolved_ifname) {
                iface.impair = impair;
            }
        }
        Ok(())
    }


    // ── Spawn / run ────────────────────────────────────────────────────

    /// Spawns an async task in this device's network namespace.
    ///
    /// The closure receives a cloned [`Device`] handle. Returns a
    /// `JoinHandle` that resolves to the task's output.
    pub fn spawn<F, Fut, T>(&self, f: F) -> tokio::task::JoinHandle<T>
    where
        F: FnOnce(Device) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let inner = self.lab.lock().unwrap();
        let ns = &inner
            .device(self.id)
            .expect("device handle has valid id")
            .ns;
        let rt = inner.rt_handle_for(ns).expect("namespace has async worker");
        let handle = self.clone();
        rt.spawn(f(handle))
    }

    /// Runs a short-lived sync closure in this device's network namespace.
    /// Blocks the caller until the closure returns.
    ///
    /// Only for fast, non-blocking work. Never pass TCP/UDP I/O here.
    pub fn run_sync<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let (ns, netns) = {
            let inner = self.lab.lock().unwrap();
            let dev = inner
                .device(self.id)
                .ok_or_else(|| anyhow!("unknown device id"))?;
            (dev.ns.clone(), Arc::clone(&inner.netns))
        };
        netns.run_closure_in(&ns, f)
    }

    /// Spawns a dedicated OS thread in this device's network namespace.
    pub fn spawn_thread<F, R>(&self, f: F) -> Result<thread::JoinHandle<Result<R>>>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let (ns, netns) = {
            let inner = self.lab.lock().unwrap();
            let dev = inner
                .device(self.id)
                .ok_or_else(|| anyhow!("unknown device id"))?;
            (dev.ns.clone(), Arc::clone(&inner.netns))
        };
        netns.spawn_thread_in(&ns, f)
    }

    /// Spawns a raw command in this device's network namespace.
    ///
    /// The sync worker thread has `/etc/hosts` and `/etc/resolv.conf` bind-mounted.
    /// `fork()` inherits the mount namespace, so child processes automatically see
    /// the DNS overlay without a separate `pre_exec` hook.
    pub fn spawn_command(&self, mut cmd: Command) -> Result<std::process::Child> {
        let (ns, netns) = {
            let inner = self.lab.lock().unwrap();
            let dev = inner
                .device(self.id)
                .ok_or_else(|| anyhow!("unknown device id"))?;
            (dev.ns.clone(), Arc::clone(&inner.netns))
        };
        netns.run_closure_in(&ns, move || {
            cmd.spawn().context("spawn command in namespace")
        })
    }

    /// Spawns an async command in this device's network namespace.
    ///
    /// The child is registered with the namespace's tokio reactor so that
    /// `.wait()` and `.wait_with_output()` work as non-blocking futures.
    pub fn spawn_command_async(
        &self,
        mut cmd: tokio::process::Command,
    ) -> Result<tokio::process::Child> {
        let (ns, rt, netns) = {
            let inner = self.lab.lock().unwrap();
            let dev = inner
                .device(self.id)
                .ok_or_else(|| anyhow!("unknown device id"))?;
            let rt = inner.rt_handle_for(&dev.ns)?;
            (dev.ns.clone(), rt, Arc::clone(&inner.netns))
        };
        netns.run_closure_in(&ns, move || {
            let _guard = rt.enter();
            cmd.spawn().context("spawn async command in namespace")
        })
    }

    /// Probes the NAT mapping seen by a reflector from this device.
    pub fn probe_udp_mapping(&self, reflector: SocketAddr) -> Result<ObservedAddr> {
        let base = 40000u16;
        let port = base + ((self.id.0 % 20000) as u16);
        self.run_sync(move || {
            crate::test_utils::probe_udp(reflector, Duration::from_millis(500), Some(port))
        })
    }

    /// Spawns a UDP reflector in this device's network namespace.
    pub fn spawn_reflector(&self, bind: SocketAddr) -> Result<()> {
        let inner = self.lab.lock().unwrap();
        let ns = &inner
            .device(self.id)
            .ok_or_else(|| anyhow!("unknown device id"))?
            .ns;
        inner.spawn_reflector_in(ns, bind)
    }

    /// Adds a hosts entry visible only to this device.
    ///
    /// Written to this device's hosts file overlay. glibc picks up changes
    /// on the next `getaddrinfo()` via mtime check.
    pub fn dns_entry(&self, name: &str, ip: std::net::IpAddr) -> Result<()> {
        let mut inner = self.lab.lock().unwrap();
        inner
            .dns
            .per_device
            .entry(self.id)
            .or_default()
            .push((name.to_string(), ip));
        inner.dns.write_hosts_file(self.id)
    }

    /// Resolves a name using this device's entries + lab-wide entries.
    /// For in-process Rust code that can't see the bind-mounted `/etc/hosts`.
    pub fn resolve(&self, name: &str) -> Option<std::net::IpAddr> {
        let inner = self.lab.lock().unwrap();
        inner.dns.resolve(Some(self.id), name)
    }

    /// Moves one of this device's interfaces to a different router's downstream
    /// network, simulating unplugging a cable and plugging it into a new router.
    ///
    /// The interface name is preserved but the IP address changes (allocated from
    /// the new router's pool). The old veth pair is torn down and a fresh one is
    /// created.
    pub async fn replug_iface(&self, ifname: &str, to_router: NodeId) -> Result<()> {
        use crate::core::{self, IfaceBuild};

        // Phase 1: Lock → extract data + allocate from new router's pool → unlock
        let (iface_build, netns, prefix, root_ns) = {
            let mut inner = self.lab.lock().unwrap();
            let dev = inner
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
                .switch(downlink_sw)
                .ok_or_else(|| anyhow!("target router's downlink switch missing"))?
                .clone();
            let gw_br = sw.bridge.clone().unwrap_or_else(|| "br-lan".to_string());
            let new_ip = if sw.cidr.is_some() {
                Some(inner.alloc_from_switch(downlink_sw)?)
            } else {
                None
            };
            let new_ip_v6 = if sw.cidr_v6.is_some() {
                Some(inner.alloc_from_switch_v6(downlink_sw)?)
            } else {
                None
            };
            let prefix_len = sw.cidr.map(|c| c.prefix_len()).unwrap_or(24);

            let netns = Arc::clone(&inner.netns);
            let prefix = inner.cfg.prefix.clone();
            let root_ns = inner.cfg.root_ns.clone();

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
            (build, netns, prefix, root_ns)
        };

        // Phase 2: Delete old veth pair.
        // The veth ends were moved out of root NS during initial setup:
        // the device end lives as `ifname` in dev_ns, the gateway end as
        // `v{idx}` in the old router's NS.  Deleting one end destroys both.
        let dev_ns = iface_build.dev_ns.clone();
        let ifname_owned = ifname.to_string();
        core::nl_run(&netns, &dev_ns, move |h: Netlink| async move {
            h.ensure_link_deleted(&ifname_owned).await.ok();
            Ok(())
        })
        .await?;

        // Phase 3: Wire new interface (reuses existing wiring logic)
        let new_ip = iface_build.dev_ip;
        let new_ip_v6 = iface_build.dev_ip_v6;
        let new_uplink = {
            let inner = self.lab.lock().unwrap();
            inner.router(to_router).unwrap().downlink.unwrap()
        };
        core::wire_iface_async(&netns, &prefix, &root_ns, iface_build).await?;

        // Phase 4: Lock → update internal records → unlock
        {
            let mut inner = self.lab.lock().unwrap();
            let dev = inner
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

    /// Simulates DHCP renewal: allocates a new IP from the current router's pool,
    /// replaces the old address on the interface, and returns the new IP.
    ///
    /// The default route remains unchanged (same gateway).
    pub async fn renew_ip(&self, ifname: &str) -> Result<Ipv4Addr> {
        use crate::core;

        // Phase 1: Lock → allocate new IP, update records → unlock
        let (ns, netns, old_ip, new_ip, prefix_len) = {
            let mut inner = self.lab.lock().unwrap();
            let dev = inner
                .device(self.id)
                .ok_or_else(|| anyhow!("unknown device id"))?;
            let iface = dev
                .iface(ifname)
                .ok_or_else(|| anyhow!("device '{}' has no interface '{}'", dev.name, ifname))?;
            let old_ip = iface
                .ip
                .ok_or_else(|| anyhow!("interface '{}' has no IPv4 address", ifname))?;
            let sw_id = iface.uplink;
            let sw = inner
                .switch(sw_id)
                .ok_or_else(|| anyhow!("switch for interface '{}' missing", ifname))?;
            let prefix_len = sw.cidr.map(|c| c.prefix_len()).unwrap_or(24);
            let ns = dev.ns.clone();
            let netns = Arc::clone(&inner.netns);

            let new_ip = inner.alloc_from_switch(sw_id)?;
            // Update internal record.
            let dev = inner.device_mut(self.id).unwrap();
            let iface = dev.iface_mut(ifname).unwrap();
            iface.ip = Some(new_ip);

            (ns, netns, old_ip, new_ip, prefix_len)
        };

        // Phase 2: Async netlink — remove old addr, add new addr.
        let ifname = ifname.to_string();
        core::nl_run(&netns, &ns, move |h: Netlink| async move {
            h.del_addr4(&ifname, old_ip, prefix_len).await?;
            h.add_addr4(&ifname, new_ip, prefix_len).await?;
            Ok(())
        })
        .await?;

        Ok(new_ip)
    }

    /// Adds a secondary IPv4 address to an interface.
    ///
    /// The address is added via netlink without removing existing addresses.
    /// Linux natively supports multiple addresses per interface.
    pub async fn add_ip(&self, ifname: &str, ip: Ipv4Addr, prefix_len: u8) -> Result<()> {
        use crate::core;

        let (ns, netns) = {
            let inner = self.lab.lock().unwrap();
            let dev = inner
                .device(self.id)
                .ok_or_else(|| anyhow!("unknown device id"))?;
            let _ = dev
                .iface(ifname)
                .ok_or_else(|| anyhow!("device '{}' has no interface '{}'", dev.name, ifname))?;
            (dev.ns.clone(), Arc::clone(&inner.netns))
        };

        let ifname = ifname.to_string();
        core::nl_run(&netns, &ns, move |h: Netlink| async move {
            h.add_addr4(&ifname, ip, prefix_len).await?;
            Ok(())
        })
        .await?;

        Ok(())
    }

}

/// Cloneable handle to a router in the lab topology.
///
/// Same pattern as [`Device`]: holds `NodeId` + `Arc<Mutex<NetworkCore>>`.
pub struct Router {
    id: NodeId,
    lab: Arc<Mutex<NetworkCore>>,
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
            .router(self.id)
            .map(|r| r.name.clone())
            .unwrap_or_default()
    }

    /// Returns the network namespace name for this router.
    pub fn ns(&self) -> String {
        let inner = self.lab.lock().unwrap();
        inner
            .router(self.id)
            .map(|r| r.ns.clone())
            .unwrap_or_default()
    }

    /// Returns the region label, if set.
    pub fn region(&self) -> Option<String> {
        let inner = self.lab.lock().unwrap();
        inner.router(self.id).and_then(|r| r.region.clone())
    }

    /// Returns the NAT mode.
    pub fn nat_mode(&self) -> Nat {
        let inner = self.lab.lock().unwrap();
        inner.router(self.id).map(|r| r.cfg.nat).unwrap_or_default()
    }

    /// Returns the configured MTU, if set.
    pub fn mtu(&self) -> Option<u32> {
        let inner = self.lab.lock().unwrap();
        inner.router(self.id).and_then(|r| r.cfg.mtu)
    }

    /// Returns the uplink (WAN-side) IP, if connected.
    pub fn uplink_ip(&self) -> Option<Ipv4Addr> {
        let inner = self.lab.lock().unwrap();
        inner.router(self.id).and_then(|r| r.upstream_ip)
    }

    /// Returns the downstream subnet CIDR, if allocated.
    pub fn downstream_cidr(&self) -> Option<Ipv4Net> {
        let inner = self.lab.lock().unwrap();
        inner.router(self.id).and_then(|r| r.downstream_cidr)
    }

    /// Returns the downstream gateway address, if allocated.
    pub fn downstream_gw(&self) -> Option<Ipv4Addr> {
        let inner = self.lab.lock().unwrap();
        inner.router(self.id).and_then(|r| r.downstream_gw)
    }

    /// Returns which IP address families this router supports.
    pub fn ip_support(&self) -> IpSupport {
        let inner = self.lab.lock().unwrap();
        inner
            .router(self.id)
            .map(|r| r.cfg.ip_support)
            .unwrap_or_default()
    }

    /// Returns the uplink (WAN-side) IPv6 address, if connected.
    pub fn uplink_ip_v6(&self) -> Option<Ipv6Addr> {
        let inner = self.lab.lock().unwrap();
        inner.router(self.id).and_then(|r| r.upstream_ip_v6)
    }

    /// Returns the downstream IPv6 subnet CIDR, if allocated.
    pub fn downstream_cidr_v6(&self) -> Option<Ipv6Net> {
        let inner = self.lab.lock().unwrap();
        inner.router(self.id).and_then(|r| r.downstream_cidr_v6)
    }

    /// Returns the downstream IPv6 gateway address, if allocated.
    pub fn downstream_gw_v6(&self) -> Option<Ipv6Addr> {
        let inner = self.lab.lock().unwrap();
        inner.router(self.id).and_then(|r| r.downstream_gw_v6)
    }

    /// Returns the IPv6 NAT mode.
    pub fn nat_v6_mode(&self) -> NatV6Mode {
        let inner = self.lab.lock().unwrap();
        inner
            .router(self.id)
            .map(|r| r.cfg.nat_v6)
            .unwrap_or_default()
    }

    // ── Dynamic operations ──────────────────────────────────────────────

    /// Replaces NAT rules on this router at runtime.
    ///
    /// Flushes the `ip nat` table then re-applies the new rules.
    pub fn set_nat_mode(&self, mode: Nat) -> Result<()> {
        let (ns, wan_if, wan_ip, netns, cfg) = {
            let mut inner = self.lab.lock().unwrap();
            inner.set_router_nat_mode(self.id, mode)?;
            let cfg = inner.router_effective_cfg(self.id)?;
            let (ns, _lan_if, wan_if, wan_ip) = inner.router_nat_params(self.id)?;
            (ns, wan_if, wan_ip, Arc::clone(&inner.netns), cfg)
        };
        run_nft_in(&netns, &ns, "flush table ip nat").ok();
        run_nft_in(&netns, &ns, "flush table ip filter").ok();
        apply_nat_for_router(&netns, &ns, &cfg, &wan_if, wan_ip)
    }


    /// Replaces IPv6 NAT rules on this router at runtime.
    pub fn set_nat_v6_mode(&self, mode: NatV6Mode) -> Result<()> {
        let (ns, wan_if, lan_prefix, wan_prefix, netns) = {
            let inner = self.lab.lock().unwrap();
            let router = inner
                .router(self.id)
                .ok_or_else(|| anyhow!("unknown router id"))?;
            let wan_if = router.wan_ifname(inner.ix_sw()).to_string();
            let lan_prefix = router
                .downstream_cidr_v6
                .unwrap_or_else(|| "fd10::/64".parse().unwrap());
            let wan_prefix = {
                let up_ip = router.upstream_ip_v6.unwrap_or(Ipv6Addr::UNSPECIFIED);
                let up_prefix = if router.uplink == Some(inner.ix_sw()) {
                    inner.cfg.ix_cidr_v6.prefix_len()
                } else {
                    router
                        .uplink
                        .and_then(|sw| inner.switch(sw))
                        .and_then(|sw| sw.cidr_v6)
                        .map(|c| c.prefix_len())
                        .unwrap_or(64)
                };
                Ipv6Net::new(up_ip, up_prefix).unwrap_or_else(|_| Ipv6Net::new(up_ip, 128).unwrap())
            };
            (
                router.ns.clone(),
                wan_if,
                lan_prefix,
                wan_prefix,
                Arc::clone(&inner.netns),
            )
        };
        run_nft_in(&netns, &ns, "flush table ip6 nat").ok();
        apply_nat_v6(&netns, &ns, mode, &wan_if, lan_prefix, wan_prefix)?;
        {
            let mut inner = self.lab.lock().unwrap();
            let router = inner
                .router_mut(self.id)
                .ok_or_else(|| anyhow!("unknown router id"))?;
            router.cfg.nat_v6 = mode;
        }
        Ok(())
    }

    /// Flushes the conntrack table, forcing all active NAT mappings to expire.
    ///
    /// Subsequent flows get new external port assignments. Requires `conntrack-tools`.
    pub fn flush_nat_state(&self) -> Result<()> {
        let (ns, netns) = {
            let inner = self.lab.lock().unwrap();
            let ns = inner.router_ns(self.id)?.to_string();
            (ns, Arc::clone(&inner.netns))
        };
        netns.run_closure_in(&ns, || {
            let st = std::process::Command::new("conntrack").arg("-F").status()?;
            if !st.success() {
                bail!("conntrack -F failed: {st}");
            }
            Ok(())
        })
    }


    // ── Spawn / run ────────────────────────────────────────────────────

    /// Spawns an async task in this router's network namespace.
    ///
    /// The closure receives a cloned [`Router`] handle.
    pub fn spawn<F, Fut, T>(&self, f: F) -> tokio::task::JoinHandle<T>
    where
        F: FnOnce(Router) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let inner = self.lab.lock().unwrap();
        let ns = &inner
            .router(self.id)
            .expect("router handle has valid id")
            .ns;
        let rt = inner.rt_handle_for(ns).expect("namespace has async worker");
        let handle = self.clone();
        rt.spawn(f(handle))
    }

    /// Runs a short-lived sync closure in this router's network namespace.
    /// Blocks the caller until the closure returns.
    pub fn run_sync<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let (ns, netns) = {
            let inner = self.lab.lock().unwrap();
            let router = inner
                .router(self.id)
                .ok_or_else(|| anyhow!("unknown router id"))?;
            (router.ns.clone(), Arc::clone(&inner.netns))
        };
        netns.run_closure_in(&ns, f)
    }

    /// Spawns a dedicated OS thread in this router's network namespace.
    pub fn spawn_thread<F, R>(&self, f: F) -> Result<thread::JoinHandle<Result<R>>>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let (ns, netns) = {
            let inner = self.lab.lock().unwrap();
            let router = inner
                .router(self.id)
                .ok_or_else(|| anyhow!("unknown router id"))?;
            (router.ns.clone(), Arc::clone(&inner.netns))
        };
        netns.spawn_thread_in(&ns, f)
    }

    /// Spawns a raw command in this router's network namespace.
    pub fn spawn_command(&self, mut cmd: Command) -> Result<std::process::Child> {
        let (ns, netns) = {
            let inner = self.lab.lock().unwrap();
            let router = inner
                .router(self.id)
                .ok_or_else(|| anyhow!("unknown router id"))?;
            (router.ns.clone(), Arc::clone(&inner.netns))
        };
        netns.run_closure_in(&ns, move || {
            cmd.spawn().context("spawn command in namespace")
        })
    }

    /// Spawns an async command in this router's network namespace.
    pub fn spawn_command_async(
        &self,
        mut cmd: tokio::process::Command,
    ) -> Result<tokio::process::Child> {
        let (ns, rt, netns) = {
            let inner = self.lab.lock().unwrap();
            let router = inner
                .router(self.id)
                .ok_or_else(|| anyhow!("unknown router id"))?;
            let rt = inner.rt_handle_for(&router.ns)?;
            (router.ns.clone(), rt, Arc::clone(&inner.netns))
        };
        netns.run_closure_in(&ns, move || {
            let _guard = rt.enter();
            cmd.spawn().context("spawn async command in namespace")
        })
    }

    /// Applies or removes impairment on this router's downlink bridge, affecting
    /// download-direction traffic to all downstream devices.
    pub fn set_downlink_condition(&self, impair: Option<LinkCondition>) -> Result<()> {
        debug!(router = ?self.id, impair = ?impair, "router: set_downlink_condition");
        let (ns, bridge, netns) = {
            let inner = self.lab.lock().unwrap();
            let r = inner.router(self.id).context("unknown router id")?;
            (
                r.ns.clone(),
                r.downlink_bridge.clone(),
                Arc::clone(&inner.netns),
            )
        };
        apply_or_remove_impair(&netns, &ns, &bridge, impair);
        Ok(())
    }


    /// Sets (or removes) the firewall on this router at runtime.
    ///
    /// Pass [`Firewall::None`] to remove all firewall rules.
    pub fn set_firewall(&self, fw: Firewall) -> Result<()> {
        let (ns, netns) = {
            let mut inner = self.lab.lock().unwrap();
            let r = inner.router_mut(self.id).context("unknown router id")?;
            r.cfg.firewall = fw.clone();
            (r.ns.clone(), Arc::clone(&inner.netns))
        };
        // Always remove existing rules first, then apply new ones.
        core::remove_firewall(&netns, &ns)?;
        core::apply_firewall(&netns, &ns, &fw)
    }

    /// Spawns a UDP reflector in this router's network namespace.
    pub fn spawn_reflector(&self, bind: SocketAddr) -> Result<()> {
        let inner = self.lab.lock().unwrap();
        let ns = &inner
            .router(self.id)
            .ok_or_else(|| anyhow!("unknown router id"))?
            .ns;
        inner.spawn_reflector_in(ns, bind)
    }
}

// ─────────────────────────────────────────────
// Ix handle
// ─────────────────────────────────────────────

/// Handle to the IX (Internet Exchange) — the lab root namespace that hosts
/// the shared bridge connecting all IX-level routers.
///
/// Same pattern as [`Device`] and [`Router`]: holds an `Arc` to the lab
/// interior. All accessor methods briefly lock the mutex.
pub struct Ix {
    lab: Arc<Mutex<NetworkCore>>,
}

impl Clone for Ix {
    fn clone(&self) -> Self {
        Self {
            lab: Arc::clone(&self.lab),
        }
    }
}

impl std::fmt::Debug for Ix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ix").finish()
    }
}

impl Ix {
    /// Returns the root namespace name.
    pub fn ns(&self) -> String {
        self.lab.lock().unwrap().root_ns().to_string()
    }

    /// Returns the IX gateway IPv4 address (e.g. 203.0.113.1).
    pub fn gw(&self) -> Ipv4Addr {
        self.lab.lock().unwrap().ix_gw()
    }

    /// Returns the IX gateway IPv6 address (e.g. 2001:db8::1).
    pub fn gw_v6(&self) -> Ipv6Addr {
        self.lab.lock().unwrap().cfg.ix_gw_v6
    }

    /// Spawns an async task in the IX root namespace.
    pub fn spawn<F, Fut, T>(&self, f: F) -> tokio::task::JoinHandle<T>
    where
        F: FnOnce(Ix) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let inner = self.lab.lock().unwrap();
        let ns = inner.root_ns();
        let rt = inner
            .rt_handle_for(ns)
            .expect("root namespace has async worker");
        let handle = self.clone();
        rt.spawn(f(handle))
    }

    /// Runs a short-lived sync closure in the IX root namespace.
    /// Blocks the caller until the closure returns.
    pub fn run_sync<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let (ns, netns) = {
            let inner = self.lab.lock().unwrap();
            (inner.root_ns().to_string(), Arc::clone(&inner.netns))
        };
        netns.run_closure_in(&ns, f)
    }

    /// Spawns a dedicated OS thread in the IX root namespace.
    pub fn spawn_thread<F, R>(&self, f: F) -> Result<thread::JoinHandle<Result<R>>>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let (ns, netns) = {
            let inner = self.lab.lock().unwrap();
            (inner.root_ns().to_string(), Arc::clone(&inner.netns))
        };
        netns.spawn_thread_in(&ns, f)
    }

    /// Spawns a raw command in the IX root namespace.
    pub fn spawn_command(&self, mut cmd: Command) -> Result<std::process::Child> {
        let (ns, netns) = {
            let inner = self.lab.lock().unwrap();
            (inner.root_ns().to_string(), Arc::clone(&inner.netns))
        };
        netns.run_closure_in(&ns, move || {
            cmd.spawn().context("spawn command in namespace")
        })
    }

    /// Spawns an async command in the IX root namespace.
    pub fn spawn_command_async(
        &self,
        mut cmd: tokio::process::Command,
    ) -> Result<tokio::process::Child> {
        let (ns, rt, netns) = {
            let inner = self.lab.lock().unwrap();
            let ns = inner.root_ns().to_string();
            let rt = inner.rt_handle_for(&ns)?;
            (ns, rt, Arc::clone(&inner.netns))
        };
        netns.run_closure_in(&ns, move || {
            let _guard = rt.enter();
            cmd.spawn().context("spawn async command in namespace")
        })
    }

    /// Spawns a UDP reflector in the IX root namespace.
    pub fn spawn_reflector(&self, bind: SocketAddr) -> Result<()> {
        let inner = self.lab.lock().unwrap();
        let ns = inner.root_ns();
        inner.spawn_reflector_in(ns, bind)
    }
}

// ─────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────


/// Normalise a device/interface name for use in an environment variable name.
fn normalize_env_name(s: &str) -> String {
    s.to_uppercase().replace('-', "_")
}
