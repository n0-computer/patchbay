//! netsim-rs — Linux network-namespace lab for NAT/routing experiments.
//!
//! # Quick start (from TOML)
//! ```no_run
//! let lab = Lab::load("lab.toml").await?;
//! lab.run_on("home-eu1", std::process::Command::new("ping").args(["-c1", "8.8.8.8"]))?;
//! ```
//!
//! # Builder API
//! ```no_run
//! let mut lab = Lab::new();
//! let isp  = lab.add_isp("isp1", "eu", false, None)?;
//! let home = lab.add_home("home1", isp, NatMode::DestinationIndependent)?;
//! lab.add_device("dev1", Gateway::Lan(home), None)?;
//! lab.build().await?;
//! ```
//!
//! **Important**: `build()` uses `setns(2)` which is thread-local.
//! Always call it (and any test using it) on a `current_thread` Tokio runtime.

#![allow(dead_code)]

use anyhow::{anyhow, bail, Context, Result};
use futures::stream::TryStreamExt;
use nix::{
    mount::{mount, MsFlags},
    sched::{setns, unshare, CloneFlags},
    sys::signal::{kill, Signal},
    unistd::{fork, ForkResult, Pid},
};
use rtnetlink::{new_connection, Handle};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    fs::{create_dir_all, File},
    io::Write as IoWrite,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    os::fd::{AsFd, AsRawFd},
    path::Path,
    process::ExitStatus,
    time::Duration,
};

// ─────────────────────────────────────────────
// Public types
// ─────────────────────────────────────────────

/// NAT mapping behaviour at a home router.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NatMode {
    /// Endpoint-independent mapping: same external port regardless of destination.
    DestinationIndependent,
    /// Endpoint-dependent (symmetric-ish): different port per destination.
    DestinationDependent,
}

/// Link-layer impairment profile applied via `tc netem`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Impair {
    /// ~20 ms delay, 5 ms jitter.
    Wifi,
    /// ~50 ms delay, 20 ms jitter, 1 % loss.
    Mobile,
}

