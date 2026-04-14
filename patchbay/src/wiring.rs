//! Async namespace setup: router, device, and root-ns wiring.

use std::{
    net::{Ipv4Addr, Ipv6Addr},
    sync::Arc,
};

use anyhow::{anyhow, bail, Context, Result};
use ipnet::{Ipv4Net, Ipv6Net};
use tokio_util::sync::CancellationToken;
use tracing::{debug, instrument, trace, Instrument as _};

use crate::{
    core::{CoreConfig, DeviceData, IfaceBuild, NodeId, RaRuntimeCfg, RouterData},
    netlink::Netlink,
    netns,
    nft::{
        apply_firewall, apply_icmp_frag_block, apply_impair_in, apply_nat_for_router, apply_nat_v6,
        nptv6_wan_prefix, run_nft_in,
    },
    Ipv6DadMode, Ipv6ProvisioningMode, NatV6Mode,
};

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
    dad_mode: Ipv6DadMode,
) -> Result<()> {
    let root_ns = cfg.root_ns.clone();
    create_named_netns(netns, &root_ns, None, None, dad_mode)?;

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
    /// Whether RA worker should run for this router.
    pub ra_enabled: bool,
}

/// Sets up a single router's namespaces, links, and NAT. No lock held.
#[instrument(name = "router", skip_all, fields(id = data.router.id.0))]
pub(crate) async fn setup_router_async(
    netns: &Arc<netns::NetnsManager>,
    data: &RouterSetupData,
) -> Result<()> {
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
            trace!(nat = ?router.cfg.nat, ip = %upstream_ip4, "router: apply NAT");
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
                trace!(nat_v6 = ?router.cfg.nat_v6, %wan_pfx, %lan_pfx, "router: apply NAT v6");
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
                    h.add_default_route_v6_scoped(&wan_if, g6).await?;
                }
                Ok(())
            }
        })
        .await?;

        if let Some(upstream_ip4) = router.upstream_ip {
            trace!(nat = ?router.cfg.nat, ip = %upstream_ip4, "router: apply NAT");
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
                trace!(nat_v6 = ?router.cfg.nat_v6, %wan_pfx, %lan_pfx, "router: apply NAT v6");
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
    if data.provisioning_mode == Ipv6ProvisioningMode::RaDriven
        && data.ra_enabled
        && router.cfg.ip_support.has_v6()
    {
        spawn_ra_worker(
            netns,
            &router.ns,
            data.cancel.clone(),
            RaWorkerCfg {
                ra_runtime: Arc::clone(&router.ra_runtime),
                router_name: router.name.to_string(),
                iface: router.downlink_bridge.to_string(),
                src_ll: router.downstream_ll_v6,
            },
        )?;
    }

    debug!(name = %router.name, ns = %router.ns, wan_ip = ?router.upstream_ip, nat = ?router.cfg.nat, "router: ready");

    Ok(())
}

pub(crate) async fn emit_router_solicitation(
    netns: &Arc<netns::NetnsManager>,
    ns: String,
    device: String,
    iface: String,
    router_ll: Option<Ipv6Addr>,
) -> Result<()> {
    let ns_for_log = ns.clone();
    nl_run(netns, &ns, move |_h: Netlink| async move {
        let router_ll = router_ll.map(|ll| ll.to_string());
        tracing::info!(
            target: "patchbay::_events::RouterSolicitation",
            ns = %ns_for_log,
            device = %device,
            iface = %iface,
            dst = "ff02::2",
            router_ll = router_ll.as_deref(),
            "router solicitation"
        );
        Ok(())
    })
    .await
}

