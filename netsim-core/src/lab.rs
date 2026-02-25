//! High-level lab API: [`Lab`], [`DeviceBuilder`], [`NatMode`], [`Impair`], [`ObservedAddr`].

use anyhow::{anyhow, bail, Context, Result};
use nix::unistd::Pid;
use serde::Deserialize;
use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    path::Path,
    process::{Command, ExitStatus},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::Duration,
};

use crate::core::{
    apply_impair_in, apply_nat, cleanup_netns, resources, run_closure_in_namespace,
    run_command_in_namespace, run_nft_in, spawn_closure_in_namespace_thread,
    spawn_command_in_namespace, CoreConfig, DownstreamPool, NetworkCore, NodeId, TaskHandle,
};

pub(crate) static LAB_COUNTER: AtomicU64 = AtomicU64::new(0);

// ─────────────────────────────────────────────
// Public types
// ─────────────────────────────────────────────

/// NAT mode for a router.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, strum::EnumIter, strum::Display)]
#[serde(rename_all = "kebab-case")]
pub enum NatMode {
    /// No NAT — downstream addresses are publicly routable (DC behaviour).
    #[default]
    None,
    /// CGNAT — SNAT subscriber traffic on the IX-facing interface.
    Cgnat,
    /// Endpoint-independent mapping: same external port regardless of destination.
    DestinationIndependent,
    /// Endpoint-dependent (symmetric-ish): different port per destination.
    DestinationDependent,
}

/// Link-layer impairment profile applied via `tc netem`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Impair {
    /// ~20 ms delay, 5 ms jitter.
    Wifi,
    /// ~50 ms delay, 20 ms jitter, 1 % loss.
    Mobile,
    /// Custom impairment settings.
    Manual {
        /// Rate limit in kbit/s.
        rate: u32,
        /// Packet loss percentage (0.0 - 100.0).
        loss: f32,
        /// One-way latency in milliseconds.
        latency: u32,
    },
}

impl<'de> Deserialize<'de> for Impair {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Preset(String),
            Manual { rate: u32, loss: f32, latency: u32 },
        }

        match Repr::deserialize(deserializer)? {
            Repr::Preset(s) => match s.as_str() {
                "wifi" => Ok(Impair::Wifi),
                "mobile" => Ok(Impair::Mobile),
                _ => Err(serde::de::Error::custom("unknown impair preset")),
            },
            Repr::Manual {
                rate,
                loss,
                latency,
            } => Ok(Impair::Manual {
                rate,
                loss,
                latency,
            }),
        }
    }
}

/// Observed external address as reported by a STUN-like reflector.
#[derive(Clone, Debug)]
pub struct ObservedAddr {
    /// External socket address observed by the reflector.
    pub observed: SocketAddr,
}

// ─────────────────────────────────────────────
// Lab
// ─────────────────────────────────────────────

/// High-level lab API built on top of `NetworkCore`.
pub struct Lab {
    /// Short process-unique prefix used on root-namespace interface names.
    prefix: String,
    /// (from_region, to_region, latency_ms) pairs; applied as tc netem during build.
    region_latencies: Vec<(String, String, u32)>,

    /// Background tasks spawned by the lab (reflectors, commands).
    children: Vec<ChildTask>,

    /// Low-level topology model.
    core: NetworkCore,
}

enum ChildTask {
    Process(std::process::Child),
    Thread {
        handle: TaskHandle,
        join: thread::JoinHandle<Result<()>>,
    },
}

impl Lab {
    // ── Constructors ────────────────────────────────────────────────────

    /// Creates a new lab with default address ranges and IX settings.
    pub fn new() -> Self {
        let pid = std::process::id();
        let pid_tag = pid % 9999 + 1;
        let lab_seq = LAB_COUNTER.fetch_add(1, Ordering::Relaxed);
        let uniq = format!("{lab_seq:x}");
        let prefix = format!("lab-p{}{}", pid_tag, uniq); // e.g. "lab-p12340"
        let root_ns = format!("{prefix}-root");
        let bridge_tag = format!("p{}{}", pid_tag, uniq);
        let ix_gw = Ipv4Addr::new(203, 0, 113, 1);
        resources().register_prefix(&prefix);
        resources().register_prefix(&format!("br-{}-", bridge_tag));
        let core = NetworkCore::new(CoreConfig {
            prefix: prefix.clone(),
            root_ns,
            bridge_tag,
            ix_br: format!("br-p{}{}-1", pid_tag, uniq),
            ix_gw,
            ix_cidr: "203.0.113.0/24".parse().expect("valid ix cidr"),
            private_cidr: "10.0.0.0/16".parse().expect("valid private cidr"),
            public_cidr: "203.0.113.0/24".parse().expect("valid public cidr"),
        });
        Self {
            prefix,
            region_latencies: vec![],
            children: vec![],
            core,
        }
    }

    /// Returns the unique resource prefix associated with this lab instance.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Returns the dedicated lab root namespace name.
    pub fn root_namespace_name(&self) -> &str {
        self.core.root_ns()
    }

    /// Parses `lab.toml`, builds the network, and returns a ready-to-use lab.
    pub async fn load(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path).context("read lab config")?;
        let cfg: crate::config::LabConfig = toml::from_str(&text).context("parse lab config")?;
        let mut lab = Self::from_config(cfg)?;
        lab.build().await?;
        Ok(lab)
    }

    /// Builds a `Lab` from a parsed config without building the network yet.
    pub fn from_config(cfg: crate::config::LabConfig) -> Result<Self> {
        let mut lab = Self::new();

        // Region latency pairs.
        if let Some(regions) = &cfg.region {
            for (from, rcfg) in regions {
                for (to, &ms) in &rcfg.latencies {
                    lab.region_latencies.push((from.clone(), to.clone(), ms));
                }
            }
        }

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
                let upstream = rcfg
                    .upstream
                    .as_deref()
                    .and_then(|n| lab.core.router_id_by_name(n));
                let mut rb = lab.add_router(&rcfg.name).nat(rcfg.nat);
                if let Some(r) = &rcfg.region { rb = rb.region(r); }
                if let Some(u) = upstream { rb = rb.upstream(u); }
                rb.build()?;
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
            ifaces: Vec<(String, NodeId, Option<Impair>)>,
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
                    let router_id = lab.core.router_id_by_name(gw_name).ok_or_else(|| {
                        anyhow!(
                            "device '{}' iface '{}' references unknown router '{}'",
                            dev_name,
                            ifname,
                            gw_name
                        )
                    })?;
                    let impair: Option<Impair> = match iface_table.get("impair") {
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
            builder.build()?;
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
    /// Default NAT mode is [`NatMode::None`] (public DC-style router, IX-connected).
    pub fn add_router(&mut self, name: &str) -> RouterBuilder<'_> {
        if self.core.router_id_by_name(name).is_some() {
            return RouterBuilder {
                lab: self,
                name: name.to_string(),
                region: None,
                upstream: None,
                nat: NatMode::None,
                result: Err(anyhow!("router '{}' already exists", name)),
            };
        }
        RouterBuilder {
            lab: self,
            name: name.to_string(),
            region: None,
            upstream: None,
            nat: NatMode::None,
            result: Ok(()),
        }
    }

    /// Begins building a device; returns a [`DeviceBuilder`] to configure interfaces.
    ///
    /// Call [`.iface()`][DeviceBuilder::iface] one or more times to attach network
    /// interfaces, then [`.build()`][DeviceBuilder::build] to finalize.
    pub fn add_device(&mut self, name: &str) -> DeviceBuilder<'_> {
        if self.core.device_id_by_name(name).is_some() {
            return DeviceBuilder {
                lab: self,
                id: NodeId(u64::MAX),
                result: Err(anyhow!("device '{}' already exists", name)),
            };
        }
        let id = self.core.add_device(name);
        DeviceBuilder {
            lab: self,
            id,
            result: Ok(()),
        }
    }

    // ── build ────────────────────────────────────────────────────────────

    /// Creates all namespaces, links, addresses, routes, and NAT rules.
    pub async fn build(&mut self) -> Result<()> {
        self.core.build(&self.region_latencies).await
    }

    // ── User-facing API ─────────────────────────────────────────────────

    /// Adds a one-way inter-region latency in milliseconds.
    pub fn add_region_latency(&mut self, from: &str, to: &str, latency_ms: u32) {
        self.region_latencies
            .push((from.to_string(), to.to_string(), latency_ms));
    }

    /// Runs a command inside a device namespace and waits for exit.
    pub fn run_on(&self, name: &str, cmd: Command) -> Result<ExitStatus> {
        let id = self.resolve_device(name)?;
        let ns = self.core.device_ns(id)?;
        run_command_in_namespace(ns, cmd)
    }

    /// Runs a closure inside a named network namespace.
    pub fn run_in_namespace<F, R>(ns_name: &str, f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        run_closure_in_namespace(ns_name, f)
    }

    /// Spawns a thread-backed task that runs `f` in `ns_name`.
    pub fn run_in_namespace_thread<F, R>(ns_name: &str, f: F) -> thread::JoinHandle<Result<R>>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        spawn_closure_in_namespace_thread(ns_name.to_string(), f)
    }

    /// Spawns a long-running process inside a device namespace and returns its PID.
    pub fn spawn_on(&mut self, name: &str, cmd: Command) -> Result<Pid> {
        let id = self.resolve_device(name)?;
        let ns = self.core.device_ns(id)?.to_string();
        let child = spawn_command_in_namespace(&ns, cmd)?;
        let pid = Pid::from_raw(child.id() as i32);
        self.children.push(ChildTask::Process(child));
        Ok(pid)
    }

    /// Spawns an unmanaged command in a device namespace and returns the raw `Child`.
    pub fn spawn_unmanaged_on(&self, device: &str, cmd: Command) -> Result<std::process::Child> {
        let id = self.resolve_device(device)?;
        let ns = self.core.device_ns(id)?.to_string();
        spawn_command_in_namespace(&ns, cmd)
    }

    /// Returns the network namespace name for a device by name.
    pub fn device_ns_name(&self, device: &str) -> Result<String> {
        let id = self.resolve_device(device)?;
        Ok(self.core.device_ns(id)?.to_string())
    }

    /// Returns the network namespace name for a router by name.
    pub fn router_ns_name(&self, router: &str) -> Result<String> {
        let id = self.resolve_router(router)?;
        Ok(self.core.router_ns(id)?.to_string())
    }

    /// Builds a map of `NETSIM_*` environment variables from the current lab state.
    pub fn env_vars(&self) -> std::collections::HashMap<String, String> {
        let mut map = std::collections::HashMap::new();
        for dev in self.core.all_devices() {
            let norm = normalize_env_name(&dev.name);
            // Default-via IP
            if let Some(ip) = dev.default_iface().ip {
                map.insert(format!("NETSIM_IP_{}", norm), ip.to_string());
            }
            // Per-interface IPs
            for iface in &dev.interfaces {
                if let Some(ip) = iface.ip {
                    let ifnorm = normalize_env_name(&iface.ifname);
                    map.insert(format!("NETSIM_IP_{}_{}", norm, ifnorm), ip.to_string());
                }
            }
            // Namespace name
            map.insert(format!("NETSIM_NS_{}", norm), dev.ns.clone());
        }
        map
    }

    // ── Reflector / probe helpers (mainly for tests) ─────────────────────

    /// Spawns a UDP reflector in a named device/router namespace.
    pub fn spawn_reflector(&mut self, ns_name: &str, bind: SocketAddr) -> Result<TaskHandle> {
        let (handle, join) = crate::test_utils::spawn_reflector_in(ns_name, bind)?;
        self.children.push(ChildTask::Thread {
            handle: handle.clone(),
            join,
        });
        Ok(handle)
    }

    /// Spawns a UDP reflector in the lab root namespace (IX bridge side).
    pub fn spawn_reflector_on_ix(&mut self, bind: SocketAddr) -> Result<TaskHandle> {
        let (handle, join) = crate::test_utils::spawn_reflector_in(self.core.root_ns(), bind)?;
        self.children.push(ChildTask::Thread {
            handle: handle.clone(),
            join,
        });
        Ok(handle)
    }

    /// Probes the NAT mapping seen by a reflector from a named device.
    pub fn probe_udp_mapping(&self, device: &str, reflector: SocketAddr) -> Result<ObservedAddr> {
        let id = self.resolve_device(device)?;
        let ns = self.core.device_ns(id)?;
        let base = 40000u16;
        let port = base + ((id.0 % 20000) as u16);
        crate::test_utils::probe_in_ns(ns, reflector, Duration::from_millis(500), Some(port))
    }

    // ── Lookup helpers ───────────────────────────────────────────────────

    /// Returns the network namespace name for a node.
    pub fn node_ns(&self, id: NodeId) -> Result<&str> {
        if let Some(r) = self.core.router(id) {
            return Ok(&r.ns);
        }
        if let Some(d) = self.core.device(id) {
            return Ok(&d.ns);
        }
        Err(anyhow!("unknown node id"))
    }

    /// Returns the router's downstream gateway IP.
    pub fn router_downlink_gw(&self, id: NodeId) -> Result<Ipv4Addr> {
        self.core
            .router(id)
            .and_then(|rt| rt.downstream_gw)
            .ok_or_else(|| anyhow!("router missing downstream gw"))
    }

    /// Returns the router's uplink IP.
    pub fn router_uplink_ip(&self, id: NodeId) -> Result<Ipv4Addr> {
        self.core
            .router(id)
            .and_then(|rt| rt.upstream_ip)
            .ok_or_else(|| anyhow!("router missing upstream ip"))
    }

    /// Returns the assigned IP of a device's default interface.
    pub fn device_ip(&self, id: NodeId) -> Result<Ipv4Addr> {
        self.core
            .device(id)
            .map(|dev| dev.default_iface().ip)
            .ok_or_else(|| anyhow!("unknown device id"))?
            .ok_or_else(|| anyhow!("device default interface missing ip"))
    }

    /// Resolves a router name to its [`NodeId`].
    pub fn router_id(&self, name: &str) -> Option<NodeId> {
        self.core.router_id_by_name(name)
    }

    /// Resolves a device name to its [`NodeId`].
    pub fn device_id(&self, name: &str) -> Option<NodeId> {
        self.core.device_id_by_name(name)
    }

    /// Returns the IX gateway IP (203.0.113.1).
    pub fn ix_gw(&self) -> Ipv4Addr {
        self.core.ix_gw()
    }

    /// Safety-net cleanup via prefix scan (normal cleanup happens in `NetworkCore::drop`).
    pub fn cleanup(&self) {
        resources().cleanup_registered_prefixes();
    }

    /// Removes any resources whose names match the lab prefix.
    pub fn cleanup_everything() {
        resources().cleanup_registered_prefixes();
    }

    // ── Private helpers ──────────────────────────────────────────────────

    fn dev_ns(&self, device: &str) -> Result<String> {
        let id = self.resolve_device(device)?;
        Ok(self.core.device_ns(id)?.to_string())
    }

    fn resolve_device(&self, name: &str) -> Result<NodeId> {
        self.core
            .device_id_by_name(name)
            .ok_or_else(|| anyhow!("unknown device '{}'", name))
    }

    fn resolve_router(&self, name: &str) -> Result<NodeId> {
        self.core
            .router_id_by_name(name)
            .ok_or_else(|| anyhow!("unknown router '{}'", name))
    }

    // ── Dynamic operations ────────────────────────────────────────────────

    /// Applies or removes a link-layer impairment on a device interface.
    pub fn set_impair(
        &mut self,
        device: &str,
        ifname: Option<&str>,
        impair: Option<Impair>,
    ) -> Result<()> {
        let id = self.resolve_device(device)?;
        let (ns, resolved_ifname) = {
            let dev = self
                .core
                .device(id)
                .ok_or_else(|| anyhow!("unknown device id"))?;
            let iname = ifname.unwrap_or(&dev.default_via).to_string();
            if dev.iface(&iname).is_none() {
                bail!("interface '{}' not found on device '{}'", iname, device);
            }
            (dev.ns.clone(), iname)
        };
        match impair {
            Some(imp) => apply_impair_in(&ns, &resolved_ifname, imp),
            None => crate::qdisc::remove_qdisc(&ns, &resolved_ifname),
        }
        // Update stored impair so switch_route can re-apply it correctly.
        if let Some(dev) = self.core.device_mut(id) {
            if let Some(iface) = dev.iface_mut(&resolved_ifname) {
                iface.impair = impair;
            }
        }
        Ok(())
    }

    /// Brings a device interface administratively down.
    pub async fn link_down(&mut self, device: &str, ifname: &str) -> Result<()> {
        let ns = self.dev_ns(device)?;
        self.core.set_link_state_in_namespace(&ns, ifname, false).await
    }

    /// Brings a device interface administratively up.
    ///
    /// Linux removes routes via an interface when it goes admin-down, so we
    /// re-add the default route if `ifname` is the device's current `default_via`.
    pub async fn link_up(&mut self, device: &str, ifname: &str) -> Result<()> {
        let id = self.resolve_device(device)?;
        let (ns, uplink, is_default_via) = {
            let dev = self
                .core
                .device(id)
                .ok_or_else(|| anyhow!("unknown device id"))?;
            let iface = dev
                .iface(ifname)
                .ok_or_else(|| anyhow!("interface '{}' not found on device '{}'", ifname, device))?;
            (dev.ns.clone(), iface.uplink, dev.default_via == ifname)
        };
        self.core.set_link_state_in_namespace(&ns, ifname, true).await?;
        if is_default_via {
            let gw_ip = self.core.router_downlink_gw_for_switch(uplink)?;
            self.core
                .replace_default_route_in_namespace(&ns, ifname, gw_ip)
                .await?;
        }
        Ok(())
    }

    /// Switches the active default route to a different interface.
    pub async fn switch_route(&mut self, device: &str, to: &str) -> Result<()> {
        let id = self.resolve_device(device)?;
        let (ns, uplink, impair) = {
            let dev = self
                .core
                .device(id)
                .ok_or_else(|| anyhow!("unknown device id"))?;
            let iface = dev
                .iface(to)
                .ok_or_else(|| anyhow!("interface '{}' not found on device '{}'", to, device))?;
            (dev.ns.clone(), iface.uplink, iface.impair)
        };
        let gw_ip = self.core.router_downlink_gw_for_switch(uplink)?;
        self.core
            .replace_default_route_in_namespace(&ns, to, gw_ip)
            .await?;
        match impair {
            Some(imp) => apply_impair_in(&ns, to, imp),
            None => crate::qdisc::remove_qdisc(&ns, to),
        }
        self.core.set_device_default_via(id, to)?;
        Ok(())
    }

    /// Returns the bridge interface name used for the router's downstream LAN.
    pub fn router_downlink_bridge(&self, router: NodeId) -> Result<String> {
        self.core
            .router(router)
            .map(|r| r.downlink_bridge.clone())
            .context("unknown router id")
    }

    /// Replaces NAT rules on `router` with `mode` at runtime.
    ///
    /// Flushes the `ip nat` table then re-applies the new rules.
    pub async fn set_nat_mode(&mut self, router: NodeId, mode: NatMode) -> Result<()> {
        let (ns, lan_if, wan_if, wan_ip) = self.core.router_nat_params(router)?;
        run_nft_in(&ns, "flush table ip nat").await.ok();
        apply_nat(&ns, mode, &lan_if, &wan_if, wan_ip).await?;
        self.core.set_router_nat_mode(router, mode)
    }

    /// Flushes the conntrack table for `router`, forcing all active NAT mappings to expire.
    ///
    /// Subsequent flows get new external port assignments. Requires `conntrack-tools`.
    pub fn rebind_nats(&mut self, router: NodeId) -> Result<()> {
        let ns = self.core.router_ns(router)?.to_string();
        run_closure_in_namespace(&ns, || {
            let st = std::process::Command::new("conntrack")
                .arg("-F")
                .status()?;
            if !st.success() {
                bail!("conntrack -F failed: {st}");
            }
            Ok(())
        })
    }

    /// Applies or removes impairment on a named interface of `router`.
    ///
    /// Use [`router_downlink_bridge`] to get the LAN-facing bridge name for
    /// download-direction rate limiting.
    pub fn set_router_impair(
        &mut self,
        router: NodeId,
        ifname: &str,
        impair: Option<Impair>,
    ) -> Result<()> {
        let ns = self.core.router_ns(router)?.to_string();
        match impair {
            Some(imp) => apply_impair_in(&ns, ifname, imp),
            None => crate::qdisc::remove_qdisc(&ns, ifname),
        }
        Ok(())
    }

    /// Like [`crate::test_utils::probe_in_ns`] but with an explicit bind address.
    pub fn probe_in_ns_from(
        ns: &str,
        reflector: SocketAddr,
        bind: SocketAddr,
        timeout: Duration,
    ) -> Result<ObservedAddr> {
        use std::net::UdpSocket;
        let ns_name = ns.to_string();
        run_closure_in_namespace(&ns_name, move || {
            let sock = UdpSocket::bind(bind)?;
            sock.set_read_timeout(Some(timeout))?;
            let mut buf = [0u8; 512];
            for _attempt in 1..=3 {
                sock.send_to(b"PROBE", reflector)?;
                match sock.recv_from(&mut buf) {
                    Ok((n, _)) => {
                        let s = std::str::from_utf8(&buf[..n])?;
                        let addr_str = s
                            .strip_prefix("OBSERVED ")
                            .ok_or_else(|| anyhow!("unexpected reflector reply: {:?}", s))?;
                        return Ok(ObservedAddr {
                            observed: addr_str.parse()?,
                        });
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            Err(anyhow!("probe timed out after 3 attempts"))
        })
    }
}

impl Default for Lab {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        for child in self.children.drain(..) {
            match child {
                ChildTask::Process(mut proc) => {
                    let _ = proc.kill();
                    let _ = proc.wait();
                }
                ChildTask::Thread { handle, join } => {
                    handle.stop();
                    let _ = join.join();
                }
            }
        }
        for ns_name in self.core.all_ns_names() {
            cleanup_netns(&ns_name);
        }
    }
}

// ─────────────────────────────────────────────
// RouterBuilder
// ─────────────────────────────────────────────

/// Builder for a router node; returned by [`Lab::add_router`].
pub struct RouterBuilder<'lab> {
    lab: &'lab mut Lab,
    name: String,
    region: Option<String>,
    upstream: Option<NodeId>,
    nat: NatMode,
    result: Result<()>,
}

