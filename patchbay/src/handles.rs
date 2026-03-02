//! Cloneable handles for interacting with lab nodes at runtime.
//!
//! [`Device`], [`Router`], and [`Ix`] are thin wrappers around a [`NodeId`] and
//! an `Arc` to the lab interior. They are cheaply cloneable and safe to share
//! across tasks and threads. All accessor methods briefly lock an internal mutex,
//! copy the requested value, and return owned data — no borrow escapes the lock.
//!
//! Mutation methods (`set_nat_mode`, `set_link_condition`, etc.) are `async` and
//! serialize per-node via a `tokio::sync::Mutex<()>` operation guard to prevent
//! TOCTOU races when multiple tasks mutate the same node concurrently.
//!
//! [`Device::name`], [`Device::ns`], [`Router::name`], and [`Router::ns`] are
//! cached at construction time and always available. Other accessors return
//! `None` (or `Err` for mutation methods) if the node has been removed.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    process::Command,
    sync::Arc,
    thread,
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use ipnet::{Ipv4Net, Ipv6Net};
use tracing::debug;

use crate::{
    core::{
        self, apply_nat_for_router, apply_nat_v6, apply_or_remove_impair, run_nft_in, LabInner,
        NodeId,
    },
    firewall::Firewall,
    lab::{net6, LinkCondition, ObservedAddr},
    nat::{IpSupport, Nat, NatV6Mode},
    netlink::Netlink,
};

// ─────────────────────────────────────────────
// Device / Router / DeviceIface handles
// ─────────────────────────────────────────────

/// Owned snapshot of a single device network interface.
///
/// Returned by [`Device::iface`], [`Device::default_iface`], and
/// [`Device::interfaces`]. This is a lightweight value type — no `Arc`.
#[derive(Clone, Debug)]
pub struct DeviceIface {
    ifname: String,
    ip: Option<Ipv4Addr>,
    ip_v6: Option<Ipv6Addr>,
    impair: Option<LinkCondition>,
}

impl DeviceIface {
    /// Returns the interface name (e.g. `"eth0"`).
    pub fn name(&self) -> &str {
        &self.ifname
    }

    /// Returns the assigned IPv4 address, if any.
    pub fn ip(&self) -> Option<Ipv4Addr> {
        self.ip
    }

    /// Returns the assigned IPv6 address, if any.
    pub fn ip6(&self) -> Option<Ipv6Addr> {
        self.ip_v6
    }

    /// Returns the impairment profile, if any.
    pub fn impair(&self) -> Option<LinkCondition> {
        self.impair
    }
}

/// Cloneable handle to a device in the lab topology.
///
/// Holds a [`NodeId`] and an `Arc` to the lab interior. All accessor methods
/// briefly lock the mutex, read a value, and return owned data.
///
/// [`name`](Self::name) and [`ns`](Self::ns) are cached and always available.
/// Other accessors return `None` if the device has been removed via
/// [`Lab::remove_device`](crate::Lab::remove_device). Mutation methods return
/// `Err` in that case.
pub struct Device {
    id: NodeId,
    name: Arc<str>,
    ns: Arc<str>,
    lab: Arc<LabInner>,
}

impl Clone for Device {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            name: Arc::clone(&self.name),
            ns: Arc::clone(&self.ns),
            lab: Arc::clone(&self.lab),
        }
    }
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device")
            .field("id", &self.id)
            .field("name", &self.name)
            .finish()
    }
}

impl Device {
    pub(crate) fn new(id: NodeId, name: Arc<str>, ns: Arc<str>, lab: Arc<LabInner>) -> Self {
        Self { id, name, ns, lab }
    }

    /// Returns the node identifier.
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// Returns the device name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the network namespace name for this device.
    pub fn ns(&self) -> &str {
        &self.ns
    }

    /// Returns the IPv4 address of the default interface, if assigned.
    ///
    /// Returns `None` if the device has been removed or no IPv4 is assigned.
    pub fn ip(&self) -> Option<Ipv4Addr> {
        self.lab
            .with_device(self.id, |d| d.default_iface().ip)
            .flatten()
    }

    /// Returns the IPv6 address of the default interface, if assigned.
    ///
    /// Returns `None` if the device has been removed or no IPv6 is assigned.
    pub fn ip6(&self) -> Option<Ipv6Addr> {
        self.lab
            .with_device(self.id, |d| d.default_iface().ip_v6)
            .flatten()
    }

    /// Returns the configured MTU, if set.
    ///
    /// Returns `None` if the device has been removed or no MTU is configured.
    pub fn mtu(&self) -> Option<u32> {
        self.lab.with_device(self.id, |d| d.mtu).flatten()
    }

    /// Returns a snapshot of the named interface, if it exists.
    ///
    /// Returns `None` if the device has been removed or the interface does
    /// not exist.
    pub fn iface(&self, name: &str) -> Option<DeviceIface> {
        let inner = self.lab.core.lock().unwrap();
        let dev = inner.device(self.id)?;
        let iface = dev.iface(name)?;
        Some(DeviceIface {
            ifname: iface.ifname.clone(),
            ip: iface.ip,
            ip_v6: iface.ip_v6,
            impair: iface.impair,
        })
    }

    /// Returns a snapshot of the default interface, or `None` if the device
    /// has been removed.
    pub fn default_iface(&self) -> Option<DeviceIface> {
        self.lab.with_device(self.id, |dev| {
            let iface = dev.default_iface();
            DeviceIface {
                ifname: iface.ifname.clone(),
                ip: iface.ip,
                ip_v6: iface.ip_v6,
                impair: iface.impair,
            }
        })
    }

