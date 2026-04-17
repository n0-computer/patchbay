//! NAT behavior presets and configuration types.

use serde::{Deserialize, Serialize};

/// NAT behavior for IPv4.
///
/// The variants form a gradient of hole-punching difficulty. `Easiest` and
/// `Easy` are reachable with standard UDP hole-punching. `Hard` is reachable
/// with port prediction. `Hardest` requires a relay like TURN.
///
/// Each preset expands to a [`NatConfig`] via [`Nat::to_config`]. Timeouts
/// and hairpin behavior come from [`ConntrackTimeouts::default`] and
/// `hairpin: false`; for deployment-specific timeouts use
/// [`RouterPreset`](crate::RouterPreset) instead.
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
    /// No NAT. The device has a publicly routable address.
    #[default]
    None,

    /// Most permissive NAT.
    ///
    /// Any external host can reach the mapped port after the first outbound
    /// packet. Typical of consumer routers with UPnP or static port
    /// forwarding, older consumer NAT boxes, and the output of
    /// `netfilter-full-cone-nat` style kernel modules.
    ///
    /// RFC 3489: Full Cone.
    /// RFC 4787: Endpoint-Independent Mapping, Endpoint-Independent
    /// Filtering.
    Easiest,

    /// Moderately restrictive NAT.
    ///
    /// The external mapping is stable across destinations, but inbound
    /// packets are accepted only from the exact addresses and ports the
    /// internal side has contacted. Standard UDP hole-punching succeeds.
    /// Default behavior of every major home router (FritzBox, Unifi, ASUS,
    /// TP-Link, OpenWRT, Linux masquerade) and of most RFC 6888 compliant
    /// CGNAT.
    ///
    /// RFC 3489: Port Restricted Cone.
    /// RFC 4787: Endpoint-Independent Mapping, Address-and-Port-Dependent
    /// Filtering.
    Easy,

    /// Restrictive NAT with a port-predictable mapping.
    ///
    /// Each destination gets its own external mapping, but the external port
    /// still matches the internal source port when the port is free.
    /// Hole-punching succeeds with port prediction. Typical of some stateful
    /// firewalls, some CGNAT variants, and most mobile carriers.
    ///
    /// RFC 4787: Endpoint-Dependent Mapping with port preservation,
    /// Address-and-Port-Dependent Filtering.
    Hard,

    /// Most restrictive NAT.
    ///
    /// Each destination gets a fresh, random external port. Hole-punching
    /// fails; peers need a relay. Typical of enterprise firewalls
    /// (Cisco ASA, Palo Alto, Fortinet) and cloud NAT gateways (AWS, Azure,
    /// GCP).
    ///
    /// RFC 3489: Symmetric.
    /// RFC 4787: Endpoint-Dependent Mapping with random ports,
    /// Address-and-Port-Dependent Filtering.
    Hardest,

    /// Fully custom NAT configuration.
    ///
    /// Use this when the named presets do not cover the scenario. Construct
    /// the config with [`NatConfig::builder`].
    #[strum(disabled)]
    Custom(NatConfig),
}

impl Nat {
    /// Expands this preset into a [`NatConfig`], or `None` for [`Nat::None`].
    pub fn to_config(self) -> Option<NatConfig> {
        self.into()
    }
}

impl From<NatConfig> for Nat {
    fn from(config: NatConfig) -> Self {
        Nat::Custom(config)
    }
}

impl From<Nat> for Option<NatConfig> {
    fn from(nat: Nat) -> Self {
        let (mapping, filtering) = match nat {
            Nat::None => return None,
            Nat::Easiest => (
                NatMapping::EndpointIndependent,
                NatFiltering::EndpointIndependent,
            ),
            Nat::Easy => (
                NatMapping::EndpointIndependent,
                NatFiltering::AddressAndPortDependent,
            ),
            Nat::Hard => (
                NatMapping::EndpointDependent(PortPreservation::Preserve),
                NatFiltering::AddressAndPortDependent,
            ),
            Nat::Hardest => (
                NatMapping::EndpointDependent(PortPreservation::Random),
                NatFiltering::AddressAndPortDependent,
            ),
            Nat::Custom(config) => return Some(config),
        };
        Some(NatConfig {
            mapping,
            filtering,
            timeouts: ConntrackTimeouts::default(),
            hairpin: false,
        })
    }
}

