//! Integration test suite for the patchbay network simulator.
//!
//! Tests are organized into submodules by feature area:
//!
//! - [`smoke`] — Basic connectivity (ping, UDP/TCP roundtrip, same-LAN)
//! - [`nat`] — NAT mapping types, reflexive IPs, CGNAT, port behavior
//! - [`nat_rebind`] — Dynamic NAT mode changes and conntrack flush
//! - [`hairpin`] — Hairpin / loopback NAT forwarding
//! - [`holepunch`] — UDP hole punching between NATted peers
//! - [`route`] — Default route switching, interface replug
//! - [`iface`] — Interface add/remove at runtime, IP renew, secondary IPs
//! - [`link_condition`] — Rate limiting, packet loss, latency, presets
//! - [`firewall`] — Firewall presets and custom rules
//! - [`dns`] — Name resolution, /etc/hosts overlay, resolv.conf
//! - [`ipv6`] — Dual-stack and v6-only behavior, accessors
//! - [`region`] — Multi-region latency, break/restore links, transit
//! - [`mtu`] — MTU configuration and PMTU blackhole
//! - [`lifecycle`] — Lab construction, TOML loading, device/router removal
//! - [`alloc`] — Internal IP allocator correctness

use std::{
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use n0_tracing_test::traced_test;
use tokio::{net::UdpSocket, sync::oneshot};
use tracing::{debug, error, error_span, info, Instrument};

use super::*;
use crate::{check_caps, config};

mod alloc;
mod devtools;
mod dns;
mod firewall;
mod hairpin;
mod holepunch;
mod iface;
mod ipv6;
mod lifecycle;
mod link_condition;
mod mtu;
mod nat;
mod nat64;
mod nat_rebind;
mod preset;
mod region;
mod route;
mod smoke;

// ── Shared test infrastructure ──────────────────────────────────────

#[ctor::ctor]
fn init() {
    let _ = init_userns();
}

fn ping(addr: &str) -> Result<()> {
    let status = std::process::Command::new("ping")
        .args(["-c", "1", "-W", "1", addr])
        .status()
        .context("ping spawn")?;
    if !status.success() {
        bail!("ping {} failed with status {}", addr, status);
    }
    Ok(())
}

fn ping_fails(addr: &str) -> Result<()> {
    let status = std::process::Command::new("ping")
        .args(["-c", "1", "-W", "1", addr])
        .status()
        .context("ping spawn")?;
    if status.success() {
        bail!("ping {} unexpectedly succeeded", addr);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, strum::EnumIter, strum::Display)]
enum UplinkWiring {
    DirectIx,
    ViaPublicIsp,
    ViaCgnatIsp,
}

impl UplinkWiring {
    fn label(self) -> &'static str {
        match self {
            Self::DirectIx => "direct-ix",
            Self::ViaPublicIsp => "via-public-isp",
            Self::ViaCgnatIsp => "via-cgnat-isp",
        }
    }
}

#[derive(Clone, Copy, Debug, strum::EnumIter, strum::Display)]
enum Proto {
    Udp,
    Tcp,
}

#[derive(Clone, Copy, Debug, strum::EnumIter, strum::Display)]
enum BindMode {
    Unspecified,
    SpecificIp,
}

struct NatTestCtx {
    dev: Device,
    dev_ip: Ipv4Addr,
    expected_ip: Ipv4Addr,
    r_dc: SocketAddr,
    r_ix: SocketAddr,
    _reflectors: Vec<core::ReflectorGuard>,
}

struct DualNatLab {
    _lab: Lab,
    dc: Router,
    dev: Device,
    nat_a: Router,
    nat_b: Router,
    reflector: SocketAddr,
    _reflector_guard: core::ReflectorGuard,
}

// ── Test helper functions ────────────────────────────────────────────