    /// Returns snapshots of all interfaces.
    ///
    /// Returns an empty `Vec` if the device has been removed.
    pub fn interfaces(&self) -> Vec<DeviceIface> {
        let inner = self.lab.core.lock().unwrap();
        let dev = match inner.device(self.id) {
            Some(d) => d,
            None => return vec![],
        };
        dev.interfaces
            .iter()
            .map(|iface| DeviceIface {
                ifname: iface.ifname.clone(),
                ip: iface.ip,
                ip_v6: iface.ip_v6,
                impair: iface.impair,
            })
            .collect()
    }

    // ── Dynamic operations ──────────────────────────────────────────────

    /// Brings an interface administratively down.
    ///
    /// # Errors
    ///
    /// Returns an error if the device has been removed or the netlink
    /// operation fails.
    pub async fn link_down(&self, ifname: &str) -> Result<()> {
        let ns = self.ns.to_string();
        let ifname = ifname.to_string();
        core::nl_run(&self.lab.netns, &ns, move |nl: Netlink| async move {
            nl.set_link_down(&ifname).await
        })
        .await
    }

    /// Brings an interface administratively up.
    ///
    /// Linux removes routes via an interface when it goes admin-down, so we
    /// re-add the default route if `ifname` is the device's current `default_via`.
    ///
    /// # Errors
    ///
    /// Returns an error if the device has been removed or the netlink
    /// operation fails.
    pub async fn link_up(&self, ifname: &str) -> Result<()> {
        let (ns, uplink, is_default_via) = {
            let inner = self.lab.core.lock().unwrap();
            let dev = inner
                .device(self.id)
                .ok_or_else(|| anyhow!("device removed"))?;
            let iface = dev
                .iface(ifname)
                .ok_or_else(|| anyhow!("interface '{}' not found", ifname))?;
            (dev.ns.clone(), iface.uplink, dev.default_via == ifname)
        };
        let ifname_owned = ifname.to_string();
        core::nl_run(&self.lab.netns, &ns, {
            let ifname_owned = ifname_owned.clone();
            move |nl: Netlink| async move { nl.set_link_up(&ifname_owned).await }
        })
        .await?;
        if is_default_via {
            let gw_ip = self
                .lab
                .core
                .lock()
                .unwrap()
                .router_downlink_gw_for_switch(uplink)?;
            core::nl_run(&self.lab.netns, &ns, move |nl: Netlink| async move {
                nl.replace_default_route_v4(&ifname_owned, gw_ip).await
            })
            .await?;
        }
        Ok(())
    }

    /// Sets the active default route to a different interface.
    ///
    /// Replaces the kernel default route and re-applies any link impairment
    /// configured on the target interface.
    ///
    /// # Errors
    ///
    /// Returns an error if the device has been removed, `to` is not a known
    /// interface on this device, or the netlink route replacement fails.
    pub async fn set_default_route(&self, to: &str) -> Result<()> {
        let op = self
            .lab
            .with_device(self.id, |d| Arc::clone(&d.op))
            .ok_or_else(|| anyhow!("device removed"))?;
        let _guard = op.lock().await;
        let (ns, impair, gw_ip) = {
            let inner = self.lab.core.lock().unwrap();
            let dev = inner
                .device(self.id)
                .ok_or_else(|| anyhow!("device removed"))?;
            let iface = dev
                .iface(to)
                .ok_or_else(|| anyhow!("interface '{}' not found", to))?;
            let gw_ip = inner.router_downlink_gw_for_switch(iface.uplink)?;
            (dev.ns.clone(), iface.impair, gw_ip)
        };
        let to_owned = to.to_string();
        core::nl_run(&self.lab.netns, &ns, move |nl: Netlink| async move {
            nl.replace_default_route_v4(&to_owned, gw_ip).await
        })
        .await?;
        apply_or_remove_impair(&self.lab.netns, &ns, to, impair).await;
        self.lab
            .core
            .lock()
            .unwrap()
            .set_device_default_via(self.id, to)?;
        Ok(())
    }

    /// Applies or removes a link-layer impairment on the named interface.
    ///
    /// Pass `Some(condition)` to apply `tc netem` rules, or `None` to remove
    /// any existing impairment and restore the default qdisc.
    ///
    /// # Errors
    ///
    /// Returns an error if the device has been removed or the interface does
    /// not exist on this device.
    pub async fn set_link_condition(
        &self,
        ifname: &str,
        impair: Option<LinkCondition>,
    ) -> Result<()> {
        let op = self
            .lab
            .with_device(self.id, |d| Arc::clone(&d.op))
            .ok_or_else(|| anyhow!("device removed"))?;
        let _guard = op.lock().await;
        let (ns, resolved_ifname) = {
            let inner = self.lab.core.lock().unwrap();
            let dev = inner
                .device(self.id)
                .ok_or_else(|| anyhow!("device removed"))?;
            let iname = ifname.to_string();
            if dev.iface(&iname).is_none() {
                bail!("interface '{}' not found", iname);
            }
            (dev.ns.clone(), iname)
        };
        apply_or_remove_impair(&self.lab.netns, &ns, &resolved_ifname, impair).await;
        {
            let mut inner = self.lab.core.lock().unwrap();
            if let Some(dev) = inner.device_mut(self.id) {
                if let Some(iface) = dev.iface_mut(&resolved_ifname) {
                    iface.impair = impair;
                }
            }
        }
        Ok(())
    }

    // ── Spawn / run ────────────────────────────────────────────────────

