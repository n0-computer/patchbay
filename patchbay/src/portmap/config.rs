//! Configuration types for the port mapping server.

/// Which port mapping protocols a router advertises.
///
/// Maps to a [`PortmapConfig`] at build time; `None` leaves the server off.
/// Off by default on every [`RouterPreset`](crate::RouterPreset) to avoid
/// changing the traffic profile of existing tests. Opt in explicitly with
/// [`RouterBuilder::portmap`](crate::RouterBuilder::portmap) or turn on at
/// runtime with [`Router::set_portmap`](crate::Router::set_portmap).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PortmapMode {
    /// No port mapping server.
    #[default]
    None,
    /// NAT-PMP only (UDP 5351, RFC 6886).
    NatPmpOnly,
    /// PCP only (UDP 5351, RFC 6887).
    PcpOnly,
    /// UPnP IGD:1 only (SSDP + HTTP/SOAP).
    UpnpOnly,
    /// NAT-PMP and PCP sharing UDP 5351, no UPnP.
    NatPmpAndPcp,
    /// All three protocols enabled.
    All,
}

/// Per-router port mapping server configuration.
///
/// Each flag toggles one protocol. Multiple protocols may be enabled at the
/// same time; NAT-PMP and PCP then share the same UDP 5351 socket as they do
/// on real gateways.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PortmapConfig {
    /// Advertise NAT-PMP on UDP 5351.
    pub enable_nat_pmp: bool,
    /// Advertise PCP on UDP 5351.
    pub enable_pcp: bool,
    /// Advertise UPnP IGD via SSDP and HTTP/SOAP.
    pub enable_upnp: bool,
}

impl PortmapConfig {
    /// Returns the config corresponding to `mode`.
    pub fn from_mode(mode: PortmapMode) -> Self {
        match mode {
            PortmapMode::None => Self::default(),
            PortmapMode::NatPmpOnly => Self {
                enable_nat_pmp: true,
                ..Self::default()
            },
            PortmapMode::PcpOnly => Self {
                enable_pcp: true,
                ..Self::default()
            },
            PortmapMode::UpnpOnly => Self {
                enable_upnp: true,
                ..Self::default()
            },
            PortmapMode::NatPmpAndPcp => Self {
                enable_nat_pmp: true,
                enable_pcp: true,
                enable_upnp: false,
            },
            PortmapMode::All => Self {
                enable_nat_pmp: true,
                enable_pcp: true,
                enable_upnp: true,
            },
        }
    }

    /// Returns `true` if any protocol is enabled.
    pub fn any_enabled(&self) -> bool {
        self.enable_nat_pmp || self.enable_pcp || self.enable_upnp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_none_disables_everything() {
        assert!(!PortmapConfig::from_mode(PortmapMode::None).any_enabled());
    }

    #[test]
    fn mode_all_enables_everything() {
        let cfg = PortmapConfig::from_mode(PortmapMode::All);
        assert!(cfg.enable_nat_pmp && cfg.enable_pcp && cfg.enable_upnp);
    }

    #[test]
    fn mode_nat_pmp_and_pcp_leaves_upnp_off() {
        let cfg = PortmapConfig::from_mode(PortmapMode::NatPmpAndPcp);
        assert!(cfg.enable_nat_pmp && cfg.enable_pcp);
        assert!(!cfg.enable_upnp);
    }
}
