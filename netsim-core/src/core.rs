use anyhow::{anyhow, bail, Context, Result};
use ipnet::Ipv4Net;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Write as IoWrite;
use std::net::Ipv4Addr;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::thread;
use tracing::debug;
use tracing::warn;

use crate::netlink::Netlink;
use crate::netns;
use crate::{qdisc, Impair, NatMode};
use nix::libc;

/// Defines static addressing and naming for one lab instance.
#[derive(Clone, Debug)]
pub struct CoreConfig {
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
}

/// Identifies a node in the topology graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(pub u64);

/// Selects the address pool used for router downstream links.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownstreamPool {
    /// Uses private RFC1918 addressing.
    Private,
    /// Uses public routable addressing.
    Public,
}

/// Configures per-router NAT and downstream behavior.
#[derive(Clone, Debug)]
pub struct RouterConfig {
    /// Selects router NAT behavior.
    pub nat: NatMode,
    /// Selects which pool to allocate downstream subnets from.
    pub downstream_pool: DownstreamPool,
}

/// One network interface on a device, connected to a router's downstream switch.
#[derive(Clone, Debug)]
pub struct DeviceIfaceData {
    /// Interface name inside the device namespace (e.g. `"eth0"`).
    pub ifname: String,
    /// Stores the switch this interface is attached to.
    pub uplink: NodeId,
    /// Assigned IP address.
    pub ip: Option<Ipv4Addr>,
    /// Optional link impairment applied via `tc netem`.
    pub impair: Option<Impair>,
    /// Unique index used to name the root-namespace veth ends.
    pub(crate) idx: u64,
}

/// A network endpoint with one or more interfaces.
#[derive(Clone, Debug)]
pub struct DeviceData {
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
    pub fn iface(&self, name: &str) -> Option<&DeviceIfaceData> {
        self.interfaces.iter().find(|i| i.ifname == name)
    }

    /// Looks up an interface mutably by name.
    pub fn iface_mut(&mut self, name: &str) -> Option<&mut DeviceIfaceData> {
        self.interfaces.iter_mut().find(|i| i.ifname == name)
    }

    /// Returns the interface that carries the default route.
    ///
    /// # Panics
    /// Panics if `default_via` does not name a known interface (invariant
    /// maintained by `add_device_iface` / `set_device_default_via`).
    pub fn default_iface(&self) -> &DeviceIfaceData {
        self.iface(&self.default_via)
            .expect("default_via names a valid interface")
    }
}

/// Represents a router and its L3 connectivity state.
#[derive(Clone, Debug)]
pub struct RouterData {
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
    /// Stores the downstream switch identifier.
    pub downlink: Option<NodeId>,
    /// Stores the downstream subnet CIDR.
    pub downstream_cidr: Option<Ipv4Net>,
    /// Stores the downstream gateway address.
    pub downstream_gw: Option<Ipv4Addr>,
}

/// Represents an L2 switch/bridge attachment point.
#[derive(Clone, Debug)]
pub struct Switch {
    /// Stores the switch name.
    pub name: String,
    /// Stores the switch subnet if assigned.
    pub cidr: Option<Ipv4Net>,
    /// Stores the switch gateway address if assigned.
    pub gw: Option<Ipv4Addr>,
    /// Stores the owning router for managed downstream switches.
    pub owner_router: Option<NodeId>,
    /// Stores the backing bridge name.
    pub bridge: Option<String>,
    next_host: u8,
}

/// Per-interface wiring job collected by `build()`.
pub(crate) struct IfaceBuild {
    pub(crate) dev_ns: String,
    pub(crate) gw_ns: String,
    pub(crate) gw_ip: Ipv4Addr,
    pub(crate) gw_br: String,
    pub(crate) dev_ip: Ipv4Addr,
    pub(crate) prefix_len: u8,
    pub(crate) impair: Option<Impair>,
    pub(crate) ifname: String,
    pub(crate) is_default: bool,
    pub(crate) idx: u64,
}