fn spawn_ra_worker(
    netns: &Arc<netns::NetnsManager>,
    ns: &str,
    cancel: CancellationToken,
    cfg: RaWorkerCfg,
) -> Result<()> {
    let rt = netns.rt_handle_for(ns)?;
    let ns = ns.to_string();
    rt.spawn(async move {
        let load_runtime = || cfg.ra_runtime.load();

        let emit_ra = |interval_secs: u64, lifetime_secs: u64| {
            if let Some(src) = cfg.src_ll {
                tracing::info!(
                    target: "patchbay::_events::RouterAdvertisement",
                    ns = %ns,
                    router = %cfg.router_name,
                    iface = %cfg.iface,
                    src = %src,
                    lifetime_secs,
                    interval_secs,
                    "router advertisement"
                );
            } else {
                tracing::warn!(
                    ns = %ns,
                    router = %cfg.router_name,
                    "ra-worker: missing link-local source address"
                );
            }
        };

        let (enabled, interval_secs, lifetime_secs) = load_runtime();
        if enabled {
            emit_ra(interval_secs, lifetime_secs);
        }

        loop {
            let (_, interval_secs, _) = load_runtime();
            let changed = cfg.ra_runtime.notified();
            tokio::pin!(changed);
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = &mut changed => {
                    tracing::trace!(ns = %ns, "ra-worker: runtime config changed");
                    let (enabled, interval_secs, lifetime_secs) = load_runtime();
                    if enabled {
                        emit_ra(interval_secs, lifetime_secs);
                    }
                }
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(interval_secs)) => {
                    tracing::trace!(ns = %ns, interval_secs, "ra-worker: tick");
                    let (enabled, interval_secs, lifetime_secs) = load_runtime();
                    if enabled {
                        emit_ra(interval_secs, lifetime_secs);
                    }
                }
            }
        }
        tracing::trace!(ns = %ns, "ra-worker: stopped");
    });
    Ok(())
}

