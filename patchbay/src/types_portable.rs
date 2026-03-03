//! Portable type definitions for non-Linux platforms.
//!
//! These are copies of the type definitions from nat.rs, firewall.rs, qdisc.rs,
//! and lab.rs that are needed for serialization/deserialization on all platforms.
//! The actual implementations only work on Linux.

use serde::{Deserialize, Serialize};

// ─────────────────────────────────────────────────────────────────────────────
// From nat.rs
// ─────────────────────────────────────────────────────────────────────────────

/// NAT behavior preset for common real-world equipment.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumIter,
    strum::Display,
)]
#[serde(rename_all = "kebab-case")]
pub enum Nat {
    /// No NAT - addresses are publicly routable.
    #[default]
    None,
    /// Home router - EIM + APDF, port-preserving.
    Home,
    /// Corporate firewall - symmetric NAT (EDM + APDF).
    Corporate,
    /// Carrier-grade NAT per RFC 6888.
    Cgnat,
    /// Cloud NAT gateway - symmetric NAT with longer timeouts.
    CloudNat,
    /// Full cone - most permissive NAT.
    FullCone,
    /// Custom NAT configuration.
    #[strum(disabled)]
    Custom(NatConfig),
}

impl From<NatConfig> for Nat {
    fn from(config: NatConfig) -> Self {
        Nat::Custom(config)
    }
}

/// NAT mapping behavior per RFC 4787 Section 4.1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NatMapping {
    /// Same external port for all destinations (EIM).
    EndpointIndependent,
    /// Different external port per destination (symmetric/EDM).
    EndpointDependent,
}

/// NAT filtering behavior per RFC 4787 Section 5.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NatFiltering {
    /// Any external host can send to the mapped port.
    EndpointIndependent,
    /// Only the exact (IP, port) contacted can reply.
    AddressAndPortDependent,
}

/// Conntrack timeout configuration for a NAT profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConntrackTimeouts {
    /// Timeout for a single unreplied UDP packet (seconds).
    pub udp: u32,
    /// Timeout for a UDP "stream" (seconds).
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

/// Expanded NAT configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Sets the UDP single-packet timeout (seconds).
    pub fn udp_timeout(mut self, secs: u32) -> Self {
        self.timeouts.udp = secs;
        self
    }

    /// Sets the UDP stream timeout (seconds).
    pub fn udp_stream_timeout(mut self, secs: u32) -> Self {
        self.timeouts.udp_stream = secs;
        self
    }

    /// Sets the TCP established timeout (seconds).
    pub fn tcp_established_timeout(mut self, secs: u32) -> Self {
        self.timeouts.tcp_established = secs;
        self
    }

    /// Enables or disables NAT hairpinning.
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NatV6Mode {
    /// No translation; devices use global unicast directly.
    #[default]
    None,
    /// RFC 6296 stateless prefix translation.
    Nptv6,
    /// Stateful masquerade.
    Masquerade,
    /// NAT64 via well-known prefix `64:ff9b::/96`.
    Nat64,
}

/// Selects which IP address families a router supports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IpSupport {
    /// IPv4 only (default).
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

// ─────────────────────────────────────────────────────────────────────────────
// From firewall.rs
// ─────────────────────────────────────────────────────────────────────────────

/// Firewall preset for a router's forward chain.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Firewall {
    /// No filtering beyond NAT (default).
    #[default]
    None,
    /// Block unsolicited inbound traffic (RFC 6092).
    BlockInbound,
    /// Corporate firewall (TCP 80,443 + UDP 53 only).
    Corporate,
    /// Hotel/airport captive-portal style.
    CaptivePortal,
    /// Fully custom firewall configuration.
    Custom(FirewallConfig),
}

/// Outbound port policy for a protocol.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortPolicy {
    /// All destination ports are allowed.
    #[default]
    AllowAll,
    /// Only the listed destination ports are allowed.
    Allow(Vec<u16>),
    /// All destination ports are blocked.
    BlockAll,
}

/// Firewall configuration controlling inbound and outbound traffic.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FirewallConfig {
    /// Block unsolicited inbound traffic on the WAN interface.
    pub block_inbound: bool,
    /// Outbound TCP port policy.
    pub outbound_tcp: PortPolicy,
    /// Outbound UDP port policy.
    pub outbound_udp: PortPolicy,
}

