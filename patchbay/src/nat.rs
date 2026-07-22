//! NAT behavior presets and configuration types.

use serde::{Deserialize, Serialize};

/// NAT behavior for IPv4.
///
/// The variants form a gradient of hole-punching difficulty. `Easiest` and
/// `Easy` are reachable with standard UDP hole-punching. `Hard` requires a
/// relay like TURN.
///
/// Each preset expands to a [`NatConfig`] via [`Nat::to_config`]. Timeouts
/// and hairpin behavior come from [`ConntrackTimeouts::default`] and
/// `hairpin: false`; for deployment-specific timeouts use
/// [`RouterPreset`](crate::RouterPreset) instead.
///
/// # Port-preserving symmetric NAT
///
/// Real-world "port-preserving symmetric" (SYMPP) hardware exists and is
/// hole-punchable with port prediction, forming a middle tier between
/// `Easy` and `Hard`. patchbay does not model it distinctly: Linux
/// `nftables` cannot produce true per-destination port allocation that
/// preserves the source port, and simulating SYMPP through plain
/// `masquerade` converges on EIM-like behavior under light load. See
/// [`docs/reference/nat-limitations.md`](https://github.com/n0-computer/patchbay/blob/main/docs/reference/nat-limitations.md)
/// for the full rationale and how a future backend could add it.
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
    /// forwarding, older consumer NAT boxes, and routers running
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

    /// Most restrictive NAT.
    ///
    /// Each destination gets a fresh, random external port. Hole-punching
    /// fails without a relay. Typical of enterprise firewalls (Cisco ASA,
    /// Palo Alto, Fortinet), cloud NAT gateways (AWS, Azure, GCP), mobile
    /// carriers, and most symmetric CGNAT.
    ///
    /// RFC 3489: Symmetric.
    /// RFC 4787: Endpoint-Dependent Mapping with random ports,
    /// Address-and-Port-Dependent Filtering.
    Hard,

    /// Fully custom NAT configuration.
    ///
    /// [`RouterBuilder::nat`](crate::RouterBuilder::nat) and
    /// [`Router::set_nat`](crate::Router::set_nat) both accept a
    /// [`NatConfig`] directly, so `Nat::Custom` is rarely needed in
    /// application code. Reach for it when you need a `Nat` value: for
    /// TOML deserialization or when pattern-matching on a stored
    /// preset.
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
                NatMapping::EndpointDependent,
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
    /// address and port. Port preservation is implicit and always enabled:
    /// the nftables backend records the pre-SNAT source tuple in a
    /// `@fullcone` map so inbound packets reach the correct internal host.
    EndpointIndependent,

    /// Different external port per destination.
    ///
    /// RFC 4787: Endpoint-Dependent Mapping (EDM), also called "symmetric".
    /// Each unique destination 4-tuple gets a fresh, random external port
    /// (Linux `masquerade random`). Hole-punching requires a relay.
    /// Port-preserving EDM ("SYMPP") exists in the wild but is not
    /// modeled distinctly; see the crate-level note on [`Nat`] for the
    /// reason.
    EndpointDependent,
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
#[non_exhaustive]
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
/// [`NatConfig::builder`], which validates cross-field invariants at
/// construction time.
///
/// The struct is `#[non_exhaustive]` so only the builder and the `Nat`
/// conversion can produce values; callers cannot write a struct literal.
/// Fields remain `pub` for ergonomic read access and pattern matching.
/// Mutating a field on an existing `NatConfig` bypasses the builder's
/// cross-field validation: for example, setting
/// `cfg.mapping = NatMapping::EndpointDependent` on a config with
/// `AddressDependent` filtering or `hairpin = true` produces a combination
/// the nftables backend cannot express. Callers that mutate fields are
/// responsible for maintaining these invariants; the library does not
/// re-validate on apply.
///
/// # Example
/// ```
/// # use patchbay::{NatConfig, NatFiltering, NatMapping};
/// // Symmetric NAT with a short UDP timeout, for fast timeout tests.
/// let cfg = NatConfig::builder()
///     .mapping(NatMapping::EndpointDependent)
///     .filtering(NatFiltering::AddressAndPortDependent)
///     .udp_stream_timeout(60)
///     .build()
///     .expect("valid combination");
/// assert_eq!(cfg.timeouts.udp_stream, 60);
/// ```
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

    /// Checks the cross-field invariants that [`NatConfigBuilder::build`]
    /// enforces.
    ///
    /// [`NatConfig`] exposes `pub` fields, so mutating one after construction
    /// can produce a combination the nftables backend cannot express, such as
    /// switching `mapping` to [`NatMapping::EndpointDependent`] while `hairpin`
    /// is set. The apply path re-runs this check so such a config surfaces as a
    /// [`NatConfigError`] instead of panicking in rule generation. See
    /// [`NatConfigError`] for the specific rules.
    pub fn validate(&self) -> Result<(), NatConfigError> {
        let eim = matches!(self.mapping, NatMapping::EndpointIndependent);
        if !eim && matches!(self.filtering, NatFiltering::AddressDependent) {
            return Err(NatConfigError::AdfRequiresEim);
        }
        if !eim && self.hairpin {
            return Err(NatConfigError::HairpinRequiresEim);
        }
        Ok(())
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
        let cfg = NatConfig {
            mapping: self.mapping,
            filtering: self.filtering,
            timeouts: self.timeouts,
            hairpin: self.hairpin,
        };
        cfg.validate()?;
        Ok(cfg)
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
            .mapping(NatMapping::EndpointDependent)
            .filtering(NatFiltering::AddressDependent)
            .build()
            .unwrap_err();
        assert_eq!(err, NatConfigError::AdfRequiresEim);
    }

    #[test]
    fn builder_rejects_edm_with_hairpin() {
        let err = NatConfig::builder()
            .mapping(NatMapping::EndpointDependent)
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
    fn builder_accepts_edm_apdf() {
        let cfg = NatConfig::builder()
            .mapping(NatMapping::EndpointDependent)
            .filtering(NatFiltering::AddressAndPortDependent)
            .build()
            .unwrap();
        assert_eq!(cfg.mapping, NatMapping::EndpointDependent);
    }

    #[test]
    fn validate_catches_mutated_edm_hairpin() {
        // Fields are public, so a caller can mutate a valid config into an
        // invalid one. `validate` (run again on apply) must catch it.
        let mut cfg = NatConfig::builder()
            .mapping(NatMapping::EndpointIndependent)
            .hairpin(true)
            .build()
            .unwrap();
        cfg.mapping = NatMapping::EndpointDependent;
        assert_eq!(cfg.validate(), Err(NatConfigError::HairpinRequiresEim));
    }

    #[test]
    fn nat_to_config_covers_every_preset() {
        for nat in [Nat::None, Nat::Easiest, Nat::Easy, Nat::Hard] {
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
    fn nat_hard_is_edm_apdf() {
        let cfg = Nat::Hard.to_config().unwrap();
        assert_eq!(cfg.mapping, NatMapping::EndpointDependent);
        assert_eq!(cfg.filtering, NatFiltering::AddressAndPortDependent);
    }
}
