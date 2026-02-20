use anyhow::{anyhow, bail, Context, Result};
use futures::stream::TryStreamExt;
use ipnet::Ipv4Net;
use rtnetlink::{new_connection, Handle, LinkBridge, LinkUnspec, LinkVeth, RouteMessageBuilder};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Write as IoWrite;
use std::net::Ipv4Addr;
use std::os::fd::AsRawFd;
use std::process::ExitStatus;
use std::sync::mpsc;
use std::sync::{Mutex, Once, OnceLock};
use std::thread;
use tracing::debug;

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

/// Identifies a device node.
pub type DeviceId = NodeId;
/// Identifies a router node.
pub type RouterId = NodeId;
/// Identifies a switch node.
pub type SwitchId = NodeId;

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
    pub nat: Option<NatMode>,
    /// Enables ISP-style CGNAT on the IX-facing interface.
    pub cgnat: bool,
    /// Stores the downstream bridge name.
    pub downlink_bridge: String,
    /// Selects which pool to allocate downstream subnets from.
    pub downstream_pool: DownstreamPool,
}

/// One network interface on a device, connected to a router's downstream switch.
#[derive(Clone, Debug)]
pub struct DeviceIface {
    /// Interface name inside the device namespace (e.g. `"eth0"`).
    pub ifname: String,
    /// Stores the switch this interface is attached to.
    pub uplink: SwitchId,
    /// Assigned IP address.
    pub ip: Option<Ipv4Addr>,
    /// Optional link impairment applied via `tc netem`.
    pub impair: Option<Impair>,
    /// Unique index used to name the root-namespace veth ends.
    pub(crate) idx: u64,
}

/// A network endpoint with one or more interfaces.
#[derive(Clone, Debug)]
pub struct Device {
    /// Identifies the device.
    pub id: DeviceId,
    /// Stores the device name.
    pub name: String,
    /// Stores the device namespace name.
    pub ns: String,
    /// Interfaces in declaration order.
    pub interfaces: Vec<DeviceIface>,
    /// `ifname` of the interface that carries the default route.
    pub default_via: String,
}

impl Device {
    /// Looks up an interface by name.
    pub fn iface(&self, name: &str) -> Option<&DeviceIface> {
        self.interfaces.iter().find(|i| i.ifname == name)
    }

    /// Looks up an interface mutably by name.
    pub fn iface_mut(&mut self, name: &str) -> Option<&mut DeviceIface> {
        self.interfaces.iter_mut().find(|i| i.ifname == name)
    }

    /// Returns the interface that carries the default route.
    ///
    /// # Panics
    /// Panics if `default_via` does not name a known interface (invariant
    /// maintained by `add_device_iface` / `set_device_default_via`).
    pub fn default_iface(&self) -> &DeviceIface {
        self.iface(&self.default_via)
            .expect("default_via names a valid interface")
    }
}

/// Represents a router and its L3 connectivity state.
#[derive(Clone, Debug)]
pub struct Router {
    /// Identifies the router.
    pub id: RouterId,
    /// Stores the router name.
    pub name: String,
    /// Stores the router namespace name.
    pub ns: String,
    /// Stores the optional router region label.
    pub region: Option<String>,
    /// Stores static router configuration.
    pub cfg: RouterConfig,
    /// Stores the uplink switch identifier.
    pub uplink: Option<SwitchId>,
    /// Stores the router uplink IPv4 address.
    pub upstream_ip: Option<Ipv4Addr>,
    /// Stores the downstream switch identifier.
    pub downlink: Option<SwitchId>,
    /// Stores the downstream subnet CIDR.
    pub downstream_cidr: Option<Ipv4Net>,
    /// Stores the downstream gateway address.
    pub downstream_gw: Option<Ipv4Addr>,
}

/// Represents an L2 switch/bridge attachment point.
#[derive(Clone, Debug)]
pub struct Switch {
    /// Identifies the switch.
    pub id: SwitchId,
    /// Stores the switch name.
    pub name: String,
    /// Stores the switch subnet if assigned.
    pub cidr: Option<Ipv4Net>,
    /// Stores the switch gateway address if assigned.
    pub gw: Option<Ipv4Addr>,
    /// Stores the owning router for managed downstream switches.
    pub owner_router: Option<RouterId>,
    /// Stores the backing bridge name.
    pub bridge: Option<String>,
    next_host: u8,
}

