//! NAT behavior presets and configuration types.

use serde::{Deserialize, Serialize};

/// NAT behavior preset for common deployment shapes.
///
/// The variants form a gradient of hole-punching difficulty. `Easiest`
/// and `Easy` are hole-punchable with standard UDP hole-punching.
/// `Hard` is hole-punchable with port prediction. `Hardest` practically
/// requires a relay like TURN.
///
/// Abbreviations in the doc comments:
/// - EIM: Endpoint-Independent Mapping (RFC 4787 §4.1).
/// - EDM: Endpoint-Dependent Mapping (RFC 4787 §4.1, "symmetric").
/// - EIF: Endpoint-Independent Filtering (RFC 4787 §5).
/// - ADF: Address-Dependent Filtering (RFC 4787 §5).
/// - APDF: Address-and-Port-Dependent Filtering (RFC 4787 §5).
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
    /// No NAT. Addresses are publicly routable.
    ///
    /// Use for datacenter racks, cloud VMs with elastic IPs, or any host
    /// that needs a stable public address.
    #[default]
    None,

    /// Full cone NAT. Once an internal endpoint has sent one outbound
    /// packet, any external host can reach the mapped external port.
    ///
    /// RFC 4787: EIM plus EIF. Port-preserving. RFC 3489 calls this
    /// "Full Cone". Deployed on older consumer routers, RFC 6888
    /// compliant fiber ISP CGNAT, and routers running the
    /// `netfilter-full-cone-nat` kernel module. Hole-punching is not
    /// needed because peers can reach the mapped port directly.
    Easiest,

    /// Port-restricted cone NAT. The external port stays the same for
    /// all destinations, but only peers the internal endpoint has
    /// contacted can reply.
    ///
    /// RFC 4787: EIM plus APDF. Port-preserving. RFC 3489 calls this
    /// "Port-Restricted Cone". This is the default behavior of almost
    /// every home router: FritzBox, Unifi, ASUS, TP-Link, OpenWRT, and
    /// Linux masquerade without the `random` flag. Standard UDP
    /// hole-punching succeeds here.
    Easy,

    /// Symmetric NAT with port preservation.
    ///
    /// RFC 4787: EDM plus APDF, with port preservation. Each
    /// destination sees a different mapping, but the external port
    /// still matches the internal source port when the port is free.
    /// RFC 3489 has no term for this because it lumps all EDM as
    /// "Symmetric". The peer-to-peer literature calls this SYMPP.
    ///
    /// Deployed on some stateful firewalls, CGNAT that uses Port Block
    /// Allocation (RFC 7753), and many mobile carriers. Hole-punching
    /// succeeds when the port-prediction technique applies.
    Hard,

    /// Symmetric NAT with random ports.
    ///
    /// RFC 4787: EDM plus APDF, with random port allocation. Each
    /// destination sees a fresh, unpredictable external port. RFC 3489
    /// calls this "Symmetric".
    ///
    /// Deployed on enterprise firewalls (Cisco ASA, Palo Alto, Fortinet,
    /// Juniper), AWS, Azure, and GCP NAT gateways, and hardened mobile
    /// CGNAT. Hole-punching practically fails: peers need a relay.
    Hardest,

    /// Fully custom NAT configuration.
    ///
    /// Use this when the named presets do not cover the scenario.
    ///
    /// # Example
    /// ```no_run
    /// # use patchbay::*;
    /// let custom = Nat::Custom(
    ///     NatConfig::builder()
    ///         .mapping(NatMapping::EndpointIndependent)
    ///         .filtering(NatFiltering::EndpointIndependent)
    ///         .hairpin(true)
    ///         .build(),
    /// );
    /// ```
    #[strum(disabled)]
    Custom(NatConfig),
}

impl From<NatConfig> for Nat {
    fn from(config: NatConfig) -> Self {
        Nat::Custom(config)
    }
}

impl From<Nat> for Option<NatConfig> {
    fn from(nat: Nat) -> Self {
        match nat {
            Nat::None => None,
            Nat::Easiest => Some(NatConfig {
                mapping: NatMapping::EndpointIndependent,
                filtering: NatFiltering::EndpointIndependent,
                port_preservation: PortPreservation::Preserve,
                timeouts: ConntrackTimeouts::default(),
                hairpin: false,
            }),
            Nat::Easy => Some(NatConfig {
                mapping: NatMapping::EndpointIndependent,
                filtering: NatFiltering::AddressAndPortDependent,
                port_preservation: PortPreservation::Preserve,
                timeouts: ConntrackTimeouts::default(),
                hairpin: false,
            }),
            Nat::Hard => Some(NatConfig {
                mapping: NatMapping::EndpointDependent,
                filtering: NatFiltering::AddressAndPortDependent,
                port_preservation: PortPreservation::Preserve,
                timeouts: ConntrackTimeouts::default(),
                hairpin: false,
            }),
            Nat::Hardest => Some(NatConfig {
                mapping: NatMapping::EndpointDependent,
                filtering: NatFiltering::AddressAndPortDependent,
                port_preservation: PortPreservation::Random,
                timeouts: ConntrackTimeouts::default(),
                hairpin: false,
            }),
            Nat::Custom(config) => Some(config),
        }
    }
}

