use anyhow::{anyhow, bail, Context, Result};
use futures::stream::TryStreamExt;
use ipnet::Ipv4Net;
use nix::sched::{setns, CloneFlags};
use rtnetlink::{
    new_connection, Handle, LinkBridge, LinkUnspec, LinkVeth, NetworkNamespace, RouteMessageBuilder,
};
use std::collections::{HashMap, HashSet};
use std::fs::{create_dir_all, File};
use std::io::Write as IoWrite;
use std::net::Ipv4Addr;
use std::os::fd::AsRawFd;
use std::process::ExitStatus;
use std::sync::mpsc;
use std::sync::{Mutex, Once, OnceLock};
use std::thread;
use tracing::debug;

use crate::{Impair, NatMode};
use nix::libc;

#[derive(Clone, Debug)]
pub struct CoreConfig {
    pub prefix: String,
    pub ix_br: String,
    pub ix_gw: Ipv4Addr,
    pub ix_cidr: Ipv4Net,
    pub private_cidr: Ipv4Net,
    pub public_cidr: Ipv4Net,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(pub u64);

pub type DeviceId = NodeId;
pub type RouterId = NodeId;
pub type SwitchId = NodeId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownstreamPool {
    Private,
    Public,
}

#[derive(Clone, Debug)]
pub struct RouterConfig {
    pub nat: Option<NatMode>,
    pub cgnat: bool,
    pub downlink_bridge: String,
    pub downstream_pool: DownstreamPool,
}

#[derive(Clone, Debug)]
pub struct Device {
    pub id: DeviceId,
    pub name: String,
    pub ns: String,
    pub uplink: Option<SwitchId>,
    pub ip: Option<Ipv4Addr>,
    pub impair_upstream: Option<Impair>,
}

#[derive(Clone, Debug)]
pub struct Router {
    pub id: RouterId,
    pub name: String,
    pub ns: String,
    pub region: Option<String>,
    pub cfg: RouterConfig,
    pub uplink: Option<SwitchId>,
    pub upstream_ip: Option<Ipv4Addr>,
    pub downlink: Option<SwitchId>,
    pub downstream_cidr: Option<Ipv4Net>,
    pub downstream_gw: Option<Ipv4Addr>,
}

#[derive(Clone, Debug)]
pub struct Switch {
    pub id: SwitchId,
    pub name: String,
    pub cidr: Option<Ipv4Net>,
    pub gw: Option<Ipv4Addr>,
    pub owner_router: Option<RouterId>,
    pub bridge: Option<String>,
    next_host: u8,
}

pub struct LabCore {
    cfg: CoreConfig,
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

#[derive(Default)]
pub struct ResourceList {
    state: Mutex<ResourceState>,
}

static RESOURCES: OnceLock<ResourceList> = OnceLock::new();
static INIT_HOOKS: Once = Once::new();

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
    pub fn register_link(&self, name: &str) {
        let mut st = self.state.lock().unwrap();
        st.links.insert(name.to_string());
    }

    pub fn register_netns(&self, name: &str) {
        let mut st = self.state.lock().unwrap();
        st.netns.insert(name.to_string());
    }

    pub fn register_prefix(&self, prefix: &str) {
        let mut st = self.state.lock().unwrap();
        st.prefixes.insert(prefix.to_string());
    }

    pub fn cleanup_all(&self) {
        let (links, netns) = {
            let st = self.state.lock().unwrap();
            (st.links.clone(), st.netns.clone())
        };
        for link in links {
            let _ = std::process::Command::new("ip")
                .args(["link", "del", &link])
                .stderr(std::process::Stdio::null())
                .status();
        }
        for ns in netns {
            let _ = std::process::Command::new("ip")
                .args(["netns", "del", &ns])
                .stderr(std::process::Stdio::null())
                .status();
        }
    }

