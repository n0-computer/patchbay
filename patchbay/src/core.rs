use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::PathBuf,
    sync::{atomic::AtomicU64, Arc},
};

use anyhow::{anyhow, bail, Context, Result};
use ipnet::{Ipv4Net, Ipv6Net};
use tokio_util::sync::CancellationToken;
use tracing::{debug, instrument, Instrument as _};

use crate::{
    netlink::Netlink, netns, qdisc, ConntrackTimeouts, Firewall, IpSupport, Ipv6DadMode,
    Ipv6ProvisioningMode, LinkCondition, Nat, NatConfig, NatFiltering, NatMapping, NatV6Mode,
};

/// Defines static addressing and naming for one lab instance.
#[derive(Clone, Debug)]
pub(crate) struct CoreConfig {
    /// Process-wide sequential lab identifier (from `LAB_COUNTER`).
    pub lab_id: u64,
    /// Process-unique lab prefix used for namespacing resources.
    pub prefix: Arc<str>,
    /// Dedicated lab root namespace name.
    pub root_ns: Arc<str>,
    /// Short tag used to generate bridge interface names (e.g. `"p1230"`).
    pub bridge_tag: Arc<str>,
    /// IX bridge interface name inside the lab root namespace.
    pub ix_br: Arc<str>,
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
    /// Base public downstream IPv6 pool (GUA).
    pub public_cidr_v6: Ipv6Net,
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

/// Parameters needed to (re-)configure NAT on a router.
#[allow(dead_code)]
pub(crate) struct RouterNatParams {
    pub ns: Arc<str>,
    pub lan_if: Arc<str>,
    pub wan_if: Arc<str>,
    pub upstream_ip: Ipv4Addr,
}

/// Parameters needed to (re-)configure IPv6 NAT on a router.
pub(crate) struct RouterNatV6Params {
    pub ns: Arc<str>,
    pub wan_if: String,
    pub lan_prefix: Ipv6Net,
    pub wan_prefix: Ipv6Net,
}

/// Everything needed to wire a newly-added interface after the lock drops.
pub(crate) struct AddIfaceSetup {
    pub iface_build: IfaceBuild,
    pub prefix: Arc<str>,
    pub root_ns: Arc<str>,
    pub mtu: Option<u32>,
}

/// Everything needed to wire a replugged interface after the lock drops.
pub(crate) struct ReplugIfaceSetup {
    pub iface_build: IfaceBuild,
    pub prefix: Arc<str>,
    pub root_ns: Arc<str>,
}

/// One network interface on a device, connected to a router's downstream switch.
#[derive(Clone, Debug)]
pub(crate) struct DeviceIfaceData {
    /// Interface name inside the device namespace (e.g. `"eth0"`).
    pub ifname: Arc<str>,
    /// Switch this interface is attached to.
    pub uplink: NodeId,
    /// Assigned IPv4 address.
    pub ip: Option<Ipv4Addr>,
    /// Assigned IPv6 address.
    pub ip_v6: Option<Ipv6Addr>,
    /// Assigned IPv6 link-local address.
    pub ll_v6: Option<Ipv6Addr>,
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
    pub name: Arc<str>,
    /// Device namespace name.
    pub ns: Arc<str>,
    /// Interfaces in declaration order.
    pub interfaces: Vec<DeviceIfaceData>,
    /// `ifname` of the interface that carries the default route.
    pub default_via: Arc<str>,
    /// Optional MTU for all interfaces.
    pub mtu: Option<u32>,
    /// Per-device operation lock — serializes multi-step mutations.
    pub op: Arc<tokio::sync::Mutex<()>>,
}

impl DeviceData {
    /// Looks up an interface by name.
    pub(crate) fn iface(&self, name: &str) -> Option<&DeviceIfaceData> {
        self.interfaces.iter().find(|i| &*i.ifname == name)
    }

    /// Looks up an interface mutably by name.
    pub(crate) fn iface_mut(&mut self, name: &str) -> Option<&mut DeviceIfaceData> {
        self.interfaces.iter_mut().find(|i| &*i.ifname == name)
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
    pub name: Arc<str>,
    /// Router namespace name.
    pub ns: Arc<str>,
    /// Optional region label.
    pub region: Option<Arc<str>>,
    /// Static router configuration.
    pub cfg: RouterConfig,
    /// Bridge name for the downstream LAN side.
    pub downlink_bridge: Arc<str>,
    /// Uplink switch identifier.
    pub uplink: Option<NodeId>,
    /// Router uplink IPv4 address.
    pub upstream_ip: Option<Ipv4Addr>,
    /// Router uplink IPv6 address.
    pub upstream_ip_v6: Option<Ipv6Addr>,
    /// Router uplink IPv6 link-local address.
    pub upstream_ll_v6: Option<Ipv6Addr>,
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
    /// Downstream bridge IPv6 link-local address.
    pub downstream_ll_v6: Option<Ipv6Addr>,
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
    pub name: Arc<str>,
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
    pub bridge: Option<Arc<str>>,
    pub(crate) next_host: u8,
    pub(crate) next_host_v6: u8,
}

/// Per-interface wiring job collected by `build()`.
#[derive(Clone)]
pub(crate) struct IfaceBuild {
    pub(crate) dev_ns: Arc<str>,
    pub(crate) gw_ns: Arc<str>,
    pub(crate) gw_ip: Option<Ipv4Addr>,
    pub(crate) gw_br: Arc<str>,
    pub(crate) dev_ip: Option<Ipv4Addr>,
    pub(crate) prefix_len: u8,
    pub(crate) gw_ip_v6: Option<Ipv6Addr>,
    pub(crate) dev_ip_v6: Option<Ipv6Addr>,
    pub(crate) gw_ll_v6: Option<Ipv6Addr>,
    pub(crate) dev_ll_v6: Option<Ipv6Addr>,
    pub(crate) prefix_len_v6: u8,
    pub(crate) impair: Option<LinkCondition>,
    pub(crate) ifname: Arc<str>,
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

/// One side of an inter-region link.
pub(crate) struct RegionSide {
    pub ns: Arc<str>,
    pub idx: u8,
    pub ip: Ipv4Addr,
    pub ip6: Ipv6Addr,
    pub sub_v6: Option<Ipv6Net>,
}

/// Setup data returned by [`NetworkCore::prepare_link_regions`].
pub(crate) struct LinkRegionsSetup {
    pub a: RegionSide,
    pub b: RegionSide,
    pub root_ns: Arc<str>,
}

/// Setup data returned by [`NetworkCore::prepare_break_region_link`].
pub(crate) struct BreakRegionSetup {
    pub a_ns: Arc<str>,
    pub b_ns: Arc<str>,
    pub link_key: (Arc<str>, Arc<str>),
    /// IP of intermediate region `m` on the m↔a veth.
    pub m_ip_on_ma: Ipv4Addr,
    /// IP of intermediate region `m` on the m↔b veth.
    pub m_ip_on_mb: Ipv4Addr,
}

/// Setup data returned by [`NetworkCore::prepare_restore_region_link`].
pub(crate) struct RestoreRegionSetup {
    pub a_ns: Arc<str>,
    pub b_ns: Arc<str>,
    pub link_key: (Arc<str>, Arc<str>),
    /// b's IP on the direct a↔b veth (route target from a's side).
    pub b_direct_ip: Ipv4Addr,
    /// a's IP on the direct a↔b veth (route target from b's side).
    pub a_direct_ip: Ipv4Addr,
}

/// Shared lab interior — holds both the topology mutex and the namespace
/// manager. `netns` and `cancel` live here (not behind the mutex) because
/// they are `Arc`-shared and internally synchronized.
pub(crate) struct LabInner {
    pub core: std::sync::Mutex<NetworkCore>,
    pub netns: Arc<netns::NetnsManager>,
    pub cancel: CancellationToken,
    /// Monotonically increasing event counter.
    pub opid: AtomicU64,
    /// Broadcast channel for lab events.
    pub events_tx: tokio::sync::broadcast::Sender<crate::event::LabEvent>,
    /// Human-readable lab label (immutable after construction).
    pub label: Option<Arc<str>>,
    /// Namespace name → node name mapping (for log file naming).
    pub ns_to_name: std::sync::Mutex<HashMap<String, String>>,
    /// Resolved run output directory (e.g. `{base}/{ts}-{label}/`), if outdir was configured.
    pub run_dir: Option<PathBuf>,
    /// IPv6 duplicate address detection behavior.
    pub ipv6_dad_mode: Ipv6DadMode,
    /// IPv6 provisioning behavior.
    pub ipv6_provisioning_mode: Ipv6ProvisioningMode,
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