/// Where a device is attached (determines its network path and IP allocation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gateway {
    /// Device sits behind a home router LAN.
    Lan(HomeId),
    /// Device lives inside a DC namespace (server/relay).
    Dc(DcId),
    /// Device connects directly to an ISP (e.g. mobile phone with SIM).
    Isp(IspId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IspId(u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DcId(u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HomeId(u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DeviceId(u32);

/// Observed external address as reported by a STUN-like reflector.
#[derive(Clone, Debug)]
pub struct ObservedAddr {
    pub observed: SocketAddr,
}

// ─────────────────────────────────────────────
// Internal structs
// ─────────────────────────────────────────────

struct Ns {
    name: String,
}

struct Isp {
    ns: Ns,
    cgnat: bool,
    /// IP on the 203.0.113.0/24 IX bridge.
    ix_ip: Ipv4Addr,
    /// Gateway for directly-attached subscriber devices (10.100.N.1/24).
    sub_gw: Ipv4Addr,
    region: String,
    impair_downstream_ms: Option<u32>,
}

struct Dc {
    ns: Ns,
    ix_ip: Ipv4Addr,
    /// Gateway for devices hosted in this DC (10.0.N.1/24).
    lan_gw: Ipv4Addr,
    region: String,
}

struct Home {
    ns: Ns,
    isp: IspId,
    nat: NatMode,
    /// Home side of the ISP↔home /30 WAN link.
    wan_ip: Ipv4Addr,
    /// ISP side of the ISP↔home /30 WAN link (home's default gateway).
    isp_wan_ip: Ipv4Addr,
    /// IP of `br-lan` in the home namespace.
    lan_gw: Ipv4Addr,
}

struct Device {
    ns: Ns,
    gateway: Gateway,
    ip: Ipv4Addr,
    impair: Option<Impair>,
    /// Stable per-device index used to derive unique interface names.
    idx: u32,
}

// ─────────────────────────────────────────────
// Lab
// ─────────────────────────────────────────────

pub struct Lab {
    /// Short process-unique prefix used on root-namespace interface names.
    prefix: String,
    /// IX bridge name in the root namespace.
    ix_br: String,
    /// IP of the IX bridge itself (203.0.113.1).
    ix_gw: Ipv4Addr,

    next_id: u32,
    // Allocators for IX-side IPs (avoids collisions even with many ISPs/DCs).
    next_isp_ix: u8, // ISPs: 203.0.113.10, .11, ...
    next_dc_ix: u8,  // DCs:  203.0.113.250, 249, ... (counting down)
    // Subnet allocators.
    next_home_wan: u8, // /30 per home: 198.51.100.{N*4+1}/30, .{N*4+2}/30
    next_home_lan: u8, // 192.168.N.0/24 per home
    next_isp_sub: u8,  // 10.100.N.0/24 per ISP (mobile subscriber bridge)
    next_dc_lan: u8,   // 10.0.N.0/24 per DC

    isps: HashMap<IspId, Isp>,
    dcs: HashMap<DcId, Dc>,
    homes: HashMap<HomeId, Home>,
    devices: HashMap<DeviceId, Device>,

    // Name → ID maps for user-facing API.
    isp_by_name: HashMap<String, IspId>,
    dc_by_name: HashMap<String, DcId>,
    home_by_name: HashMap<String, HomeId>,
    device_by_name: HashMap<String, DeviceId>,

    /// (from_region, to_region, latency_ms) pairs; applied as tc netem during build.
    region_latencies: Vec<(String, String, u32)>,

    /// PIDs of forked background processes (reflectors, etc.); killed on drop.
    children: Vec<Pid>,
}

impl Lab {
    // ── Constructors ────────────────────────────────────────────────────

    pub fn new() -> Self {
        let pid = std::process::id();
        let prefix = format!("p{}", pid % 9999 + 1); // e.g. "p1234" (5 chars)
        Self {
            ix_br: format!("br{}", pid % 9999 + 1), // e.g. "br1234"
            ix_gw: Ipv4Addr::new(203, 0, 113, 1),
            prefix,
            next_id: 1,
            next_isp_ix: 10,
            next_dc_ix: 250,
            next_home_wan: 0,
            next_home_lan: 1,
            next_isp_sub: 1,
            next_dc_lan: 1,
            isps: HashMap::new(),
            dcs: HashMap::new(),
            homes: HashMap::new(),
            devices: HashMap::new(),
            isp_by_name: HashMap::new(),
            dc_by_name: HashMap::new(),
            home_by_name: HashMap::new(),
            device_by_name: HashMap::new(),
            region_latencies: vec![],
            children: vec![],
        }
    }

    /// Parse `lab.toml`, instantiate the lab, run `build()`, and return the
    /// ready-to-use lab.  Must be called on a `current_thread` Tokio runtime.
    pub async fn load(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path).context("read lab config")?;
        let cfg: config::LabConfig = toml::from_str(&text).context("parse lab config")?;
        let mut lab = Self::from_config(cfg)?;
        lab.build().await?;
        Ok(lab)
    }

    /// Build a `Lab` from a parsed config without building the network yet.
    fn from_config(cfg: config::LabConfig) -> Result<Self> {
        let mut lab = Self::new();

        // Region latency pairs.
        if let Some(regions) = &cfg.region {
            for (from, rcfg) in regions {
                for (to, &ms) in &rcfg.latencies {
                    lab.region_latencies.push((from.clone(), to.clone(), ms));
                }
            }
        }

        for isp_cfg in &cfg.isp {
            let cgnat = isp_cfg.nat == Some(config::IspNat::Cgnat);
            lab.add_isp(
                &isp_cfg.name,
                &isp_cfg.region,
                cgnat,
                isp_cfg.impair_downstream.as_ref().map(|i| i.latency),
            )?;
        }
        for dc_cfg in &cfg.dc {
            lab.add_dc(&dc_cfg.name, &dc_cfg.region)?;
        }
        for lan_cfg in &cfg.lan {
            let isp_id = *lab.isp_by_name.get(&lan_cfg.isp).ok_or_else(|| {
                anyhow!(
                    "lan '{}' references unknown isp '{}'",
                    lan_cfg.name,
                    lan_cfg.isp
                )
            })?;
            lab.add_home(&lan_cfg.name, isp_id, lan_cfg.nat)?;
        }
        for dev_cfg in &cfg.device {
            let gw = if let Some(&id) = lab.home_by_name.get(&dev_cfg.gateway) {
                Gateway::Lan(id)
            } else if let Some(&id) = lab.dc_by_name.get(&dev_cfg.gateway) {
                Gateway::Dc(id)
            } else if let Some(&id) = lab.isp_by_name.get(&dev_cfg.gateway) {
                Gateway::Isp(id)
            } else {
                bail!(
                    "device '{}' references unknown gateway '{}'",
                    dev_cfg.name,
                    dev_cfg.gateway
                );
            };
            lab.add_device(&dev_cfg.name, gw, dev_cfg.impair)?;
        }
        Ok(lab)
    }

    // ── Builder methods (sync — just populate data structures) ──────────

    pub fn add_isp(
        &mut self,
        name: &str,
        region: &str,
        cgnat: bool,
        impair_downstream_ms: Option<u32>,
    ) -> Result<IspId> {
        if self.isp_by_name.contains_key(name) {
            bail!("isp '{}' already exists", name);
        }
        let id = IspId(self.alloc_id());
        let ix_ip = Ipv4Addr::new(203, 0, 113, self.next_isp_ix);
        self.next_isp_ix += 1;
        let sub_n = self.next_isp_sub;
        self.next_isp_sub += 1;
        let sub_gw = Ipv4Addr::new(10, 100, sub_n, 1);

        self.isps.insert(
            id,
            Isp {
                ns: Ns {
                    name: self.ns_name(name),
                },
                cgnat,
                ix_ip,
                sub_gw,
                region: region.to_string(),
                impair_downstream_ms,
            },
        );
        self.isp_by_name.insert(name.to_string(), id);
        Ok(id)
    }

    pub fn add_dc(&mut self, name: &str, region: &str) -> Result<DcId> {
        if self.dc_by_name.contains_key(name) {
            bail!("dc '{}' already exists", name);
        }
        let id = DcId(self.alloc_id());
        let ix_ip = Ipv4Addr::new(203, 0, 113, self.next_dc_ix);
        self.next_dc_ix -= 1;
        let lan_n = self.next_dc_lan;
        self.next_dc_lan += 1;
        let lan_gw = Ipv4Addr::new(10, 0, lan_n, 1);

        self.dcs.insert(
            id,
            Dc {
                ns: Ns {
                    name: self.ns_name(name),
                },
                ix_ip,
                lan_gw,
                region: region.to_string(),
            },
        );
        self.dc_by_name.insert(name.to_string(), id);
        Ok(id)
    }

    pub fn add_home(&mut self, name: &str, isp: IspId, nat: NatMode) -> Result<HomeId> {
        if !self.isps.contains_key(&isp) {
            bail!("unknown IspId");
        }
        if self.home_by_name.contains_key(name) {
            bail!("home '{}' already exists", name);
        }
        let id = HomeId(self.alloc_id());

        // /30 per home: ISP side .{4n+1}, home side .{4n+2} in 198.51.100/22
        let n = self.next_home_wan as u32;
        self.next_home_wan += 1;
        let base = 198u32 << 24 | 51 << 16 | 100 << 8; // 198.51.100.0
        let isp_o = base + n * 4 + 1;
        let home_o = base + n * 4 + 2;
        let isp_wan_ip = Ipv4Addr::from(isp_o);
        let wan_ip = Ipv4Addr::from(home_o);

        let lan_n = self.next_home_lan;
        self.next_home_lan += 1;
        let lan_gw = Ipv4Addr::new(192, 168, lan_n, 1);

        self.homes.insert(
            id,
            Home {
                ns: Ns {
                    name: self.ns_name(name),
                },
                isp,
                nat,
                wan_ip,
                isp_wan_ip,
                lan_gw,
            },
        );
        self.home_by_name.insert(name.to_string(), id);
        Ok(id)
    }

    pub fn add_device(
        &mut self,
        name: &str,
        gateway: Gateway,
        impair: Option<Impair>,
    ) -> Result<DeviceId> {
        if self.device_by_name.contains_key(name) {
            bail!("device '{}' already exists", name);
        }
        // Count existing devices on the same gateway segment for IP allocation.
        let peer_count = self
            .devices
            .values()
            .filter(|d| d.gateway == gateway)
            .count() as u8;

        let ip = match gateway {
            Gateway::Lan(h) => {
                let h = self
                    .homes
                    .get(&h)
                    .ok_or_else(|| anyhow!("unknown HomeId"))?;
                let o = h.lan_gw.octets();
                Ipv4Addr::new(o[0], o[1], o[2], 10 + peer_count)
            }
            Gateway::Dc(d) => {
                let d = self.dcs.get(&d).ok_or_else(|| anyhow!("unknown DcId"))?;
                let o = d.lan_gw.octets();
                Ipv4Addr::new(o[0], o[1], o[2], 10 + peer_count)
            }
            Gateway::Isp(i) => {
                let i = self.isps.get(&i).ok_or_else(|| anyhow!("unknown IspId"))?;
                let o = i.sub_gw.octets();
                Ipv4Addr::new(o[0], o[1], o[2], 10 + peer_count)
            }
        };

        let id = DeviceId(self.alloc_id());
        let idx = id.0;
        self.devices.insert(
            id,
            Device {
                ns: Ns {
                    name: self.ns_name(name),
                },
                gateway,
                ip,
                impair,
                idx,
            },
        );
        self.device_by_name.insert(name.to_string(), id);
        Ok(id)
    }

    // ── build ────────────────────────────────────────────────────────────

    /// Create all namespaces, links, addresses, routes, and NAT rules.
    ///
    /// Must be called on a `current_thread` Tokio runtime because `setns(2)`
    /// is thread-local and we must ensure all netlink operations happen in the
    /// correct namespace on the same OS thread.
    pub async fn build(&mut self) -> Result<()> {
        ensure_netns_dir()?;

        // 1) Create all namespaces up front.
        for ns in self.all_ns_names() {
            create_named_netns(&ns)?;
        }

        // 2) IX bridge in root namespace.
        let ix_br = self.ix_br.clone();
        let ix_cidr = format!("{}/24", self.ix_gw);
        with_root_netlink(async |h| {
            ensure_link_deleted(h, &ix_br).await.ok();
            add_bridge(h, &ix_br).await?;
            set_link_up(h, &ix_br).await?;
            add_addr4(h, &ix_br, &ix_cidr).await?;
            Ok(())
        })
        .await?;
        set_sysctl_root("net/ipv4/ip_forward", "1")?;

        // 3) Connect each ISP to the IX bridge.
        //    One veth pair: root side joins the bridge, other end goes into ISP ns.
        //    If CGNAT, apply nft masquerade on the IX-facing interface (once per ISP).
        let isp_data: Vec<_> = self
            .isps
            .iter()
            .map(|(id, isp)| (*id, isp.ns.name.clone(), isp.ix_ip, isp.sub_gw, isp.cgnat))
            .collect();

        for (isp_id, isp_ns, ix_ip, sub_gw, cgnat) in isp_data {
            let root_if = self.root_if("i", isp_id.0); // root-side of IX↔ISP veth
            let ns_if = format!("ix"); // ISP ns IX-facing interface
                                       // NOTE: "ix" is short and unambiguous inside the ISP namespace.
                                       // We use a fixed name since there is exactly one IX link per ISP ns.

            let ix_br = self.ix_br.clone();
            let ix_gw = self.ix_gw;
            let isp_ns2 = isp_ns.clone();
            let ns_if2 = ns_if.clone();
            let root_if2 = root_if.clone();
            with_root_netlink(async |h| {
                add_veth(h, &root_if2, &ns_if2).await?;
                set_master(h, &root_if2, &ix_br).await?;
                set_link_up(h, &root_if2).await?;
                move_link_to_netns(h, &ns_if2, &open_netns_fd(&isp_ns2)?).await?;
                Ok(())
            })
            .await?;

            let ix_cidr = format!("{}/24", ix_ip);
            let ns_if3 = ns_if.clone();
            with_netns(&isp_ns, async |h| {
                set_link_up(h, "lo").await?;
                set_link_up(h, &ns_if3).await?;
                add_addr4(h, &ns_if3, &ix_cidr).await?;
                add_default_route_v4(h, ix_gw).await?;
                // Subscriber bridge for mobile/direct-attached devices.
                add_bridge(h, "br-sub").await?;
                set_link_up(h, "br-sub").await?;
                add_addr4(h, "br-sub", &format!("{}/24", sub_gw)).await?;
                Ok(())
            })
            .await?;
            set_sysctl_in(&isp_ns, "net/ipv4/ip_forward", "1")?;

            if cgnat {
                // Masquerade everything leaving via the IX interface — applied
                // exactly once per ISP here, covering both home subscribers and
                // directly-attached mobile devices.
                apply_isp_cgnat(&isp_ns, &ns_if).await?;
            }
        }

        // 4) Connect each DC to the IX bridge.
        let dc_data: Vec<_> = self
            .dcs
            .iter()
            .map(|(id, dc)| (*id, dc.ns.name.clone(), dc.ix_ip, dc.lan_gw))
            .collect();

        for (dc_id, dc_ns, ix_ip, lan_gw) in dc_data {
            let root_if = self.root_if("d", dc_id.0);
            let ns_if = "ix".to_string(); // DC ns IX-facing interface

            let ix_br = self.ix_br.clone();
            let ix_gw = self.ix_gw;
            let dc_ns2 = dc_ns.clone();
            let root_if2 = root_if.clone();
            let ns_if2 = ns_if.clone();
            with_root_netlink(async |h| {
                add_veth(h, &root_if2, &ns_if2).await?;
                set_master(h, &root_if2, &ix_br).await?;
                set_link_up(h, &root_if2).await?;
                move_link_to_netns(h, &ns_if2, &open_netns_fd(&dc_ns2)?).await?;
                Ok(())
            })
            .await?;

            let ix_cidr = format!("{}/24", ix_ip);
            let lan_cidr = format!("{}/24", lan_gw);
            with_netns(&dc_ns, async |h| {
                set_link_up(h, "lo").await?;
                set_link_up(h, &ns_if).await?;
                add_addr4(h, &ns_if, &ix_cidr).await?;
                add_default_route_v4(h, ix_gw).await?;
                // LAN bridge for DC-hosted devices.
                add_bridge(h, "br-lan").await?;
                set_link_up(h, "br-lan").await?;
                add_addr4(h, "br-lan", &lan_cidr).await?;
                Ok(())
            })
            .await?;
            set_sysctl_in(&dc_ns, "net/ipv4/ip_forward", "1")?;
        }

        // 5) Connect each home to its ISP via a /30 WAN veth pair, set up
        //    the LAN bridge, and apply nft NAT.
        let home_data: Vec<_> = self
            .homes
            .iter()
            .map(|(id, h)| {
                let isp = &self.isps[&h.isp];
                (
                    *id,
                    h.ns.name.clone(),
                    h.isp,
                    isp.ns.name.clone(),
                    h.nat,
                    h.wan_ip,
                    h.isp_wan_ip,
                    h.lan_gw,
                )
            })
            .collect();

        for (home_id, home_ns, isp_id, isp_ns, nat, wan_ip, isp_wan_ip, lan_gw) in home_data {
            // Temporary names in root ns (moved immediately, so prefix is enough).
            let root_a = self.root_if("a", home_id.0);
            let root_b = self.root_if("b", home_id.0);
            // Final names inside each namespace.
            let isp_if = format!("h{}", home_id.0); // inside ISP ns
            let wan_if = "wan".to_string(); // inside home ns

            let isp_ns2 = isp_ns.clone();
            let root_a2 = root_a.clone();
            let root_b2 = root_b.clone();
            let home_ns2 = home_ns.clone();
            with_root_netlink(async |h| {
                add_veth(h, &root_a2, &root_b2).await?;
                move_link_to_netns(h, &root_a2, &open_netns_fd(&isp_ns2)?).await?;
                move_link_to_netns(h, &root_b2, &open_netns_fd(&home_ns2)?).await?;
                Ok(())
            })
            .await?;

            // ISP side: rename + assign /30 address.
            let isp_wan_cidr = format!("{}/30", isp_wan_ip);
            let isp_if2 = isp_if.clone();
            let root_a3 = root_a.clone();
            with_netns(&isp_ns, async |h| {
                rename_link(h, &root_a3, &isp_if2).await?;
                set_link_up(h, &isp_if2).await?;
                add_addr4(h, &isp_if2, &isp_wan_cidr).await?;
                Ok(())
            })
            .await?;

            // Home side: rename + assign /30 address + default route + LAN bridge.
            let home_wan_cidr = format!("{}/30", wan_ip);
            let lan_cidr = format!("{}/24", lan_gw);
            let root_b3 = root_b.clone();
            let wan_if2 = wan_if.clone();
            with_netns(&home_ns, async |h| {
                set_link_up(h, "lo").await?;
                rename_link(h, &root_b3, &wan_if2).await?;
                set_link_up(h, &wan_if2).await?;
                add_addr4(h, &wan_if2, &home_wan_cidr).await?;
                add_default_route_v4(h, isp_wan_ip).await?;
                add_bridge(h, "br-lan").await?;
                set_link_up(h, "br-lan").await?;
                add_addr4(h, "br-lan", &lan_cidr).await?;
                Ok(())
            })
            .await?;
            set_sysctl_in(&home_ns, "net/ipv4/ip_forward", "1")?;

            apply_home_nat(&home_ns, nat, "br-lan", &wan_if, wan_ip).await?;
        }

        // 6) Connect devices to their gateway namespace.
        let dev_data: Vec<_> = self
            .devices
            .iter()
            .map(|(_, d)| {
                let (gw_ns, gw_ip) = match d.gateway {
                    Gateway::Lan(h) => {
                        let h = &self.homes[&h];
                        (h.ns.name.clone(), h.lan_gw)
                    }
                    Gateway::Dc(dc) => {
                        let dc = &self.dcs[&dc];
                        (dc.ns.name.clone(), dc.lan_gw)
                    }
                    Gateway::Isp(i) => {
                        let i = &self.isps[&i];
                        (i.ns.name.clone(), i.sub_gw)
                    }
                };
                let br = match d.gateway {
                    Gateway::Isp(_) => "br-sub",
                    _ => "br-lan",
                };
                (
                    d.ns.name.clone(),
                    gw_ns,
                    gw_ip,
                    br.to_string(),
                    d.ip,
                    d.impair,
                    d.idx,
                )
            })
            .collect();

        for (dev_ns, gw_ns, gw_ip, gw_br, dev_ip, impair, idx) in dev_data {
            // Two root-ns temp names, immediately moved to target namespaces.
            let root_gw = self.root_if("g", idx);
            let root_dev = self.root_if("e", idx);

            let gw_ns2 = gw_ns.clone();
            let dev_ns2 = dev_ns.clone();
            let root_gw2 = root_gw.clone();
            let root_dev2 = root_dev.clone();
            with_root_netlink(async |h| {
                add_veth(h, &root_gw2, &root_dev2).await?;
                move_link_to_netns(h, &root_gw2, &open_netns_fd(&gw_ns2)?).await?;
                move_link_to_netns(h, &root_dev2, &open_netns_fd(&dev_ns2)?).await?;
                Ok(())
            })
            .await?;

            // Gateway side: attach to the appropriate bridge.
            let gw_br2 = gw_br.clone();
            let root_gw3 = root_gw.clone();
            with_netns(&gw_ns, async |h| {
                set_link_up(h, &root_gw3).await?;
                set_master(h, &root_gw3, &gw_br2).await?;
                Ok(())
            })
            .await?;

            // Device side: rename to eth0, assign IP, default route.
            let ip_cidr = format!("{}/24", dev_ip);
            let root_dev3 = root_dev.clone();
            with_netns(&dev_ns, async |h| {
                set_link_up(h, "lo").await?;
                rename_link(h, &root_dev3, "eth0").await?;
                set_link_up(h, "eth0").await?;
                add_addr4(h, "eth0", &ip_cidr).await?;
                add_default_route_v4(h, gw_ip).await?;
                Ok(())
            })
            .await?;

            if let Some(imp) = impair {
                apply_impair_in(&dev_ns, "eth0", imp);
            }
        }

        // 7) Root-ns return routes so IX bridge can reach DC LAN subnets
        //    (required for reflectors hosted inside DC namespaces).
        let dc_routes: Vec<_> = self.dcs.values().map(|dc| (dc.lan_gw, dc.ix_ip)).collect();
        with_root_netlink(async |h| {
            for (lan_gw, ix_ip) in dc_routes {
                let o = lan_gw.octets();
                let net = Ipv4Addr::new(o[0], o[1], o[2], 0);
                add_route_v4(h, &format!("{}/24", net), ix_ip).await.ok();
            }
            Ok(())
        })
        .await
        .ok();

        // TODO(region-latency): apply inter-region latency via tc netem.
        // For each (from, to, ms) in self.region_latencies:
        //   1. Find ISPs/DCs in `from` region; their root-side IX veths are
        //      self.root_if("i", isp_id.0) and self.root_if("d", dc_id.0).
        //   2. Apply `tc qdisc add dev <if> root netem delay <ms/2>ms` on those
        //      interfaces (splitting delay symmetrically).

        Ok(())
    }

    // ── User-facing API ─────────────────────────────────────────────────

    /// Run a command inside a device namespace (blocks until it exits).
    ///
    /// ```no_run
    /// lab.run_on("home-eu1", Command::new("ping").args(["-c1", "1.1.1.1"]))?;
    /// ```
    pub fn run_on(&self, name: &str, cmd: std::process::Command) -> Result<ExitStatus> {
        let id = self
            .device_by_name
            .get(name)
            .ok_or_else(|| anyhow!("unknown device '{}'", name))?;
        run_in_netns(&self.devices[id].ns.name, cmd)
    }

    /// Spawn a long-running process inside a device namespace and return its PID.
    /// The process is killed when the `Lab` is dropped.
    pub fn spawn_on(&mut self, name: &str, cmd: std::process::Command) -> Result<Pid> {
        let id = *self
            .device_by_name
            .get(name)
            .ok_or_else(|| anyhow!("unknown device '{}'", name))?;
        let ns = self.devices[&id].ns.name.clone();
        let pid = spawn_in_netns(&ns, cmd)?;
        self.children.push(pid);
        Ok(pid)
    }

    // ── Reflector / probe helpers (mainly for tests) ─────────────────────

    /// Spawn a UDP reflector in a named device/DC/ISP namespace.
    ///
    /// Use `dc_ix_ip(dc)` or `isp_public_ip(isp)` to pick a bind address.
    pub fn spawn_reflector(&mut self, ns_name: &str, bind: SocketAddr) -> Result<Pid> {
        let pid = spawn_reflector_in(Some(ns_name), bind)?;
        self.children.push(pid);
        Ok(pid)
    }

    /// Spawn a UDP reflector in the root namespace (IX bridge side).
    pub fn spawn_reflector_on_ix(&mut self, bind: SocketAddr) -> Result<Pid> {
        let pid = spawn_reflector_in(None, bind)?;
        self.children.push(pid);
        Ok(pid)
    }

    /// Probe the NAT mapping seen by a reflector from a named device.
    pub fn probe_udp_mapping(&self, device: &str, reflector: SocketAddr) -> Result<ObservedAddr> {
        let id = self
            .device_by_name
            .get(device)
            .ok_or_else(|| anyhow!("unknown device '{}'", device))?;
        probe_in_ns(
            &self.devices[id].ns.name,
            reflector,
            Duration::from_millis(500),
        )
    }

    // ── Lookup helpers ───────────────────────────────────────────────────

    pub fn isp_public_ip(&self, isp: IspId) -> Result<IpAddr> {
        Ok(IpAddr::V4(
            self.isps.get(&isp).context("unknown IspId")?.ix_ip,
        ))
    }

    pub fn dc_ix_ip(&self, dc: DcId) -> Result<Ipv4Addr> {
        Ok(self.dcs.get(&dc).context("unknown DcId")?.ix_ip)
    }

    pub fn isp_id(&self, name: &str) -> Option<IspId> {
        self.isp_by_name.get(name).copied()
    }
    pub fn dc_id(&self, name: &str) -> Option<DcId> {
        self.dc_by_name.get(name).copied()
    }
    pub fn home_id(&self, name: &str) -> Option<HomeId> {
        self.home_by_name.get(name).copied()
    }
    pub fn device_id(&self, name: &str) -> Option<DeviceId> {
        self.device_by_name.get(name).copied()
    }

    /// The IX gateway IP (203.0.113.1) — useful for binding a root-ns reflector.
    pub fn ix_gw(&self) -> Ipv4Addr {
        self.ix_gw
    }

    // ── Private helpers ──────────────────────────────────────────────────

    fn alloc_id(&mut self) -> u32 {
        let x = self.next_id;
        self.next_id += 1;
        x
    }

    fn ns_name(&self, name: &str) -> String {
        format!("{}-{}", self.prefix, name)
    }

    /// Interface name for root-namespace veths.  Uses the process-unique prefix
    /// to avoid collisions when multiple test labs run concurrently.
    /// Result is ≤ 15 chars (prefix ≤ 5, tag ≤ 2, id ≤ 5 → ≤ 12).
    fn root_if(&self, tag: &str, id: u32) -> String {
        format!("{}{}{}", self.prefix, tag, id)
    }

    fn all_ns_names(&self) -> Vec<String> {
        self.isps
            .values()
            .map(|x| x.ns.name.clone())
            .chain(self.dcs.values().map(|x| x.ns.name.clone()))
            .chain(self.homes.values().map(|x| x.ns.name.clone()))
            .chain(self.devices.values().map(|x| x.ns.name.clone()))
            .collect()
    }
}

impl Default for Lab {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        for &pid in &self.children {
            let _ = kill(pid, Signal::SIGKILL);
        }
        for ns_name in self.all_ns_names() {
            let _ = std::fs::remove_file(format!("/var/run/netns/{}", ns_name));
        }
        // Best-effort IX bridge removal.
        let _ = std::process::Command::new("ip")
            .args(["link", "del", &self.ix_br])
            .status();
    }
}

// ─────────────────────────────────────────────
// TOML config types
// ─────────────────────────────────────────────

mod config {
    use super::{Impair, NatMode};
    use serde::Deserialize;
    use std::collections::HashMap;

    #[derive(Deserialize)]
    pub struct LabConfig {
        pub region: Option<HashMap<String, RegionConfig>>,
        #[serde(default)]
        pub isp: Vec<IspConfig>,
        #[serde(default)]
        pub dc: Vec<DcConfig>,
        #[serde(default)]
        pub lan: Vec<LanConfig>,
        #[serde(default)]
        pub device: Vec<DeviceConfig>,
    }

    #[derive(Deserialize)]
    pub struct RegionConfig {
        /// Map of target-region name → one-way latency in ms.
        #[serde(default)]
        pub latencies: HashMap<String, u32>,
    }

    /// `nat = "cgnat"` on an ISP entry.
    #[derive(Deserialize, PartialEq)]
    #[serde(rename_all = "lowercase")]
    pub enum IspNat {
        Cgnat,
    }

    #[derive(Deserialize)]
    pub struct IspConfig {
        pub name: String,
        pub region: String,
        /// Set to `"cgnat"` to enable CGNAT on this ISP.
        pub nat: Option<IspNat>,
        pub impair_downstream: Option<ImpairCfg>,
    }

    #[derive(Deserialize)]
    pub struct ImpairCfg {
        pub latency: u32, // milliseconds added to downstream links
    }

    #[derive(Deserialize)]
    pub struct DcConfig {
        pub name: String,
        pub region: String,
    }

    #[derive(Deserialize)]
    pub struct LanConfig {
        pub name: String,
        /// Name of an `[[isp]]` entry.
        pub isp: String,
        /// `"destination-independent"` or `"destination-dependent"`.
        pub nat: NatMode,
    }

    #[derive(Deserialize)]
    pub struct DeviceConfig {
        pub name: String,
        /// Name of a `[[lan]]`, `[[dc]]`, or `[[isp]]` entry.
        pub gateway: String,
        /// Optional link impairment: `"wifi"` or `"mobile"`.
        pub impair: Option<Impair>,
    }
}

// ─────────────────────────────────────────────
// Netns + process helpers
// ─────────────────────────────────────────────

fn ensure_netns_dir() -> Result<()> {
    create_dir_all("/var/run/netns").context("create /var/run/netns")
}

fn open_netns_fd(name: &str) -> Result<File> {
    File::open(format!("/var/run/netns/{}", name))
        .with_context(|| format!("open netns fd for '{}'", name))
}

/// Fork, unshare a new network namespace, bind-mount it at /var/run/netns/<name>.
fn create_named_netns(name: &str) -> Result<()> {
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
            nix::sys::wait::waitpid(child, None)?;
            Ok(())
        }
    }
}