/// Stores mutable topology state and build-time allocators.
pub struct NetworkCore {
    pub(crate) cfg: CoreConfig,
    pub(crate) netns: Arc<netns::NetnsManager>,
    next_id: u64,
    next_private_subnet: u16,
    next_public_subnet: u16,
    next_ix_low: u8,
    bridge_counter: u32,
    ns_counter: u32,
    ix_sw: NodeId,
    devices: HashMap<NodeId, DeviceData>,
    routers: HashMap<NodeId, RouterData>,
    switches: HashMap<NodeId, Switch>,
    nodes_by_name: HashMap<String, NodeId>,
    /// Links created by this instance; cleaned up on drop.
    own_links: Arc<Mutex<Vec<String>>>,
    /// Namespaces created by this instance; cleaned up on drop.
    pub(crate) own_netns: Vec<String>,
    /// Whether root namespace and IX bridge have been set up.
    pub(crate) root_ns_initialized: bool,
}

// ─────────────────────────────────────────────
// Global resource tracking / cleanup
// ─────────────────────────────────────────────

/// Process-wide prefix registry — used only as a safety net at process exit and on panic.
/// Per-instance resource tracking (links, namespaces) lives in `NetworkCore`.
#[derive(Default)]
struct ResourceState {
    prefixes: HashSet<String>,
}

/// Tracks lab prefixes for best-effort cleanup on panic/exit.
pub struct ResourceList {
    state: Mutex<ResourceState>,
    cleanup_enabled: AtomicBool,
}

impl Default for ResourceList {
    fn default() -> Self {
        Self {
            state: Mutex::new(ResourceState::default()),
            cleanup_enabled: AtomicBool::new(true),
        }
    }
}

static RESOURCES: OnceLock<ResourceList> = OnceLock::new();
static INIT_HOOKS: Once = Once::new();

/// Returns the global process resource tracker.
pub fn resources() -> &'static ResourceList {
    RESOURCES.get_or_init(|| {
        INIT_HOOKS.call_once(|| {
            unsafe {
                libc::atexit(cleanup_at_exit);
            }
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                resources().cleanup_registered_prefixes();
                prev(info);
            }));
        });
        ResourceList::default()
    })
}

extern "C" fn cleanup_at_exit() {
    resources().cleanup_registered_prefixes();
}

impl ResourceList {
    /// Enables or disables automatic cleanup in panic/atexit paths.
    pub fn set_cleanup_enabled(&self, enabled: bool) {
        self.cleanup_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Registers a resource-name prefix for broad cleanup at process exit or on panic.
    pub fn register_prefix(&self, prefix: &str) {
        let mut st = self.state.lock().unwrap();
        st.prefixes.insert(prefix.to_string());
    }

    /// Removes links and namespaces that match `prefix`.
    pub fn cleanup_by_prefix(&self, prefix: &str) {
        debug!("netsim cleanup: scanning prefix '{prefix}'");
        cleanup_links_with_prefix_ip(prefix);
        debug!("netsim cleanup: drop fd-registry entries with prefix '{prefix}'");
        netns::cleanup_registry_prefix(prefix);
    }

    /// Safety-net cleanup: removes all resources matching any registered prefix.
    /// Called at process exit and on panic. Normal cleanup goes through `NetworkCore::drop`.
    pub fn cleanup_registered_prefixes(&self) {
        if !self.cleanup_enabled.load(Ordering::Relaxed) {
            debug!("netsim cleanup: skipped (disabled)");
            return;
        }
        let prefixes = {
            let st = self.state.lock().unwrap();
            st.prefixes.clone()
        };
        debug!(
            "netsim cleanup: registered prefixes: {}",
            prefixes
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        );
        for prefix in prefixes {
            self.cleanup_by_prefix(&prefix);
        }
    }
}

fn cleanup_links_with_prefix_ip(prefix: &str) {
    let output = std::process::Command::new("ip")
        .args(["-o", "link", "show"])
        .output();
    if let Ok(out) = output {
        if let Ok(text) = String::from_utf8(out.stdout) {
            for line in text.lines() {
                let mut parts = line.split_whitespace();
                let _ = parts.next();
                if let Some(name) = parts.next() {
                    let name = name.trim_end_matches(':');
                    let ifname = name.split('@').next().unwrap_or(name);
                    if ifname.starts_with(prefix) {
                        delete_link_logged(ifname);
                    }
                }
            }
        }
    }
}

fn delete_link_logged(name: &str) {
    debug!("netsim cleanup: ip link del {name}");
    let out = std::process::Command::new("ip")
        .args(["link", "del", name])
        .output();
    if let Ok(out) = out {
        if !out.status.success() {
            let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if msg.contains("Cannot find device") {
                debug!("netsim cleanup: link '{name}' already gone");
            } else {
                warn!("netsim cleanup: failed ip link del {name}: {msg}");
            }
        }
    }
}

impl Drop for NetworkCore {
    fn drop(&mut self) {
        let links: Vec<String> = std::mem::take(&mut *self.own_links.lock().unwrap());
        let netns: Vec<String> = std::mem::take(&mut self.own_netns);
        debug!(
            "netsim cleanup: NetworkCore drop: {} links, {} namespaces",
            links.len(),
            netns.len()
        );
        for ns in netns {
            debug!("netsim cleanup: drop netns {ns}");
            netns::cleanup_netns(&ns);
        }
        for link in links {
            delete_link_logged(&link);
        }
    }
}

impl NetworkCore {
    /// Constructs a new topology core and pre-creates the IX switch.
    pub fn new(cfg: CoreConfig) -> Self {
        let own_links = Arc::new(Mutex::new(Vec::new()));
        let mut core = Self {
            cfg,
            netns: Arc::new(netns::NetnsManager::new_with_tracker(own_links.clone())),
            next_id: 1,
            next_private_subnet: 1,
            next_public_subnet: 1,
            next_ix_low: 10,
            bridge_counter: 2,
            ns_counter: 1,
            ix_sw: NodeId(0),
            devices: HashMap::new(),
            routers: HashMap::new(),
            switches: HashMap::new(),
            nodes_by_name: HashMap::new(),
            own_links,
            own_netns: Vec::new(),
            root_ns_initialized: false,
        };
        let ix_sw = core.add_switch("ix", Some(core.cfg.ix_cidr), Some(core.cfg.ix_gw));
        core.ix_sw = ix_sw;
        core
    }

