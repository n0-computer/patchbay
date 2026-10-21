//! L4 load balancer backed by nftables DNAT rules on a router.
//!
//! The balancer uses the router's existing IX (public) IP as the VIP.
//! Different balancers on the same router use different ports. Backends
//! are private devices behind the router.
//!
//! Traffic flow: client sends to `<router-ix-ip>:<port>` on the IX bridge.
//! The router's DNAT rules rewrite to a backend's private IP. Masquerade
//! ensures return traffic goes through the router.

use std::{
    net::{Ipv4Addr, Ipv6Addr},
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, Result};
use tracing::debug;

use crate::{
    core::{NodeId, RouterData},
    lab::LabInner,
    nft::run_nft_in,
};

// ─────────────────────────────────────────────
// Types
// ─────────────────────────────────────────────

/// Load-balancing algorithm.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LbAlgorithm {
    /// Distribute connections evenly across backends in order.
    #[default]
    RoundRobin,
}

/// Transport protocol for the load balancer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LbProtocol {
    /// TCP (default).
    #[default]
    Tcp,
    /// UDP.
    Udp,
}

impl LbProtocol {
    fn nft_name(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

/// A single backend target: device ID and port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BackendEntry {
    /// Device node identifier.
    pub device_id: NodeId,
    /// Port on the backend device.
    pub port: u16,
}

/// Resolved configuration for a single load balancer, stored on [`RouterData`].
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) struct BalancerConfig {
    /// Human-readable name (e.g. `"web"`).
    pub name: String,
    /// Frontend port on the router's WAN IP.
    pub port: u16,
    /// Backend targets.
    pub backends: Vec<BackendEntry>,
    /// Balancing algorithm.
    pub algorithm: LbAlgorithm,
    /// Transport protocol.
    pub protocol: LbProtocol,
    /// Optional session affinity timeout (not yet wired into nft rules).
    pub affinity: Option<Duration>,
}

// ─────────────────────────────────────────────
// BalancerBuilder
// ─────────────────────────────────────────────

/// Builder for an L4 load balancer on a router.
///
/// Created by [`Router::add_balancer`]. Call `.backend()` to register
/// targets, then `.build().await` to install the nftables rules.
pub struct BalancerBuilder {
    router_id: NodeId,
    lab: Arc<LabInner>,
    name: String,
    port: u16,
    backends: Vec<BackendEntry>,
    algorithm: LbAlgorithm,
    protocol: LbProtocol,
    affinity: Option<Duration>,
}

impl BalancerBuilder {
    /// Adds a backend device at the given port.
    pub fn backend(mut self, device_id: NodeId, port: u16) -> Self {
        self.backends.push(BackendEntry { device_id, port });
        self
    }

    /// Selects round-robin distribution (the default).
    pub fn round_robin(mut self) -> Self {
        self.algorithm = LbAlgorithm::RoundRobin;
        self
    }

    /// Sets the transport protocol.
    pub fn protocol(mut self, proto: LbProtocol) -> Self {
        self.protocol = proto;
        self
    }

    /// Enables session affinity with the given timeout.
    pub fn session_affinity(mut self, duration: Duration) -> Self {
        self.affinity = Some(duration);
        self
    }

    /// Builds the balancer, installs nftables rules, and returns a handle.
    pub async fn build(self) -> Result<Balancer> {
        if self.backends.is_empty() {
            return Err(anyhow!("balancer '{}' has no backends", self.name));
        }

        let config = BalancerConfig {
            name: self.name.clone(),
            port: self.port,
            backends: self.backends,
            algorithm: self.algorithm,
            protocol: self.protocol,
            affinity: self.affinity,
        };

        // Store config on the router.
        {
            let mut core = self.lab.core.lock().expect("poisoned");
            let router = core
                .router_mut(self.router_id)
                .ok_or_else(|| anyhow!("router removed"))?;
            // Check for duplicate name.
            if router.balancers.iter().any(|b| b.name == config.name) {
                return Err(anyhow!(
                    "balancer '{}' already exists on this router",
                    config.name
                ));
            }
            router.balancers.push(config);
        }

        // Apply nftables rules.
        apply_balancer_rules(&self.lab, self.router_id).await?;

        Ok(Balancer {
            router_id: self.router_id,
            name: self.name,
            lab: self.lab,
        })
    }
}