/// Run a configured `Command` inside a network namespace.  Forks, setns, execs.
/// Blocks until the command exits.
fn run_in_netns(ns: &str, mut cmd: std::process::Command) -> Result<ExitStatus> {
    let ns_fd = open_netns_fd(ns)?;
    match unsafe { fork()? } {
        ForkResult::Child => {
            setns(ns_fd.as_fd().clone(), CloneFlags::CLONE_NEWNET).expect("child setns failed");
            // cmd.status() forks again; use exec() to replace this child directly.
            let err = std::os::unix::process::CommandExt::exec(&mut cmd);
            eprintln!("exec failed: {}", err);
            std::process::exit(1);
        }
        ForkResult::Parent { child } => {
            let st = nix::sys::wait::waitpid(child, None)?;
            match st {
                nix::sys::wait::WaitStatus::Exited(_, code) => {
                    use std::os::unix::process::ExitStatusExt;
                    Ok(ExitStatus::from_raw(code << 8))
                }
                other => Err(anyhow!("process did not exit normally: {:?}", other)),
            }
        }
    }
}

/// Spawn a long-running process in a namespace.  Forks, setns, exec's the
/// command.  The forked child *becomes* the process (no double-fork).
fn spawn_in_netns(ns: &str, mut cmd: std::process::Command) -> Result<Pid> {
    let ns_fd = open_netns_fd(ns)?;
    match unsafe { fork()? } {
        ForkResult::Child => {
            setns(&ns_fd, CloneFlags::CLONE_NEWNET).expect("child setns failed");
            let err = std::os::unix::process::CommandExt::exec(&mut cmd);
            eprintln!("exec failed: {}", err);
            std::process::exit(1);
        }
        ForkResult::Parent { child } => Ok(child),
    }
}