/// Per-interface wiring job collected by `build()`.
struct IfaceBuild {
    dev_ns: String,
    gw_ns: String,
    gw_ip: Ipv4Addr,
    gw_br: String,
    dev_ip: Ipv4Addr,
    prefix_len: u8,
    impair: Option<Impair>,
    /// Interface name inside the device namespace.
    ifname: String,
    /// Only this interface gets `ip route add default`.
    is_default: bool,
    /// Unique index drives veth naming in the lab-root namespace.
    idx: u64,
}

/// Stores mutable topology state and build-time allocators.
pub struct LabCore {
    cfg: CoreConfig,
    netns: netns::NetnsManager,
    next_id: u64,
    next_private_subnet: u16,
    next_public_subnet: u16,
    next_ix_low: u8,
    next_ix_high: u8,
    ix_sw: SwitchId,
    devices: HashMap<DeviceId, Device>,
    routers: HashMap<RouterId, Router>,
    switches: HashMap<SwitchId, Switch>,
    nodes_by_name: HashMap<String, NodeId>,
}

// ─────────────────────────────────────────────
// Global resource tracking / cleanup
// ─────────────────────────────────────────────

#[derive(Default)]
struct ResourceState {
    links: HashSet<String>,
    netns: HashSet<String>,
    prefixes: HashSet<String>,
}

/// Tracks resources for best-effort cleanup on panic/exit.
#[derive(Default)]
pub struct ResourceList {
    state: Mutex<ResourceState>,
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
                resources().cleanup_all();
                prev(info);
            }));
        });
        ResourceList::default()
    })
}

extern "C" fn cleanup_at_exit() {
    resources().cleanup_all();
}

impl ResourceList {
    /// Registers a link name for cleanup.
    pub fn register_link(&self, name: &str) {
        // Only track names we own by prefix; generic names like "ix" are moved
        // into namespaces and may collide with host interfaces.
        if !(name.starts_with("lab-") || name.starts_with("br-")) {
            return;
        }
        let mut st = self.state.lock().unwrap();
        st.links.insert(name.to_string());
    }

    /// Registers a namespace name for cleanup.
    pub fn register_netns(&self, name: &str) {
        let mut st = self.state.lock().unwrap();
        st.netns.insert(name.to_string());
    }

    /// Registers a resource-name prefix for broad cleanup.
    pub fn register_prefix(&self, prefix: &str) {
        let mut st = self.state.lock().unwrap();
        st.prefixes.insert(prefix.to_string());
    }

    /// Removes all explicitly registered links and namespaces.
    pub fn cleanup_all(&self) {
        let (links, netns) = {
            let mut st = self.state.lock().unwrap();
            (std::mem::take(&mut st.links), std::mem::take(&mut st.netns))
        };
        println!(
            "netsim cleanup: explicit resources: {} links, {} namespaces",
            links.len(),
            netns.len()
        );
        for ns in netns {
            println!("netsim cleanup: cleanup netns {ns}");
            cleanup_netns_logged(&ns);
        }
        for link in links {
            delete_link_logged(&link);
        }
    }

    /// Removes links and namespaces that match `prefix`.
    pub fn cleanup_everything_with_prefix(&self, prefix: &str) {
        println!("netsim cleanup: scanning prefix '{prefix}'");
        cleanup_links_with_prefix_ip(prefix);

        let output = std::process::Command::new("ip")
            .args(["netns", "list"])
            .output();
        if let Ok(out) = output {
            if let Ok(text) = String::from_utf8(out.stdout) {
                for line in text.lines() {
                    let name = line.split_whitespace().next().unwrap_or_default();
                    if name.starts_with(prefix) {
                        cleanup_netns_logged(name);
                    }
                }
            }
        }
        println!("netsim cleanup: drop fd-registry entries with prefix '{prefix}'");
        netns::cleanup_registry_prefix(prefix);
    }

    /// Removes resources for all registered prefixes.
    pub fn cleanup_everything(&self) {
        let prefixes = {
            let st = self.state.lock().unwrap();
            st.prefixes.clone()
        };
        println!(
            "netsim cleanup: registered prefixes: {}",
            prefixes
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        );
        for prefix in prefixes {
            self.cleanup_everything_with_prefix(&prefix);
        }
    }
}