// ─────────────────────────────────────────────
// Balancer handle
// ─────────────────────────────────────────────

/// Handle to an active L4 load balancer on a router.
///
/// Provides read access to the VIP and runtime mutation (add/remove backends).
pub struct Balancer {
    router_id: NodeId,
    name: String,
    lab: Arc<LabInner>,
}

impl Clone for Balancer {
    fn clone(&self) -> Self {
        Self {
            router_id: self.router_id,
            name: self.name.clone(),
            lab: Arc::clone(&self.lab),
        }
    }
}

impl std::fmt::Debug for Balancer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Balancer")
            .field("router_id", &self.router_id)
            .field("name", &self.name)
            .finish()
    }
}

impl Balancer {
    /// Returns the balancer name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the frontend port.
    pub fn port(&self) -> u16 {
        let core = self.lab.core.lock().expect("poisoned");
        core.router(self.router_id)
            .and_then(|r| r.balancers.iter().find(|b| b.name == self.name))
            .map(|b| b.port)
            .unwrap_or(0)
    }

    /// Returns the VIP (router's IX IPv4 address).
    pub fn ip(&self) -> Option<Ipv4Addr> {
        let core = self.lab.core.lock().expect("poisoned");
        core.router(self.router_id).and_then(|r| r.upstream_ip)
    }

    /// Returns the VIP (router's IX IPv6 address).
    pub fn ip6(&self) -> Option<Ipv6Addr> {
        let core = self.lab.core.lock().expect("poisoned");
        core.router(self.router_id).and_then(|r| r.upstream_ip_v6)
    }

    /// Adds a backend device at runtime and regenerates rules.
    pub async fn add_backend(&self, device_id: NodeId, port: u16) -> Result<()> {
        {
            let mut core = self.lab.core.lock().expect("poisoned");
            let router = core
                .router_mut(self.router_id)
                .ok_or_else(|| anyhow!("router removed"))?;
            let cfg = router
                .balancers
                .iter_mut()
                .find(|b| b.name == self.name)
                .ok_or_else(|| anyhow!("balancer '{}' not found", self.name))?;
            if cfg
                .backends
                .iter()
                .any(|b| b.device_id == device_id && b.port == port)
            {
                return Ok(());
            }
            cfg.backends.push(BackendEntry { device_id, port });
        }
        apply_balancer_rules(&self.lab, self.router_id).await
    }

    /// Removes a backend device at runtime and regenerates rules.
    pub async fn remove_backend(&self, device_id: NodeId) -> Result<()> {
        {
            let mut core = self.lab.core.lock().expect("poisoned");
            let router = core
                .router_mut(self.router_id)
                .ok_or_else(|| anyhow!("router removed"))?;
            let cfg = router
                .balancers
                .iter_mut()
                .find(|b| b.name == self.name)
                .ok_or_else(|| anyhow!("balancer '{}' not found", self.name))?;
            cfg.backends.retain(|b| b.device_id != device_id);
        }
        apply_balancer_rules(&self.lab, self.router_id).await
    }
}

// ─────────────────────────────────────────────
// Router / Lab glue (impl blocks on foreign types)
// ─────────────────────────────────────────────

impl crate::Router {
    /// Begins building an L4 load balancer on this router.
    ///
    /// The balancer uses this router's public (IX) IP as the VIP and the
    /// given `port` as the frontend port.
    pub fn add_balancer(&self, name: &str, port: u16) -> BalancerBuilder {
        BalancerBuilder {
            router_id: self.id(),
            lab: Arc::clone(&self.lab),
            name: name.to_string(),
            port,
            backends: Vec::new(),
            algorithm: LbAlgorithm::default(),
            protocol: LbProtocol::default(),
            affinity: None,
        }
    }

