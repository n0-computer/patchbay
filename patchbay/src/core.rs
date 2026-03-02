use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::PathBuf,
    sync::Arc,
};

use anyhow::{anyhow, bail, Context, Result};
use ipnet::{Ipv4Net, Ipv6Net};
use tokio_util::sync::CancellationToken;
use tracing::{debug, instrument, Instrument as _};

use crate::{
    netlink::Netlink, netns, qdisc, ConntrackTimeouts, Firewall, IpSupport, LinkCondition, Nat,
    NatConfig, NatFiltering, NatMapping, NatV6Mode,
};

/// Defines static addressing and naming for one lab instance.
#[derive(Clone, Debug)]
pub(crate) struct CoreConfig {
    /// Process-wide sequential lab identifier (from `LAB_COUNTER`).
    pub lab_id: u64,
    /// Process-unique lab prefix used for namespacing resources.
    pub prefix: String,
    /// Dedicated lab root namespace name.
    pub root_ns: String,
    /// Short tag used to generate bridge interface names (e.g. `"p1230"`).
    pub bridge_tag: String,
    /// IX bridge interface name inside the lab root namespace.
    pub ix_br: String,
    /// IX gateway IPv4 address.
    pub ix_gw: Ipv4Addr,
    /// IX subnet CIDR.
    pub ix_cidr: Ipv4Net,
    /// Base private downstream address pool.
    pub private_cidr: Ipv4Net,
    /// Base public downstream address pool.
    pub public_cidr: Ipv4Net,
    /// IX gateway IPv6 address.
    pub ix_gw_v6: Ipv6Addr,
    /// IX IPv6 subnet CIDR.
    pub ix_cidr_v6: Ipv6Net,
    /// Base private downstream IPv6 pool (ULA).
    pub private_cidr_v6: Ipv6Net,
    /// Tracing span for this lab; used to parent worker thread spans.
    pub span: tracing::Span,
}

/// Opaque identifier for a node (device or router) in the topology graph.
///
/// Obtained from [`Device::id`](crate::Device::id), [`Router::id`](crate::Router::id),
/// or builder methods.
/// Cheaply copyable and usable as a hash-map key.
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
    /// Selects router NAT behavior. Use [`Nat::Custom`] for a custom config.
    pub nat: Nat,
    /// Selects which pool to allocate downstream subnets from.
    pub downstream_pool: DownstreamPool,
    /// Selects router IPv6 NAT behavior.
    pub nat_v6: NatV6Mode,
    /// Selects which IP address families this router supports.
    pub ip_support: IpSupport,
    /// Optional MTU for WAN and LAN interfaces.
    pub mtu: Option<u32>,
    /// Whether to block ICMP "fragmentation needed" messages (PMTU blackhole).
    pub block_icmp_frag_needed: bool,
    /// Firewall preset for the router's forward chain.
    pub firewall: Firewall,
}

impl RouterConfig {
    /// Returns the effective NAT config by expanding the preset (or returning
    /// the custom config). Returns `None` for `Nat::None` and `Nat::Cgnat`.
    pub(crate) fn effective_nat_config(&self) -> Option<NatConfig> {
        self.nat.to_config()
    }
}

/// One network interface on a device, connected to a router's downstream switch.
#[derive(Clone, Debug)]
pub(crate) struct DeviceIfaceData {
    /// Interface name inside the device namespace (e.g. `"eth0"`).
    pub ifname: String,
    /// Switch this interface is attached to.
    pub uplink: NodeId,
    /// Assigned IPv4 address.
    pub ip: Option<Ipv4Addr>,
    /// Assigned IPv6 address.
    pub ip_v6: Option<Ipv6Addr>,
    /// Optional link impairment applied via `tc netem`.
    pub impair: Option<LinkCondition>,
    /// Unique index used to name the root-namespace veth ends.
    pub(crate) idx: u64,
}