/// NAT mapping behavior per RFC 4787 §4.1.
///
/// Controls how the NAT assigns external ports when translating outbound
/// packets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NatMapping {
    /// Same external port for all destinations.
    ///
    /// RFC 4787: Endpoint-Independent Mapping (EIM). The NAT reuses one
    /// external port for every destination from a given internal source
    /// address and port. Port preservation is implicit and always enabled.
    EndpointIndependent,

    /// Different external port per destination.
    ///
    /// RFC 4787: Endpoint-Dependent Mapping (EDM), also called "symmetric".
    /// Each unique destination 4-tuple gets a fresh external port. The
    /// [`PortPreservation`] value selects how the fresh port is chosen.
    EndpointDependent(PortPreservation),
}

/// Port allocation policy for [`NatMapping::EndpointDependent`].
///
/// Only applies to EDM: [`NatMapping::EndpointIndependent`] always
/// preserves the source port, and the variant has no configuration.
//
// Gap: sequential port allocation (deployed in some older SOHO boxes and in
// Port-Block-Allocated CGNAT) is not modeled. Hole-punching literature
// treats it as a distinct class from `Preserve` because ports are
// predictable across flows in a deterministic way. If a future test needs
// it, implement as a new variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortPreservation {
    /// Reuse the internal source port when the external port is free.
    ///
    /// Linux `masquerade` without flags. Combined with EDM this produces a
    /// symmetric NAT that port-prediction techniques can traverse.
    #[default]
    Preserve,
    /// Allocate a random external port for each flow.
    ///
    /// Linux `masquerade random`. Combined with EDM this makes hole-punching
    /// practically impossible.
    Random,
}

/// NAT filtering behavior per RFC 4787 §5.
///
/// Controls which inbound packets the NAT allows through to the internal
/// host after an outbound mapping has been established. Not every filtering
/// mode is compatible with every mapping mode: [`NatMapping::EndpointDependent`]
/// paired with `AddressDependent` is rejected at build time because the
/// implementation has no way to deliver inbound packets from unsolicited
/// ports without a fullcone map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NatFiltering {
    /// Any external host can send to the mapped port.
    ///
    /// RFC 4787: Endpoint-Independent Filtering (EIF), also called
    /// "full cone". nftables implementation: fullcone DNAT map in
    /// prerouting.
    EndpointIndependent,
    /// Only external hosts the internal endpoint has contacted can reply.
    /// Any source port on those hosts is allowed.
    ///
    /// RFC 3489: Restricted Cone. RFC 4787: Address-Dependent Filtering
    /// (ADF). Less common than APDF but documented on some consumer
    /// routers. Only supported with [`NatMapping::EndpointIndependent`].
    AddressDependent,
    /// Only the exact address and port the internal endpoint contacted can
    /// reply.
    ///
    /// RFC 3489: Port-Restricted Cone. RFC 4787: Address-and-Port-Dependent
    /// Filtering (APDF). nftables implementation: conntrack match only,
    /// no prerouting DNAT.
    AddressAndPortDependent,
}

/// Conntrack timeout configuration for a NAT profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
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

/// Errors returned by [`NatConfigBuilder::build`] when the requested
/// combination cannot be expressed in nftables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NatConfigError {
    /// Address-Dependent filtering requires Endpoint-Independent mapping.
    ///
    /// The nftables rules for ADF rely on the fullcone DNAT map to deliver
    /// inbound packets from any port on a contacted address. That map only
    /// exists when mapping is [`NatMapping::EndpointIndependent`].
    AdfRequiresEim,
    /// Hairpin requires Endpoint-Independent mapping.
    ///
    /// The hairpin DNAT rule rewrites traffic destined for the router's
    /// WAN IP back into the LAN using the fullcone map. Endpoint-Dependent
    /// mapping has no such map, and the current implementation would route
    /// hairpin traffic to the router itself, not to the LAN peer.
    HairpinRequiresEim,
}

impl std::fmt::Display for NatConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AdfRequiresEim => f.write_str(
                "NatFiltering::AddressDependent requires \
                 NatMapping::EndpointIndependent; Endpoint-Dependent mapping \
                 has no way to deliver inbound packets from unsolicited ports",
            ),
            Self::HairpinRequiresEim => f.write_str(
                "hairpin requires NatMapping::EndpointIndependent; \
                 Endpoint-Dependent mapping has no fullcone map to DNAT \
                 hairpin traffic",
            ),
        }
    }
}

impl std::error::Error for NatConfigError {}