    /// Returns a handle to an existing balancer by name.
    pub fn balancer(&self, name: &str) -> Option<Balancer> {
        let core = self.lab.core.lock().expect("poisoned");
        let router = core.router(self.id())?;
        if router.balancers.iter().any(|b| b.name == name) {
            Some(Balancer {
                router_id: self.id(),
                name: name.to_string(),
                lab: Arc::clone(&self.lab),
            })
        } else {
            None
        }
    }
}

// ─────────────────────────────────────────────
// nftables rule generation
// ─────────────────────────────────────────────

/// Resolved backend address for rule generation.
struct ResolvedBackend {
    ip: Ipv4Addr,
    port: u16,
}

/// Resolved v6 backend address for rule generation.
struct ResolvedBackendV6 {
    ip: Ipv6Addr,
    port: u16,
}

/// Regenerates and applies all balancer nftables rules for a router.
///
/// Deletes the old `table ip lb` (and `table ip6 lb`) then recreates
/// them from the current balancer configs stored on the router.
async fn apply_balancer_rules(lab: &Arc<LabInner>, router_id: NodeId) -> Result<()> {
    // Phase 1: lock, snapshot, unlock.
    let (ns, rules) = {
        let core = lab.core.lock().expect("poisoned");
        let router = core
            .router(router_id)
            .ok_or_else(|| anyhow!("router removed"))?;
        let ns = router.ns.clone();

        if router.balancers.is_empty() {
            (ns, None)
        } else {
            let r = generate_all_balancer_rules(router, &core)?;
            (ns, Some(r))
        }
    };

    // Phase 2: apply rules (no lock held).
    // Always delete existing lb tables first (ignoring errors if they
    // do not exist yet).
    run_nft_in(&lab.netns, &ns, "delete table ip lb\n")
        .await
        .ok();
    run_nft_in(&lab.netns, &ns, "delete table ip6 lb\n")
        .await
        .ok();

    match rules {
        None => Ok(()),
        Some(rules) => {
            debug!(ns = %ns, rules = %rules, "balancer: apply rules");
            run_nft_in(&lab.netns, &ns, &rules).await
        }
    }
}

/// Generates the complete nftables ruleset for all balancers on a router.
fn generate_all_balancer_rules(
    router: &RouterData,
    core: &crate::core::NetworkCore,
) -> Result<String> {
    let wan_ip = router
        .upstream_ip
        .ok_or_else(|| anyhow!("router has no WAN IP for balancer"))?;

    let mut rules = String::new();

    // IPv4 table.
    rules.push_str("table ip lb {\n");

    // Per-service chains.
    for cfg in &router.balancers {
        let resolved = resolve_backends_v4(cfg, core)?;
        if resolved.is_empty() {
            continue;
        }
        rules.push_str(&format!("    chain svc_{} {{\n", cfg.name));
        rules.push_str(&generate_dnat_map_v4(&resolved, cfg.protocol));
        rules.push_str("    }\n");
    }

    // Prerouting chain.
    rules.push_str("    chain prerouting {\n");
    rules.push_str("        type nat hook prerouting priority dstnat - 5; policy accept;\n");
    for cfg in &router.balancers {
        let resolved = resolve_backends_v4(cfg, core)?;
        if resolved.is_empty() {
            continue;
        }
        rules.push_str(&format!(
            "        ip daddr {} {} dport {} goto svc_{}\n",
            wan_ip,
            cfg.protocol.nft_name(),
            cfg.port,
            cfg.name
        ));
    }
    rules.push_str("    }\n");

    // Postrouting chain.
    rules.push_str("    chain postrouting {\n");
    rules.push_str("        type nat hook postrouting priority srcnat; policy accept;\n");
    rules.push_str("        ct status dnat masquerade\n");
    rules.push_str("    }\n");

    rules.push_str("}\n");

    // IPv6 table (if the router has an upstream v6 address).
    if let Some(wan_ip6) = router.upstream_ip_v6 {
        rules.push_str("table ip6 lb {\n");

        for cfg in &router.balancers {
            let resolved = resolve_backends_v6(cfg, core);
            if resolved.is_empty() {
                continue;
            }
            rules.push_str(&format!("    chain svc_{} {{\n", cfg.name));
            rules.push_str(&generate_dnat_map_v6(&resolved, cfg.protocol));
            rules.push_str("    }\n");
        }

        rules.push_str("    chain prerouting {\n");
        rules.push_str("        type nat hook prerouting priority dstnat - 5; policy accept;\n");
        for cfg in &router.balancers {
            let resolved = resolve_backends_v6(cfg, core);
            if resolved.is_empty() {
                continue;
            }
            rules.push_str(&format!(
                "        ip6 daddr {} {} dport {} goto svc_{}\n",
                wan_ip6,
                cfg.protocol.nft_name(),
                cfg.port,
                cfg.name
            ));
        }
        rules.push_str("    }\n");

        rules.push_str("    chain postrouting {\n");
        rules.push_str("        type nat hook postrouting priority srcnat; policy accept;\n");
        rules.push_str("        ct status dnat masquerade\n");
        rules.push_str("    }\n");

        rules.push_str("}\n");
    }

    Ok(rules)
}