    /// Spawns an async task on this device's namespace tokio runtime.
    ///
    /// The closure receives a cloned [`Device`] handle and can use
    /// `tokio::net` for network I/O that will go through this device's
    /// network namespace.
    ///
    /// # Errors
    ///
    /// Returns an error if the namespace worker is not available.
    pub fn spawn<F, Fut, T>(&self, f: F) -> Result<tokio::task::JoinHandle<T>>
    where
        F: FnOnce(Device) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let rt = self.lab.rt_handle_for(&self.ns)?;
        let handle = self.clone();
        Ok(rt.spawn(f(handle)))
    }

    /// Runs a short-lived sync closure in this device's network namespace.
    ///
    /// Blocks the caller until the closure returns. Only for fast,
    /// non-blocking work (sysctl writes, `Command::spawn`). **Never** perform
    /// TCP/UDP I/O here — use [`spawn`](Self::spawn) with `tokio::net` instead.
    pub fn run_sync<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        self.lab.netns.run_closure_in(&self.ns, f)
    }

    /// Spawns a dedicated OS thread in this device's network namespace.
    ///
    /// The thread inherits the namespace's network stack and DNS overlays.
    /// Use for long-running blocking work that cannot be made async.
    pub fn spawn_thread<F, R>(&self, f: F) -> Result<thread::JoinHandle<Result<R>>>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        self.lab.netns.spawn_thread_in(&self.ns, f)
    }

    /// Spawns a raw command in this device's network namespace.
    ///
    /// The sync worker thread has `/etc/hosts` and `/etc/resolv.conf` bind-mounted.
    /// `fork()` inherits the mount namespace, so child processes automatically see
    /// the DNS overlay without a separate `pre_exec` hook.
    pub fn spawn_command(&self, mut cmd: Command) -> Result<std::process::Child> {
        let ns = self.ns.to_string();
        self.lab.netns.run_closure_in(&ns, move || {
            cmd.spawn().context("spawn command in namespace")
        })
    }

    /// Spawns a [`tokio::process::Command`] in this device's network namespace.
    ///
    /// The child is registered with the namespace's tokio reactor so that
    /// `.wait()` and `.wait_with_output()` work as non-blocking futures.
    /// The sync worker's DNS bind-mounts are inherited by the child process.
    pub fn spawn_command_async(
        &self,
        mut cmd: tokio::process::Command,
    ) -> Result<tokio::process::Child> {
        let ns = self.ns.to_string();
        let rt = self.lab.rt_handle_for(&ns)?;
        self.lab.netns.run_closure_in(&ns, move || {
            let _guard = rt.enter();
            cmd.spawn().context("spawn async command in namespace")
        })
    }

    /// Probes the NAT mapping seen by a reflector from this device.
    ///
    /// Sends a UDP probe to `reflector` and returns the [`ObservedAddr`] — the
    /// `ip:port` as seen by the reflector after NAT translation.
    ///
    /// The local bind port is deterministic based on the device's [`NodeId`].
    pub fn probe_udp_mapping(&self, reflector: SocketAddr) -> Result<ObservedAddr> {
        let base = 40000u16;
        let port = base + ((self.id.0 % 20000) as u16);
        let unspec = if reflector.is_ipv4() {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        } else {
            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        };
        let bind = SocketAddr::new(unspec, port);
        self.run_sync(move || {
            crate::test_utils::probe_udp(reflector, Duration::from_millis(500), Some(bind))
        })
    }

    /// Spawns a STUN-like UDP reflector in this device's network namespace.
    ///
    /// The reflector echoes the sender's observed `ip:port` back, enabling
    /// NAT mapping tests via [`probe_udp_mapping`](Self::probe_udp_mapping).
    pub fn spawn_reflector(&self, bind: SocketAddr) -> Result<()> {
        self.lab.spawn_reflector_in(&self.ns, bind)
    }

    /// Adds a hosts entry visible only to this device.
    ///
    /// Written to this device's hosts file overlay. glibc picks up changes
    /// on the next `getaddrinfo()` via mtime check.
    pub fn dns_entry(&self, name: &str, ip: IpAddr) -> Result<()> {
        let mut inner = self.lab.core.lock().unwrap();
        inner
            .dns
            .per_device
            .entry(self.id)
            .or_default()
            .push((name.to_string(), ip));
        inner.dns.write_hosts_file(self.id)
    }

    /// Resolves a name using this device's entries plus lab-wide entries.
    ///
    /// For in-process Rust code that cannot see the bind-mounted `/etc/hosts`.
    /// Spawned child processes resolve names through glibc automatically.
    pub fn resolve(&self, name: &str) -> Option<IpAddr> {
        let inner = self.lab.core.lock().unwrap();
        inner.dns.resolve(Some(self.id), name)
    }

    /// Adds a new interface to this device at runtime, connected to the given
    /// router's downstream network.
    ///
    /// The new interface gets an IP allocated from the router's pool.  It does
    /// **not** become the default route unless you call
    /// [`set_default_route`](Self::set_default_route) afterwards.
    ///
    /// # Errors
    ///
    /// Returns an error if the device has been removed, the router is unknown,
    /// the router has no downstream switch, or the name collides with an
    /// existing interface.
    pub async fn add_iface(
        &self,
        ifname: &str,
        router: NodeId,
        impair: Option<LinkCondition>,
    ) -> Result<()> {
        use crate::core::{self, IfaceBuild};

        let op = self
            .lab
            .with_device(self.id, |d| Arc::clone(&d.op))
            .ok_or_else(|| anyhow!("device removed"))?;
        let _guard = op.lock().await;

        // Phase 1: Lock → register iface + allocate IP → unlock
        let (iface_build, prefix, root_ns, mtu) = {
            let mut inner = self.lab.core.lock().unwrap();
            let dev = inner
                .device(self.id)
                .ok_or_else(|| anyhow!("device removed"))?;
            if dev.interfaces.iter().any(|i| i.ifname == ifname) {
                bail!("device '{}' already has interface '{}'", dev.name, ifname);
            }
            let dev_ns = dev.ns.clone();
            let mtu = dev.mtu;

            // Register the interface in core (allocates IP, idx, updates records).
            inner.add_device_iface(self.id, ifname, router, impair)?;

            // Now snapshot what we need for wiring.
            let dev = inner.device(self.id).unwrap();
            let iface = dev.interfaces.iter().find(|i| i.ifname == ifname).unwrap();
            let sw = inner
                .switch(iface.uplink)
                .ok_or_else(|| anyhow!("switch missing"))?;
            let gw_router = sw
                .owner_router
                .ok_or_else(|| anyhow!("switch missing owner"))?;
            let gw_br = sw.bridge.clone().unwrap_or_else(|| "br-lan".to_string());
            let gw_ns = inner.router(gw_router).unwrap().ns.clone();

            let build = IfaceBuild {
                dev_ns,
                gw_ns,
                gw_ip: sw.gw,
                gw_br,
                dev_ip: iface.ip,
                prefix_len: sw.cidr.map(|c| c.prefix_len()).unwrap_or(24),
                gw_ip_v6: sw.gw_v6,
                dev_ip_v6: iface.ip_v6,
                prefix_len_v6: sw.cidr_v6.map(|c| c.prefix_len()).unwrap_or(64),
                impair,
                ifname: ifname.to_string(),
                is_default: false, // never default — caller opts in via set_default_route
                idx: iface.idx,
            };
            let prefix = inner.cfg.prefix.clone();
            let root_ns = inner.cfg.root_ns.clone();
            (build, prefix, root_ns, mtu)
        };

        // Phase 2: Wire the interface (veth pair, IPs, bridge attachment).
        let netns = &self.lab.netns;
        core::wire_iface_async(netns, &prefix, &root_ns, iface_build).await?;

        // Phase 3: Apply MTU if the device has one configured.
        if let Some(mtu) = mtu {
            let dev_ns = self.ns.to_string();
            let ifname_owned = ifname.to_string();
            core::nl_run(netns, &dev_ns, move |h: Netlink| async move {
                h.set_mtu(&ifname_owned, mtu).await?;
                Ok(())
            })
            .await?;
        }

        Ok(())
    }

    /// Removes an interface from this device, tearing down its veth pair.
    ///
    /// If the removed interface was the default route, the default switches to
    /// the first remaining interface (if any).  Cannot remove the last interface.
    ///
    /// # Errors
    ///
    /// Returns an error if the device has been removed, `ifname` does not exist,
    /// or it is the only interface on the device.
    pub async fn remove_iface(&self, ifname: &str) -> Result<()> {
        use crate::core;

        let op = self
            .lab
            .with_device(self.id, |d| Arc::clone(&d.op))
            .ok_or_else(|| anyhow!("device removed"))?;
        let _guard = op.lock().await;

        // Phase 1: Lock → validate + remove from records → unlock
        let dev_ns = {
            let mut inner = self.lab.core.lock().unwrap();
            let dev = inner
                .device_mut(self.id)
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
                .position(|i| i.ifname == ifname)
                .ok_or_else(|| anyhow!("device '{}' has no interface '{}'", dev.name, ifname))?;
            dev.interfaces.remove(pos);
            // Fix default_via if we just removed it.
            if dev.default_via == ifname {
                dev.default_via = dev.interfaces[0].ifname.clone();
            }
            dev.ns.clone()
        };

        // Phase 2: Delete the veth pair (peer side auto-removed by kernel).
        let ifname_owned = ifname.to_string();
        core::nl_run(&self.lab.netns, &dev_ns, move |h: Netlink| async move {
            h.ensure_link_deleted(&ifname_owned).await.ok();
            Ok(())
        })
        .await?;

        Ok(())
    }

    /// Moves one of this device's interfaces to a different router's downstream
    /// network, simulating unplugging a cable and plugging it into a new router.
    ///
    /// The interface name is preserved but the IP address changes (allocated from
    /// the new router's pool). The old veth pair is torn down and a fresh one is
    /// created.
    ///
    /// # Errors
    ///
    /// Returns an error if the device has been removed, `ifname` does not exist
    /// on this device, `to_router` is unknown, or the target router has no
    /// downstream switch.
    pub async fn replug_iface(&self, ifname: &str, to_router: NodeId) -> Result<()> {
        use crate::core::{self, IfaceBuild};

        // Phase 1: Lock → extract data + allocate from new router's pool → unlock
        let (iface_build, prefix, root_ns) = {
            let mut inner = self.lab.core.lock().unwrap();
            let dev = inner
                .device(self.id)
                .ok_or_else(|| anyhow!("device removed"))?
                .clone();
            let iface = dev
                .interfaces
                .iter()
                .find(|i| i.ifname == ifname)
                .ok_or_else(|| anyhow!("device '{}' has no interface '{}'", dev.name, ifname))?;
            let old_idx = iface.idx;
            let target_router = inner
                .router(to_router)
                .ok_or_else(|| anyhow!("unknown target router id"))?
                .clone();
            let downlink_sw = target_router.downlink.ok_or_else(|| {
                anyhow!(
                    "target router '{}' has no downstream switch",
                    target_router.name
                )
            })?;
            let sw = inner
                .switch(downlink_sw)
                .ok_or_else(|| anyhow!("target router's downlink switch missing"))?
                .clone();
            let gw_br = sw.bridge.clone().unwrap_or_else(|| "br-lan".to_string());
            let new_ip = if sw.cidr.is_some() {
                Some(inner.alloc_from_switch(downlink_sw)?)
            } else {
                None
            };
            let new_ip_v6 = if sw.cidr_v6.is_some() {
                Some(inner.alloc_from_switch_v6(downlink_sw)?)
            } else {
                None
            };
            let prefix_len = sw.cidr.map(|c| c.prefix_len()).unwrap_or(24);

            let prefix = inner.cfg.prefix.clone();
            let root_ns = inner.cfg.root_ns.clone();

            let build = IfaceBuild {
                dev_ns: dev.ns.clone(),
                gw_ns: target_router.ns.clone(),
                gw_ip: sw.gw,
                gw_br,
                dev_ip: new_ip,
                prefix_len,
                gw_ip_v6: sw.gw_v6,
                dev_ip_v6: new_ip_v6,
                prefix_len_v6: sw.cidr_v6.map(|c| c.prefix_len()).unwrap_or(64),
                impair: iface.impair,
                ifname: ifname.to_string(),
                is_default: ifname == dev.default_via,
                idx: old_idx,
            };
            (build, prefix, root_ns)
        };

        // Phase 2: Delete old veth pair.
        let dev_ns = iface_build.dev_ns.clone();
        let ifname_owned = ifname.to_string();
        let netns = &self.lab.netns;
        core::nl_run(netns, &dev_ns, move |h: Netlink| async move {
            h.ensure_link_deleted(&ifname_owned).await.ok();
            Ok(())
        })
        .await?;

        // Phase 3: Wire new interface (reuses existing wiring logic)
        let new_ip = iface_build.dev_ip;
        let new_ip_v6 = iface_build.dev_ip_v6;
        let new_uplink = {
            let inner = self.lab.core.lock().unwrap();
            let router = inner
                .router(to_router)
                .ok_or_else(|| anyhow!("target router disappeared"))?;
            router
                .downlink
                .ok_or_else(|| anyhow!("target router has no downlink"))?
        };
        core::wire_iface_async(netns, &prefix, &root_ns, iface_build).await?;

        // Phase 4: Lock → update internal records → unlock
        {
            let mut inner = self.lab.core.lock().unwrap();
            let dev = inner
                .device_mut(self.id)
                .ok_or_else(|| anyhow!("device disappeared"))?;
            if let Some(iface) = dev.interfaces.iter_mut().find(|i| i.ifname == ifname) {
                iface.uplink = new_uplink;
                iface.ip = new_ip;
                iface.ip_v6 = new_ip_v6;
            }
        }

        Ok(())
    }

    /// Simulates DHCP renewal: allocates a new IP from the current router's pool,
    /// replaces the old address on the interface, and returns the new address.
    ///
    /// The default route remains unchanged (same gateway). The old address is
    /// removed before the new one is added.
    ///
    /// # Errors
    ///
    /// Returns an error if the device has been removed, the interface has no
    /// IPv4 address, or the router's address pool is exhausted.
    pub async fn renew_ip(&self, ifname: &str) -> Result<Ipv4Addr> {
        use crate::core;

        // Phase 1: Lock → allocate new IP, update records → unlock
        let (ns, old_ip, new_ip, prefix_len) = {
            let mut inner = self.lab.core.lock().unwrap();
            let dev = inner
                .device(self.id)
                .ok_or_else(|| anyhow!("device removed"))?;
            let iface = dev
                .iface(ifname)
                .ok_or_else(|| anyhow!("device '{}' has no interface '{}'", dev.name, ifname))?;
            let old_ip = iface
                .ip
                .ok_or_else(|| anyhow!("interface '{}' has no IPv4 address", ifname))?;
            let sw_id = iface.uplink;
            let sw = inner
                .switch(sw_id)
                .ok_or_else(|| anyhow!("switch for interface '{}' missing", ifname))?;
            let prefix_len = sw.cidr.map(|c| c.prefix_len()).unwrap_or(24);
            let ns = dev.ns.clone();

            let new_ip = inner.alloc_from_switch(sw_id)?;
            // Update internal record.
            let dev = inner.device_mut(self.id).unwrap();
            let iface = dev.iface_mut(ifname).unwrap();
            iface.ip = Some(new_ip);

            (ns, old_ip, new_ip, prefix_len)
        };

        // Phase 2: Async netlink — remove old addr, add new addr.
        let ifname = ifname.to_string();
        core::nl_run(&self.lab.netns, &ns, move |h: Netlink| async move {
            h.del_addr4(&ifname, old_ip, prefix_len).await?;
            h.add_addr4(&ifname, new_ip, prefix_len).await?;
            Ok(())
        })
        .await?;

        Ok(new_ip)
    }

    /// Adds a secondary IPv4 address to an interface.
    ///
    /// The address is added via netlink without removing existing addresses.
    /// Linux natively supports multiple addresses per interface.
    ///
    /// # Errors
    ///
    /// Returns an error if the device has been removed or the interface does
    /// not exist.
    pub async fn add_ip(&self, ifname: &str, ip: Ipv4Addr, prefix_len: u8) -> Result<()> {
        use crate::core;

        let ns = {
            let inner = self.lab.core.lock().unwrap();
            let dev = inner
                .device(self.id)
                .ok_or_else(|| anyhow!("device removed"))?;
            let _ = dev
                .iface(ifname)
                .ok_or_else(|| anyhow!("device '{}' has no interface '{}'", dev.name, ifname))?;
            dev.ns.clone()
        };

        let ifname = ifname.to_string();
        core::nl_run(&self.lab.netns, &ns, move |h: Netlink| async move {
            h.add_addr4(&ifname, ip, prefix_len).await?;
            Ok(())
        })
        .await?;

        Ok(())
    }
}

