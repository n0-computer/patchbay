//! Device handle and builder.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    process::Command,
    sync::Arc,
    thread,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use tracing::Instrument as _;

use crate::{
    core::{self, IfaceBuild, NodeId},
    event::{DeviceState, IfaceSnapshot, LabEventKind},
    lab::{Ipv6ProvisioningMode, LabInner},
    netlink::Netlink,
    nft::apply_or_remove_impair,
    wiring::{self, setup_device_async, DeviceSetupData},
};

/// Record a metric via the given tracing dispatch.
pub(crate) fn record_metric(dispatch: &tracing::Dispatch, key: &str, value: f64) {
    let mut map = serde_json::Map::new();
    if let Some(n) = serde_json::Number::from_f64(value) {
        map.insert(key.to_string(), serde_json::Value::Number(n));
    }
    let json = serde_json::to_string(&map).unwrap_or_default();
    let _guard = tracing::dispatcher::set_default(dispatch);
    tracing::event!(
        target: "patchbay::_metrics",
        tracing::Level::INFO,
        metrics_json = %json,
    );
}

pub(crate) fn select_default_v6_gateway(
    provisioning: Ipv6ProvisioningMode,
    ra_default_enabled: bool,
    gw_v6: Option<Ipv6Addr>,
    gw_ll_v6: Option<Ipv6Addr>,
) -> Option<Ipv6Addr> {
    if provisioning == Ipv6ProvisioningMode::RaDriven {
        if ra_default_enabled {
            gw_ll_v6.or(gw_v6)
        } else {
            None
        }
    } else {
        gw_v6.or(gw_ll_v6)
    }
}

// ─────────────────────────────────────────────
// Device handle
// ─────────────────────────────────────────────

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
    dispatch: tracing::Dispatch,
}

