#![allow(dead_code)]

use anyhow::{anyhow, Context, Result};
use futures::stream::TryStreamExt;
use netlink_packet_route::rtnl::link::nlas::InfoKind;
use nix::{
    mount::{mount, MsFlags},
    sched::{setns, unshare, CloneFlags},
    sys::signal::{kill, Signal},
    unistd::{fork, ForkResult, Pid},
};
use rtnetlink::{new_connection, Handle};
use std::{
    collections::HashMap,
    ffi::OsStr,
    fs::{create_dir_all, File},
    io::Write,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    os::fd::{AsRawFd, RawFd},
    path::Path,
    time::Duration,
};

/// High-level API

#[derive(Clone, Copy, Debug)]
pub enum IspMode {
    NoCgnat,
    Cgnat { pool_cidr: &'static str }, // currently informational; we just NAT at ISP edge
}

#[derive(Clone, Copy, Debug)]
pub enum NatMode {
    DestinationIndependent,
    DestinationDependent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IspId(u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DcId(u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HomeId(u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DeviceId(u32);

#[derive(Clone, Debug)]
pub struct ObservedAddr {
    pub observed: SocketAddr,
}

struct Ns {
    name: String,
}

struct Isp {
    ns: Ns,
    mode: IspMode,
    // ix-side IP (public)
    ix_ip: Ipv4Addr,
}

struct Dc {
    ns: Ns,
    ix_ip: Ipv4Addr,
    // dc LAN for servers
    lan_gw: Ipv4Addr,
}

struct Home {
    ns: Ns,
    isp: IspId,
    nat: NatMode,
    wan_ip: Ipv4Addr,
    lan_gw: Ipv4Addr,
}

struct Device {
    ns: Ns,
    home: HomeId,
    ip: Ipv4Addr,
}

pub struct Lab {
    prefix: String,

    // root IX bridge name
    ix_br: String,
    ix_cidr: &'static str,
    ix_gw: Ipv4Addr, // on the bridge

    next_id: u32,
    isps: HashMap<IspId, Isp>,
    dcs: HashMap<DcId, Dc>,
    homes: HashMap<HomeId, Home>,
    devices: HashMap<DeviceId, Device>,

    // child PIDs (reflectors) for cleanup
    children: Vec<Pid>,
}

impl Lab {
    pub async fn new() -> Result<Self> {
        // unique-ish per process
        let pid = std::process::id();
        Ok(Self {
            prefix: format!("nl{}", pid),
            ix_br: format!("br-ix-{}", pid),
            ix_cidr: "203.0.113.1/24",
            ix_gw: Ipv4Addr::new(203, 0, 113, 1),
            next_id: 1,
            isps: HashMap::new(),
            dcs: HashMap::new(),
            homes: HashMap::new(),
            devices: HashMap::new(),
            children: vec![],
        })
    }

    pub async fn add_isp(&mut self, name: &str, mode: IspMode) -> Result<IspId> {
        let id = IspId(self.alloc_id());
        let ns = Ns {
            name: self.ns_name(name),
        };
        // allocate an IX-side IP for ISP
        let ix_ip = Ipv4Addr::new(203, 0, 113, (10 + id.0 as u8));
        self.isps.insert(id, Isp { ns, mode, ix_ip });
        Ok(id)
    }

    pub async fn add_dc(&mut self, name: &str) -> Result<DcId> {
        let id = DcId(self.alloc_id());
        let ns = Ns {
            name: self.ns_name(name),
        };
        let ix_ip = Ipv4Addr::new(203, 0, 113, (200 - (id.0 as u8 % 40)));
        let lan_gw = Ipv4Addr::new(10, 0, id.0 as u8, 1);
        self.dcs.insert(id, Dc { ns, ix_ip, lan_gw });
        Ok(id)
    }

    pub async fn add_home(&mut self, name: &str, isp: IspId, nat: NatMode) -> Result<HomeId> {
        if !self.isps.contains_key(&isp) {
            return Err(anyhow!("unknown isp id"));
        }
        let id = HomeId(self.alloc_id());
        let ns = Ns {
            name: self.ns_name(name),
        };
        // subscriber WAN /30 per home, per isp:
        let wan_ip = Ipv4Addr::new(198, 51, (id.0 % 250) as u8, 2);
        let lan_gw = Ipv4Addr::new(192, 168, (id.0 % 250) as u8, 1);
        self.homes.insert(
            id,
            Home {
                ns,
                isp,
                nat,
                wan_ip,
                lan_gw,
            },
        );
        Ok(id)
    }

    pub async fn add_device(&mut self, name: &str, home: HomeId) -> Result<DeviceId> {
        if !self.homes.contains_key(&home) {
            return Err(anyhow!("unknown home id"));
        }
        let id = DeviceId(self.alloc_id());
        let ns = Ns {
            name: self.ns_name(name),
        };
        let ip = {
            let h = &self.homes[&home];
            let oct = h.lan_gw.octets();
            Ipv4Addr::new(oct[0], oct[1], oct[2], 10 + (id.0 as u8 % 200))
        };
        self.devices.insert(id, Device { ns, home, ip });
        Ok(id)
    }

    /// Builds:
    /// - root IX bridge with 203.0.113.1/24
    /// - ISP routers attached to IX
    /// - DC edge attached to IX, with DC LAN 10.0.X.0/24
    /// - Home routers attached to ISP via /30 WAN, with LAN 192.168.X.0/24 and NAT
    /// - Devices on LAN with default routes
    ///
    /// NAT:
    /// - home NAT uses nft: DestinationIndependent => snat persistent; DestinationDependent => masquerade random
    /// - ISP CGNAT (optional): NAT subscriber WAN ranges -> ISP IX IP
    pub async fn build(&mut self) -> Result<()> {
        ensure_netns_dir()?;

        // 1) create namespaces
        for isp in self.isps.values() {
            create_named_netns(&isp.ns.name)?;
        }
        for dc in self.dcs.values() {
            create_named_netns(&dc.ns.name)?;
        }
        for home in self.homes.values() {
            create_named_netns(&home.ns.name)?;
        }
        for dev in self.devices.values() {
            create_named_netns(&dev.ns.name)?;
        }

        // 2) root: create IX bridge + addr + forward
        with_root_netlink(|h| async move {
            ensure_link_deleted(h, &self.ix_br).await.ok(); // best-effort
            add_bridge(h, &self.ix_br).await?;
            set_link_up(h, &self.ix_br).await?;
            add_addr4(h, &self.ix_br, self.ix_cidr).await?;
            Ok(())
        })
        .await?;

        set_sysctl_root("net/ipv4/ip_forward", "1")?;

        // 3) connect ISP(s) to IX
        for (isp_id, isp) in self.isps.iter() {
            let root_if = format!("v-ix-isp{}", isp_id.0);
            let ns_if = format!("v-isp{}-ix", isp_id.0);
            // veth in root, peer moved into isp ns
            with_root_netlink(|h| async move {
                add_veth(h, &root_if, &ns_if).await?;
                set_master(h, &root_if, &self.ix_br).await?;
                set_link_up(h, &root_if).await?;
                move_link_to_netns(h, &ns_if, &open_netns_fd(&isp.ns.name)?).await?;
                Ok(())
            })
            .await?;

            // configure in ISP ns
            with_netns(&isp.ns.name, |h| async move {
                set_link_up(h, "lo").await?;
                set_link_up(h, &ns_if).await?;
                add_addr4(h, &ns_if, &format!("{}/24", isp.ix_ip)).await?;
                // default route via IX bridge
                add_default_route_v4(h, self.ix_gw).await?;
                Ok(())
            })
            .await?;

            set_sysctl_in(&isp.ns.name, "net/ipv4/ip_forward", "1")?;
        }

        // 4) connect DC(s) to IX + create one server inside DC ns as reflector endpoint
        for (dc_id, dc) in self.dcs.iter() {
            let root_if = format!("v-ix-dc{}", dc_id.0);
            let ns_if = format!("v-dc{}-ix", dc_id.0);

            with_root_netlink(|h| async move {
                add_veth(h, &root_if, &ns_if).await?;
                set_master(h, &root_if, &self.ix_br).await?;
                set_link_up(h, &root_if).await?;
                move_link_to_netns(h, &ns_if, &open_netns_fd(&dc.ns.name)?).await?;
                Ok(())
            })
            .await?;

            with_netns(&dc.ns.name, |h| async move {
                set_link_up(h, "lo").await?;
                set_link_up(h, &ns_if).await?;
                add_addr4(h, &ns_if, &format!("{}/24", dc.ix_ip)).await?;
                add_default_route_v4(h, self.ix_gw).await?;

                // add a DC "lan" interface and assign gw (so we can bind reflectors on DC LAN too if desired)
                // We'll create a dummy interface for LAN gateway.
                add_dummy(h, "dc-lan").await?;
                set_link_up(h, "dc-lan").await?;
                add_addr4(h, "dc-lan", &format!("{}/24", dc.lan_gw)).await?;
                Ok(())
            })
            .await?;

            set_sysctl_in(&dc.ns.name, "net/ipv4/ip_forward", "1")?;
        }

        // 5) connect homes to their ISP, then devices to home LAN
        for (home_id, home) in self.homes.iter() {
            let isp = &self.isps[&home.isp];

            // ISP<->Home WAN /30
            // isp side ip: 198.51.<home_id_mod>.1/30, home side .2/30
            let wan_net_oct = (home_id.0 % 250) as u8;
            let isp_wan_ip = Ipv4Addr::new(198, 51, wan_net_oct, 1);
            let home_wan_ip = home.wan_ip;

            let isp_if = format!("v-isp{}-h{}w", home.isp.0, home_id.0);
            let home_if = format!("v-h{}-wan", home_id.0);

            // create veth in root and move ends into both namespaces
            let root_a = format!("v-root-a-{}", home_id.0);
            let root_b = format!("v-root-b-{}", home_id.0);

            // We need a veth pair, then move each end into the two namespaces.
            with_root_netlink(|h| async move {
                add_veth(h, &root_a, &root_b).await?;
                set_link_up(h, &root_a).await?;
                set_link_up(h, &root_b).await?;
                move_link_to_netns(h, &root_a, &open_netns_fd(&isp.ns.name)?).await?;
                move_link_to_netns(h, &root_b, &open_netns_fd(&home.ns.name)?).await?;
                Ok(())
            })
            .await?;

            // rename inside namespaces (optional), configure addresses + routes
            with_netns(&isp.ns.name, |h| async move {
                set_link_up(h, "lo").await?;
                rename_link(h, &root_a, &isp_if).await?;
                set_link_up(h, &isp_if).await?;
                add_addr4(h, &isp_if, &format!("{}/30", isp_wan_ip)).await?;
                Ok(())
            })
            .await?;

            with_netns(&home.ns.name, |h| async move {
                set_link_up(h, "lo").await?;
                rename_link(h, &root_b, &home_if).await?;
                set_link_up(h, &home_if).await?;
                add_addr4(h, &home_if, &format!("{}/30", home_wan_ip)).await?;
                add_default_route_v4(h, isp_wan_ip).await?;
                Ok(())
            })
            .await?;

            // home LAN dummy interface for gateway (simpler than creating bridges for this minimal lab)
            with_netns(&home.ns.name, |h| async move {
                add_dummy(h, "lan0").await?;
                set_link_up(h, "lan0").await?;
                add_addr4(h, "lan0", &format!("{}/24", home.lan_gw)).await?;
                Ok(())
            })
            .await?;

            set_sysctl_in(&home.ns.name, "net/ipv4/ip_forward", "1")?;

            // configure device namespaces: assign IP to dummy "eth0" and route via home.lan_gw
            for (dev_id, dev) in self.devices.iter().filter(|(_, d)| d.home == *home_id) {
                let dev_if = format!("dev{}-eth0", dev_id.0);
                with_netns(&dev.ns.name, |h| async move {
                    set_link_up(h, "lo").await?;
                    add_dummy(h, &dev_if).await?;
                    set_link_up(h, &dev_if).await?;
                    add_addr4(h, &dev_if, &format!("{}/24", dev.ip)).await?;
                    add_default_route_v4(h, home.lan_gw).await?;
                    Ok(())
                })
                .await?;
            }

            // NAT on home (LAN -> WAN)
            apply_home_nat(&home.ns.name, home.nat, "lan0", &home_if, home_wan_ip).await?;

            // If ISP CGNAT: NAT subscriber WAN ranges -> ISP IX ip
            if let IspMode::Cgnat { .. } = isp.mode {
                apply_isp_cgnat(
                    &isp.ns.name,
                    &isp_if,
                    &format!("v-isp{}-ix", home.isp.0),
                    isp.ix_ip,
                )
                .await
                .ok(); // best-effort if multiple homes call it repeatedly
            }
        }

        // 6) root return routes:
        // - route to DC LANs via DC IX IP
        // - route to ISP subscriber /16-ish via ISP IX IP is not needed for NAT tests, but harmless.
        with_root_netlink(|h| async move {
            for dc in self.dcs.values() {
                add_route_v4(
                    h,
                    &format!(
                        "{}/24",
                        dc.lan_gw.octets()[0..3]
                            .iter()
                            .copied()
                            .chain([0u8])
                            .collect::<Vec<_>>()
                            .as_slice()
                    ),
                    dc.ix_ip,
                )
                .await
                .ok();
            }
            Ok(())
        })
        .await
        .ok();

        Ok(())
    }

    /// Run a program inside a device namespace (simple: no stdout capture here).
    /// For tests, we mainly use `probe_udp_mapping`.
    pub async fn run_in(&self, dev: DeviceId, program: &str, args: &[&str]) -> Result<()> {
        let ns = &self.devices.get(&dev).context("unknown device")?.ns.name;
        run_in_netns(ns, program, args)
    }

    pub async fn probe_udp_mapping(
        &self,
        dev: DeviceId,
        reflector: SocketAddr,
    ) -> Result<ObservedAddr> {
        let ns = &self.devices.get(&dev).context("unknown device")?.ns.name;
        probe_in_ns(ns, reflector, Duration::from_millis(500))
    }

    /// Spawn a reflector bound inside a DC namespace on the DC IX IP (or any IP you pass).
    pub async fn spawn_reflector_in_dc(&mut self, dc: DcId, bind: SocketAddr) -> Result<()> {
        let ns = &self.dcs.get(&dc).context("unknown dc")?.ns.name;
        let pid = spawn_reflector(ns, bind)?;
        self.children.push(pid);
        Ok(())
    }

    /// Spawn a reflector in the root namespace (IX side) bound to an IP on the IX bridge.
    /// You can bind to 203.0.113.1:port (the bridge IP).
    pub async fn spawn_reflector_on_ix(&mut self, bind: SocketAddr) -> Result<()> {
        let pid = spawn_reflector_root(bind)?;
        self.children.push(pid);
        Ok(())
    }

    pub fn isp_public_ip(&self, isp: IspId) -> Result<IpAddr> {
        Ok(IpAddr::V4(
            self.isps.get(&isp).context("unknown isp")?.ix_ip,
        ))
    }

    fn alloc_id(&mut self) -> u32 {
        let x = self.next_id;
        self.next_id += 1;
        x
    }
    fn ns_name(&self, name: &str) -> String {
        format!("{}-{}", self.prefix, name)
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        // kill reflectors
        for &pid in &self.children {
            let _ = kill(pid, Signal::SIGKILL);
        }
        // delete namespaces mountpoints
        for isp in self.isps.values() {
            let _ = std::fs::remove_file(format!("/var/run/netns/{}", isp.ns.name));
        }
        for dc in self.dcs.values() {
            let _ = std::fs::remove_file(format!("/var/run/netns/{}", dc.ns.name));
        }
        for home in self.homes.values() {
            let _ = std::fs::remove_file(format!("/var/run/netns/{}", home.ns.name));
        }
        for dev in self.devices.values() {
            let _ = std::fs::remove_file(format!("/var/run/netns/{}", dev.ns.name));
        }

        // best-effort delete bridge
        let _ = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("ip link del {} 2>/dev/null || true", self.ix_br))
            .status();
    }
}

/// =======================
/// Netns + command helpers
/// =======================

fn ensure_netns_dir() -> Result<()> {
    create_dir_all("/var/run/netns").context("create /var/run/netns")?;
    Ok(())
}

fn create_named_netns(name: &str) -> Result<()> {
    ensure_netns_dir()?;
    let target = format!("/var/run/netns/{}", name);

    match unsafe { fork()? } {
        ForkResult::Child => {
            unshare(CloneFlags::CLONE_NEWNET)?;
            File::create(&target)?;
            mount(
                Some("/proc/self/ns/net"),
                Path::new(&target),
                Some("none"),
                MsFlags::MS_BIND,
                None::<&str>,
            )?;
            std::process::exit(0);
        }
        ForkResult::Parent { child } => {
            let _ = nix::sys::wait::waitpid(child, None)?;
        }
    }
    Ok(())
}

fn open_netns_fd(name: &str) -> Result<File> {
    Ok(File::open(format!("/var/run/netns/{}", name))?)
}

fn run_in_netns(ns_name: &str, program: &str, args: &[&str]) -> Result<()> {
    let ns_fd = open_netns_fd(ns_name)?;
    match unsafe { fork()? } {
        ForkResult::Child => {
            setns(ns_fd.as_raw_fd(), CloneFlags::CLONE_NEWNET)?;
            let status = std::process::Command::new(program).args(args).status()?;
            std::process::exit(if status.success() { 0 } else { 1 });
        }
        ForkResult::Parent { child } => {
            let st = nix::sys::wait::waitpid(child, None)?;
            match st {
                nix::sys::wait::WaitStatus::Exited(_, 0) => Ok(()),
                other => Err(anyhow!("command failed: {:?}", other)),
            }
        }
    }
}

/// Run a closure with a fresh rtnetlink Handle in the *root* netns.
async fn with_root_netlink<F, Fut>(f: F) -> Result<()>
where
    F: FnOnce(&Handle) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let (conn, handle, _) = new_connection().context("new_connection")?;
    tokio::spawn(conn);
    f(&handle).await
}

/// Run a closure in the given netns with a fresh rtnetlink Handle.
/// IMPORTANT: tests must run on a single-threaded runtime (`#[tokio::test(flavor="current_thread")]`).
async fn with_netns<F, Fut>(ns: &str, f: F) -> Result<()>
where
    F: FnOnce(&Handle) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let orig = File::open("/proc/self/ns/net")?;
    let target = open_netns_fd(ns)?;
    setns(target.as_raw_fd(), CloneFlags::CLONE_NEWNET).context("setns target")?;

    let res = async {
        let (conn, handle, _) = new_connection().context("new_connection in netns")?;
        tokio::spawn(conn);
        f(&handle).await
    }
    .await;

    // restore
    setns(orig.as_raw_fd(), CloneFlags::CLONE_NEWNET).context("restore setns")?;
    res
}

fn set_sysctl_root(path: &str, val: &str) -> Result<()> {
    let full = format!("/proc/sys/{}", path);
    std::fs::write(full, val).context("sysctl write")?;
    Ok(())
}

fn set_sysctl_in(ns: &str, path: &str, val: &str) -> Result<()> {
    let orig = File::open("/proc/self/ns/net")?;
    let target = open_netns_fd(ns)?;
    setns(target.as_raw_fd(), CloneFlags::CLONE_NEWNET)?;
    let res = set_sysctl_root(path, val);
    setns(orig.as_raw_fd(), CloneFlags::CLONE_NEWNET)?;
    res
}

/// =======================
/// rtnetlink helpers
/// =======================

async fn link_index(handle: &Handle, ifname: &str) -> Result<u32> {
    let mut links = handle.link().get().match_name(ifname.to_string()).execute();
    if let Some(msg) = links.try_next().await? {
        Ok(msg.header.index)
    } else {
        Err(anyhow!("link not found: {}", ifname))
    }
}

async fn ensure_link_deleted(handle: &Handle, ifname: &str) -> Result<()> {
    if let Ok(idx) = link_index(handle, ifname).await {
        handle.link().del(idx).execute().await?;
    }
    Ok(())
}

async fn add_bridge(handle: &Handle, name: &str) -> Result<()> {
    handle
        .link()
        .add()
        .bridge(name.to_string())
        .execute()
        .await?;
    Ok(())
}

async fn add_dummy(handle: &Handle, name: &str) -> Result<()> {
    handle
        .link()
        .add()
        .dummy(name.to_string())
        .execute()
        .await?;
    Ok(())
}

async fn add_veth(handle: &Handle, a: &str, b: &str) -> Result<()> {
    handle
        .link()
        .add()
        .veth(a.to_string(), b.to_string())
        .execute()
        .await?;
    Ok(())
}

async fn set_link_up(handle: &Handle, ifname: &str) -> Result<()> {
    let idx = link_index(handle, ifname).await?;
    handle.link().set(idx).up().execute().await?;
    Ok(())
}

async fn rename_link(handle: &Handle, from: &str, to: &str) -> Result<()> {
    let idx = link_index(handle, from).await?;
    handle
        .link()
        .set(idx)
        .name(to.to_string())
        .execute()
        .await?;
    Ok(())
}

async fn set_master(handle: &Handle, ifname: &str, master: &str) -> Result<()> {
    let idx = link_index(handle, ifname).await?;
    let midx = link_index(handle, master).await?;
    handle.link().set(idx).master(midx).execute().await?;
    Ok(())
}

async fn move_link_to_netns(handle: &Handle, ifname: &str, ns_fd: &File) -> Result<()> {
    let idx = link_index(handle, ifname).await?;
    handle
        .link()
        .set(idx)
        .setns_by_fd(ns_fd.as_raw_fd())
        .execute()
        .await?;
    Ok(())
}

async fn add_addr4(handle: &Handle, ifname: &str, cidr: &str) -> Result<()> {
    let idx = link_index(handle, ifname).await?;
    let (ip, prefix) = parse_cidr_v4(cidr)?;
    handle
        .address()
        .add(idx, ip.into(), prefix)
        .execute()
        .await?;
    Ok(())
}

async fn add_default_route_v4(handle: &Handle, via: Ipv4Addr) -> Result<()> {
    handle.route().add().v4().gateway(via).execute().await?;
    Ok(())
}

async fn add_route_v4(handle: &Handle, dst_cidr: &str, via: Ipv4Addr) -> Result<()> {
    let (ip, prefix) = parse_cidr_v4(dst_cidr)?;
    handle
        .route()
        .add()
        .v4()
        .destination_prefix(ip, prefix)
        .gateway(via)
        .execute()
        .await?;
    Ok(())
}

fn parse_cidr_v4(cidr: &str) -> Result<(Ipv4Addr, u8)> {
    let mut parts = cidr.split('/');
    let ip: Ipv4Addr = parts.next().ok_or_else(|| anyhow!("bad cidr"))?.parse()?;
    let prefix: u8 = parts.next().ok_or_else(|| anyhow!("bad cidr"))?.parse()?;
    Ok((ip, prefix))
}

/// =======================
/// nft NAT helpers
/// =======================

async fn apply_home_nat(
    ns: &str,
    mode: NatMode,
    lan_if: &str,
    wan_if: &str,
    wan_ip: Ipv4Addr,
) -> Result<()> {
    let rules = match mode {
        NatMode::DestinationIndependent => format!(
            r#"
flush ruleset
table ip nat {{
  chain prerouting {{ type nat hook prerouting priority -100; }}
  chain postrouting {{ type nat hook postrouting priority 100; }}
}}
table ip filter {{
  chain forward {{ type filter hook forward priority 0; policy drop; }}
}}
add rule ip filter forward ct state established,related accept
add rule ip filter forward iif "{lan}" oif "{wan}" accept
add rule ip nat postrouting oif "{wan}" ip saddr 192.168.0.0/16 udp snat to {wanip} persistent
add rule ip nat postrouting oif "{wan}" ip saddr 192.168.0.0/16 tcp snat to {wanip} persistent
"#,
            lan = lan_if,
            wan = wan_if,
            wanip = wan_ip
        ),
        NatMode::DestinationDependent => format!(
            r#"
flush ruleset
table ip nat {{
  chain prerouting {{ type nat hook prerouting priority -100; }}
  chain postrouting {{ type nat hook postrouting priority 100; }}
}}
table ip filter {{
  chain forward {{ type filter hook forward priority 0; policy drop; }}
}}
add rule ip filter forward ct state established,related accept
add rule ip filter forward iif "{lan}" oif "{wan}" accept
add rule ip nat postrouting oif "{wan}" ip saddr 192.168.0.0/16 masquerade random
"#,
            lan = lan_if,
            wan = wan_if
        ),
    };
    run_nft_in(ns, &rules).await
}

async fn apply_isp_cgnat(ns: &str, in_if: &str, out_if: &str, public_ip: Ipv4Addr) -> Result<()> {
    // subscriber space here is 198.51.0.0/16 in this minimal lab
    let rules = format!(
        r#"
table ip nat {{
  chain postrouting {{ type nat hook postrouting priority 100; }}
}}
add rule ip nat postrouting iif "{inif}" oif "{outif}" ip saddr 198.51.0.0/16 masquerade
"#,
        inif = in_if,
        outif = out_if,
    );
    // note: masquerade uses out_if address (public_ip already set on out_if)
    let _ = public_ip;
    run_nft_in(ns, &rules).await
}

async fn run_nft_in(ns: &str, rules: &str) -> Result<()> {
    // Fork + setns + run `nft -f -` with stdin rules
    let ns_fd = open_netns_fd(ns)?;
    match unsafe { fork()? } {
        ForkResult::Child => {
            setns(ns_fd.as_raw_fd(), CloneFlags::CLONE_NEWNET)?;
            let mut child = std::process::Command::new("nft")
                .arg("-f")
                .arg("-")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()?;
            {
                let mut stdin = child.stdin.take().unwrap();
                stdin.write_all(rules.as_bytes())?;
            }
            let st = child.wait()?;
            std::process::exit(if st.success() { 0 } else { 1 });
        }
        ForkResult::Parent { child } => {
            let st = nix::sys::wait::waitpid(child, None)?;
            match st {
                nix::sys::wait::WaitStatus::Exited(_, 0) => Ok(()),
                _ => Err(anyhow!("nft apply failed in {}", ns)),
            }
        }
    }
}

/// =======================
/// STUN-like reflector + probe
/// =======================

fn spawn_reflector(ns: &str, bind: SocketAddr) -> Result<Pid> {
    let ns_fd = open_netns_fd(ns)?;
    match unsafe { fork()? } {
        ForkResult::Child => {
            setns(ns_fd.as_raw_fd(), CloneFlags::CLONE_NEWNET)?;
            // best-effort: bind + loop forever
            let sock = UdpSocket::bind(bind).context("bind reflector")?;
            let mut buf = [0u8; 1024];
            loop {
                let (_, peer) = sock.recv_from(&mut buf).context("recv_from")?;
                let msg = format!("OBSERVED {}", peer);
                let _ = sock.send_to(msg.as_bytes(), peer);
            }
        }
        ForkResult::Parent { child } => Ok(child),
    }
}

fn spawn_reflector_root(bind: SocketAddr) -> Result<Pid> {
    match unsafe { fork()? } {
        ForkResult::Child => {
            let sock = UdpSocket::bind(bind).context("bind reflector root")?;
            let mut buf = [0u8; 1024];
            loop {
                let (_, peer) = sock.recv_from(&mut buf).context("recv_from")?;
                let msg = format!("OBSERVED {}", peer);
                let _ = sock.send_to(msg.as_bytes(), peer);
            }
        }
        ForkResult::Parent { child } => Ok(child),
    }
}

fn probe_in_ns(ns: &str, reflector: SocketAddr, timeout: Duration) -> Result<ObservedAddr> {
    let ns_fd = open_netns_fd(ns)?;
    let orig = File::open("/proc/self/ns/net")?;
    setns(ns_fd.as_raw_fd(), CloneFlags::CLONE_NEWNET)?;

    let res = (|| -> Result<ObservedAddr> {
        let sock = UdpSocket::bind("0.0.0.0:0")?;
        sock.set_read_timeout(Some(timeout))?;
        sock.send_to(b"PROBE", reflector)?;
        let mut buf = [0u8; 1024];
        let (n, _) = sock.recv_from(&mut buf)?;
        let s = std::str::from_utf8(&buf[..n])?;
        let observed_str = s
            .strip_prefix("OBSERVED ")
            .ok_or_else(|| anyhow!("bad reply: {}", s))?;
        let observed: SocketAddr = observed_str.parse()?;
        Ok(ObservedAddr { observed })
    })();

    setns(orig.as_raw_fd(), CloneFlags::CLONE_NEWNET)?;
    res
}

/// =======================
/// Tests
/// =======================

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn require_root() {
        if nix::unistd::Uid::effective().is_root() == false {
            panic!("needs root (CAP_NET_ADMIN). Run: sudo -E cargo test -- --nocapture");
        }
    }

