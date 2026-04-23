//! Router handle and builder.

use std::{
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    process::Command,
    sync::Arc,
    thread,
};

use anyhow::{anyhow, bail, Context, Result};
use ipnet::{Ipv4Net, Ipv6Net};
use tracing::{debug, Instrument as _};

use crate::{
    core::{
        self, DownstreamPool, NodeId, RA_DEFAULT_ENABLED, RA_DEFAULT_INTERVAL_SECS,
        RA_DEFAULT_LIFETIME_SECS,
    },
    device::record_metric,
    event::{LabEventKind, RouterState},
    firewall::{Firewall, FirewallConfigBuilder},
    lab::{Ipv6ProvisioningMode, LabInner, LinkCondition},
    nat::{IpSupport, Nat, NatV6Mode},
    netlink::Netlink,
    nft::{
        apply_firewall, apply_nat_for_router, apply_nat_v6, apply_or_remove_impair,
        remove_firewall, run_nft_in,
    },
    portmap::{self, PortmapConfig, PortmapMode},
    wiring::{self, setup_router_async, RouterSetupData},
};

async fn reconcile_radriven_default_v6_routes(
    lab: &Arc<LabInner>,
    router: NodeId,
    install_ll: Option<Ipv6Addr>,
) -> Result<()> {
    let targets = {
        let inner = lab.core.lock().unwrap();
        inner.router_default_v6_targets(router, lab.ipv6_provisioning_mode)?
    };
    for t in targets {
        let ifname = t.ifname.to_string();
        wiring::nl_run(&lab.netns, &t.ns, move |nl: Netlink| async move {
            nl.set_default_route_v6(&ifname, install_ll).await
        })
        .await?;
    }
    Ok(())
}

// ─────────────────────────────────────────────
// RouterIface
// ─────────────────────────────────────────────

/// Owned snapshot of a single router network interface.
#[derive(Clone, Debug)]
pub struct RouterIface {
    ifname: String,
    ip: Option<Ipv4Addr>,
    ip_v6: Option<Ipv6Addr>,
    ll_v6: Option<Ipv6Addr>,
}

impl RouterIface {
    /// Returns the interface name.
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

    /// Returns the assigned IPv6 link-local address, if any.
    pub fn ll6(&self) -> Option<Ipv6Addr> {
        self.ll_v6
    }
}

// ─────────────────────────────────────────────
// Router handle
// ─────────────────────────────────────────────

/// Cloneable handle to a router in the lab topology.
///
/// Same pattern as [`Device`](crate::Device): holds [`NodeId`] + `Arc<LabInner>`.
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
    dispatch: tracing::Dispatch,
}

impl Clone for Router {
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

    /// Enter this router's tracing context.
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

    /// Returns the router name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the network namespace name for this router.
    pub fn ns(&self) -> &str {
        &self.ns
    }

    /// Builds a path in the lab run directory for this router.
    ///
    /// Returns `None` when the lab was created without an output directory.
    /// The resulting filename is `router.{name}.{suffix}`.
    pub fn filepath(&self, suffix: &str) -> Option<PathBuf> {
        let run_dir = self.lab.run_dir.as_ref()?;
        let suffix = suffix.trim_start_matches('.');
        let filename = crate::consts::node_file(crate::consts::KIND_ROUTER, &self.name, suffix);
        Some(run_dir.join(filename))
    }

    /// Returns a snapshot of the named router interface, if it exists.
    pub fn iface(&self, name: &str) -> Option<RouterIface> {
        self.interfaces()
            .into_iter()
            .find(|iface| iface.name() == name)
    }

    /// Returns snapshots of all router-facing interfaces.
    ///
    /// Currently includes WAN (`ix` or `wan`) and downstream bridge interface.
    pub fn interfaces(&self) -> Vec<RouterIface> {
        let core = self.lab.core.lock().unwrap();
        let Some(router) = core.router(self.id) else {
            return vec![];
        };
        let mut out = Vec::new();
        let wan_if = router.wan_ifname(core.ix_sw());
        out.push(RouterIface {
            ifname: wan_if.to_string(),
            ip: router.upstream_ip,
            ip_v6: router.upstream_ip_v6,
            ll_v6: router.upstream_ll_v6,
        });
        out.push(RouterIface {
            ifname: router.downlink_bridge.to_string(),
            ip: router.downstream_gw,
            ip_v6: router.downstream_gw_v6,
            ll_v6: router.downstream_ll_v6,
        });
        out
    }