/// Run an async closure with a fresh rtnetlink handle in the **root** namespace.
async fn with_root_netlink<F>(f: F) -> Result<()>
where
    F: AsyncFnOnce(&Handle) -> Result<()>,
{
    let (conn, handle, _) = new_connection().context("rtnetlink new_connection")?;
    tokio::spawn(conn);
    f(&handle).await
}

/// Temporarily switch to `ns`, run an async closure with a fresh rtnetlink
/// handle, then switch back.  Must be on a `current_thread` Tokio runtime.
async fn with_netns<F>(ns: &str, f: F) -> Result<()>
where
    F: AsyncFnOnce(&Handle) -> Result<()>,
{
    let orig = File::open("/proc/self/ns/net").context("open self netns")?;
    let target = open_netns_fd(ns)?;
    setns(&target, CloneFlags::CLONE_NEWNET).context("setns target")?;

    let res = async {
        let (conn, handle, _) = new_connection().context("rtnetlink new_connection in netns")?;
        tokio::spawn(conn);
        f(&handle).await
    }
    .await;

    setns(&orig, CloneFlags::CLONE_NEWNET).context("restore setns")?;
    res
}

fn set_sysctl_root(path: &str, val: &str) -> Result<()> {
    std::fs::write(format!("/proc/sys/{}", path), val)
        .with_context(|| format!("sysctl write {}", path))
}

