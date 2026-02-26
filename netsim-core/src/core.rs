use std::{
    collections::HashMap,
    io::Write as IoWrite,
    net::{Ipv4Addr, Ipv6Addr},
    sync::Arc,
};

use anyhow::{anyhow, bail, Context, Result};
use ipnet::{Ipv4Net, Ipv6Net};
use tokio_util::sync::CancellationToken;
use tracing::{debug, instrument, Instrument as _};

use crate::{netlink::Netlink, netns, qdisc, Impair, IpSupport, NatMode, NatV6Mode};

/// Defines static addressing and naming for one lab instance.
#[derive(Clone, Debug)]
pub(crate) struct CoreConfig {
    /// Process-wide sequential lab identifier (from `LAB_COUNTER`).
    pub lab_id: u64,
    /// Stores the process-unique lab prefix used for namespacing resources.
    pub prefix: String,
    /// Stores the dedicated lab root namespace name.
    pub root_ns: String,
    /// Short tag used to generate bridge interface names (e.g. `"p1230"`).
    pub bridge_tag: String,
    /// Stores the IX bridge interface name inside the lab root namespace.
    pub ix_br: String,
    /// Stores the IX gateway IPv4 address.
    pub ix_gw: Ipv4Addr,
    /// Stores the IX subnet CIDR.
    pub ix_cidr: Ipv4Net,
    /// Stores the base private downstream address pool.
    pub private_cidr: Ipv4Net,
    /// Stores the base public downstream address pool.
    pub public_cidr: Ipv4Net,
    /// Stores the IX gateway IPv6 address.
    pub ix_gw_v6: Ipv6Addr,
    /// Stores the IX IPv6 subnet CIDR.
    pub ix_cidr_v6: Ipv6Net,
    /// Stores the base private downstream IPv6 pool (ULA).
    pub private_cidr_v6: Ipv6Net,
    /// Tracing span for this lab; used to parent worker thread spans.
    pub span: tracing::Span,
}

/// Identifies a node in the topology graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(pub u64);

/// Selects the address pool used for router downstream links.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DownstreamPool {
    /// Uses private RFC1918 addressing.
    Private,
    /// Uses public routable addressing.
    Public,
}

/// Configures per-router NAT and downstream behavior.
#[derive(Clone, Debug)]
pub(crate) struct RouterConfig {
    /// Selects router NAT behavior.
    pub nat: NatMode,
    /// Selects which pool to allocate downstream subnets from.
    pub downstream_pool: DownstreamPool,
    /// Selects router IPv6 NAT behavior.
    pub nat_v6: NatV6Mode,
    /// Selects which IP address families this router supports.
    pub ip_support: IpSupport,
}

/// One network interface on a device, connected to a router's downstream switch.
#[derive(Clone, Debug)]
pub(crate) struct DeviceIfaceData {
    /// Interface name inside the device namespace (e.g. `"eth0"`).
    pub ifname: String,
    /// Stores the switch this interface is attached to.
    pub uplink: NodeId,
    /// Assigned IPv4 address.
    pub ip: Option<Ipv4Addr>,
    /// Assigned IPv6 address.
    pub ip_v6: Option<Ipv6Addr>,
    /// Optional link impairment applied via `tc netem`.
    pub impair: Option<Impair>,
    /// Unique index used to name the root-namespace veth ends.
    pub(crate) idx: u64,
}

/// A network endpoint with one or more interfaces.
#[derive(Clone, Debug)]
pub(crate) struct DeviceData {
    /// Identifies the device node.
    pub id: NodeId,
    /// Stores the device name.
    pub name: String,
    /// Stores the device namespace name.
    pub ns: String,
    /// Interfaces in declaration order.
    pub interfaces: Vec<DeviceIfaceData>,
    /// `ifname` of the interface that carries the default route.
    pub default_via: String,
}

impl DeviceData {
    /// Looks up an interface by name.
    pub(crate) fn iface(&self, name: &str) -> Option<&DeviceIfaceData> {
        self.interfaces.iter().find(|i| i.ifname == name)
    }

    /// Looks up an interface mutably by name.
    pub(crate) fn iface_mut(&mut self, name: &str) -> Option<&mut DeviceIfaceData> {
        self.interfaces.iter_mut().find(|i| i.ifname == name)
    }

    /// Returns the interface that carries the default route.
    ///
    /// # Panics
    /// Panics if `default_via` does not name a known interface (invariant
    /// maintained by `add_device_iface` / `set_device_default_via`).
    pub(crate) fn default_iface(&self) -> &DeviceIfaceData {
        self.iface(&self.default_via)
            .expect("default_via names a valid interface")
    }
}

/// Represents a router and its L3 connectivity state.
#[derive(Clone, Debug)]
pub(crate) struct RouterData {
    /// Identifies the router.
    pub id: NodeId,
    /// Stores the router name.
    pub name: String,
    /// Stores the router namespace name.
    pub ns: String,
    /// Stores the optional router region label.
    pub region: Option<String>,
    /// Stores static router configuration.
    pub cfg: RouterConfig,
    /// Stores the bridge name for the downstream LAN side.
    pub downlink_bridge: String,
    /// Stores the uplink switch identifier.
    pub uplink: Option<NodeId>,
    /// Stores the router uplink IPv4 address.
    pub upstream_ip: Option<Ipv4Addr>,
    /// Stores the router uplink IPv6 address.
    pub upstream_ip_v6: Option<Ipv6Addr>,
    /// Stores the downstream switch identifier.
    pub downlink: Option<NodeId>,
    /// Stores the downstream subnet CIDR.
    pub downstream_cidr: Option<Ipv4Net>,
    /// Stores the downstream gateway address.
    pub downstream_gw: Option<Ipv4Addr>,
    /// Stores the downstream IPv6 subnet CIDR.
    pub downstream_cidr_v6: Option<Ipv6Net>,
    /// Stores the downstream IPv6 gateway address.
    pub downstream_gw_v6: Option<Ipv6Addr>,
}

impl RouterData {
    /// Returns the WAN interface name: `"ix"` for IX-connected routers, `"wan"` for sub-routers.
    pub(crate) fn wan_ifname(&self, ix_sw: NodeId) -> &'static str {
        if self.uplink == Some(ix_sw) {
            "ix"
        } else {
            "wan"
        }
    }
}

/// Represents an L2 switch/bridge attachment point.
#[derive(Clone, Debug)]
pub(crate) struct Switch {
    /// Stores the switch name.
    pub name: String,
    /// Stores the switch IPv4 subnet if assigned.
    pub cidr: Option<Ipv4Net>,
    /// Stores the switch IPv4 gateway address if assigned.
    pub gw: Option<Ipv4Addr>,
    /// Stores the switch IPv6 subnet if assigned.
    pub cidr_v6: Option<Ipv6Net>,
    /// Stores the switch IPv6 gateway address if assigned.
    pub gw_v6: Option<Ipv6Addr>,
    /// Stores the owning router for managed downstream switches.
    pub owner_router: Option<NodeId>,
    /// Stores the backing bridge name.
    pub bridge: Option<String>,
    next_host: u8,
    next_host_v6: u8,
}