fn cleanup_netns_logged(name: &str) {
    println!("netsim cleanup: ip netns del {name}");
    let out = std::process::Command::new("ip")
        .args(["netns", "del", name])
        .output();
    if let Ok(out) = out {
        if !out.status.success() {
            let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if msg.contains("No such file or directory") {
                println!(
                    "netsim cleanup: netns '{name}' not present in /var/run/netns (ok for fd backend)"
                );
            } else {
                eprintln!("netsim cleanup: failed ip netns del {name}: {msg}");
            }
        }
    }
    // Ensure FD backend entries are removed even when named deletion fails.
    netns::cleanup_netns(name);
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
    println!("netsim cleanup: ip link del {name}");
    let out = std::process::Command::new("ip")
        .args(["link", "del", name])
        .output();
    if let Ok(out) = out {
        if !out.status.success() {
            let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if msg.contains("Cannot find device") {
                println!("netsim cleanup: link '{name}' already gone");
            } else {
                eprintln!("netsim cleanup: failed ip link del {name}: {msg}");
            }
        }
    }
}
impl LabCore {
    /// Constructs a new topology core and pre-creates the IX switch.
    pub fn new(cfg: CoreConfig) -> Self {
        let mut core = Self {
            cfg,
            netns: netns::NetnsManager::new(),
            next_id: 1,
            next_private_subnet: 1,
            next_public_subnet: 1,
            next_ix_low: 10,
            next_ix_high: 250,
            ix_sw: NodeId(0),
            devices: HashMap::new(),
            routers: HashMap::new(),
            switches: HashMap::new(),
            nodes_by_name: HashMap::new(),
        };
        let ix_sw = core.add_switch("ix", Some(core.cfg.ix_cidr), Some(core.cfg.ix_gw));
        core.ix_sw = ix_sw;
        core
    }

    async fn with_netns<F>(&self, ns: &str, f: F) -> Result<()>
    where
        F: AsyncFnOnce(&mut Netlink) -> Result<()> + Send + 'static,
    {
        let ns_name = ns.to_string();
        self.netns
            .run_in(&ns_name, move || async move {
                let (conn, handle, _) =
                    new_connection().context("rtnetlink new_connection in netns")?;
                tokio::spawn(conn);
                let mut nl = Netlink::new(handle);
                f(&mut nl).await
            })
            .await
    }

    /// Returns the IX gateway address.
    pub fn ix_gw(&self) -> Ipv4Addr {
        self.cfg.ix_gw
    }

    /// Returns the IX bridge name.
    pub fn ix_br(&self) -> &str {
        &self.cfg.ix_br
    }

    /// Allocates the next low-end IX host address.
    pub fn alloc_ix_ip_low(&mut self) -> Ipv4Addr {
        let o = self.cfg.ix_gw.octets();
        let ip = Ipv4Addr::new(o[0], o[1], o[2], self.next_ix_low);
        self.next_ix_low = self.next_ix_low.saturating_add(1);
        ip
    }

    /// Allocates the next high-end IX host address.
    pub fn alloc_ix_ip_high(&mut self) -> Ipv4Addr {
        let o = self.cfg.ix_gw.octets();
        let ip = Ipv4Addr::new(o[0], o[1], o[2], self.next_ix_high);
        self.next_ix_high = self.next_ix_high.saturating_sub(1);
        ip
    }

    /// Returns the IX switch identifier.
    pub fn ix_sw(&self) -> SwitchId {
        self.ix_sw
    }

    /// Returns the lab root namespace name.
    pub fn root_ns(&self) -> &str {
        &self.cfg.root_ns
    }

    /// Returns the namespace name for router `id`.
    pub fn router_ns(&self, id: RouterId) -> Result<&str> {
        self.routers
            .get(&id)
            .map(|r| r.ns.as_str())
            .ok_or_else(|| anyhow!("unknown router id"))
    }

    /// Returns the namespace name for device `id`.
    pub fn device_ns(&self, id: DeviceId) -> Result<&str> {
        self.devices
            .get(&id)
            .map(|d| d.ns.as_str())
            .ok_or_else(|| anyhow!("unknown device id"))
    }

