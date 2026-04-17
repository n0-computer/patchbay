use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

use anyhow::{anyhow, bail, Context, Result};
use ipnet::{Ipv4Net, Ipv6Net};

use crate::{
    nft::nptv6_wan_prefix,
    wiring::{add_host, link_local_from_seed, seed2, seed3},
    Firewall, IpSupport, Ipv6ProvisioningMode, LinkCondition, NatConfig, NatV6Mode,
};

pub(crate) const RA_DEFAULT_ENABLED: bool = true;
pub(crate) const RA_DEFAULT_INTERVAL_SECS: u64 = 30;
pub(crate) const RA_DEFAULT_LIFETIME_SECS: u64 = 1800;

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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, derive_more::Display)]
#[display("{_0}")]
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
    /// NAT configuration. `None` disables NAT entirely.
    pub nat: Option<NatConfig>,
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
    /// Whether this router emits Router Advertisements in RA-driven mode.
    pub ra_enabled: bool,
    /// Router Advertisement interval in seconds.
    pub ra_interval_secs: u64,
    /// Router Advertisement lifetime in seconds.
    pub ra_lifetime_secs: u64,
}

impl RouterConfig {
    /// Returns the effective NAT config, or `None` if NAT is disabled.
    pub(crate) fn effective_nat_config(&self) -> Option<NatConfig> {
        self.nat
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

pub(crate) struct DeviceDefaultV6RouteTarget {
    pub ns: Arc<str>,
    pub ifname: Arc<str>,
}

pub(crate) struct DownlinkV6Gateways {
    pub global_v6: Option<Ipv6Addr>,
    pub link_local_v6: Option<Ipv6Addr>,
}

/// How a device interface is attached to the network.
#[derive(Clone, Copy, Debug)]
pub(crate) enum IfaceAttachment {
    /// Connected to a router's downstream bridge via a veth pair.
    Routed(NodeId),
    /// Linux dummy device with no bridge attachment.
    Dummy,
}

/// One network interface on a device.
#[derive(Clone, Debug)]
pub(crate) struct DeviceIfaceData {
    /// Interface name inside the device namespace (e.g. `"eth0"`).
    pub ifname: Arc<str>,
    /// How this interface is attached to the network.
    pub attachment: IfaceAttachment,
    /// Assigned IPv4 address.
    pub ip: Option<Ipv4Addr>,
    /// Assigned IPv6 address.
    pub ip_v6: Option<Ipv6Addr>,
    /// Assigned IPv6 link-local address.
    pub ll_v6: Option<Ipv6Addr>,
    /// Egress impairment (device-side veth or dummy device).
    pub egress: Option<LinkCondition>,
    /// Ingress impairment (bridge-side veth). Always `None` for dummy interfaces.
    pub ingress: Option<LinkCondition>,
    /// If `true`, the interface should be created in link-down state.
    pub start_down: bool,
    /// IPv4 prefix length (for dummy interfaces with explicit addr).
    pub(crate) prefix_len: Option<u8>,
    /// IPv6 prefix length (for dummy interfaces with explicit addr).
    pub(crate) prefix_len_v6: Option<u8>,
    /// Unique index used to name the root-namespace veth ends.
    pub(crate) idx: u64,
}

impl DeviceIfaceData {
    /// Returns the switch NodeId if this is a routed interface.
    pub(crate) fn uplink(&self) -> Option<NodeId> {
        match self.attachment {
            IfaceAttachment::Routed(id) => Some(id),
            IfaceAttachment::Dummy => None,
        }
    }

    /// Returns true if this is a dummy (non-routed) interface.
    pub(crate) fn is_dummy(&self) -> bool {
        matches!(self.attachment, IfaceAttachment::Dummy)
    }
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
    /// Optional per-device IPv6 provisioning override.
    pub provisioning_mode: Option<Ipv6ProvisioningMode>,
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
    /// Runtime RA settings consumed by the RA worker.
    pub ra_runtime: Arc<RaRuntimeCfg>,
    /// Per-router operation lock — serializes multi-step mutations.
    pub op: Arc<tokio::sync::Mutex<()>>,
}

impl RouterData {
    pub(crate) fn ra_default_enabled(&self) -> bool {
        self.cfg.ra_enabled && self.cfg.ra_lifetime_secs > 0
    }

    pub(crate) fn active_downstream_ll_v6(&self) -> Option<Ipv6Addr> {
        if self.ra_default_enabled() {
            self.downstream_ll_v6
        } else {
            None
        }
    }
}

/// Runtime-adjustable RA configuration shared between the router handle and
/// the RA worker task. Atomics use `Relaxed` ordering because the `Notify`
/// wakeup provides the happens-before edge that ensures the worker sees
/// updated values after being notified.
#[derive(Debug)]
pub(crate) struct RaRuntimeCfg {
    enabled: AtomicBool,
    interval_secs: AtomicU64,
    lifetime_secs: AtomicU64,
    changed: tokio::sync::Notify,
}

impl RaRuntimeCfg {
    pub(crate) fn new(enabled: bool, interval_secs: u64, lifetime_secs: u64) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            interval_secs: AtomicU64::new(interval_secs),
            lifetime_secs: AtomicU64::new(lifetime_secs),
            changed: tokio::sync::Notify::new(),
        }
    }

