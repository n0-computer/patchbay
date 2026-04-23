//! Port mapping server orchestration.
//!
//! [`PortmapServer`] owns the UDP 5351 socket shared between NAT-PMP and
//! PCP, the shared mapping registry, and the per-protocol dispatch task.
//! When UPnP is enabled it also owns the SSDP listener and the HTTP
//! listener spawned from [`super::upnp`].
//!
//! The server follows the same lifecycle pattern as
//! [`crate::dns_server::DnsServer`]: a cloneable handle backed by
//! `Arc<AbortOnDropHandle<()>>` so the task dies when every clone drops.
//! Explicit shutdown via [`PortmapServer::shutdown`] runs the nft cleanup
//! before releasing the registry, ensuring the router's namespace does not
//! retain orphan DNAT rules after the server goes away.

use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use ipnet::Ipv4Net;
use tokio::{net::UdpSocket, sync::Mutex};
use tokio_util::task::AbortOnDropHandle;
use tracing::{debug, warn};

use super::{config::PortmapConfig, nat_pmp, nft, pcp, registry::PortmapRegistry, upnp};
use crate::netns::NetnsManager;

/// Maximum lifetime a client may request, in seconds. Mirrors the
/// recommendation from RFC 6886 section 3.3.
const MAX_LIFETIME_SECS: u64 = 2 * 60 * 60;

/// Period at which the background reaper sweeps expired mappings.
///
/// Mappings carry a client-requested lifetime clamped by
/// [`MAX_LIFETIME_SECS`]. The reaper removes any mapping whose deadline
/// has elapsed and re-renders the nftables table so the DNAT rules
/// match the current registry.
const REAPER_INTERVAL: Duration = Duration::from_secs(30);

/// Shared state threaded through every protocol handler.
///
/// `epoch` is the router-local epoch zero: response `epoch_time` fields
/// report seconds since this instant. Real routers use seconds since the
/// portmap service started; the simulator only needs the value to be
/// monotonic and non-zero within a single server instance.
pub(crate) struct ServerContext {
    pub(super) registry: Arc<Mutex<PortmapRegistry>>,
    pub(super) netns: Arc<NetnsManager>,
    pub(super) ns: Arc<str>,
    pub(super) wan_ip: Ipv4Addr,
    pub(super) downstream_cidr: Ipv4Net,
    pub(super) epoch: Instant,
    /// Clamp for requested lifetimes. Matches the recommended 2-hour
    /// maximum from RFC 6886 section 3.3.
    pub(super) max_lifetime: Duration,
}

impl ServerContext {
    /// Seconds since this server started, saturating at `u32::MAX`.
    pub(super) fn epoch_time(&self) -> u32 {
        self.epoch.elapsed().as_secs().min(u64::from(u32::MAX)) as u32
    }

    /// Snapshots the mapping set while still holding the registry lock
    /// and applies it to nftables. Holding the lock across the apply
    /// serializes concurrent allocate+apply sequences so an older
    /// snapshot cannot overwrite a newer one.
    ///
    /// On `nft` failure a just-created mapping is rolled back so
    /// clients that receive an error do not find an orphan entry in
    /// the registry later.
    pub(super) async fn apply_after_mutation(
        &self,
        registry: &mut PortmapRegistry,
        created: Option<super::registry::MappingKey>,
    ) -> Result<()> {
        let snapshot: Vec<_> = registry.iter().cloned().collect();
        match nft::apply_portmap_rules(&self.netns, &self.ns, self.wan_ip, &snapshot).await {
            Ok(()) => Ok(()),
            Err(e) => {
                warn!(error = %e, "portmap: nft apply failed; rolling back");
                if let Some(key) = created {
                    registry.remove(key);
                }
                Err(e)
            }
        }
    }
}

/// Cloneable handle to a per-router portmap server.
#[derive(Clone)]
pub(crate) struct PortmapServer {
    inner: Arc<PortmapServerInner>,
}

struct PortmapServerInner {
    cfg: PortmapConfig,
    #[allow(dead_code)]
    registry: Arc<Mutex<PortmapRegistry>>,
    netns: Arc<NetnsManager>,
    ns: Arc<str>,
    wan_ip: Ipv4Addr,
    _task: AbortOnDropHandle<()>,
    _upnp_tasks: Vec<AbortOnDropHandle<()>>,
    _reaper: AbortOnDropHandle<()>,
}

impl std::fmt::Debug for PortmapServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortmapServer")
            .field("cfg", &self.inner.cfg)
            .field("ns", &self.inner.ns)
            .field("wan_ip", &self.inner.wan_ip)
            .finish()
    }
}