fn set_sysctl_in(ns: &str, path: &str, val: &str) -> Result<()> {
    let orig = File::open("/proc/self/ns/net")?;
    let target = open_netns_fd(ns)?;
    setns(&target, CloneFlags::CLONE_NEWNET)?;
    let res = set_sysctl_root(path, val);
    setns(&orig, CloneFlags::CLONE_NEWNET)?;
    res
}

// ─────────────────────────────────────────────
// rtnetlink helpers
// ─────────────────────────────────────────────

async fn link_index(handle: &Handle, ifname: &str) -> Result<u32> {
    let mut links = handle.link().get().match_name(ifname.to_string()).execute();
    links
        .try_next()
        .await?
        .map(|msg| msg.header.index)
        .ok_or_else(|| anyhow!("link not found: {}", ifname))
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
    let (dst, prefix) = parse_cidr_v4(dst_cidr)?;
    handle
        .route()
        .add()
        .v4()
        .destination_prefix(dst, prefix)
        .gateway(via)
        .execute()
        .await?;
    Ok(())
}

fn parse_cidr_v4(cidr: &str) -> Result<(Ipv4Addr, u8)> {
    let (addr, len) = cidr
        .split_once('/')
        .ok_or_else(|| anyhow!("bad cidr: {}", cidr))?;
    Ok((addr.parse()?, len.parse()?))
}