impl<'lab> RouterBuilder<'lab> {
    /// Sets the region tag used for inter-region latency rules.
    pub fn region(mut self, region: &str) -> Self {
        if self.result.is_ok() {
            self.region = Some(region.to_string());
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

    /// Sets the NAT mode. Defaults to [`NatMode::None`] (no NAT, public addressing).
    pub fn nat(mut self, mode: NatMode) -> Self {
        if self.result.is_ok() {
            self.nat = mode;
        }
        self
    }

    /// Finalises the router and returns its [`NodeId`].
    pub fn build(self) -> Result<NodeId> {
        self.result?;
        let lab = self.lab;
        let nat = self.nat;
        let downstream_pool = if nat == NatMode::None {
            DownstreamPool::Public
        } else {
            DownstreamPool::Private
        };
        let id = lab
            .core
            .add_router(&self.name, nat, downstream_pool, self.region);
        let sub_switch = lab
            .core
            .add_switch(&format!("{}-sub", self.name), None, None);
        lab.core.connect_router_downlink(id, sub_switch)?;
        match self.upstream {
            None => {
                let ix_ip = lab.core.alloc_ix_ip_low();
                lab.core
                    .connect_router_uplink(id, lab.core.ix_sw(), Some(ix_ip))?;
            }
            Some(parent_id) => {
                let parent_downlink = lab
                    .core
                    .router(parent_id)
                    .and_then(|r| r.downlink)
                    .ok_or_else(|| anyhow!("parent router missing downlink switch"))?;
                lab.core.connect_router_uplink(id, parent_downlink, None)?;
            }
        }
        Ok(id)
    }
}

// ─────────────────────────────────────────────
// DeviceBuilder
// ─────────────────────────────────────────────

/// Builder for a device node; returned by [`Lab::add_device`].
pub struct DeviceBuilder<'lab> {
    lab: &'lab mut Lab,
    id: NodeId,
    result: Result<()>,
}

impl<'lab> DeviceBuilder<'lab> {
    /// Attach `ifname` inside the device namespace to `router`'s downstream switch.
    pub fn iface(mut self, ifname: &str, router: NodeId, impair: Option<Impair>) -> Self {
        if self.result.is_ok() {
            self.result = self
                .lab
                .core
                .add_device_iface(self.id, ifname, router, impair)
                .map(|_| ());
        }
        self
    }

    /// Overrides which interface carries the default route.
    pub fn default_via(mut self, ifname: &str) -> Self {
        if self.result.is_ok() {
            self.result = self.lab.core.set_device_default_via(self.id, ifname);
        }
        self
    }

    /// Finalizes the device and returns its [`NodeId`].
    pub fn build(self) -> Result<NodeId> {
        self.result?;
        Ok(self.id)
    }
}

// ─────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────

/// Normalise a device/interface name for use in an environment variable name.
fn normalize_env_name(s: &str) -> String {
    s.to_uppercase().replace('-', "_")
}

// ─────────────────────────────────────────────
// Test ctor bootstrap
// ─────────────────────────────────────────────

#[cfg(test)]
mod test_init {
    #[ctor::ctor]
    fn init() {
        let _ = crate::init_userns();
    }
}

#[cfg(test)]
mod tests {
    use anyhow::{anyhow, bail, Context, Result};
    use n0_tracing_test::traced_test;
    use std::io::{Read, Write};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::thread;
    use std::time::Duration;
    use tracing::debug;

    use super::*;
    use crate::check_caps;
    use crate::config;
    use crate::core::{run_closure_in_namespace, run_command_in_namespace};
    use crate::netns::spawn_task_in_netns;
    use crate::test_utils::{udp_roundtrip_in_ns, udp_rtt_in_ns};

    fn ping_in_ns(ns: &str, addr: &str) -> Result<()> {
        let mut cmd = std::process::Command::new("ping");
        cmd.args(["-c", "1", "-W", "1", addr]);
        let status = run_command_in_namespace(ns, cmd)?;
        if !status.success() {
            bail!("ping {} failed with status {}", addr, status);
        }
        Ok(())
    }

    fn ping_fails_in_ns(ns: &str, addr: &str) -> Result<()> {
        let mut cmd = std::process::Command::new("ping");
        cmd.args(["-c", "1", "-W", "1", addr]);
        let status = run_command_in_namespace(ns, cmd)?;
        if status.success() {
            bail!("ping {} unexpectedly succeeded", addr);
        }
        Ok(())
    }

    #[derive(Clone, Copy, Debug, strum::EnumIter, strum::Display)]
    enum UplinkWiring {
        DirectIx,
        ViaPublicIsp,
        ViaCgnatIsp,
    }

    impl UplinkWiring {
        fn label(self) -> &'static str {
            match self {
                Self::DirectIx => "direct-ix",
                Self::ViaPublicIsp => "via-public-isp",
                Self::ViaCgnatIsp => "via-cgnat-isp",
            }
        }
    }

    #[derive(Clone, Copy, Debug, strum::EnumIter, strum::Display)]
    enum Proto {
        Udp,
        Tcp,
    }

    #[derive(Clone, Copy, Debug, strum::EnumIter, strum::Display)]
    enum BindMode {
        Unspecified,
        SpecificIp,
    }

    struct NatTestCtx {
        dev_ns: String,
        dev_ip: Ipv4Addr,
        expected_ip: Ipv4Addr,
        r_dc: SocketAddr,
        r_ix: SocketAddr,
    }

    struct DualNatLab {
        lab: Lab,
        dev: NodeId,
        nat_a: NodeId,
        nat_b: NodeId,
        reflector: SocketAddr,
    }

    // ── Test helper functions ────────────────────────────────────────────

    /// UDP probe with explicit bind address.
    fn probe_udp(ns: &str, reflector: SocketAddr, bind: SocketAddr) -> Result<ObservedAddr> {
        Lab::probe_in_ns_from(ns, reflector, bind, Duration::from_millis(500))
    }

    /// TCP probe from `ns`, reads "OBSERVED {addr}" from server.
    ///
    /// The `_bind` address is accepted for API parity with `probe_udp`; in
    /// practice the OS always picks the device's primary IP as source address
    /// (since there is only one default-route interface in test topologies).
    async fn probe_tcp(ns: &str, target: SocketAddr, _bind: SocketAddr) -> Result<ObservedAddr> {
        use tokio::io::AsyncReadExt;
        let ns = ns.to_string();
        let timeout = Duration::from_millis(500);
        spawn_task_in_netns(&ns, move || async move {
            let mut stream = tokio::time::timeout(timeout, tokio::net::TcpStream::connect(target))
                .await
                .context("tcp connect timeout")?
                .context("tcp connect")?;
            let mut buf = vec![0u8; 256];
            let n = tokio::time::timeout(timeout, stream.read(&mut buf))
                .await
                .context("tcp read timeout")?
                .context("tcp read")?;
            let s = std::str::from_utf8(&buf[..n]).context("utf8")?;
            let addr_str = s
                .strip_prefix("OBSERVED ")
                .ok_or_else(|| anyhow!("unexpected tcp reflector reply: {:?}", s))?;
            Ok::<_, anyhow::Error>(ObservedAddr {
                observed: addr_str.parse().context("parse observed addr")?,
            })
        })
        .await
        .map_err(|_| anyhow!("probe_tcp: netns task cancelled"))?
    }