impl PortmapServer {
    /// Starts the portmap server inside `ns`.
    ///
    /// Binds UDP 5351 on `downstream_gw` (the router's LAN gateway IP).
    /// Both NAT-PMP and PCP share that socket once PCP lands; today only
    /// NAT-PMP is wired through. `wan_ip` is reported to clients as the
    /// external address, and `downstream_cidr` is the authorization check
    /// for the client source address.
    pub(crate) async fn start(
        netns: Arc<NetnsManager>,
        ns: Arc<str>,
        cfg: PortmapConfig,
        downstream_gw: Ipv4Addr,
        wan_ip: Ipv4Addr,
        downstream_cidr: Ipv4Net,
    ) -> Result<Self> {
        let registry = Arc::new(Mutex::new(PortmapRegistry::new()));

        // Wipe any stale table from an earlier run before starting: the
        // router namespace may have been recycled.
        nft::apply_portmap_rules(&netns, &ns, wan_ip, &[])
            .await
            .ok();

        let ctx = Arc::new(ServerContext {
            registry: registry.clone(),
            netns: netns.clone(),
            ns: ns.clone(),
            wan_ip,
            downstream_cidr,
            epoch: Instant::now(),
            max_lifetime: Duration::from_secs(MAX_LIFETIME_SECS),
        });

        let task = spawn_dispatch(netns.clone(), ns.clone(), cfg, ctx.clone(), downstream_gw)?;

        let mut upnp_tasks = Vec::new();
        if cfg.enable_upnp {
            let (ssdp, http) =
                upnp::spawn(netns.clone(), ns.clone(), ctx.clone(), downstream_gw).await?;
            upnp_tasks.push(AbortOnDropHandle::new(ssdp));
            upnp_tasks.push(AbortOnDropHandle::new(http));
        }

        let reaper = spawn_reaper(&netns, &ns, ctx)?;

        Ok(Self {
            inner: Arc::new(PortmapServerInner {
                cfg,
                registry,
                netns,
                ns,
                wan_ip,
                _task: task,
                _upnp_tasks: upnp_tasks,
                _reaper: reaper,
            }),
        })
    }

    /// Tears down the server's nftables table. Callers should call this
    /// before dropping the last handle so the rules do not outlive the
    /// server. Missing rules are not an error: the helper is idempotent.
    pub(crate) async fn shutdown(&self) -> Result<()> {
        nft::clear_portmap_rules(&self.inner.netns, &self.inner.ns).await
    }
}

fn spawn_dispatch(
    netns: Arc<NetnsManager>,
    ns: Arc<str>,
    cfg: PortmapConfig,
    ctx: Arc<ServerContext>,
    downstream_gw: Ipv4Addr,
) -> Result<AbortOnDropHandle<()>> {
    let std_socket: std::net::UdpSocket = netns.run_closure_in(&ns, move || {
        let addr = SocketAddrV4::new(downstream_gw, nat_pmp::SERVER_PORT);
        let sock = std::net::UdpSocket::bind(addr)
            .with_context(|| format!("bind portmap server to {addr}"))?;
        sock.set_nonblocking(true)?;
        Ok(sock)
    })?;

    let rt = netns.rt_handle_for(&ns)?;
    let handle = rt.spawn(async move {
        let socket = match UdpSocket::from_std(std_socket) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "portmap: convert UDP socket");
                return;
            }
        };
        debug!(
            ns = %ctx.ns,
            gw = %downstream_gw,
            port = nat_pmp::SERVER_PORT,
            "portmap server listening"
        );
        dispatch_loop(socket, ctx, cfg).await;
    });

    Ok(AbortOnDropHandle::new(handle))
}

fn spawn_reaper(
    netns: &Arc<NetnsManager>,
    ns: &str,
    ctx: Arc<ServerContext>,
) -> Result<AbortOnDropHandle<()>> {
    let rt = netns.rt_handle_for(ns)?;
    let handle = rt.spawn(async move {
        let mut ticker = tokio::time::interval(REAPER_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Burn the immediate first tick so we wait one REAPER_INTERVAL
        // before the first sweep. Reduces noise at startup.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let mut registry = ctx.registry.lock().await;
            let reaped = registry.reap_expired(Instant::now());
            if reaped.is_empty() {
                continue;
            }
            debug!(n = reaped.len(), "portmap: reaped expired mappings");
            ctx.apply_after_mutation(&mut registry, None).await.ok();
        }
    });
    Ok(AbortOnDropHandle::new(handle))
}

async fn dispatch_loop(socket: UdpSocket, ctx: Arc<ServerContext>, cfg: PortmapConfig) {
    let mut buf = vec![0u8; 1500];
    loop {
        let (len, src) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "portmap: recv error");
                continue;
            }
        };
        let packet = &buf[..len];
        let version = packet.first().copied().unwrap_or(u8::MAX);
        let SocketAddr::V4(src_v4) = src else {
            continue;
        };
        let client_ip = *src_v4.ip();
        let response = match version {
            nat_pmp::VERSION if cfg.enable_nat_pmp => {
                nat_pmp::handle_request(&ctx, client_ip, packet).await
            }
            pcp::VERSION if cfg.enable_pcp => pcp::handle_request(&ctx, client_ip, packet).await,
            _ => None,
        };
        if let Some(bytes) = response {
            if let Err(e) = socket.send_to(&bytes, src).await {
                warn!(error = %e, "portmap: send error");
            }
        }
    }
}
