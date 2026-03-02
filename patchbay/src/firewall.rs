//! Firewall presets and configuration types.

/// Firewall preset for a router's forward chain.
///
/// Firewall rules are applied as nftables rules in a separate `inet fw` table
/// (priority 10, after NAT filter rules at priority 0). Rules apply to both
/// IPv4 and IPv6 traffic.
///
/// All presets expand to a [`FirewallConfig`] via [`Firewall::to_config`].
/// Use [`Firewall::Custom`] for full control.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Firewall {
    /// No filtering beyond NAT (default).
    #[default]
    None,

    /// Block unsolicited inbound traffic on the WAN interface (RFC 6092).
    ///
    /// Allows all outbound traffic and return traffic for established flows.
    /// Drops new connections arriving from the WAN side. This is the default
    /// security posture of every home router and IPv6 CE router.
    ///
    /// For IPv4 with NAT, this is redundant (NAT + APDF already blocks
    /// inbound). For IPv6 without NAT, this is the primary security boundary
    /// — devices have globally routable addresses but are not reachable from
    /// the internet.
    ///
    /// Observed on: every home router (FritzBox, Unifi, TP-Link, etc.).
    BlockInbound,

    /// Corporate/enterprise firewall.
    ///
    /// Blocks unsolicited inbound. Outbound allows TCP 80, 443 and UDP 53
    /// (DNS) only. All other TCP and UDP are dropped. STUN/ICE fails, must
    /// use TURN-over-TLS on port 443.
    ///
    /// Observed on: Cisco ASA, Palo Alto, Fortinet in enterprise deployments.
    Corporate,

    /// Hotel/airport captive-portal style firewall.
    ///
    /// Blocks unsolicited inbound. Outbound allows TCP on any port plus
    /// UDP 53 (DNS). All other UDP is dropped.
    ///
    /// Observed on: hotel/airport guest WiFi after captive portal auth.
    CaptivePortal,

    /// Fully custom firewall configuration.
    Custom(FirewallConfig),
}

/// Outbound port policy for a protocol (TCP or UDP).
///
/// Controls which destination ports are allowed for outbound traffic
/// traversing the router's forward chain.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum PortPolicy {
    /// All destination ports are allowed (no filtering).
    #[default]
    AllowAll,
    /// Only the listed destination ports are allowed; all others are dropped.
    Allow(Vec<u16>),
    /// All destination ports are blocked.
    BlockAll,
}

/// Firewall configuration controlling inbound and outbound traffic.
///
/// # Example
/// ```
/// # use patchbay::FirewallConfig;
/// let cfg = FirewallConfig::builder()
///     .block_inbound()
///     .outbound_tcp(patchbay::PortPolicy::Allow(vec![80, 443, 8443]))
///     .outbound_udp(patchbay::PortPolicy::Allow(vec![53]))
///     .build();
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FirewallConfig {
    /// Block unsolicited inbound traffic on the WAN interface (RFC 6092).
    /// When true, only established/related return traffic is allowed inbound.
    pub block_inbound: bool,
    /// Outbound TCP port policy.
    pub outbound_tcp: PortPolicy,
    /// Outbound UDP port policy.
    pub outbound_udp: PortPolicy,
}

impl Firewall {
    /// Expands a preset to a [`FirewallConfig`], or returns `None` for [`Firewall::None`].
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

impl FirewallConfig {
    /// Returns a builder for constructing a custom firewall configuration.
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
    /// Block unsolicited inbound traffic on the WAN interface.
    pub fn block_inbound(&mut self) -> &mut Self {
        self.block_inbound = true;
        self
    }

    /// Set outbound TCP port policy.
    pub fn outbound_tcp(&mut self, policy: PortPolicy) -> &mut Self {
        self.outbound_tcp = policy;
        self
    }

    /// Set outbound UDP port policy.
    pub fn outbound_udp(&mut self, policy: PortPolicy) -> &mut Self {
        self.outbound_udp = policy;
        self
    }

    /// Convenience: allow only these outbound TCP destination ports.
    pub fn allow_tcp(&mut self, ports: &[u16]) -> &mut Self {
        self.outbound_tcp = PortPolicy::Allow(ports.to_vec());
        self
    }

    /// Convenience: allow only these outbound UDP destination ports.
    pub fn allow_udp(&mut self, ports: &[u16]) -> &mut Self {
        self.outbound_udp = PortPolicy::Allow(ports.to_vec());
        self
    }

    /// Convenience: block all outbound TCP.
    pub fn block_tcp(&mut self) -> &mut Self {
        self.outbound_tcp = PortPolicy::BlockAll;
        self
    }

    /// Convenience: block all outbound UDP.
    pub fn block_udp(&mut self) -> &mut Self {
        self.outbound_udp = PortPolicy::BlockAll;
        self
    }

    /// Builds the [`FirewallConfig`].
    pub fn build(&self) -> FirewallConfig {
        FirewallConfig {
            block_inbound: self.block_inbound,
            outbound_tcp: self.outbound_tcp.clone(),
            outbound_udp: self.outbound_udp.clone(),
        }
    }
}