/// Cloneable handle to a router in the lab topology.
///
/// Same pattern as [`Device`]: holds [`NodeId`] + `Arc<LabInner>`.
///
/// [`name`](Self::name) and [`ns`](Self::ns) are cached and always available.
/// Other accessors return `None` if the router has been removed via
/// [`Lab::remove_router`](crate::Lab::remove_router). Mutation methods return
/// `Err` in that case.
pub struct Router {
    id: NodeId,
    name: Arc<str>,
    ns: Arc<str>,
    lab: Arc<LabInner>,
}

impl Clone for Router {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            name: Arc::clone(&self.name),
            ns: Arc::clone(&self.ns),
            lab: Arc::clone(&self.lab),
        }
    }
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router")
            .field("id", &self.id)
            .field("name", &self.name)
            .finish()
    }
}

impl Router {
    pub(crate) fn new(id: NodeId, name: Arc<str>, ns: Arc<str>, lab: Arc<LabInner>) -> Self {
        Self { id, name, ns, lab }
    }

    /// Returns the node identifier.
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// Returns the router name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the network namespace name for this router.
    pub fn ns(&self) -> &str {
        &self.ns
    }

    /// Returns the region label, if set.
    ///
    /// Returns `None` if the router has been removed or no region is assigned.
    pub fn region(&self) -> Option<String> {
        self.lab
            .with_router(self.id, |r| r.region.clone())
            .flatten()
    }