/// Per-interface wiring job collected by `build()`.
#[derive(Clone)]
pub(crate) struct IfaceBuild {
    pub(crate) dev_ns: String,
    pub(crate) gw_ns: String,
    pub(crate) gw_ip: Option<Ipv4Addr>,
    pub(crate) gw_br: String,
    pub(crate) dev_ip: Option<Ipv4Addr>,
    pub(crate) prefix_len: u8,
    pub(crate) gw_ip_v6: Option<Ipv6Addr>,
    pub(crate) dev_ip_v6: Option<Ipv6Addr>,
    pub(crate) prefix_len_v6: u8,
    pub(crate) impair: Option<Impair>,
    pub(crate) ifname: String,
    pub(crate) is_default: bool,
    pub(crate) idx: u64,
}

/// Stores mutable topology state and build-time allocators.
pub(crate) struct NetworkCore {
    pub(crate) cfg: CoreConfig,
    pub(crate) netns: Arc<netns::NetnsManager>,
    /// (from_region, to_region, latency_ms) pairs; applied as tc netem during build.
    pub(crate) region_latencies: Vec<(String, String, u32)>,
    /// Cancellation token: cancelled on Drop to stop all spawned async tasks.
    pub(crate) cancel: CancellationToken,
    next_id: u64,
    next_private_subnet: u16,
    next_public_subnet: u16,
    next_ix_low: u8,
    next_ix_low_v6: u16,
    next_private_subnet_v6: u16,
    bridge_counter: u32,
    ix_sw: NodeId,
    devices: HashMap<NodeId, DeviceData>,
    routers: HashMap<NodeId, RouterData>,
    switches: HashMap<NodeId, Switch>,
    nodes_by_name: HashMap<String, NodeId>,
    /// Whether root namespace and IX bridge have been set up.
    pub(crate) root_ns_initialized: bool,
}

impl Drop for NetworkCore {
    fn drop(&mut self) {
        self.cancel.cancel();
        // NetnsManager::drop cleans up all workers when the last Arc is released.
    }
}

impl NetworkCore {
    /// Constructs a new topology core and pre-creates the IX switch.
    pub(crate) fn new(cfg: CoreConfig) -> Self {
        let mut core = Self {
            netns: Arc::new(netns::NetnsManager::new_with_span(cfg.span.clone())),
            cfg,
            region_latencies: Vec::new(),
            cancel: CancellationToken::new(),
            next_id: 1,
            next_private_subnet: 1,
            next_public_subnet: 1,
            next_ix_low: 10,
            next_ix_low_v6: 0x10,
            next_private_subnet_v6: 1,
            bridge_counter: 2,
            ix_sw: NodeId(0),
            devices: HashMap::new(),
            routers: HashMap::new(),
            switches: HashMap::new(),
            nodes_by_name: HashMap::new(),
            root_ns_initialized: false,
        };
        let ix_sw = core.add_switch(
            "ix",
            Some(core.cfg.ix_cidr),
            Some(core.cfg.ix_gw),
            Some(core.cfg.ix_cidr_v6),
            Some(core.cfg.ix_gw_v6),
        );
        core.ix_sw = ix_sw;
        core
    }

    fn next_bridge_name(&mut self) -> String {
        let name = format!("br-{}-{}", self.cfg.bridge_tag, self.bridge_counter);
        self.bridge_counter = self.bridge_counter.saturating_add(1);
        name
    }

    /// Returns a cloned tokio runtime handle for the given namespace.
    pub(crate) fn rt_handle_for(&self, ns: &str) -> Result<tokio::runtime::Handle> {
        self.netns.rt_handle_for(ns)
    }

    /// Spawns an async UDP reflector in the given namespace.
    pub(crate) fn spawn_reflector_in(&self, ns: &str, bind: std::net::SocketAddr) -> Result<()> {
        let cancel = self.cancel.clone();
        let rt = self.rt_handle_for(ns)?;
        rt.spawn(async move {
            let _ = crate::test_utils::run_reflector(bind, cancel).await;
        });
        Ok(())
    }

    /// Returns the IX gateway address.
    pub(crate) fn ix_gw(&self) -> Ipv4Addr {
        self.cfg.ix_gw
    }

    /// Allocates the next low-end IX host address.
    pub(crate) fn alloc_ix_ip_low(&mut self) -> Ipv4Addr {
        let o = self.cfg.ix_gw.octets();
        let ip = Ipv4Addr::new(o[0], o[1], o[2], self.next_ix_low);
        self.next_ix_low = self.next_ix_low.saturating_add(1);
        ip
    }

    /// Returns the IX switch identifier.
    pub(crate) fn ix_sw(&self) -> NodeId {
        self.ix_sw
    }

    /// Returns the lab root namespace name.
    pub(crate) fn root_ns(&self) -> &str {
        &self.cfg.root_ns
    }

    /// Returns the namespace name for router `id`.
    pub(crate) fn router_ns(&self, id: NodeId) -> Result<&str> {
        self.routers
            .get(&id)
            .map(|r| r.ns.as_str())
            .ok_or_else(|| anyhow!("unknown router id"))
    }

    /// Returns router data for `id`.
    pub(crate) fn router(&self, id: NodeId) -> Option<&RouterData> {
        self.routers.get(&id)
    }

    /// Returns mutable router data for `id`.
    pub(crate) fn router_mut(&mut self, id: NodeId) -> Option<&mut RouterData> {
        self.routers.get_mut(&id)
    }

    /// Returns device data for `id`.
    pub(crate) fn device(&self, id: NodeId) -> Option<&DeviceData> {
        self.devices.get(&id)
    }

    /// Returns mutable device data for `id`.
    pub(crate) fn device_mut(&mut self, id: NodeId) -> Option<&mut DeviceData> {
        self.devices.get_mut(&id)
    }

    /// Returns switch data for `id`.
    pub(crate) fn switch(&self, id: NodeId) -> Option<&Switch> {
        self.switches.get(&id)
    }

    /// Returns the router identifier for `name`, or `None` if not a router.
    pub(crate) fn router_id_by_name(&self, name: &str) -> Option<NodeId> {
        let id = *self.nodes_by_name.get(name)?;
        self.routers.contains_key(&id).then_some(id)
    }

    /// Returns the device identifier for `name`, or `None` if not a device.
    pub(crate) fn device_id_by_name(&self, name: &str) -> Option<NodeId> {
        let id = *self.nodes_by_name.get(name)?;
        self.devices.contains_key(&id).then_some(id)
    }