/// A network endpoint with one or more interfaces.
#[derive(Clone, Debug)]
pub(crate) struct DeviceData {
    /// Identifies the device node.
    pub id: NodeId,
    /// Device name.
    pub name: String,
    /// Device namespace name.
    pub ns: String,
    /// Interfaces in declaration order.
    pub interfaces: Vec<DeviceIfaceData>,
    /// `ifname` of the interface that carries the default route.
    pub default_via: String,
    /// Optional MTU for all interfaces.
    pub mtu: Option<u32>,
    /// Per-device operation lock — serializes multi-step mutations.
    pub op: Arc<tokio::sync::Mutex<()>>,
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
    /// Router name.
    pub name: String,
    /// Router namespace name.
    pub ns: String,
    /// Optional region label.
    pub region: Option<String>,
    /// Static router configuration.
    pub cfg: RouterConfig,
    /// Bridge name for the downstream LAN side.
    pub downlink_bridge: String,
    /// Uplink switch identifier.
    pub uplink: Option<NodeId>,
    /// Router uplink IPv4 address.
    pub upstream_ip: Option<Ipv4Addr>,
    /// Router uplink IPv6 address.
    pub upstream_ip_v6: Option<Ipv6Addr>,
    /// Downstream switch identifier.
    pub downlink: Option<NodeId>,
    /// Downstream subnet CIDR.
    pub downstream_cidr: Option<Ipv4Net>,
    /// Downstream gateway address.
    pub downstream_gw: Option<Ipv4Addr>,
    /// Downstream IPv6 subnet CIDR.
    pub downstream_cidr_v6: Option<Ipv6Net>,
    /// Downstream IPv6 gateway address.
    pub downstream_gw_v6: Option<Ipv6Addr>,
    /// Per-router operation lock — serializes multi-step mutations.
    pub op: Arc<tokio::sync::Mutex<()>>,
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
    /// Switch name.
    pub name: String,
    /// IPv4 subnet, if assigned.
    pub cidr: Option<Ipv4Net>,
    /// IPv4 gateway address, if assigned.
    pub gw: Option<Ipv4Addr>,
    /// IPv6 subnet, if assigned.
    pub cidr_v6: Option<Ipv6Net>,
    /// IPv6 gateway address, if assigned.
    pub gw_v6: Option<Ipv6Addr>,
    /// Owning router for managed downstream switches.
    pub owner_router: Option<NodeId>,
    /// Backing bridge name.
    pub bridge: Option<String>,
    pub(crate) next_host: u8,
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
    pub(crate) impair: Option<LinkCondition>,
    pub(crate) ifname: String,
    pub(crate) is_default: bool,
    pub(crate) idx: u64,
}

/// Per-device DNS host entries for `/etc/hosts` overlay.
///
/// Each device gets a persistent hosts file at `<hosts_dir>/<node_id>.hosts`.
/// A shared `resolv.conf` lives at `<hosts_dir>/resolv.conf`. Files are created
/// at init with default content and bind-mounted into worker threads at startup.
/// Subsequent `dns_entry()` / `set_nameserver()` calls just rewrite the file —
/// glibc picks up changes via mtime check on next `getaddrinfo()`.
pub(crate) struct DnsEntries {
    /// Lab-wide entries applied to every device.
    pub global: Vec<(String, IpAddr)>,
    /// Per-device entries, keyed by device `NodeId`.
    pub per_device: HashMap<NodeId, Vec<(String, IpAddr)>>,
    /// Optional nameserver IP for `/etc/resolv.conf` overlay.
    pub nameserver: Option<IpAddr>,
    /// Directory for generated hosts/resolv files.
    pub hosts_dir: PathBuf,
}

impl DnsEntries {
    fn new(prefix: &str) -> Result<Self> {
        let hosts_dir = std::env::temp_dir().join(format!("patchbay-{prefix}-hosts"));
        std::fs::create_dir_all(&hosts_dir).context("create hosts dir")?;
        // Create initial resolv.conf with localhost as fallback nameserver.
        // glibc and hickory-resolver need at least one nameserver entry.
        let resolv_path = hosts_dir.join("resolv.conf");
        std::fs::write(
            &resolv_path,
            "# generated by patchbay\nnameserver 127.0.0.53\n",
        )
        .context("write initial resolv.conf")?;
        Ok(Self {
            global: Vec::new(),
            per_device: HashMap::new(),
            nameserver: None,
            hosts_dir,
        })
    }

    /// Returns the path to the hosts file for a device. Always valid after
    /// `ensure_hosts_file()` has been called for this device.
    pub(crate) fn hosts_path_for(&self, device_id: NodeId) -> PathBuf {
        self.hosts_dir.join(format!("{}.hosts", device_id.0))
    }

    /// Returns the path to the shared resolv.conf overlay.
    pub(crate) fn resolv_path(&self) -> PathBuf {
        self.hosts_dir.join("resolv.conf")
    }

    /// Creates the hosts file for a device with default content if it doesn't exist.
    pub(crate) fn ensure_hosts_file(&self, device_id: NodeId) -> Result<()> {
        let path = self.hosts_path_for(device_id);
        if !path.exists() {
            self.write_hosts_file(device_id)?;
        }
        Ok(())
    }