    pub(crate) fn with_device<R>(&self, id: NodeId, f: impl FnOnce(&DeviceData) -> R) -> Option<R> {
        let core = self.core.lock().unwrap();
        core.device(id).map(f)
    }

    pub(crate) fn with_router<R>(&self, id: NodeId, f: impl FnOnce(&RouterData) -> R) -> Option<R> {
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
    next_public_subnet_v6: u16,
    bridge_counter: u32,
    ix_sw: NodeId,
    devices: HashMap<NodeId, DeviceData>,
    routers: HashMap<NodeId, RouterData>,
    switches: HashMap<NodeId, Switch>,
    nodes_by_name: HashMap<Arc<str>, NodeId>,
    /// Named regions. Key = user-facing name (e.g. "us"), not "region_us".
    pub(crate) regions: HashMap<Arc<str>, RegionInfo>,
    /// Inter-region links. Key = canonically ordered (min, max) region names.
    pub(crate) region_links: HashMap<(Arc<str>, Arc<str>), RegionLinkData>,
    /// Next region index (1–16).
    next_region_idx: u8,
    /// Next /30 offset for inter-region veths in 203.0.113.0/24.
    next_interregion_subnet: u8,
    /// Next /126 offset for inter-region v6 veths in fd11::/48.
    next_interregion_subnet_v6: u8,
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
            next_public_subnet_v6: 1,
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
            next_interregion_subnet_v6: 0,
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

    /// Returns the namespace, interface names, and upstream IP needed for NAT configuration.
    pub(crate) fn router_nat_params(&self, id: NodeId) -> Result<RouterNatParams> {
        let router = self.routers.get(&id).context("unknown router id")?;
        let upstream_ip = router
            .upstream_ip
            .context("router has no upstream ip (not yet built?)")?;
        Ok(RouterNatParams {
            ns: router.ns.clone(),
            lan_if: router.downlink_bridge.clone(),
            wan_if: router.wan_ifname(self.ix_sw).into(),
            upstream_ip,
        })
    }

    /// Stores an updated NAT mode on the router record.
    pub(crate) fn set_router_nat_mode(&mut self, id: NodeId, mode: Nat) -> Result<()> {
        let router = self.routers.get_mut(&id).context("unknown router id")?;
        router.cfg.nat = mode;
        Ok(())
    }

    /// Returns parameters needed to configure IPv6 NAT on a router.
    pub(crate) fn router_nat_v6_params(&self, id: NodeId) -> Result<RouterNatV6Params> {
        let router = self.routers.get(&id).context("router removed")?;
        let wan_if = router.wan_ifname(self.ix_sw()).to_string();
        let lan_prefix = router.downstream_cidr_v6.unwrap_or_else(|| {
            Ipv6Net::new(Ipv6Addr::new(0xfd10, 0, 0, 0, 0, 0, 0, 0), 64).unwrap()
        });
        let up_ip = router.upstream_ip_v6.unwrap_or(Ipv6Addr::UNSPECIFIED);
        let wan_prefix = nptv6_wan_prefix(up_ip, lan_prefix.prefix_len());
        Ok(RouterNatV6Params {
            ns: router.ns.clone(),
            wan_if,
            lan_prefix,
            wan_prefix,
        })
    }

    /// Stores an updated IPv6 NAT mode on the router record.
    pub(crate) fn set_router_nat_v6_mode(&mut self, id: NodeId, mode: NatV6Mode) -> Result<()> {
        let router = self.routers.get_mut(&id).context("router removed")?;
        router.cfg.nat_v6 = mode;
        Ok(())
    }

    /// Stores an updated firewall config on the router record.
    pub(crate) fn set_router_firewall(&mut self, id: NodeId, fw: Firewall) -> Result<()> {
        let router = self.routers.get_mut(&id).context("router removed")?;
        router.cfg.firewall = fw;
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
        region: Option<Arc<str>>,
        ip_support: IpSupport,
        nat_v6: NatV6Mode,
    ) -> NodeId {
        let id = NodeId(self.alloc_id());
        let ns: Arc<str> = format!("lab{}-r{}", self.cfg.lab_id, id.0).into();
        let downlink_bridge: Arc<str> = self.next_bridge_name().into();
        self.nodes_by_name.insert(name.into(), id);
        self.routers.insert(
            id,
            RouterData {
                id,
                name: name.into(),
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
                upstream_ll_v6: None,
                downlink: None,
                downstream_cidr: None,
                downstream_gw: None,
                downstream_cidr_v6: None,
                downstream_gw_v6: None,
                downstream_ll_v6: None,
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
        let ns: Arc<str> = format!("lab{}-d{}", self.cfg.lab_id, id.0).into();
        self.nodes_by_name.insert(name.into(), id);
        self.devices.insert(
            id,
            DeviceData {
                id,
                name: name.into(),
                ns,
                interfaces: vec![],
                default_via: "".into(),
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
            dev.default_via = ifname.into();
        }
        dev.interfaces.push(DeviceIfaceData {
            ifname: ifname.into(),
            uplink: downlink,
            ip: assigned,
            ip_v6: assigned_v6,
            ll_v6: assigned_v6.map(|_| link_local_from_seed(idx)),
            impair,
            idx,
        });
        Ok(assigned)
    }

    /// Registers a new interface on a device and returns everything needed to wire it.
    ///
    /// Validates uniqueness, allocates IPs, snapshots switch/gateway data.
    pub(crate) fn prepare_add_iface(
        &mut self,
        device: NodeId,
        ifname: &str,
        router: NodeId,
        impair: Option<LinkCondition>,
    ) -> Result<AddIfaceSetup> {
        let dev = self
            .device(device)
            .ok_or_else(|| anyhow!("device removed"))?;
        if dev.interfaces.iter().any(|i| &*i.ifname == ifname) {
            bail!("device '{}' already has interface '{}'", dev.name, ifname);
        }
        let dev_ns = dev.ns.clone();
        let mtu = dev.mtu;

        self.add_device_iface(device, ifname, router, impair)?;

        let dev = self.device(device).unwrap();
        let iface = dev
            .interfaces
            .iter()
            .find(|i| &*i.ifname == ifname)
            .unwrap();
        let sw = self
            .switch(iface.uplink)
            .ok_or_else(|| anyhow!("switch missing"))?;
        let gw_router = sw
            .owner_router
            .ok_or_else(|| anyhow!("switch missing owner"))?;
        let gw_br = sw.bridge.clone().unwrap_or_else(|| "br-lan".into());
        let gw_ns = self.router(gw_router).unwrap().ns.clone();

        let iface_build = IfaceBuild {
            dev_ns,
            gw_ns,
            gw_ip: sw.gw,
            gw_br,
            dev_ip: iface.ip,
            prefix_len: sw.cidr.map(|c| c.prefix_len()).unwrap_or(24),
            gw_ip_v6: sw.gw_v6,
            dev_ip_v6: iface.ip_v6,
            gw_ll_v6: self.router(gw_router).and_then(|r| r.downstream_ll_v6),
            dev_ll_v6: iface.ll_v6,
            prefix_len_v6: sw.cidr_v6.map(|c| c.prefix_len()).unwrap_or(64),
            impair,
            ifname: ifname.into(),
            is_default: false,
            idx: iface.idx,
        };
        Ok(AddIfaceSetup {
            iface_build,
            prefix: self.cfg.prefix.clone(),
            root_ns: self.cfg.root_ns.clone(),
            mtu,
        })
    }

    /// Prepares data for replugging an interface to a different router.
    ///
    /// Extracts old interface info, allocates new IPs from target router's pool,
    /// and builds the `IfaceBuild` snapshot.
    pub(crate) fn prepare_replug_iface(
        &mut self,
        device: NodeId,
        ifname: &str,
        to_router: NodeId,
    ) -> Result<ReplugIfaceSetup> {
        let dev = self
            .device(device)
            .ok_or_else(|| anyhow!("device removed"))?
            .clone();
        let iface = dev
            .interfaces
            .iter()
            .find(|i| &*i.ifname == ifname)
            .ok_or_else(|| anyhow!("device '{}' has no interface '{}'", dev.name, ifname))?;
        let old_idx = iface.idx;
        let impair = iface.impair;
        let is_default = ifname == &*dev.default_via;

        let target_router = self
            .router(to_router)
            .ok_or_else(|| anyhow!("unknown target router id"))?
            .clone();
        let downlink_sw = target_router.downlink.ok_or_else(|| {
            anyhow!(
                "target router '{}' has no downstream switch",
                target_router.name
            )
        })?;
        let sw = self
            .switch(downlink_sw)
            .ok_or_else(|| anyhow!("target router's downlink switch missing"))?
            .clone();
        let gw_br = sw.bridge.clone().unwrap_or_else(|| "br-lan".into());
        let new_ip = if sw.cidr.is_some() {
            Some(self.alloc_from_switch(downlink_sw)?)
        } else {
            None
        };
        let new_ip_v6 = if sw.cidr_v6.is_some() {
            Some(self.alloc_from_switch_v6(downlink_sw)?)
        } else {
            None
        };
        let prefix_len = sw.cidr.map(|c| c.prefix_len()).unwrap_or(24);

        let iface_build = IfaceBuild {
            dev_ns: dev.ns.clone(),
            gw_ns: target_router.ns.clone(),
            gw_ip: sw.gw,
            gw_br,
            dev_ip: new_ip,
            prefix_len,
            gw_ip_v6: sw.gw_v6,
            dev_ip_v6: new_ip_v6,
            gw_ll_v6: target_router.downstream_ll_v6,
            dev_ll_v6: new_ip_v6.map(|_| link_local_from_seed(old_idx)),
            prefix_len_v6: sw.cidr_v6.map(|c| c.prefix_len()).unwrap_or(64),
            impair,
            ifname: ifname.into(),
            is_default,
            idx: old_idx,
        };
        Ok(ReplugIfaceSetup {
            iface_build,
            prefix: self.cfg.prefix.clone(),
            root_ns: self.cfg.root_ns.clone(),
        })
    }

    /// Updates interface records after a replug (new uplink, IPs).
    pub(crate) fn finish_replug_iface(
        &mut self,
        device: NodeId,
        ifname: &str,
        to_router: NodeId,
        new_ip: Option<Ipv4Addr>,
        new_ip_v6: Option<Ipv6Addr>,
    ) -> Result<()> {
        let new_uplink = self
            .router(to_router)
            .ok_or_else(|| anyhow!("target router disappeared"))?
            .downlink
            .ok_or_else(|| anyhow!("target router has no downlink"))?;
        let dev = self
            .device_mut(device)
            .ok_or_else(|| anyhow!("device disappeared"))?;
        if let Some(iface) = dev.interfaces.iter_mut().find(|i| &*i.ifname == ifname) {
            iface.uplink = new_uplink;
            iface.ip = new_ip;
            iface.ip_v6 = new_ip_v6;
            iface.ll_v6 = new_ip_v6.map(|_| link_local_from_seed(iface.idx));
        }
        Ok(())
    }

    /// Changes which interface carries the default route.
    pub(crate) fn set_device_default_via(&mut self, device: NodeId, ifname: &str) -> Result<()> {
        let dev = self
            .devices
            .get_mut(&device)
            .ok_or_else(|| anyhow!("unknown device id"))?;
        if !dev.interfaces.iter().any(|i| &*i.ifname == ifname) {
            bail!("interface '{}' not found on device '{}'", ifname, dev.name);
        }
        dev.default_via = ifname.into();
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
        self.nodes_by_name.insert(name.into(), id);
        self.switches.insert(
            id,
            Switch {
                name: name.into(),
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
        router_entry.upstream_ll_v6 = ip_v6.map(|_| link_local_from_seed(router.0 ^ sw.0));
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
                let c6 = match pool {
                    DownstreamPool::Private => self.alloc_private_cidr_v6()?,
                    DownstreamPool::Public => self.alloc_public_cidr_v6()?,
                };
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
        if cidr.is_some() {
            sw_entry.cidr = cidr;
            sw_entry.gw = gw;
        }
        if cidr_v6.is_some() {
            sw_entry.cidr_v6 = cidr_v6;
            sw_entry.gw_v6 = gw_v6;
        }
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
        router_entry.downstream_ll_v6 =
            cidr_v6.map(|_| link_local_from_seed(router.0 ^ sw.0 ^ 0xA5A5));
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

    /// Allocates the next public GUA /64 from the pool (2001:db8:1:N::/64).
    fn alloc_public_cidr_v6(&mut self) -> Result<Ipv6Net> {
        let subnet = self.next_public_subnet_v6;
        self.next_public_subnet_v6 = subnet
            .checked_add(1)
            .ok_or_else(|| anyhow!("public IPv6 subnet pool exhausted"))?;
        let base = self.cfg.public_cidr_v6.addr().segments();
        let cidr = Ipv6Net::new(
            Ipv6Addr::new(base[0], base[1], base[2], subnet, 0, 0, 0, 0),
            64,
        )
        .context("allocate public /64 v6")?;
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

    /// Allocates the next /126 from fd11::/48 for inter-region v6 veths.
    /// Returns (ip_a, ip_b) — the two usable IPs in the /126.
    pub(crate) fn alloc_interregion_ips_v6(&mut self) -> Result<(Ipv6Addr, Ipv6Addr)> {
        let offset = self.next_interregion_subnet_v6;
        if offset >= 64 {
            bail!("inter-region v6 /126 pool exhausted (max 64 links)");
        }
        self.next_interregion_subnet_v6 = offset + 1;
        // fd11::N:1 and fd11::N:2 for each link
        let ip_a = Ipv6Addr::new(0xfd11, 0, 0, offset as u16, 0, 0, 0, 1);
        let ip_b = Ipv6Addr::new(0xfd11, 0, 0, offset as u16, 0, 0, 0, 2);
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

    /// Validates and removes a device, returning its namespace for worker cleanup.
    pub(crate) fn remove_device(&mut self, id: NodeId) -> Result<DeviceData> {
        let dev = self
            .devices
            .remove(&id)
            .ok_or_else(|| anyhow!("unknown device id {:?}", id))?;
        self.nodes_by_name.remove(&dev.name);
        Ok(dev)
    }

    /// Validates and removes a router, returning its namespace for worker cleanup.
    ///
    /// Fails if any devices are still connected to this router's downstream switch.
    pub(crate) fn remove_router(&mut self, id: NodeId) -> Result<RouterData> {
        let router = self
            .routers
            .get(&id)
            .ok_or_else(|| anyhow!("unknown router id {:?}", id))?;
        // Check that no devices are connected to this router's downstream switch.
        if let Some(sw_id) = router.downlink {
            for dev in self.devices.values() {
                for iface in &dev.interfaces {
                    if iface.uplink == sw_id {
                        bail!(
                            "cannot remove router '{}': device '{}' is still connected",
                            router.name,
                            dev.name
                        );
                    }
                }
            }
        }
        self.nodes_by_name.remove(&router.name);
        let router_data = self.routers.remove(&id).unwrap();
        if let Some(sw_id) = router_data.downlink {
            self.switches.remove(&sw_id);
        }
        Ok(router_data)
    }

    /// Validates and removes an interface from a device, returning the device ns.
    ///
    /// Ensures the device keeps at least one interface and fixes `default_via`
    /// if the removed interface was the default.
    pub(crate) fn remove_device_iface(&mut self, dev_id: NodeId, ifname: &str) -> Result<Arc<str>> {
        let dev = self
            .device_mut(dev_id)
            .ok_or_else(|| anyhow!("device removed"))?;
        if dev.interfaces.len() <= 1 {
            bail!(
                "cannot remove '{}': device '{}' must keep at least one interface",
                ifname,
                dev.name
            );
        }
        let pos = dev
            .interfaces
            .iter()
            .position(|i| &*i.ifname == ifname)
            .ok_or_else(|| anyhow!("device '{}' has no interface '{}'", dev.name, ifname))?;
        dev.interfaces.remove(pos);
        if &*dev.default_via == ifname {
            dev.default_via = dev.interfaces[0].ifname.clone();
        }
        Ok(dev.ns.clone())
    }

    /// Allocates a new IP for a device interface, updates the record, returns
    /// `(ns, old_ip, new_ip, prefix_len)`.
    pub(crate) fn renew_device_ip(
        &mut self,
        dev_id: NodeId,
        ifname: &str,
    ) -> Result<(Arc<str>, Ipv4Addr, Ipv4Addr, u8)> {
        let dev = self
            .device(dev_id)
            .ok_or_else(|| anyhow!("device removed"))?;
        let iface = dev
            .iface(ifname)
            .ok_or_else(|| anyhow!("device '{}' has no interface '{}'", dev.name, ifname))?;
        let old_ip = iface
            .ip
            .ok_or_else(|| anyhow!("interface '{}' has no IPv4 address", ifname))?;
        let sw_id = iface.uplink;
        let prefix_len = self
            .switch(sw_id)
            .ok_or_else(|| anyhow!("switch for interface '{}' missing", ifname))?
            .cidr
            .map(|c| c.prefix_len())
            .unwrap_or(24);
        let ns = dev.ns.clone();
        let new_ip = self.alloc_from_switch(sw_id)?;
        let dev = self.device_mut(dev_id).unwrap();
        dev.iface_mut(ifname).unwrap().ip = Some(new_ip);
        Ok((ns, old_ip, new_ip, prefix_len))
    }

    /// Adds a global DNS entry and writes all hosts files.
    pub(crate) fn add_dns_entry(&mut self, name: &str, ip: IpAddr) -> Result<()> {
        self.dns.global.push((name.to_string(), ip));
        let ids: Vec<_> = self.all_device_ids();
        self.dns.write_all_hosts_files(&ids)
    }

    // ── Link target resolution ───────────────────────────────────────

    /// Resolves the `(namespace, ifname)` for impairment between two connected nodes.
    ///
    /// Handles Device↔Router (in either order) and Router↔Router (upstream/downstream).
    pub(crate) fn resolve_link_target(&self, a: NodeId, b: NodeId) -> Result<(Arc<str>, Arc<str>)> {
        // Try Device ↔ Router in both orderings.
        for (dev_id, router_id) in [(a, b), (b, a)] {
            if let (Some(dev), Some(router)) = (self.device(dev_id), self.router(router_id)) {
                let downlink_sw = router
                    .downlink
                    .ok_or_else(|| anyhow!("router '{}' has no downstream switch", router.name))?;
                let iface = dev
                    .interfaces
                    .iter()
                    .find(|i| i.uplink == downlink_sw)
                    .ok_or_else(|| {
                        anyhow!(
                            "device '{}' is not connected to router '{}'",
                            dev.name,
                            router.name
                        )
                    })?;
                return Ok((dev.ns.clone(), iface.ifname.clone()));
            }
        }

        // Router ↔ Router — one must be upstream of the other.
        if let (Some(ra), Some(rb)) = (self.router(a), self.router(b)) {
            let ix_sw = self.ix_sw();
            // Check if b is downstream of a.
            if let Some(a_down) = ra.downlink {
                if rb.uplink == Some(a_down) {
                    return Ok((rb.ns.clone(), rb.wan_ifname(ix_sw).into()));
                }
            }
            // Check if a is downstream of b.
            if let Some(b_down) = rb.downlink {
                if ra.uplink == Some(b_down) {
                    return Ok((ra.ns.clone(), ra.wan_ifname(ix_sw).into()));
                }
            }
            bail!(
                "routers '{}' and '{}' are not directly connected",
                ra.name,
                rb.name
            );
        }

        bail!(
            "nodes {:?} and {:?} are not a connected device-router or router-router pair",
            a,
            b
        );
    }

    // ── Region link helpers ────────────────────────────────────────────

    /// Canonical sorted key for a region-pair link.
    pub(crate) fn region_link_key(a: &str, b: &str) -> (Arc<str>, Arc<str>) {
        if a < b {
            (Arc::from(a), Arc::from(b))
        } else {
            (Arc::from(b), Arc::from(a))
        }
    }

    /// Validates and allocates everything needed to create an inter-region link.
    ///
    /// Caller is responsible for the async network setup after releasing the lock.
    pub(crate) fn prepare_link_regions(
        &mut self,
        a_name: &str,
        b_name: &str,
    ) -> Result<LinkRegionsSetup> {
        let link_key = Self::region_link_key(a_name, b_name);
        if self.region_links.contains_key(&link_key) {
            bail!("regions '{a_name}' and '{b_name}' are already linked");
        }

        let a_info = self
            .regions
            .get(a_name)
            .ok_or_else(|| anyhow!("region '{a_name}' not found"))?
            .clone();
        let b_info = self
            .regions
            .get(b_name)
            .ok_or_else(|| anyhow!("region '{b_name}' not found"))?
            .clone();

        let a_ns = self.router(a_info.router_id).unwrap().ns.clone();
        let b_ns = self.router(b_info.router_id).unwrap().ns.clone();
        let root_ns = self.cfg.root_ns.clone();

        // v6 CIDRs from region sub-switches.
        let a_downlink = self.router(a_info.router_id).unwrap().downlink;
        let b_downlink = self.router(b_info.router_id).unwrap().downlink;
        let a_sub_v6 = a_downlink.and_then(|sw| self.switch(sw).and_then(|s| s.cidr_v6));
        let b_sub_v6 = b_downlink.and_then(|sw| self.switch(sw).and_then(|s| s.cidr_v6));

        let (ip_a, ip_b) = self.alloc_interregion_ips()?;
        let (ip6_a, ip6_b) = self.alloc_interregion_ips_v6()?;

        // Store IPs in sorted key order: ip_a belongs to link_key.0, ip_b to link_key.1.
        let (stored_ip_a, stored_ip_b) = if a_name < b_name {
            (ip_a, ip_b)
        } else {
            (ip_b, ip_a)
        };
        self.region_links.insert(
            link_key,
            RegionLinkData {
                ip_a: stored_ip_a,
                ip_b: stored_ip_b,
                broken: false,
            },
        );

        Ok(LinkRegionsSetup {
            a: RegionSide {
                ns: a_ns,
                idx: a_info.idx,
                ip: ip_a,
                ip6: ip6_a,
                sub_v6: a_sub_v6,
            },
            b: RegionSide {
                ns: b_ns,
                idx: b_info.idx,
                ip: ip_b,
                ip6: ip6_b,
                sub_v6: b_sub_v6,
            },
            root_ns,
        })
    }

    /// Validates and resolves the intermediate region for breaking a region link.
    ///
    /// Does **not** mark the link as broken — caller must do that after the
    /// route-replace commands succeed.
    pub(crate) fn prepare_break_region_link(
        &self,
        a_name: &str,
        b_name: &str,
    ) -> Result<BreakRegionSetup> {
        let link_key = Self::region_link_key(a_name, b_name);
        let link = self
            .region_links
            .get(&link_key)
            .ok_or_else(|| anyhow!("no link between '{a_name}' and '{b_name}'"))?;
        if link.broken {
            bail!("link between '{a_name}' and '{b_name}' is already broken");
        }

        let a_rid = self
            .regions
            .get(a_name)
            .ok_or_else(|| anyhow!("region '{a_name}' not found"))?
            .router_id;
        let b_rid = self
            .regions
            .get(b_name)
            .ok_or_else(|| anyhow!("region '{b_name}' not found"))?
            .router_id;

        // Find intermediate region m with non-broken links to both a and b.
        let m_name = self
            .regions
            .keys()
            .find(|name| {
                let n: &str = name;
                if n == a_name || n == b_name {
                    return false;
                }
                let key_ma = Self::region_link_key(n, a_name);
                let key_mb = Self::region_link_key(n, b_name);
                let link_ma = self.region_links.get(&key_ma);
                let link_mb = self.region_links.get(&key_mb);
                matches!((link_ma, link_mb), (Some(la), Some(lb)) if !la.broken && !lb.broken)
            })
            .cloned()
            .ok_or_else(|| {
                anyhow!("no intermediate region found to reroute '{a_name}'↔'{b_name}'")
            })?;

        // Get the veth IPs for m↔a and m↔b links.
        let key_ma = Self::region_link_key(&m_name, a_name);
        let link_ma = self.region_links.get(&key_ma).unwrap();
        let m_ip_on_ma = if &*key_ma.0 == a_name {
            link_ma.ip_b
        } else {
            link_ma.ip_a
        };

        let key_mb = Self::region_link_key(&m_name, b_name);
        let link_mb = self.region_links.get(&key_mb).unwrap();
        let m_ip_on_mb = if &*key_mb.0 == b_name {
            link_mb.ip_b
        } else {
            link_mb.ip_a
        };

        let a_ns = self.router(a_rid).unwrap().ns.clone();
        let b_ns = self.router(b_rid).unwrap().ns.clone();

        Ok(BreakRegionSetup {
            a_ns,
            b_ns,
            link_key,
            m_ip_on_ma,
            m_ip_on_mb,
        })
    }

    /// Validates and resolves IPs for restoring a broken region link.
    ///
    /// Does **not** mark the link as restored — caller must do that after the
    /// route-replace commands succeed.
    pub(crate) fn prepare_restore_region_link(
        &self,
        a_name: &str,
        b_name: &str,
    ) -> Result<RestoreRegionSetup> {
        let link_key = Self::region_link_key(a_name, b_name);
        let link = self
            .region_links
            .get(&link_key)
            .ok_or_else(|| anyhow!("no link between '{a_name}' and '{b_name}'"))?;
        if !link.broken {
            bail!("link between '{a_name}' and '{b_name}' is not broken");
        }

        let a_rid = self
            .regions
            .get(a_name)
            .ok_or_else(|| anyhow!("region '{a_name}' not found"))?
            .router_id;
        let b_rid = self
            .regions
            .get(b_name)
            .ok_or_else(|| anyhow!("region '{b_name}' not found"))?
            .router_id;
        let a_ns = self.router(a_rid).unwrap().ns.clone();
        let b_ns = self.router(b_rid).unwrap().ns.clone();

        // link_key.0 is the alphabetically-first region name.
        // link.ip_a belongs to link_key.0, link.ip_b to link_key.1.
        let (b_direct_ip, a_direct_ip) = if &*link_key.0 == a_name {
            // a == link_key.0 → b's direct IP is link.ip_b, a's is link.ip_a
            (link.ip_b, link.ip_a)
        } else {
            (link.ip_a, link.ip_b)
        };

        Ok(RestoreRegionSetup {
            a_ns,
            b_ns,
            link_key,
            b_direct_ip,
            a_direct_ip,
        })
    }

    /// Marks a region link as broken or restored.
    pub(crate) fn set_region_link_broken(&mut self, link_key: &(Arc<str>, Arc<str>), broken: bool) {
        if let Some(link) = self.region_links.get_mut(link_key) {
            link.broken = broken;
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
    create_named_netns(netns, &root_ns, None, None, Ipv6DadMode::Disabled)?;

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
#[allow(clippy::type_complexity)]
pub(crate) struct RouterSetupData {
    pub router: RouterData,
    pub root_ns: Arc<str>,
    pub prefix: Arc<str>,
    pub ix_sw: NodeId,
    pub ix_br: Arc<str>,
    pub ix_gw: Ipv4Addr,
    pub ix_cidr_prefix: u8,
    /// For sub-routers: upstream switch info.
    pub upstream_owner_ns: Option<Arc<str>>,
    pub upstream_bridge: Option<Arc<str>>,
    pub upstream_gw: Option<Ipv4Addr>,
    pub upstream_cidr_prefix: Option<u8>,
    /// For IX-level public routers: downstream CIDR for return route.
    pub return_route: Option<(Ipv4Addr, u8, Ipv4Addr)>,
    /// Downlink bridge name (if router has downstream switch) and optional v4 address.
    pub downlink_bridge: Option<(Arc<str>, Option<(Ipv4Addr, u8)>)>,
    // ── IPv6 fields ──
    pub ix_gw_v6: Option<Ipv6Addr>,
    pub ix_cidr_v6_prefix: Option<u8>,
    pub upstream_gw_v6: Option<Ipv6Addr>,
    pub upstream_cidr_prefix_v6: Option<u8>,
    pub return_route_v6: Option<(Ipv6Addr, u8, Ipv6Addr)>,
    pub downlink_bridge_v6: Option<(Ipv6Addr, u8)>,
    /// For sub-routers with NatV6Mode::None: route in the parent router's ns
    /// for the sub-router's downstream v6 subnet via the sub-router's WAN IP.
    pub parent_route_v6: Option<(Arc<str>, Ipv6Addr, u8, Ipv6Addr)>, // (parent_ns, net, prefix, via)
    /// For sub-routers with public downstream in a region: route in the parent
    /// (region) router's ns for this sub-router's downstream /24 via its WAN IP.
    pub parent_route_v4: Option<(Arc<str>, Ipv4Addr, u8, Ipv4Addr)>, // (parent_ns, net, prefix, via)
    /// Cancellation token for long-running background tasks (NAT64 translator).
    pub cancel: CancellationToken,
    /// IPv6 DAD behavior for created namespaces.
    pub dad_mode: Ipv6DadMode,
    /// IPv6 provisioning behavior.
    pub provisioning_mode: Ipv6ProvisioningMode,
}

/// Sets up a single router's namespaces, links, and NAT. No lock held.
#[instrument(name = "router", skip_all, fields(id = data.router.id.0))]
pub(crate) async fn setup_router_async(
    netns: &Arc<netns::NetnsManager>,
    data: &RouterSetupData,
) -> Result<()> {
    match data.provisioning_mode {
        Ipv6ProvisioningMode::Static | Ipv6ProvisioningMode::RaDriven => {}
    }
    let router = &data.router;
    let id = router.id;
    debug!(name = %router.name, ns = %router.ns, "router: setup");

    let log_prefix = format!("{}.{}", crate::consts::KIND_ROUTER, router.name);
    create_named_netns(netns, &router.ns, None, Some(log_prefix), data.dad_mode)?;

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
                    if let Some(ll6) = d.router.upstream_ll_v6 {
                        h.add_addr6(&ns_if, ll6, 64).await?;
                    }
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
            if let (Some(up_v6), Some((dl_gw_v6, dl_prefix))) =
                (router.upstream_ip_v6, data.downlink_bridge_v6)
            {
                let lan_pfx = Ipv6Net::new(dl_gw_v6, dl_prefix)
                    .unwrap_or_else(|_| Ipv6Net::new(dl_gw_v6, 64).unwrap());
                let wan_pfx = nptv6_wan_prefix(up_v6, lan_pfx.prefix_len());
                debug!(nat_v6 = ?router.cfg.nat_v6, %wan_pfx, %lan_pfx, "router: apply NAT v6");
                apply_nat_v6(
                    netns,
                    &router.ns,
                    router.cfg.nat_v6,
                    &ns_if,
                    lan_pfx,
                    wan_pfx,
                )
                .await?;

                // Add return route in root ns for the WAN prefix so translated
                // traffic can be routed back to this router.
                let root_ns = data.root_ns.clone();
                nl_run(netns, &root_ns, move |h: Netlink| async move {
                    h.add_route_v6(wan_pfx.addr(), wan_pfx.prefix_len(), up_v6)
                        .await
                        .ok();
                    Ok(())
                })
                .await
                .ok();
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
                    if let Some(ll6) = d.router.upstream_ll_v6 {
                        h.add_addr6(&wan_if, ll6, 64).await?;
                    }
                    if g6.is_unicast_link_local() {
                        h.add_default_route_v6_scoped(&wan_if, g6).await?;
                    } else {
                        h.add_default_route_v6(g6).await?;
                    }
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
            if let (Some(up_v6), Some((dl_gw_v6, dl_prefix))) =
                (router.upstream_ip_v6, data.downlink_bridge_v6)
            {
                let lan_pfx = Ipv6Net::new(dl_gw_v6, dl_prefix)
                    .unwrap_or_else(|_| Ipv6Net::new(dl_gw_v6, 64).unwrap());
                let wan_pfx = nptv6_wan_prefix(up_v6, lan_pfx.prefix_len());
                debug!(nat_v6 = ?router.cfg.nat_v6, %wan_pfx, %lan_pfx, "router: apply NAT v6");
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
        let downlink_ll_v6 = data.router.downstream_ll_v6;
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
                if let Some(ll6) = downlink_ll_v6 {
                    h.add_addr6(&br, ll6, 64).await?;
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
    let fw_wan = if router.uplink == Some(data.ix_sw) {
        "ix"
    } else {
        "wan"
    };
    apply_firewall(netns, &router.ns, &router.cfg.firewall, fw_wan).await?;

    // NAT64: create TUN device, routes, nft masquerade, and start translator.
    if router.cfg.nat_v6 == NatV6Mode::Nat64 {
        setup_nat64(netns, &router.ns, fw_wan, &data.cancel).await?;
    }

    // RA worker scaffold for RA-driven mode.
    if data.provisioning_mode == Ipv6ProvisioningMode::RaDriven && router.cfg.ip_support.has_v6() {
        spawn_ra_worker(netns, &router.ns, data.cancel.clone())?;
    }

    Ok(())
}

fn spawn_ra_worker(
    netns: &Arc<netns::NetnsManager>,
    ns: &str,
    cancel: CancellationToken,
) -> Result<()> {
    let rt = netns.rt_handle_for(ns)?;
    let ns = ns.to_string();
    rt.spawn(async move {
        let interval = tokio::time::Duration::from_secs(30);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(interval) => {
                    tracing::trace!(ns = %ns, "ra-worker: tick");
                }
            }
        }
        tracing::trace!(ns = %ns, "ra-worker: stopped");
    });
    Ok(())
}

/// Sets up NAT64 in the router namespace:
/// 1. Creates TUN device `nat64`
/// 2. Assigns the NAT64 IPv4 pool address
/// 3. Adds routes for the NAT64 prefix and pool
/// 4. Adds nftables masquerade for outbound v4 from pool
/// 5. Spawns the async SIIT translation loop
async fn setup_nat64(
    netns: &Arc<netns::NetnsManager>,
    ns: &str,
    wan_if: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    use crate::nat64;

    let v4_pool = nat64::NAT64_V4_POOL;
    let tun_name = "nat64";

    // Create TUN device inside the router namespace.
    let tun_fd = netns.run_closure_in(ns, || nat64::create_tun(tun_name))?;

    debug!(ns = %ns, "nat64: TUN created, configuring routes");

    // Configure the TUN: bring up, add routes.
    // We don't assign an IP to the TUN — that would create a "local" route
    // that prevents return traffic from reaching the TUN. Instead we add
    // device routes for both the NAT64 prefix (v6→v4) and the pool (v4→v6).
    nl_run(netns, ns, {
        let pool = v4_pool;
        move |h: Netlink| async move {
            h.set_link_up(tun_name).await?;
            // Route the NAT64 well-known prefix (64:ff9b::/96) to the TUN device.
            h.add_route_v6_dev(
                Ipv6Addr::new(0x0064, 0xff9b, 0, 0, 0, 0, 0, 0),
                96,
                tun_name,
            )
            .await?;
            // Route the pool address to the TUN for return traffic (v4→v6).
            // After conntrack demasquerades, dst=192.0.2.64 needs to go to TUN.
            h.add_route_v4_dev(pool, 32, tun_name).await?;
            Ok(())
        }
    })
    .await?;

    // Masquerade outbound IPv4 traffic from the pool address on the WAN interface.
    // This gives the translated packets a real source IP (the router's WAN IP)
    // and handles port allocation via conntrack.
    let rules = format!(
        r#"
table ip nat64 {{
    chain postrouting {{
        type nat hook postrouting priority 100; policy accept;
        oif "{wan}" ip saddr {pool} masquerade
    }}
}}
"#,
        wan = wan_if,
        pool = v4_pool,
    );
    run_nft_in(netns, ns, &rules).await?;

    // Spawn the translation loop on the router namespace's tokio runtime.
    let rt = netns.rt_handle_for(ns)?;
    let cancel = cancel.clone();
    rt.spawn(async move {
        if let Err(e) = nat64::run_nat64_loop(tun_fd, v4_pool, cancel).await {
            tracing::error!("nat64: translation loop error: {e:#}");
        }
    });

    debug!(ns = %ns, "nat64: setup complete");
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

/// Generates nftables rules from a [`FirewallConfig`].
///
/// Uses a separate `table inet fw` at priority 10 to avoid conflicts with the
/// NAT filter table (`ip filter` at priority 0). Handles both inbound blocking
/// and outbound port restrictions in a single unified chain.
fn generate_firewall_rules(cfg: &crate::firewall::FirewallConfig, wan_if: &str) -> String {
    use crate::firewall::PortPolicy;

    let mut rules = String::new();
    rules.push_str("table inet fw {\n");
    rules.push_str("    chain forward {\n");
    rules.push_str("        type filter hook forward priority 10; policy accept;\n");
    rules.push_str("        ct state established,related accept\n");

    // Block unsolicited inbound on the WAN interface (RFC 6092).
    if cfg.block_inbound {
        rules.push_str(&format!("        iif \"{}\" drop\n", wan_if));
    }

    // Outbound TCP policy.
    match &cfg.outbound_tcp {
        PortPolicy::AllowAll => {}
        PortPolicy::Allow(ports) if !ports.is_empty() => {
            let ports: Vec<String> = ports.iter().map(|p| p.to_string()).collect();
            rules.push_str(&format!(
                "        tcp dport {{ {} }} accept\n",
                ports.join(", ")
            ));
            rules.push_str("        meta l4proto tcp drop\n");
        }
        // Allow(empty) or BlockAll → drop all TCP.
        _ => {
            rules.push_str("        meta l4proto tcp drop\n");
        }
    }

    // Outbound UDP policy.
    match &cfg.outbound_udp {
        PortPolicy::AllowAll => {}
        PortPolicy::Allow(ports) if !ports.is_empty() => {
            let ports: Vec<String> = ports.iter().map(|p| p.to_string()).collect();
            rules.push_str(&format!(
                "        udp dport {{ {} }} accept\n",
                ports.join(", ")
            ));
            rules.push_str("        meta l4proto udp drop\n");
        }
        // Allow(empty) or BlockAll → drop all UDP.
        _ => {
            rules.push_str("        meta l4proto udp drop\n");
        }
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
    wan_if: &str,
) -> Result<()> {
    match firewall.to_config() {
        None => Ok(()),
        Some(cfg) => {
            let rules = generate_firewall_rules(&cfg, wan_if);
            run_nft_in(netns, ns, &rules).await
        }
    }
}

/// Removes firewall rules by flushing the `inet fw` table.
pub(crate) async fn remove_firewall(netns: &netns::NetnsManager, ns: &str) -> Result<()> {
    // Flush and delete; ignore errors (table may not exist).
    run_nft_in(netns, ns, "delete table inet fw\n").await.ok();
    // Also clean up legacy `ip fw` table from older configurations.
    run_nft_in(netns, ns, "delete table ip fw\n").await.ok();
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
    dad_mode: Ipv6DadMode,
    provisioning_mode: Ipv6ProvisioningMode,
) -> Result<()> {
    match provisioning_mode {
        Ipv6ProvisioningMode::Static | Ipv6ProvisioningMode::RaDriven => {}
    }
    debug!(name = %dev.name, ns = %dev.ns, "device: setup");
    let log_prefix = format!("{}.{}", crate::consts::KIND_DEVICE, dev.name);
    create_named_netns(netns, &dev.ns, dns_overlay, Some(log_prefix), dad_mode)?;

    for iface in ifaces {
        wire_iface_async(netns, prefix, root_ns, iface).await?;
    }

    // Apply MTU on all device interfaces if configured.
    if let Some(mtu) = dev.mtu {
        let dev_ns = dev.ns.clone();
        let ifnames: Vec<Arc<str>> = dev.interfaces.iter().map(|i| i.ifname.clone()).collect();
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
                if let Some(ll6) = d.dev_ll_v6 {
                    h.add_addr6(&d.ifname, ll6, 64).await?;
                }
                if d.is_default {
                    if let Some(gw6) = d.gw_ip_v6 {
                        h.add_default_route_v6(gw6).await?;
                    } else if let Some(gw_ll6) = d.gw_ll_v6 {
                        h.add_default_route_v6_scoped(&d.ifname, gw_ll6).await?;
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

fn link_local_from_seed(seed: u64) -> Ipv6Addr {
    let a = ((seed >> 48) & 0xffff) as u16;
    let b = ((seed >> 32) & 0xffff) as u16;
    let c = ((seed >> 16) & 0xffff) as u16;
    let d = (seed & 0xffff) as u16;
    Ipv6Addr::new(0xfe80, 0, 0, 0, a, b, c, d)
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
    log_prefix: Option<String>,
    dad_mode: Ipv6DadMode,
) -> Result<()> {
    netns.create_netns(name, dns_overlay, log_prefix)?;
    if dad_mode == Ipv6DadMode::Disabled {
        // Disable DAD before any interfaces are created or moved in.
        netns.run_closure_in(name, || {
            set_sysctl_root("net/ipv6/conf/all/accept_dad", "0").ok();
            set_sysctl_root("net/ipv6/conf/default/accept_dad", "0").ok();
            set_sysctl_root("net/ipv6/conf/all/dad_transmits", "0").ok();
            set_sysctl_root("net/ipv6/conf/default/dad_transmits", "0").ok();
            Ok(())
        })?;
    }
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

/// Derives a unique WAN /64 for NPTv6 from a router's upstream IP.
///
/// For an IX-level router with upstream IP `2001:db8::11` on IX CIDR `2001:db8::/64`,
/// this produces `2001:db8:0:11::/64` — a unique /64 outside the IX on-link range
/// that matches the LAN-side /64 prefix length required by NPTv6.
///
/// For sub-routers where the upstream is already on a /64 parent LAN, we use the host
/// part of the upstream IP to derive a /64 within the parent's subnet space.
pub(crate) fn nptv6_wan_prefix(upstream_ip: Ipv6Addr, lan_prefix_len: u8) -> Ipv6Net {
    // Place the host portion (last segment) of the upstream IP into segment 3,
    // zeroing segments 4-7 to form a clean /64 network prefix.
    let seg = upstream_ip.segments();
    let host = seg[7];
    let wan_net = Ipv6Addr::new(seg[0], seg[1], seg[2], host, 0, 0, 0, 0);
    Ipv6Net::new(wan_net, lan_prefix_len).unwrap_or_else(|_| Ipv6Net::new(wan_net, 64).unwrap())
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
            // Match only packets within the LAN/WAN prefix ranges so that
            // NDP, ICMPv6, and other traffic to/from the router's own IX
            // address is not inadvertently translated.
            let rules = format!(
                r#"
table ip6 nat {{
    chain postrouting {{
        type nat hook postrouting priority 100; policy accept;
        oif "{wan}" ip6 saddr {lan_pfx} snat prefix to {wan_pfx}
    }}
    chain prerouting {{
        type nat hook prerouting priority -100; policy accept;
        iif "{wan}" ip6 daddr {wan_pfx} dnat prefix to {lan_pfx}
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
        NatV6Mode::Nat64 => {
            // NAT64 is handled separately in setup_nat64 — the apply_nat_v6
            // call is a no-op for this mode (the SIIT translator and nft
            // masquerade are set up after the router's downlink bridge exists).
            Ok(())
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