impl FirewallConfig {
    /// Returns a builder for constructing a firewall configuration.
    pub fn builder() -> FirewallConfigBuilder {
        FirewallConfigBuilder::default()
    }
}

/// Builder for [`FirewallConfig`].
#[derive(Clone, Debug, Default)]
pub struct FirewallConfigBuilder {
    block_inbound: bool,
    outbound_tcp: PortPolicy,
    outbound_udp: PortPolicy,
}

impl FirewallConfigBuilder {
    /// Block unsolicited inbound traffic.
    pub fn block_inbound(mut self) -> Self {
        self.block_inbound = true;
        self
    }

    /// Sets the outbound TCP port policy.
    pub fn outbound_tcp(mut self, policy: PortPolicy) -> Self {
        self.outbound_tcp = policy;
        self
    }

    /// Sets the outbound UDP port policy.
    pub fn outbound_udp(mut self, policy: PortPolicy) -> Self {
        self.outbound_udp = policy;
        self
    }

    /// Builds the [`FirewallConfig`].
    pub fn build(self) -> FirewallConfig {
        FirewallConfig {
            block_inbound: self.block_inbound,
            outbound_tcp: self.outbound_tcp,
            outbound_udp: self.outbound_udp,
        }
    }
}

impl Firewall {
    /// Expands a preset to a [`FirewallConfig`].
    pub fn to_config(&self) -> Option<FirewallConfig> {
        match self {
            Firewall::None => None,
            Firewall::BlockInbound => Some(FirewallConfig {
                block_inbound: true,
                outbound_tcp: PortPolicy::AllowAll,
                outbound_udp: PortPolicy::AllowAll,
            }),
            Firewall::Corporate => Some(FirewallConfig {
                block_inbound: true,
                outbound_tcp: PortPolicy::Allow(vec![80, 443]),
                outbound_udp: PortPolicy::Allow(vec![53]),
            }),
            Firewall::CaptivePortal => Some(FirewallConfig {
                block_inbound: true,
                outbound_tcp: PortPolicy::AllowAll,
                outbound_udp: PortPolicy::Allow(vec![53]),
            }),
            Firewall::Custom(cfg) => Some(cfg.clone()),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// From qdisc.rs
// ─────────────────────────────────────────────────────────────────────────────

/// Parameters for `tc netem` impairment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkLimits {
    /// Rate limit in kbit/s (0 = unlimited).
    pub rate_kbit: u32,
    /// Packet loss percentage (0.0–100.0).
    pub loss_pct: f32,
    /// One-way latency in milliseconds.
    pub latency_ms: u32,
    /// Jitter in milliseconds.
    pub jitter_ms: u32,
    /// Packet reordering percentage (0.0–100.0).
    pub reorder_pct: f32,
    /// Packet duplication percentage (0.0–100.0).
    pub duplicate_pct: f32,
    /// Bit-error corruption percentage (0.0–100.0).
    pub corrupt_pct: f32,
}

// ─────────────────────────────────────────────────────────────────────────────
// From lab.rs
// ─────────────────────────────────────────────────────────────────────────────

/// Link-layer impairment profile applied via `tc netem`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkCondition {
    /// Wired LAN (1G Ethernet). No impairment.
    Lan,
    /// Good WiFi — 5 GHz band, close to AP.
    Wifi,
    /// Congested WiFi — 2.4 GHz, interference.
    WifiBad,
    /// 4G/LTE good signal.
    Mobile4G,
    /// 3G or degraded 4G.
    Mobile3G,
    /// LEO satellite (Starlink-class).
    Satellite,
    /// GEO satellite (HughesNet/Viasat).
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
    /// Converts this preset into concrete [`LinkLimits`].
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

/// Router preset combining NAT, firewall, IP support, and address pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RouterPreset {
    /// Home router (FritzBox, UniFi, TP-Link, etc.).
    Home,
    /// ISP transit / datacenter / cloud VM / public server.
    Datacenter,
    /// ISP transit — IPv4 only.
    IspV4,
    /// Mobile carrier (T-Mobile, Jio, Vodafone, O2).
    Mobile,
    /// Mobile carrier — IPv6-only with NAT64.
    MobileV6,
    /// Corporate firewall.
    Corporate,
    /// Hotel/airport guest WiFi.
    Hotel,
    /// Cloud provider NAT gateway.
    Cloud,
}
