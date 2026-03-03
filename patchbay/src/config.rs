//! TOML configuration structures used by [`crate::Lab::load`].

use std::collections::HashMap;

use serde::Deserialize;

use crate::{IpSupport, Nat, NatV6Mode};

/// Parsed lab configuration from TOML.
#[derive(Deserialize, Clone, Default)]
pub struct LabConfig {
    /// Optional region-latency map.
    pub region: Option<HashMap<String, RegionConfig>>,
    /// Router entries.
    #[serde(default)]
    pub router: Vec<RouterConfig>,
    /// Raw device tables; post-processed by [`crate::Lab::from_config`].
    #[serde(default)]
    pub device: HashMap<String, toml::Value>,
}

/// Per-region latency configuration.
#[derive(Deserialize, Clone)]
pub struct RegionConfig {
    /// Map of target-region name to one-way latency in ms.
    #[serde(default)]
    pub latencies: HashMap<String, u32>,
}

/// Router configuration entry.
#[derive(Deserialize, Clone)]
pub struct RouterConfig {
    /// Router name.
    pub name: String,
    /// Optional region tag (used for inter-region latency rules).
    pub region: Option<String>,
    /// Name of the upstream router.  If absent the router attaches to the IX switch.
    pub upstream: Option<String>,
    /// NAT mode.  Defaults to `"none"` (public downstream, no NAT).
    #[serde(default)]
    pub nat: Nat,
    /// IP address family support.  Defaults to `"v4-only"`.
    #[serde(default)]
    pub ip_support: IpSupport,
    /// IPv6 NAT mode.  Defaults to `"none"`.
    #[serde(default)]
    pub nat_v6: NatV6Mode,
    /// Optional override for RA emission in RA-driven provisioning mode.
    pub ra_enabled: Option<bool>,
    /// Optional RA interval in seconds.
    pub ra_interval_secs: Option<u64>,
}