/// NAT mapping behavior per RFC 4787 §4.1.
///
/// Controls how the NAT assigns external ports when translating outbound
/// packets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NatMapping {
    /// Same external port for all destinations (EIM).
    ///
    /// The NAT reuses one external port for every destination from a given
    /// internal source. nftables: `snat to <ip>` with a fullcone map.
    EndpointIndependent,
    /// Different external port per destination IP and port (EDM, "symmetric").
    ///
    /// The NAT assigns a fresh mapping per 4-tuple. Whether the new port
    /// is predictable depends on [`PortPreservation`].
    EndpointDependent,
}

/// NAT filtering behavior per RFC 4787 §5.
///
/// Controls which inbound packets the NAT allows through to the internal
/// host after an outbound mapping has been established.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NatFiltering {
    /// Any external host can send to the mapped port (EIF, "full cone").
    ///
    /// nftables: fullcone DNAT map in prerouting.
    EndpointIndependent,
    /// Only external hosts the internal endpoint has contacted can reply,
    /// but any port on those hosts is allowed (ADF, "restricted cone").
    ///
    /// RFC 3489 calls this "Restricted Cone". Less common than APDF but
    /// documented on some consumer routers.
    AddressDependent,
    /// Only the exact (IP, port) the internal endpoint contacted can reply
    /// (APDF, "port-restricted cone").
    ///
    /// nftables: conntrack-only (no prerouting DNAT).
    AddressAndPortDependent,
}

/// Port allocation behavior for the NAT.
///
/// Orthogonal to mapping and filtering per RFC 4787 §4.2 (port preservation).
//
// Gap: sequential port allocation (deployed in some older SOHO boxes and in
// Port-Block-Allocated CGNAT per RFC 7753) is not modeled. Hole-punching
// literature treats it as a distinct class from `Preserve` because ports
// are predictable across flows in a deterministic way.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortPreservation {
    /// Reuse the internal source port for the external port when the port
    /// is free. Fall back to a fresh port on collision.
    ///
    /// This is the Linux conntrack default for `masquerade` without flags.
    /// Combined with `EndpointDependent` mapping this produces a symmetric
    /// NAT that port-prediction techniques can still traverse.
    #[default]
    Preserve,
    /// Allocate a random external port.
    ///
    /// Combined with `EndpointDependent` mapping this makes hole-punching
    /// practically impossible.
    Random,
}

/// Conntrack timeout configuration for a NAT profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

/// Expanded NAT configuration produced from a [`Nat`] preset or the builder.
///
/// Carries all parameters needed to generate nftables rules and conntrack
/// settings for a router's NAT. Each preset ([`Nat::Easy`], [`Nat::Hard`],
/// etc.) expands to a specific `NatConfig` via [`From<Nat>`] for
/// `Option<NatConfig>`.
///
/// # Example
/// ```
/// # use patchbay::{NatConfig, NatMapping, NatFiltering};
/// let cfg = NatConfig::builder()
///     .mapping(NatMapping::EndpointIndependent)
///     .filtering(NatFiltering::AddressAndPortDependent)
///     .udp_stream_timeout(120)
///     .build();
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NatConfig {
    /// How outbound port mapping works.
    pub mapping: NatMapping,
    /// Which inbound packets are forwarded.
    pub filtering: NatFiltering,
    /// How external ports are allocated.
    #[serde(default)]
    pub port_preservation: PortPreservation,
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
/// Defaults to EIM + APDF with port preservation and standard home-router
/// timeouts.
#[derive(Clone, Debug)]
pub struct NatConfigBuilder {
    mapping: NatMapping,
    filtering: NatFiltering,
    port_preservation: PortPreservation,
    timeouts: ConntrackTimeouts,
    hairpin: bool,
}

impl Default for NatConfigBuilder {
    fn default() -> Self {
        Self {
            mapping: NatMapping::EndpointIndependent,
            filtering: NatFiltering::AddressAndPortDependent,
            port_preservation: PortPreservation::Preserve,
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

    /// Sets the port allocation behavior.
    pub fn port_preservation(mut self, pp: PortPreservation) -> Self {
        self.port_preservation = pp;
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
            port_preservation: self.port_preservation,
            timeouts: self.timeouts,
            hairpin: self.hairpin,
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
    /// RFC 6296 stateless prefix translation (1:1 prefix mapping).
    Nptv6,
    /// Stateful masquerade (useful for testing symmetric behavior on IPv6).
    Masquerade,
    /// NAT64: IPv6-only devices reach IPv4 hosts via the well-known prefix
    /// `64:ff9b::/96`. A userspace SIIT translator on the router converts
    /// between IPv6 and IPv4 headers; nftables masquerade handles port
    /// mapping on the v4 side.
    ///
    /// This is the dominant IPv6 deployment model for mobile carriers
    /// (T-Mobile US/DE, NTT Docomo, etc.). Pair with `IpSupport::DualStack`
    /// (the router still needs a v4 uplink for the translated traffic).
    Nat64,
}

/// Selects which IP address families a router supports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