// ─────────────────────────────────────────────
// nft NAT helpers
// ─────────────────────────────────────────────

async fn apply_home_nat(
    ns: &str,
    mode: NatMode,
    lan_if: &str,
    wan_if: &str,
    wan_ip: Ipv4Addr,
) -> Result<()> {
    let snat_rule = match mode {
        NatMode::DestinationIndependent =>
        // `persistent` keeps the same external port across destinations (EIM).
        {
            format!(
                "add rule ip nat postrouting oif \"{wan}\" snat to {ip} persistent",
                wan = wan_if,
                ip = wan_ip
            )
        }
        NatMode::DestinationDependent =>
        // `random` encourages a different port mapping per destination (EDM).
        {
            format!(
                "add rule ip nat postrouting oif \"{wan}\" masquerade random",
                wan = wan_if
            )
        }
    };

    let rules = format!(
        r#"table ip nat {{
  chain postrouting {{ type nat hook postrouting priority 100; policy accept; }}
}}
table ip filter {{
  chain forward {{ type filter hook forward priority 0; policy drop; }}
}}
add rule ip filter forward ct state established,related accept
add rule ip filter forward iif "{lan}" oif "{wan}" accept
{snat}
"#,
        lan = lan_if,
        wan = wan_if,
        snat = snat_rule,
    );
    run_nft_in(ns, &rules).await
}