    fn next_bridge_name(&mut self) -> String {
        let name = format!("br-{}-{}", self.cfg.bridge_tag, self.bridge_counter);
        self.bridge_counter = self.bridge_counter.saturating_add(1);
        name
    }

    fn next_ns_name(&mut self) -> String {
        let id = self.ns_counter;
        self.ns_counter = self.ns_counter.saturating_add(1);
        format!("{}-{}", self.cfg.prefix, id)
    }

    /// Returns the IX gateway address.
    pub fn ix_gw(&self) -> Ipv4Addr {
        self.cfg.ix_gw
    }

    /// Allocates the next low-end IX host address.
    pub fn alloc_ix_ip_low(&mut self) -> Ipv4Addr {
        let o = self.cfg.ix_gw.octets();
        let ip = Ipv4Addr::new(o[0], o[1], o[2], self.next_ix_low);
        self.next_ix_low = self.next_ix_low.saturating_add(1);
        ip
    }

    /// Returns the IX switch identifier.
    pub fn ix_sw(&self) -> NodeId {
        self.ix_sw
    }

    /// Returns the lab root namespace name.
    pub fn root_ns(&self) -> &str {
        &self.cfg.root_ns
    }

    /// Returns the namespace name for router `id`.
    pub fn router_ns(&self, id: NodeId) -> Result<&str> {
        self.routers
            .get(&id)
            .map(|r| r.ns.as_str())
            .ok_or_else(|| anyhow!("unknown router id"))
    }

    /// Returns the namespace name for device `id`.
    pub fn device_ns(&self, id: NodeId) -> Result<&str> {
        self.devices
            .get(&id)
            .map(|d| d.ns.as_str())
            .ok_or_else(|| anyhow!("unknown device id"))
    }

    /// Returns router data for `id`.
    pub fn router(&self, id: NodeId) -> Option<&RouterData> {
        self.routers.get(&id)
    }

    /// Returns device data for `id`.
    pub fn device(&self, id: NodeId) -> Option<&DeviceData> {
        self.devices.get(&id)
    }

    /// Returns mutable device data for `id`.
    pub fn device_mut(&mut self, id: NodeId) -> Option<&mut DeviceData> {
        self.devices.get_mut(&id)
    }

    /// Returns switch data for `id`.
    pub fn switch(&self, id: NodeId) -> Option<&Switch> {
        self.switches.get(&id)
    }

    /// Returns the router identifier for `name`, or `None` if not a router.
    pub fn router_id_by_name(&self, name: &str) -> Option<NodeId> {
        let id = *self.nodes_by_name.get(name)?;
        self.routers.contains_key(&id).then_some(id)
    }

    /// Returns the device identifier for `name`, or `None` if not a device.
    pub fn device_id_by_name(&self, name: &str) -> Option<NodeId> {
        let id = *self.nodes_by_name.get(name)?;
        self.devices.contains_key(&id).then_some(id)
    }