    pub fn cleanup_everything_with_prefix(&self, prefix: &str) {
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
                        if name.starts_with(prefix) {
                            let _ = std::process::Command::new("ip")
                                .args(["link", "del", name])
                                .stderr(std::process::Stdio::null())
                                .status();
                        }
                    }
                }
            }
        }

        let output = std::process::Command::new("ip")
            .args(["netns", "list"])
            .output();
        if let Ok(out) = output {
            if let Ok(text) = String::from_utf8(out.stdout) {
                for line in text.lines() {
                    let name = line.split_whitespace().next().unwrap_or_default();
                    if name.starts_with(prefix) {
                        let _ = std::process::Command::new("ip")
                            .args(["netns", "del", name])
                            .stderr(std::process::Stdio::null())
                            .status();
                    }
                }
            }
        }
    }

    pub fn cleanup_everything(&self) {
        let prefixes = {
            let st = self.state.lock().unwrap();
            st.prefixes.clone()
        };
        for prefix in prefixes {
            self.cleanup_everything_with_prefix(&prefix);
        }
    }
}
impl LabCore {
    pub fn new(cfg: CoreConfig) -> Self {
        let mut core = Self {
            cfg,
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

    pub fn ix_gw(&self) -> Ipv4Addr {
        self.cfg.ix_gw
    }

    pub fn ix_br(&self) -> &str {
        &self.cfg.ix_br
    }

    pub fn alloc_ix_ip_low(&mut self) -> Ipv4Addr {
        let o = self.cfg.ix_gw.octets();
        let ip = Ipv4Addr::new(o[0], o[1], o[2], self.next_ix_low);
        self.next_ix_low = self.next_ix_low.saturating_add(1);
        ip
    }

    pub fn alloc_ix_ip_high(&mut self) -> Ipv4Addr {
        let o = self.cfg.ix_gw.octets();
        let ip = Ipv4Addr::new(o[0], o[1], o[2], self.next_ix_high);
        self.next_ix_high = self.next_ix_high.saturating_sub(1);
        ip
    }

    pub fn ix_sw(&self) -> SwitchId {
        self.ix_sw
    }

    pub fn router_ns(&self, id: RouterId) -> Result<&str> {
        self.routers
            .get(&id)
            .map(|r| r.ns.as_str())
            .ok_or_else(|| anyhow!("unknown router id"))
    }

    pub fn device_ns(&self, id: DeviceId) -> Result<&str> {
        self.devices
            .get(&id)
            .map(|d| d.ns.as_str())
            .ok_or_else(|| anyhow!("unknown device id"))
    }

    pub fn router(&self, id: RouterId) -> Option<&Router> {
        self.routers.get(&id)
    }

    pub fn device(&self, id: DeviceId) -> Option<&Device> {
        self.devices.get(&id)
    }

    pub fn switch(&self, id: SwitchId) -> Option<&Switch> {
        self.switches.get(&id)
    }

    pub fn node_id_by_name(&self, name: &str) -> Option<NodeId> {
        self.nodes_by_name.get(name).copied()
    }

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

    pub fn add_device(&mut self, name: &str, ns: String, impair: Option<Impair>) -> DeviceId {
        let id = NodeId(self.alloc_id());
        self.nodes_by_name.insert(name.to_string(), id);
        self.devices.insert(
            id,
            Device {
                id,
                name: name.to_string(),
                ns,
                uplink: None,
                ip: None,
                impair_upstream: impair,
            },
        );
        id
    }

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

    pub fn connect_device_to_router(
        &mut self,
        device: DeviceId,
        router: RouterId,
    ) -> Result<Ipv4Addr> {
        let downlink = self
            .routers
            .get(&router)
            .and_then(|r| r.downlink)
            .ok_or_else(|| anyhow!("router missing downlink"))?;
        let assigned = self.alloc_from_switch(downlink)?;
        let dev = self
            .devices
            .get_mut(&device)
            .ok_or_else(|| anyhow!("unknown device id"))?;
        dev.uplink = Some(downlink);
        dev.ip = Some(assigned);
        Ok(assigned)
    }

    pub async fn build(&mut self, region_latencies: &[(String, String, u32)]) -> Result<()> {
        struct DevBuild {
            dev_ns: String,
            gw_ns: String,
            gw_ip: Ipv4Addr,
            gw_br: String,
            dev_ip: Ipv4Addr,
            prefix_len: u8,
            impair: Option<Impair>,
            idx: u32,
        }

        debug!("build: ensure /var/run/netns exists");
        ensure_netns_dir()?;

        for ns in self.all_ns_names() {
            debug!(ns = %ns, "build: create named netns");
            create_named_netns(&ns).await?;
        }

        // IX bridge in root namespace.
        let ix_br = self.cfg.ix_br.clone();
        let ix_cidr = format!("{}/{}", self.cfg.ix_gw, self.cfg.ix_cidr.prefix_len());
        with_root_netlink(async |h| {
            debug!(bridge = %ix_br, "build: ensure root IX bridge");
            h.ensure_link_deleted(&ix_br).await.ok();
            h.add_bridge(&ix_br).await?;
            h.set_link_up(&ix_br).await?;
            h.add_addr4(&ix_br, &ix_cidr).await?;
            Ok(())
        })
        .await?;
        set_sysctl_root("net/ipv4/ip_forward", "1")?;

        // Routers attached to IX.
        for (id, router) in self.routers.clone() {
            if router.uplink != Some(self.ix_sw) {
                continue;
            }
            let root_if = self.root_if("i", id.0);
            let ns_if = "ix".to_string();
            let ix_br = self.cfg.ix_br.clone();
            let ix_gw = self.cfg.ix_gw;
            let router_ns = router.ns.clone();
            let ns_if2 = ns_if.clone();
            let root_if2 = root_if.clone();
            with_root_netlink(async |h| {
                h.ensure_link_deleted(&root_if2).await.ok();
                h.ensure_link_deleted(&ns_if2).await.ok();
                h.add_veth(&root_if2, &ns_if2).await?;
                h.set_master(&root_if2, &ix_br).await?;
                h.set_link_up(&root_if2).await?;
                h.move_link_to_netns(&ns_if2, &open_netns_fd(&router_ns)?)
                    .await?;
                Ok(())
            })
            .await?;

            let ix_cidr = format!(
                "{}/{}",
                router.upstream_ip.unwrap(),
                self.cfg.ix_cidr.prefix_len()
            );
            with_netns(&router.ns, async |h| {
                h.set_link_up("lo").await?;
                h.set_link_up(&ns_if).await?;
                h.add_addr4(&ns_if, &ix_cidr).await?;
                h.add_default_route_v4(ix_gw).await?;
                Ok(())
            })
            .await?;
            set_sysctl_in(&router.ns, "net/ipv4/ip_forward", "1")?;

            if router.cfg.cgnat {
                apply_isp_cgnat(&router.ns, &ns_if).await?;
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
                with_netns(&router.ns, async |h| {
                    h.set_link_up("lo").await?;
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
            let owner_ns2 = owner_ns.clone();
            let router_ns2 = router.ns.clone();
            with_root_netlink(async |h| {
                h.ensure_link_deleted(&root_a).await.ok();
                h.ensure_link_deleted(&root_b).await.ok();
                h.add_veth(&root_a, &root_b).await?;
                h.move_link_to_netns(&root_a, &open_netns_fd(&owner_ns2)?)
                    .await?;
                h.move_link_to_netns(&root_b, &open_netns_fd(&router_ns2)?)
                    .await?;
                Ok(())
            })
            .await?;

            let owner_if = format!("h{}", id.0);
            with_netns(&owner_ns, async |h| {
                h.rename_link(&root_a, &owner_if).await?;
                h.set_link_up(&owner_if).await?;
                h.set_master(&owner_if, &bridge).await?;
                Ok(())
            })
            .await?;

            let wan_if = "wan".to_string();
            let wan_cidr = format!(
                "{}/{}",
                router.upstream_ip.unwrap(),
                sw.cidr.unwrap().prefix_len()
            );
            with_netns(&router.ns, async |h| {
                h.set_link_up("lo").await?;
                h.rename_link(&root_b, &wan_if).await?;
                h.set_link_up(&wan_if).await?;
                h.add_addr4(&wan_if, &wan_cidr).await?;
                h.add_default_route_v4(gw_ip).await?;
                Ok(())
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

        // Devices
        let mut dev_data = Vec::new();
        for dev in self.devices.values() {
            let sw_id = dev.uplink.ok_or_else(|| anyhow!("device missing uplink"))?;
            let sw = self
                .switches
                .get(&sw_id)
                .ok_or_else(|| anyhow!("device switch missing"))?;
            let gw_router = sw
                .owner_router
                .ok_or_else(|| anyhow!("device switch missing owner"))?;
            let gw = sw.gw.ok_or_else(|| anyhow!("device switch missing gw"))?;
            let gw_br = sw.bridge.clone().unwrap_or_else(|| "br-lan".to_string());
            let gw_ns = self.routers.get(&gw_router).unwrap().ns.clone();
            dev_data.push(DevBuild {
                dev_ns: dev.ns.clone(),
                gw_ns,
                gw_ip: gw,
                gw_br,
                dev_ip: dev.ip.unwrap(),
                prefix_len: sw.cidr.unwrap().prefix_len(),
                impair: dev.impair_upstream.clone(),
                idx: dev.id.0 as u32,
            });
        }

        for dev in dev_data {
            debug!(
                dev_ns = %dev.dev_ns,
                gw_ns = %dev.gw_ns,
                gw_ip = %dev.gw_ip,
                gw_br = %dev.gw_br,
                dev_ip = %dev.dev_ip,
                impair = ?dev.impair,
                "build: connect device to gateway"
            );
            let root_gw = self.root_if("g", dev.idx as u64);
            let root_dev = self.root_if("e", dev.idx as u64);

            with_root_netlink(async |h| {
                h.ensure_link_deleted(&root_gw).await.ok();
                h.ensure_link_deleted(&root_dev).await.ok();
                h.add_veth(&root_gw, &root_dev).await?;
                h.move_link_to_netns(&root_gw, &open_netns_fd(&dev.gw_ns)?)
                    .await?;
                h.move_link_to_netns(&root_dev, &open_netns_fd(&dev.dev_ns)?)
                    .await?;
                Ok(())
            })
            .await?;

            let ip_cidr = format!("{}/{}", dev.dev_ip, dev.prefix_len);
            with_netns(&dev.dev_ns, async |h| {
                h.set_link_up("lo").await?;
                h.rename_link(&root_dev, "eth0").await?;
                h.set_link_up("eth0").await?;
                h.add_addr4("eth0", &ip_cidr).await?;
                h.add_default_route_v4(dev.gw_ip).await?;
                Ok(())
            })
            .await?;

            with_netns(&dev.gw_ns, async |h| {
                h.rename_link(&root_gw, &format!("v{}", dev.idx)).await?;
                h.set_link_up(&format!("v{}", dev.idx)).await?;
                h.set_master(&format!("v{}", dev.idx), &dev.gw_br).await?;
                Ok(())
            })
            .await?;

            if let Some(imp) = dev.impair {
                apply_impair_in(&dev.dev_ns, "eth0", imp);
            }
        }

        // Root-ns return routes to public downstreams behind IX routers.
        with_root_netlink(async |h| {
            for router in self.routers.values() {
                if router.uplink != Some(self.ix_sw) {
                    continue;
                }
                if router.cfg.downstream_pool != DownstreamPool::Public {
                    continue;
                }
                if let Some(cidr) = router.downstream_cidr {
                    let net = cidr.addr();
                    debug!(
                        dst = %format!("{}/{}", net, cidr.prefix_len()),
                        via = %router.upstream_ip.unwrap(),
                        "build: add root return route"
                    );
                    h.add_route_v4(
                        &format!("{}/{}", net, cidr.prefix_len()),
                        router.upstream_ip.unwrap(),
                    )
                    .await
                    .ok();
                }
            }
            Ok(())
        })
        .await
        .ok();

        let _ = region_latencies;
        Ok(())
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

    pub fn all_ns_names(&self) -> Vec<String> {
        let mut v = Vec::new();
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

pub fn ensure_netns_dir() -> Result<()> {
    debug!(path = "/var/run/netns", "netns: ensure directory");
    create_dir_all("/var/run/netns").context("create /var/run/netns")
}

pub fn open_netns_fd(name: &str) -> Result<File> {
    debug!(ns = %name, "netns: open namespace fd");
    let path = format!("/var/run/netns/{}", name);
    match File::open(&path) {
        Ok(f) => Ok(f),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = std::process::Command::new("ip")
                .args(["netns", "add", name])
                .stderr(std::process::Stdio::null())
                .status();
            File::open(&path).with_context(|| format!("open netns fd for '{}'", name))
        }
        Err(e) => Err(e).with_context(|| format!("open netns fd for '{}'", name)),
    }
}

pub fn cleanup_netns(name: &str) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let name = name.to_string();
        handle.spawn(async move {
            let _ = NetworkNamespace::del(name).await;
        });
        return;
    }
    let _ = std::process::Command::new("ip")
        .args(["netns", "del", name])
        .status();
}

pub async fn create_named_netns(name: &str) -> Result<()> {
    debug!(ns = %name, "netns: create named namespace");
    let _ = NetworkNamespace::del(name.to_string()).await;
    let _ = std::process::Command::new("ip")
        .args(["netns", "add", name])
        .stderr(std::process::Stdio::null())
        .status();
    let _ = open_netns_fd(name)?;
    resources().register_netns(name);
    Ok(())
}

pub fn spawn_in_netns_thread<F, R>(ns: String, f: F) -> thread::JoinHandle<Result<R>>
where
    F: FnOnce() -> Result<R> + Send + 'static,
    R: Send + 'static,
{
    thread::spawn(move || {
        let orig = File::open("/proc/self/ns/net").context("open self netns")?;
        let target = open_netns_fd(&ns)?;
        setns(&target, CloneFlags::CLONE_NEWNET).context("setns target")?;
        let res = f();
        setns(&orig, CloneFlags::CLONE_NEWNET).context("restore setns")?;
        res
    })
}

pub fn with_netns_thread<F, R>(ns: &str, f: F) -> Result<R>
where
    F: FnOnce() -> Result<R> + Send + 'static,
    R: Send + 'static,
{
    let join = spawn_in_netns_thread(ns.to_string(), f);
    match join.join() {
        Ok(res) => res,
        Err(_) => Err(anyhow!("netns thread panicked")),
    }
}

pub fn run_in_netns(ns: &str, mut cmd: std::process::Command) -> Result<ExitStatus> {
    debug!(ns = %ns, cmd = ?cmd, "netns: run command");
    with_netns_thread(ns, move || cmd.status().context("run command in netns"))
}

pub fn spawn_in_netns(ns: &str, mut cmd: std::process::Command) -> Result<std::process::Child> {
    debug!(ns = %ns, cmd = ?cmd, "netns: spawn command");
    with_netns_thread(ns, move || cmd.spawn().context("spawn command in netns"))
}

pub fn set_sysctl_root(path: &str, val: &str) -> Result<()> {
    debug!(path = %path, val = %val, "sysctl: set in root");
    std::fs::write(format!("/proc/sys/{}", path), val)
        .with_context(|| format!("sysctl write {}", path))
}

pub fn set_sysctl_in(ns: &str, path: &str, val: &str) -> Result<()> {
    debug!(ns = %ns, path = %path, val = %val, "sysctl: set in namespace");
    let orig = File::open("/proc/self/ns/net")?;
    let target = open_netns_fd(ns)?;
    setns(&target, CloneFlags::CLONE_NEWNET)?;
    let res = set_sysctl_root(path, val);
    setns(&orig, CloneFlags::CLONE_NEWNET)?;
    res
}

pub async fn run_nft_in(ns: &str, rules: &str) -> Result<()> {
    debug!(ns = %ns, rules = %rules, "nft: apply rules");
    let rules = rules.to_string();
    let ns = ns.to_string();
    let ns_err = ns.clone();
    with_netns_thread(&ns, move || {
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

pub async fn apply_home_nat(
    ns: &str,
    mode: NatMode,
    lan_if: &str,
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
    };

    let rules = format!(
        r#"
table ip nat {{
    chain prerouting {{
        type nat hook prerouting priority -100;
        iif "{lan}" ct state established,related accept
    }}
    chain postrouting {{
        type nat hook postrouting priority 100;
        {snat}
    }}
}}
"#,
        lan = lan_if,
        snat = snat_rule,
    );
    run_nft_in(ns, &rules).await
}

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

pub fn apply_region_latency(ns: &str, ifname: &str, filters: &[(Ipv4Net, u32)]) -> Result<()> {
    if filters.is_empty() {
        return Ok(());
    }

    let _ = run_in_netns(ns, {
        let mut cmd = std::process::Command::new("tc");
        cmd.args(["qdisc", "del", "dev", ifname, "root"]);
        cmd.stderr(std::process::Stdio::null());
        cmd
    });

    let base_args = [
        "class", "add", "dev", ifname, "parent", "1:", "classid", "1:1", "htb", "rate", "1000mbit",
    ];
    let status = run_in_netns(ns, {
        let mut cmd = std::process::Command::new("tc");
        cmd.args([
            "qdisc", "add", "dev", ifname, "root", "handle", "1:", "htb", "default", "1", "r2q",
            "1",
        ]);
        cmd
    })?;
    if !status.success() {
        bail!("tc qdisc add failed for {}", ifname);
    }
    let status = run_in_netns(ns, {
        let mut cmd = std::process::Command::new("tc");
        cmd.args(base_args);
        cmd
    })?;
    if !status.success() {
        bail!("tc class add base failed for {}", ifname);
    }

    for (idx, (cidr, latency)) in filters.iter().enumerate() {
        let class_id = format!("1:{}", 10 + idx as u16);
        let handle = format!("{}:", 10 + idx as u16);
        let cidr_str = format!("{}/{}", cidr.addr(), cidr.prefix_len());

        let status = run_in_netns(ns, {
            let mut cmd = std::process::Command::new("tc");
            cmd.args([
                "class", "add", "dev", ifname, "parent", "1:", "classid", &class_id, "htb", "rate",
                "1000mbit",
            ]);
            cmd
        })?;
        if !status.success() {
            bail!("tc class add failed for {}", ifname);
        }

        let status = run_in_netns(ns, {
            let mut cmd = std::process::Command::new("tc");
            cmd.args([
                "qdisc",
                "add",
                "dev",
                ifname,
                "parent",
                &class_id,
                "handle",
                &handle,
                "netem",
                "delay",
                &format!("{}ms", latency),
            ]);
            cmd
        })?;
        if !status.success() {
            bail!("tc netem add failed for {}", ifname);
        }

        let status = run_in_netns(ns, {
            let mut cmd = std::process::Command::new("tc");
            cmd.args([
                "filter", "add", "dev", ifname, "protocol", "ip", "parent", "1:", "prio", "1",
                "u32", "match", "ip", "dst", &cidr_str, "flowid", &class_id,
            ]);
            cmd
        })?;
        if !status.success() {
            bail!("tc filter add failed for {}", ifname);
        }
    }

    Ok(())
}

pub fn apply_impair_in(ns: &str, ifname: &str, impair: Impair) {
    debug!(ns = %ns, ifname = %ifname, impair = ?impair, "tc: apply impairment");
    let _ = run_in_netns(ns, {
        let mut cmd = std::process::Command::new("tc");
        cmd.args(["qdisc", "del", "dev", ifname, "root"]);
        cmd.stderr(std::process::Stdio::null());
        cmd
    });

    let limits = match impair {
        Impair::Wifi => ImpairLimits {
            rate_kbit: 0,
            loss_pct: 0.0,
            latency_ms: 20,
        },
        Impair::Mobile => ImpairLimits {
            rate_kbit: 0,
            loss_pct: 1.0,
            latency_ms: 50,
        },
        Impair::Manual {
            rate,
            loss,
            latency,
        } => ImpairLimits {
            rate_kbit: rate,
            loss_pct: loss,
            latency_ms: latency,
        },
    };

    let qdisc = Qdisc::new(ifname, limits);
    if let Err(e) = qdisc.apply(ns) {
        eprintln!("warn: apply_impair_in({}): {}", ifname, e);
    }
}

#[derive(Debug, Clone, Copy)]
struct ImpairLimits {
    rate_kbit: u32,
    loss_pct: f32,
    latency_ms: u32,
}

struct Qdisc<'a> {
    ifname: &'a str,
    limits: ImpairLimits,
}

impl<'a> Qdisc<'a> {
    fn new(ifname: &'a str, limits: ImpairLimits) -> Self {
        Self { ifname, limits }
    }

    fn apply(&self, ns: &str) -> Result<()> {
        let mut cmd = std::process::Command::new("tc");
        cmd.args([
            "qdisc",
            "add",
            "dev",
            self.ifname,
            "root",
            "handle",
            "1:",
            "netem",
            "delay",
            &format!("{}ms", self.limits.latency_ms),
            "loss",
            &format!("{:.3}%", self.limits.loss_pct),
        ]);
        run_in_netns(ns, cmd)?;

        if self.limits.rate_kbit > 0 {
            let mut cmd = std::process::Command::new("tc");
            cmd.args([
                "qdisc",
                "add",
                "dev",
                self.ifname,
                "parent",
                "1:1",
                "handle",
                "10:",
                "tbf",
                "rate",
                &format!("{}kbit", self.limits.rate_kbit),
                "burst",
                "32kbit",
                "latency",
                "400ms",
            ]);
            run_in_netns(ns, cmd)?;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct TaskHandle {
    stop: mpsc::Sender<()>,
}

impl TaskHandle {
    pub(crate) fn new(stop: mpsc::Sender<()>) -> Self {
        Self { stop }
    }

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
        self.handle
            .link()
            .add(LinkBridge::new(name).build())
            .execute()
            .await?;
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
        self.handle
            .address()
            .add(idx, ip.into(), prefix)
            .execute()
            .await?;
        Ok(())
    }

    async fn add_default_route_v4(&mut self, via: Ipv4Addr) -> Result<()> {
        self.bump();
        debug!(via = %via, "netlink: add default route");
        let msg = RouteMessageBuilder::<Ipv4Addr>::new().gateway(via).build();
        self.handle.route().add(msg).execute().await?;
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
        self.handle.route().add(msg).execute().await?;
        Ok(())
    }
}

async fn with_root_netlink<F>(f: F) -> Result<()>
where
    F: AsyncFnOnce(&mut Netlink) -> Result<()>,
{
    debug!("netlink: open root rtnetlink connection");
    let (conn, handle, _) = new_connection().context("rtnetlink new_connection")?;
    tokio::spawn(conn);
    let mut nl = Netlink::new(handle);
    f(&mut nl).await
}

async fn with_netns<F>(ns: &str, f: F) -> Result<()>
where
    F: AsyncFnOnce(&mut Netlink) -> Result<()>,
{
    debug!(ns = %ns, "netlink: enter namespace");
    let orig = File::open("/proc/self/ns/net").context("open self netns")?;
    let target = open_netns_fd(ns)?;
    setns(&target, CloneFlags::CLONE_NEWNET).context("setns target")?;

    let res = async {
        let (conn, handle, _) = new_connection().context("rtnetlink new_connection in netns")?;
        tokio::spawn(conn);
        let mut nl = Netlink::new(handle);
        f(&mut nl).await
    }
    .await;

    debug!(ns = %ns, "netlink: exit namespace");
    setns(&orig, CloneFlags::CLONE_NEWNET).context("restore setns")?;
    res
}

fn parse_cidr_v4(cidr: &str) -> Result<(Ipv4Addr, u8)> {
    let (addr, len) = cidr
        .split_once('/')
        .ok_or_else(|| anyhow!("bad cidr: {}", cidr))?;
    Ok((addr.parse()?, len.parse()?))
}