/// Expanded NAT configuration produced from a [`Nat`] preset or the builder.
///
/// Each preset ([`Nat::Easy`], [`Nat::Hard`], etc.) expands via
/// [`Nat::to_config`]. Custom configurations are built with
/// [`NatConfig::builder`], which validates cross-field invariants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
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
/// Defaults to EIM + APDF with standard home-router timeouts. Call
/// [`NatConfigBuilder::build`] to validate cross-field invariants and
/// produce the final [`NatConfig`].
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
    /// public IP. Only supported with [`NatMapping::EndpointIndependent`].
    pub fn hairpin(mut self, enabled: bool) -> Self {
        self.hairpin = enabled;
        self
    }

    /// Validates the configuration and returns the [`NatConfig`].
    ///
    /// Returns an error when the combination of mapping, filtering, and
    /// hairpin cannot be expressed in nftables. See [`NatConfigError`] for
    /// the specific rules.
    pub fn build(self) -> Result<NatConfig, NatConfigError> {
        let eim = matches!(self.mapping, NatMapping::EndpointIndependent);
        if !eim && matches!(self.filtering, NatFiltering::AddressDependent) {
            return Err(NatConfigError::AdfRequiresEim);
        }
        if !eim && self.hairpin {
            return Err(NatConfigError::HairpinRequiresEim);
        }
        Ok(NatConfig {
            mapping: self.mapping,
            filtering: self.filtering,
            timeouts: self.timeouts,
            hairpin: self.hairpin,
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_rejects_edm_with_adf() {
        let err = NatConfig::builder()
            .mapping(NatMapping::EndpointDependent(PortPreservation::Preserve))
            .filtering(NatFiltering::AddressDependent)
            .build()
            .unwrap_err();
        assert_eq!(err, NatConfigError::AdfRequiresEim);
    }

    #[test]
    fn builder_rejects_edm_with_hairpin() {
        let err = NatConfig::builder()
            .mapping(NatMapping::EndpointDependent(PortPreservation::Random))
            .hairpin(true)
            .build()
            .unwrap_err();
        assert_eq!(err, NatConfigError::HairpinRequiresEim);
    }

    #[test]
    fn builder_accepts_eim_with_adf() {
        let cfg = NatConfig::builder()
            .mapping(NatMapping::EndpointIndependent)
            .filtering(NatFiltering::AddressDependent)
            .build()
            .unwrap();
        assert_eq!(cfg.filtering, NatFiltering::AddressDependent);
    }

    #[test]
    fn builder_accepts_eim_with_hairpin() {
        let cfg = NatConfig::builder()
            .mapping(NatMapping::EndpointIndependent)
            .hairpin(true)
            .build()
            .unwrap();
        assert!(cfg.hairpin);
    }

    #[test]
    fn builder_accepts_edm_preserve() {
        let cfg = NatConfig::builder()
            .mapping(NatMapping::EndpointDependent(PortPreservation::Preserve))
            .filtering(NatFiltering::AddressAndPortDependent)
            .build()
            .unwrap();
        assert!(matches!(
            cfg.mapping,
            NatMapping::EndpointDependent(PortPreservation::Preserve)
        ));
    }

    #[test]
    fn builder_accepts_edm_random() {
        let cfg = NatConfig::builder()
            .mapping(NatMapping::EndpointDependent(PortPreservation::Random))
            .filtering(NatFiltering::AddressAndPortDependent)
            .build()
            .unwrap();
        assert!(matches!(
            cfg.mapping,
            NatMapping::EndpointDependent(PortPreservation::Random)
        ));
    }

    #[test]
    fn nat_to_config_covers_every_preset() {
        for nat in [Nat::None, Nat::Easiest, Nat::Easy, Nat::Hard, Nat::Hardest] {
            let cfg = nat.to_config();
            assert_eq!(cfg.is_none(), nat == Nat::None);
        }
    }

    #[test]
    fn nat_easiest_is_eim_eif() {
        let cfg = Nat::Easiest.to_config().unwrap();
        assert_eq!(cfg.mapping, NatMapping::EndpointIndependent);
        assert_eq!(cfg.filtering, NatFiltering::EndpointIndependent);
    }

    #[test]
    fn nat_easy_is_eim_apdf() {
        let cfg = Nat::Easy.to_config().unwrap();
        assert_eq!(cfg.mapping, NatMapping::EndpointIndependent);
        assert_eq!(cfg.filtering, NatFiltering::AddressAndPortDependent);
    }

    #[test]
    fn nat_hard_is_edm_preserve_apdf() {
        let cfg = Nat::Hard.to_config().unwrap();
        assert_eq!(
            cfg.mapping,
            NatMapping::EndpointDependent(PortPreservation::Preserve)
        );
        assert_eq!(cfg.filtering, NatFiltering::AddressAndPortDependent);
    }

    #[test]
    fn nat_hardest_is_edm_random_apdf() {
        let cfg = Nat::Hardest.to_config().unwrap();
        assert_eq!(
            cfg.mapping,
            NatMapping::EndpointDependent(PortPreservation::Random)
        );
        assert_eq!(cfg.filtering, NatFiltering::AddressAndPortDependent);
    }
}