    pub(crate) fn load(&self) -> (bool, u64, u64) {
        (
            self.enabled.load(Ordering::Relaxed),
            self.interval_secs.load(Ordering::Relaxed),
            self.lifetime_secs.load(Ordering::Relaxed),
        )
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
        self.changed.notify_waiters();
    }

    pub(crate) fn set_interval_secs(&self, secs: u64) {
        self.interval_secs.store(secs.max(1), Ordering::Relaxed);
        self.changed.notify_waiters();
    }

    pub(crate) fn set_lifetime_secs(&self, secs: u64) {
        self.lifetime_secs.store(secs, Ordering::Relaxed);
        self.changed.notify_waiters();
    }

    pub(crate) fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.changed.notified()
    }
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
    pub(crate) egress: Option<LinkCondition>,
    pub(crate) ingress: Option<LinkCondition>,
    pub(crate) dummy: bool,
    pub(crate) start_down: bool,
    pub(crate) ifname: Arc<str>,
    pub(crate) is_default: bool,
    pub(crate) idx: u64,
}

/// Per-device DNS host entries for `/etc/hosts` overlay.
///
/// Per-device `/etc/hosts` overlay and shared `resolv.conf`.
///
/// Each device gets a hosts file at `<hosts_dir>/<node_id>.hosts` and a shared
/// `resolv.conf` at `<hosts_dir>/resolv.conf`, bind-mounted into worker threads.
/// Lab-wide DNS records live in [`DnsServer`](crate::dns_server::DnsServer).
pub(crate) struct DnsOverlayDir {
    /// Nameservers for `/etc/resolv.conf` overlay.
    pub nameservers: Vec<IpAddr>,
    /// Directory for generated hosts/resolv files.
    pub hosts_dir: PathBuf,
}

impl DnsOverlayDir {
    fn new(prefix: &str) -> Result<Self> {
        let hosts_dir = std::env::temp_dir().join(format!("patchbay-{prefix}-hosts"));
        std::fs::create_dir_all(&hosts_dir).context("create hosts dir")?;
        std::fs::write(
            hosts_dir.join("resolv.conf"),
            "# generated by patchbay\nnameserver 127.0.0.53\n",
        )
        .context("write initial resolv.conf")?;
        Ok(Self {
            nameservers: Vec::new(),
            hosts_dir,
        })
    }

    pub(crate) fn hosts_path_for(&self, device_id: NodeId) -> PathBuf {
        self.hosts_dir.join(format!("{}.hosts", device_id.0))
    }

    pub(crate) fn resolv_path(&self) -> PathBuf {
        self.hosts_dir.join("resolv.conf")
    }

    /// Creates the default hosts file for a device if it doesn't exist.
    pub(crate) fn ensure_hosts_file(&self, device_id: NodeId) -> Result<()> {
        let path = self.hosts_path_for(device_id);
        if !path.exists() {
            std::fs::write(
                &path,
                "# generated by patchbay\n127.0.0.1\tlocalhost\n::1\tlocalhost\n",
            )
            .with_context(|| format!("write {}", path.display()))?;
        }
        Ok(())
    }

    /// Appends a host entry to a device's hosts file.
    pub(crate) fn append_host(&self, device_id: NodeId, name: &str, ip: IpAddr) -> Result<()> {
        use std::io::Write;
        let path = self.hosts_path_for(device_id);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        writeln!(f, "{ip}\t{name}").with_context(|| format!("append to {}", path.display()))?;
        Ok(())
    }