    /// Returns the NAT mode, or `None` if the router has been removed.
    pub fn nat_mode(&self) -> Option<Nat> {
        self.lab.with_router(self.id, |r| r.cfg.nat)
    }

    /// Returns the configured MTU, if set.
    ///
    /// Returns `None` if the router has been removed or no MTU is configured.
    pub fn mtu(&self) -> Option<u32> {
        self.lab.with_router(self.id, |r| r.cfg.mtu).flatten()
    }

    /// Returns the uplink (WAN-side) IP, if connected.
    ///
    /// Returns `None` if the router has been removed or no uplink IP is assigned.
    pub fn uplink_ip(&self) -> Option<Ipv4Addr> {
        self.lab.with_router(self.id, |r| r.upstream_ip).flatten()
    }

    /// Returns the downstream subnet CIDR, if allocated.
    ///
    /// Returns `None` if the router has been removed or no downstream is allocated.
    pub fn downstream_cidr(&self) -> Option<Ipv4Net> {
        self.lab
            .with_router(self.id, |r| r.downstream_cidr)
            .flatten()
    }

    /// Returns the downstream gateway address, if allocated.
    ///
    /// Returns `None` if the router has been removed or no downstream is allocated.
    pub fn downstream_gw(&self) -> Option<Ipv4Addr> {
        self.lab.with_router(self.id, |r| r.downstream_gw).flatten()
    }