/// TCP probe: connects, reads "OBSERVED <addr>", parses it.
async fn probe_tcp(target: SocketAddr) -> Result<ObservedAddr> {
    use tokio::io::AsyncReadExt;
    let timeout = Duration::from_millis(500);
    let mut stream = tokio::time::timeout(timeout, tokio::net::TcpStream::connect(target))
        .await
        .context("tcp connect timeout")?
        .context("tcp connect")?;
    let mut buf = vec![0u8; 256];
    let n = tokio::time::timeout(timeout, stream.read(&mut buf))
        .await
        .context("tcp read timeout")?
        .context("tcp read")?;
    let s = std::str::from_utf8(&buf[..n]).context("utf8")?;
    let addr_str = s
        .strip_prefix("OBSERVED ")
        .ok_or_else(|| anyhow!("unexpected tcp reflector reply: {:?}", s))?;
    addr_str.parse().context("parse observed addr")
}

async fn probe_reflexive_addr(
    dev: &Device,
    proto: Proto,
    bind: BindMode,
    dev_ip: Ipv4Addr,
    reflector: SocketAddr,
) -> Result<ObservedAddr> {
    let bind_addr = match bind {
        BindMode::Unspecified => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        BindMode::SpecificIp => SocketAddr::new(IpAddr::V4(dev_ip), 0),
    };
    match proto {
        Proto::Udp => dev.run_sync(move || {
            test_utils::probe_udp(reflector, Duration::from_millis(500), Some(bind_addr))
        }),
        Proto::Tcp => dev
            .spawn(move |_| async move { probe_tcp(reflector).await })?
            .await
            .context("probe_tcp task panicked")?,
    }
}

async fn probe_reflexive(
    dev: &Device,
    proto: Proto,
    bind: BindMode,
    ctx: &NatTestCtx,
) -> Result<ObservedAddr> {
    probe_reflexive_addr(dev, proto, bind, ctx.dev_ip, ctx.r_dc).await
}

/// TCP sink: accepts one connection, drains all bytes, then exits.
fn tcp_sink(addr: SocketAddr) -> Result<()> {
    use std::io::Read as _;
    let listener = std::net::TcpListener::bind(addr).context("tcp sink bind")?;
    let (mut stream, _) = listener.accept().context("tcp sink accept")?;
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Sends `bytes` bytes over TCP. Returns `(elapsed, kbit/s)`.
fn tcp_measure_throughput(server_addr: SocketAddr, bytes: usize) -> Result<(Duration, u32)> {
    use std::{
        io::{Read as _, Write as _},
        time::Instant,
    };
    let mut stream = std::net::TcpStream::connect_timeout(&server_addr, Duration::from_secs(5))
        .context("tcp connect")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(60)))
        .context("set write timeout")?;
    let chunk = vec![0u8; 4096];
    let start = Instant::now();
    let mut sent = 0;
    while sent < bytes {
        let n = chunk.len().min(bytes - sent);
        stream.write_all(&chunk[..n]).context("tcp write")?;
        sent += n;
    }
    stream
        .shutdown(std::net::Shutdown::Write)
        .context("tcp shutdown")?;
    let mut tmp = [0u8; 1];
    let _ = stream.read(&mut tmp);
    let elapsed = start.elapsed();
    let kbps = ((bytes as u64 * 8) / (elapsed.as_millis() as u64).max(1)) as u32;
    Ok((elapsed, kbps))
}

fn join_sink(join: thread::JoinHandle<Result<()>>) -> Result<()> {
    join.join()
        .map_err(|_| anyhow!("tcp sink thread panicked"))?
}