/// Resolves backend device IDs to IPv4 addresses.
fn resolve_backends_v4(
    cfg: &BalancerConfig,
    core: &crate::core::NetworkCore,
) -> Result<Vec<ResolvedBackend>> {
    let mut out = Vec::new();
    for be in &cfg.backends {
        let dev = core
            .device(be.device_id)
            .ok_or_else(|| anyhow!("backend device {} not found", be.device_id))?;
        if let Some(ip) = dev.default_iface().ip {
            out.push(ResolvedBackend { ip, port: be.port });
        }
    }
    Ok(out)
}

/// Resolves backend device IDs to IPv6 addresses.
fn resolve_backends_v6(
    cfg: &BalancerConfig,
    core: &crate::core::NetworkCore,
) -> Vec<ResolvedBackendV6> {
    let mut out = Vec::new();
    for be in &cfg.backends {
        if let Some(dev) = core.device(be.device_id) {
            if let Some(ip) = dev.default_iface().ip_v6 {
                out.push(ResolvedBackendV6 { ip, port: be.port });
            }
        }
    }
    out
}

/// Generates the DNAT map rule for IPv4 backends.
///
/// The `meta l4proto` match must appear on the same rule as the dnat
/// statement so nft can resolve the port part of the concatenated target.
fn generate_dnat_map_v4(backends: &[ResolvedBackend], proto: LbProtocol) -> String {
    let proto_kw = proto.nft_name();
    if backends.len() == 1 {
        return format!(
            "        meta l4proto {} dnat to {} : {}\n",
            proto_kw, backends[0].ip, backends[0].port
        );
    }
    let mut s = format!(
        "        meta l4proto {} dnat to numgen inc mod {} map {{\n",
        proto_kw,
        backends.len()
    );
    for (i, be) in backends.iter().enumerate() {
        s.push_str(&format!("            {} : {} . {},\n", i, be.ip, be.port));
    }
    s.push_str("        }\n");
    s
}

/// Generates the DNAT map rule for IPv6 backends.
fn generate_dnat_map_v6(backends: &[ResolvedBackendV6], proto: LbProtocol) -> String {
    let proto_kw = proto.nft_name();
    if backends.len() == 1 {
        return format!(
            "        meta l4proto {} dnat to {} . {}\n",
            proto_kw, backends[0].ip, backends[0].port
        );
    }
    let mut s = format!(
        "        meta l4proto {} dnat to numgen inc mod {} map {{\n",
        proto_kw,
        backends.len()
    );
    for (i, be) in backends.iter().enumerate() {
        s.push_str(&format!("            {} : {} . {},\n", i, be.ip, be.port));
    }
    s.push_str("        }\n");
    s
}