/// Apply ISP CGNAT: masquerade all outbound traffic through the IX interface.
/// Called at most once per ISP (in step 3 of build).
async fn apply_isp_cgnat(ns: &str, ix_if: &str) -> Result<()> {
    let rules = format!(
        r#"table ip nat {{
  chain postrouting {{ type nat hook postrouting priority 100; policy accept; }}
}}
add rule ip nat postrouting oif "{ix}" masquerade
"#,
        ix = ix_if,
    );
    run_nft_in(ns, &rules).await
}

/// Fork, setns into `ns`, pipe `rules` into `nft -f -`, wait for it.
async fn run_nft_in(ns: &str, rules: &str) -> Result<()> {
    let ns_fd = open_netns_fd(ns)?;
    let rules = rules.to_string(); // own the string for the child
    match unsafe { fork()? } {
        ForkResult::Child => {
            setns(&ns_fd, CloneFlags::CLONE_NEWNET).expect("setns nft child");
            let mut child = std::process::Command::new("nft")
                .args(["-f", "-"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::inherit()) // let nft errors surface
                .spawn()
                .expect("spawn nft");
            child
                .stdin
                .take()
                .unwrap()
                .write_all(rules.as_bytes())
                .expect("write nft stdin");
            let st = child.wait().expect("wait nft");
            std::process::exit(if st.success() { 0 } else { 1 });
        }
        ForkResult::Parent { child } => match nix::sys::wait::waitpid(child, None)? {
            nix::sys::wait::WaitStatus::Exited(_, 0) => Ok(()),
            _ => Err(anyhow!("nft apply failed in namespace '{}'", ns)),
        },
    }
}

// ─────────────────────────────────────────────
// Impairment
// ─────────────────────────────────────────────

/// Apply tc-netem impairment on `ifname` inside `ns`.  Best-effort: if `tc`
/// is not installed the error is logged but not propagated.
fn apply_impair_in(ns: &str, ifname: &str, impair: Impair) {
    let args: &[&str] = match impair {
        Impair::Wifi => &[
            "qdisc",
            "add",
            "dev",
            ifname,
            "root",
            "netem",
            "delay",
            "20ms",
            "5ms",
            "distribution",
            "normal",
        ],
        Impair::Mobile => &[
            "qdisc", "add", "dev", ifname, "root", "netem", "delay", "50ms", "20ms", "loss", "1%",
        ],
    };
    let mut cmd = std::process::Command::new("tc");
    cmd.args(args);
    match run_in_netns(ns, cmd) {
        Ok(_) => {}
        Err(e) => eprintln!("warn: apply_impair_in({}): {}", ifname, e),
    }
}

// ─────────────────────────────────────────────
// STUN-like reflector + probe
// ─────────────────────────────────────────────

/// Spawn a UDP reflector that echoes "OBSERVED <peer_ip>:<peer_port>" back to
/// each sender.  Pass `ns = Some(name)` to run inside a named netns, or
/// `None` for the root namespace.
fn spawn_reflector_in(ns: Option<&str>, bind: SocketAddr) -> Result<Pid> {
    let ns_fd = ns.map(open_netns_fd).transpose()?;
    match unsafe { fork()? } {
        ForkResult::Child => {
            if let Some(fd) = ns_fd {
                setns(&fd, CloneFlags::CLONE_NEWNET).expect("reflector setns");
            }
            let sock = UdpSocket::bind(bind).expect("reflector bind");
            let mut buf = [0u8; 512];
            loop {
                if let Ok((_, peer)) = sock.recv_from(&mut buf) {
                    let msg = format!("OBSERVED {}", peer);
                    let _ = sock.send_to(msg.as_bytes(), peer);
                }
            }
        }
        ForkResult::Parent { child } => Ok(child),
    }
}

/// Send a UDP probe from inside `ns` to `reflector`, parse the "OBSERVED …"
/// reply, and return the observed external address.
fn probe_in_ns(ns: &str, reflector: SocketAddr, timeout: Duration) -> Result<ObservedAddr> {
    let ns_fd = open_netns_fd(ns)?;
    let orig = File::open("/proc/self/ns/net")?;
    setns(&ns_fd, CloneFlags::CLONE_NEWNET)?;

    let res = (|| -> Result<ObservedAddr> {
        let sock = UdpSocket::bind("0.0.0.0:0")?;
        sock.set_read_timeout(Some(timeout))?;
        sock.send_to(b"PROBE", reflector)?;
        let mut buf = [0u8; 512];
        let (n, _) = sock.recv_from(&mut buf)?;
        let s = std::str::from_utf8(&buf[..n])?;
        let addr_str = s
            .strip_prefix("OBSERVED ")
            .ok_or_else(|| anyhow!("unexpected reflector reply: {:?}", s))?;
        Ok(ObservedAddr {
            observed: addr_str.parse()?,
        })
    })();

    setns(&orig, CloneFlags::CLONE_NEWNET)?;
    res
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn require_root() {
        if !nix::unistd::Uid::effective().is_root() {
            panic!("test requires root / CAP_NET_ADMIN — run: sudo -E cargo test -- --nocapture");
        }
    }

    // ── Builder-API NAT tests ────────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    async fn nat_dest_independent_keeps_port() -> Result<()> {
        require_root();
        let mut lab = Lab::new();
        let isp = lab.add_isp("isp1", "eu", false, None)?;
        let dc = lab.add_dc("dc1", "eu")?;
        let home = lab.add_home("home1", isp, NatMode::DestinationIndependent)?;
        lab.add_device("dev1", Gateway::Lan(home), None)?;
        lab.build().await?;

        // Reflector in DC namespace.
        let dc_ip = lab.dc_ix_ip(dc)?;
        let r1 = SocketAddr::new(IpAddr::V4(dc_ip), 3478);
        let dc_ns = lab.dcs[&dc].ns.name.clone();
        lab.spawn_reflector(&dc_ns, r1)?;

        // Reflector on IX bridge (root ns).
        let r2 = SocketAddr::new(IpAddr::V4(lab.ix_gw()), 3479);
        lab.spawn_reflector_on_ix(r2)?;

        tokio::time::sleep(Duration::from_millis(100)).await;

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
    #[serial]
    async fn nat_dest_dependent_changes_port() -> Result<()> {
        require_root();
        let mut lab = Lab::new();
        let isp = lab.add_isp("isp1", "eu", false, None)?;
        let dc = lab.add_dc("dc1", "eu")?;
        let home = lab.add_home("home1", isp, NatMode::DestinationDependent)?;
        lab.add_device("dev1", Gateway::Lan(home), None)?;
        lab.build().await?;

        let dc_ip = lab.dc_ix_ip(dc)?;
        let r1 = SocketAddr::new(IpAddr::V4(dc_ip), 4478);
        let dc_ns = lab.dcs[&dc].ns.name.clone();
        lab.spawn_reflector(&dc_ns, r1)?;

        let r2 = SocketAddr::new(IpAddr::V4(lab.ix_gw()), 4479);
        lab.spawn_reflector_on_ix(r2)?;

        tokio::time::sleep(Duration::from_millis(100)).await;

        let o1 = lab.probe_udp_mapping("dev1", r1)?;
        let o2 = lab.probe_udp_mapping("dev1", r2)?;

        assert_eq!(o1.observed.ip(), o2.observed.ip(), "external IP differs");
        assert_ne!(
            o1.observed.port(),
            o2.observed.port(),
            "EDM: external port must change per destination",
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    async fn cgnat_hides_behind_isp_public_ip() -> Result<()> {
        require_root();
        let mut lab = Lab::new();
        let isp = lab.add_isp("isp1", "eu", true /* cgnat */, None)?;
        let dc = lab.add_dc("dc1", "eu")?;
        let home = lab.add_home("home1", isp, NatMode::DestinationIndependent)?;
        lab.add_device("dev1", Gateway::Lan(home), None)?;
        lab.build().await?;

        let dc_ip = lab.dc_ix_ip(dc)?;
        let r = SocketAddr::new(IpAddr::V4(dc_ip), 5478);
        let dc_ns = lab.dcs[&dc].ns.name.clone();
        lab.spawn_reflector(&dc_ns, r)?;

        tokio::time::sleep(Duration::from_millis(100)).await;

        let o = lab.probe_udp_mapping("dev1", r)?;
        let isp_public = lab.isp_public_ip(isp)?;

        assert_eq!(
            o.observed.ip(),
            isp_public,
            "with CGNAT the observed IP must be the ISP's IX IP",
        );
        Ok(())
    }

    // ── Lab::load test ───────────────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    async fn load_from_toml() -> Result<()> {
        require_root();
        // Minimal inline TOML so the test is self-contained.
        let toml = r#"
[[isp]]
name   = "isp1"
region = "eu"

[[dc]]
name   = "dc1"
region = "eu"

[[lan]]
name    = "lan1"
isp     = "isp1"
nat     = "destination-independent"

[[device]]
name    = "dev1"
gateway = "lan1"
"#;
        let tmp = std::env::temp_dir().join("netsim_test_lab.toml");
        std::fs::write(&tmp, toml)?;

        let lab = Lab::load(&tmp).await?;
        assert!(lab.device_id("dev1").is_some());
        Ok(())
    }
}