    /// Returns router data for `id`.
    pub fn router(&self, id: RouterId) -> Option<&Router> {
        self.routers.get(&id)
    }

    /// Returns device data for `id`.
    pub fn device(&self, id: DeviceId) -> Option<&Device> {
        self.devices.get(&id)
    }

    /// Returns mutable device data for `id`.
    pub fn device_mut(&mut self, id: DeviceId) -> Option<&mut Device> {
        self.devices.get_mut(&id)
    }

    /// Returns switch data for `id`.
    pub fn switch(&self, id: SwitchId) -> Option<&Switch> {
        self.switches.get(&id)
    }

    /// Returns the node identifier mapped from `name`.
    pub fn node_id_by_name(&self, name: &str) -> Option<NodeId> {
        self.nodes_by_name.get(name).copied()
    }

    /// Adds a router node and returns its identifier.
    pub fn add_router(
        &mut self,
        name: &str,
        ns: String,
        cfg: RouterConfig,
        region: Option<String>,
    ) -> RouterId {
        let id = NodeId(self.alloc_id());
        self.nodes_by_name.insert(name.to_string(), id);
        self.routers.insert(
            id,
            Router {
                id,
                name: name.to_string(),
                ns,
                region,
                cfg,
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
    /// Call [`add_device_iface`] one or more times to attach interfaces, then
    /// optionally [`set_device_default_via`] to override the default route
    /// interface (first interface by default).
    pub fn add_device(&mut self, name: &str, ns: String) -> DeviceId {
        let id = NodeId(self.alloc_id());
        self.nodes_by_name.insert(name.to_string(), id);
        self.devices.insert(
            id,
            Device {
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
    pub fn add_device_iface(
        &mut self,
        device: DeviceId,
        ifname: &str,
        router: RouterId,
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
        dev.interfaces.push(DeviceIface {
            ifname: ifname.to_string(),
            uplink: downlink,
            ip: Some(assigned),
            impair,
            idx,
        });
        Ok(assigned)
    }

    /// Changes which interface carries the default route.
    pub fn set_device_default_via(&mut self, device: DeviceId, ifname: &str) -> Result<()> {
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
    pub fn router_downlink_gw_for_switch(&self, sw: SwitchId) -> Result<Ipv4Addr> {
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
    ) -> SwitchId {
        let id = NodeId(self.alloc_id());
        self.nodes_by_name.insert(name.to_string(), id);
        self.switches.insert(
            id,
            Switch {
                id,
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
        router: RouterId,
        sw: SwitchId,
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
        router: RouterId,
        sw: SwitchId,
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
            .cfg
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

    /// Builds all namespaces, links, addressing, routing, and NAT state.
    pub async fn build(&mut self, region_latencies: &[(String, String, u32)]) -> Result<()> {
        debug!("build: ensure /var/run/netns exists");
        ensure_netns_dir()?;
        let root_ns = self.cfg.root_ns.clone();

        for ns in self.all_ns_names() {
            debug!(ns = %ns, "build: create named netns");
            create_named_netns(&ns).await?;
        }

        // Namespaces may inherit host nftables rules (including drop policies).
        // Start from a clean ruleset so lab behavior is deterministic.
        for ns in self.all_ns_names() {
            if let Err(err) = run_nft_in(&ns, "flush ruleset").await {
                debug!(ns = %ns, error = %err, "build: nft flush failed; continuing");
            }
        }

        // IX bridge in the lab root namespace.
        let ix_br = self.cfg.ix_br.clone();
        let ix_cidr = format!("{}/{}", self.cfg.ix_gw, self.cfg.ix_cidr.prefix_len());
        self.with_netns(&root_ns, {
            let root_ns = root_ns.clone();
            let ix_br = ix_br.clone();
            let ix_cidr = ix_cidr.clone();
            async move |h| {
                debug!(bridge = %ix_br, root_ns = %root_ns, "build: ensure lab-root IX bridge");
                h.set_link_up("lo").await?;
                h.ensure_link_deleted(&ix_br).await.ok();
                h.add_bridge(&ix_br).await?;
                h.set_link_up(&ix_br).await?;
                h.add_addr4(&ix_br, &ix_cidr).await?;
                Ok(())
            }
        })
        .await?;
        set_sysctl_in(&root_ns, "net/ipv4/ip_forward", "1")?;

        // Routers attached to IX.
        for (id, router) in self.routers.clone() {
            if router.uplink != Some(self.ix_sw) {
                continue;
            }
            let root_if = self.root_if("i", id.0);
            let ns_if = "ix".to_string();
            let ix_br = self.cfg.ix_br.clone();
            let ix_gw = self.cfg.ix_gw;
            self.with_netns(&root_ns, {
                let root_if = root_if.clone();
                let ns_if = ns_if.clone();
                let ix_br = ix_br.clone();
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

            let ix_cidr = format!(
                "{}/{}",
                router.upstream_ip.unwrap(),
                self.cfg.ix_cidr.prefix_len()
            );
            self.with_netns(&router.ns, {
                let ns_if = ns_if.clone();
                let ix_cidr = ix_cidr.clone();
                async move |h| {
                    h.set_link_up("lo").await?;
                    h.set_link_up(&ns_if).await?;
                    h.add_addr4(&ns_if, &ix_cidr).await?;
                    h.add_default_route_v4(ix_gw).await?;
                    Ok(())
                }
            })
            .await?;
            set_sysctl_in(&router.ns, "net/ipv4/ip_forward", "1")?;

            if router.cfg.cgnat {
                apply_isp_cgnat(&router.ns, &ns_if).await?;
            }
            if let Some(nat) = router.cfg.nat {
                apply_home_nat(
                    &router.ns,
                    nat,
                    &router.cfg.downlink_bridge,
                    &ns_if,
                    router.upstream_ip.unwrap(),
                )
                .await?;
            }
        }

        // Inter-region latency (per-destination netem on IX links).
        if !region_latencies.is_empty() {
            let mut region_targets: HashMap<String, Vec<Ipv4Net>> = HashMap::new();
            for router in self.routers.values() {
                if router.uplink != Some(self.ix_sw) {
                    continue;
                }
                let Some(region) = router.region.as_ref() else {
                    continue;
                };
                if let Some(ix_ip) = router.upstream_ip {
                    if let Ok(cidr) = Ipv4Net::new(ix_ip, 32) {
                        region_targets.entry(region.clone()).or_default().push(cidr);
                    }
                }
                if router.cfg.downstream_pool == DownstreamPool::Public {
                    if let Some(cidr) = router.downstream_cidr {
                        region_targets.entry(region.clone()).or_default().push(cidr);
                    }
                }
            }

            for router in self.routers.values() {
                if router.uplink != Some(self.ix_sw) {
                    continue;
                }
                let Some(region) = router.region.as_ref() else {
                    continue;
                };
                let mut filters = Vec::new();
                for (from, to, latency) in region_latencies {
                    if from != region {
                        continue;
                    }
                    if let Some(targets) = region_targets.get(to) {
                        for cidr in targets {
                            filters.push((*cidr, *latency));
                        }
                    }
                }
                if !filters.is_empty() {
                    debug!(
                        ns = %router.ns,
                        ifname = "ix",
                        filters = filters.len(),
                        "build: apply inter-region latency filters"
                    );
                    apply_region_latency(&router.ns, "ix", &filters)?;
                }
            }
        }

        // Ensure router downlink bridges before attaching subscriber links/devices.
        for router in self.routers.values() {
            if let Some(sw) = router.downlink {
                let sw = self.switches.get(&sw).unwrap();
                let br = sw.bridge.clone().unwrap_or_else(|| "br-lan".to_string());
                let lan_cidr = format!("{}/{}", sw.gw.unwrap(), sw.cidr.unwrap().prefix_len());
                self.with_netns(&router.ns, async move |h| {
                    h.set_link_up("lo").await?;
                    h.ensure_link_deleted(&br).await.ok();
                    h.add_bridge(&br).await?;
                    h.set_link_up(&br).await?;
                    h.add_addr4(&br, &lan_cidr).await?;
                    Ok(())
                })
                .await?;
            }
        }

        // Routers attached to another router (subscriber links).
        for (id, router) in self.routers.clone() {
            let Some(uplink) = router.uplink else {
                continue;
            };
            if uplink == self.ix_sw {
                continue;
            }
            let sw = self
                .switches
                .get(&uplink)
                .ok_or_else(|| anyhow!("router uplink switch missing"))?;
            let owner = sw
                .owner_router
                .ok_or_else(|| anyhow!("uplink switch missing owner"))?;
            let owner_ns = self.routers.get(&owner).unwrap().ns.clone();
            let bridge = sw.bridge.clone().unwrap_or_else(|| "br-lan".to_string());
            let gw_ip = sw.gw.ok_or_else(|| anyhow!("uplink switch missing gw"))?;

            let root_a = self.root_if("a", id.0);
            let root_b = self.root_if("b", id.0);
            self.with_netns(&root_ns, {
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
            self.with_netns(&owner_ns, {
                let root_a = root_a.clone();
                let owner_if = owner_if.clone();
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
            let wan_cidr = format!(
                "{}/{}",
                router.upstream_ip.unwrap(),
                sw.cidr.unwrap().prefix_len()
            );
            self.with_netns(&router.ns, {
                let root_b = root_b.clone();
                let wan_if = wan_if.clone();
                let wan_cidr = wan_cidr.clone();
                async move |h| {
                    h.set_link_up("lo").await?;
                    h.rename_link(&root_b, &wan_if).await?;
                    h.set_link_up(&wan_if).await?;
                    h.add_addr4(&wan_if, &wan_cidr).await?;
                    h.add_default_route_v4(gw_ip).await?;
                    Ok(())
                }
            })
            .await?;
            set_sysctl_in(&router.ns, "net/ipv4/ip_forward", "1")?;

            if let Some(nat) = router.cfg.nat {
                apply_home_nat(
                    &router.ns,
                    nat,
                    &router.cfg.downlink_bridge,
                    &wan_if,
                    router.upstream_ip.unwrap(),
                )
                .await?;
            }
        }

        // Devices — one IfaceBuild per (device, interface) pair.
        let mut iface_data = Vec::new();
        for dev in self.devices.values() {
            for iface in &dev.interfaces {
                let sw = self.switches.get(&iface.uplink).ok_or_else(|| {
                    anyhow!(
                        "device '{}' iface '{}' switch missing",
                        dev.name,
                        iface.ifname
                    )
                })?;
                let gw_router = sw.owner_router.ok_or_else(|| {
                    anyhow!(
                        "device '{}' iface '{}' switch missing owner",
                        dev.name,
                        iface.ifname
                    )
                })?;
                let gw = sw.gw.ok_or_else(|| anyhow!("device switch missing gw"))?;
                let gw_br = sw.bridge.clone().unwrap_or_else(|| "br-lan".to_string());
                let gw_ns = self.routers.get(&gw_router).unwrap().ns.clone();
                iface_data.push(IfaceBuild {
                    dev_ns: dev.ns.clone(),
                    gw_ns,
                    gw_ip: gw,
                    gw_br,
                    dev_ip: iface.ip.unwrap(),
                    prefix_len: sw.cidr.unwrap().prefix_len(),
                    impair: iface.impair,
                    ifname: iface.ifname.clone(),
                    is_default: iface.ifname == dev.default_via,
                    idx: iface.idx,
                });
            }
        }

        for iface in iface_data {
            self.wire_iface(iface).await?;
        }

        // Lab-root return routes to public downstreams behind IX routers.
        let return_routes: Vec<(Ipv4Addr, u8, Ipv4Addr)> = self
            .routers
            .values()
            .filter(|router| {
                router.uplink == Some(self.ix_sw)
                    && router.cfg.downstream_pool == DownstreamPool::Public
                    && router.downstream_cidr.is_some()
            })
            .map(|router| {
                let cidr = router.downstream_cidr.expect("checked above");
                (
                    cidr.addr(),
                    cidr.prefix_len(),
                    router.upstream_ip.expect("ix uplink ip"),
                )
            })
            .collect();
        self.with_netns(&root_ns, async move |h| {
            for (net, prefix_len, via) in return_routes {
                debug!(
                    dst = %format!("{}/{}", net, prefix_len),
                    via = %via,
                    "build: add lab-root return route"
                );
                h.add_route_v4(&format!("{}/{}", net, prefix_len), via)
                    .await
                    .ok();
            }
            Ok(())
        })
        .await
        .ok();

        Ok(())
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Wire one device interface: create veth pair, move ends to correct
    /// namespaces, assign IP, and optionally add a default route and impairment.
    async fn wire_iface(&self, dev: IfaceBuild) -> Result<()> {
        debug!(
            dev_ns = %dev.dev_ns,
            gw_ns = %dev.gw_ns,
            gw_ip = %dev.gw_ip,
            gw_br = %dev.gw_br,
            dev_ip = %dev.dev_ip,
            ifname = %dev.ifname,
            impair = ?dev.impair,
            is_default = dev.is_default,
            "build: connect device interface to gateway"
        );
        let root_gw = self.root_if("g", dev.idx);
        let root_dev = self.root_if("e", dev.idx);
        let root_ns = self.cfg.root_ns.clone();
        self.with_netns(&root_ns, {
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

        let ip_cidr = format!("{}/{}", dev.dev_ip, dev.prefix_len);
        let ifname = dev.ifname.clone();
        let is_default = dev.is_default;
        let gw_ip = dev.gw_ip;
        self.with_netns(&dev.dev_ns, {
            let root_dev = root_dev.clone();
            let ifname = ifname.clone();
            let ip_cidr = ip_cidr.clone();
            async move |h| {
                h.set_link_up("lo").await?;
                h.rename_link(&root_dev, &ifname).await?;
                h.set_link_up(&ifname).await?;
                h.add_addr4(&ifname, &ip_cidr).await?;
                if is_default {
                    h.add_default_route_v4(gw_ip).await?;
                }
                Ok(())
            }
        })
        .await?;

        self.with_netns(&dev.gw_ns, {
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

    fn alloc_from_switch(&mut self, sw: SwitchId) -> Result<Ipv4Addr> {
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

    fn ns_name(&self, name: &str) -> String {
        format!("{}-{}", self.cfg.prefix, name)
    }

    fn root_if(&self, tag: &str, id: u64) -> String {
        format!("{}{}{}", self.cfg.prefix, tag, id)
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

/// Creates a namespace entry and registers it for cleanup.
pub async fn create_named_netns(name: &str) -> Result<()> {
    netns::create_named_netns(name).await?;
    resources().register_netns(name);
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
            unreachable!("apply_home_nat called with non-home NAT mode {:?}", mode)
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

// ─────────────────────────────────────────────
// rtnetlink helpers
// ─────────────────────────────────────────────

struct Netlink {
    handle: Handle,
    ops: u64,
}

impl Netlink {
    fn new(handle: Handle) -> Self {
        Self { handle, ops: 0 }
    }

    fn bump(&mut self) {
        self.ops = self.ops.wrapping_add(1);
    }

    async fn link_index(&mut self, ifname: &str) -> Result<u32> {
        self.bump();
        debug!(ifname = %ifname, "netlink: lookup link index");
        let mut links = self
            .handle
            .link()
            .get()
            .match_name(ifname.to_string())
            .execute();
        links
            .try_next()
            .await?
            .map(|msg| msg.header.index)
            .ok_or_else(|| anyhow!("link not found: {}", ifname))
    }

    async fn ensure_link_deleted(&mut self, ifname: &str) -> Result<()> {
        self.bump();
        debug!(ifname = %ifname, "netlink: ensure link deleted");
        if let Ok(idx) = self.link_index(ifname).await {
            debug!(ifname = %ifname, idx, "netlink: delete link");
            self.handle.link().del(idx).execute().await?;
        }
        Ok(())
    }

    async fn add_bridge(&mut self, name: &str) -> Result<()> {
        self.bump();
        debug!(bridge = %name, "netlink: add bridge");
        if let Err(err) = self
            .handle
            .link()
            .add(LinkBridge::new(name).build())
            .execute()
            .await
        {
            if is_eexist(&err) {
                debug!(bridge = %name, "netlink: bridge already exists");
            } else {
                return Err(err.into());
            }
        }
        resources().register_link(name);
        Ok(())
    }

    async fn add_veth(&mut self, a: &str, b: &str) -> Result<()> {
        self.bump();
        debug!(a = %a, b = %b, "netlink: add veth pair");
        self.handle
            .link()
            .add(LinkVeth::new(a, b).build())
            .execute()
            .await?;
        resources().register_link(a);
        resources().register_link(b);
        Ok(())
    }

    async fn set_link_up(&mut self, ifname: &str) -> Result<()> {
        self.bump();
        debug!(ifname = %ifname, "netlink: set link up");
        let idx = self.link_index(ifname).await?;
        let msg = LinkUnspec::new_with_index(idx).up().build();
        self.handle.link().change(msg).execute().await?;
        Ok(())
    }

    async fn rename_link(&mut self, from: &str, to: &str) -> Result<()> {
        self.bump();
        debug!(from = %from, to = %to, "netlink: rename link");
        let idx = self.link_index(from).await?;
        let msg = LinkUnspec::new_with_index(idx).name(to.to_string()).build();
        self.handle.link().change(msg).execute().await?;
        Ok(())
    }

    async fn set_master(&mut self, ifname: &str, master: &str) -> Result<()> {
        self.bump();
        debug!(ifname = %ifname, master = %master, "netlink: set master");
        let idx = self.link_index(ifname).await?;
        let midx = self.link_index(master).await?;
        let msg = LinkUnspec::new_with_index(idx).controller(midx).build();
        self.handle.link().set(msg).execute().await?;
        Ok(())
    }

    async fn move_link_to_netns(&mut self, ifname: &str, ns_fd: &File) -> Result<()> {
        self.bump();
        debug!(ifname = %ifname, "netlink: move link to netns");
        let idx = self.link_index(ifname).await?;
        let msg = LinkUnspec::new_with_index(idx)
            .setns_by_fd(ns_fd.as_raw_fd())
            .build();
        self.handle.link().change(msg).execute().await?;
        Ok(())
    }

    async fn add_addr4(&mut self, ifname: &str, cidr: &str) -> Result<()> {
        self.bump();
        debug!(ifname = %ifname, cidr = %cidr, "netlink: add IPv4 address");
        let idx = self.link_index(ifname).await?;
        let (ip, prefix) = parse_cidr_v4(cidr)?;
        if let Err(err) = self
            .handle
            .address()
            .add(idx, ip.into(), prefix)
            .execute()
            .await
        {
            if is_eexist(&err) {
                debug!(ifname = %ifname, cidr = %cidr, "netlink: IPv4 address already exists");
                return Ok(());
            }
            return Err(err.into());
        }
        Ok(())
    }

    async fn add_default_route_v4(&mut self, via: Ipv4Addr) -> Result<()> {
        self.bump();
        debug!(via = %via, "netlink: add default route");
        let msg = RouteMessageBuilder::<Ipv4Addr>::new().gateway(via).build();
        if let Err(err) = self.handle.route().add(msg).execute().await {
            if is_eexist(&err) {
                debug!(via = %via, "netlink: default route already exists");
                return Ok(());
            }
            return Err(err.into());
        }
        Ok(())
    }

    async fn add_route_v4(&mut self, dst_cidr: &str, via: Ipv4Addr) -> Result<()> {
        self.bump();
        debug!(dst = %dst_cidr, via = %via, "netlink: add route");
        let (dst, prefix) = parse_cidr_v4(dst_cidr)?;
        let msg = RouteMessageBuilder::<Ipv4Addr>::new()
            .destination_prefix(dst, prefix)
            .gateway(via)
            .build();
        if let Err(err) = self.handle.route().add(msg).execute().await {
            if is_eexist(&err) {
                debug!(dst = %dst_cidr, via = %via, "netlink: route already exists");
                return Ok(());
            }
            return Err(err.into());
        }
        Ok(())
    }
}

fn is_eexist(err: &rtnetlink::Error) -> bool {
    match err {
        rtnetlink::Error::NetlinkError(msg) => msg
            .code
            .map(|code| -code.get() == nix::libc::EEXIST)
            .unwrap_or(false),
        _ => false,
    }
}

fn parse_cidr_v4(cidr: &str) -> Result<(Ipv4Addr, u8)> {
    let (addr, len) = cidr
        .split_once('/')
        .ok_or_else(|| anyhow!("bad cidr: {}", cidr))?;
    Ok((addr.parse()?, len.parse()?))
}
