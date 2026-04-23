//! Port mapping server orchestration.
//!
//! [`PortmapServer`] owns the UDP 5351 socket shared between NAT-PMP and
//! PCP, the shared mapping registry, and the per-protocol dispatch task.
//! UPnP lands alongside in Step 5; until then only the NAT-PMP dispatcher
//! is wired in.
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

use super::{config::PortmapConfig, nat_pmp, nft, registry::PortmapRegistry};
use crate::netns::NetnsManager;

/// Maximum lifetime a client may request, in seconds. Mirrors the
/// recommendation from RFC 6886 section 3.3.
const MAX_LIFETIME_SECS: u64 = 2 * 60 * 60;

/// Cloneable handle to a per-router portmap server.
#[derive(Clone)]
pub(crate) struct PortmapServer {
    inner: Arc<PortmapServerInner>,
}

struct PortmapServerInner {
    cfg: PortmapConfig,
    registry: Arc<Mutex<PortmapRegistry>>,
    netns: Arc<NetnsManager>,
    ns: Arc<str>,
    wan_ip: Ipv4Addr,
    _task: AbortOnDropHandle<()>,
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

        let task = spawn_dispatch(
            netns.clone(),
            ns.clone(),
            cfg,
            registry.clone(),
            downstream_gw,
            wan_ip,
            downstream_cidr,
        )?;

        Ok(Self {
            inner: Arc::new(PortmapServerInner {
                cfg,
                registry,
                netns,
                ns,
                wan_ip,
                _task: task,
            }),
        })
    }

    /// Returns the configuration the server was started with.
    #[allow(dead_code)]
    pub(crate) fn config(&self) -> PortmapConfig {
        self.inner.cfg
    }

    /// Returns the current mapping count. Intended for tests and metrics.
    #[allow(dead_code)]
    pub(crate) async fn mapping_count(&self) -> usize {
        self.inner.registry.lock().await.len()
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
    registry: Arc<Mutex<PortmapRegistry>>,
    downstream_gw: Ipv4Addr,
    wan_ip: Ipv4Addr,
    downstream_cidr: Ipv4Net,
) -> Result<AbortOnDropHandle<()>> {
    let std_socket: std::net::UdpSocket = netns.run_closure_in(&ns, move || {
        let addr = SocketAddrV4::new(downstream_gw, nat_pmp::SERVER_PORT);
        let sock = std::net::UdpSocket::bind(addr)
            .with_context(|| format!("bind portmap server to {addr}"))?;
        sock.set_nonblocking(true)?;
        Ok(sock)
    })?;

    let rt = netns.rt_handle_for(&ns)?;
    let ctx = Arc::new(nat_pmp::Context {
        registry,
        netns: netns.clone(),
        ns: ns.clone(),
        wan_ip,
        downstream_cidr,
        epoch: Instant::now(),
        max_lifetime: Duration::from_secs(MAX_LIFETIME_SECS),
    });
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

async fn dispatch_loop(socket: UdpSocket, ctx: Arc<nat_pmp::Context>, cfg: PortmapConfig) {
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
        let response = match version {
            nat_pmp::VERSION if cfg.enable_nat_pmp => {
                let SocketAddr::V4(src_v4) = src else {
                    continue;
                };
                nat_pmp::handle_request(&ctx, *src_v4.ip(), packet).await
            }
            // PCP wire version is 2; handled in a later step. For now the
            // server silently drops PCP packets when PCP is disabled and
            // responds with "unsupported version" once PCP is added.
            _ => None,
        };
        if let Some(bytes) = response {
            if let Err(e) = socket.send_to(&bytes, src).await {
                warn!(error = %e, "portmap: send error");
            }
        }
    }
}