    /// Writes (or rewrites) the hosts file for a single device.
    pub(crate) fn write_hosts_file(&self, device_id: NodeId) -> Result<()> {
        let path = self.hosts_path_for(device_id);
        let mut buf =
            String::from("# generated by patchbay\n127.0.0.1\tlocalhost\n::1\tlocalhost\n");
        for (name, ip) in &self.global {
            buf.push_str(&format!("{ip}\t{name}\n"));
        }
        if let Some(entries) = self.per_device.get(&device_id) {
            for (name, ip) in entries {
                buf.push_str(&format!("{ip}\t{name}\n"));
            }
        }
        std::fs::write(&path, buf.as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    /// Rewrites hosts files for all given devices.
    pub(crate) fn write_all_hosts_files(&self, device_ids: &[NodeId]) -> Result<()> {
        for &id in device_ids {
            self.write_hosts_file(id)?;
        }
        Ok(())
    }

    /// Writes the resolv.conf file with the current nameserver setting.
    pub(crate) fn write_resolv_conf(&self) -> Result<()> {
        let path = self.resolv_path();
        let content = match self.nameserver {
            Some(ip) => format!("# generated by patchbay\nnameserver {ip}\n"),
            None => "# generated by patchbay\n".to_string(),
        };
        std::fs::write(&path, content.as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    /// Resolves a name using global + per-device entries. Returns the first match.
    pub(crate) fn resolve(&self, device_id: Option<NodeId>, name: &str) -> Option<IpAddr> {
        if let Some(id) = device_id {
            if let Some(entries) = self.per_device.get(&id) {
                for (n, ip) in entries {
                    if n == name {
                        return Some(*ip);
                    }
                }
            }
        }
        for (n, ip) in &self.global {
            if n == name {
                return Some(*ip);
            }
        }
        None
    }
}

/// Per-region metadata stored in `NetworkCore`.
#[derive(Clone, Debug)]
pub(crate) struct RegionInfo {
    /// Region index (1–16). Determines the /20 address block.
    pub idx: u8,
    /// NodeId of the region's internal router.
    pub router_id: NodeId,
    /// Next downstream /24 offset within the region's /20 (1, 2, ... up to 15).
    pub next_downstream: u8,
}

/// Stored data for one inter-region link.
#[derive(Clone, Debug)]
pub(crate) struct RegionLinkData {
    /// IP of A's end of the /30.
    pub ip_a: Ipv4Addr,
    /// IP of B's end of the /30.
    pub ip_b: Ipv4Addr,
    /// Whether this link is currently broken.
    pub broken: bool,
}

/// Shared lab interior — holds both the topology mutex and the namespace
/// manager. `netns` and `cancel` live here (not behind the mutex) because
/// they are `Arc`-shared and internally synchronized.
pub(crate) struct LabInner {
    pub core: std::sync::Mutex<NetworkCore>,
    pub netns: Arc<netns::NetnsManager>,
    pub cancel: CancellationToken,
}

impl Drop for LabInner {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl LabInner {
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

    // ── with() helpers ──────────────────────────────────────────────────

    pub(crate) fn with_device<R>(
        &self,
        id: NodeId,
        f: impl FnOnce(&DeviceData) -> R,
    ) -> Option<R> {
        let core = self.core.lock().unwrap();
        core.device(id).map(f)
    }

    pub(crate) fn with_router<R>(
        &self,
        id: NodeId,
        f: impl FnOnce(&RouterData) -> R,
    ) -> Option<R> {
        let core = self.core.lock().unwrap();
        core.router(id).map(f)
    }
}

/// Stores mutable topology state and build-time allocators.
pub(crate) struct NetworkCore {
    pub(crate) cfg: CoreConfig,
    /// DNS host entries for `/etc/hosts` overlay in spawned commands.
    pub(crate) dns: DnsEntries,
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
    /// Named regions. Key = user-facing name (e.g. "us"), not "region_us".
    pub(crate) regions: HashMap<String, RegionInfo>,
    /// Inter-region links. Key = canonically ordered (min, max) region names.
    pub(crate) region_links: HashMap<(String, String), RegionLinkData>,
    /// Next region index (1–16).
    next_region_idx: u8,
    /// Next /30 offset for inter-region veths in 203.0.113.0/24.
    next_interregion_subnet: u8,
}

impl Drop for NetworkCore {
    fn drop(&mut self) {
        // Clean up generated hosts files.
        let _ = std::fs::remove_dir_all(&self.dns.hosts_dir);
    }
}

impl NetworkCore {
    /// Constructs a new topology core and pre-creates the IX switch.
    pub(crate) fn new(cfg: CoreConfig) -> Result<Self> {
        let dns = DnsEntries::new(&cfg.prefix).context("create DNS entries dir")?;
        let mut core = Self {
            cfg,
            dns,
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
            regions: HashMap::new(),
            region_links: HashMap::new(),
            next_region_idx: 1,
            next_interregion_subnet: 0,
        };
        let ix_sw = core.add_switch(
            "ix",
            Some(core.cfg.ix_cidr),
            Some(core.cfg.ix_gw),
            Some(core.cfg.ix_cidr_v6),
            Some(core.cfg.ix_gw_v6),
        );
        core.ix_sw = ix_sw;
        Ok(core)
    }

    fn next_bridge_name(&mut self) -> String {
        let name = format!("br-{}-{}", self.cfg.bridge_tag, self.bridge_counter);
        self.bridge_counter = self.bridge_counter.saturating_add(1);
        name
    }

    /// Returns the IX gateway address.
    pub(crate) fn ix_gw(&self) -> Ipv4Addr {
        self.cfg.ix_gw
    }

    /// Allocates the next low-end IX host address.
    pub(crate) fn alloc_ix_ip_low(&mut self) -> Result<Ipv4Addr> {
        let host = self.next_ix_low;
        if host == 0 || host == 255 {
            bail!("IX IPv4 address pool exhausted");
        }
        self.next_ix_low = host + 1;
        let o = self.cfg.ix_gw.octets();
        Ok(Ipv4Addr::new(o[0], o[1], o[2], host))
    }

    /// Returns the IX switch identifier.
    pub(crate) fn ix_sw(&self) -> NodeId {
        self.ix_sw
    }

    /// Returns the lab root namespace name.
    pub(crate) fn root_ns(&self) -> &str {
        &self.cfg.root_ns
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

    /// Returns mutable switch data for `id`.
    pub(crate) fn switch_mut(&mut self, id: NodeId) -> Option<&mut Switch> {
        self.switches.get_mut(&id)
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
    pub(crate) fn router_nat_params(
        &self,
        id: NodeId,
    ) -> Result<(String, String, String, Ipv4Addr)> {
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
    pub(crate) fn set_router_nat_mode(&mut self, id: NodeId, mode: Nat) -> Result<()> {
        let router = self.routers.get_mut(&id).context("unknown router id")?;
        router.cfg.nat = mode;
        Ok(())
    }

    /// Returns the router's effective NAT config and WAN parameters.
    pub(crate) fn router_effective_cfg(&self, id: NodeId) -> Result<RouterConfig> {
        let router = self.routers.get(&id).context("unknown router id")?;
        Ok(router.cfg.clone())
    }

    /// Adds a router node and returns its identifier.
    ///
    /// The namespace name and downstream bridge name are generated internally.
    pub(crate) fn add_router(
        &mut self,
        name: &str,
        nat: Nat,
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
                    mtu: None,
                    block_icmp_frag_needed: false,
                    firewall: Firewall::None,
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
                op: Arc::new(tokio::sync::Mutex::new(())),
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
                mtu: None,
                op: Arc::new(tokio::sync::Mutex::new(())),
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
        impair: Option<LinkCondition>,
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
    ///
    /// If `override_cidr` is `Some`, that subnet is used instead of
    /// auto-allocating from the router's downstream pool.
    pub(crate) fn connect_router_downlink(
        &mut self,
        router: NodeId,
        sw: NodeId,
        override_cidr: Option<Ipv4Net>,
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
            } else if let Some(cidr) = override_cidr {
                let gw = add_host(cidr, 1)?;
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
        self.next_private_subnet = subnet
            .checked_add(1)
            .ok_or_else(|| anyhow!("private IPv4 subnet pool exhausted"))?;
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
        self.next_public_subnet = subnet
            .checked_add(1)
            .ok_or_else(|| anyhow!("public IPv4 subnet pool exhausted"))?;
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
        sw_entry.next_host = sw_entry
            .next_host
            .checked_add(1)
            .ok_or_else(|| anyhow!("switch '{}' host pool exhausted", sw_entry.name))?;
        Ok(ip)
    }

    /// Allocates the next IX IPv6 address (2001:db8::N).
    pub(crate) fn alloc_ix_ip_v6_low(&mut self) -> Result<Ipv6Addr> {
        let host = self.next_ix_low_v6;
        if host == u16::MAX {
            bail!("IX IPv6 address pool exhausted");
        }
        self.next_ix_low_v6 = host + 1;
        let seg = self.cfg.ix_gw_v6.segments();
        Ok(Ipv6Addr::new(
            seg[0], seg[1], seg[2], seg[3], seg[4], seg[5], seg[6], host,
        ))
    }

    /// Allocates the next private /64 from the ULA pool (fd10:0:N::/64).
    pub(crate) fn alloc_private_cidr_v6(&mut self) -> Result<Ipv6Net> {
        let subnet = self.next_private_subnet_v6;
        self.next_private_subnet_v6 = subnet
            .checked_add(1)
            .ok_or_else(|| anyhow!("private IPv6 subnet pool exhausted"))?;
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
        sw_entry.next_host_v6 = sw_entry
            .next_host_v6
            .checked_add(1)
            .ok_or_else(|| anyhow!("switch '{}' v6 host pool exhausted", sw_entry.name))?;
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

    /// Allocates the next region index (1–16).
    pub(crate) fn alloc_region_idx(&mut self) -> Result<u8> {
        let idx = self.next_region_idx;
        if idx > 16 {
            bail!("region pool exhausted (max 16 regions)");
        }
        self.next_region_idx = idx + 1;
        Ok(idx)
    }

    /// Allocates the next public downstream /24 from a region's /20 pool.
    /// Region `idx` → downstream starts at 198.18.{idx*16 + 1}.0/24.
    pub(crate) fn alloc_region_public_cidr(&mut self, region_name: &str) -> Result<Ipv4Net> {
        let region = self
            .regions
            .get_mut(region_name)
            .ok_or_else(|| anyhow!("unknown region '{}'", region_name))?;
        let offset = region.next_downstream;
        if offset > 15 {
            bail!(
                "region '{}' public downstream pool exhausted (max 15 /24s)",
                region_name
            );
        }
        region.next_downstream = offset + 1;
        let third = region.idx as u16 * 16 + offset as u16;
        let cidr = Ipv4Net::new(Ipv4Addr::new(198, 18, third as u8, 0), 24)
            .context("allocate region public /24")?;
        Ok(cidr)
    }

    /// Allocates the next /30 from 203.0.113.0/24 for inter-region veths.
    /// Returns (ip_a, ip_b) — the two usable IPs in the /30.
    pub(crate) fn alloc_interregion_ips(&mut self) -> Result<(Ipv4Addr, Ipv4Addr)> {
        let offset = self.next_interregion_subnet;
        // Each /30 = 4 IPs, max offset = 63 (64 * 4 = 256, but .0 and .255 are unusable,
        // and we need network + broadcast per /30, so: offsets 0..63 give base 0,4,8,...252)
        if offset >= 64 {
            bail!("inter-region /30 pool exhausted (max 64 links)");
        }
        self.next_interregion_subnet = offset + 1;
        let base = offset as u16 * 4;
        let ip_a = Ipv4Addr::new(203, 0, 113, (base + 1) as u8);
        let ip_b = Ipv4Addr::new(203, 0, 113, (base + 2) as u8);
        Ok((ip_a, ip_b))
    }

    /// Returns an iterator over all devices in the topology.
    pub(crate) fn all_devices(&self) -> impl Iterator<Item = &DeviceData> {
        self.devices.values()
    }

    /// Returns all device node ids.
    pub(crate) fn all_device_ids(&self) -> Vec<NodeId> {
        self.devices.keys().copied().collect()
    }

    /// Returns all router node ids.
    pub(crate) fn all_router_ids(&self) -> Vec<NodeId> {
        self.routers.keys().copied().collect()
    }

    /// Removes a device from the internal data structures.
    pub(crate) fn remove_device(&mut self, id: NodeId) {
        if let Some(dev) = self.devices.remove(&id) {
            self.nodes_by_name.remove(&dev.name);
        }
    }

    /// Removes a router and its downstream switch from the internal data structures.
    pub(crate) fn remove_router(&mut self, id: NodeId) {
        if let Some(router) = self.routers.remove(&id) {
            self.nodes_by_name.remove(&router.name);
            if let Some(sw_id) = router.downlink {
                self.switches.remove(&sw_id);
            }
        }
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
    create_named_netns(netns, &root_ns, None)?;

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
    /// For sub-routers with public downstream in a region: route in the parent
    /// (region) router's ns for this sub-router's downstream /24 via its WAN IP.
    pub parent_route_v4: Option<(String, Ipv4Addr, u8, Ipv4Addr)>, // (parent_ns, net, prefix, via)
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

    create_named_netns(netns, &router.ns, None)?;

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
            apply_nat_for_router(netns, &router.ns, &router.cfg, &ns_if, upstream_ip4).await?;
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
                apply_nat_v6(
                    netns,
                    &router.ns,
                    router.cfg.nat_v6,
                    &ns_if,
                    lan_pfx,
                    wan_pfx,
                )
                .await?;
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
            apply_nat_for_router(netns, &router.ns, &router.cfg, &wan_if, upstream_ip4).await?;
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
                apply_nat_v6(
                    netns,
                    &router.ns,
                    router.cfg.nat_v6,
                    &wan_if,
                    lan_pfx,
                    wan_pfx,
                )
                .await?;
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

    // Return route in lab root for public downstreams (v4 + v6).
    if data.return_route.is_some() || data.return_route_v6.is_some() {
        let rr4 = data.return_route;
        let rr6 = data.return_route_v6;
        nl_run(netns, &data.root_ns, move |h: Netlink| async move {
            if let Some((net, prefix_len, via)) = rr4 {
                h.add_route_v4(net, prefix_len, via).await.ok();
            }
            if let Some((net6, prefix6, via6)) = rr6 {
                h.add_route_v6(net6, prefix6, via6).await.ok();
            }
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

    // Route in parent (region) router's ns for sub-router's public downstream.
    if let Some((ref parent_ns, net4, prefix4, via4)) = data.parent_route_v4 {
        nl_run(netns, parent_ns, move |h: Netlink| async move {
            h.add_route_v4(net4, prefix4, via4).await.ok();
            Ok(())
        })
        .await
        .ok();
    }

    // Apply MTU on WAN and LAN interfaces if configured.
    if let Some(mtu) = router.cfg.mtu {
        let wan_if = if router.uplink == Some(data.ix_sw) {
            "ix"
        } else {
            "wan"
        };
        let br = data.downlink_bridge.as_ref().map(|(br, _)| br.clone());
        nl_run(netns, &router.ns, move |h: Netlink| async move {
            h.set_mtu(wan_if, mtu).await?;
            if let Some(br) = br {
                h.set_mtu(&br, mtu).await?;
            }
            Ok(())
        })
        .await?;
    }

    // Block ICMP "fragmentation needed" if configured (PMTU blackhole).
    if router.cfg.block_icmp_frag_needed {
        apply_icmp_frag_block(netns, &router.ns).await?;
    }

    // Apply firewall rules if configured.
    apply_firewall(netns, &router.ns, &router.cfg.firewall).await?;

    Ok(())
}

/// Applies nftables rules to drop ICMP "fragmentation needed" messages,
/// simulating a PMTU blackhole middlebox.
async fn apply_icmp_frag_block(netns: &netns::NetnsManager, ns: &str) -> Result<()> {
    run_nft_in(
        netns,
        ns,
        r#"
table ip filter {
    chain forward {
        type filter hook forward priority 0; policy accept;
        icmp type destination-unreachable icmp code frag-needed drop
    }
}
"#,
    )
    .await
}

/// Generates nftables rules for a [`FirewallConfig`].
///
/// Uses a separate `table ip fw` at priority 10 to avoid conflicts with the
/// NAT filter table (`ip filter` at priority 0).
fn generate_firewall_rules(cfg: &crate::lab::FirewallConfig) -> String {
    let mut rules = String::new();
    rules.push_str("table ip fw {\n");
    rules.push_str("    chain forward {\n");
    rules.push_str("        type filter hook forward priority 10; policy accept;\n");
    rules.push_str("        ct state established,related accept\n");

    // Allow specific TCP ports.
    if !cfg.allow_tcp.is_empty() {
        let ports: Vec<String> = cfg.allow_tcp.iter().map(|p| p.to_string()).collect();
        rules.push_str(&format!(
            "        tcp dport {{ {} }} accept\n",
            ports.join(", ")
        ));
    }

    // Allow specific UDP ports.
    if !cfg.allow_udp.is_empty() {
        let ports: Vec<String> = cfg.allow_udp.iter().map(|p| p.to_string()).collect();
        rules.push_str(&format!(
            "        udp dport {{ {} }} accept\n",
            ports.join(", ")
        ));
    }

    // Block remaining UDP if configured.
    if cfg.block_udp {
        rules.push_str("        ip protocol udp drop\n");
    }

    // Block remaining TCP if configured.
    if cfg.block_tcp {
        rules.push_str("        ip protocol tcp drop\n");
    }

    rules.push_str("    }\n");
    rules.push_str("}\n");
    rules
}

/// Applies firewall rules for a router. No-op for [`Firewall::None`].
pub(crate) async fn apply_firewall(
    netns: &netns::NetnsManager,
    ns: &str,
    firewall: &Firewall,
) -> Result<()> {
    match firewall.to_config() {
        None => Ok(()),
        Some(cfg) => {
            let rules = generate_firewall_rules(&cfg);
            run_nft_in(netns, ns, &rules).await
        }
    }
}

/// Removes firewall rules by flushing the `ip fw` table.
pub(crate) async fn remove_firewall(netns: &netns::NetnsManager, ns: &str) -> Result<()> {
    // Flush and delete; ignore errors (table may not exist).
    let rules = "delete table ip fw\n";
    run_nft_in(netns, ns, rules).await.ok();
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
    dns_overlay: Option<netns::DnsOverlay>,
) -> Result<()> {
    debug!(name = %dev.name, ns = %dev.ns, "device: setup");
    create_named_netns(netns, &dev.ns, dns_overlay)?;

    for iface in ifaces {
        wire_iface_async(netns, prefix, root_ns, iface).await?;
    }

    // Apply MTU on all device interfaces if configured.
    if let Some(mtu) = dev.mtu {
        let dev_ns = dev.ns.clone();
        let ifnames: Vec<String> = dev.interfaces.iter().map(|i| i.ifname.clone()).collect();
        nl_run(netns, &dev_ns, move |h: Netlink| async move {
            for ifname in &ifnames {
                h.set_mtu(ifname, mtu).await?;
            }
            Ok(())
        })
        .await?;
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
        apply_impair_in(netns, &dev.dev_ns, &dev.ifname, imp).await;
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

/// Creates a namespace with optional DNS overlay and disables IPv6 DAD.
///
/// IPv6 DAD is disabled immediately so interfaces moved in will inherit
/// `dad_transmits=0` and addresses go straight to "valid" state.
pub(crate) fn create_named_netns(
    netns: &netns::NetnsManager,
    name: &str,
    dns_overlay: Option<netns::DnsOverlay>,
) -> Result<()> {
    netns.create_netns(name, dns_overlay)?;
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
async fn run_nft(rules: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("nft")
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
        .await
        .context("write nft stdin")?;
    let st = child.wait().await.context("wait nft")?;
    if st.success() {
        Ok(())
    } else {
        Err(anyhow!("nft apply failed"))
    }
}

/// Applies nftables rules inside `ns` on the namespace's async worker.
pub(crate) async fn run_nft_in(netns: &netns::NetnsManager, ns: &str, rules: &str) -> Result<()> {
    debug!(ns = %ns, rules = %rules, "nft: apply rules");
    let rules = rules.to_string();
    let rt = netns.rt_handle_for(ns)?;
    rt.spawn(async move { run_nft(&rules).await })
        .await
        .context("nft task panicked")?
}

/// Generates nftables rules for a [`NatConfig`].
///
/// EIM uses a dynamic fullcone map to preserve source ports across destinations.
/// EDM uses `masquerade random` for per-flow port randomization.
/// EIF adds unconditional fullcone DNAT in prerouting.
/// APDF adds a forward filter that only allows established/related flows.
fn generate_nat_rules(cfg: &NatConfig, wan_if: &str, wan_ip: Ipv4Addr) -> String {
    let use_fullcone_map = cfg.mapping == NatMapping::EndpointIndependent;
    let hairpin = cfg.hairpin;

    let map_decl = if use_fullcone_map {
        r#"    map fullcone {
        type inet_service : ipv4_addr . inet_service
        flags dynamic,timeout
        timeout 300s
        size 65536
    }"#
    } else {
        ""
    };

    // Prerouting: for EIM, DNAT via fullcone map so inbound UDP reaches
    // the correct internal host.  For EDM, an empty prerouting chain is
    // still needed for conntrack reverse-NAT on reply packets.
    //
    // With hairpin: match on `ip daddr <wan_ip>` instead of `iif "<wan>"` so
    // packets from the LAN side destined to the router's public IP also get
    // DNAT'd.
    let prerouting_rules = if use_fullcone_map {
        if hairpin {
            format!(
                r#"        ip daddr {ip} meta l4proto udp dnat to udp dport map @fullcone"#,
                ip = wan_ip
            )
        } else {
            format!(
                r#"        iif "{wan}" meta l4proto udp dnat to udp dport map @fullcone"#,
                wan = wan_if
            )
        }
    } else if hairpin {
        // EDM + hairpin: redirect traffic destined for the WAN IP back.
        format!(r#"        ip daddr {ip} redirect"#, ip = wan_ip,)
    } else {
        String::new()
    };

    // Postrouting: EIM uses snat + fullcone map update. EDM uses masquerade random.
    // With hairpin: masquerade DNAT'd packets so the return path goes through
    // the router (otherwise the LAN peer replies directly, confusing conntrack).
    let hairpin_masq = if hairpin {
        "        ct status dnat masquerade\n".to_string()
    } else {
        String::new()
    };

    let postrouting_rules = if use_fullcone_map {
        format!(
            r#"{hairpin}        oif "{wan}" meta l4proto udp update @fullcone {{ udp sport timeout 300s : ip saddr . udp sport }}
        oif "{wan}" snat to {ip}"#,
            hairpin = hairpin_masq,
            wan = wan_if,
            ip = wan_ip,
        )
    } else {
        format!(
            r#"{hairpin}        oif "{wan}" masquerade random"#,
            hairpin = hairpin_masq,
            wan = wan_if,
        )
    };

    let postrouting_priority = if use_fullcone_map { "srcnat" } else { "100" };

    // APDF filter: only forward inbound packets matching existing conntrack flows.
    let filter_table = if cfg.filtering == NatFiltering::AddressAndPortDependent {
        format!(
            r#"
table ip filter {{
    chain forward {{
        type filter hook forward priority 0; policy accept;
        iif "{wan}" ct state established,related accept
        iif "{wan}" drop
    }}
}}"#,
            wan = wan_if
        )
    } else {
        String::new()
    };

    format!(
        r#"
table ip nat {{
{map_decl}
    chain prerouting {{
        type nat hook prerouting priority dstnat; policy accept;
{prerouting_rules}
    }}
    chain postrouting {{
        type nat hook postrouting priority {postrouting_priority}; policy accept;
{postrouting_rules}
    }}
}}
{filter_table}
"#
    )
}

/// Applies NAT rules from a [`NatConfig`] in the given namespace.
pub(crate) async fn apply_nat_config(
    netns: &netns::NetnsManager,
    ns: &str,
    cfg: &NatConfig,
    wan_if: &str,
    wan_ip: Ipv4Addr,
) -> Result<()> {
    let rules = generate_nat_rules(cfg, wan_if, wan_ip);
    run_nft_in(netns, ns, &rules).await?;
    apply_conntrack_timeouts_from_config(netns, ns, &cfg.timeouts)
}

/// Configures conntrack timeouts from a [`ConntrackTimeouts`].
fn apply_conntrack_timeouts_from_config(
    netns: &netns::NetnsManager,
    ns: &str,
    t: &ConntrackTimeouts,
) -> Result<()> {
    let (udp, udp_stream, tcp_est) = (t.udp, t.udp_stream, t.tcp_established);
    netns.run_closure_in(ns, move || {
        set_sysctl_root("net/netfilter/nf_conntrack_udp_timeout", &udp.to_string())?;
        set_sysctl_root(
            "net/netfilter/nf_conntrack_udp_timeout_stream",
            &udp_stream.to_string(),
        )?;
        set_sysctl_root(
            "net/netfilter/nf_conntrack_tcp_timeout_established",
            &tcp_est.to_string(),
        )?;
        Ok(())
    })
}

/// Applies router NAT rules for the configured mode.
///
/// Uses the effective NAT config from the router's [`Nat`] variant.
/// Otherwise expands the [`Nat`] preset via [`Nat::to_config`].
/// CGNAT and None are handled separately.
pub(crate) async fn apply_nat_for_router(
    netns: &netns::NetnsManager,
    ns: &str,
    router_cfg: &RouterConfig,
    wan_if: &str,
    wan_ip: Ipv4Addr,
) -> Result<()> {
    // CGNAT is applied at the ISP level, not via NatConfig.
    if router_cfg.nat == Nat::Cgnat {
        return apply_isp_cgnat(netns, ns, wan_if).await;
    }
    match router_cfg.effective_nat_config() {
        None => Ok(()),
        Some(cfg) => apply_nat_config(netns, ns, &cfg, wan_if, wan_ip).await,
    }
}

/// Applies IPv6 NAT rules in `ns`.
pub(crate) async fn apply_nat_v6(
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
            run_nft_in(netns, ns, &rules).await
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
            run_nft_in(netns, ns, &rules).await
        }
    }
}

/// Applies ISP CGNAT masquerade rules in `ns` on `ix_if`.
pub(crate) async fn apply_isp_cgnat(
    netns: &netns::NetnsManager,
    ns: &str,
    ix_if: &str,
) -> Result<()> {
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
    run_nft_in(netns, ns, &rules).await
}

/// Applies an impairment preset or manual limits on `ifname` inside `ns`.
pub(crate) async fn apply_impair_in(
    netns: &netns::NetnsManager,
    ns: &str,
    ifname: &str,
    impair: LinkCondition,
) {
    debug!(ns = %ns, ifname = %ifname, impair = ?impair, "tc: apply impairment");
    let limits = impair.to_limits();
    let ifname = ifname.to_string();
    let rt = match netns.rt_handle_for(ns) {
        Ok(rt) => rt,
        Err(e) => {
            tracing::warn!(ns = %ns, error = %e, "apply_impair_in: no rt handle");
            return;
        }
    };
    if let Err(e) = rt
        .spawn(async move { qdisc::apply_impair(&ifname, limits).await })
        .await
    {
        tracing::warn!(ns = %ns, error = %e, "apply_impair_in failed");
    }
}

/// Applies or removes impairment on `ifname` inside `ns`.
pub(crate) async fn apply_or_remove_impair(
    netns: &netns::NetnsManager,
    ns: &str,
    ifname: &str,
    impair: Option<LinkCondition>,
) {
    match impair {
        Some(imp) => apply_impair_in(netns, ns, ifname, imp).await,
        None => {
            let ifname = ifname.to_string();
            let rt = match netns.rt_handle_for(ns) {
                Ok(rt) => rt,
                Err(_) => return,
            };
            let _ = rt
                .spawn(async move { qdisc::remove_qdisc(&ifname).await })
                .await;
        }
    }
}