    /// Returns the region label, if set.
    ///
    /// Returns `None` if the router has been removed or no region is assigned.
    pub fn region(&self) -> Option<String> {
        self.lab
            .with_router(self.id, |r| r.region.as_ref().map(|s| s.to_string()))
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

    /// Returns whether RA emission is enabled for this router, if present.
    pub fn ra_enabled(&self) -> Option<bool> {
        self.lab.with_router(self.id, |r| r.cfg.ra_enabled)
    }

    /// Returns RA emission interval in seconds for this router, if present.
    pub fn ra_interval_secs(&self) -> Option<u64> {
        self.lab.with_router(self.id, |r| r.cfg.ra_interval_secs)
    }

    /// Returns RA lifetime in seconds for this router, if present.
    pub fn ra_lifetime_secs(&self) -> Option<u64> {
        self.lab.with_router(self.id, |r| r.cfg.ra_lifetime_secs)
    }

    // ── Dynamic operations ──────────────────────────────────────────────

    /// Updates the RA enabled flag at runtime.
    ///
    /// This affects subsequent RA-driven route refresh operations and any
    /// future device wiring. Existing RA worker task lifecycle is not changed.
    /// RA behavior in patchbay is currently modeled through structured events
    /// and route updates, not raw ICMPv6 control packets.
    pub async fn set_ra_enabled(&self, enabled: bool) -> Result<()> {
        let op = self
            .lab
            .with_router(self.id, |r| Arc::clone(&r.op))
            .ok_or_else(|| anyhow!("router removed"))?;
        let _guard = op.lock().await;
        let install_ll = {
            let mut inner = self.lab.core.lock().unwrap();
            let router = inner
                .router_mut(self.id)
                .ok_or_else(|| anyhow!("router removed"))?;
            router.cfg.ra_enabled = enabled;
            router.ra_runtime.set_enabled(enabled);
            let ll = router.downstream_ll_v6;
            if self.lab.ipv6_provisioning_mode == Ipv6ProvisioningMode::RaDriven
                && router.ra_default_enabled()
            {
                ll
            } else {
                None
            }
        };
        if self.lab.ipv6_provisioning_mode == Ipv6ProvisioningMode::RaDriven {
            reconcile_radriven_default_v6_routes(&self.lab, self.id, install_ll).await?;
        }
        Ok(())
    }

    /// Updates RA interval in seconds at runtime.
    ///
    /// Value is clamped to at least one second.
    /// Existing RA worker task lifecycle is not changed.
    /// This interval controls modeled RA event cadence.
    pub async fn set_ra_interval_secs(&self, secs: u64) -> Result<()> {
        let op = self
            .lab
            .with_router(self.id, |r| Arc::clone(&r.op))
            .ok_or_else(|| anyhow!("router removed"))?;
        let _guard = op.lock().await;
        let mut inner = self.lab.core.lock().unwrap();
        let router = inner
            .router_mut(self.id)
            .ok_or_else(|| anyhow!("router removed"))?;
        router.cfg.ra_interval_secs = secs.max(1);
        router.ra_runtime.set_interval_secs(secs);
        Ok(())
    }

    /// Updates RA lifetime in seconds at runtime.
    ///
    /// A value of `0` represents default-router withdrawal semantics.
    /// Existing RA worker task lifecycle is not changed.
    /// This affects modeled route withdrawal in RA-driven mode.
    pub async fn set_ra_lifetime_secs(&self, secs: u64) -> Result<()> {
        let op = self
            .lab
            .with_router(self.id, |r| Arc::clone(&r.op))
            .ok_or_else(|| anyhow!("router removed"))?;
        let _guard = op.lock().await;
        let install_ll = {
            let mut inner = self.lab.core.lock().unwrap();
            let router = inner
                .router_mut(self.id)
                .ok_or_else(|| anyhow!("router removed"))?;
            router.cfg.ra_lifetime_secs = secs;
            router.ra_runtime.set_lifetime_secs(secs);
            let ll = router.downstream_ll_v6;
            if self.lab.ipv6_provisioning_mode == Ipv6ProvisioningMode::RaDriven
                && router.ra_default_enabled()
            {
                ll
            } else {
                None
            }
        };
        if self.lab.ipv6_provisioning_mode == Ipv6ProvisioningMode::RaDriven {
            reconcile_radriven_default_v6_routes(&self.lab, self.id, install_ll).await?;
        }
        Ok(())
    }

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
        let (nat_params, cfg) = {
            let mut inner = self.lab.core.lock().unwrap();
            inner.set_router_nat_mode(self.id, mode)?;
            let cfg = inner.router_effective_cfg(self.id)?;
            let nat_params = inner.router_nat_params(self.id)?;
            (nat_params, cfg)
        };
        run_nft_in(&self.lab.netns, &nat_params.ns, "flush table ip nat")
            .await
            .ok();
        run_nft_in(&self.lab.netns, &nat_params.ns, "flush table ip filter")
            .await
            .ok();
        apply_nat_for_router(
            &self.lab.netns,
            &nat_params.ns,
            &cfg,
            &nat_params.wan_if,
            nat_params.upstream_ip,
        )
        .await?;

        self.lab.emit(LabEventKind::NatChanged {
            router: self.name.to_string(),
            nat: mode,
        });
        Ok(())
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
        let p = self
            .lab
            .core
            .lock()
            .unwrap()
            .router_nat_v6_params(self.id)?;
        run_nft_in(&self.lab.netns, &p.ns, "flush table ip6 nat")
            .await
            .ok();
        apply_nat_v6(
            &self.lab.netns,
            &p.ns,
            mode,
            &p.wan_if,
            p.lan_prefix,
            p.wan_prefix,
        )
        .await?;
        self.lab
            .core
            .lock()
            .unwrap()
            .set_router_nat_v6_mode(self.id, mode)?;

        self.lab.emit(LabEventKind::NatV6Changed {
            router: self.name.to_string(),
            nat_v6: mode,
        });
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
                .await
                .context("spawn conntrack -F")?;
            if !st.success() {
                bail!("conntrack -F failed: {st}");
            }
            Ok(())
        })
        .await
        .context("conntrack flush task panicked")??;

        self.lab.emit(LabEventKind::NatStateFlushed {
            router: self.name.to_string(),
        });
        Ok(())
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

    /// Spawns a [`tokio::process::Command`] in this router's network namespace.
    ///
    /// The child is registered with the namespace's tokio reactor.
    pub fn spawn_command(&self, mut cmd: tokio::process::Command) -> Result<tokio::process::Child> {
        let ns = self.ns.to_string();
        let rt = self.lab.rt_handle_for(&ns)?;
        self.lab.netns.run_closure_in(&ns, move || {
            let _guard = rt.enter();
            cmd.spawn().context("spawn async command in namespace")
        })
    }

    /// Spawns a [`std::process::Command`] in this router's network namespace.
    ///
    /// The child inherits the namespace's DNS bind-mounts.
    pub fn spawn_command_sync(&self, mut cmd: Command) -> Result<std::process::Child> {
        let ns = self.ns.to_string();
        self.lab.netns.run_closure_in(&ns, move || {
            cmd.spawn().context("spawn command in namespace")
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
        self.lab.emit(LabEventKind::DownlinkConditionChanged {
            router: self.name.to_string(),
            condition: impair,
        });
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
        let wan_if: Arc<str> = {
            let mut inner = self.lab.core.lock().unwrap();
            inner.set_router_firewall(self.id, fw.clone())?;
            let ix_sw = inner.ix_sw();
            inner
                .router(self.id)
                .map(|r| Arc::<str>::from(r.wan_ifname(ix_sw)))
                .ok_or_else(|| anyhow!("router removed"))?
        };
        let ns = self.ns.to_string();
        // Always remove existing rules first, then apply new ones.
        remove_firewall(&self.lab.netns, &ns).await?;
        apply_firewall(&self.lab.netns, &ns, &fw, &wan_if).await?;
        self.lab.emit(LabEventKind::FirewallChanged {
            router: self.name.to_string(),
            firewall: fw,
        });
        Ok(())
    }

    /// Spawns a STUN-like UDP reflector in this router's network namespace.
    ///
    /// See [`Device::spawn_reflector`](crate::Device::spawn_reflector) for details.
    pub async fn spawn_reflector(&self, bind: SocketAddr) -> Result<core::ReflectorGuard> {
        self.lab.spawn_reflector_in(&self.ns, bind).await
    }

    /// Enables, disables, or reconfigures the portmap server at runtime.
    ///
    /// Dropping to [`PortmapMode::None`] tears the server down and flushes
    /// the `ip portmap` nftables table. Switching between non-`None` modes
    /// replaces the server, which forgets every active mapping: callers
    /// should expect clients to reprobe. Passing the same mode the router
    /// is already running is a no-op.
    ///
    /// # Errors
    ///
    /// Returns an error if the router has been removed, the namespace
    /// runtime is gone, or starting the server inside the namespace
    /// fails.
    pub async fn set_portmap(&self, mode: PortmapMode) -> Result<()> {
        let op = self
            .lab
            .with_router(self.id, |r| Arc::clone(&r.op))
            .ok_or_else(|| anyhow!("router removed"))?;
        let _guard = op.lock().await;
        let new_cfg = PortmapConfig::from_mode(mode);

        let (existing, ns, downstream_gw, wan_ip, downstream_cidr) = {
            let inner = self.lab.core.lock().unwrap();
            let r = inner
                .router(self.id)
                .ok_or_else(|| anyhow!("router removed"))?;
            (
                r.cfg.portmap,
                r.ns.clone(),
                r.downstream_gw,
                r.upstream_ip,
                r.downstream_cidr,
            )
        };

        if existing == new_cfg {
            return Ok(());
        }

        // Tear down the current server before potentially starting a new
        // one. Shutdown removes lingering nftables state; dropping the
        // handle aborts its tasks.
        let existing_server = {
            let mut inner = self.lab.core.lock().unwrap();
            let taken = inner.router_mut(self.id).and_then(|r| {
                r.cfg.portmap = new_cfg;
                r.portmap_server.take()
            });
            taken
        };
        if let Some(server) = existing_server {
            server.shutdown().await.ok();
            drop(server);
        } else {
            portmap::nft::clear_portmap_rules(&self.lab.netns, &ns)
                .await
                .ok();
        }

        if new_cfg.any_enabled() {
            let downstream_gw = downstream_gw
                .ok_or_else(|| anyhow!("portmap requires an IPv4 downstream gateway"))?;
            let wan_ip = wan_ip.ok_or_else(|| anyhow!("portmap requires an IPv4 uplink IP"))?;
            let downstream_cidr = downstream_cidr
                .ok_or_else(|| anyhow!("portmap requires an IPv4 downstream CIDR"))?;
            let server = portmap::server::PortmapServer::start(
                Arc::clone(&self.lab.netns),
                ns,
                new_cfg,
                downstream_gw,
                wan_ip,
                downstream_cidr,
            )
            .await?;
            if let Some(r) = self.lab.core.lock().unwrap().router_mut(self.id) {
                r.portmap_server = Some(server);
            }
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────
// RouterPreset
// ─────────────────────────────────────────────

/// Pre-built router configurations that match common real-world deployments.
///
/// Each preset configures NAT mode, firewall policy, IP address family, and
/// downstream address pool as a single unit. Methods called after `.preset()`
/// on the [`RouterBuilder`] override the preset's defaults, so you can start
/// from a known configuration and adjust only what your test needs.
///
/// The ISP presets (`IspCgnat`, `IspV6`) cover both fixed-line and mobile
/// carriers. Most mobile networks (T-Mobile, Vodafone, AT&T) use the same
/// CGNAT or NAT64 infrastructure as their fixed-line counterparts, and
/// real-world measurements confirm that hole-punching succeeds on the
/// majority of them.
///
/// # Example
///
/// ```ignore
/// let home = lab.add_router("home")
///     .preset(RouterPreset::Home)
///     .build().await?;
///
/// // Override NAT while keeping the rest of the Home preset:
/// let home = lab.add_router("home")
///     .preset(RouterPreset::Home)
///     .nat(Nat::FullCone)
///     .build().await?;
/// ```
#[derive(Clone, Copy, Debug)]
pub enum RouterPreset {
    /// Residential home router.
    ///
    /// Models the standard consumer setup: a FritzBox, UniFi, TP-Link, or
    /// similar device where every LAN host gets an RFC 1918 IPv4 address
    /// behind NAT and a ULA IPv6 address behind a stateful firewall. The
    /// NAT is endpoint-independent mapping with address-and-port-dependent
    /// filtering (EIM+APDF), which preserves the external port and allows
    /// UDP hole-punching. The firewall blocks unsolicited inbound
    /// connections on both address families (RFC 6092 CE router behavior).
    ///
    /// Dual-stack, private downstream pool.
    Home,

    /// Public-IP router with no NAT or firewall.
    ///
    /// Downstream devices receive globally routable addresses on both
    /// address families. Use this for datacenter switches, ISP handoff
    /// points, VPS hosts, and any topology where devices need direct
    /// reachability without translation or filtering.
    ///
    /// Dual-stack, public downstream pool.
    Public,

    /// IPv4-only variant of [`Public`](Self::Public).
    ///
    /// Same behavior — no NAT, no firewall, public downstream — but
    /// without IPv6. Models legacy ISPs and v4-only VPS providers.
    ///
    /// V4-only, public downstream pool.
    PublicV4,

    /// ISP or mobile carrier with carrier-grade NAT.
    ///
    /// Models any provider that shares a pool of public IPv4 addresses
    /// across subscribers via CGNAT: budget fiber, fixed-wireless,
    /// satellite (Starlink), and dual-stack mobile carriers (Vodafone, O2,
    /// AT&T). The CGNAT uses endpoint-independent mapping and filtering
    /// per RFC 6888, so hole-punching works — inbound packets reach
    /// mapped ports. No additional firewall beyond the NAT. IPv6 addresses
    /// are globally routable.
    ///
    /// Dual-stack, private downstream pool.
    IspCgnat,

    /// IPv6-only ISP or mobile carrier with NAT64.
    ///
    /// Models T-Mobile US, Jio, NTT Docomo, and other providers that run
    /// pure IPv6 networks. The device has no IPv4 address. A userspace
    /// SIIT translator on the router converts between IPv6 and IPv4 via
    /// the well-known prefix `64:ff9b::/96`, and nftables masquerade
    /// handles port mapping on the IPv4 side. A `BlockInbound` firewall
    /// prevents unsolicited connections.
    ///
    /// V6-only, public downstream pool.
    IspV6,

    /// Enterprise gateway with restrictive outbound filtering.
    ///
    /// Models a Cisco ASA, Palo Alto, or Fortinet appliance. Symmetric NAT
    /// (endpoint-dependent mapping) makes STUN useless — the external port
    /// changes with every new destination, so the reflexive address learned
    /// from a STUN server does not work for other peers. The `Corporate`
    /// firewall restricts outbound traffic to TCP 80/443 and UDP 53,
    /// blocking all other UDP. Applications behind this preset must fall
    /// back to TURN-over-TLS on port 443.
    ///
    /// Dual-stack, private downstream pool.
    Corporate,

    /// Hotel, airport, or conference guest WiFi.
    ///
    /// Symmetric NAT with a `CaptivePortal` firewall that allows TCP on
    /// any port but blocks all non-DNS UDP. This kills QUIC and prevents
    /// direct P2P, but TURN-over-TCP on non-standard ports can still work
    /// — unlike the stricter `Corporate` preset. IPv4-only, because most
    /// guest networks still do not offer IPv6.
    ///
    /// V4-only, private downstream pool.
    Hotel,

    /// Cloud NAT gateway.
    ///
    /// Models AWS NAT Gateway, Azure NAT Gateway, and GCP Cloud NAT. VPC
    /// instances get private addresses, and the NAT gateway handles
    /// public-facing translation with symmetric mapping. Timeouts are
    /// longer than residential NAT (350 seconds for UDP) to accommodate
    /// long-lived cloud workloads. No firewall — security groups and
    /// NACLs are a separate concern in cloud environments.
    ///
    /// Dual-stack, private downstream pool.
    Cloud,
}

impl RouterPreset {
    fn nat(self) -> Nat {
        match self {
            Self::Home => Nat::Home,
            Self::Public | Self::PublicV4 | Self::IspV6 => Nat::None,
            Self::IspCgnat => Nat::Cgnat,
            Self::Corporate | Self::Hotel => Nat::Corporate,
            Self::Cloud => Nat::CloudNat,
        }
    }

    fn nat_v6(self) -> NatV6Mode {
        match self {
            Self::IspV6 => NatV6Mode::Nat64,
            _ => NatV6Mode::None,
        }
    }

    fn firewall(self) -> Firewall {
        match self {
            Self::Home | Self::IspV6 => Firewall::BlockInbound,
            Self::Public | Self::PublicV4 | Self::IspCgnat | Self::Cloud => Firewall::None,
            Self::Corporate => Firewall::Corporate,
            Self::Hotel => Firewall::CaptivePortal,
        }
    }

    fn ip_support(self) -> IpSupport {
        match self {
            Self::PublicV4 | Self::Hotel => IpSupport::V4Only,
            Self::IspV6 => IpSupport::V6Only,
            _ => IpSupport::DualStack,
        }
    }

    fn downstream_pool(self) -> DownstreamPool {
        match self {
            Self::Public | Self::PublicV4 | Self::IspV6 => DownstreamPool::Public,
            _ => DownstreamPool::Private,
        }
    }

    /// Returns the recommended IPv6 profile for this preset.
    ///
    /// All presets return [`Ipv6Profile::Realistic`](crate::lab::Ipv6Profile). Use
    /// [`LabBuilder::ipv6_profile`](crate::LabBuilder::ipv6_profile) with [`Ipv6Profile::Deterministic`](crate::lab::Ipv6Profile::Deterministic) to
    /// override for fast, reproducible tests.
    pub fn recommended_ipv6_profile(self) -> crate::lab::Ipv6Profile {
        crate::lab::Ipv6Profile::Realistic
    }
}

// ─────────────────────────────────────────────
// RouterBuilder
// ─────────────────────────────────────────────

/// Builder for a router node; returned by [`Lab::add_router`].
pub struct RouterBuilder {
    pub(crate) inner: Arc<LabInner>,
    pub(crate) lab_span: tracing::Span,
    pub(crate) name: String,
    pub(crate) region: Option<Arc<str>>,
    pub(crate) upstream: Option<NodeId>,
    pub(crate) nat: Nat,
    pub(crate) ip_support: IpSupport,
    pub(crate) nat_v6: NatV6Mode,
    pub(crate) downstream_pool: Option<DownstreamPool>,
    pub(crate) downstream_cidr: Option<Ipv4Net>,
    pub(crate) downlink_condition: Option<LinkCondition>,
    pub(crate) mtu: Option<u32>,
    pub(crate) block_icmp_frag_needed: bool,
    pub(crate) firewall: Firewall,
    pub(crate) ra_enabled: bool,
    pub(crate) ra_interval_secs: u64,
    pub(crate) ra_lifetime_secs: u64,
    pub(crate) portmap: PortmapConfig,
    pub(crate) result: Result<()>,
}

impl RouterBuilder {
    /// Creates a builder in an error state; `build()` will return this error.
    pub(crate) fn error(
        inner: Arc<LabInner>,
        lab_span: tracing::Span,
        name: &str,
        err: anyhow::Error,
    ) -> Self {
        Self {
            inner,
            lab_span,
            name: name.to_string(),
            region: None,
            upstream: None,
            nat: Nat::None,
            ip_support: IpSupport::V4Only,
            nat_v6: NatV6Mode::None,
            downstream_pool: None,
            downstream_cidr: None,
            downlink_condition: None,
            mtu: None,
            block_icmp_frag_needed: false,
            firewall: Firewall::None,
            ra_enabled: RA_DEFAULT_ENABLED,
            ra_interval_secs: RA_DEFAULT_INTERVAL_SECS,
            ra_lifetime_secs: RA_DEFAULT_LIFETIME_SECS,
            portmap: PortmapConfig::default(),
            result: Err(err),
        }
    }

    /// Places this router in a region, connecting it to the region's bridge.
    ///
    /// The router becomes a sub-router of the region router. For `Nat::None`
    /// routers, a return route is added in the region router's namespace.
    pub fn region(mut self, region: &crate::lab::Region) -> Self {
        if self.result.is_ok() {
            self.region = Some(region.name.clone());
            self.upstream = Some(region.router_id);
        }
        self
    }

    /// Connects this router as a sub-router behind `parent`'s downstream switch.
    ///
    /// Without this, the router attaches directly to the IX switch.
    pub fn upstream(mut self, parent: NodeId) -> Self {
        if self.result.is_ok() {
            self.upstream = Some(parent);
        }
        self
    }

    /// Applies a [`RouterPreset`] that sets NAT, firewall, IP support, and
    /// address pool to match a real-world deployment pattern.
    ///
    /// Individual methods (`.nat()`, `.firewall()`, etc.) called **after**
    /// `preset()` override the preset's values.
    ///
    /// # Example
    /// ```ignore
    /// // Home router with full-cone NAT instead of default port-restricted:
    /// lab.add_router("home")
    ///     .preset(RouterPreset::Home)
    ///     .nat(Nat::FullCone)
    ///     .build().await?;
    /// ```
    pub fn preset(mut self, p: RouterPreset) -> Self {
        if self.result.is_ok() {
            self.nat = p.nat();
            self.nat_v6 = p.nat_v6();
            self.firewall = p.firewall();
            self.ip_support = p.ip_support();
            self.downstream_pool = Some(p.downstream_pool());
        }
        self
    }

    /// Sets the NAT mode. Defaults to [`Nat::None`] (no NAT, public addressing).
    pub fn nat(mut self, mode: Nat) -> Self {
        if self.result.is_ok() {
            self.nat = mode;
        }
        self
    }

    /// Sets an impairment condition on this router's downlink bridge, affecting
    /// download-direction traffic to all downstream devices.
    ///
    /// Equivalent to calling [`Router::set_downlink_condition`] after build.
    pub fn downlink_condition(mut self, condition: LinkCondition) -> Self {
        if self.result.is_ok() {
            self.downlink_condition = Some(condition);
        }
        self
    }

    /// Sets the MTU on this router's WAN and LAN bridge interfaces.
    ///
    /// Useful for simulating VPN tunnels (e.g. 1420 for WireGuard) or
    /// constrained paths.
    pub fn mtu(mut self, mtu: u32) -> Self {
        if self.result.is_ok() {
            self.mtu = Some(mtu);
        }
        self
    }

    /// Blocks ICMP "fragmentation needed" (type 3, code 4) in the forward chain.
    ///
    /// Simulates a PMTU blackhole middlebox — devices behind this router
    /// will not receive path MTU discovery feedback.
    pub fn block_icmp_frag_needed(mut self) -> Self {
        if self.result.is_ok() {
            self.block_icmp_frag_needed = true;
        }
        self
    }

    /// Sets a firewall preset for this router.
    pub fn firewall(mut self, fw: Firewall) -> Self {
        if self.result.is_ok() {
            self.firewall = fw;
        }
        self
    }

    /// Configures a custom firewall via a builder closure.
    ///
    /// # Example
    /// ```ignore
    /// lab.add_router("fw")
    ///     .firewall_custom(|f| f.allow_tcp(&[80, 443]).allow_udp(&[53]).block_udp())
    ///     .build().await?;
    /// ```
    pub fn firewall_custom(
        mut self,
        f: impl FnOnce(&mut FirewallConfigBuilder) -> &mut FirewallConfigBuilder,
    ) -> Self {
        if self.result.is_ok() {
            let mut builder = FirewallConfigBuilder::default();
            f(&mut builder);
            self.firewall = Firewall::Custom(builder.build());
        }
        self
    }

    /// Sets which IP address families this router supports. Defaults to [`IpSupport::V4Only`].
    pub fn ip_support(mut self, support: IpSupport) -> Self {
        if self.result.is_ok() {
            self.ip_support = support;
        }
        self
    }

    /// Sets the IPv6 NAT mode. Defaults to [`NatV6Mode::None`].
    pub fn nat_v6(mut self, mode: NatV6Mode) -> Self {
        if self.result.is_ok() {
            self.nat_v6 = mode;
        }
        self
    }

    /// Enables or disables router advertisement emission in RA-driven mode.
    ///
    /// In the current implementation, this controls structured RA events and
    /// default-route behavior, not raw ICMPv6 packet emission.
    pub fn ra_enabled(mut self, enabled: bool) -> Self {
        if self.result.is_ok() {
            self.ra_enabled = enabled;
        }
        self
    }

    /// Sets the RA interval in seconds, clamped to at least 1 second.
    ///
    /// This interval drives patchbay's RA event cadence in RA-driven mode.
    pub fn ra_interval_secs(mut self, secs: u64) -> Self {
        if self.result.is_ok() {
            self.ra_interval_secs = secs.max(1);
        }
        self
    }

    /// Sets Router Advertisement lifetime in seconds.
    ///
    /// A value of `0` advertises default-router withdrawal semantics in
    /// patchbay's RA-driven route model.
    pub fn ra_lifetime_secs(mut self, secs: u64) -> Self {
        if self.result.is_ok() {
            self.ra_lifetime_secs = secs;
        }
        self
    }

    /// Overrides the downstream subnet instead of auto-allocating from the pool.
    ///
    /// The gateway address is the `.1` host of the given CIDR. Device addresses
    /// are allocated sequentially starting at `.2`.
    pub fn downstream_cidr(mut self, cidr: Ipv4Net) -> Self {
        if self.result.is_ok() {
            self.downstream_cidr = Some(cidr);
        }
        self
    }

    /// Enables the port mapping server on this router.
    ///
    /// Off by default on every [`RouterPreset`]. Passing
    /// [`PortmapMode::All`] advertises NAT-PMP, PCP, and UPnP IGD;
    /// narrower modes select individual protocols. Clients inside the
    /// router's downstream subnet can then obtain port mappings through
    /// the `portmapper` crate or any RFC 6886 / 6887 / UPnP IGD:1 client.
    ///
    /// See [`Router::set_portmap`] for runtime reconfiguration.
    pub fn portmap(mut self, mode: PortmapMode) -> Self {
        if self.result.is_ok() {
            self.portmap = PortmapConfig::from_mode(mode);
        }
        self
    }

    /// Finalizes the router, creates its namespace and links, and returns a [`Router`] handle.
    pub async fn build(self) -> Result<Router> {
        self.result?;

        // Phase 1: Lock → register topology + extract snapshot → unlock.
        let (id, setup_data) = {
            let mut inner = self.inner.core.lock().unwrap();
            let nat = self.nat;
            let downstream_pool = self.downstream_pool.unwrap_or(if nat == Nat::None {
                DownstreamPool::Public
            } else {
                DownstreamPool::Private
            });
            let id = inner.add_router(
                &self.name,
                nat,
                downstream_pool,
                self.region,
                self.ip_support,
                self.nat_v6,
            );
            // Apply builder-level config to the registered RouterData.
            if let Some(r) = inner.router_mut(id) {
                r.cfg.mtu = self.mtu;
                r.cfg.block_icmp_frag_needed = self.block_icmp_frag_needed;
                r.cfg.firewall = self.firewall.clone();
                r.cfg.ra_enabled = self.ra_enabled;
                r.cfg.ra_interval_secs = self.ra_interval_secs.max(1);
                r.cfg.ra_lifetime_secs = self.ra_lifetime_secs;
                r.cfg.portmap = self.portmap;
                r.ra_runtime.set_enabled(self.ra_enabled);
                r.ra_runtime.set_interval_secs(self.ra_interval_secs);
                r.ra_runtime.set_lifetime_secs(self.ra_lifetime_secs);
            }
            let has_v4 = self.ip_support.has_v4();
            let has_v6 = self.ip_support.has_v6();
            // NAT64 needs v4 on the uplink even when IpSupport::V6Only,
            // but downstream devices stay v6-only (no v4 CIDR for them).
            let uplink_needs_v4 = has_v4 || self.nat_v6 == NatV6Mode::Nat64;
            let sub_switch =
                inner.add_switch(&format!("{}-sub", self.name), None, None, None, None);
            // For Nat::None sub-routers in a region, allocate downstream /24
            // from the region's pool instead of the global pool.
            let downstream_cidr = if self.downstream_cidr.is_some() {
                self.downstream_cidr
            } else if downstream_pool == DownstreamPool::Public {
                if let Some(region_name) = inner.router(id).and_then(|r| r.region.clone()) {
                    if inner.regions.contains_key(&region_name) {
                        Some(inner.alloc_region_public_cidr(&region_name)?)
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };
            inner.connect_router_downlink(id, sub_switch, downstream_cidr)?;
            match self.upstream {
                None => {
                    let ix_ip = if uplink_needs_v4 {
                        Some(inner.alloc_ix_ip_low()?)
                    } else {
                        None
                    };
                    let ix_ip_v6 = if has_v6 {
                        Some(inner.alloc_ix_ip_v6_low()?)
                    } else {
                        None
                    };
                    let ix_sw = inner.ix_sw();
                    inner.connect_router_uplink(id, ix_sw, ix_ip, ix_ip_v6)?;
                }
                Some(parent_id) => {
                    let parent_downlink = inner
                        .router(parent_id)
                        .and_then(|r| r.downlink)
                        .ok_or_else(|| anyhow!("parent router missing downlink switch"))?;
                    let uplink_ip_v4 = if uplink_needs_v4 {
                        Some(inner.alloc_from_switch(parent_downlink)?)
                    } else {
                        None
                    };
                    let uplink_ip_v6 = if has_v6 {
                        Some(inner.alloc_from_switch_v6(parent_downlink)?)
                    } else {
                        None
                    };
                    inner.connect_router_uplink(id, parent_downlink, uplink_ip_v4, uplink_ip_v6)?;
                }
            }

            // Extract snapshot for async setup.
            let router = inner.router(id).unwrap().clone();
            let cfg = &inner.cfg;
            let ix_sw = inner.ix_sw();

            // Upstream info for sub-routers.
            let (
                upstream_owner_ns,
                upstream_bridge,
                upstream_gw,
                upstream_cidr_prefix,
                upstream_gw_v6,
                upstream_cidr_prefix_v6,
            ) = if let Some(uplink) = router.uplink {
                if uplink != ix_sw {
                    let sw = inner.switch(uplink).unwrap();
                    let owner = sw.owner_router.unwrap();
                    let owner_ns = inner.router(owner).unwrap().ns.clone();
                    let bridge = sw.bridge.clone().unwrap_or_else(|| "br-lan".into());
                    let gw = sw.gw;
                    let prefix = sw.cidr.map(|c| c.prefix_len());
                    let gw_v6 = sw.gw_v6;
                    let prefix_v6 = sw.cidr_v6.map(|c| c.prefix_len());
                    (Some(owner_ns), Some(bridge), gw, prefix, gw_v6, prefix_v6)
                } else {
                    (None, None, None, None, None, None)
                }
            } else {
                (None, None, None, None, None, None)
            };

            // Downlink bridge info.
            let downlink_bridge = router.downlink.and_then(|sw_id| {
                let sw = inner.switch(sw_id)?;
                let br = sw.bridge.clone().unwrap_or_else(|| "br-lan".into());
                let v4 = sw.gw.and_then(|gw| Some((gw, sw.cidr?.prefix_len())));
                Some((br, v4))
            });
            let downlink_bridge_v6 = router.downlink.and_then(|sw_id| {
                let sw = inner.switch(sw_id)?;
                Some((sw.gw_v6?, sw.cidr_v6?.prefix_len()))
            });

            // Return route for public downstreams.
            let return_route = if router.uplink == Some(ix_sw)
                && router.cfg.downstream_pool == DownstreamPool::Public
            {
                if let (Some(cidr), Some(via)) = (router.downstream_cidr, router.upstream_ip) {
                    Some((cidr.addr(), cidr.prefix_len(), via))
                } else {
                    None
                }
            } else {
                None
            };
            let mut return_route_v6 = if router.uplink == Some(ix_sw) {
                // IX-level router: return route via this router's IX IP.
                if let (Some(cidr6), Some(via6)) =
                    (router.downstream_cidr_v6, router.upstream_ip_v6)
                {
                    Some((cidr6.addr(), cidr6.prefix_len(), via6))
                } else {
                    None
                }
            } else {
                None
            };

            // For sub-routers with NatV6Mode::None: add routes so that return
            // traffic for the sub-router's ULA subnet can reach it.
            let parent_route_v6 = if let Some(uplink_sw) = router
                .uplink
                .filter(|&u| u != ix_sw && router.cfg.nat_v6 == NatV6Mode::None)
            {
                let parent_id = inner.switch(uplink_sw).and_then(|sw| sw.owner_router);
                // Route in the parent router's ns: sub-router's LAN via sub-router's WAN IP.
                let parent_rt = if let (Some(cidr6), Some(via6), Some(ref owner_ns)) = (
                    router.downstream_cidr_v6,
                    router.upstream_ip_v6,
                    &upstream_owner_ns,
                ) {
                    Some((owner_ns.clone(), cidr6.addr(), cidr6.prefix_len(), via6))
                } else {
                    None
                };
                // Also need a root-ns route via the IX-level ancestor's IX IP.
                if parent_rt.is_some() {
                    if let Some(pid) = parent_id {
                        if let Some(parent_router) = inner.router(pid) {
                            if parent_router.uplink == Some(ix_sw) {
                                // Parent is IX-level; use its IX IP as the root-ns next-hop.
                                if let Some(parent_ix_v6) = parent_router.upstream_ip_v6 {
                                    if let Some(cidr6) = router.downstream_cidr_v6 {
                                        // Overwrite return_route_v6 for root ns
                                        return_route_v6 =
                                            Some((cidr6.addr(), cidr6.prefix_len(), parent_ix_v6));
                                    }
                                }
                            }
                        }
                    }
                }
                parent_rt
            } else {
                None
            };

            // For sub-routers with public downstream: add return route in
            // parent router's NS (e.g. region router) so return traffic can
            // reach this sub-router's downstream /24.
            let parent_route_v4 = if router.uplink.is_some()
                && router.uplink != Some(ix_sw)
                && router.cfg.downstream_pool == DownstreamPool::Public
            {
                if let (Some(cidr), Some(via), Some(ref owner_ns)) = (
                    router.downstream_cidr,
                    router.upstream_ip,
                    &upstream_owner_ns,
                ) {
                    Some((owner_ns.clone(), cidr.addr(), cidr.prefix_len(), via))
                } else {
                    None
                }
            } else {
                None
            };

            let has_v6 = router.cfg.ip_support.has_v6();
            let ra_enabled = router.cfg.ra_enabled;
            let setup_data = RouterSetupData {
                router,
                root_ns: cfg.root_ns.clone(),
                prefix: cfg.prefix.clone(),
                ix_sw,
                ix_br: cfg.ix_br.clone(),
                ix_gw: cfg.ix_gw,
                ix_cidr_prefix: cfg.ix_cidr.prefix_len(),
                upstream_owner_ns,
                upstream_bridge,
                upstream_gw,
                upstream_cidr_prefix,
                return_route,
                downlink_bridge,
                ix_gw_v6: if has_v6 { Some(cfg.ix_gw_v6) } else { None },
                ix_cidr_v6_prefix: if has_v6 {
                    Some(cfg.ix_cidr_v6.prefix_len())
                } else {
                    None
                },
                upstream_gw_v6,
                upstream_cidr_prefix_v6,
                return_route_v6,
                downlink_bridge_v6,
                parent_route_v6,
                parent_route_v4,
                cancel: self.inner.cancel.clone(),
                dad_mode: self.inner.ipv6_dad_mode,
                provisioning_mode: self.inner.ipv6_provisioning_mode,
                ra_enabled,
            };

            (id, setup_data)
        }; // lock released

        // Phase 2: Async network setup (no lock held).
        let netns = &self.inner.netns;
        async { setup_router_async(netns, &setup_data).await }
            .instrument(self.lab_span.clone())
            .await?;

        // Phase 2b: Start the portmap server if any protocol is enabled.
        // Needs the downstream gateway, downstream CIDR, and upstream IP,
        // all of which are known only after setup_router_async assigns
        // addresses. Captures of those fields are made under the core lock
        // in the next block so they race-free with any concurrent
        // reconfiguration.
        if self.portmap.any_enabled() {
            let (ns, downstream_gw, wan_ip, downstream_cidr) = {
                let inner = self.inner.core.lock().unwrap();
                let r = inner.router(id).ok_or_else(|| anyhow!("router removed"))?;
                let gw = r
                    .downstream_gw
                    .ok_or_else(|| anyhow!("portmap requires an IPv4 downstream gateway"))?;
                let ip = r
                    .upstream_ip
                    .ok_or_else(|| anyhow!("portmap requires an IPv4 uplink IP"))?;
                let cidr = r
                    .downstream_cidr
                    .ok_or_else(|| anyhow!("portmap requires an IPv4 downstream CIDR"))?;
                (r.ns.clone(), gw, ip, cidr)
            };
            let server = portmap::server::PortmapServer::start(
                Arc::clone(&self.inner.netns),
                ns,
                self.portmap,
                downstream_gw,
                wan_ip,
                downstream_cidr,
            )
            .await?;
            if let Some(r) = self.inner.core.lock().unwrap().router_mut(id) {
                r.portmap_server = Some(server);
            }
        }

        let router = {
            let inner = self.inner.core.lock().unwrap();
            let r = inner.router(id).unwrap();
            let ix_sw = inner.ix_sw();

            // Resolve upstream router name.
            let upstream_name = r.uplink.and_then(|sw_id| {
                if sw_id == ix_sw {
                    return None;
                }
                let sw = inner.switch(sw_id)?;
                let owner = sw.owner_router?;
                Some(inner.router(owner)?.name.to_string())
            });

            // Resolve downstream bridge name.
            let ds_bridge = r
                .downlink
                .and_then(|sw_id| inner.switch(sw_id)?.bridge.as_ref().map(|b| b.to_string()))
                .unwrap_or_default();

            // Emit RouterAdded event.
            let router_state = RouterState::from_router_data(r, upstream_name, ds_bridge);
            self.inner.emit(LabEventKind::RouterAdded {
                name: r.name.to_string(),
                state: Box::new(router_state),
            });

            // Register ns → name mapping.
            self.inner
                .ns_to_name
                .lock()
                .unwrap()
                .insert(r.ns.to_string(), r.name.to_string());

            Router::new(id, r.name.clone(), r.ns.clone(), Arc::clone(&self.inner))
        };
        if let Some(cond) = self.downlink_condition {
            router.set_downlink_condition(Some(cond)).await?;
        }
        Ok(router)
    }
}
