//! NAT behavior presets and configuration types.

use serde::Deserialize;

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
    /// # use patchbay::*;
    /// let custom = Nat::Custom(
    ///     NatConfig::builder()
    ///         .mapping(NatMapping::EndpointIndependent)
    ///         .filtering(NatFiltering::EndpointIndependent)
    ///         .udp_stream_timeout(120)
    ///         .build(),
    /// );
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
/// # use patchbay::{NatConfig, NatMapping, NatFiltering};
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
    /// Returns `None` for [`Nat::None`] and [`Nat::Cgnat`], which use
    /// different code paths (no NAT and ISP-level masquerade respectively).
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
    /// No translation; devices use global unicast directly.
    #[default]
    None,
    /// RFC 6296 stateless prefix translation (1:1 prefix mapping).
    Nptv6,
    /// Stateful masquerade (useful for testing symmetric behavior on IPv6).
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