    pub(crate) fn write_resolv_conf(&self) -> Result<()> {
        let path = self.resolv_path();
        let mut content = String::from("# generated by patchbay\n");
        for ip in &self.nameservers {
            content.push_str(&format!("nameserver {ip}\n"));
        }
        std::fs::write(&path, content.as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
        Ok(())
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

/// RAII guard that aborts the reflector task when dropped.
pub struct ReflectorGuard(pub(crate) tokio::task::AbortHandle);

impl Drop for ReflectorGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Stores mutable topology state and build-time allocators.
pub(crate) struct NetworkCore {
    pub(crate) cfg: CoreConfig,
    /// DNS host entries for `/etc/hosts` overlay in spawned commands.
    pub(crate) dns: DnsOverlayDir,
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
        let dns = DnsOverlayDir::new(&cfg.prefix).context("create DNS entries dir")?;
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
    pub(crate) fn set_router_nat_mode(
        &mut self,
        id: NodeId,
        mode: Option<NatConfig>,
    ) -> Result<()> {
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
        nat: Option<NatConfig>,
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
                    ra_enabled: RA_DEFAULT_ENABLED,
                    ra_interval_secs: RA_DEFAULT_INTERVAL_SECS,
                    ra_lifetime_secs: RA_DEFAULT_LIFETIME_SECS,
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
                ra_runtime: Arc::new(RaRuntimeCfg::new(
                    RA_DEFAULT_ENABLED,
                    RA_DEFAULT_INTERVAL_SECS,
                    RA_DEFAULT_LIFETIME_SECS,
                )),
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
                provisioning_mode: None,
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
        if dev.interfaces.iter().any(|i| &*i.ifname == ifname) {
            bail!("device '{}' already has interface '{}'", dev.name, ifname);
        }
        // First interface becomes the default unless overridden later.
        if dev.default_via.is_empty() {
            dev.default_via = ifname.into();
        }
        dev.interfaces.push(DeviceIfaceData {
            ifname: ifname.into(),
            attachment: IfaceAttachment::Routed(downlink),
            ip: assigned,
            ip_v6: assigned_v6,
            ll_v6: assigned_v6.map(|_| link_local_from_seed(idx)),
            egress: impair,
            ingress: None,
            start_down: false,
            prefix_len: None,
            prefix_len_v6: None,
            idx,
        });
        Ok(assigned)
    }

    /// Adds an interface to a device from an [`IfaceConfig`](crate::IfaceConfig).
    ///
    /// Handles both routed (gateway present) and dummy (gateway absent)
    /// interfaces. For routed interfaces, allocates IPs from the router's pool
    /// unless overridden by the config.
    pub(crate) fn add_device_iface_from_config(
        &mut self,
        device: NodeId,
        ifname: &str,
        config: crate::IfaceConfig,
    ) -> Result<()> {
        if let Some(router) = config.gateway {
            // Routed interface: delegate to add_device_iface for pool allocation.
            self.add_device_iface(device, ifname, router, config.egress)?;
            // Apply fields that add_device_iface doesn't handle.
            let dev = self.device_mut(device).expect("just inserted");
            let iface = dev.iface_mut(ifname).expect("just inserted");
            if let Some(addr) = config.addr {
                iface.ip = Some(addr.addr());
                iface.prefix_len = Some(addr.prefix_len());
            }
            if let Some(addr_v6) = config.addr_v6 {
                iface.ip_v6 = Some(addr_v6.addr());
                iface.prefix_len_v6 = Some(addr_v6.prefix_len());
            }
            iface.ingress = config.ingress;
            iface.start_down = config.start_down;
        } else {
            // Dummy interface: no router, no pool allocation.
            let idx = self.alloc_id();
            let dev = self
                .devices
                .get_mut(&device)
                .ok_or_else(|| anyhow!("unknown device id"))?;
            if dev.interfaces.iter().any(|i| &*i.ifname == ifname) {
                bail!("device '{}' already has interface '{}'", dev.name, ifname);
            }
            if dev.default_via.is_empty() {
                dev.default_via = ifname.into();
            }
            let ip = config.addr.map(|n| n.addr());
            let ip_v6 = config.addr_v6.map(|n| n.addr());
            dev.interfaces.push(DeviceIfaceData {
                ifname: ifname.into(),
                attachment: IfaceAttachment::Dummy,
                ip,
                ip_v6,
                ll_v6: None,
                egress: config.egress,
                ingress: None,
                start_down: config.start_down,
                prefix_len: config.addr.map(|n| n.prefix_len()),
                prefix_len_v6: config.addr_v6.map(|n| n.prefix_len()),
                idx,
            });
        }
        Ok(())
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
        let uplink = iface.uplink().ok_or_else(|| anyhow!("switch missing"))?;
        let sw = self
            .switch(uplink)
            .ok_or_else(|| anyhow!("switch missing"))?;
        let gw_router = sw
            .owner_router
            .ok_or_else(|| anyhow!("switch missing owner"))?;
        let gw_router_data = self
            .router(gw_router)
            .ok_or_else(|| anyhow!("gateway router missing"))?;
        let gw_br = sw.bridge.clone().unwrap_or_else(|| "br-lan".into());
        let gw_ns = gw_router_data.ns.clone();
        let gw_ll_v6 = gw_router_data.active_downstream_ll_v6();
        let iface_build = IfaceBuild {
            dev_ns,
            gw_ns,
            gw_ip: sw.gw,
            gw_br,
            dev_ip: iface.ip,
            prefix_len: sw.cidr.map(|c| c.prefix_len()).unwrap_or(24),
            gw_ip_v6: sw.gw_v6,
            dev_ip_v6: iface.ip_v6,
            gw_ll_v6,
            dev_ll_v6: iface.ll_v6,
            prefix_len_v6: sw.cidr_v6.map(|c| c.prefix_len()).unwrap_or(64),
            egress: impair,
            ingress: None,
            dummy: false,
            start_down: false,
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
        let egress = iface.egress;
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
            gw_ll_v6: target_router.active_downstream_ll_v6(),
            dev_ll_v6: new_ip_v6.map(|_| link_local_from_seed(old_idx)),
            prefix_len_v6: sw.cidr_v6.map(|c| c.prefix_len()).unwrap_or(64),
            egress,
            ingress: None,
            dummy: false,
            start_down: false,
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
            iface.attachment = IfaceAttachment::Routed(new_uplink);
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

    /// Returns IPv6 default-router candidates for a router downstream switch.
    pub(crate) fn router_downlink_gw6_for_switch(&self, sw: NodeId) -> Result<DownlinkV6Gateways> {
        let switch = self
            .switches
            .get(&sw)
            .ok_or_else(|| anyhow!("switch missing"))?;
        let link_local_v6 = switch
            .owner_router
            .and_then(|rid| self.routers.get(&rid))
            .and_then(|r| r.downstream_ll_v6);
        Ok(DownlinkV6Gateways {
            global_v6: switch.gw_v6,
            link_local_v6,
        })
    }

    pub(crate) fn router_default_v6_targets(
        &self,
        router: NodeId,
        default_mode: Ipv6ProvisioningMode,
    ) -> Result<Vec<DeviceDefaultV6RouteTarget>> {
        let downlink = self
            .router(router)
            .ok_or_else(|| anyhow!("router removed"))?
            .downlink
            .ok_or_else(|| anyhow!("router has no downlink"))?;

        let mut out = Vec::new();
        for dev in self.devices.values() {
            let Some(iface) = dev.iface(&dev.default_via) else {
                continue;
            };
            let mode = dev.provisioning_mode.unwrap_or(default_mode);
            if mode != Ipv6ProvisioningMode::RaDriven {
                continue;
            }
            if iface.uplink() == Some(downlink) && iface.ip_v6.is_some() {
                out.push(DeviceDefaultV6RouteTarget {
                    ns: dev.ns.clone(),
                    ifname: iface.ifname.clone(),
                });
            }
        }
        Ok(out)
    }

    /// Returns whether RA-driven default-route learning is active for this switch.
    pub(crate) fn ra_default_enabled_for_switch(&self, sw: NodeId) -> Result<bool> {
        let switch = self
            .switches
            .get(&sw)
            .ok_or_else(|| anyhow!("switch missing"))?;
        let router = switch
            .owner_router
            .and_then(|rid| self.routers.get(&rid))
            .ok_or_else(|| anyhow!("switch missing owner router"))?;
        Ok(router.ra_default_enabled())
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
        router_entry.upstream_ll_v6 = ip_v6.map(|_| link_local_from_seed(seed2(router.0, sw.0)));
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
            if let Some(cidr) = sw_entry.cidr {
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
            cidr_v6.map(|_| link_local_from_seed(seed3(router.0, sw.0, 0xA5A5)));
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
                    if iface.uplink() == Some(sw_id) {
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
        let sw_id = iface
            .uplink()
            .ok_or_else(|| anyhow!("cannot renew IP on dummy interface '{}'", ifname))?;
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
                    .find(|i| i.uplink() == Some(downlink_sw))
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