    /// Returns `(ns, downlink_bridge_name, wan_if_name, upstream_ip)` for a built router.
    pub fn router_nat_params(&self, id: NodeId) -> Result<(String, String, String, Ipv4Addr)> {
        let router = self.routers.get(&id).context("unknown router id")?;
        let wan_if = if router.uplink == Some(self.ix_sw) {
            "ix"
        } else {
            "wan"
        };
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
    pub fn set_router_nat_mode(&mut self, id: NodeId, mode: NatMode) -> Result<()> {
        let router = self.routers.get_mut(&id).context("unknown router id")?;
        router.cfg.nat = mode;
        Ok(())
    }

    /// Adds a router node and returns its identifier.
    ///
    /// The namespace name and downstream bridge name are generated internally.
    pub fn add_router(
        &mut self,
        name: &str,
        nat: NatMode,
        downstream_pool: DownstreamPool,
        region: Option<String>,
    ) -> NodeId {
        let ns = self.next_ns_name();
        let downlink_bridge = self.next_bridge_name();
        let id = NodeId(self.alloc_id());
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
                },
                downlink_bridge,
                uplink: None,
                upstream_ip: None,
                downlink: None,
                downstream_cidr: None,
                downstream_gw: None,
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
    pub fn add_device(&mut self, name: &str) -> NodeId {
        let ns = self.next_ns_name();
        let id = NodeId(self.alloc_id());
        self.nodes_by_name.insert(name.to_string(), id);
        self.devices.insert(
            id,
            DeviceData {
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
    pub fn add_device_iface(
        &mut self,
        device: NodeId,
        ifname: &str,
        router: NodeId,
        impair: Option<Impair>,
    ) -> Result<Ipv4Addr> {
        let downlink = self
            .routers
            .get(&router)
            .and_then(|r| r.downlink)
            .ok_or_else(|| anyhow!("router missing downlink switch"))?;
        let assigned = self.alloc_from_switch(downlink)?;
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
            ip: Some(assigned),
            impair,
            idx,
        });
        Ok(assigned)
    }

    /// Changes which interface carries the default route.
    pub fn set_device_default_via(&mut self, device: NodeId, ifname: &str) -> Result<()> {
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
    pub fn router_downlink_gw_for_switch(&self, sw: NodeId) -> Result<Ipv4Addr> {
        self.switches
            .get(&sw)
            .and_then(|s| s.gw)
            .ok_or_else(|| anyhow!("switch missing gateway ip"))
    }

    /// Adds a switch node and returns its identifier.
    pub fn add_switch(
        &mut self,
        name: &str,
        cidr: Option<Ipv4Net>,
        gw: Option<Ipv4Addr>,
    ) -> NodeId {
        let id = NodeId(self.alloc_id());
        self.nodes_by_name.insert(name.to_string(), id);
        self.switches.insert(
            id,
            Switch {
                name: name.to_string(),
                cidr,
                gw,
                owner_router: None,
                bridge: None,
                next_host: 2,
            },
        );
        id
    }

    /// Connects `router` to uplink switch `sw` and returns its uplink IP.
    pub fn connect_router_uplink(
        &mut self,
        router: NodeId,
        sw: NodeId,
        ip: Option<Ipv4Addr>,
    ) -> Result<Ipv4Addr> {
        let assigned = match ip {
            Some(ip) => ip,
            None => self.alloc_from_switch(sw)?,
        };
        let router_entry = self
            .routers
            .get_mut(&router)
            .ok_or_else(|| anyhow!("unknown router id"))?;
        router_entry.uplink = Some(sw);
        router_entry.upstream_ip = Some(assigned);
        Ok(assigned)
    }

    /// Connects `router` to downstream switch `sw` and returns `(cidr, gw)`.
    pub fn connect_router_downlink(
        &mut self,
        router: NodeId,
        sw: NodeId,
    ) -> Result<(Ipv4Net, Ipv4Addr)> {
        let pool = self
            .routers
            .get(&router)
            .ok_or_else(|| anyhow!("unknown router id"))?
            .cfg
            .downstream_pool;
        let (cidr, gw) = {
            let sw_entry = self
                .switches
                .get(&sw)
                .ok_or_else(|| anyhow!("unknown switch id"))?;
            if sw_entry.cidr.is_some() {
                let cidr = sw_entry.cidr.unwrap();
                let gw = sw_entry
                    .gw
                    .ok_or_else(|| anyhow!("switch '{}' missing gw", sw_entry.name))?;
                (cidr, gw)
            } else {
                let cidr = match pool {
                    DownstreamPool::Private => self.alloc_private_cidr()?,
                    DownstreamPool::Public => self.alloc_public_cidr()?,
                };
                let gw = add_host(cidr, 1)?;
                (cidr, gw)
            }
        };

        let sw_entry = self
            .switches
            .get_mut(&sw)
            .ok_or_else(|| anyhow!("unknown switch id"))?;
        sw_entry.cidr = Some(cidr);
        sw_entry.gw = Some(gw);
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
        router_entry.downstream_cidr = Some(cidr);
        router_entry.downstream_gw = Some(gw);
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

    /// Returns an iterator over all devices in the topology.
    pub fn all_devices(&self) -> impl Iterator<Item = &DeviceData> {
        self.devices.values()
    }

    /// Returns an iterator over all routers in the topology.
    pub fn all_routers(&self) -> impl Iterator<Item = &RouterData> {
        self.routers.values()
    }

    /// Returns all device node ids.
    pub fn all_device_ids(&self) -> Vec<NodeId> {
        self.devices.keys().copied().collect()
    }

    /// Returns all router node ids.
    pub fn all_router_ids(&self) -> Vec<NodeId> {
        self.routers.keys().copied().collect()
    }

    /// Returns all namespace names owned by the lab.
    pub fn all_ns_names(&self) -> Vec<String> {
        let mut v = vec![self.cfg.root_ns.clone()];
        for r in self.routers.values() {
            v.push(r.ns.clone());
        }
        for d in self.devices.values() {
            v.push(d.ns.clone());
        }
        v
    }
}

// ─────────────────────────────────────────────
// Free async setup functions (used by builders; no lock held)
// ─────────────────────────────────────────────

/// Helper: run a netlink operation in a namespace via the shared NetnsManager.
pub(crate) async fn nl_run<F>(netns: &Arc<netns::NetnsManager>, ns: &str, f: F) -> Result<()>
where
    F: AsyncFnOnce(&mut Netlink) -> Result<()> + Send + 'static,
{
    netns
        .spawn_netlink_task_in(ns, move |nl_arc| async move {
            let mut nl = nl_arc.lock().await;
            f(&mut *nl).await
        })
        .await
        .map_err(|_| anyhow!("netns task cancelled"))?
}

/// Creates root namespace, IX bridge, and enables forwarding. Idempotent-safe at caller level.
pub(crate) async fn setup_root_ns_async(
    cfg: &CoreConfig,
    netns: &Arc<netns::NetnsManager>,
) -> Result<()> {
    ensure_netns_dir()?;
    let root_ns = cfg.root_ns.clone();
    create_named_netns(&root_ns).await?;

    if let Err(err) = run_nft_in(&root_ns, "flush ruleset").await {
        debug!(ns = %root_ns, error = %err, "setup_root_ns: nft flush failed; continuing");
    }

    let ix_br = cfg.ix_br.clone();
    let ix_ip = cfg.ix_gw;
    let ix_prefix = cfg.ix_cidr.prefix_len();
    nl_run(netns, &root_ns, {
        let ix_br = ix_br.clone();
        async move |h| {
            h.set_link_up("lo").await?;
            h.ensure_link_deleted(&ix_br).await.ok();
            h.add_bridge(&ix_br).await?;
            h.set_link_up(&ix_br).await?;
            h.add_addr4(&ix_br, ix_ip, ix_prefix).await?;
            Ok(())
        }
    })
    .await?;
    set_sysctl_in(&root_ns, "net/ipv4/ip_forward", "1")?;
    Ok(())
}

/// Data snapshot needed to set up a single router.
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
    /// Downlink bridge info (if router has downstream switch).
    pub downlink_bridge: Option<(String, Ipv4Addr, u8)>,
}

/// Sets up a single router's namespaces, links, and NAT. No lock held.
pub(crate) async fn setup_router_async(
    netns: &Arc<netns::NetnsManager>,
    data: &RouterSetupData,
) -> Result<()> {
    let router = &data.router;
    let id = router.id;

    // Create router namespace.
    create_named_netns(&router.ns).await?;
    if let Err(err) = run_nft_in(&router.ns, "flush ruleset").await {
        debug!(ns = %router.ns, error = %err, "setup_router: nft flush failed; continuing");
    }

    let uplink = router
        .uplink
        .ok_or_else(|| anyhow!("router missing uplink"))?;

    if uplink == data.ix_sw {
        // IX-level router.
        let root_if = format!("{}i{}", data.prefix, id.0);
        let ns_if = "ix".to_string();

        nl_run(netns, &data.root_ns, {
            let root_if = root_if.clone();
            let ns_if = ns_if.clone();
            let ix_br = data.ix_br.clone();
            let router_ns = router.ns.clone();
            async move |h| {
                h.ensure_link_deleted(&root_if).await.ok();
                h.ensure_link_deleted(&ns_if).await.ok();
                h.add_veth(&root_if, &ns_if).await?;
                h.set_master(&root_if, &ix_br).await?;
                h.set_link_up(&root_if).await?;
                h.move_link_to_netns(&ns_if, &open_netns_fd(&router_ns)?)
                    .await?;
                Ok(())
            }
        })
        .await?;

        let ix_ip = router.upstream_ip.unwrap();
        let ix_prefix = data.ix_cidr_prefix;
        let ix_gw = data.ix_gw;
        nl_run(netns, &router.ns, {
            let ns_if = ns_if.clone();
            async move |h| {
                h.set_link_up("lo").await?;
                h.set_link_up(&ns_if).await?;
                h.add_addr4(&ns_if, ix_ip, ix_prefix).await?;
                h.add_default_route_v4(ix_gw).await?;
                Ok(())
            }
        })
        .await?;
        set_sysctl_in(&router.ns, "net/ipv4/ip_forward", "1")?;

        apply_nat(
            &router.ns,
            router.cfg.nat,
            &router.downlink_bridge,
            &ns_if,
            router.upstream_ip.unwrap(),
        )
        .await?;
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
        nl_run(netns, &data.root_ns, {
            let root_a = root_a.clone();
            let root_b = root_b.clone();
            let owner_ns = owner_ns.clone();
            let router_ns = router.ns.clone();
            async move |h| {
                h.ensure_link_deleted(&root_a).await.ok();
                h.ensure_link_deleted(&root_b).await.ok();
                h.add_veth(&root_a, &root_b).await?;
                h.move_link_to_netns(&root_a, &open_netns_fd(&owner_ns)?)
                    .await?;
                h.move_link_to_netns(&root_b, &open_netns_fd(&router_ns)?)
                    .await?;
                Ok(())
            }
        })
        .await?;

        let owner_if = format!("h{}", id.0);
        nl_run(netns, owner_ns, {
            let root_a = root_a.clone();
            let bridge = bridge.clone();
            async move |h| {
                h.rename_link(&root_a, &owner_if).await?;
                h.set_link_up(&owner_if).await?;
                h.set_master(&owner_if, &bridge).await?;
                Ok(())
            }
        })
        .await?;

        let wan_if = "wan".to_string();
        let wan_ip = router.upstream_ip.unwrap();
        let wan_prefix = data.upstream_cidr_prefix.unwrap();
        nl_run(netns, &router.ns, {
            let root_b = root_b.clone();
            let wan_if = wan_if.clone();
            async move |h| {
                h.set_link_up("lo").await?;
                h.rename_link(&root_b, &wan_if).await?;
                h.set_link_up(&wan_if).await?;
                h.add_addr4(&wan_if, wan_ip, wan_prefix).await?;
                h.add_default_route_v4(gw_ip).await?;
                Ok(())
            }
        })
        .await?;
        set_sysctl_in(&router.ns, "net/ipv4/ip_forward", "1")?;

        apply_nat(
            &router.ns,
            router.cfg.nat,
            &router.downlink_bridge,
            &wan_if,
            router.upstream_ip.unwrap(),
        )
        .await?;
    }

    // Create downlink bridge.
    if let Some((br, lan_ip, lan_prefix)) = &data.downlink_bridge {
        nl_run(netns, &router.ns, {
            let br = br.clone();
            let lan_ip = *lan_ip;
            let lan_prefix = *lan_prefix;
            async move |h| {
                h.set_link_up("lo").await?;
                h.ensure_link_deleted(&br).await.ok();
                h.add_bridge(&br).await?;
                h.set_link_up(&br).await?;
                h.add_addr4(&br, lan_ip, lan_prefix).await?;
                Ok(())
            }
        })
        .await?;
    }

    // Return route in lab root for public downstreams.
    if let Some((net, prefix_len, via)) = data.return_route {
        nl_run(netns, &data.root_ns, async move |h| {
            h.add_route_v4(net, prefix_len, via).await.ok();
            Ok(())
        })
        .await
        .ok();
    }

    Ok(())
}

/// Sets up a single device's namespace and wires all interfaces. No lock held.
pub(crate) async fn setup_device_async(
    netns: &Arc<netns::NetnsManager>,
    prefix: &str,
    root_ns: &str,
    dev: &DeviceData,
    ifaces: Vec<IfaceBuild>,
) -> Result<()> {
    create_named_netns(&dev.ns).await?;
    if let Err(err) = run_nft_in(&dev.ns, "flush ruleset").await {
        debug!(ns = %dev.ns, error = %err, "setup_device: nft flush failed; continuing");
    }

    for iface in ifaces {
        wire_iface_async(netns, prefix, root_ns, iface).await?;
    }
    Ok(())
}

/// Wire one device interface: veth pair, move, IP, route, impairment.
pub(crate) async fn wire_iface_async(
    netns: &Arc<netns::NetnsManager>,
    prefix: &str,
    root_ns: &str,
    dev: IfaceBuild,
) -> Result<()> {
    let root_gw = format!("{}g{}", prefix, dev.idx);
    let root_dev = format!("{}e{}", prefix, dev.idx);

    nl_run(netns, root_ns, {
        let root_gw = root_gw.clone();
        let root_dev = root_dev.clone();
        let dev_ns = dev.dev_ns.clone();
        let gw_ns = dev.gw_ns.clone();
        async move |h| {
            h.ensure_link_deleted(&root_gw).await.ok();
            h.ensure_link_deleted(&root_dev).await.ok();
            h.add_veth(&root_gw, &root_dev).await?;
            h.move_link_to_netns(&root_gw, &open_netns_fd(&gw_ns)?)
                .await?;
            h.move_link_to_netns(&root_dev, &open_netns_fd(&dev_ns)?)
                .await?;
            Ok(())
        }
    })
    .await?;

    let dev_ip = dev.dev_ip;
    let dev_prefix = dev.prefix_len;
    let ifname = dev.ifname.clone();
    let is_default = dev.is_default;
    let gw_ip = dev.gw_ip;
    nl_run(netns, &dev.dev_ns, {
        let root_dev = root_dev.clone();
        let ifname = ifname.clone();
        async move |h| {
            h.set_link_up("lo").await?;
            h.rename_link(&root_dev, &ifname).await?;
            h.set_link_up(&ifname).await?;
            h.add_addr4(&ifname, dev_ip, dev_prefix).await?;
            if is_default {
                h.add_default_route_v4(gw_ip).await?;
            }
            Ok(())
        }
    })
    .await?;

    nl_run(netns, &dev.gw_ns, {
        let root_gw = root_gw.clone();
        let gw_if = format!("v{}", dev.idx);
        let gw_br = dev.gw_br.clone();
        async move |h| {
            h.rename_link(&root_gw, &gw_if).await?;
            h.set_link_up(&gw_if).await?;
            h.set_master(&gw_if, &gw_br).await?;
            Ok(())
        }
    })
    .await?;

    if let Some(imp) = dev.impair {
        apply_impair_in(&dev.dev_ns, &dev.ifname, imp);
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

/// Ensures netns runtime prerequisites are initialized.
pub fn ensure_netns_dir() -> Result<()> {
    netns::ensure_netns_dir()
}

/// Opens a namespace file descriptor for `name`.
pub fn open_netns_fd(name: &str) -> Result<File> {
    netns::open_netns_fd(name)
}

/// Cleans up a namespace by name.
pub fn cleanup_netns(name: &str) {
    netns::cleanup_netns(name);
}

/// Creates a named network namespace.
pub async fn create_named_netns(name: &str) -> Result<()> {
    netns::create_named_netns(name).await?;
    Ok(())
}

/// Spawns a worker-thread task that runs a closure inside `ns`.
pub fn spawn_closure_in_namespace_thread<F, R>(ns: String, f: F) -> thread::JoinHandle<Result<R>>
where
    F: FnOnce() -> Result<R> + Send + 'static,
    R: Send + 'static,
{
    netns::spawn_closure_in_netns(ns, f)
}

/// Runs a synchronous closure inside `ns`.
pub fn run_closure_in_namespace<F, R>(ns: &str, f: F) -> Result<R>
where
    F: FnOnce() -> Result<R> + Send + 'static,
    R: Send + 'static,
{
    netns::run_closure_in_netns(ns, f)
}

/// Runs a command to completion inside `ns`.
pub fn run_command_in_namespace(ns: &str, cmd: std::process::Command) -> Result<ExitStatus> {
    netns::run_command_in_netns(ns, cmd)
}

/// Spawns a command process inside `ns`.
pub fn spawn_command_in_namespace(
    ns: &str,
    cmd: std::process::Command,
) -> Result<std::process::Child> {
    netns::spawn_command_in_netns(ns, cmd)
}

/// Sets a sysctl value in the current namespace.
pub fn set_sysctl_root(path: &str, val: &str) -> Result<()> {
    debug!(path = %path, val = %val, "sysctl: set in root");
    std::fs::write(format!("/proc/sys/{}", path), val)
        .with_context(|| format!("sysctl write {}", path))
}

/// Sets a sysctl value inside `ns`.
pub fn set_sysctl_in(ns: &str, path: &str, val: &str) -> Result<()> {
    debug!(ns = %ns, path = %path, val = %val, "sysctl: set in namespace");
    let path = path.to_string();
    let val = val.to_string();
    run_closure_in_namespace(ns, move || set_sysctl_root(&path, &val))
}

/// Applies nftables rules inside `ns`.
pub async fn run_nft_in(ns: &str, rules: &str) -> Result<()> {
    debug!(ns = %ns, rules = %rules, "nft: apply rules");
    let rules = rules.to_string();
    let ns = ns.to_string();
    let ns_err = ns.clone();
    run_closure_in_namespace(&ns, move || {
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
            Err(anyhow!("nft apply failed in namespace '{}'", ns_err))
        }
    })
}

/// Applies home-router NAT rules in `ns` for `mode`.
pub async fn apply_home_nat(
    ns: &str,
    mode: NatMode,
    _lan_if: &str,
    wan_if: &str,
    wan_ip: Ipv4Addr,
) -> Result<()> {
    let snat_rule = match mode {
        NatMode::DestinationIndependent => {
            format!("oif \"{wan}\" snat to {ip}", wan = wan_if, ip = wan_ip)
        }
        NatMode::DestinationDependent => {
            format!("oif \"{wan}\" masquerade random", wan = wan_if)
        }
        NatMode::None | NatMode::Cgnat => {
            return Ok(());
        }
    };

    let rules = format!(
        r#"
table ip nat {{
    chain postrouting {{
        type nat hook postrouting priority 100;
        {snat}
    }}
}}
"#,
        snat = snat_rule,
    );
    run_nft_in(ns, &rules).await
}

/// Applies router NAT rules for the configured mode.
pub async fn apply_nat(
    ns: &str,
    mode: NatMode,
    lan_if: &str,
    wan_if: &str,
    wan_ip: Ipv4Addr,
) -> Result<()> {
    match mode {
        NatMode::None => Ok(()),
        NatMode::Cgnat => apply_isp_cgnat(ns, wan_if).await,
        NatMode::DestinationIndependent | NatMode::DestinationDependent => {
            apply_home_nat(ns, mode, lan_if, wan_if, wan_ip).await
        }
    }
}

/// Applies ISP CGNAT masquerade rules in `ns` on `ix_if`.
pub async fn apply_isp_cgnat(ns: &str, ix_if: &str) -> Result<()> {
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
    run_nft_in(ns, &rules).await
}

/// Applies per-destination latency filters on `ifname` inside `ns`.
pub fn apply_region_latency(ns: &str, ifname: &str, filters: &[(Ipv4Net, u32)]) -> Result<()> {
    if filters.is_empty() {
        return Ok(());
    }
    qdisc::apply_region_latency(ns, ifname, filters)
}

/// Applies an impairment preset or manual limits on `ifname` inside `ns`.
pub fn apply_impair_in(ns: &str, ifname: &str, impair: Impair) {
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

    if let Err(e) = qdisc::apply_impair(ns, ifname, limits) {
        eprintln!("warn: apply_impair_in({}): {}", ifname, e);
    }
}

/// Controls a background task spawned by the lab.
#[derive(Clone)]
pub struct TaskHandle {
    stop: mpsc::Sender<()>,
}

impl TaskHandle {
    pub(crate) fn new(stop: mpsc::Sender<()>) -> Self {
        Self { stop }
    }

    /// Signals the task to stop.
    pub fn stop(&self) {
        let _ = self.stop.send(());
    }
}