    /// Returns `(ns, downlink_bridge_name, wan_if_name, upstream_ip)` for a built router.
    pub(crate) fn router_nat_params(&self, id: NodeId) -> Result<(String, String, String, Ipv4Addr)> {
        let router = self.routers.get(&id).context("unknown router id")?;
        let wan_if = router.wan_ifname(self.ix_sw);
        let upstream_ip = router
            .upstream_ip
            .context("router has no upstream ip (not yet built?)")?;
        Ok((
            router.ns.clone(),
            router.downlink_bridge.clone(),
            wan_if.to_string(),
            upstream_ip,
        ))
    }

    /// Stores an updated NAT mode on the router record.
    pub(crate) fn set_router_nat_mode(&mut self, id: NodeId, mode: NatMode) -> Result<()> {
        let router = self.routers.get_mut(&id).context("unknown router id")?;
        router.cfg.nat = mode;
        Ok(())
    }

    /// Adds a router node and returns its identifier.
    ///
    /// The namespace name and downstream bridge name are generated internally.
    pub(crate) fn add_router(
        &mut self,
        name: &str,
        nat: NatMode,
        downstream_pool: DownstreamPool,
        region: Option<String>,
        ip_support: IpSupport,
        nat_v6: NatV6Mode,
    ) -> NodeId {
        let id = NodeId(self.alloc_id());
        let ns = format!("lab{}-r{}", self.cfg.lab_id, id.0);
        let downlink_bridge = self.next_bridge_name();
        self.nodes_by_name.insert(name.to_string(), id);
        self.routers.insert(
            id,
            RouterData {
                id,
                name: name.to_string(),
                ns,
                region,
                cfg: RouterConfig {
                    nat,
                    downstream_pool,
                    nat_v6,
                    ip_support,
                },
                downlink_bridge,
                uplink: None,
                upstream_ip: None,
                upstream_ip_v6: None,
                downlink: None,
                downstream_cidr: None,
                downstream_gw: None,
                downstream_cidr_v6: None,
                downstream_gw_v6: None,
            },
        );
        id
    }

    /// Creates a device shell with no interfaces yet.
    ///
    /// The namespace name is generated internally.
    /// Call [`add_device_iface`] one or more times to attach interfaces, then
    /// optionally [`set_device_default_via`] to override the default route
    /// interface (first interface by default).
    pub(crate) fn add_device(&mut self, name: &str) -> NodeId {
        let id = NodeId(self.alloc_id());
        let ns = format!("lab{}-d{}", self.cfg.lab_id, id.0);
        self.nodes_by_name.insert(name.to_string(), id);
        self.devices.insert(
            id,
            DeviceData {
                id,
                name: name.to_string(),
                ns,
                interfaces: vec![],
                default_via: String::new(),
            },
        );
        id
    }

    /// Adds an interface to a device, connected to `router`'s downstream switch.
    ///
    /// Allocates an IP from the router's downstream pool.  The first interface
    /// added becomes the `default_via` unless [`set_device_default_via`] is
    /// called afterwards.
    ///
    /// Returns the allocated IP address.
    pub(crate) fn add_device_iface(
        &mut self,
        device: NodeId,
        ifname: &str,
        router: NodeId,
        impair: Option<Impair>,
    ) -> Result<Option<Ipv4Addr>> {
        let downlink = self
            .routers
            .get(&router)
            .and_then(|r| r.downlink)
            .ok_or_else(|| anyhow!("router missing downlink switch"))?;
        // Allocate v4 if the switch has a v4 CIDR (skip for V6Only routers).
        let assigned = self
            .switches
            .get(&downlink)
            .and_then(|sw| sw.cidr)
            .is_some()
            .then(|| self.alloc_from_switch(downlink))
            .transpose()?;
        // Allocate v6 if the switch has a v6 CIDR.
        let assigned_v6 = self
            .switches
            .get(&downlink)
            .and_then(|sw| sw.cidr_v6)
            .is_some()
            .then(|| self.alloc_from_switch_v6(downlink))
            .transpose()?;
        let idx = self.alloc_id();
        let dev = self
            .devices
            .get_mut(&device)
            .ok_or_else(|| anyhow!("unknown device id"))?;
        // First interface becomes the default unless overridden later.
        if dev.default_via.is_empty() {
            dev.default_via = ifname.to_string();
        }
        dev.interfaces.push(DeviceIfaceData {
            ifname: ifname.to_string(),
            uplink: downlink,
            ip: assigned,
            ip_v6: assigned_v6,
            impair,
            idx,
        });
        Ok(assigned)
    }

    /// Changes which interface carries the default route.
    pub(crate) fn set_device_default_via(&mut self, device: NodeId, ifname: &str) -> Result<()> {
        let dev = self
            .devices
            .get_mut(&device)
            .ok_or_else(|| anyhow!("unknown device id"))?;
        if !dev.interfaces.iter().any(|i| i.ifname == ifname) {
            bail!("interface '{}' not found on device '{}'", ifname, dev.name);
        }
        dev.default_via = ifname.to_string();
        Ok(())
    }

    /// Returns the gateway IP of a router's downstream switch.
    ///
    /// Used by dynamic operations that need to re-issue `ip route add default`.
    pub(crate) fn router_downlink_gw_for_switch(&self, sw: NodeId) -> Result<Ipv4Addr> {
        self.switches
            .get(&sw)
            .and_then(|s| s.gw)
            .ok_or_else(|| anyhow!("switch missing gateway ip"))
    }

    /// Adds a switch node and returns its identifier.
    pub(crate) fn add_switch(
        &mut self,
        name: &str,
        cidr: Option<Ipv4Net>,
        gw: Option<Ipv4Addr>,
        cidr_v6: Option<Ipv6Net>,
        gw_v6: Option<Ipv6Addr>,
    ) -> NodeId {
        let id = NodeId(self.alloc_id());
        self.nodes_by_name.insert(name.to_string(), id);
        self.switches.insert(
            id,
            Switch {
                name: name.to_string(),
                cidr,
                gw,
                cidr_v6,
                gw_v6,
                owner_router: None,
                bridge: None,
                next_host: 2,
                next_host_v6: 2,
            },
        );
        id
    }

    /// Connects `router` to uplink switch `sw` and returns its uplink IP.
    pub(crate) fn connect_router_uplink(
        &mut self,
        router: NodeId,
        sw: NodeId,
        ip: Option<Ipv4Addr>,
        ip_v6: Option<Ipv6Addr>,
    ) -> Result<()> {
        let router_entry = self
            .routers
            .get_mut(&router)
            .ok_or_else(|| anyhow!("unknown router id"))?;
        router_entry.uplink = Some(sw);
        router_entry.upstream_ip = ip;
        router_entry.upstream_ip_v6 = ip_v6;
        Ok(())
    }