    async fn probe_reflexive_addr(
        proto: Proto,
        bind: BindMode,
        ns: &str,
        dev_ip: Ipv4Addr,
        reflector: SocketAddr,
    ) -> Result<ObservedAddr> {
        let bind_addr = match bind {
            BindMode::Unspecified => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            BindMode::SpecificIp => SocketAddr::new(IpAddr::V4(dev_ip), 0),
        };
        match proto {
            Proto::Udp => probe_udp(ns, reflector, bind_addr),
            Proto::Tcp => probe_tcp(ns, reflector, bind_addr).await,
        }
    }

    async fn probe_reflexive(
        proto: Proto,
        bind: BindMode,
        ctx: &NatTestCtx,
    ) -> Result<ObservedAddr> {
        probe_reflexive_addr(proto, bind, &ctx.dev_ns, ctx.dev_ip, ctx.r_dc).await
    }

    /// TCP reflector: accept → "OBSERVED {peer}" → close, repeat until stop.
    fn spawn_tcp_reflector(
        ns: &str,
        bind: SocketAddr,
    ) -> (crate::core::TaskHandle, thread::JoinHandle<Result<()>>) {
        use std::io::Write as _;
        let ns = ns.to_string();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let join = spawn_closure_in_namespace_thread(ns, move || {
            let listener = std::net::TcpListener::bind(bind).context("tcp reflector bind")?;
            listener
                .set_nonblocking(true)
                .context("set nonblocking")?;
            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, peer)) => {
                        stream.set_nonblocking(false).ok();
                        let msg = format!("OBSERVED {}", peer);
                        let _ = stream.write_all(msg.as_bytes());
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(_) => break,
                }
            }
            Ok(())
        });
        (crate::core::TaskHandle::new(stop_tx), join)
    }

    /// TCP sink: accept one connection, drain all bytes, exit.
    fn spawn_tcp_sink(
        server_ns: &str,
        addr: SocketAddr,
    ) -> thread::JoinHandle<Result<()>> {
        use std::io::Read as _;
        let ns = server_ns.to_string();
        spawn_closure_in_namespace_thread(ns, move || {
            let listener = std::net::TcpListener::bind(addr).context("tcp sink bind")?;
            let (mut stream, _) = listener.accept().context("tcp sink accept")?;
            let mut buf = [0u8; 8192];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            Ok(())
        })
    }

    /// Sends `bytes` bytes over TCP from `client_ns` to `server_addr`.
    /// Returns `(elapsed, kbit/s)`.
    fn tcp_measure_throughput(
        client_ns: &str,
        server_addr: SocketAddr,
        bytes: usize,
    ) -> Result<(Duration, u32)> {
        use std::io::Write as _;
        use std::io::Read as _;
        use std::time::Instant;
        let ns = client_ns.to_string();
        run_closure_in_namespace(&ns, move || {
            let mut stream = std::net::TcpStream::connect_timeout(
                &server_addr,
                Duration::from_secs(5),
            )
            .context("tcp connect")?;
            stream
                .set_write_timeout(Some(Duration::from_secs(60)))
                .context("set write timeout")?;
            let chunk = vec![0u8; 4096];
            let start = Instant::now();
            let mut sent = 0;
            while sent < bytes {
                let n = chunk.len().min(bytes - sent);
                stream.write_all(&chunk[..n]).context("tcp write")?;
                sent += n;
            }
            stream
                .shutdown(std::net::Shutdown::Write)
                .context("tcp shutdown")?;
            // Wait for server to acknowledge EOF.
            let mut tmp = [0u8; 1];
            let _ = stream.read(&mut tmp);
            let elapsed = start.elapsed();
            let kbps = ((bytes as u64 * 8) / (elapsed.as_millis() as u64).max(1)) as u32;
            Ok((elapsed, kbps))
        })
    }

    /// Sends `total` UDP datagrams from `ns` to `target` and collects echoes.
    /// Returns `(sent, received)`.
    fn udp_send_recv_count(
        ns: &str,
        target: SocketAddr,
        total: usize,
        payload: usize,
        wait: Duration,
    ) -> Result<(usize, usize)> {
        use std::time::Instant;
        let ns = ns.to_string();
        run_closure_in_namespace(&ns, move || {
            let sock = std::net::UdpSocket::bind("0.0.0.0:0").context("udp bind")?;
            sock.set_read_timeout(Some(Duration::from_millis(200)))
                .context("set timeout")?;
            let buf = vec![0u8; payload];
            let mut recv_buf = vec![0u8; payload + 64];
            for _ in 0..total {
                let _ = sock.send_to(&buf, target);
            }
            let deadline = Instant::now() + wait;
            let mut received = 0usize;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let timeout = remaining.min(Duration::from_millis(200));
                sock.set_read_timeout(Some(timeout)).ok();
                match sock.recv_from(&mut recv_buf) {
                    Ok(_) => received += 1,
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
            }
            Ok((total, received))
        })
    }

    /// Spawns an async TCP reflector (accept → "OBSERVED {peer}" → close) in `ns`.
    ///
    /// Returns when the listener is bound. The task continues on the namespace's
    /// persistent async worker until the listener is closed.
    async fn spawn_tcp_reflector_in_ns(ns: &str, bind: SocketAddr) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let ns = ns.to_string();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<()>>();
        spawn_task_in_netns(&ns, move || async move {
            match tokio::net::TcpListener::bind(bind).await {
                Ok(listener) => {
                    let _ = ready_tx.send(Ok(()));
                    loop {
                        let Ok((mut stream, peer)) = listener.accept().await else { break };
                        let msg = format!("OBSERVED {}", peer);
                        let _ = stream.write_all(msg.as_bytes()).await;
                    }
                }
                Err(e) => {
                    let _ = ready_tx
                        .send(Err(anyhow!("tcp reflector bind {bind}: {e}")));
                }
            }
        });
        ready_rx
            .await
            .map_err(|_| anyhow!("tcp reflector task dropped before ready"))?
    }

    async fn build_nat_case(
        nat_mode: NatMode,
        wiring: UplinkWiring,
        port_base: u16,
    ) -> Result<(Lab, NatTestCtx)> {
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").region("eu").build()?;
        let upstream = match wiring {
            UplinkWiring::DirectIx => None,
            UplinkWiring::ViaPublicIsp => {
                Some(lab.add_router("isp").region("eu").build()?)
            }
            UplinkWiring::ViaCgnatIsp => {
                Some(lab.add_router("isp").region("eu").nat(NatMode::Cgnat).build()?)
            }
        };
        let nat = { let mut rb = lab.add_router("nat").nat(nat_mode); if let Some(u) = upstream { rb = rb.upstream(u); } rb.build()? };
        let dev = lab.add_device("dev").iface("eth0", nat, None).build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r_dc = SocketAddr::new(IpAddr::V4(dc_ip), port_base);
        let r_ix = SocketAddr::new(IpAddr::V4(lab.ix_gw()), port_base + 1);
        let dc_ns = lab.node_ns(dc)?.to_string();

        // UDP reflector (managed by lab).
        lab.spawn_reflector(&dc_ns, r_dc)?;
        lab.spawn_reflector_on_ix(r_ix)?;

        // TCP reflector on the namespace's async worker.
        spawn_tcp_reflector_in_ns(&dc_ns, r_dc).await?;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let dev_ns = lab.node_ns(dev)?.to_string();
        let dev_ip = lab.device_ip(dev)?;
        let expected_ip = match (nat_mode, wiring) {
            (_, UplinkWiring::ViaCgnatIsp) => {
                let isp = lab.router_id("isp").context("missing isp")?;
                lab.router_uplink_ip(isp)?
            }
            (NatMode::None, _) => dev_ip,
            _ => lab.router_uplink_ip(nat)?,
        };
        Ok((
            lab,
            NatTestCtx {
                dev_ns,
                dev_ip,
                expected_ip,
                r_dc,
                r_ix,
            },
        ))
    }

    async fn build_dual_nat_lab(
        mode_a: NatMode,
        mode_b: NatMode,
        port_base: u16,
    ) -> Result<DualNatLab> {
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").region("eu").build()?;
        let nat_a = lab.add_router("nat-a").nat(mode_a).build()?;
        let nat_b = lab.add_router("nat-b").nat(mode_b).build()?;
        let dev = lab
            .add_device("dev")
            .iface("eth0", nat_a, None)
            .iface("eth1", nat_b, None)
            .default_via("eth0")
            .build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let reflector = SocketAddr::new(IpAddr::V4(dc_ip), port_base);
        let dc_ns = lab.node_ns(dc)?.to_string();

        lab.spawn_reflector(&dc_ns, reflector)?;
        spawn_tcp_reflector_in_ns(&dc_ns, reflector).await?;

        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(DualNatLab {
            lab,
            dev,
            nat_a,
            nat_b,
            reflector,
        })
    }

    async fn build_single_nat_case(
        nat_mode: NatMode,
        wiring: UplinkWiring,
        port_base: u16,
    ) -> Result<(Lab, String, SocketAddr, SocketAddr, Ipv4Addr)> {
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").region("eu").build()?;
        let upstream = match wiring {
            UplinkWiring::DirectIx => None,
            UplinkWiring::ViaPublicIsp => {
                Some(lab.add_router("isp").region("eu").build()?)
            }
            UplinkWiring::ViaCgnatIsp => {
                Some(lab.add_router("isp").region("eu").nat(NatMode::Cgnat).build()?)
            }
        };
        let nat = { let mut rb = lab.add_router("nat").nat(nat_mode); if let Some(u) = upstream { rb = rb.upstream(u); } rb.build()? };
        let dev = lab.add_device("dev").iface("eth0", nat, None).build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r_dc = SocketAddr::new(IpAddr::V4(dc_ip), port_base);
        let r_ix = SocketAddr::new(IpAddr::V4(lab.ix_gw()), port_base + 1);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, r_dc)?;
        lab.spawn_reflector_on_ix(r_ix)?;
        tokio::time::sleep(Duration::from_millis(200)).await;

        let dev_ns = lab.node_ns(dev)?.to_string();
        let expected_ip = match (nat_mode, wiring) {
            (_, UplinkWiring::ViaCgnatIsp) => {
                let isp = lab.router_id("isp").context("missing isp")?;
                lab.router_uplink_ip(isp)?
            }
            (NatMode::None, _) => lab.device_ip(dev)?,
            _ => lab.router_uplink_ip(nat)?,
        };
        Ok((lab, dev_ns, r_dc, r_ix, expected_ip))
    }

    /// Spawns an async TCP echo server in `ns` that loops accepting connections,
    /// echoes each one's payload, and continues until the namespace is torn down.
    /// Returns when the listener is bound.
    async fn spawn_tcp_echo_server(ns: &str, bind: SocketAddr) -> Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let ns = ns.to_string();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<()>>();
        spawn_task_in_netns(&ns, move || async move {
            match tokio::net::TcpListener::bind(bind).await {
                Ok(listener) => {
                    let _ = ready_tx.send(Ok(()));
                    loop {
                        let Ok((mut stream, _)) = listener.accept().await else { break };
                        let mut buf = [0u8; 64];
                        if let Ok(n) = stream.read(&mut buf).await {
                            let _ = stream.write_all(&buf[..n]).await;
                        }
                    }
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(anyhow!("tcp echo bind {bind}: {e}")));
                }
            }
        });
        ready_rx
            .await
            .map_err(|_| anyhow!("tcp echo server task dropped before ready"))?
    }

    /// Spawns an async TCP echo server in `ns` that accepts one connection, echoes bytes, then stops.
    /// Returns when the listener is bound. The task runs on the namespace's async worker.
    async fn spawn_tcp_echo_in(ns: &str, bind: SocketAddr) -> Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let ns = ns.to_string();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<()>>();
        spawn_task_in_netns(&ns, move || async move {
            match tokio::net::TcpListener::bind(bind).await {
                Ok(listener) => {
                    let _ = ready_tx.send(Ok(()));
                    if let Ok((mut stream, _)) = listener.accept().await {
                        let mut buf = [0u8; 64];
                        if let Ok(n) = stream.read(&mut buf).await {
                            let _ = stream.write_all(&buf[..n]).await;
                        }
                    }
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(anyhow!("tcp echo bind {bind}: {e}")));
                }
            }
        });
        ready_rx
            .await
            .map_err(|_| anyhow!("tcp echo task dropped before ready"))?
    }

    async fn tcp_roundtrip_in_ns(ns: &str, target: SocketAddr) -> Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let ns = ns.to_string();
        let timeout = Duration::from_millis(500);
        spawn_task_in_netns(&ns, move || async move {
            let mut stream =
                tokio::time::timeout(timeout, tokio::net::TcpStream::connect(target))
                    .await
                    .context("tcp connect timeout")?
                    .context("tcp connect")?;
            let payload = b"ping";
            tokio::time::timeout(timeout, stream.write_all(payload))
                .await
                .context("tcp write timeout")?
                .context("tcp write")?;
            let mut buf = [0u8; 4];
            tokio::time::timeout(timeout, stream.read_exact(&mut buf))
                .await
                .context("tcp read timeout")?
                .context("tcp read")?;
            if &buf != payload {
                bail!("tcp echo mismatch: {:?}", buf);
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .map_err(|_| anyhow!("tcp_roundtrip: netns task cancelled"))?
    }

    fn current_netns_inode() -> Result<String> {
        let link = std::fs::read_link("/proc/self/ns/net").context("read host netns inode")?;
        Ok(link.to_string_lossy().to_string())
    }

    fn netns_inode(ns: &str) -> Result<String> {
        let ns = ns.to_string();
        let ns_for_msg = ns.clone();
        run_closure_in_namespace(&ns, move || {
            let link = std::fs::read_link("/proc/thread-self/ns/net")
                .or_else(|_| std::fs::read_link("/proc/self/ns/net"))
                .with_context(|| format!("read netns inode in '{ns_for_msg}'"))?;
            Ok(link.to_string_lossy().to_string())
        })
    }

    fn run_cmd_output_in_ns(
        ns: &str,
        program: &str,
        args: &[&str],
    ) -> Result<std::process::Output> {
        let ns = ns.to_string();
        let ns_for_msg = ns.clone();
        let program = program.to_string();
        let args: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        run_closure_in_namespace(&ns, move || {
            let mut cmd = std::process::Command::new(&program);
            cmd.args(&args);
            cmd.output()
                .with_context(|| format!("run '{program} {}' in ns '{ns_for_msg}'", args.join(" ")))
        })
    }

    fn dump_ns_state(ns: &str, phase: &str) {
        eprintln!("diag[{phase}] ns={ns}");
        match netns_inode(ns) {
            Ok(ino) => eprintln!("diag[{phase}] ns={ns} inode={ino}"),
            Err(err) => eprintln!("diag[{phase}] ns={ns} inode_error={err:#}"),
        }
        for (label, args) in [
            ("links", vec!["-o", "link", "show"]),
            ("addrs", vec!["-4", "addr", "show"]),
            ("routes", vec!["-4", "route", "show"]),
        ] {
            match run_cmd_output_in_ns(ns, "ip", &args) {
                Ok(out) => {
                    eprintln!("diag[{phase}] ns={ns} {label} status={}", out.status);
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    if !stdout.trim().is_empty() {
                        eprintln!("diag[{phase}] ns={ns} {label} stdout:\n{stdout}");
                    }
                    if !stderr.trim().is_empty() {
                        eprintln!("diag[{phase}] ns={ns} {label} stderr:\n{stderr}");
                    }
                }
                Err(err) => eprintln!("diag[{phase}] ns={ns} {label} error={err:#}"),
            }
        }
    }

    // ── Builder-API NAT tests ────────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn smoke_debug_netns_exit_trace() -> Result<()> {
        check_caps()?;
        let host_inode_before = current_netns_inode()?;
        debug!(host_inode_before = %host_inode_before, "diag: host inode before build");

        let mut lab = Lab::new();
        let isp = lab.add_router("isp1").region("eu").build()?;
        let home = lab.add_router("home1").upstream(isp).nat(NatMode::DestinationIndependent).build()?;
        lab.add_device("dev1").iface("eth0", home, None).build()?;

        let ns_plan = lab.core.all_ns_names();
        eprintln!("diag[pre-build] host_inode={}", current_netns_inode()?);
        for ns in &ns_plan {
            dump_ns_state(ns, "pre-build");
        }

        if let Err(err) = lab.build().await {
            eprintln!("diag[build-error] host_inode={}", current_netns_inode()?);
            eprintln!("diag[build-error] build_err={err:#}");
            for ns in &ns_plan {
                dump_ns_state(ns, "build-error");
            }
            return Err(err).context("smoke_debug_netns_exit_trace build failed");
        }

        let ns_after = lab.core.all_ns_names();
        eprintln!("diag[post-build] host_inode={}", current_netns_inode()?);
        for ns in &ns_after {
            dump_ns_state(ns, "post-build");
        }

        let dev_id = lab.device_id("dev1").context("missing dev1")?;
        let dev_ns = lab.node_ns(dev_id)?.to_string();
        let lan_gw = lab.router_downlink_gw(home)?;
        ping_in_ns(&dev_ns, &lan_gw.to_string())?;

        let host_inode_after = current_netns_inode()?;
        debug!(host_inode_after = %host_inode_after, "diag: host inode after smoke");
        eprintln!("diag[done] host_inode_after={host_inode_after}");
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn nat_dest_independent_keeps_port() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        let isp = lab.add_router("isp1").region("eu").build()?;
        let dc = lab.add_router("dc1").region("eu").build()?;
        let home = lab.add_router("home1").upstream(isp).nat(NatMode::DestinationIndependent).build()?;
        lab.add_device("dev1").iface("eth0", home, None).build()?;
        lab.build().await?;

        // Reflector in DC namespace.
        let dc_ip = lab.router_uplink_ip(dc)?;
        let r1 = SocketAddr::new(IpAddr::V4(dc_ip), 3478);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, r1)?;

        // Reflector on IX bridge (lab-root ns).
        let r2 = SocketAddr::new(IpAddr::V4(lab.ix_gw()), 3479);
        lab.spawn_reflector_on_ix(r2)?;

        tokio::time::sleep(Duration::from_millis(250)).await;

        let o1 = lab.probe_udp_mapping("dev1", r1)?;
        let o2 = lab.probe_udp_mapping("dev1", r2)?;

        assert_eq!(o1.observed.ip(), o2.observed.ip(), "external IP differs");
        assert_eq!(
            o1.observed.port(),
            o2.observed.port(),
            "EIM: external port must be stable across destinations",
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn nat_dest_dependent_changes_port() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        let isp = lab.add_router("isp1").region("eu").build()?;
        let dc = lab.add_router("dc1").region("eu").build()?;
        let home = lab.add_router("home1").upstream(isp).nat(NatMode::DestinationDependent).build()?;
        lab.add_device("dev1").iface("eth0", home, None).build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r1 = SocketAddr::new(IpAddr::V4(dc_ip), 4478);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, r1)?;

        let r2 = SocketAddr::new(IpAddr::V4(lab.ix_gw()), 4479);
        lab.spawn_reflector_on_ix(r2)?;

        tokio::time::sleep(Duration::from_millis(250)).await;

        let o1 = lab.probe_udp_mapping("dev1", r1)?;
        let o2 = lab.probe_udp_mapping("dev1", r2)?;
        println!("o1 {o1:?}");
        println!("o2 {o2:?}");

        assert_eq!(o1.observed.ip(), o2.observed.ip(), "external IP differs");
        assert_ne!(
            o1.observed.port(),
            o2.observed.port(),
            "EDM: external port must change per destination",
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn cgnat_hides_behind_isp_public_ip() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        let isp = lab.add_router("isp1").region("eu").nat(NatMode::Cgnat).build()?;
        let dc = lab.add_router("dc1").region("eu").build()?;
        let home = lab.add_router("home1").upstream(isp).nat(NatMode::DestinationIndependent).build()?;
        lab.add_device("dev1").iface("eth0", home, None).build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r = SocketAddr::new(IpAddr::V4(dc_ip), 5478);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, r)?;

        tokio::time::sleep(Duration::from_millis(250)).await;

        let o = lab.probe_udp_mapping("dev1", r)?;
        let isp_public = IpAddr::V4(lab.router_uplink_ip(isp)?);

        assert_eq!(
            o.observed.ip(),
            isp_public,
            "with CGNAT the observed IP must be the ISP's IX IP",
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn iroh_nat_like_nodes_report_public_203_mapped_addrs() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").region("eu").build()?;
        let isp = lab.add_router("isp").region("eu").nat(NatMode::Cgnat).build()?;
        let lan_provider = lab.add_router("lan-provider").upstream(isp).nat(NatMode::DestinationIndependent).build()?;
        let lan_fetcher = lab.add_router("lan-fetcher").upstream(isp).nat(NatMode::DestinationIndependent).build()?;
        lab.add_device("provider")
            .iface("eth0", lan_provider, None)
            .build()?;
        lab.add_device("fetcher")
            .iface("eth0", lan_fetcher, None)
            .build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 6478);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, reflector)?;
        tokio::time::sleep(Duration::from_millis(250)).await;

        let provider_obs = lab.probe_udp_mapping("provider", reflector)?;
        let fetcher_obs = lab.probe_udp_mapping("fetcher", reflector)?;
        let isp_public = lab.router_uplink_ip(isp)?;

        let provider_ip = match provider_obs.observed.ip() {
            IpAddr::V4(ip) => ip,
            IpAddr::V6(ip) => bail!("expected provider observed IPv4 address, got {ip}"),
        };
        let fetcher_ip = match fetcher_obs.observed.ip() {
            IpAddr::V4(ip) => ip,
            IpAddr::V6(ip) => bail!("expected fetcher observed IPv4 address, got {ip}"),
        };

        assert_eq!(
            provider_ip.octets()[0],
            203,
            "provider STUN report should be public 203.* mapped IP, got {}",
            provider_obs.observed
        );
        assert_eq!(
            fetcher_ip.octets()[0],
            203,
            "fetcher STUN report should be public 203.* mapped IP, got {}",
            fetcher_obs.observed
        );
        assert_eq!(
            provider_ip, isp_public,
            "provider should be mapped behind ISP public address"
        );
        assert_eq!(
            fetcher_ip, isp_public,
            "fetcher should be mapped behind ISP public address"
        );
        assert_ne!(
            provider_obs.observed.port(),
            0,
            "provider mapped port should be non-zero"
        );
        assert_ne!(
            fetcher_obs.observed.port(),
            0,
            "fetcher mapped port should be non-zero"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn load_from_toml() -> Result<()> {
        check_caps()?;
        let toml = r#"
[[router]]
name   = "isp1"
region = "eu"

[[router]]
name   = "dc1"
region = "eu"

[[router]]
name     = "lan1"
upstream = "isp1"
nat      = "destination-independent"

[device.dev1.eth0]
gateway = "lan1"
"#;
        let tmp = std::env::temp_dir().join("netsim_test_lab.toml");
        std::fs::write(&tmp, toml)?;

        let lab = Lab::load(&tmp).await?;
        assert!(lab.device_id("dev1").is_some());
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn smoke_ping_gateway() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        let isp = lab.add_router("isp1").region("eu").build()?;
        let home = lab.add_router("home1").upstream(isp).nat(NatMode::DestinationIndependent).build()?;
        lab.add_device("dev1").iface("eth0", home, None).build()?;
        lab.build().await?;

        let dev_id = lab.device_id("dev1").expect("dev1 exists");
        let dev_ns = lab.node_ns(dev_id)?.to_string();
        let lan_gw = lab.router_downlink_gw(home)?;
        ping_in_ns(&dev_ns, &lan_gw.to_string())?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn smoke_udp_dc_roundtrip() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        let isp = lab.add_router("isp1").region("eu").build()?;
        let dc = lab.add_router("dc1").region("eu").build()?;
        let home = lab.add_router("home1").upstream(isp).nat(NatMode::DestinationIndependent).build()?;
        lab.add_device("dev1").iface("eth0", home, None).build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r = SocketAddr::new(IpAddr::V4(dc_ip), 3478);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, r)?;

        tokio::time::sleep(Duration::from_millis(250)).await;

        let dev_id = lab.device_id("dev1").expect("dev1 exists");
        let dev_ns = lab.node_ns(dev_id)?.to_string();
        let _ = udp_roundtrip_in_ns(&dev_ns, r)?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn smoke_tcp_dc_roundtrip() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        let isp = lab.add_router("isp1").region("eu").build()?;
        let dc = lab.add_router("dc1").region("eu").build()?;
        let home = lab.add_router("home1").upstream(isp).nat(NatMode::DestinationIndependent).build()?;
        lab.add_device("dev1").iface("eth0", home, None).build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let bind = SocketAddr::new(IpAddr::V4(dc_ip), 9000);
        let dc_ns = lab.node_ns(dc)?.to_string();
        spawn_tcp_echo_in(&dc_ns, bind).await?;

        tokio::time::sleep(Duration::from_millis(250)).await;

        let dev_id = lab.device_id("dev1").expect("dev1 exists");
        let dev_ns = lab.node_ns(dev_id)?.to_string();
        tcp_roundtrip_in_ns(&dev_ns, bind).await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn smoke_ping_home_to_isp() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        let isp = lab.add_router("isp1").region("eu").build()?;
        let home = lab.add_router("home1").upstream(isp).nat(NatMode::DestinationIndependent).build()?;
        lab.build().await?;

        let home_ns = lab.node_ns(home)?.to_string();
        let isp_wan_ip = lab.router_downlink_gw(isp)?;
        ping_in_ns(&home_ns, &isp_wan_ip.to_string())?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn smoke_ping_isp_to_ix_and_dc() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        let isp = lab.add_router("isp1").region("eu").build()?;
        let dc = lab.add_router("dc1").region("eu").build()?;
        lab.build().await?;

        let isp_ns = lab.node_ns(isp)?.to_string();
        ping_in_ns(&isp_ns, &lab.ix_gw().to_string())?;
        let dc_ip = lab.router_uplink_ip(dc)?;
        ping_in_ns(&isp_ns, &dc_ip.to_string())?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn smoke_nat_homes_can_ping_public_relay_device() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();

        let dc = lab.add_router("dc").build()?;
        let lan_provider =
            lab.add_router("lan-provider").nat(NatMode::DestinationIndependent).build()?;
        let lan_fetcher =
            lab.add_router("lan-fetcher").nat(NatMode::DestinationIndependent).build()?;

        let relay = lab.add_device("relay").iface("eth0", dc, None).build()?;
        let provider = lab
            .add_device("provider")
            .iface("eth0", lan_provider, None)
            .build()?;
        let fetcher = lab
            .add_device("fetcher")
            .iface("eth0", lan_fetcher, None)
            .build()?;

        lab.build().await?;

        let relay_ip = lab.device_ip(relay)?;
        let provider_ns = lab.node_ns(provider)?.to_string();
        let fetcher_ns = lab.node_ns(fetcher)?.to_string();

        ping_in_ns(&provider_ns, &relay_ip.to_string())?;
        ping_in_ns(&fetcher_ns, &relay_ip.to_string())?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn nat_matrix_public_connectivity_and_reflexive_ip() -> Result<()> {
        check_caps()?;
        let cases = [
            (NatMode::None, UplinkWiring::DirectIx),
            (NatMode::Cgnat, UplinkWiring::DirectIx),
            (NatMode::DestinationIndependent, UplinkWiring::DirectIx),
            (NatMode::DestinationIndependent, UplinkWiring::ViaPublicIsp),
            (NatMode::DestinationIndependent, UplinkWiring::ViaCgnatIsp),
            (NatMode::DestinationDependent, UplinkWiring::DirectIx),
            (NatMode::DestinationDependent, UplinkWiring::ViaPublicIsp),
            (NatMode::DestinationDependent, UplinkWiring::ViaCgnatIsp),
        ];

        let mut case_idx = 0u16;
        for (mode, wiring) in cases {
            let port_base = 10000 + case_idx * 10;
            case_idx = case_idx.saturating_add(1);
            let (lab, dev_ns, r_dc, _r_ix, expected_ip) =
                build_single_nat_case(mode, wiring, port_base).await?;

            ping_in_ns(&dev_ns, &r_dc.ip().to_string())?;
            let _ = udp_roundtrip_in_ns(&dev_ns, r_dc)?;
            let observed = lab.probe_udp_mapping("dev", r_dc)?;
            assert_eq!(
                observed.observed.ip(),
                IpAddr::V4(expected_ip),
                "unexpected reflexive IP for mode={mode:?} wiring={}",
                wiring.label()
            );
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn nat_mapping_port_behavior_by_mode_and_wiring() -> Result<()> {
        check_caps()?;
        let modes = [
            NatMode::DestinationIndependent,
            NatMode::DestinationDependent,
        ];
        let wirings = [
            UplinkWiring::DirectIx,
            UplinkWiring::ViaPublicIsp,
            UplinkWiring::ViaCgnatIsp,
        ];

        let mut case_idx = 0u16;
        for mode in modes {
            for wiring in wirings {
                let port_base = 11000 + case_idx * 10;
                case_idx = case_idx.saturating_add(1);
                let (lab, _dev_ns, r_dc, r_ix, expected_ip) =
                    build_single_nat_case(mode, wiring, port_base).await?;
                let o1 = lab.probe_udp_mapping("dev", r_dc)?;
                let o2 = lab.probe_udp_mapping("dev", r_ix)?;

                assert_eq!(
                    o1.observed.ip(),
                    IpAddr::V4(expected_ip),
                    "unexpected reflexive IP for mode={mode:?} wiring={}",
                    wiring.label()
                );
                assert_eq!(
                    o2.observed.ip(),
                    IpAddr::V4(expected_ip),
                    "unexpected reflexive IP for mode={mode:?} wiring={}",
                    wiring.label()
                );

                match mode {
                    NatMode::DestinationIndependent => assert_eq!(
                        o1.observed.port(),
                        o2.observed.port(),
                        "expected stable external port for mode={mode:?} wiring={}",
                        wiring.label()
                    ),
                    NatMode::DestinationDependent => assert_ne!(
                        o1.observed.port(),
                        o2.observed.port(),
                        "expected destination-dependent external port for mode={mode:?} wiring={}",
                        wiring.label()
                    ),
                    _ => unreachable!("only destination modes are tested here"),
                }
            }
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn nat_private_reachability_isolated_public_reachable() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").region("eu").build()?;
        let nat_a = lab.add_router("nat-a").nat(NatMode::DestinationIndependent).build()?;
        let nat_b = lab.add_router("nat-b").nat(NatMode::DestinationIndependent).build()?;

        let relay = lab.add_device("relay").iface("eth0", dc, None).build()?;
        let a1 = lab.add_device("a1").iface("eth0", nat_a, None).build()?;
        let a2 = lab.add_device("a2").iface("eth0", nat_a, None).build()?;
        let b1 = lab.add_device("b1").iface("eth0", nat_b, None).build()?;
        lab.build().await?;

        let a1_ns = lab.node_ns(a1)?.to_string();
        let b1_ns = lab.node_ns(b1)?.to_string();
        let a2_ip = lab.device_ip(a2)?;
        let b1_ip = lab.device_ip(b1)?;
        let a1_ip = lab.device_ip(a1)?;
        let relay_ip = lab.device_ip(relay)?;

        ping_in_ns(&a1_ns, &a2_ip.to_string())?;
        ping_fails_in_ns(&a1_ns, &b1_ip.to_string())?;
        ping_fails_in_ns(&b1_ns, &a1_ip.to_string())?;

        ping_in_ns(&a1_ns, &relay_ip.to_string())?;
        ping_in_ns(&b1_ns, &relay_ip.to_string())?;

        let nat_a_public = lab.router_uplink_ip(nat_a)?;
        let nat_b_public = lab.router_uplink_ip(nat_b)?;
        ping_in_ns(&a1_ns, &nat_b_public.to_string())?;
        ping_in_ns(&b1_ns, &nat_a_public.to_string())?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 12000);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, reflector)?;
        tokio::time::sleep(Duration::from_millis(200)).await;

        let a1_map = lab.probe_udp_mapping("a1", reflector)?;
        let a2_map = lab.probe_udp_mapping("a2", reflector)?;
        let b1_map = lab.probe_udp_mapping("b1", reflector)?;
        assert_eq!(a1_map.observed.ip(), IpAddr::V4(nat_a_public));
        assert_eq!(a2_map.observed.ip(), IpAddr::V4(nat_a_public));
        assert_eq!(b1_map.observed.ip(), IpAddr::V4(nat_b_public));
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn smoke_device_to_device_same_lan() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        let isp = lab.add_router("isp1").region("eu").build()?;
        let home = lab.add_router("home1").upstream(isp).nat(NatMode::DestinationIndependent).build()?;
        let dev1 = lab.add_device("dev1").iface("eth0", home, None).build()?;
        let dev2 = lab.add_device("dev2").iface("eth0", home, None).build()?;
        lab.build().await?;

        let dev1_ns = lab.node_ns(dev1)?.to_string();
        let dev2_ip = lab.device_ip(dev2)?;
        ping_in_ns(&dev1_ns, &dev2_ip.to_string())?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn latency_directional_between_regions() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        lab.add_region_latency("eu", "us", 30);
        lab.add_region_latency("us", "eu", 70);
        let dc_eu = lab.add_router("dc-eu").region("eu").build()?;
        let dc_us = lab.add_router("dc-us").region("us").build()?;
        let dev_eu = lab
            .add_device("dev-eu")
            .iface("eth0", dc_eu, None)
            .build()?;
        let dev_us = lab
            .add_device("dev-us")
            .iface("eth0", dc_us, None)
            .build()?;
        lab.build().await?;

        let dc_us_ip = lab.router_uplink_ip(dc_us)?;
        let r_us = SocketAddr::new(IpAddr::V4(dc_us_ip), 9010);
        let dc_us_ns = lab.node_ns(dc_us)?.to_string();
        lab.spawn_reflector(&dc_us_ns, r_us)?;

        let dc_eu_ip = lab.router_uplink_ip(dc_eu)?;
        let r_eu = SocketAddr::new(IpAddr::V4(dc_eu_ip), 9011);
        let dc_eu_ns = lab.node_ns(dc_eu)?.to_string();
        lab.spawn_reflector(&dc_eu_ns, r_eu)?;

        tokio::time::sleep(Duration::from_millis(250)).await;

        let dev_eu_ns = lab.node_ns(dev_eu)?.to_string();
        let dev_us_ns = lab.node_ns(dev_us)?.to_string();
        let rtt_eu_to_us = udp_rtt_in_ns(&dev_eu_ns, r_us)?;
        let rtt_us_to_eu = udp_rtt_in_ns(&dev_us_ns, r_eu)?;
        let expected = Duration::from_millis(100);

        assert!(
            rtt_eu_to_us >= expected - Duration::from_millis(10),
            "expected eu->us RTT >= 90ms, got {rtt_eu_to_us:?}"
        );
        assert!(
            rtt_us_to_eu >= expected - Duration::from_millis(10),
            "expected us->eu RTT >= 90ms, got {rtt_us_to_eu:?}"
        );
        let diff = rtt_eu_to_us.abs_diff(rtt_us_to_eu);
        assert!(
            diff <= Duration::from_millis(20),
            "expected RTTs to be close; eu->us={rtt_eu_to_us:?} us->eu={rtt_us_to_eu:?}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn latency_inter_region_dc_to_dc() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        lab.add_region_latency("eu", "us", 50);
        lab.add_region_latency("us", "eu", 50);
        let dc_eu = lab.add_router("dc-eu").region("eu").build()?;
        let dc_us = lab.add_router("dc-us").region("us").build()?;
        lab.add_device("dev1").iface("eth0", dc_eu, None).build()?;
        lab.build().await?;

        let dc_us_ip = lab.router_uplink_ip(dc_us)?;
        let r = SocketAddr::new(IpAddr::V4(dc_us_ip), 9000);
        let dc_us_ns = lab.node_ns(dc_us)?.to_string();
        lab.spawn_reflector(&dc_us_ns, r)?;
        tokio::time::sleep(Duration::from_millis(250)).await;

        let dev_id = lab.device_id("dev1").context("missing dev1")?;
        let dev_ns = lab.node_ns(dev_id)?.to_string();
        let rtt = udp_rtt_in_ns(&dev_ns, r)?;
        assert!(
            rtt >= Duration::from_millis(90),
            "expected inter-region RTT >= 90ms, got {rtt:?}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn latency_device_impair_adds_delay() -> Result<()> {
        check_caps()?;

        async fn measure(impair: Option<Impair>) -> Result<Duration> {
            let mut lab = Lab::new();
            lab.add_region_latency("eu", "us", 40);
            lab.add_region_latency("us", "eu", 40);
            let dc_eu = lab.add_router("dc-eu").region("eu").build()?;
            let dc_us = lab.add_router("dc-us").region("us").build()?;
            lab.add_device("dev1")
                .iface("eth0", dc_eu, impair)
                .build()?;
            lab.build().await?;

            let dc_us_ip = lab.router_uplink_ip(dc_us)?;
            let r = SocketAddr::new(IpAddr::V4(dc_us_ip), 9001);
            let dc_us_ns = lab.node_ns(dc_us)?.to_string();
            lab.spawn_reflector(&dc_us_ns, r)?;
            tokio::time::sleep(Duration::from_millis(250)).await;

            let dev_id = lab.device_id("dev1").context("missing dev1")?;
            let dev_ns = lab.node_ns(dev_id)?.to_string();
            udp_rtt_in_ns(&dev_ns, r)
        }

        let base = measure(None).await?;
        let impaired = measure(Some(Impair::Mobile)).await?;
        assert!(
            impaired >= base + Duration::from_millis(30),
            "expected impaired RTT >= base + 30ms, base={base:?} impaired={impaired:?}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn latency_manual_impair_applies() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        let dc_eu = lab.add_router("dc-eu").region("eu").build()?;
        let dc_us = lab.add_router("dc-us").region("us").build()?;
        lab.add_region_latency("eu", "us", 20);
        lab.add_region_latency("us", "eu", 20);
        let dev = lab
            .add_device("dev1")
            .iface(
                "eth0",
                dc_eu,
                Some(Impair::Manual {
                    rate: 10_000,
                    loss: 0.0,
                    latency: 60,
                }),
            )
            .build()?;
        lab.build().await?;

        let dc_us_ip = lab.router_uplink_ip(dc_us)?;
        let r = SocketAddr::new(IpAddr::V4(dc_us_ip), 9020);
        let dc_us_ns = lab.node_ns(dc_us)?.to_string();
        lab.spawn_reflector(&dc_us_ns, r)?;
        tokio::time::sleep(Duration::from_millis(250)).await;

        let dev_ns = lab.node_ns(dev)?.to_string();
        let rtt = udp_rtt_in_ns(&dev_ns, r)?;
        assert!(
            rtt >= Duration::from_millis(90),
            "expected manual latency >= 90ms RTT, got {rtt:?}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn isp_home_wan_pool_selection() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        let isp_public = lab.add_router("isp-public").region("eu").build()?;
        let isp_cgnat = lab.add_router("isp-cgnat").region("eu").nat(NatMode::Cgnat).build()?;
        let home_public = lab.add_router("home-public").upstream(isp_public).nat(NatMode::DestinationIndependent).build()?;
        let home_cgnat = lab.add_router("home-cgnat").upstream(isp_cgnat).nat(NatMode::DestinationIndependent).build()?;
        lab.build().await?;

        let wan_public = lab.router_uplink_ip(home_public)?;
        let wan_cgnat = lab.router_uplink_ip(home_cgnat)?;

        let is_private_10 = |ip: Ipv4Addr| ip.octets()[0] == 10;
        assert!(
            !is_private_10(wan_public),
            "expected public WAN for non-CGNAT home, got {wan_public}"
        );
        assert!(
            is_private_10(wan_cgnat),
            "expected private WAN for CGNAT home, got {wan_cgnat}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn dynamic_set_impair_changes_rtt() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        let dc = lab.add_router("dc1").region("eu").build()?;
        let dev = lab.add_device("dev1").iface("eth0", dc, None).build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r = SocketAddr::new(IpAddr::V4(dc_ip), 9100);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, r)?;
        tokio::time::sleep(Duration::from_millis(250)).await;

        let dev_ns = lab.node_ns(dev)?.to_string();
        let base_rtt = udp_rtt_in_ns(&dev_ns, r)?;

        lab.set_impair("dev1", None, Some(Impair::Mobile))?;
        let impaired_rtt = udp_rtt_in_ns(&dev_ns, r)?;
        assert!(
            impaired_rtt >= base_rtt + Duration::from_millis(40),
            "expected impaired RTT >= base + 40ms, base={base_rtt:?} impaired={impaired_rtt:?}"
        );

        lab.set_impair("dev1", None, None)?;
        let recovered_rtt = udp_rtt_in_ns(&dev_ns, r)?;
        assert!(
            recovered_rtt < base_rtt + Duration::from_millis(30),
            "expected recovered RTT close to base, base={base_rtt:?} recovered={recovered_rtt:?}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn dynamic_link_down_up_connectivity() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        let dc = lab.add_router("dc1").region("eu").build()?;
        let dev = lab.add_device("dev1").iface("eth0", dc, None).build()?;
        lab.build().await?;

        let gw = lab.router_downlink_gw(dc)?;
        let dev_ns = lab.node_ns(dev)?.to_string();

        ping_in_ns(&dev_ns, &gw.to_string())?;

        lab.link_down("dev1", "eth0").await?;
        let result = ping_in_ns(&dev_ns, &gw.to_string());
        assert!(result.is_err(), "expected ping to fail after link_down");

        lab.link_up("dev1", "eth0").await?;
        ping_in_ns(&dev_ns, &gw.to_string())?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn dynamic_switch_route_changes_path() -> Result<()> {
        check_caps()?;
        let mut lab = Lab::new();
        let dc = lab.add_router("dc1").region("eu").build()?;
        let isp = lab.add_router("isp1").region("eu").build()?;
        let dev = lab
            .add_device("dev1")
            .iface("eth0", dc, None)
            .iface("eth1", isp, Some(Impair::Mobile))
            .default_via("eth0")
            .build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r = SocketAddr::new(IpAddr::V4(dc_ip), 9200);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, r)?;
        tokio::time::sleep(Duration::from_millis(250)).await;

        let dev_ns = lab.node_ns(dev)?.to_string();
        let fast_rtt = udp_rtt_in_ns(&dev_ns, r)?;

        lab.switch_route("dev1", "eth1").await?;
        let slow_rtt = udp_rtt_in_ns(&dev_ns, r)?;

        assert!(
            slow_rtt >= fast_rtt + Duration::from_millis(80),
            "expected slow RTT >= fast + 80ms, fast={fast_rtt:?} slow={slow_rtt:?}"
        );
        Ok(())
    }

    #[test]
    fn manual_impair_deserialize() -> Result<()> {
        let cfg = r#"
[[router]]
name = "dc1"
region = "eu"

[device.dev1.eth0]
gateway = "dc1"
impair = { rate = 5000, loss = 1.5, latency = 40 }
"#;
        let parsed: config::LabConfig = toml::from_str(cfg)?;
        let dev1 = parsed.device.get("dev1").context("missing dev1")?;
        let eth0 = dev1.get("eth0").context("missing eth0")?;
        let impair: Impair = eth0
            .get("impair")
            .context("missing impair")?
            .clone()
            .try_into()
            .map_err(|e: toml::de::Error| anyhow!("{}", e))?;
        match impair {
            Impair::Manual {
                rate,
                loss,
                latency,
            } => {
                assert_eq!(rate, 5000);
                assert!((loss - 1.5).abs() < f32::EPSILON);
                assert_eq!(latency, 40);
            }
            other => bail!("unexpected impair: {:?}", other),
        }
        Ok(())
    }

    #[test]
    fn from_config_expands_count_devices() -> Result<()> {
        let cfg = r#"
[[router]]
name = "dc1"

[device.fetcher]
count = 2
default_via = "eth0"

[device.fetcher.eth0]
gateway = "dc1"
"#;
        let parsed: config::LabConfig = toml::from_str(cfg)?;
        let lab = Lab::from_config(parsed)?;
        assert!(lab.device_id("fetcher-0").is_some());
        assert!(lab.device_id("fetcher-1").is_some());
        assert!(lab.device_id("fetcher").is_none());
        Ok(())
    }

    // ── 5a: TCP reflector smoke ──────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn tcp_reflector_basic() -> Result<()> {
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").build()?;
        let dev = lab.add_device("dev").iface("eth0", dc, None).build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r = SocketAddr::new(IpAddr::V4(dc_ip), 13_000);
        let dc_ns = lab.node_ns(dc)?.to_string();
        let dev_ns = lab.node_ns(dev)?.to_string();

        let (_stop, _join) = spawn_tcp_reflector(&dc_ns, r);
        tokio::time::sleep(Duration::from_millis(200)).await;

        let obs = probe_tcp(&dev_ns, r, "0.0.0.0:0".parse().unwrap()).await?;
        assert_ne!(obs.observed.port(), 0, "expected non-zero port");
        Ok(())
    }

    // ── 5b: Reflexive IP — full matrix ───────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn reflexive_ip_all_combos() -> Result<()> {
        use strum::IntoEnumIterator;

        // NatMode::None + Via*Isp is skipped: with no NAT the device gets a public
        // IP, but the nat router sits behind an ISP router (not directly on IX),
        // so no return route is installed from DC → device subnet.  DC's reply
        // is dropped and all probes time out.  The meaningful None case is
        // DirectIx where the return route IS set up.
        let combos: Vec<_> = NatMode::iter()
            .flat_map(|m| UplinkWiring::iter().map(move |w| (m, w)))
            .filter(|(m, w)| {
                !(*m == NatMode::None
                    && matches!(w, UplinkWiring::ViaPublicIsp | UplinkWiring::ViaCgnatIsp))
            })
            .flat_map(|(m, w)| Proto::iter().map(move |p| (m, w, p)))
            .flat_map(|(m, w, p)| BindMode::iter().map(move |b| (m, w, p, b)))
            .collect();

        let mut port_base = 14_000u16;
        let mut failures = Vec::new();
        for (mode, wiring, proto, bind) in combos {
            let result: Result<()> = async {
                let (_lab, ctx) = build_nat_case(mode, wiring, port_base).await?;
                let obs = probe_reflexive(proto, bind, &ctx).await?;
                if obs.observed.ip() != IpAddr::V4(ctx.expected_ip) {
                    bail!("expected {} got {}", ctx.expected_ip, obs.observed.ip());
                }
                Ok(())
            }
            .await;
            if let Err(e) = result {
                let label = format!("{mode}/{wiring}/{proto}/{bind}");
                eprintln!("FAIL {label}: {e:#}");
                failures.push(format!("{label}: {e:#}"));
            }
            port_base += 10;
        }
        if !failures.is_empty() {
            bail!("{} combos failed:\n{}", failures.len(), failures.join("\n"));
        }
        Ok(())
    }

    // ── 5c: Port mapping behavior ────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn port_mapping_eim_stable() -> Result<()> {
        use strum::IntoEnumIterator;
        let mut port_base = 16_000u16;
        let mut failures = Vec::new();
        for wiring in UplinkWiring::iter() {
            let result: Result<()> = async {
                let (lab, ctx) =
                    build_nat_case(NatMode::DestinationIndependent, wiring, port_base).await?;
                let o1 = lab.probe_udp_mapping("dev", ctx.r_dc)?;
                let o2 = lab.probe_udp_mapping("dev", ctx.r_ix)?;
                if o1.observed.port() != o2.observed.port() {
                    bail!(
                        "EIM: external port changed: r_dc={} r_ix={}",
                        o1.observed.port(),
                        o2.observed.port()
                    );
                }
                Ok(())
            }
            .await;
            if let Err(e) = result {
                failures.push(format!("DestIndep/{wiring}: {e:#}"));
            }
            port_base += 10;
        }
        if !failures.is_empty() {
            bail!("{} combos failed:\n{}", failures.len(), failures.join("\n"));
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn port_mapping_edm_changes() -> Result<()> {
        use strum::IntoEnumIterator;
        let mut port_base = 16_100u16;
        let mut failures = Vec::new();
        for wiring in UplinkWiring::iter() {
            let result: Result<()> = async {
                let (lab, ctx) =
                    build_nat_case(NatMode::DestinationDependent, wiring, port_base).await?;
                let o1 = lab.probe_udp_mapping("dev", ctx.r_dc)?;
                let o2 = lab.probe_udp_mapping("dev", ctx.r_ix)?;
                if o1.observed.port() == o2.observed.port() {
                    bail!(
                        "EDM: external port must change: r_dc={} r_ix={}",
                        o1.observed.port(),
                        o2.observed.port()
                    );
                }
                Ok(())
            }
            .await;
            if let Err(e) = result {
                failures.push(format!("DestDep/{wiring}: {e:#}"));
            }
            port_base += 10;
        }
        if !failures.is_empty() {
            bail!("{} combos failed:\n{}", failures.len(), failures.join("\n"));
        }
        Ok(())
    }

    // ── 5d: Route switching + reflexive IP ───────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn switch_route_reflexive_ip() -> Result<()> {
        use strum::IntoEnumIterator;
        let DualNatLab { mut lab, dev, nat_a, nat_b, reflector } =
            build_dual_nat_lab(NatMode::DestinationIndependent, NatMode::DestinationDependent, 16_200)
                .await?;

        let dev_ns = lab.node_ns(dev)?.to_string();
        let wan_a = lab.router_uplink_ip(nat_a)?;
        let wan_b = lab.router_uplink_ip(nat_b)?;

        let mut failures = Vec::new();
        for proto in Proto::iter() {
            for bind in BindMode::iter() {
                // SpecificIp must use the IP of the currently-active interface;
                // device_ip() returns the default_via interface IP, which changes on switch_route.
                let dev_ip = lab.device_ip(dev)?;
                let obs = probe_reflexive_addr(proto, bind, &dev_ns, dev_ip, reflector).await;
                match obs {
                    Ok(o) if o.observed.ip() == IpAddr::V4(wan_a) => {}
                    Ok(o) => failures.push(format!(
                        "{proto}/{bind} before switch: expected {wan_a} got {}",
                        o.observed.ip()
                    )),
                    Err(e) => failures.push(format!("{proto}/{bind} before switch: {e:#}")),
                }

                lab.switch_route("dev", "eth1").await?;
                tokio::time::sleep(Duration::from_millis(50)).await;

                let dev_ip = lab.device_ip(dev)?;
                let obs = probe_reflexive_addr(proto, bind, &dev_ns, dev_ip, reflector).await;
                match obs {
                    Ok(o) if o.observed.ip() == IpAddr::V4(wan_b) => {}
                    Ok(o) => failures.push(format!(
                        "{proto}/{bind} after switch: expected {wan_b} got {}",
                        o.observed.ip()
                    )),
                    Err(e) => failures.push(format!("{proto}/{bind} after switch: {e:#}")),
                }

                lab.switch_route("dev", "eth0").await?;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        if !failures.is_empty() {
            bail!("{} failures:\n{}", failures.len(), failures.join("\n"));
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn switch_route_multiple() -> Result<()> {
        let DualNatLab { mut lab, dev, nat_a, nat_b, reflector } =
            build_dual_nat_lab(NatMode::DestinationIndependent, NatMode::DestinationIndependent, 16_300)
                .await?;

        let dev_ns = lab.node_ns(dev)?.to_string();
        let wan_a = lab.router_uplink_ip(nat_a)?;
        let wan_b = lab.router_uplink_ip(nat_b)?;

        let o = udp_roundtrip_in_ns(&dev_ns, reflector)?;
        assert_eq!(o.observed.ip(), IpAddr::V4(wan_a), "expected nat_a WAN on eth0");

        lab.switch_route("dev", "eth1").await?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let o = udp_roundtrip_in_ns(&dev_ns, reflector)?;
        assert_eq!(o.observed.ip(), IpAddr::V4(wan_b), "expected nat_b WAN on eth1");

        lab.switch_route("dev", "eth0").await?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let o = udp_roundtrip_in_ns(&dev_ns, reflector)?;
        assert_eq!(o.observed.ip(), IpAddr::V4(wan_a), "expected nat_a WAN after switch back");

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn switch_route_tcp_roundtrip() -> Result<()> {
        let DualNatLab { mut lab, dev, nat_a: _, nat_b: _, reflector: _ } =
            build_dual_nat_lab(NatMode::DestinationIndependent, NatMode::DestinationDependent, 16_400)
                .await?;

        let dc = lab.router_id("dc").context("missing dc")?;
        let dc_ip = lab.router_uplink_ip(dc)?;
        let dc_ns = lab.node_ns(dc)?.to_string();
        let dev_ns = lab.node_ns(dev)?.to_string();

        let r = SocketAddr::new(IpAddr::V4(dc_ip), 16_410);
        spawn_tcp_echo_server(&dc_ns, r).await?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        tcp_roundtrip_in_ns(&dev_ns, r).await?;

        lab.switch_route("dev", "eth1").await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        tcp_roundtrip_in_ns(&dev_ns, r).await?;

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn switch_route_udp_reflexive_change() -> Result<()> {
        let DualNatLab { mut lab, dev, nat_a, nat_b, reflector } =
            build_dual_nat_lab(NatMode::DestinationIndependent, NatMode::DestinationIndependent, 16_500)
                .await?;

        let dev_ns = lab.node_ns(dev)?.to_string();
        let wan_a = lab.router_uplink_ip(nat_a)?;
        let wan_b = lab.router_uplink_ip(nat_b)?;

        let before = udp_roundtrip_in_ns(&dev_ns, reflector)?;
        assert_eq!(before.observed.ip(), IpAddr::V4(wan_a), "before switch: expected nat_a WAN");

        lab.switch_route("dev", "eth1").await?;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let after = udp_roundtrip_in_ns(&dev_ns, reflector)?;
        assert_eq!(after.observed.ip(), IpAddr::V4(wan_b), "after switch: expected nat_b WAN");
        assert_ne!(
            before.observed.ip(),
            after.observed.ip(),
            "reflexive IP must change after route switch"
        );
        Ok(())
    }

    // ── 5e: Link down/up ─────────────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn link_down_up_connectivity() -> Result<()> {
        use strum::IntoEnumIterator;
        let mut port_base = 16_600u16;
        let mut failures = Vec::new();
        for proto in Proto::iter() {
            let result: Result<()> = async {
                let mut lab = Lab::new();
                let dc = lab.add_router("dc").build()?;
                let dev = lab.add_device("dev").iface("eth0", dc, None).build()?;
                lab.build().await?;

                let dc_ip = lab.router_uplink_ip(dc)?;
                let r = SocketAddr::new(IpAddr::V4(dc_ip), port_base);
                let dc_ns = lab.node_ns(dc)?.to_string();
                let dev_ns = lab.node_ns(dev)?.to_string();
                let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);

                match proto {
                    Proto::Udp => {
                        lab.spawn_reflector(&dc_ns, r)?;
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        probe_udp(&dev_ns, r, bind).context("before link_down")?;
                        lab.link_down("dev", "eth0").await?;
                        if probe_udp(&dev_ns, r, bind).is_ok() {
                            bail!("probe should fail after link_down");
                        }
                        lab.link_up("dev", "eth0").await?;
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        probe_udp(&dev_ns, r, bind).context("after link_up")?;
                    }
                    Proto::Tcp => {
                        // Persistent echo server: handles all connections for the whole test.
                        spawn_tcp_echo_server(&dc_ns, r).await?;
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        tcp_roundtrip_in_ns(&dev_ns, r).await.context("before link_down")?;
                        lab.link_down("dev", "eth0").await?;
                        if tcp_roundtrip_in_ns(&dev_ns, r).await.is_ok() {
                            bail!("tcp should fail after link_down");
                        }
                        lab.link_up("dev", "eth0").await?;
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        tcp_roundtrip_in_ns(&dev_ns, r).await.context("after link_up")?;
                    }
                }
                Ok(())
            }
            .await;
            if let Err(e) = result {
                failures.push(format!("{proto}: {e:#}"));
            }
            port_base += 10;
        }
        if !failures.is_empty() {
            bail!("{} failures:\n{}", failures.len(), failures.join("\n"));
        }
        Ok(())
    }

    // ── 5f: NAT rebinding ────────────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn nat_rebind_mode_port() -> Result<()> {
        // DestIndep→DestDep: port changes; DestDep→DestIndep: port stabilises.
        let cases: &[(NatMode, NatMode, bool)] = &[
            (NatMode::DestinationIndependent, NatMode::DestinationDependent, false),
            (NatMode::DestinationDependent, NatMode::DestinationIndependent, true),
        ];
        let mut port_base = 16_800u16;
        let mut failures = Vec::new();
        for &(from, to, expect_stable) in cases {
            let result: Result<()> = async {
                let (mut lab, ctx) = build_nat_case(from, UplinkWiring::DirectIx, port_base).await?;
                let nat = lab.router_id("nat").context("missing nat")?;
                lab.set_nat_mode(nat, to).await?;
                tokio::time::sleep(Duration::from_millis(50)).await;
                let o1 = lab.probe_udp_mapping("dev", ctx.r_dc)?;
                let o2 = lab.probe_udp_mapping("dev", ctx.r_ix)?;
                let port_stable = o1.observed.port() == o2.observed.port();
                if port_stable != expect_stable {
                    bail!(
                        "{from}→{to}: expected stable={expect_stable} got stable={port_stable} \
                         (r_dc port={}, r_ix port={})",
                        o1.observed.port(),
                        o2.observed.port()
                    );
                }
                Ok(())
            }
            .await;
            if let Err(e) = result {
                failures.push(format!("{from}→{to}: {e:#}"));
            }
            port_base += 10;
        }
        if !failures.is_empty() {
            bail!("{} failures:\n{}", failures.len(), failures.join("\n"));
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn nat_rebind_mode_ip() -> Result<()> {
        // DestinationIndependent→None is omitted: with NAT=None, the device's
        // private IP appears as the packet source; the DC has no return route, so
        // the UDP probe times out rather than completing.
        let cases: &[(NatMode, NatMode)] = &[
            (NatMode::None, NatMode::DestinationIndependent),
        ];
        let mut port_base = 16_900u16;
        let mut failures = Vec::new();
        for &(from, to) in cases {
            let result: Result<()> = async {
                let (mut lab, ctx) =
                    build_nat_case(from, UplinkWiring::DirectIx, port_base).await?;
                let nat = lab.router_id("nat").context("missing nat")?;
                let wan_ip = lab.router_uplink_ip(nat)?;
                lab.set_nat_mode(nat, to).await?;
                tokio::time::sleep(Duration::from_millis(50)).await;
                let o = probe_udp(
                    &ctx.dev_ns,
                    ctx.r_dc,
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
                )?;
                let expected = match to {
                    NatMode::DestinationIndependent => IpAddr::V4(wan_ip),
                    NatMode::None => IpAddr::V4(ctx.dev_ip),
                    _ => unreachable!(),
                };
                if o.observed.ip() != expected {
                    bail!("{from}→{to}: expected {expected} got {}", o.observed.ip());
                }
                Ok(())
            }
            .await;
            if let Err(e) = result {
                failures.push(format!("{from}→{to}: {e:#}"));
            }
            port_base += 10;
        }
        if !failures.is_empty() {
            bail!("{} failures:\n{}", failures.len(), failures.join("\n"));
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn nat_rebind_conntrack_flush() -> Result<()> {
        // Skip if conntrack-tools is not installed.
        if std::process::Command::new("conntrack")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping nat_rebind_conntrack_flush: conntrack not found");
            return Ok(());
        }
        let (mut lab, ctx) = build_nat_case(
            NatMode::DestinationDependent,
            UplinkWiring::DirectIx,
            17_000,
        )
        .await?;
        let nat = lab.router_id("nat").context("missing nat")?;
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
        let o1 = probe_udp(&ctx.dev_ns, ctx.r_dc, bind)?;
        lab.rebind_nats(nat)?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let o2 = probe_udp(&ctx.dev_ns, ctx.r_dc, bind)?;
        assert_ne!(
            o1.observed.port(),
            o2.observed.port(),
            "expected new external port after conntrack flush"
        );
        Ok(())
    }

    // ── 5g: Multi-device cross-NAT ───────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn devices_same_nat_share_ip() -> Result<()> {
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").build()?;
        let nat = lab.add_router("nat").nat(NatMode::DestinationIndependent).build()?;
        let dev_a = lab.add_device("dev-a").iface("eth0", nat, None).build()?;
        let dev_b = lab.add_device("dev-b").iface("eth0", nat, None).build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r = SocketAddr::new(IpAddr::V4(dc_ip), 17_100);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, r)?;
        tokio::time::sleep(Duration::from_millis(200)).await;

        let ns_a = lab.node_ns(dev_a)?.to_string();
        let ns_b = lab.node_ns(dev_b)?.to_string();
        let oa = udp_roundtrip_in_ns(&ns_a, r)?;
        let ob = udp_roundtrip_in_ns(&ns_b, r)?;
        assert_eq!(
            oa.observed.ip(),
            ob.observed.ip(),
            "devices behind the same NAT must share the same external IP"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn devices_diff_nat_isolate() -> Result<()> {
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").build()?;
        let nat_a = lab.add_router("nat-a").nat(NatMode::DestinationIndependent).build()?;
        let nat_b = lab.add_router("nat-b").nat(NatMode::DestinationIndependent).build()?;
        let dev_a = lab.add_device("dev-a").iface("eth0", nat_a, None).build()?;
        let dev_b = lab.add_device("dev-b").iface("eth0", nat_b, None).build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r = SocketAddr::new(IpAddr::V4(dc_ip), 17_200);
        let dc_ns = lab.node_ns(dc)?.to_string();
        lab.spawn_reflector(&dc_ns, r)?;
        tokio::time::sleep(Duration::from_millis(200)).await;

        let ns_a = lab.node_ns(dev_a)?.to_string();
        let ns_b = lab.node_ns(dev_b)?.to_string();
        let ip_a = lab.device_ip(dev_a)?;
        let ip_b = lab.device_ip(dev_b)?;

        ping_fails_in_ns(&ns_a, &ip_b.to_string())?;
        ping_fails_in_ns(&ns_b, &ip_a.to_string())?;
        ping_in_ns(&ns_a, &dc_ip.to_string())?;
        ping_in_ns(&ns_b, &dc_ip.to_string())?;

        let oa = udp_roundtrip_in_ns(&ns_a, r)?;
        let ob = udp_roundtrip_in_ns(&ns_b, r)?;
        assert_ne!(
            oa.observed.ip(),
            ob.observed.ip(),
            "devices behind different NATs must have different external IPs"
        );
        Ok(())
    }

    // ── 5h: Hairpinning — TODO ───────────────────────────────────────────
    // Implementing ct-dnat-based hairpin in nftables requires per-port DNAT
    // rules derived from the live conntrack table. Deferred.

    // ── 5i: Rate limiting ────────────────────────────────────────────────

    fn join_sink(join: thread::JoinHandle<Result<()>>) -> Result<()> {
        join.join()
            .map_err(|_| anyhow!("tcp sink thread panicked"))?
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn rate_limit_tcp_upload() -> Result<()> {
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").build()?;
        let dev = lab
            .add_device("dev")
            .iface("eth0", dc, Some(Impair::Manual { rate: 2000, loss: 0.0, latency: 0 }))
            .build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let addr = SocketAddr::new(IpAddr::V4(dc_ip), 17_300);
        let dc_ns = lab.node_ns(dc)?.to_string();
        let dev_ns = lab.node_ns(dev)?.to_string();

        let sink = spawn_tcp_sink(&dc_ns, addr);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (_elapsed, kbps) = tcp_measure_throughput(&dev_ns, addr, 256 * 1024)?;
        join_sink(sink)?;

        assert!(kbps >= 1400, "expected ≥ 1400 kbit/s, got {kbps}");
        assert!(kbps <= 3000, "expected ≤ 3000 kbit/s, got {kbps}");
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn rate_limit_tcp_download() -> Result<()> {
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").build()?;
        let dev_id = lab.add_device("dev").iface("eth0", dc, None).build()?;
        lab.build().await?;

        let bridge = lab.router_downlink_bridge(dc)?;
        lab.set_router_impair(dc, &bridge, Some(Impair::Manual { rate: 2000, loss: 0.0, latency: 0 }))?;

        let dev_ip = lab.device_ip(dev_id)?;
        let addr = SocketAddr::new(IpAddr::V4(dev_ip), 17_400);
        let dev_ns = lab.node_ns(dev_id)?.to_string();
        let dc_ns = lab.node_ns(dc)?.to_string();

        // Client (DC) writes to server (device) — bytes travel the download path.
        let sink = spawn_tcp_sink(&dev_ns, addr);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (_elapsed, kbps) = tcp_measure_throughput(&dc_ns, addr, 256 * 1024)?;
        join_sink(sink)?;

        assert!(kbps >= 1400, "expected ≥ 1400 kbit/s, got {kbps}");
        assert!(kbps <= 3000, "expected ≤ 3000 kbit/s, got {kbps}");
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn rate_limit_udp_upload() -> Result<()> {
        use std::time::Instant;
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").build()?;
        let dev = lab
            .add_device("dev")
            .iface("eth0", dc, Some(Impair::Manual { rate: 2000, loss: 0.0, latency: 0 }))
            .build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r = SocketAddr::new(IpAddr::V4(dc_ip), 17_500);
        let dc_ns = lab.node_ns(dc)?.to_string();
        let dev_ns = lab.node_ns(dev)?.to_string();
        lab.spawn_reflector(&dc_ns, r)?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        // ~300 KB at 2 Mbit/s ≈ 1.2 s.
        let start = Instant::now();
        udp_send_recv_count(&dev_ns, r, 300, 1024, Duration::from_secs(5))?;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(1000),
            "expected ≥ 1.0 s for 300 KB at 2 Mbit/s, got {elapsed:?}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn rate_limit_udp_download() -> Result<()> {
        use std::time::Instant;
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").build()?;
        let dev_id = lab.add_device("dev").iface("eth0", dc, None).build()?;
        lab.build().await?;

        let bridge = lab.router_downlink_bridge(dc)?;
        lab.set_router_impair(dc, &bridge, Some(Impair::Manual { rate: 2000, loss: 0.0, latency: 0 }))?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r = SocketAddr::new(IpAddr::V4(dc_ip), 17_600);
        let dc_ns = lab.node_ns(dc)?.to_string();
        let dev_ns = lab.node_ns(dev_id)?.to_string();
        lab.spawn_reflector(&dc_ns, r)?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Replies travel the download path (DC → device) which is throttled.
        let start = Instant::now();
        udp_send_recv_count(&dev_ns, r, 300, 1024, Duration::from_secs(5))?;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(1000),
            "expected ≥ 1.0 s for 300 KB download at 2 Mbit/s, got {elapsed:?}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn rate_limit_asymmetric() -> Result<()> {
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").build()?;
        let dev_id = lab
            .add_device("dev")
            .iface("eth0", dc, Some(Impair::Manual { rate: 1000, loss: 0.0, latency: 0 }))
            .build()?;
        lab.build().await?;

        let bridge = lab.router_downlink_bridge(dc)?;
        lab.set_router_impair(dc, &bridge, Some(Impair::Manual { rate: 4000, loss: 0.0, latency: 0 }))?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let up_addr = SocketAddr::new(IpAddr::V4(dc_ip), 17_700);
        let dev_ip = lab.device_ip(dev_id)?;
        let down_addr = SocketAddr::new(IpAddr::V4(dev_ip), 17_710);
        let dc_ns = lab.node_ns(dc)?.to_string();
        let dev_ns = lab.node_ns(dev_id)?.to_string();

        let sink_up = spawn_tcp_sink(&dc_ns, up_addr);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (_e, kbps_up) = tcp_measure_throughput(&dev_ns, up_addr, 128 * 1024)?;
        join_sink(sink_up)?;

        let sink_down = spawn_tcp_sink(&dev_ns, down_addr);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (_e, kbps_down) = tcp_measure_throughput(&dc_ns, down_addr, 128 * 1024)?;
        join_sink(sink_down)?;

        assert!(kbps_up <= 1500, "expected upload ≤ 1500 kbit/s, got {kbps_up}");
        assert!(kbps_down >= 2000, "expected download ≥ 2000 kbit/s, got {kbps_down}");
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn rate_limit_multihop_bottleneck() -> Result<()> {
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").build()?;
        let isp = lab.add_router("isp").build()?;
        let nat = lab.add_router("nat").upstream(isp).nat(NatMode::DestinationIndependent).build()?;
        let dev = lab.add_device("dev").iface("eth0", nat, None).build()?;
        lab.build().await?;

        lab.set_router_impair(nat, "wan", Some(Impair::Manual { rate: 1000, loss: 0.0, latency: 0 }))?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let addr = SocketAddr::new(IpAddr::V4(dc_ip), 17_800);
        let dc_ns = lab.node_ns(dc)?.to_string();
        let dev_ns = lab.node_ns(dev)?.to_string();

        let sink = spawn_tcp_sink(&dc_ns, addr);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (_e, kbps) = tcp_measure_throughput(&dev_ns, addr, 128 * 1024)?;
        join_sink(sink)?;

        assert!(kbps <= 1500, "NAT WAN bottleneck: expected ≤ 1500 kbit/s, got {kbps}");
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn rate_limit_two_hops_stack() -> Result<()> {
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").build()?;
        let dev = lab
            .add_device("dev")
            .iface("eth0", dc, Some(Impair::Manual { rate: 2000, loss: 0.0, latency: 0 }))
            .build()?;
        lab.build().await?;

        let bridge = lab.router_downlink_bridge(dc)?;
        lab.set_router_impair(dc, &bridge, Some(Impair::Manual { rate: 2000, loss: 0.0, latency: 0 }))?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let addr = SocketAddr::new(IpAddr::V4(dc_ip), 17_900);
        let dc_ns = lab.node_ns(dc)?.to_string();
        let dev_ns = lab.node_ns(dev)?.to_string();

        let sink = spawn_tcp_sink(&dc_ns, addr);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (_e, kbps) = tcp_measure_throughput(&dev_ns, addr, 256 * 1024)?;
        join_sink(sink)?;

        // Both hops at 2 Mbit/s → effective rate ≤ 2 Mbit/s.
        assert!(kbps <= 3000, "expected ≤ 3000 kbit/s, got {kbps}");
        Ok(())
    }

    // ── 5j: Packet loss ──────────────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn loss_udp_moderate() -> Result<()> {
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").build()?;
        let dev = lab
            .add_device("dev")
            .iface("eth0", dc, Some(Impair::Manual { rate: 0, loss: 50.0, latency: 0 }))
            .build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r = SocketAddr::new(IpAddr::V4(dc_ip), 18_000);
        let dc_ns = lab.node_ns(dc)?.to_string();
        let dev_ns = lab.node_ns(dev)?.to_string();
        lab.spawn_reflector(&dc_ns, r)?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let (_, received) = udp_send_recv_count(&dev_ns, r, 100, 64, Duration::from_secs(3))?;
        assert!(received >= 20, "expected ≥ 20 received at 50% loss, got {received}");
        assert!(received <= 80, "expected ≤ 80 received at 50% loss, got {received}");
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn loss_udp_high() -> Result<()> {
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").build()?;
        let dev = lab
            .add_device("dev")
            .iface("eth0", dc, Some(Impair::Manual { rate: 0, loss: 90.0, latency: 0 }))
            .build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r = SocketAddr::new(IpAddr::V4(dc_ip), 18_100);
        let dc_ns = lab.node_ns(dc)?.to_string();
        let dev_ns = lab.node_ns(dev)?.to_string();
        lab.spawn_reflector(&dc_ns, r)?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let (_, received) = udp_send_recv_count(&dev_ns, r, 100, 64, Duration::from_secs(3))?;
        assert!(received <= 30, "expected ≤ 30 received at 90% loss, got {received}");
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn loss_tcp_integrity() -> Result<()> {
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").build()?;
        let dev = lab
            .add_device("dev")
            .iface("eth0", dc, Some(Impair::Manual { rate: 0, loss: 5.0, latency: 0 }))
            .build()?;
        lab.build().await?;

        const BYTES: usize = 200 * 1024;
        let dc_ip = lab.router_uplink_ip(dc)?;
        let addr = SocketAddr::new(IpAddr::V4(dc_ip), 18_200);
        let dc_ns = lab.node_ns(dc)?.to_string();
        let dev_ns = lab.node_ns(dev)?.to_string();

        // Server in DC writes BYTES to client; client counts received bytes.
        let server = spawn_closure_in_namespace_thread(dc_ns, move || {
            let listener = std::net::TcpListener::bind(addr)?;
            let (mut stream, _) = listener.accept()?;
            let data = vec![0xABu8; BYTES];
            stream.write_all(&data)?;
            Ok(())
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        let n = run_closure_in_namespace(&dev_ns, move || {
            let mut stream =
                std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
            stream.set_read_timeout(Some(Duration::from_secs(30)))?;
            let mut buf = Vec::with_capacity(BYTES);
            stream.read_to_end(&mut buf)?;
            Ok(buf.len())
        })?;

        server
            .join()
            .map_err(|_| anyhow!("server thread panicked"))??;
        assert_eq!(n, BYTES, "TCP must deliver all bytes despite 5% loss");
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn loss_udp_both_directions() -> Result<()> {
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").build()?;
        let dev = lab
            .add_device("dev")
            .iface("eth0", dc, Some(Impair::Manual { rate: 0, loss: 30.0, latency: 0 }))
            .build()?;
        lab.build().await?;

        let bridge = lab.router_downlink_bridge(dc)?;
        lab.set_router_impair(dc, &bridge, Some(Impair::Manual { rate: 0, loss: 30.0, latency: 0 }))?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r = SocketAddr::new(IpAddr::V4(dc_ip), 18_300);
        let dc_ns = lab.node_ns(dc)?.to_string();
        let dev_ns = lab.node_ns(dev)?.to_string();
        lab.spawn_reflector(&dc_ns, r)?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Round-trip delivery ≈ (1-0.3)×(1-0.3) = 49 %; expect < 80.
        let (_, received) = udp_send_recv_count(&dev_ns, r, 100, 64, Duration::from_secs(3))?;
        assert!(received <= 80, "expected < 80 echoes with bidirectional loss, got {received}");
        Ok(())
    }

    // ── 5k: Latency ──────────────────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    #[ignore = "hangs — download-direction impair path needs async worker fix"]
    async fn latency_download_direction() -> Result<()> {
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").build()?;
        let dev = lab.add_device("dev").iface("eth0", dc, None).build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r = SocketAddr::new(IpAddr::V4(dc_ip), 18_400);
        let dc_ns = lab.node_ns(dc)?.to_string();
        let dev_ns = lab.node_ns(dev)?.to_string();
        lab.spawn_reflector(&dc_ns, r)?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let base = udp_rtt_in_ns(&dev_ns, r)?;

        let bridge = lab.router_downlink_bridge(dc)?;
        lab.set_router_impair(dc, &bridge, Some(Impair::Manual { rate: 0, loss: 0.0, latency: 50 }))?;

        let impaired = udp_rtt_in_ns(&dev_ns, r)?;
        assert!(
            impaired >= base + Duration::from_millis(40),
            "expected RTT +40ms after 50ms download latency, base={base:?} impaired={impaired:?}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn latency_upload_and_download() -> Result<()> {
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").build()?;
        let dev = lab
            .add_device("dev")
            .iface("eth0", dc, Some(Impair::Manual { rate: 0, loss: 0.0, latency: 20 }))
            .build()?;
        lab.build().await?;

        let bridge = lab.router_downlink_bridge(dc)?;
        lab.set_router_impair(dc, &bridge, Some(Impair::Manual { rate: 0, loss: 0.0, latency: 30 }))?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r = SocketAddr::new(IpAddr::V4(dc_ip), 18_500);
        let dc_ns = lab.node_ns(dc)?.to_string();
        let dev_ns = lab.node_ns(dev)?.to_string();
        lab.spawn_reflector(&dc_ns, r)?;
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Each packet traverses: upload(20ms) + download(30ms) = 50ms one-way → RTT ≥ 100ms.
        let rtt = udp_rtt_in_ns(&dev_ns, r)?;
        assert!(
            rtt >= Duration::from_millis(90),
            "expected RTT ≥ 90ms with 20ms upload + 30ms download, got {rtt:?}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn latency_device_plus_region() -> Result<()> {
        let mut lab = Lab::new();
        lab.add_region_latency("eu", "us", 40);
        lab.add_region_latency("us", "eu", 40);
        let dc_eu = lab.add_router("dc-eu").region("eu").build()?;
        let dc_us = lab.add_router("dc-us").region("us").build()?;
        let dev = lab
            .add_device("dev")
            .iface("eth0", dc_eu, Some(Impair::Manual { rate: 0, loss: 0.0, latency: 30 }))
            .build()?;
        lab.build().await?;

        let r_us = SocketAddr::new(IpAddr::V4(lab.router_uplink_ip(dc_us)?), 18_600);
        let r_eu = SocketAddr::new(IpAddr::V4(lab.router_uplink_ip(dc_eu)?), 18_601);
        let dc_us_ns = lab.node_ns(dc_us)?.to_string();
        let dc_eu_ns = lab.node_ns(dc_eu)?.to_string();
        let dev_ns = lab.node_ns(dev)?.to_string();
        lab.spawn_reflector(&dc_us_ns, r_us)?;
        lab.spawn_reflector(&dc_eu_ns, r_eu)?;
        tokio::time::sleep(Duration::from_millis(250)).await;

        // eu→us: device(30ms) + region(40ms) = 70ms one-way → RTT ≥ 140ms.
        let rtt_eu_us = udp_rtt_in_ns(&dev_ns, r_us)?;
        assert!(
            rtt_eu_us >= Duration::from_millis(130),
            "expected eu→us RTT ≥ 130ms, got {rtt_eu_us:?}"
        );

        // eu→eu: only device upload impair (30ms egress on dev eth0) → RTT ≈ 30ms.
        // Download path has no qdisc, so we expect at least 25ms to allow slack.
        let rtt_eu_eu = udp_rtt_in_ns(&dev_ns, r_eu)?;
        assert!(
            rtt_eu_eu >= Duration::from_millis(25),
            "expected eu→eu RTT ≥ 25ms (device upload impair only), got {rtt_eu_eu:?}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn latency_multihop_chain() -> Result<()> {
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").build()?;
        let isp = lab.add_router("isp").build()?;
        let nat = lab.add_router("nat").upstream(isp).nat(NatMode::DestinationIndependent).build()?;
        let dev = lab
            .add_device("dev")
            .iface("eth0", nat, Some(Impair::Manual { rate: 0, loss: 0.0, latency: 20 }))
            .build()?;
        lab.build().await?;

        lab.set_router_impair(nat, "wan", Some(Impair::Manual { rate: 0, loss: 0.0, latency: 30 }))?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r = SocketAddr::new(IpAddr::V4(dc_ip), 18_700);
        let dc_ns = lab.node_ns(dc)?.to_string();
        let dev_ns = lab.node_ns(dev)?.to_string();
        lab.spawn_reflector(&dc_ns, r)?;
        tokio::time::sleep(Duration::from_millis(200)).await;

        // One-way: device(20ms) + nat WAN(30ms) = 50ms → RTT ≥ 100ms.
        let rtt = udp_rtt_in_ns(&dev_ns, r)?;
        assert!(
            rtt >= Duration::from_millis(90),
            "expected RTT ≥ 90ms for multi-hop chain, got {rtt:?}"
        );
        Ok(())
    }

    // ── 5l: Dynamic rate and latency changes ─────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn rate_dynamic_decrease() -> Result<()> {
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").build()?;
        let dev = lab
            .add_device("dev")
            .iface("eth0", dc, Some(Impair::Manual { rate: 5000, loss: 0.0, latency: 0 }))
            .build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let dc_ns = lab.node_ns(dc)?.to_string();
        let dev_ns = lab.node_ns(dev)?.to_string();

        let sink = spawn_tcp_sink(&dc_ns, SocketAddr::new(IpAddr::V4(dc_ip), 18_800));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (_e, kbps_fast) =
            tcp_measure_throughput(&dev_ns, SocketAddr::new(IpAddr::V4(dc_ip), 18_800), 256 * 1024)?;
        join_sink(sink)?;

        lab.set_impair("dev", None, Some(Impair::Manual { rate: 500, loss: 0.0, latency: 0 }))?;

        let sink = spawn_tcp_sink(&dc_ns, SocketAddr::new(IpAddr::V4(dc_ip), 18_801));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (_e, kbps_slow) =
            tcp_measure_throughput(&dev_ns, SocketAddr::new(IpAddr::V4(dc_ip), 18_801), 64 * 1024)?;
        join_sink(sink)?;

        assert!(kbps_fast >= 3000, "expected fast ≥ 3000 kbit/s, got {kbps_fast}");
        assert!(kbps_slow <= 700, "expected slow ≤ 700 kbit/s, got {kbps_slow}");
        assert!(
            kbps_slow <= kbps_fast / 4,
            "expected slow ≤ fast/4: slow={kbps_slow} fast={kbps_fast}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn rate_dynamic_remove() -> Result<()> {
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").build()?;
        let dev = lab
            .add_device("dev")
            .iface("eth0", dc, Some(Impair::Manual { rate: 1000, loss: 0.0, latency: 0 }))
            .build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let dc_ns = lab.node_ns(dc)?.to_string();
        let dev_ns = lab.node_ns(dev)?.to_string();

        let sink = spawn_tcp_sink(&dc_ns, SocketAddr::new(IpAddr::V4(dc_ip), 18_900));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (_e, kbps_throttled) =
            tcp_measure_throughput(&dev_ns, SocketAddr::new(IpAddr::V4(dc_ip), 18_900), 128 * 1024)?;
        join_sink(sink)?;

        lab.set_impair("dev", None, None)?;

        let sink = spawn_tcp_sink(&dc_ns, SocketAddr::new(IpAddr::V4(dc_ip), 18_901));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (_e, kbps_free) =
            tcp_measure_throughput(&dev_ns, SocketAddr::new(IpAddr::V4(dc_ip), 18_901), 256 * 1024)?;
        join_sink(sink)?;

        assert!(
            kbps_free >= kbps_throttled * 3,
            "expected unthrottled ≥ 3× throttled: free={kbps_free} throttled={kbps_throttled}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn latency_dynamic_add_remove() -> Result<()> {
        let mut lab = Lab::new();
        let dc = lab.add_router("dc").build()?;
        let dev = lab.add_device("dev").iface("eth0", dc, None).build()?;
        lab.build().await?;

        let dc_ip = lab.router_uplink_ip(dc)?;
        let r = SocketAddr::new(IpAddr::V4(dc_ip), 19_000);
        let dc_ns = lab.node_ns(dc)?.to_string();
        let dev_ns = lab.node_ns(dev)?.to_string();
        lab.spawn_reflector(&dc_ns, r)?;
        tokio::time::sleep(Duration::from_millis(200)).await;

        let baseline = udp_rtt_in_ns(&dev_ns, r)?;

        lab.set_impair("dev", None, Some(Impair::Manual { rate: 0, loss: 0.0, latency: 100 }))?;
        let high = udp_rtt_in_ns(&dev_ns, r)?;
        assert!(
            high >= baseline + Duration::from_millis(90),
            "expected RTT +90ms after 100ms impair, baseline={baseline:?} high={high:?}"
        );

        lab.set_impair("dev", None, None)?;
        let recovered = udp_rtt_in_ns(&dev_ns, r)?;
        assert!(
            recovered < baseline + Duration::from_millis(30),
            "expected RTT to recover near baseline, baseline={baseline:?} recovered={recovered:?}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[traced_test]
    async fn rate_presets() -> Result<()> {
        let cases = [
            (Impair::Wifi, 20u64, 0.0f32),
            (Impair::Mobile, 50u64, 1.0f32),
        ];
        let mut port_base = 19_100u16;
        let mut failures = Vec::new();
        for (preset, min_latency_ms, loss_pct) in cases {
            let result: Result<()> = async {
                let mut lab = Lab::new();
                let dc = lab.add_router("dc").build()?;
                let dev = lab.add_device("dev").iface("eth0", dc, Some(preset)).build()?;
                lab.build().await?;

                let dc_ip = lab.router_uplink_ip(dc)?;
                let r = SocketAddr::new(IpAddr::V4(dc_ip), port_base);
                let dc_ns = lab.node_ns(dc)?.to_string();
                let dev_ns = lab.node_ns(dev)?.to_string();
                lab.spawn_reflector(&dc_ns, r)?;
                tokio::time::sleep(Duration::from_millis(200)).await;

                let rtt = udp_rtt_in_ns(&dev_ns, r)?;
                if rtt < Duration::from_millis(min_latency_ms) {
                    bail!("preset {preset:?}: expected RTT ≥ {min_latency_ms}ms, got {rtt:?}");
                }
                if loss_pct > 0.0 {
                    // 1000 packets: P(zero loss at 1%) ≈ 0.000045 — reliably detects loss.
                    let (_, received) =
                        udp_send_recv_count(&dev_ns, r, 1000, 64, Duration::from_secs(5))?;
                    if received == 1000 {
                        bail!("preset {preset:?}: expected some loss at {loss_pct}%, got {received}/1000");
                    }
                }
                Ok(())
            }
            .await;
            if let Err(e) = result {
                failures.push(format!("{preset:?}: {e:#}"));
            }
            port_base += 10;
        }
        if !failures.is_empty() {
            bail!("{} failures:\n{}", failures.len(), failures.join("\n"));
        }
        Ok(())
    }
}