impl Clone for Device {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            name: Arc::clone(&self.name),
            ns: Arc::clone(&self.ns),
            lab: Arc::clone(&self.lab),
            dispatch: self.dispatch.clone(),
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
        let dispatch = lab
            .netns
            .dispatch_for(&ns)
            .unwrap_or_else(|| tracing::dispatcher::get_default(|d| d.clone()));
        Self {
            id,
            name,
            ns,
            lab,
            dispatch,
        }
    }

    /// Enter this device's tracing context.
    pub fn enter_tracing(&self) -> tracing::subscriber::DefaultGuard {
        tracing::dispatcher::set_default(&self.dispatch)
    }

    /// Record a single metric.
    pub fn record(&self, key: &str, value: f64) {
        record_metric(&self.dispatch, key, value);
    }

    /// Returns a builder for recording multiple metrics at once.
    pub fn metrics(&self) -> crate::metrics::MetricsBuilder {
        crate::metrics::MetricsBuilder::new(self.dispatch.clone())
    }

    /// Record all counter/gauge values from an iroh-metrics group.
    ///
    /// Iterates the group's metrics and emits each counter or gauge as a
    /// patchbay metric line. Histograms are skipped.
    #[cfg(feature = "iroh-metrics")]
    pub fn record_iroh_metrics(&self, group: &dyn iroh_metrics::MetricsGroup) {
        let _guard = self.enter_tracing();
        let mut builder = self.metrics();
        for item in group.iter() {
            let value: f64 = match item.value() {
                iroh_metrics::MetricValue::Counter(v) => v as f64,
                iroh_metrics::MetricValue::Gauge(v) => v as f64,
                _ => continue,
            };
            builder = builder.record(item.name(), value);
        }
        builder.emit();
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

    /// Builds a path in the lab run directory for this device.
    ///
    /// Returns `None` when the lab was created without an output directory.
    /// The resulting filename is `device.{name}.{suffix}`.
    pub fn filepath(&self, suffix: &str) -> Option<PathBuf> {
        let run_dir = self.lab.run_dir.as_ref()?;
        let suffix = suffix.trim_start_matches('.');
        let filename = crate::consts::node_file(crate::consts::KIND_DEVICE, &self.name, suffix);
        Some(run_dir.join(filename))
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

    /// Returns a handle to the named interface, if it exists.
    ///
    /// Returns `None` if the device has been removed or the interface does
    /// not exist.
    pub fn iface(&self, name: &str) -> Option<crate::Iface> {
        let inner = self.lab.core.lock().expect("poisoned");
        let dev = inner.device(self.id)?;
        let _ = dev.iface(name)?;
        Some(crate::Iface::new(
            self.id,
            name.into(),
            Arc::clone(&self.lab),
        ))
    }

    /// Returns a handle to the default interface, or `None` if the device
    /// has been removed.
    pub fn default_iface(&self) -> Option<crate::Iface> {
        let inner = self.lab.core.lock().expect("poisoned");
        let dev = inner.device(self.id)?;
        let iface = dev.default_iface();
        Some(crate::Iface::new(
            self.id,
            iface.ifname.clone(),
            Arc::clone(&self.lab),
        ))
    }

    /// Returns handles to all interfaces.
    ///
    /// Returns an empty `Vec` if the device has been removed.
    pub fn interfaces(&self) -> Vec<crate::Iface> {
        let inner = self.lab.core.lock().expect("poisoned");
        let dev = match inner.device(self.id) {
            Some(d) => d,
            None => return vec![],
        };
        dev.interfaces
            .iter()
            .map(|iface| crate::Iface::new(self.id, iface.ifname.clone(), Arc::clone(&self.lab)))
            .collect()
    }

    fn provisioning_mode(&self) -> Result<Ipv6ProvisioningMode> {
        let inner = self.lab.core.lock().expect("poisoned");
        let dev = inner
            .device(self.id)
            .ok_or_else(|| anyhow!("device removed"))?;
        Ok(dev
            .provisioning_mode
            .unwrap_or(self.lab.ipv6_provisioning_mode))
    }

    // ── Dynamic operations ──────────────────────────────────────────────

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
        let provisioning = self.provisioning_mode()?;
        let (ns, egress, gw_ip, gw_v6, gw_ll_v6, ra_default_enabled) = {
            let inner = self.lab.core.lock().expect("poisoned");
            let dev = inner
                .device(self.id)
                .ok_or_else(|| anyhow!("device removed"))?;
            let iface = dev
                .iface(to)
                .ok_or_else(|| anyhow!("interface '{}' not found", to))?;
            let uplink = iface
                .uplink()
                .ok_or_else(|| anyhow!("cannot set default route to dummy interface '{}'", to))?;
            let gw_ip = inner.router_downlink_gw_for_switch(uplink)?;
            let gw_v6 = inner.router_downlink_gw6_for_switch(uplink)?;
            let ra_default_enabled = inner.ra_default_enabled_for_switch(uplink)?;
            (
                dev.ns.clone(),
                iface.egress,
                gw_ip,
                gw_v6.global_v6,
                gw_v6.link_local_v6,
                ra_default_enabled,
            )
        };
        let to_owned = to.to_string();
        let primary_v6 =
            select_default_v6_gateway(provisioning, ra_default_enabled, gw_v6, gw_ll_v6);
        wiring::nl_run(&self.lab.netns, &ns, move |nl: Netlink| async move {
            nl.replace_default_route_v4(&to_owned, gw_ip).await?;
            nl.set_default_route_v6(&to_owned, primary_v6).await
        })
        .await?;
        if provisioning == Ipv6ProvisioningMode::RaDriven {
            let rs_router_ll = if ra_default_enabled { gw_ll_v6 } else { None };
            wiring::emit_router_solicitation(
                &self.lab.netns,
                ns.to_string(),
                self.name.to_string(),
                to.to_string(),
                rs_router_ll,
            )
            .await?;
        }
        apply_or_remove_impair(&self.lab.netns, &ns, to, egress).await;
        self.lab
            .core
            .lock()
            .expect("poisoned")
            .set_device_default_via(self.id, to)?;
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

    /// Spawns a [`tokio::process::Command`] in this device's network namespace.
    ///
    /// The child is registered with the namespace's tokio reactor so that
    /// `.wait()` and `.wait_with_output()` work as non-blocking futures.
    /// The sync worker's DNS bind-mounts are inherited by the child process.
    pub fn spawn_command(&self, mut cmd: tokio::process::Command) -> Result<tokio::process::Child> {
        let ns = self.ns.to_string();
        let rt = self.lab.rt_handle_for(&ns)?;
        self.lab.netns.run_closure_in(&ns, move || {
            let _guard = rt.enter();
            cmd.spawn().context("spawn async command in namespace")
        })
    }

    /// Spawns a [`std::process::Command`] in this device's network namespace.
    ///
    /// The sync worker thread has `/etc/hosts` and `/etc/resolv.conf` bind-mounted.
    /// `fork()` inherits the mount namespace, so child processes automatically see
    /// the DNS overlay without a separate `pre_exec` hook.
    pub fn spawn_command_sync(&self, mut cmd: Command) -> Result<std::process::Child> {
        let ns = self.ns.to_string();
        self.lab.netns.run_closure_in(&ns, move || {
            cmd.spawn().context("spawn command in namespace")
        })
    }

    /// Probes the NAT mapping seen by a reflector from this device.
    ///
    /// Sends a UDP probe to `reflector` and returns the `ip:port` as seen by
    /// the reflector after NAT translation.
    ///
    /// The local bind port is deterministic based on the device's [`NodeId`].
    pub fn probe_udp_mapping(&self, reflector: SocketAddr) -> Result<SocketAddr> {
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
    /// Returns after the socket is confirmed bound. The returned
    /// [`ReflectorGuard`](core::ReflectorGuard) aborts the reflector
    /// task when dropped — callers must keep it alive for the reflector's
    /// lifetime.
    pub async fn spawn_reflector(&self, bind: SocketAddr) -> Result<core::ReflectorGuard> {
        self.lab.spawn_reflector_in(&self.ns, bind).await
    }

    /// Adds a hosts entry visible only to this device (via `/etc/hosts` overlay).
    ///
    /// glibc picks up changes on the next `getaddrinfo()` via mtime check.
    /// For lab-wide DNS records, use [`Lab::dns_server`] instead.
    pub fn set_host(&self, name: &str, ip: IpAddr) -> Result<()> {
        let inner = self.lab.core.lock().expect("poisoned");
        inner.dns.append_host(self.id, name, ip)
    }

    /// Resolves a name via this device's `/etc/hosts` + `resolv.conf` overlay,
    /// using `tokio::net::lookup_host` on the device's async worker.
    pub async fn resolve(&self, name: &str) -> Option<IpAddr> {
        let name = format!("{name}:0");
        self.spawn(move |_dev| async move {
            tokio::net::lookup_host(&name)
                .await
                .ok()
                .and_then(|mut addrs| addrs.next())
                .map(|a| a.ip())
        })
        .ok()?
        .await
        .ok()
        .flatten()
    }

    /// Adds a new interface to this device at runtime.
    ///
    /// Accepts anything that converts to [`IfaceConfig`](crate::IfaceConfig),
    /// including a bare [`NodeId`] for simple routed interfaces. Returns an
    /// [`Iface`](crate::Iface) handle to the new interface.
    ///
    /// The new interface does **not** become the default route unless you call
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
        config: impl Into<crate::IfaceConfig>,
    ) -> Result<crate::Iface> {
        use crate::wiring;

        let config = config.into();

        let op = self
            .lab
            .with_device(self.id, |d| Arc::clone(&d.op))
            .ok_or_else(|| anyhow!("device removed"))?;
        let _guard = op.lock().await;

        if let Some(router) = config.gateway {
            // Routed interface path.
            let mut setup = self.lab.core.lock().expect("poisoned").prepare_add_iface(
                self.id,
                ifname,
                router,
                config.egress,
            )?;
            if self.provisioning_mode()? == Ipv6ProvisioningMode::RaDriven {
                setup.iface_build.gw_ip_v6 = None;
            }
            setup.iface_build.ingress = config.ingress;
            setup.iface_build.start_down = config.start_down;
            // Override pool-allocated addresses with explicit ones.
            if let Some(addr) = config.addr {
                setup.iface_build.dev_ip = Some(addr.addr());
                setup.iface_build.prefix_len = addr.prefix_len();
            }
            if let Some(addr_v6) = config.addr_v6 {
                setup.iface_build.dev_ip_v6 = Some(addr_v6.addr());
                setup.iface_build.prefix_len_v6 = addr_v6.prefix_len();
            }

            let netns = &self.lab.netns;
            wiring::wire_iface_async(netns, &setup.prefix, &setup.root_ns, setup.iface_build)
                .await?;

            if let Some(mtu) = setup.mtu {
                let dev_ns = self.ns.to_string();
                let ifname_owned = ifname.to_string();
                wiring::nl_run(netns, &dev_ns, move |h: Netlink| async move {
                    h.set_mtu(&ifname_owned, mtu).await?;
                    Ok(())
                })
                .await?;
            }

            // Update stored state for fields not handled by prepare_add_iface.
            {
                let mut inner = self.lab.core.lock().expect("poisoned");
                if let Some(dev) = inner.device_mut(self.id) {
                    if let Some(iface) = dev.iface_mut(ifname) {
                        iface.ingress = config.ingress;
                        if let Some(addr) = config.addr {
                            iface.ip = Some(addr.addr());
                            iface.prefix_len = Some(addr.prefix_len());
                        }
                        if let Some(addr_v6) = config.addr_v6 {
                            iface.ip_v6 = Some(addr_v6.addr());
                            iface.prefix_len_v6 = Some(addr_v6.prefix_len());
                        }
                    }
                }
            }
        } else {
            // Dummy interface path.
            self.lab
                .core
                .lock()
                .expect("poisoned")
                .add_device_iface_from_config(self.id, ifname, config)?;

            let (dev_ns, prefix, root_ns) = {
                let inner = self.lab.core.lock().expect("poisoned");
                let dev = inner
                    .device(self.id)
                    .ok_or_else(|| anyhow!("device removed"))?;
                let iface = dev.iface(ifname).expect("just inserted");
                let prefix_len = iface.prefix_len.unwrap_or(24);
                let prefix_len_v6 = iface.prefix_len_v6.unwrap_or(64);
                let build = IfaceBuild {
                    dev_ns: dev.ns.clone(),
                    gw_ns: "".into(),
                    gw_ip: None,
                    gw_br: "".into(),
                    dev_ip: iface.ip,
                    prefix_len,
                    gw_ip_v6: None,
                    dev_ip_v6: iface.ip_v6,
                    gw_ll_v6: None,
                    dev_ll_v6: None,
                    prefix_len_v6,
                    egress: iface.egress,
                    ingress: None,
                    dummy: true,
                    start_down: config.start_down,
                    ifname: iface.ifname.clone(),
                    is_default: false,
                    idx: iface.idx,
                };
                let cfg_prefix = inner.cfg.prefix.clone();
                let cfg_root = inner.cfg.root_ns.clone();
                (build, cfg_prefix, cfg_root)
            };
            wiring::wire_iface_async(&self.lab.netns, &prefix, &root_ns, dev_ns).await?;
        }

        // Emit event.
        {
            let inner = self.lab.core.lock().expect("poisoned");
            let dev = inner.device(self.id);
            let iface_data = dev.and_then(|d| d.iface(ifname));
            let router_name = iface_data
                .and_then(|i| i.uplink())
                .and_then(|sw| inner.switch(sw))
                .and_then(|sw| sw.owner_router)
                .and_then(|rid| inner.router(rid))
                .map(|r| r.name.to_string())
                .unwrap_or_default();
            let iface_ip = iface_data.and_then(|i| i.ip);
            let iface_ip_v6 = iface_data.and_then(|i| i.ip_v6);
            let iface_ll_v6 = iface_data.and_then(|i| i.ll_v6);
            drop(inner);
            self.lab.emit(LabEventKind::InterfaceAdded {
                device: self.name.to_string(),
                iface: IfaceSnapshot {
                    name: ifname.to_string(),
                    router: router_name,
                    ip: iface_ip,
                    ip_v6: iface_ip_v6,
                    ll_v6: iface_ll_v6,
                    link_condition: config.egress,
                    ingress_condition: config.ingress,
                },
            });
        }

        Ok(crate::Iface::new(
            self.id,
            ifname.into(),
            Arc::clone(&self.lab),
        ))
    }
}

// ─────────────────────────────────────────────
// DeviceBuilder
// ─────────────────────────────────────────────

/// Builder for a device node; returned by [`Lab::add_device`].
pub struct DeviceBuilder {
    pub(crate) inner: Arc<LabInner>,
    pub(crate) lab_span: tracing::Span,
    pub(crate) id: NodeId,
    pub(crate) mtu: Option<u32>,
    pub(crate) provisioning_mode: Option<Ipv6ProvisioningMode>,
    pub(crate) result: Result<()>,
}

impl DeviceBuilder {
    /// Sets the MTU on all interfaces of this device.
    pub fn mtu(mut self, mtu: u32) -> Self {
        if self.result.is_ok() {
            self.mtu = Some(mtu);
        }
        self
    }

    /// Overrides IPv6 provisioning mode for this device only.
    pub fn ipv6_provisioning_mode(mut self, mode: Ipv6ProvisioningMode) -> Self {
        if self.result.is_ok() {
            self.provisioning_mode = Some(mode);
        }
        self
    }

    /// Adds a named interface with the given configuration.
    ///
    /// Accepts anything that converts to [`IfaceConfig`], including a bare
    /// [`NodeId`] for simple routed interfaces:
    ///
    /// ```ignore
    /// builder.iface("eth0", router.id())
    /// builder.iface("eth0", IfaceConfig::routed(router.id()).condition(cond, dir))
    /// builder.iface("tun0", IfaceConfig::dummy().addr("10.8.0.1/24".parse()?))
    /// ```
    pub fn iface(mut self, ifname: &str, config: impl Into<crate::IfaceConfig>) -> Self {
        if self.result.is_ok() {
            self.result = self
                .inner
                .core
                .lock()
                .expect("poisoned")
                .add_device_iface_from_config(self.id, ifname, config.into());
        }
        self
    }

    /// Adds an auto-named interface (eth0, eth1, ...) with the given config.
    ///
    /// Accepts anything that converts to [`IfaceConfig`], including a bare
    /// [`NodeId`].
    pub fn uplink(mut self, config: impl Into<crate::IfaceConfig>) -> Self {
        if self.result.is_ok() {
            let idx = {
                let inner = self.inner.core.lock().expect("poisoned");
                inner
                    .device(self.id)
                    .map(|d| d.interfaces.len())
                    .unwrap_or(0)
            };
            let ifname = format!("eth{}", idx);
            self.result = self
                .inner
                .core
                .lock()
                .expect("poisoned")
                .add_device_iface_from_config(self.id, &ifname, config.into());
        }
        self
    }

    /// Overrides which interface carries the default route.
    pub fn default_via(mut self, ifname: &str) -> Self {
        if self.result.is_ok() {
            self.result = self
                .inner
                .core
                .lock()
                .expect("poisoned")
                .set_device_default_via(self.id, ifname);
        }
        self
    }

    /// Finalizes the device, creates its namespace and links, and returns a [`Device`] handle.
    pub async fn build(self) -> Result<Device> {
        self.result?;

        // Phase 1: Lock → extract snapshot + DNS overlay → unlock.
        let (dev, ifaces, prefix, root_ns, dns_overlay, provisioning_mode) = {
            let mut inner = self.inner.core.lock().expect("poisoned");
            // Apply builder-level config before snapshot.
            if let Some(d) = inner.device_mut(self.id) {
                d.mtu = self.mtu;
                d.provisioning_mode = self.provisioning_mode;
            }
            let dev = inner
                .device(self.id)
                .ok_or_else(|| anyhow!("unknown device id"))?
                .clone();
            let provisioning_mode = dev
                .provisioning_mode
                .unwrap_or(self.inner.ipv6_provisioning_mode);

            let mut iface_data = Vec::new();
            for iface in &dev.interfaces {
                if iface.is_dummy() {
                    // Dummy interface: Linux dummy device, no gateway.
                    let prefix_len = iface.prefix_len.unwrap_or(24);
                    let prefix_len_v6 = iface.prefix_len_v6.unwrap_or(64);
                    iface_data.push(IfaceBuild {
                        dev_ns: dev.ns.clone(),
                        gw_ns: "".into(),
                        gw_ip: None,
                        gw_br: "".into(),
                        dev_ip: iface.ip,
                        prefix_len,
                        gw_ip_v6: None,
                        dev_ip_v6: iface.ip_v6,
                        gw_ll_v6: None,
                        dev_ll_v6: None,
                        prefix_len_v6,
                        egress: iface.egress,
                        ingress: None,
                        dummy: true,
                        start_down: iface.start_down,
                        ifname: iface.ifname.clone(),
                        is_default: iface.ifname == dev.default_via,
                        idx: iface.idx,
                    });
                } else {
                    // Routed interface: veth pair, gateway, pool allocation.
                    let uplink = iface.uplink().ok_or_else(|| {
                        anyhow!(
                            "device '{}' iface '{}' switch missing",
                            dev.name,
                            iface.ifname
                        )
                    })?;
                    let sw = inner.switch(uplink).ok_or_else(|| {
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
                    let gw_br = sw.bridge.clone().unwrap_or_else(|| "br-lan".into());
                    let gw_ns = inner.router(gw_router).unwrap().ns.clone();
                    let gw_ip_v6 = if provisioning_mode == Ipv6ProvisioningMode::RaDriven {
                        None
                    } else {
                        sw.gw_v6
                    };
                    let gw_ll_v6 = inner.router(gw_router).and_then(|r| {
                        if provisioning_mode == Ipv6ProvisioningMode::RaDriven {
                            r.active_downstream_ll_v6()
                        } else {
                            r.downstream_ll_v6
                        }
                    });
                    iface_data.push(IfaceBuild {
                        dev_ns: dev.ns.clone(),
                        gw_ns,
                        gw_ip: sw.gw,
                        gw_br,
                        dev_ip: iface.ip,
                        prefix_len: iface
                            .prefix_len
                            .unwrap_or_else(|| sw.cidr.map(|c| c.prefix_len()).unwrap_or(24)),
                        gw_ip_v6,
                        dev_ip_v6: iface.ip_v6,
                        gw_ll_v6,
                        dev_ll_v6: iface.ll_v6,
                        prefix_len_v6: iface
                            .prefix_len_v6
                            .unwrap_or_else(|| sw.cidr_v6.map(|c| c.prefix_len()).unwrap_or(64)),
                        egress: iface.egress,
                        ingress: iface.ingress,
                        dummy: false,
                        start_down: iface.start_down,
                        ifname: iface.ifname.clone(),
                        is_default: iface.ifname == dev.default_via,
                        idx: iface.idx,
                    });
                }
            }

            // Prepare DNS overlay: ensure the hosts file exists and build paths.
            inner.dns.ensure_hosts_file(self.id)?;
            let overlay = crate::netns::DnsOverlay {
                hosts_path: inner.dns.hosts_path_for(self.id),
                resolv_path: inner.dns.resolv_path(),
            };

            let prefix = inner.cfg.prefix.clone();
            let root_ns = inner.cfg.root_ns.clone();
            (dev, iface_data, prefix, root_ns, overlay, provisioning_mode)
        }; // lock released

        // Phase 2: Async network setup (no lock held).
        // The DNS overlay is passed to create_named_netns so worker threads
        // get /etc/hosts and /etc/resolv.conf bind-mounted at startup.
        let netns = &self.inner.netns;
        async {
            setup_device_async(
                netns,
                DeviceSetupData {
                    prefix,
                    root_ns,
                    dev: dev.clone(),
                    ifaces,
                    dns_overlay: Some(dns_overlay),
                    dad_mode: self.inner.ipv6_dad_mode,
                    provisioning_mode,
                },
            )
            .await
        }
        .instrument(self.lab_span.clone())
        .await?;

        // Emit DeviceAdded event.
        {
            let inner = self.inner.core.lock().expect("poisoned");
            let d = inner.device(self.id).expect("device just created");
            let device_state = DeviceState::from_device_data(d, &inner);

            self.inner.emit(LabEventKind::DeviceAdded {
                name: d.name.to_string(),
                state: device_state,
            });

            // Register ns → name mapping.
            self.inner
                .ns_to_name
                .lock()
                .expect("poisoned")
                .insert(d.ns.to_string(), d.name.to_string());
        }

        Ok(Device::new(
            self.id,
            dev.name.clone(),
            dev.ns.clone(),
            Arc::clone(&self.inner),
        ))
    }
}