    /// Connects `router` to downstream switch `sw` and returns `(cidr, gw)`.
    pub(crate) fn connect_router_downlink(
        &mut self,
        router: NodeId,
        sw: NodeId,
    ) -> Result<(Option<Ipv4Net>, Option<Ipv4Addr>)> {
        let router_data = self
            .routers
            .get(&router)
            .ok_or_else(|| anyhow!("unknown router id"))?;
        let pool = router_data.cfg.downstream_pool;
        let has_v4 = router_data.cfg.ip_support.has_v4();
        let has_v6 = router_data.cfg.ip_support.has_v6();

        // Allocate v4 CIDR for the downstream switch (skip for V6Only).
        let (cidr, gw) = if has_v4 {
            let sw_entry = self
                .switches
                .get(&sw)
                .ok_or_else(|| anyhow!("unknown switch id"))?;
            if sw_entry.cidr.is_some() {
                let cidr = sw_entry.cidr.unwrap();
                let gw = sw_entry
                    .gw
                    .ok_or_else(|| anyhow!("switch '{}' missing gw", sw_entry.name))?;
                (Some(cidr), Some(gw))
            } else {
                let cidr = match pool {
                    DownstreamPool::Private => self.alloc_private_cidr()?,
                    DownstreamPool::Public => self.alloc_public_cidr()?,
                };
                let gw = add_host(cidr, 1)?;
                (Some(cidr), Some(gw))
            }
        } else {
            (None, None)
        };

        // Allocate v6 CIDR for the downstream switch if needed.
        let (cidr_v6, gw_v6) = if has_v6 {
            let sw_entry = self
                .switches
                .get(&sw)
                .ok_or_else(|| anyhow!("unknown switch id"))?;
            if sw_entry.cidr_v6.is_some() {
                (sw_entry.cidr_v6, sw_entry.gw_v6)
            } else {
                let c6 = self.alloc_private_cidr_v6()?;
                let seg = c6.addr().segments();
                let g6 = Ipv6Addr::new(seg[0], seg[1], seg[2], seg[3], seg[4], seg[5], seg[6], 1);
                (Some(c6), Some(g6))
            }
        } else {
            (None, None)
        };

        let sw_entry = self
            .switches
            .get_mut(&sw)
            .ok_or_else(|| anyhow!("unknown switch id"))?;
        sw_entry.cidr = cidr;
        sw_entry.gw = gw;
        sw_entry.cidr_v6 = cidr_v6;
        sw_entry.gw_v6 = gw_v6;
        let bridge = self
            .routers
            .get(&router)
            .ok_or_else(|| anyhow!("unknown router id"))?
            .downlink_bridge
            .clone();
        sw_entry.owner_router = Some(router);
        sw_entry.bridge = Some(bridge);

        let router_entry = self
            .routers
            .get_mut(&router)
            .ok_or_else(|| anyhow!("unknown router id"))?;
        router_entry.downlink = Some(sw);
        router_entry.downstream_cidr = cidr;
        router_entry.downstream_gw = gw;
        router_entry.downstream_cidr_v6 = cidr_v6;
        router_entry.downstream_gw_v6 = gw_v6;
        Ok((cidr, gw))
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn alloc_private_cidr(&mut self) -> Result<Ipv4Net> {
        let subnet = self.next_private_subnet;
        self.next_private_subnet = self.next_private_subnet.saturating_add(1);
        let base = self.cfg.private_cidr.addr().octets();
        let cidr = Ipv4Net::new(
            Ipv4Addr::new(base[0], base[1], (subnet & 0xff) as u8, 0),
            24,
        )
        .context("allocate private /24")?;
        Ok(cidr)
    }

    fn alloc_public_cidr(&mut self) -> Result<Ipv4Net> {
        let subnet = self.next_public_subnet;
        self.next_public_subnet = self.next_public_subnet.saturating_add(1);
        let base = self.cfg.public_cidr.addr().octets();
        let cidr = Ipv4Net::new(
            Ipv4Addr::new(base[0], base[1], (subnet & 0xff) as u8, 0),
            24,
        )
        .context("allocate public /24")?;
        Ok(cidr)
    }

    pub(crate) fn alloc_from_switch(&mut self, sw: NodeId) -> Result<Ipv4Addr> {
        let sw_entry = self
            .switches
            .get_mut(&sw)
            .ok_or_else(|| anyhow!("unknown switch id"))?;
        let cidr = sw_entry
            .cidr
            .ok_or_else(|| anyhow!("switch '{}' missing cidr", sw_entry.name))?;
        let ip = add_host(cidr, sw_entry.next_host)?;
        sw_entry.next_host = sw_entry.next_host.saturating_add(1);
        Ok(ip)
    }

    /// Allocates the next IX IPv6 address (2001:db8::N).
    pub(crate) fn alloc_ix_ip_v6_low(&mut self) -> Ipv6Addr {
        let seg = self.cfg.ix_gw_v6.segments();
        let host = self.next_ix_low_v6;
        self.next_ix_low_v6 = self.next_ix_low_v6.saturating_add(1);
        Ipv6Addr::new(seg[0], seg[1], seg[2], seg[3], seg[4], seg[5], seg[6], host)
    }

    /// Allocates the next private /64 from the ULA pool (fd10:0:N::/64).
    pub(crate) fn alloc_private_cidr_v6(&mut self) -> Result<Ipv6Net> {
        let subnet = self.next_private_subnet_v6;
        self.next_private_subnet_v6 = self.next_private_subnet_v6.saturating_add(1);
        let base = self.cfg.private_cidr_v6.addr().segments();
        let cidr = Ipv6Net::new(
            Ipv6Addr::new(base[0], base[1], base[2], subnet, 0, 0, 0, 0),
            64,
        )
        .context("allocate private /64 v6")?;
        Ok(cidr)
    }

    /// Allocates the next host address from a switch's IPv6 pool.
    pub(crate) fn alloc_from_switch_v6(&mut self, sw: NodeId) -> Result<Ipv6Addr> {
        let sw_entry = self
            .switches
            .get_mut(&sw)
            .ok_or_else(|| anyhow!("unknown switch id"))?;
        let cidr = sw_entry
            .cidr_v6
            .ok_or_else(|| anyhow!("switch '{}' missing v6 cidr", sw_entry.name))?;
        let host = sw_entry.next_host_v6;
        sw_entry.next_host_v6 = sw_entry.next_host_v6.saturating_add(1);
        let seg = cidr.addr().segments();
        Ok(Ipv6Addr::new(
            seg[0],
            seg[1],
            seg[2],
            seg[3],
            seg[4],
            seg[5],
            seg[6],
            host as u16,
        ))
    }

    /// Returns an iterator over all devices in the topology.
    pub(crate) fn all_devices(&self) -> impl Iterator<Item = &DeviceData> {
        self.devices.values()
    }

    /// Returns an iterator over all routers in the topology.
    pub(crate) fn all_routers(&self) -> impl Iterator<Item = &RouterData> {
        self.routers.values()
    }

    /// Returns all device node ids.
    pub(crate) fn all_device_ids(&self) -> Vec<NodeId> {
        self.devices.keys().copied().collect()
    }

    /// Returns all router node ids.
    pub(crate) fn all_router_ids(&self) -> Vec<NodeId> {
        self.routers.keys().copied().collect()
    }

}

// ─────────────────────────────────────────────
// Free async setup functions (used by builders; no lock held)
// ─────────────────────────────────────────────

/// Helper: run a netlink operation in a namespace via the shared NetnsManager.
pub(crate) async fn nl_run<F, Fut>(netns: &Arc<netns::NetnsManager>, ns: &str, f: F) -> Result<()>
where
    F: FnOnce(Netlink) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<()>> + Send + 'static,
{
    let nl = netns.netlink_for(ns)?;
    let rt = netns.rt_handle_for(ns)?;
    let span = tracing::Span::current();
    rt.spawn(f(nl).instrument(span))
        .await
        .context("netlink task panicked")?
}

/// Creates root namespace, IX bridge, and enables forwarding. Idempotent-safe at caller level.
#[instrument(name = "root", skip_all)]
pub(crate) async fn setup_root_ns_async(
    cfg: &CoreConfig,
    netns: &Arc<netns::NetnsManager>,
) -> Result<()> {
    let root_ns = cfg.root_ns.clone();
    create_named_netns(netns, &root_ns)?;

    if let Err(err) = run_nft_in(netns, &root_ns, "flush ruleset") {
        debug!(error = %err, "setup_root_ns: nft flush failed; continuing");
    }

    netns.run_closure_in(&root_ns, || {
        set_sysctl_root("net/ipv4/ip_forward", "1")?;
        set_sysctl_root("net/ipv6/conf/all/forwarding", "1")?;
        Ok(())
    })?;

    let cfg = cfg.clone();
    nl_run(netns, &root_ns, move |h: Netlink| async move {
        h.set_link_up("lo").await?;
        h.ensure_link_deleted(&cfg.ix_br).await.ok();
        h.add_bridge(&cfg.ix_br).await?;
        h.set_link_up(&cfg.ix_br).await?;
        h.add_addr4(&cfg.ix_br, cfg.ix_gw, cfg.ix_cidr.prefix_len())
            .await?;
        h.add_addr6(&cfg.ix_br, cfg.ix_gw_v6, cfg.ix_cidr_v6.prefix_len())
            .await?;
        Ok(())
    })
    .await?;
    Ok(())
}

/// Data snapshot needed to set up a single router.
#[derive(Clone)]
pub(crate) struct RouterSetupData {
    pub router: RouterData,
    pub root_ns: String,
    pub prefix: String,
    pub ix_sw: NodeId,
    pub ix_br: String,
    pub ix_gw: Ipv4Addr,
    pub ix_cidr_prefix: u8,
    /// For sub-routers: upstream switch info.
    pub upstream_owner_ns: Option<String>,
    pub upstream_bridge: Option<String>,
    pub upstream_gw: Option<Ipv4Addr>,
    pub upstream_cidr_prefix: Option<u8>,
    /// For IX-level public routers: downstream CIDR for return route.
    pub return_route: Option<(Ipv4Addr, u8, Ipv4Addr)>,
    /// Downlink bridge name (if router has downstream switch) and optional v4 address.
    pub downlink_bridge: Option<(String, Option<(Ipv4Addr, u8)>)>,
    // ── IPv6 fields ──
    pub ix_gw_v6: Option<Ipv6Addr>,
    pub ix_cidr_v6_prefix: Option<u8>,
    pub upstream_gw_v6: Option<Ipv6Addr>,
    pub upstream_cidr_prefix_v6: Option<u8>,
    pub return_route_v6: Option<(Ipv6Addr, u8, Ipv6Addr)>,
    pub downlink_bridge_v6: Option<(Ipv6Addr, u8)>,
    /// For sub-routers with NatV6Mode::None: route in the parent router's ns
    /// for the sub-router's downstream v6 subnet via the sub-router's WAN IP.
    pub parent_route_v6: Option<(String, Ipv6Addr, u8, Ipv6Addr)>, // (parent_ns, net, prefix, via)
}

/// Sets up a single router's namespaces, links, and NAT. No lock held.
#[instrument(name = "router", skip_all, fields(id = data.router.id.0))]
pub(crate) async fn setup_router_async(
    netns: &Arc<netns::NetnsManager>,
    data: &RouterSetupData,
) -> Result<()> {
    let router = &data.router;
    let id = router.id;
    debug!(name = %router.name, ns = %router.ns, "router: setup");

    // Create router namespace.
    create_named_netns(netns, &router.ns)?;
    if let Err(err) = run_nft_in(netns, &router.ns, "flush ruleset") {
        debug!(error = %err, "setup_router: nft flush failed; continuing");
    }

    let uplink = router
        .uplink
        .ok_or_else(|| anyhow!("router missing uplink"))?;

    if uplink == data.ix_sw {
        // IX-level router.
        let root_if = format!("{}i{}", data.prefix, id.0);
        let ns_if = "ix".to_string();

        let router_ns_fd = netns.ns_fd(&router.ns)?;
        nl_run(netns, &data.root_ns, {
            let root_if = root_if.clone();
            let ns_if = ns_if.clone();
            let ix_br = data.ix_br.clone();
            move |h: Netlink| async move {
                h.ensure_link_deleted(&root_if).await.ok();
                h.ensure_link_deleted(&ns_if).await.ok();
                h.add_veth(&root_if, &ns_if).await?;
                h.set_master(&root_if, &ix_br).await?;
                h.set_link_up(&root_if).await?;
                h.move_link_to_netns(&ns_if, &router_ns_fd).await?;
                Ok(())
            }
        })
        .await?;

        // DAD already disabled by create_netns; enable forwarding.
        {
            let has_v6 = router.cfg.ip_support.has_v6();
            netns.run_closure_in(&router.ns, move || {
                set_sysctl_root("net/ipv4/ip_forward", "1")?;
                if has_v6 {
                    set_sysctl_root("net/ipv6/conf/all/forwarding", "1")?;
                }
                Ok(())
            })?;
        }

        nl_run(netns, &router.ns, {
            let d = data.clone();
            let ns_if = ns_if.clone();
            move |h: Netlink| async move {
                h.set_link_up("lo").await?;
                h.set_link_up(&ns_if).await?;
                if let Some(ip4) = d.router.upstream_ip {
                    h.add_addr4(&ns_if, ip4, d.ix_cidr_prefix).await?;
                    h.add_default_route_v4(d.ix_gw).await?;
                }
                if let (Some(ip6), Some(prefix6), Some(gw6)) =
                    (d.router.upstream_ip_v6, d.ix_cidr_v6_prefix, d.ix_gw_v6)
                {
                    h.add_addr6(&ns_if, ip6, prefix6).await?;
                    h.add_default_route_v6(gw6).await?;
                }
                Ok(())
            }
        })
        .await?;

        if let Some(upstream_ip4) = router.upstream_ip {
            debug!(nat = ?router.cfg.nat, ip = %upstream_ip4, "router: apply NAT");
            apply_nat(
                netns,
                &router.ns,
                router.cfg.nat,
                &router.downlink_bridge,
                &ns_if,
                upstream_ip4,
            )?;
        }

        // IPv6 NAT (IX-level router).
        if router.cfg.nat_v6 != NatV6Mode::None {
            if let (Some(up_v6), Some(up_prefix), Some((dl_gw_v6, dl_prefix))) = (
                router.upstream_ip_v6,
                data.ix_cidr_v6_prefix,
                data.downlink_bridge_v6,
            ) {
                let wan_pfx = Ipv6Net::new(up_v6, up_prefix)
                    .unwrap_or_else(|_| Ipv6Net::new(up_v6, 128).unwrap());
                let lan_pfx = Ipv6Net::new(dl_gw_v6, dl_prefix)
                    .unwrap_or_else(|_| Ipv6Net::new(dl_gw_v6, 64).unwrap());
                debug!(nat_v6 = ?router.cfg.nat_v6, "router: apply NAT v6");
                apply_nat_v6(netns, &router.ns, router.cfg.nat_v6, &ns_if, lan_pfx, wan_pfx)?;
            }
        }
    } else {
        // Sub-router.
        let owner_ns = data
            .upstream_owner_ns
            .as_ref()
            .ok_or_else(|| anyhow!("sub-router missing upstream owner ns"))?;
        let bridge = data
            .upstream_bridge
            .as_ref()
            .ok_or_else(|| anyhow!("sub-router missing upstream bridge"))?;
        let gw_ip = data
            .upstream_gw
            .ok_or_else(|| anyhow!("sub-router missing upstream gw"))?;

        let root_a = format!("{}a{}", data.prefix, id.0);
        let root_b = format!("{}b{}", data.prefix, id.0);
        let owner_ns_fd = netns.ns_fd(owner_ns)?;
        let router_ns_fd = netns.ns_fd(&router.ns)?;
        nl_run(netns, &data.root_ns, {
            let root_a = root_a.clone();
            let root_b = root_b.clone();
            move |h: Netlink| async move {
                h.ensure_link_deleted(&root_a).await.ok();
                h.ensure_link_deleted(&root_b).await.ok();
                h.add_veth(&root_a, &root_b).await?;
                h.move_link_to_netns(&root_a, &owner_ns_fd).await?;
                h.move_link_to_netns(&root_b, &router_ns_fd).await?;
                Ok(())
            }
        })
        .await?;

        let owner_if = format!("h{}", id.0);
        nl_run(netns, owner_ns, {
            let root_a = root_a.clone();
            let bridge = bridge.clone();
            move |h: Netlink| async move {
                h.rename_link(&root_a, &owner_if).await?;
                h.set_link_up(&owner_if).await?;
                h.set_master(&owner_if, &bridge).await?;
                Ok(())
            }
        })
        .await?;

        // DAD already disabled by create_named_netns; enable forwarding.
        {
            let has_v6 = router.cfg.ip_support.has_v6();
            netns.run_closure_in(&router.ns, move || {
                set_sysctl_root("net/ipv4/ip_forward", "1")?;
                if has_v6 {
                    set_sysctl_root("net/ipv6/conf/all/forwarding", "1")?;
                }
                Ok(())
            })?;
        }

        let wan_if = "wan".to_string();
        nl_run(netns, &router.ns, {
            let d = data.clone();
            let root_b = root_b.clone();
            let wan_if = wan_if.clone();
            move |h: Netlink| async move {
                h.set_link_up("lo").await?;
                h.rename_link(&root_b, &wan_if).await?;
                h.set_link_up(&wan_if).await?;
                if let (Some(ip4), Some(prefix4)) = (d.router.upstream_ip, d.upstream_cidr_prefix) {
                    h.add_addr4(&wan_if, ip4, prefix4).await?;
                    h.add_default_route_v4(gw_ip).await?;
                }
                if let (Some(ip6), Some(prefix6), Some(g6)) = (
                    d.router.upstream_ip_v6,
                    d.upstream_cidr_prefix_v6,
                    d.upstream_gw_v6,
                ) {
                    h.add_addr6(&wan_if, ip6, prefix6).await?;
                    h.add_default_route_v6(g6).await?;
                }
                Ok(())
            }
        })
        .await?;

        if let Some(upstream_ip4) = router.upstream_ip {
            debug!(nat = ?router.cfg.nat, ip = %upstream_ip4, "router: apply NAT");
            apply_nat(
                netns,
                &router.ns,
                router.cfg.nat,
                &router.downlink_bridge,
                &wan_if,
                upstream_ip4,
            )?;
        }

        // IPv6 NAT (sub-router).
        if router.cfg.nat_v6 != NatV6Mode::None {
            if let (Some(up_v6), Some(up_prefix), Some((dl_gw_v6, dl_prefix))) = (
                router.upstream_ip_v6,
                data.upstream_cidr_prefix_v6,
                data.downlink_bridge_v6,
            ) {
                let wan_pfx = Ipv6Net::new(up_v6, up_prefix)
                    .unwrap_or_else(|_| Ipv6Net::new(up_v6, 128).unwrap());
                let lan_pfx = Ipv6Net::new(dl_gw_v6, dl_prefix)
                    .unwrap_or_else(|_| Ipv6Net::new(dl_gw_v6, 64).unwrap());
                debug!(nat_v6 = ?router.cfg.nat_v6, "router: apply NAT v6");
                apply_nat_v6(netns, &router.ns, router.cfg.nat_v6, &wan_if, lan_pfx, wan_pfx)?;
            }
        }
    }

    // Create downlink bridge.
    if let Some((br, v4_addr)) = &data.downlink_bridge {
        let downlink_v6 = data.downlink_bridge_v6;
        let v4_addr = *v4_addr;
        nl_run(netns, &router.ns, {
            let br = br.clone();
            move |h: Netlink| async move {
                h.set_link_up("lo").await?;
                h.ensure_link_deleted(&br).await.ok();
                h.add_bridge(&br).await?;
                h.set_link_up(&br).await?;
                if let Some((lan_ip, lan_prefix)) = v4_addr {
                    h.add_addr4(&br, lan_ip, lan_prefix).await?;
                }
                if let Some((gw_v6, prefix_v6)) = downlink_v6 {
                    h.add_addr6(&br, gw_v6, prefix_v6).await?;
                }
                Ok(())
            }
        })
        .await?;
    }

    // Return route in lab root for public downstreams.
    if let Some((net, prefix_len, via)) = data.return_route {
        nl_run(netns, &data.root_ns, move |h: Netlink| async move {
            h.add_route_v4(net, prefix_len, via).await.ok();
            Ok(())
        })
        .await
        .ok();
    }
    if let Some((net6, prefix6, via6)) = data.return_route_v6 {
        nl_run(netns, &data.root_ns, move |h: Netlink| async move {
            h.add_route_v6(net6, prefix6, via6).await.ok();
            Ok(())
        })
        .await
        .ok();
    }

    // Route in parent router's ns for sub-router's downstream (NatV6Mode::None).
    if let Some((ref parent_ns, net6, prefix6, via6)) = data.parent_route_v6 {
        nl_run(netns, parent_ns, move |h: Netlink| async move {
            h.add_route_v6(net6, prefix6, via6).await.ok();
            Ok(())
        })
        .await
        .ok();
    }

    Ok(())
}

/// Sets up a single device's namespace and wires all interfaces. No lock held.
#[instrument(name = "device", skip_all, fields(id = dev.id.0))]
pub(crate) async fn setup_device_async(
    netns: &Arc<netns::NetnsManager>,
    prefix: &str,
    root_ns: &str,
    dev: &DeviceData,
    ifaces: Vec<IfaceBuild>,
) -> Result<()> {
    debug!(name = %dev.name, ns = %dev.ns, "device: setup");
    create_named_netns(netns, &dev.ns)?;
    if let Err(err) = run_nft_in(netns, &dev.ns, "flush ruleset") {
        debug!(error = %err, "setup_device: nft flush failed; continuing");
    }

    for iface in ifaces {
        wire_iface_async(netns, prefix, root_ns, iface).await?;
    }
    Ok(())
}

/// Wire one device interface: veth pair, move, IP, route, impairment.
#[instrument(name = "iface", skip_all, fields(iface = %dev.ifname))]
pub(crate) async fn wire_iface_async(
    netns: &Arc<netns::NetnsManager>,
    prefix: &str,
    root_ns: &str,
    dev: IfaceBuild,
) -> Result<()> {
    debug!(ip = ?dev.dev_ip, ip6 = ?dev.dev_ip_v6, gw = ?dev.gw_ip, gw6 = ?dev.gw_ip_v6, "iface: assigned addresses");
    let root_gw = format!("{}g{}", prefix, dev.idx);
    let root_dev = format!("{}e{}", prefix, dev.idx);

    let gw_ns_fd = netns.ns_fd(&dev.gw_ns)?;
    let dev_ns_fd = netns.ns_fd(&dev.dev_ns)?;
    nl_run(netns, root_ns, {
        let root_gw = root_gw.clone();
        let root_dev = root_dev.clone();
        move |h: Netlink| async move {
            h.ensure_link_deleted(&root_gw).await.ok();
            h.ensure_link_deleted(&root_dev).await.ok();
            h.add_veth(&root_gw, &root_dev).await?;
            h.move_link_to_netns(&root_gw, &gw_ns_fd).await?;
            h.move_link_to_netns(&root_dev, &dev_ns_fd).await?;
            Ok(())
        }
    })
    .await?;

    // DAD already disabled by create_named_netns.
    nl_run(netns, &dev.dev_ns, {
        let d = dev.clone();
        let root_dev = root_dev.clone();
        move |h: Netlink| async move {
            h.set_link_up("lo").await?;
            h.rename_link(&root_dev, &d.ifname).await?;
            h.set_link_up(&d.ifname).await?;
            if let Some(ip4) = d.dev_ip {
                h.add_addr4(&d.ifname, ip4, d.prefix_len).await?;
                if d.is_default {
                    if let Some(gw4) = d.gw_ip {
                        h.add_default_route_v4(gw4).await?;
                    }
                }
            }
            if let Some(ip6) = d.dev_ip_v6 {
                h.add_addr6(&d.ifname, ip6, d.prefix_len_v6).await?;
                if d.is_default {
                    if let Some(gw6) = d.gw_ip_v6 {
                        h.add_default_route_v6(gw6).await?;
                    }
                }
            }
            Ok(())
        }
    })
    .await?;

    nl_run(netns, &dev.gw_ns, {
        let root_gw = root_gw.clone();
        let gw_if = format!("v{}", dev.idx);
        let gw_br = dev.gw_br.clone();
        move |h: Netlink| async move {
            h.rename_link(&root_gw, &gw_if).await?;
            h.set_link_up(&gw_if).await?;
            h.set_master(&gw_if, &gw_br).await?;
            Ok(())
        }
    })
    .await?;

    if let Some(imp) = dev.impair {
        apply_impair_in(netns, &dev.dev_ns, &dev.ifname, imp);
    }
    Ok(())
}

fn add_host(cidr: Ipv4Net, host: u8) -> Result<Ipv4Addr> {
    let octets = cidr.addr().octets();
    if host == 0 || host == 255 {
        bail!("invalid host offset {}", host);
    }
    Ok(Ipv4Addr::new(octets[0], octets[1], octets[2], host))
}

// ─────────────────────────────────────────────
// Netns + process helpers
// ─────────────────────────────────────────────

/// Creates a namespace with DAD disabled.
///
/// IPv6 DAD is disabled immediately so interfaces moved in will inherit
/// `dad_transmits=0` and addresses go straight to "valid" state.
pub(crate) fn create_named_netns(netns: &netns::NetnsManager, name: &str) -> Result<()> {
    netns.create_netns(name)?;
    // Disable DAD before any interfaces are created or moved in.
    netns.run_closure_in(name, || {
        set_sysctl_root("net/ipv6/conf/all/accept_dad", "0").ok();
        set_sysctl_root("net/ipv6/conf/default/accept_dad", "0").ok();
        set_sysctl_root("net/ipv6/conf/all/dad_transmits", "0").ok();
        set_sysctl_root("net/ipv6/conf/default/dad_transmits", "0").ok();
        Ok(())
    })?;
    Ok(())
}

/// Sets a sysctl value in the current namespace (caller must already be in the ns).
pub(crate) fn set_sysctl_root(path: &str, val: &str) -> Result<()> {
    debug!(path = %path, val = %val, "sysctl: set");
    std::fs::write(format!("/proc/sys/{}", path), val)
        .with_context(|| format!("sysctl write {}", path))
}

/// Applies nftables rules (assumes caller is already in the target namespace).
pub(crate) fn run_nft(rules: &str) -> Result<()> {
    let mut child = std::process::Command::new("nft")
        .args(["-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .context("spawn nft")?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(rules.as_bytes())
        .context("write nft stdin")?;
    let st = child.wait().context("wait nft")?;
    if st.success() {
        Ok(())
    } else {
        Err(anyhow!("nft apply failed"))
    }
}

/// Applies nftables rules inside `ns` via the sync worker.
pub(crate) fn run_nft_in(netns: &netns::NetnsManager, ns: &str, rules: &str) -> Result<()> {
    debug!(ns = %ns, rules = %rules, "nft: apply rules");
    let rules = rules.to_string();
    netns.run_closure_in(ns, move || run_nft(&rules))
}

/// Applies home-router NAT rules in `ns` for `mode`.
pub(crate) fn apply_home_nat(
    netns: &netns::NetnsManager,
    ns: &str,
    mode: NatMode,
    _lan_if: &str,
    wan_if: &str,
    wan_ip: Ipv4Addr,
) -> Result<()> {
    let rules = match mode {
        NatMode::DestinationIndependent => {
            // Full-cone NAT (EIM + EIF) via dynamic nftables map.
            //
            // postrouting: record udp sport → src_ip . src_port in @fullcone,
            //   then SNAT to wan_ip.  With `snat to <ip>` (no port range),
            //   Linux keeps the original source port when it is free, so the
            //   pre-SNAT sport == mapped port in virtually all cases.
            // prerouting: any inbound UDP whose dst port is in @fullcone gets
            //   DNATted back to the original internal endpoint.
            //
            // This gives endpoint-independent filtering: once an internal host
            // opens a port, *any* external host can send to that mapped port.
            format!(
                r#"
table ip nat {{
    map fullcone {{
        type inet_service : ipv4_addr . inet_service
        flags dynamic,timeout
        timeout 300s
        size 65536
    }}
    chain prerouting {{
        type nat hook prerouting priority dstnat; policy accept;
        iif "{wan}" meta l4proto udp dnat to udp dport map @fullcone
    }}
    chain postrouting {{
        type nat hook postrouting priority srcnat; policy accept;
        oif "{wan}" meta l4proto udp update @fullcone {{ udp sport timeout 300s : ip saddr . udp sport }}
        oif "{wan}" snat to {ip}
    }}
}}
"#,
                wan = wan_if,
                ip = wan_ip,
            )
        }
        NatMode::DestinationDependent => {
            format!(
                r#"
table ip nat {{
    chain postrouting {{
        type nat hook postrouting priority 100;
        oif "{wan}" masquerade random
    }}
}}
"#,
                wan = wan_if,
            )
        }
        NatMode::None | NatMode::Cgnat => {
            return Ok(());
        }
    };
    run_nft_in(netns, ns, &rules)
}

/// Applies router NAT rules for the configured mode.
pub(crate) fn apply_nat(
    netns: &netns::NetnsManager,
    ns: &str,
    mode: NatMode,
    lan_if: &str,
    wan_if: &str,
    wan_ip: Ipv4Addr,
) -> Result<()> {
    match mode {
        NatMode::None => Ok(()),
        NatMode::Cgnat => apply_isp_cgnat(netns, ns, wan_if),
        NatMode::DestinationIndependent | NatMode::DestinationDependent => {
            apply_home_nat(netns, ns, mode, lan_if, wan_if, wan_ip)
        }
    }
}

/// Applies IPv6 NAT rules in `ns`.
pub(crate) fn apply_nat_v6(
    netns: &netns::NetnsManager,
    ns: &str,
    mode: NatV6Mode,
    wan_if: &str,
    lan_prefix: Ipv6Net,
    wan_prefix: Ipv6Net,
) -> Result<()> {
    match mode {
        NatV6Mode::None => Ok(()),
        NatV6Mode::Nptv6 => {
            let rules = format!(
                r#"
table ip6 nat {{
    chain postrouting {{
        type nat hook postrouting priority 100; policy accept;
        oif "{wan}" snat prefix to {wan_pfx}
    }}
    chain prerouting {{
        type nat hook prerouting priority -100; policy accept;
        iif "{wan}" dnat prefix to {lan_pfx}
    }}
}}
"#,
                wan = wan_if,
                wan_pfx = wan_prefix,
                lan_pfx = lan_prefix,
            );
            run_nft_in(netns, ns, &rules)
        }
        NatV6Mode::Masquerade => {
            let rules = format!(
                r#"
table ip6 nat {{
    chain postrouting {{
        type nat hook postrouting priority 100; policy accept;
        oif "{wan}" masquerade
    }}
    chain forward {{
        type filter hook forward priority 0; policy accept;
        meta l4proto ipv6-icmp accept
    }}
}}
"#,
                wan = wan_if,
            );
            run_nft_in(netns, ns, &rules)
        }
    }
}

/// Applies ISP CGNAT masquerade rules in `ns` on `ix_if`.
pub(crate) fn apply_isp_cgnat(netns: &netns::NetnsManager, ns: &str, ix_if: &str) -> Result<()> {
    let rules = format!(
        r#"
table ip nat {{
    chain postrouting {{
        type nat hook postrouting priority 100;
        oif "{ix}" masquerade
    }}
}}
"#,
        ix = ix_if,
    );
    run_nft_in(netns, ns, &rules)
}

/// Applies an impairment preset or manual limits on `ifname` inside `ns`.
pub(crate) fn apply_impair_in(
    netns: &netns::NetnsManager,
    ns: &str,
    ifname: &str,
    impair: Impair,
) {
    debug!(ns = %ns, ifname = %ifname, impair = ?impair, "tc: apply impairment");
    let limits = match impair {
        Impair::Wifi => qdisc::ImpairLimits {
            rate_kbit: 0,
            loss_pct: 0.0,
            latency_ms: 20,
        },
        Impair::Mobile => qdisc::ImpairLimits {
            rate_kbit: 0,
            loss_pct: 1.0,
            latency_ms: 50,
        },
        Impair::Manual {
            rate,
            loss,
            latency,
        } => qdisc::ImpairLimits {
            rate_kbit: rate,
            loss_pct: loss,
            latency_ms: latency,
        },
    };

    let ifname = ifname.to_string();
    if let Err(e) = netns.run_closure_in(ns, move || qdisc::apply_impair(&ifname, limits)) {
        eprintln!("warn: apply_impair_in({}): {}", ns, e);
    }
}

/// Applies or removes impairment on `ifname` inside `ns`.
pub(crate) fn apply_or_remove_impair(
    netns: &netns::NetnsManager,
    ns: &str,
    ifname: &str,
    impair: Option<Impair>,
) {
    match impair {
        Some(imp) => apply_impair_in(netns, ns, ifname, imp),
        None => {
            let ifname = ifname.to_string();
            let _ = netns.run_closure_in(ns, move || {
                crate::qdisc::remove_qdisc(&ifname);
                Ok(())
            });
        }
    }
}