    /// Returns which IP address families this router supports, or `None` if
    /// the router has been removed.
    pub fn ip_support(&self) -> Option<IpSupport> {
        self.lab.with_router(self.id, |r| r.cfg.ip_support)
    }

    /// Returns the uplink (WAN-side) IPv6 address, if connected.
    ///
    /// Returns `None` if the router has been removed or no IPv6 uplink is assigned.
    pub fn uplink_ip_v6(&self) -> Option<Ipv6Addr> {
        self.lab
            .with_router(self.id, |r| r.upstream_ip_v6)
            .flatten()
    }

    /// Returns the downstream IPv6 subnet CIDR, if allocated.
    ///
    /// Returns `None` if the router has been removed or no IPv6 downstream is allocated.
    pub fn downstream_cidr_v6(&self) -> Option<Ipv6Net> {
        self.lab
            .with_router(self.id, |r| r.downstream_cidr_v6)
            .flatten()
    }

    /// Returns the downstream IPv6 gateway address, if allocated.
    ///
    /// Returns `None` if the router has been removed or no IPv6 downstream is allocated.
    pub fn downstream_gw_v6(&self) -> Option<Ipv6Addr> {
        self.lab
            .with_router(self.id, |r| r.downstream_gw_v6)
            .flatten()
    }

    /// Returns the IPv6 NAT mode, or `None` if the router has been removed.
    pub fn nat_v6_mode(&self) -> Option<NatV6Mode> {
        self.lab.with_router(self.id, |r| r.cfg.nat_v6)
    }

    // ── Dynamic operations ──────────────────────────────────────────────

    /// Replaces NAT rules on this router at runtime.
    ///
    /// Flushes the `ip nat` and `ip filter` nftables tables, then re-applies
    /// rules matching `mode`. The change takes effect immediately for new
    /// connections; existing conntrack entries are not flushed (use
    /// [`flush_nat_state`](Self::flush_nat_state) for that).
    ///
    /// # Errors
    ///
    /// Returns an error if the router has been removed or nftables commands fail.
    pub async fn set_nat_mode(&self, mode: Nat) -> Result<()> {
        let op = self
            .lab
            .with_router(self.id, |r| Arc::clone(&r.op))
            .ok_or_else(|| anyhow!("router removed"))?;
        let _guard = op.lock().await;
        let (ns, wan_if, wan_ip, cfg) = {
            let mut inner = self.lab.core.lock().unwrap();
            inner.set_router_nat_mode(self.id, mode)?;
            let cfg = inner.router_effective_cfg(self.id)?;
            let (ns, _lan_if, wan_if, wan_ip) = inner.router_nat_params(self.id)?;
            (ns, wan_if, wan_ip, cfg)
        };
        run_nft_in(&self.lab.netns, &ns, "flush table ip nat")
            .await
            .ok();
        run_nft_in(&self.lab.netns, &ns, "flush table ip filter")
            .await
            .ok();
        apply_nat_for_router(&self.lab.netns, &ns, &cfg, &wan_if, wan_ip).await
    }