/// Spawns an async TCP reflector that replies "OBSERVED {peer}" then closes.
async fn spawn_tcp_reflector(bind: SocketAddr) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let (ready_tx, ready_rx) = oneshot::channel::<Result<()>>();
    tokio::spawn(async move {
        match tokio::net::TcpListener::bind(bind).await {
            Ok(listener) => {
                let _ = ready_tx.send(Ok(()));
                loop {
                    let Ok((mut stream, peer)) = listener.accept().await else {
                        break;
                    };
                    let msg = format!("OBSERVED {}", peer);
                    let _ = stream.write_all(msg.as_bytes()).await;
                }
            }
            Err(e) => {
                let _ = ready_tx.send(Err(anyhow!("tcp reflector bind {bind}: {e}")));
            }
        }
    });
    ready_rx
        .await
        .map_err(|_| anyhow!("tcp reflector task dropped before ready"))?
}

/// Spawns an async TCP echo server (reads payload, writes it back).
async fn spawn_tcp_echo_server(bind: SocketAddr) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (ready_tx, ready_rx) = oneshot::channel::<Result<()>>();
    tokio::spawn(async move {
        match tokio::net::TcpListener::bind(bind).await {
            Ok(listener) => {
                debug!(addr = %listener.local_addr().unwrap(), "TCP listener bound");
                let _ = ready_tx.send(Ok(()));
                loop {
                    let Ok((mut stream, remote)) = listener.accept().await else {
                        break;
                    };
                    debug!(%remote, "accepted stream");
                    let mut buf = [0u8; 64];
                    if let Ok(n) = stream.read(&mut buf).await {
                        let _ = stream.write_all(&buf[..n]).await;
                    }
                }
            }
            Err(e) => {
                let _ = ready_tx.send(Err(anyhow!("tcp echo bind {bind}: {e}")));
            }
        }
    });
    ready_rx
        .await
        .map_err(|_| anyhow!("tcp echo server task dropped before ready"))?
}

/// TCP echo roundtrip with 500ms timeout.
async fn tcp_roundtrip(target: SocketAddr) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let timeout = Duration::from_millis(500);
    let start = Instant::now();
    let mut stream = tokio::time::timeout(timeout, tokio::net::TcpStream::connect(target))
        .await
        .context("tcp connect timeout")?
        .context("tcp connect")?;
    debug!(remote = %target, local_addr = %stream.local_addr().unwrap(), time=?start.elapsed(), "connected");
    let payload = b"ping";
    let start = Instant::now();
    tokio::time::timeout(timeout, stream.write_all(payload))
        .await
        .context("tcp write timeout")?
        .context("tcp write")?;
    let mut buf = [0u8; 4];
    tokio::time::timeout(timeout, stream.read_exact(&mut buf))
        .await
        .context("tcp read timeout")?
        .context("tcp read")?;
    debug!(time=?start.elapsed(), "echo complete");
    if &buf != payload {
        bail!("tcp echo mismatch: {:?}", buf);
    }
    Ok(())
}

// ── Lab builder helpers ──────────────────────────────────────────────

async fn build_nat_case(
    nat_mode: Nat,
    wiring: UplinkWiring,
    port_base: u16,
) -> Result<(Lab, NatTestCtx)> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let upstream = match wiring {
        UplinkWiring::DirectIx => None,
        UplinkWiring::ViaPublicIsp => Some(lab.add_router("isp").build().await?),
        UplinkWiring::ViaCgnatIsp => Some(lab.add_router("isp").nat(Nat::Cgnat).build().await?),
    };
    let nat = {
        let mut rb = lab.add_router("nat").nat(nat_mode);
        if let Some(u) = &upstream {
            rb = rb.upstream(u.id());
        }
        rb.build().await?
    };
    let dev = lab
        .add_device("dev")
        .iface("eth0", nat.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r_dc = SocketAddr::new(IpAddr::V4(dc_ip), port_base);
    let r_ix = SocketAddr::new(IpAddr::V4(lab.ix().gw()), port_base + 1);

    let g1 = dc.spawn_reflector(r_dc).await?;
    let ix = lab.ix();
    let g2 = ix.spawn_reflector(r_ix).await?;

    dc.spawn(move |_| async move { spawn_tcp_reflector(r_dc).await })?
        .await
        .context("tcp reflector task panicked")??;

    tokio::time::sleep(Duration::from_millis(200)).await;

    let dev_ip = dev.ip().unwrap();
    let expected_ip = match (nat_mode, wiring) {
        (_, UplinkWiring::ViaCgnatIsp) => lab
            .router_by_name("isp")
            .context("missing isp")?
            .uplink_ip()
            .context("no uplink ip")?,
        (Nat::None, _) => dev_ip,
        _ => nat.uplink_ip().context("no uplink ip")?,
    };
    Ok((
        lab,
        NatTestCtx {
            dev,
            dev_ip,
            expected_ip,
            r_dc,
            r_ix,
            _reflectors: vec![g1, g2],
        },
    ))
}