    // NOTE: These tests assume:
    // - `nft` installed
    // - kernel supports netns + veth + bridge
    // - run as root
    //
    // Run:
    //   sudo -E cargo test -- --nocapture
    //
    // Use current_thread runtime because setns is thread-local.
    #[tokio::test(flavor = "current_thread")]
    #[serial]
    async fn nat_dest_independent_keeps_same_observed_port_across_destinations() -> Result<()> {
        require_root();

        let mut lab = Lab::new().await?;
        let isp = lab.add_isp("isp1", IspMode::NoCgnat).await?;
        let dc = lab.add_dc("dc1").await?;
        let home = lab
            .add_home("home1", isp, NatMode::DestinationIndependent)
            .await?;
        let dev = lab.add_device("dev1", home).await?;

        lab.build().await?;

        // Reflector 1: in DC namespace on DC IX IP (we know it's 203.0.113.x)
        let dc_ix_ip = lab.dcs.get(&dc).unwrap().ix_ip;
        let r1: SocketAddr = SocketAddr::new(IpAddr::V4(dc_ix_ip), 3478);
        lab.spawn_reflector_in_dc(dc, r1).await?;

        // Reflector 2: on IX bridge in root (203.0.113.1)
        let r2: SocketAddr = SocketAddr::new(IpAddr::V4(lab.ix_gw), 3479);
        lab.spawn_reflector_on_ix(r2).await?;

        // Give reflectors a moment to bind
        tokio::time::sleep(Duration::from_millis(50)).await;

        let o1 = lab.probe_udp_mapping(dev, r1).await?;
        let o2 = lab.probe_udp_mapping(dev, r2).await?;

        assert_eq!(o1.observed.ip(), o2.observed.ip(), "external IP differs");
        assert_eq!(
            o1.observed.port(),
            o2.observed.port(),
            "EIM/persistent NAT should keep same external port across destinations"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    async fn nat_dest_dependent_changes_observed_port_across_destinations() -> Result<()> {
        require_root();

        let mut lab = Lab::new().await?;
        let isp = lab.add_isp("isp1", IspMode::NoCgnat).await?;
        let dc = lab.add_dc("dc1").await?;
        let home = lab
            .add_home("home1", isp, NatMode::DestinationDependent)
            .await?;
        let dev = lab.add_device("dev1", home).await?;

        lab.build().await?;

        let dc_ix_ip = lab.dcs.get(&dc).unwrap().ix_ip;
        let r1: SocketAddr = SocketAddr::new(IpAddr::V4(dc_ix_ip), 4478);
        lab.spawn_reflector_in_dc(dc, r1).await?;

        let r2: SocketAddr = SocketAddr::new(IpAddr::V4(lab.ix_gw), 4479);
        lab.spawn_reflector_on_ix(r2).await?;

        tokio::time::sleep(Duration::from_millis(50)).await;

        let o1 = lab.probe_udp_mapping(dev, r1).await?;
        let o2 = lab.probe_udp_mapping(dev, r2).await?;

        assert_eq!(o1.observed.ip(), o2.observed.ip(), "external IP differs");
        assert_ne!(
            o1.observed.port(),
            o2.observed.port(),
            "EDM/symmetric-ish NAT should change external port across destinations"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    async fn isp_cgnat_changes_visible_external_ip_to_isp_public() -> Result<()> {
        require_root();

        let mut lab = Lab::new().await?;
        let isp = lab
            .add_isp(
                "isp1",
                IspMode::Cgnat {
                    pool_cidr: "100.64.0.0/10",
                },
            )
            .await?;
        let home = lab
            .add_home("home1", isp, NatMode::DestinationIndependent)
            .await?;
        let dev = lab.add_device("dev1", home).await?;

        // add a DC only so we have a non-device reflector reachable via IX
        let dc = lab.add_dc("dc1").await?;

        lab.build().await?;

        let dc_ix_ip = lab.dcs.get(&dc).unwrap().ix_ip;
        let r: SocketAddr = SocketAddr::new(IpAddr::V4(dc_ix_ip), 5478);
        lab.spawn_reflector_in_dc(dc, r).await?;

        tokio::time::sleep(Duration::from_millis(50)).await;

        let o = lab.probe_udp_mapping(dev, r).await?;
        let isp_public = lab.isp_public_ip(isp)?;

        assert_eq!(
            o.observed.ip(),
            isp_public,
            "with CGNAT, external observed IP should be ISP's public IX IP"
        );
        Ok(())
    }
}