    /// Replaces IPv6 NAT rules on this router at runtime.
    ///
    /// Flushes the `ip6 nat` nftables table, then applies rules matching
    /// `mode` (NPTv6 prefix translation or stateful masquerade). Pass
    /// [`NatV6Mode::None`] to remove all IPv6 NAT rules.
    ///
    /// # Errors
    ///
    /// Returns an error if the router has been removed or nftables commands fail.
    pub async fn set_nat_v6_mode(&self, mode: NatV6Mode) -> Result<()> {
        let op = self
            .lab
            .with_router(self.id, |r| Arc::clone(&r.op))
            .ok_or_else(|| anyhow!("router removed"))?;
        let _guard = op.lock().await;
        let (ns, wan_if, lan_prefix, wan_prefix) = {
            let inner = self.lab.core.lock().unwrap();
            let router = inner
                .router(self.id)
                .ok_or_else(|| anyhow!("router removed"))?;
            let wan_if = router.wan_ifname(inner.ix_sw()).to_string();
            let lan_prefix = router
                .downstream_cidr_v6
                .unwrap_or(net6(Ipv6Addr::new(0xfd10, 0, 0, 0, 0, 0, 0, 0), 64));
            let wan_prefix = {
                let up_ip = router.upstream_ip_v6.unwrap_or(Ipv6Addr::UNSPECIFIED);
                let up_prefix = if router.uplink == Some(inner.ix_sw()) {
                    inner.cfg.ix_cidr_v6.prefix_len()
                } else {
                    router
                        .uplink
                        .and_then(|sw| inner.switch(sw))
                        .and_then(|sw| sw.cidr_v6)
                        .map(|c| c.prefix_len())
                        .unwrap_or(64)
                };
                Ipv6Net::new(up_ip, up_prefix).unwrap_or_else(|_| Ipv6Net::new(up_ip, 128).unwrap())
            };
            (router.ns.clone(), wan_if, lan_prefix, wan_prefix)
        };
        run_nft_in(&self.lab.netns, &ns, "flush table ip6 nat")
            .await
            .ok();
        apply_nat_v6(&self.lab.netns, &ns, mode, &wan_if, lan_prefix, wan_prefix).await?;
        {
            let mut inner = self.lab.core.lock().unwrap();
            let router = inner
                .router_mut(self.id)
                .ok_or_else(|| anyhow!("router removed"))?;
            router.cfg.nat_v6 = mode;
        }
        Ok(())
    }

    /// Flushes the conntrack table, forcing all active NAT mappings to expire.
    ///
    /// Subsequent flows get new external port assignments. Pair with
    /// [`set_nat_mode`](Self::set_nat_mode) when testing mode transitions.
    ///
    /// # Errors
    ///
    /// Returns an error if the router has been removed or `conntrack -F` fails.
    pub async fn flush_nat_state(&self) -> Result<()> {
        let op = self
            .lab
            .with_router(self.id, |r| Arc::clone(&r.op))
            .ok_or_else(|| anyhow!("router removed"))?;
        let _guard = op.lock().await;
        let ns = self.ns.to_string();
        let rt = self.lab.netns.rt_handle_for(&ns)?;
        rt.spawn(async move {
            let st = tokio::process::Command::new("conntrack")
                .arg("-F")
                .status()
                .await?;
            if !st.success() {
                bail!("conntrack -F failed: {st}");
            }
            Ok(())
        })
        .await
        .context("conntrack flush task panicked")?
    }

    // ── Spawn / run ────────────────────────────────────────────────────

    /// Spawns an async task on this router's namespace tokio runtime.
    ///
    /// The closure receives a cloned [`Router`] handle and can use
    /// `tokio::net` for network I/O through this router's namespace.
    ///
    /// # Errors
    ///
    /// Returns an error if the namespace worker is not available.
    pub fn spawn<F, Fut, T>(&self, f: F) -> Result<tokio::task::JoinHandle<T>>
    where
        F: FnOnce(Router) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let rt = self.lab.rt_handle_for(&self.ns)?;
        let handle = self.clone();
        Ok(rt.spawn(f(handle)))
    }

    /// Runs a short-lived sync closure in this router's network namespace.
    ///
    /// Blocks the caller until the closure returns. Only for fast,
    /// non-blocking work. **Never** perform TCP/UDP I/O here.
    pub fn run_sync<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        self.lab.netns.run_closure_in(&self.ns, f)
    }

    /// Spawns a dedicated OS thread in this router's network namespace.
    ///
    /// The thread inherits the namespace's network stack and DNS overlays.
    pub fn spawn_thread<F, R>(&self, f: F) -> Result<thread::JoinHandle<Result<R>>>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        self.lab.netns.spawn_thread_in(&self.ns, f)
    }

    /// Spawns a [`Command`] in this router's network namespace.
    ///
    /// The child inherits the namespace's DNS bind-mounts.
    pub fn spawn_command(&self, mut cmd: Command) -> Result<std::process::Child> {
        let ns = self.ns.to_string();
        self.lab.netns.run_closure_in(&ns, move || {
            cmd.spawn().context("spawn command in namespace")
        })
    }

    /// Spawns a [`tokio::process::Command`] in this router's network namespace.
    ///
    /// The child is registered with the namespace's tokio reactor.
    pub fn spawn_command_async(
        &self,
        mut cmd: tokio::process::Command,
    ) -> Result<tokio::process::Child> {
        let ns = self.ns.to_string();
        let rt = self.lab.rt_handle_for(&ns)?;
        self.lab.netns.run_closure_in(&ns, move || {
            let _guard = rt.enter();
            cmd.spawn().context("spawn async command in namespace")
        })
    }

    /// Applies or removes impairment on this router's downlink bridge.
    ///
    /// Affects download-direction traffic to **all** downstream devices.
    /// Pass `Some(condition)` to apply `tc netem` rules on the bridge, or
    /// `None` to remove any existing impairment.
    ///
    /// # Errors
    ///
    /// Returns an error if the router has been removed.
    pub async fn set_downlink_condition(&self, impair: Option<LinkCondition>) -> Result<()> {
        let op = self
            .lab
            .with_router(self.id, |r| Arc::clone(&r.op))
            .ok_or_else(|| anyhow!("router removed"))?;
        let _guard = op.lock().await;
        debug!(router = ?self.id, impair = ?impair, "router: set_downlink_condition");
        let bridge = self
            .lab
            .with_router(self.id, |r| r.downlink_bridge.clone())
            .ok_or_else(|| anyhow!("router removed"))?;
        apply_or_remove_impair(&self.lab.netns, &self.ns, &bridge, impair).await;
        Ok(())
    }

    /// Sets or removes the firewall on this router at runtime.
    ///
    /// Removes any existing firewall rules before applying the new preset.
    /// Pass [`Firewall::None`] to remove all firewall rules without adding new ones.
    ///
    /// # Errors
    ///
    /// Returns an error if the router has been removed or nftables commands fail.
    pub async fn set_firewall(&self, fw: Firewall) -> Result<()> {
        let op = self
            .lab
            .with_router(self.id, |r| Arc::clone(&r.op))
            .ok_or_else(|| anyhow!("router removed"))?;
        let _guard = op.lock().await;
        {
            let mut inner = self.lab.core.lock().unwrap();
            let r = inner.router_mut(self.id).context("router removed")?;
            r.cfg.firewall = fw.clone();
        }
        let ns = self.ns.to_string();
        // Always remove existing rules first, then apply new ones.
        core::remove_firewall(&self.lab.netns, &ns).await?;
        core::apply_firewall(&self.lab.netns, &ns, &fw).await
    }

    /// Spawns a STUN-like UDP reflector in this router's network namespace.
    ///
    /// See [`Device::spawn_reflector`] for details.
    pub fn spawn_reflector(&self, bind: SocketAddr) -> Result<()> {
        self.lab.spawn_reflector_in(&self.ns, bind)
    }
}