async fn build_dual_nat_lab(mode_a: Nat, mode_b: Nat, port_base: u16) -> Result<DualNatLab> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let nat_a = lab.add_router("nat-a").nat(mode_a).build().await?;
    let nat_b = lab.add_router("nat-b").nat(mode_b).build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", nat_a.id(), None)
        .iface("eth1", nat_b.id(), None)
        .default_via("eth0")
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), port_base);

    let guard = dc.spawn_reflector(reflector).await?;
    dc.spawn(move |_| async move { spawn_tcp_reflector(reflector).await })?
        .await
        .context("tcp reflector task panicked")??;

    tokio::time::sleep(Duration::from_millis(200)).await;
    Ok(DualNatLab {
        _lab: lab,
        dc,
        dev,
        nat_a,
        nat_b,
        reflector,
        _reflector_guard: guard,
    })
}

async fn build_single_nat_case(
    nat_mode: Nat,
    wiring: UplinkWiring,
    port_base: u16,
) -> Result<(
    Lab,
    String,
    SocketAddr,
    SocketAddr,
    Ipv4Addr,
    Vec<core::ReflectorGuard>,
)> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let upstream = match wiring {
        UplinkWiring::DirectIx => None,
        UplinkWiring::ViaPublicIsp => Some(lab.add_router("isp").build().await?),
        UplinkWiring::ViaCgnatIsp => Some(lab.add_router("isp").nat(Nat::Cgnat).build().await?),
    };
    let nat = {
        let mut rb = lab.add_router("nat").nat(nat_mode);
        if let Some(u) = &upstream {
            rb = rb.upstream(u.id());
        }
        rb.build().await?
    };
    let dev = lab
        .add_device("dev")
        .iface("eth0", nat.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r_dc = SocketAddr::new(IpAddr::V4(dc_ip), port_base);
    let r_ix = SocketAddr::new(IpAddr::V4(lab.ix().gw()), port_base + 1);
    let g1 = dc.spawn_reflector(r_dc).await?;
    let ix = lab.ix();
    let g2 = ix.spawn_reflector(r_ix).await?;

    let dev_ns = dev.ns();
    let expected_ip = match (nat_mode, wiring) {
        (_, UplinkWiring::ViaCgnatIsp) => lab
            .router_by_name("isp")
            .context("missing isp")?
            .uplink_ip()
            .context("no uplink ip")?,
        (Nat::None, _) => dev.ip().unwrap(),
        _ => nat.uplink_ip().context("no uplink ip")?,
    };
    Ok((
        lab,
        dev_ns.to_string(),
        r_dc,
        r_ix,
        expected_ip,
        vec![g1, g2],
    ))
}

/// Wraps a future with a tracing span and tokio timeout.
async fn span_log_timeout(
    id: &str,
    timeout: Duration,
    fut: impl Future<Output = Result<()>>,
) -> Result<()> {
    async {
        match tokio::time::timeout(timeout, fut).await {
            Err(err) => Err(err.into()),
            Ok(res) => res,
        }
        .inspect_err(|err| error!("{err:#}"))
    }
    .instrument(error_span!("ep", %id))
    .await
}