struct RaWorkerCfg {
    ra_runtime: Arc<RaRuntimeCfg>,
    router_name: String,
    iface: String,
    src_ll: Option<Ipv6Addr>,
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

// Firewall and ICMP block rules moved to nft.rs.

/// Sets up a single device's namespace and wires all interfaces. No lock held.
pub(crate) struct DeviceSetupData {
    pub prefix: Arc<str>,
    pub root_ns: Arc<str>,
    pub dev: DeviceData,
    pub ifaces: Vec<IfaceBuild>,
    pub dns_overlay: Option<netns::DnsOverlay>,
    pub dad_mode: Ipv6DadMode,
    pub provisioning_mode: Ipv6ProvisioningMode,
}

/// Sets up a single device's namespace and wires all interfaces. No lock held.
#[instrument(name = "device", skip_all)]
pub(crate) async fn setup_device_async(
    netns: &Arc<netns::NetnsManager>,
    data: DeviceSetupData,
) -> Result<()> {
    let DeviceSetupData {
        prefix,
        root_ns,
        dev,
        ifaces,
        dns_overlay,
        dad_mode,
        provisioning_mode,
    } = data;
    let rs_ifaces: Vec<(Arc<str>, Option<Ipv6Addr>)> =
        if provisioning_mode == Ipv6ProvisioningMode::RaDriven {
            ifaces
                .iter()
                .filter(|iface| iface.is_default && iface.dev_ip_v6.is_some())
                .map(|iface| (iface.ifname.clone(), iface.gw_ll_v6))
                .collect()
        } else {
            Vec::new()
        };
    debug!(id = dev.id.0, name = %dev.name, ns = %dev.ns, "device: setup");
    let dev_ip = ifaces.iter().find(|i| i.is_default).and_then(|i| i.dev_ip);
    let log_prefix = format!("{}.{}", crate::consts::KIND_DEVICE, dev.name);
    create_named_netns(netns, &dev.ns, dns_overlay, Some(log_prefix), dad_mode)?;

    for iface in ifaces {
        wire_iface_async(netns, &prefix, &root_ns, iface).await?;
    }

    for (ifname, router_ll) in rs_ifaces {
        emit_router_solicitation(
            netns,
            dev.ns.to_string(),
            dev.name.to_string(),
            ifname.to_string(),
            router_ll,
        )
        .await?;
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

    debug!(name = %dev.name, ns = %dev.ns, ip = ?dev_ip, "device: ready");

    Ok(())
}

/// Wire a dummy interface inside a device namespace.
#[instrument(name = "iface_dummy", skip_all, fields(iface = %build.ifname))]
async fn wire_dummy_async(netns: &Arc<netns::NetnsManager>, build: &IfaceBuild) -> Result<()> {
    trace!(ip = ?build.dev_ip, ip6 = ?build.dev_ip_v6, "iface_dummy: setup");
    nl_run(netns, &build.dev_ns, {
        let ifname = build.ifname.clone();
        let dev_ip = build.dev_ip;
        let prefix_len = build.prefix_len;
        let dev_ip_v6 = build.dev_ip_v6;
        let prefix_len_v6 = build.prefix_len_v6;
        let start_down = build.start_down;
        move |h: Netlink| async move {
            h.set_link_up("lo").await?;
            h.add_dummy(&ifname).await?;
            if let Some(ip4) = dev_ip {
                h.add_addr4(&ifname, ip4, prefix_len).await?;
            }
            if let Some(ip6) = dev_ip_v6 {
                h.add_addr6(&ifname, ip6, prefix_len_v6).await?;
            }
            if !start_down {
                h.set_link_up(&ifname).await?;
            }
            Ok(())
        }
    })
    .await?;
    if let Some(cond) = build.egress {
        apply_impair_in(netns, &build.dev_ns, &build.ifname, cond).await;
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
    if dev.dummy {
        return wire_dummy_async(netns, &dev).await;
    }
    trace!(ip = ?dev.dev_ip, ip6 = ?dev.dev_ip_v6, gw = ?dev.gw_ip, gw6 = ?dev.gw_ip_v6, "iface: assigned addresses");
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

    // DAD mode was configured by create_named_netns before interfaces were created.
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

    if let Some(cond) = dev.egress {
        apply_impair_in(netns, &dev.dev_ns, &dev.ifname, cond).await;
    }
    if let Some(cond) = dev.ingress {
        let gw_ifname: Arc<str> = format!("v{}", dev.idx).into();
        apply_impair_in(netns, &dev.gw_ns, &gw_ifname, cond).await;
    }
    if dev.start_down {
        nl_run(netns, &dev.dev_ns, {
            let ifname = dev.ifname.clone();
            move |h: Netlink| async move { h.set_link_down(&ifname).await }
        })
        .await?;
    }
    Ok(())
}

pub(crate) fn add_host(cidr: Ipv4Net, host: u8) -> Result<Ipv4Addr> {
    let octets = cidr.addr().octets();
    if host == 0 || host == 255 {
        bail!("invalid host offset {}", host);
    }
    Ok(Ipv4Addr::new(octets[0], octets[1], octets[2], host))
}

// ── Link-local address generation ─────────────

pub(crate) fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
    x ^ (x >> 31)
}

pub(crate) fn seed2(a: u64, b: u64) -> u64 {
    splitmix64(a.rotate_left(7) ^ b.rotate_right(3) ^ 0xC2B2AE3D27D4EB4F)
}

pub(crate) fn seed3(a: u64, b: u64, c: u64) -> u64 {
    splitmix64(seed2(a, b) ^ c.rotate_left(17))
}

pub(crate) fn link_local_from_seed(seed: u64) -> Ipv6Addr {
    let mixed = splitmix64(seed);
    let mut iid = mixed.to_be_bytes();
    iid[0] |= 0x02;
    let a = u16::from_be_bytes([iid[0], iid[1]]);
    let b = u16::from_be_bytes([iid[2], iid[3]]);
    let c = u16::from_be_bytes([iid[4], iid[5]]);
    let d = u16::from_be_bytes([iid[6], iid[7]]);
    Ipv6Addr::new(0xfe80, 0, 0, 0, a, b, c, d)
}

// ─────────────────────────────────────────────
// Netns + process helpers
// ─────────────────────────────────────────────

/// Creates a namespace with optional DNS overlay and applies IPv6 DAD mode.
///
/// When `dad_mode` is disabled, this sets `accept_dad=0` and
/// `dad_transmits=0` before interfaces are moved in.
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
    trace!(path = %path, val = %val, "sysctl: set");
    std::fs::write(format!("/proc/sys/{}", path), val)
        .with_context(|| format!("sysctl write {}", path))
}

// nftables rules, NAT application, firewall, and tc impairment moved to nft.rs.