// ─────────────────────────────────────────────
// Ix handle
// ─────────────────────────────────────────────

/// Handle to the Internet Exchange — the lab's root namespace that hosts
/// the shared bridge connecting all IX-level routers.
///
/// Same pattern as [`Device`] and [`Router`]: holds an `Arc` to the lab
/// interior. All accessor methods briefly lock the mutex.
pub struct Ix {
    lab: Arc<LabInner>,
}

impl Clone for Ix {
    fn clone(&self) -> Self {
        Self {
            lab: Arc::clone(&self.lab),
        }
    }
}

impl std::fmt::Debug for Ix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ix").finish()
    }
}

impl Ix {
    pub(crate) fn new(lab: Arc<LabInner>) -> Self {
        Self { lab }
    }

    /// Returns the root namespace name.
    pub fn ns(&self) -> String {
        self.lab.core.lock().unwrap().root_ns().to_string()
    }

    /// Returns the IX gateway IPv4 address (e.g. 203.0.113.1).
    pub fn gw(&self) -> Ipv4Addr {
        self.lab.core.lock().unwrap().ix_gw()
    }

    /// Returns the IX gateway IPv6 address (e.g. 2001:db8::1).
    pub fn gw_v6(&self) -> Ipv6Addr {
        self.lab.core.lock().unwrap().cfg.ix_gw_v6
    }

    /// Spawns an async task on the IX root namespace's tokio runtime.
    ///
    /// The closure receives a cloned [`Ix`] handle.
    pub fn spawn<F, Fut, T>(&self, f: F) -> tokio::task::JoinHandle<T>
    where
        F: FnOnce(Ix) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let ns = self.lab.core.lock().unwrap().root_ns().to_string();
        let rt = self
            .lab
            .rt_handle_for(&ns)
            .expect("root namespace has async worker");
        let handle = self.clone();
        rt.spawn(f(handle))
    }

    /// Runs a short-lived sync closure in the IX root namespace.
    ///
    /// Blocks the caller until the closure returns.
    pub fn run_sync<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let ns = self.lab.core.lock().unwrap().root_ns().to_string();
        self.lab.netns.run_closure_in(&ns, f)
    }

    /// Spawns a dedicated OS thread in the IX root namespace.
    pub fn spawn_thread<F, R>(&self, f: F) -> Result<thread::JoinHandle<Result<R>>>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let ns = self.lab.core.lock().unwrap().root_ns().to_string();
        self.lab.netns.spawn_thread_in(&ns, f)
    }

    /// Spawns a [`Command`] in the IX root namespace.
    pub fn spawn_command(&self, mut cmd: Command) -> Result<std::process::Child> {
        let ns = self.lab.core.lock().unwrap().root_ns().to_string();
        self.lab.netns.run_closure_in(&ns, move || {
            cmd.spawn().context("spawn command in namespace")
        })
    }

    /// Spawns a [`tokio::process::Command`] in the IX root namespace.
    pub fn spawn_command_async(
        &self,
        mut cmd: tokio::process::Command,
    ) -> Result<tokio::process::Child> {
        let ns = self.lab.core.lock().unwrap().root_ns().to_string();
        let rt = self.lab.rt_handle_for(&ns)?;
        self.lab.netns.run_closure_in(&ns, move || {
            let _guard = rt.enter();
            cmd.spawn().context("spawn async command in namespace")
        })
    }

    /// Spawns a STUN-like UDP reflector in the IX root namespace.
    ///
    /// See [`Device::spawn_reflector`] for details.
    pub fn spawn_reflector(&self, bind: SocketAddr) -> Result<()> {
        let ns = self.lab.core.lock().unwrap().root_ns().to_string();
        self.lab.spawn_reflector_in(&ns, bind)
    }
}

// ─────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────

/// Normalizes a device/interface name for use in an environment variable name.
pub(crate) fn normalize_env_name(s: &str) -> String {
    s.to_uppercase().replace('-', "_")
}
