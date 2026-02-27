use std::{
    future::Future,
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    thread,
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use n0_tracing_test::traced_test;
use tokio::{net::UdpSocket, sync::oneshot};
use tracing::{error, error_span, info, Instrument};

use super::*;
use crate::{check_caps, config};

#[ctor::ctor]
fn init() {
    let _ = crate::init_userns();
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
}

struct DualNatLab {
    _lab: Lab,
    dc: Router,
    dev: Device,
    nat_a: Router,
    nat_b: Router,
    reflector: SocketAddr,
}

// ── Test helper functions ────────────────────────────────────────────

/// UDP probe with explicit bind address — ns-free, call inside `handle.run_sync`.
fn probe_udp_from(reflector: SocketAddr, bind: SocketAddr) -> Result<ObservedAddr> {
    use std::net::UdpSocket;
    let sock = UdpSocket::bind(bind)?;
    sock.set_read_timeout(Some(Duration::from_millis(500)))?;
    let mut buf = [0u8; 512];
    for _attempt in 1..=3 {
        sock.send_to(b"PROBE", reflector)?;
        match sock.recv_from(&mut buf) {
            Ok((n, _)) => {
                let s = std::str::from_utf8(&buf[..n]).context("utf8")?;
                let addr_str = s
                    .strip_prefix("OBSERVED ")
                    .ok_or_else(|| anyhow!("unexpected reflector reply: {:?}", s))?;
                return Ok(ObservedAddr {
                    observed: addr_str.parse().context("parse observed addr")?,
                });
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(anyhow!("probe timed out after 3 attempts"))
}

/// TCP probe — ns-free async, call inside `handle.spawn(|_| async { probe_tcp(t).await })`.
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
    Ok(ObservedAddr {
        observed: addr_str.parse().context("parse observed addr")?,
    })
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
        Proto::Udp => dev.run_sync(move || probe_udp_from(reflector, bind_addr)),
        Proto::Tcp => dev
            .spawn(move |_| async move { probe_tcp(reflector).await })
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

/// TCP sink: accept one connection, drain all bytes, exit — ns-free.
/// Call via `handle.spawn_thread(move || tcp_sink(addr))`.
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

/// Sends `bytes` bytes over TCP to `server_addr` — ns-free.
/// Call via `handle.run_sync(move || tcp_measure_throughput(addr, bytes))`.
/// Returns `(elapsed, kbit/s)`.
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
    // Wait for server to acknowledge EOF.
    let mut tmp = [0u8; 1];
    let _ = stream.read(&mut tmp);
    let elapsed = start.elapsed();
    let kbps = ((bytes as u64 * 8) / (elapsed.as_millis() as u64).max(1)) as u32;
    Ok((elapsed, kbps))
}

/// Sends `total` UDP datagrams to `target` and collects echoes — ns-free.
/// Call via `handle.run_sync(move || udp_send_recv_count(target, total, payload, wait))`.
/// Returns `(sent, received)`.
fn udp_send_recv_count(
    target: SocketAddr,
    total: usize,
    payload: usize,
    wait: Duration,
) -> Result<(usize, usize)> {
    use std::time::Instant;
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").context("udp bind")?;
    sock.set_read_timeout(Some(Duration::from_millis(200)))
        .context("set timeout")?;
    let buf = vec![0u8; payload];
    let mut recv_buf = vec![0u8; payload + 64];
    for _ in 0..total {
        let _ = sock.send_to(&buf, target);
    }
    let deadline = Instant::now() + wait;
    let mut received = 0usize;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let timeout = remaining.min(Duration::from_millis(200));
        sock.set_read_timeout(Some(timeout)).ok();
        match sock.recv_from(&mut recv_buf) {
            Ok(_) => received += 1,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
    }
    Ok((total, received))
}

/// Spawns an async TCP reflector (accept → "OBSERVED {peer}" → close) — ns-free.
///
/// Returns when the listener is bound. The background task continues on the
/// current tokio runtime. Call inside `handle.spawn(|_| async { spawn_tcp_reflector(b).await })`.
async fn spawn_tcp_reflector(bind: SocketAddr) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<()>>();
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

async fn build_nat_case(
    nat_mode: NatMode,
    wiring: UplinkWiring,
    port_base: u16,
) -> Result<(Lab, NatTestCtx)> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").region("eu").build().await?;
    let upstream = match wiring {
        UplinkWiring::DirectIx => None,
        UplinkWiring::ViaPublicIsp => Some(lab.add_router("isp").region("eu").build().await?),
        UplinkWiring::ViaCgnatIsp => Some(
            lab.add_router("isp")
                .region("eu")
                .nat(NatMode::Cgnat)
                .build()
                .await?,
        ),
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

    // UDP reflector (managed by lab).
    dc.spawn_reflector(r_dc)?;
    let ix = lab.ix();
    ix.spawn_reflector(r_ix)?;

    // TCP reflector on the DC namespace's async worker.
    dc.spawn(move |_| async move { spawn_tcp_reflector(r_dc).await })
        .await
        .context("tcp reflector task panicked")??;

    tokio::time::sleep(Duration::from_millis(200)).await;

    let dev_ip = dev.ip();
    let expected_ip = match (nat_mode, wiring) {
        (_, UplinkWiring::ViaCgnatIsp) => lab
            .router_by_name("isp")
            .context("missing isp")?
            .uplink_ip()
            .context("no uplink ip")?,
        (NatMode::None, _) => dev_ip,
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
        },
    ))
}

async fn build_dual_nat_lab(
    mode_a: NatMode,
    mode_b: NatMode,
    port_base: u16,
) -> Result<DualNatLab> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").region("eu").build().await?;
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

    dc.spawn_reflector(reflector)?;
    dc.spawn(move |_| async move { spawn_tcp_reflector(reflector).await })
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
    })
}

async fn build_single_nat_case(
    nat_mode: NatMode,
    wiring: UplinkWiring,
    port_base: u16,
) -> Result<(Lab, String, SocketAddr, SocketAddr, Ipv4Addr)> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").region("eu").build().await?;
    let upstream = match wiring {
        UplinkWiring::DirectIx => None,
        UplinkWiring::ViaPublicIsp => Some(lab.add_router("isp").region("eu").build().await?),
        UplinkWiring::ViaCgnatIsp => Some(
            lab.add_router("isp")
                .region("eu")
                .nat(NatMode::Cgnat)
                .build()
                .await?,
        ),
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
    dc.spawn_reflector(r_dc)?;
    let ix = lab.ix();
    ix.spawn_reflector(r_ix)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let dev_ns = dev.ns();
    let expected_ip = match (nat_mode, wiring) {
        (_, UplinkWiring::ViaCgnatIsp) => lab
            .router_by_name("isp")
            .context("missing isp")?
            .uplink_ip()
            .context("no uplink ip")?,
        (NatMode::None, _) => dev.ip(),
        _ => nat.uplink_ip().context("no uplink ip")?,
    };
    Ok((lab, dev_ns, r_dc, r_ix, expected_ip))
}

/// Spawns an async TCP echo server that loops accepting connections,
/// echoes each one's payload, and continues until the runtime shuts down — ns-free.
/// Returns when the listener is bound. Call inside `handle.spawn(|_| async { spawn_tcp_echo_server(b).await })`.
async fn spawn_tcp_echo_server(bind: SocketAddr) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<()>>();
    tokio::spawn(async move {
        match tokio::net::TcpListener::bind(bind).await {
            Ok(listener) => {
                let _ = ready_tx.send(Ok(()));
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        break;
                    };
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

/// Spawns an async TCP echo server that accepts one connection, echoes bytes, then stops — ns-free.
/// Returns when the listener is bound. Call inside `handle.spawn(|_| async { spawn_tcp_echo_in(b).await })`.
async fn spawn_tcp_echo_in(bind: SocketAddr) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<()>>();
    tokio::spawn(async move {
        match tokio::net::TcpListener::bind(bind).await {
            Ok(listener) => {
                let _ = ready_tx.send(Ok(()));
                if let Ok((mut stream, _)) = listener.accept().await {
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
        .map_err(|_| anyhow!("tcp echo task dropped before ready"))?
}

/// TCP roundtrip — ns-free async. Call inside `handle.spawn(|_| async { tcp_roundtrip(t).await })`.
async fn tcp_roundtrip(target: SocketAddr) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let timeout = Duration::from_millis(500);
    let mut stream = tokio::time::timeout(timeout, tokio::net::TcpStream::connect(target))
        .await
        .context("tcp connect timeout")?
        .context("tcp connect")?;
    let payload = b"ping";
    tokio::time::timeout(timeout, stream.write_all(payload))
        .await
        .context("tcp write timeout")?
        .context("tcp write")?;
    let mut buf = [0u8; 4];
    tokio::time::timeout(timeout, stream.read_exact(&mut buf))
        .await
        .context("tcp read timeout")?
        .context("tcp read")?;
    if &buf != payload {
        bail!("tcp echo mismatch: {:?}", buf);
    }
    Ok(())
}

// ── Builder-API NAT tests ────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn nat_dest_independent_keeps_port() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let isp = lab.add_router("isp1").region("eu").build().await?;
    let dc = lab.add_router("dc1").region("eu").build().await?;
    let home = lab
        .add_router("home1")
        .upstream(isp.id())
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;
    lab.add_device("dev1")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    // Reflector in DC namespace.
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r1 = SocketAddr::new(IpAddr::V4(dc_ip), 3478);
    dc.spawn_reflector(r1)?;

    // Reflector on IX bridge (lab-root ns).
    let r2 = SocketAddr::new(IpAddr::V4(lab.ix().gw()), 3479);
    let ix = lab.ix();
    ix.spawn_reflector(r2)?;

    tokio::time::sleep(Duration::from_millis(250)).await;

    let dev1 = lab.device_by_name("dev1").unwrap();
    let o1 = dev1.probe_udp_mapping(r1)?;
    let o2 = dev1.probe_udp_mapping(r2)?;

    assert_eq!(o1.observed.ip(), o2.observed.ip(), "external IP differs");
    assert_eq!(
        o1.observed.port(),
        o2.observed.port(),
        "EIM: external port must be stable across destinations",
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn nat_dest_dependent_changes_port() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let isp = lab.add_router("isp1").region("eu").build().await?;
    let dc = lab.add_router("dc1").region("eu").build().await?;
    let home = lab
        .add_router("home1")
        .upstream(isp.id())
        .nat(NatMode::DestinationDependent)
        .build()
        .await?;
    lab.add_device("dev1")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r1 = SocketAddr::new(IpAddr::V4(dc_ip), 4478);
    dc.spawn_reflector(r1)?;

    let r2 = SocketAddr::new(IpAddr::V4(lab.ix().gw()), 4479);
    let ix = lab.ix();
    ix.spawn_reflector(r2)?;

    tokio::time::sleep(Duration::from_millis(250)).await;

    let dev1 = lab.device_by_name("dev1").unwrap();
    let o1 = dev1.probe_udp_mapping(r1)?;
    let o2 = dev1.probe_udp_mapping(r2)?;
    println!("o1 {o1:?}");
    println!("o2 {o2:?}");

    assert_eq!(o1.observed.ip(), o2.observed.ip(), "external IP differs");
    assert_ne!(
        o1.observed.port(),
        o2.observed.port(),
        "EDM: external port must change per destination",
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn cgnat_hides_behind_isp_public_ip() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let isp = lab
        .add_router("isp1")
        .region("eu")
        .nat(NatMode::Cgnat)
        .build()
        .await?;
    let dc = lab.add_router("dc1").region("eu").build().await?;
    let home = lab
        .add_router("home1")
        .upstream(isp.id())
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;
    lab.add_device("dev1")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 5478);
    dc.spawn_reflector(r)?;

    tokio::time::sleep(Duration::from_millis(250)).await;

    let dev1 = lab.device_by_name("dev1").unwrap();
    let o = dev1.probe_udp_mapping(r)?;
    let isp_public = IpAddr::V4(isp.uplink_ip().context("no uplink ip")?);

    assert_eq!(
        o.observed.ip(),
        isp_public,
        "with CGNAT the observed IP must be the ISP's IX IP",
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn iroh_nat_like_nodes_report_public_203_mapped_addrs() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let dc = lab.add_router("dc").region("eu").build().await?;
    let isp = lab
        .add_router("isp")
        .region("eu")
        .nat(NatMode::Cgnat)
        .build()
        .await?;
    let lan_provider = lab
        .add_router("lan-provider")
        .upstream(isp.id())
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;
    let lan_fetcher = lab
        .add_router("lan-fetcher")
        .upstream(isp.id())
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;
    lab.add_device("provider")
        .iface("eth0", lan_provider.id(), None)
        .build()
        .await?;
    lab.add_device("fetcher")
        .iface("eth0", lan_fetcher.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 6478);
    dc.spawn_reflector(reflector)?;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let provider = lab.device_by_name("provider").unwrap();
    let fetcher = lab.device_by_name("fetcher").unwrap();
    let provider_obs = provider.probe_udp_mapping(reflector)?;
    let fetcher_obs = fetcher.probe_udp_mapping(reflector)?;
    let isp_public = isp.uplink_ip().context("no uplink ip")?;

    let provider_ip = match provider_obs.observed.ip() {
        IpAddr::V4(ip) => ip,
        IpAddr::V6(ip) => bail!("expected provider observed IPv4 address, got {ip}"),
    };
    let fetcher_ip = match fetcher_obs.observed.ip() {
        IpAddr::V4(ip) => ip,
        IpAddr::V6(ip) => bail!("expected fetcher observed IPv4 address, got {ip}"),
    };

    assert_eq!(
        provider_ip.octets()[0],
        203,
        "provider STUN report should be public 203.* mapped IP, got {}",
        provider_obs.observed
    );
    assert_eq!(
        fetcher_ip.octets()[0],
        203,
        "fetcher STUN report should be public 203.* mapped IP, got {}",
        fetcher_obs.observed
    );
    assert_eq!(
        provider_ip, isp_public,
        "provider should be mapped behind ISP public address"
    );
    assert_eq!(
        fetcher_ip, isp_public,
        "fetcher should be mapped behind ISP public address"
    );
    assert_ne!(
        provider_obs.observed.port(),
        0,
        "provider mapped port should be non-zero"
    );
    assert_ne!(
        fetcher_obs.observed.port(),
        0,
        "fetcher mapped port should be non-zero"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn load_from_toml() -> Result<()> {
    check_caps()?;
    let toml = r#"
[[router]]
name   = "isp1"
region = "eu"

[[router]]
name   = "dc1"
region = "eu"

[[router]]
name     = "lan1"
upstream = "isp1"
nat      = "destination-independent"

[device.dev1.eth0]
gateway = "lan1"
"#;
    let tmp = std::env::temp_dir().join("netsim_test_lab.toml");
    std::fs::write(&tmp, toml)?;

    let lab = Lab::load(&tmp).await?;
    assert!(lab.device_by_name("dev1").is_some());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn smoke_ping_gateway() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let isp = lab.add_router("isp1").region("eu").build().await?;
    let home = lab
        .add_router("home1")
        .upstream(isp.id())
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;
    let dev = lab
        .add_device("dev1")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    let lan_gw = home.downstream_gw().context("no downstream gw")?;
    let lan_gw_str = lan_gw.to_string();
    dev.run_sync(move || ping(&lan_gw_str))?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn smoke_udp_dc_roundtrip() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let isp = lab.add_router("isp1").region("eu").build().await?;
    let dc = lab.add_router("dc1").region("eu").build().await?;
    let home = lab
        .add_router("home1")
        .upstream(isp.id())
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;
    let dev = lab
        .add_device("dev1")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 3478);
    dc.spawn_reflector(r)?;

    tokio::time::sleep(Duration::from_millis(250)).await;

    let _ = dev.run_sync(move || crate::test_utils::udp_roundtrip(r))?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn smoke_tcp_dc_roundtrip() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let isp = lab.add_router("isp1").region("eu").build().await?;
    let dc = lab.add_router("dc1").region("eu").build().await?;
    let home = lab
        .add_router("home1")
        .upstream(isp.id())
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;
    let dev = lab
        .add_device("dev1")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no uplink ip")?;
    let bind = SocketAddr::new(IpAddr::V4(dc_ip), 9000);
    dc.spawn(move |_| async move { spawn_tcp_echo_in(bind).await })
        .await
        .context("tcp echo task panicked")??;

    tokio::time::sleep(Duration::from_millis(250)).await;

    dev.spawn(move |_| async move { tcp_roundtrip(bind).await })
        .await
        .context("tcp roundtrip task panicked")??;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn smoke_ping_home_to_isp() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let isp = lab.add_router("isp1").region("eu").build().await?;
    let home = lab
        .add_router("home1")
        .upstream(isp.id())
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;

    let isp_wan_ip = isp.downstream_gw().context("no downstream gw")?;
    let isp_wan_str = isp_wan_ip.to_string();
    home.run_sync(move || ping(&isp_wan_str))?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn smoke_ping_isp_to_ix_and_dc() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let isp = lab.add_router("isp1").region("eu").build().await?;
    let dc = lab.add_router("dc1").region("eu").build().await?;

    let ix_gw_str = lab.ix().gw().to_string();
    isp.run_sync(move || ping(&ix_gw_str))?;
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let dc_ip_str = dc_ip.to_string();
    isp.run_sync(move || ping(&dc_ip_str))?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn smoke_nat_homes_can_ping_public_relay_device() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();

    let dc = lab.add_router("dc").build().await?;
    let lan_provider = lab
        .add_router("lan-provider")
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;
    let lan_fetcher = lab
        .add_router("lan-fetcher")
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;

    let relay = lab
        .add_device("relay")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;
    let provider = lab
        .add_device("provider")
        .iface("eth0", lan_provider.id(), None)
        .build()
        .await?;
    let fetcher = lab
        .add_device("fetcher")
        .iface("eth0", lan_fetcher.id(), None)
        .build()
        .await?;

    let relay_ip = relay.ip();
    let relay_ip_str = relay_ip.to_string();
    let relay_ip_str2 = relay_ip_str.clone();
    provider.run_sync(move || ping(&relay_ip_str))?;
    fetcher.run_sync(move || ping(&relay_ip_str2))?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn nat_matrix_public_connectivity_and_reflexive_ip() -> Result<()> {
    check_caps()?;
    let cases = [
        (NatMode::None, UplinkWiring::DirectIx),
        (NatMode::Cgnat, UplinkWiring::DirectIx),
        (NatMode::DestinationIndependent, UplinkWiring::DirectIx),
        (NatMode::DestinationIndependent, UplinkWiring::ViaPublicIsp),
        (NatMode::DestinationIndependent, UplinkWiring::ViaCgnatIsp),
        (NatMode::DestinationDependent, UplinkWiring::DirectIx),
        (NatMode::DestinationDependent, UplinkWiring::ViaPublicIsp),
        (NatMode::DestinationDependent, UplinkWiring::ViaCgnatIsp),
    ];

    let mut case_idx = 0u16;
    for (mode, wiring) in cases {
        let port_base = 10000 + case_idx * 10;
        case_idx = case_idx.saturating_add(1);
        let (lab, _dev_ns, r_dc, _r_ix, expected_ip) =
            build_single_nat_case(mode, wiring, port_base).await?;
        let dev = lab.device_by_name("dev").unwrap();
        let r_dc_ip_str = r_dc.ip().to_string();
        dev.run_sync(move || ping(&r_dc_ip_str))?;
        let _ = dev.run_sync(move || crate::test_utils::udp_roundtrip(r_dc))?;
        let observed = dev.probe_udp_mapping(r_dc)?;
        assert_eq!(
            observed.observed.ip(),
            IpAddr::V4(expected_ip),
            "unexpected reflexive IP for mode={mode:?} wiring={}",
            wiring.label()
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn nat_mapping_port_behavior_by_mode_and_wiring() -> Result<()> {
    check_caps()?;
    let modes = [
        NatMode::DestinationIndependent,
        NatMode::DestinationDependent,
    ];
    let wirings = [
        UplinkWiring::DirectIx,
        UplinkWiring::ViaPublicIsp,
        UplinkWiring::ViaCgnatIsp,
    ];

    let mut case_idx = 0u16;
    for mode in modes {
        for wiring in wirings {
            let port_base = 11000 + case_idx * 10;
            case_idx = case_idx.saturating_add(1);
            let (lab, _dev_ns, r_dc, r_ix, expected_ip) =
                build_single_nat_case(mode, wiring, port_base).await?;
            let dev = lab.device_by_name("dev").unwrap();
            let o1 = dev.probe_udp_mapping(r_dc)?;
            let o2 = dev.probe_udp_mapping(r_ix)?;

            assert_eq!(
                o1.observed.ip(),
                IpAddr::V4(expected_ip),
                "unexpected reflexive IP for mode={mode:?} wiring={}",
                wiring.label()
            );
            assert_eq!(
                o2.observed.ip(),
                IpAddr::V4(expected_ip),
                "unexpected reflexive IP for mode={mode:?} wiring={}",
                wiring.label()
            );

            match mode {
                NatMode::DestinationIndependent => assert_eq!(
                    o1.observed.port(),
                    o2.observed.port(),
                    "expected stable external port for mode={mode:?} wiring={}",
                    wiring.label()
                ),
                NatMode::DestinationDependent => assert_ne!(
                    o1.observed.port(),
                    o2.observed.port(),
                    "expected destination-dependent external port for mode={mode:?} wiring={}",
                    wiring.label()
                ),
                _ => unreachable!("only destination modes are tested here"),
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn nat_private_reachability_isolated_public_reachable() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let dc = lab.add_router("dc").region("eu").build().await?;
    let nat_a = lab
        .add_router("nat-a")
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;
    let nat_b = lab
        .add_router("nat-b")
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;

    let relay = lab
        .add_device("relay")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;
    let a1 = lab
        .add_device("a1")
        .iface("eth0", nat_a.id(), None)
        .build()
        .await?;
    let a2 = lab
        .add_device("a2")
        .iface("eth0", nat_a.id(), None)
        .build()
        .await?;
    let b1 = lab
        .add_device("b1")
        .iface("eth0", nat_b.id(), None)
        .build()
        .await?;

    let a2_ip = a2.ip();
    let b1_ip = b1.ip();
    let a1_ip = a1.ip();
    let relay_ip = relay.ip();

    let a2_ip_str = a2_ip.to_string();
    let b1_ip_str = b1_ip.to_string();
    let a1_ip_str = a1_ip.to_string();
    let relay_ip_str = relay_ip.to_string();
    let relay_ip_str2 = relay_ip_str.clone();

    a1.run_sync(move || ping(&a2_ip_str))?;
    a1.run_sync(move || ping_fails(&b1_ip_str))?;
    b1.run_sync(move || ping_fails(&a1_ip_str))?;

    a1.run_sync(move || ping(&relay_ip_str))?;
    b1.run_sync(move || ping(&relay_ip_str2))?;

    let nat_a_public = nat_a.uplink_ip().context("no uplink ip")?;
    let nat_b_public = nat_b.uplink_ip().context("no uplink ip")?;
    let nat_b_str = nat_b_public.to_string();
    let nat_a_str = nat_a_public.to_string();
    a1.run_sync(move || ping(&nat_b_str))?;
    b1.run_sync(move || ping(&nat_a_str))?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 12000);
    dc.spawn_reflector(reflector)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let a1_map = a1.probe_udp_mapping(reflector)?;
    let a2_map = a2.probe_udp_mapping(reflector)?;
    let b1_map = b1.probe_udp_mapping(reflector)?;
    assert_eq!(a1_map.observed.ip(), IpAddr::V4(nat_a_public));
    assert_eq!(a2_map.observed.ip(), IpAddr::V4(nat_a_public));
    assert_eq!(b1_map.observed.ip(), IpAddr::V4(nat_b_public));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn smoke_device_to_device_same_lan() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let isp = lab.add_router("isp1").region("eu").build().await?;
    let home = lab
        .add_router("home1")
        .upstream(isp.id())
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;
    let dev1 = lab
        .add_device("dev1")
        .iface("eth0", home.id(), None)
        .build()
        .await?;
    let dev2 = lab
        .add_device("dev2")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    let dev2_ip_str = dev2.ip().to_string();
    dev1.run_sync(move || ping(&dev2_ip_str))?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn latency_directional_between_regions() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    lab.set_region_latency("eu", "us", 30);
    lab.set_region_latency("us", "eu", 70);
    let dc_eu = lab.add_router("dc-eu").region("eu").build().await?;
    let dc_us = lab.add_router("dc-us").region("us").build().await?;
    let dev_eu = lab
        .add_device("dev-eu")
        .iface("eth0", dc_eu.id(), None)
        .build()
        .await?;
    let dev_us = lab
        .add_device("dev-us")
        .iface("eth0", dc_us.id(), None)
        .build()
        .await?;

    let dc_us_ip = dc_us.uplink_ip().context("no uplink ip")?;
    let r_us = SocketAddr::new(IpAddr::V4(dc_us_ip), 9010);
    dc_us.spawn_reflector(r_us)?;

    let dc_eu_ip = dc_eu.uplink_ip().context("no uplink ip")?;
    let r_eu = SocketAddr::new(IpAddr::V4(dc_eu_ip), 9011);
    dc_eu.spawn_reflector(r_eu)?;

    tokio::time::sleep(Duration::from_millis(250)).await;

    let rtt_eu_to_us = dev_eu.run_sync(move || crate::test_utils::udp_rtt(r_us))?;
    let rtt_us_to_eu = dev_us.run_sync(move || crate::test_utils::udp_rtt(r_eu))?;
    let expected = Duration::from_millis(100);

    assert!(
        rtt_eu_to_us >= expected - Duration::from_millis(10),
        "expected eu->us RTT >= 90ms, got {rtt_eu_to_us:?}"
    );
    assert!(
        rtt_us_to_eu >= expected - Duration::from_millis(10),
        "expected us->eu RTT >= 90ms, got {rtt_us_to_eu:?}"
    );
    let diff = rtt_eu_to_us.abs_diff(rtt_us_to_eu);
    assert!(
        diff <= Duration::from_millis(20),
        "expected RTTs to be close; eu->us={rtt_eu_to_us:?} us->eu={rtt_us_to_eu:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn latency_inter_region_dc_to_dc() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    lab.set_region_latency("eu", "us", 50);
    lab.set_region_latency("us", "eu", 50);
    let dc_eu = lab.add_router("dc-eu").region("eu").build().await?;
    let dc_us = lab.add_router("dc-us").region("us").build().await?;
    lab.add_device("dev1")
        .iface("eth0", dc_eu.id(), None)
        .build()
        .await?;

    let dc_us_ip = dc_us.uplink_ip().context("no uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_us_ip), 9000);
    dc_us.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let dev = lab.device_by_name("dev1").context("missing dev1")?;
    let rtt = dev.run_sync(move || crate::test_utils::udp_rtt(r))?;
    assert!(
        rtt >= Duration::from_millis(90),
        "expected inter-region RTT >= 90ms, got {rtt:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn latency_device_impair_adds_delay() -> Result<()> {
    check_caps()?;

    async fn measure(impair: Option<Impair>) -> Result<Duration> {
        let lab = Lab::new();
        lab.set_region_latency("eu", "us", 40);
        lab.set_region_latency("us", "eu", 40);
        let dc_eu = lab.add_router("dc-eu").region("eu").build().await?;
        let dc_us = lab.add_router("dc-us").region("us").build().await?;
        lab.add_device("dev1")
            .iface("eth0", dc_eu.id(), impair)
            .build()
            .await?;

        let dc_us_ip = dc_us.uplink_ip().context("no uplink ip")?;
        let r = SocketAddr::new(IpAddr::V4(dc_us_ip), 9001);
        dc_us.spawn_reflector(r)?;
        tokio::time::sleep(Duration::from_millis(250)).await;

        let dev = lab.device_by_name("dev1").context("missing dev1")?;
        dev.run_sync(move || crate::test_utils::udp_rtt(r))
    }

    let base = measure(None).await?;
    let impaired = measure(Some(Impair::Mobile)).await?;
    assert!(
        impaired >= base + Duration::from_millis(30),
        "expected impaired RTT >= base + 30ms, base={base:?} impaired={impaired:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn latency_manual_impair_applies() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let dc_eu = lab.add_router("dc-eu").region("eu").build().await?;
    let dc_us = lab.add_router("dc-us").region("us").build().await?;
    lab.set_region_latency("eu", "us", 20);
    lab.set_region_latency("us", "eu", 20);
    let dev = lab
        .add_device("dev1")
        .iface(
            "eth0",
            dc_eu.id(),
            Some(Impair::Manual {
                rate: 10_000,
                loss: 0.0,
                latency: 60,
            }),
        )
        .build()
        .await?;

    let dc_us_ip = dc_us.uplink_ip().context("no uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_us_ip), 9020);
    dc_us.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let rtt = dev.run_sync(move || crate::test_utils::udp_rtt(r))?;
    assert!(
        rtt >= Duration::from_millis(90),
        "expected manual latency >= 90ms RTT, got {rtt:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn isp_home_wan_pool_selection() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let isp_public = lab.add_router("isp-public").region("eu").build().await?;
    let isp_cgnat = lab
        .add_router("isp-cgnat")
        .region("eu")
        .nat(NatMode::Cgnat)
        .build()
        .await?;
    let home_public = lab
        .add_router("home-public")
        .upstream(isp_public.id())
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;
    let home_cgnat = lab
        .add_router("home-cgnat")
        .upstream(isp_cgnat.id())
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;

    let wan_public = home_public.uplink_ip().context("no uplink ip")?;
    let wan_cgnat = home_cgnat.uplink_ip().context("no uplink ip")?;

    let is_private_10 = |ip: Ipv4Addr| ip.octets()[0] == 10;
    assert!(
        !is_private_10(wan_public),
        "expected public WAN for non-CGNAT home, got {wan_public}"
    );
    assert!(
        is_private_10(wan_cgnat),
        "expected private WAN for CGNAT home, got {wan_cgnat}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn dynamic_set_impair_changes_rtt() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let dc = lab.add_router("dc1").region("eu").build().await?;
    let dev = lab
        .add_device("dev1")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 9100);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let base_rtt = dev.run_sync(move || crate::test_utils::udp_rtt(r))?;

    let dev_handle = lab.device_by_name("dev1").unwrap();
    let default_if = dev_handle.default_iface().name().to_string();
    dev_handle.set_impair(&default_if, Some(Impair::Mobile))?;
    let impaired_rtt = dev.run_sync(move || crate::test_utils::udp_rtt(r))?;
    assert!(
        impaired_rtt >= base_rtt + Duration::from_millis(40),
        "expected impaired RTT >= base + 40ms, base={base_rtt:?} impaired={impaired_rtt:?}"
    );

    dev_handle.set_impair(&default_if, None)?;
    let recovered_rtt = dev.run_sync(move || crate::test_utils::udp_rtt(r))?;
    assert!(
        recovered_rtt < base_rtt + Duration::from_millis(30),
        "expected recovered RTT close to base, base={base_rtt:?} recovered={recovered_rtt:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn dynamic_link_down_up_connectivity() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let dc = lab.add_router("dc1").region("eu").build().await?;
    let dev = lab
        .add_device("dev1")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    let gw = dc.downstream_gw().context("no downstream gw")?;
    let gw_str = gw.to_string();

    {
        let gw_str = gw_str.clone();
        dev.run_sync(move || ping(&gw_str))?;
    }

    lab.device_by_name("dev1")
        .unwrap()
        .link_down("eth0")
        .await?;
    {
        let gw_str = gw_str.clone();
        let result = dev.run_sync(move || ping(&gw_str));
        assert!(result.is_err(), "expected ping to fail after link_down");
    }

    lab.device_by_name("dev1").unwrap().link_up("eth0").await?;
    dev.run_sync(move || ping(&gw_str))?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn dynamic_switch_route_changes_path() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let dc = lab.add_router("dc1").region("eu").build().await?;
    let isp = lab.add_router("isp1").region("eu").build().await?;
    let dev = lab
        .add_device("dev1")
        .iface("eth0", dc.id(), None)
        .iface("eth1", isp.id(), Some(Impair::Mobile))
        .default_via("eth0")
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 9200);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let fast_rtt = dev.run_sync(move || crate::test_utils::udp_rtt(r))?;

    lab.device_by_name("dev1")
        .unwrap()
        .switch_route("eth1")
        .await?;
    let slow_rtt = dev.run_sync(move || crate::test_utils::udp_rtt(r))?;

    assert!(
        slow_rtt >= fast_rtt + Duration::from_millis(80),
        "expected slow RTT >= fast + 80ms, fast={fast_rtt:?} slow={slow_rtt:?}"
    );
    Ok(())
}

#[test]
fn manual_impair_deserialize() -> Result<()> {
    let cfg = r#"
[[router]]
name = "dc1"
region = "eu"

[device.dev1.eth0]
gateway = "dc1"
impair = { rate = 5000, loss = 1.5, latency = 40 }
"#;
    let parsed: config::LabConfig = toml::from_str(cfg)?;
    let dev1 = parsed.device.get("dev1").context("missing dev1")?;
    let eth0 = dev1.get("eth0").context("missing eth0")?;
    let impair: Impair = eth0
        .get("impair")
        .context("missing impair")?
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| anyhow!("{}", e))?;
    match impair {
        Impair::Manual {
            rate,
            loss,
            latency,
        } => {
            assert_eq!(rate, 5000);
            assert!((loss - 1.5).abs() < f32::EPSILON);
            assert_eq!(latency, 40);
        }
        other => bail!("unexpected impair: {:?}", other),
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn from_config_expands_count_devices() -> Result<()> {
    let cfg = r#"
[[router]]
name = "dc1"

[device.fetcher]
count = 2
default_via = "eth0"

[device.fetcher.eth0]
gateway = "dc1"
"#;
    let parsed: config::LabConfig = toml::from_str(cfg)?;
    let lab = Lab::from_config(parsed).await?;
    assert!(lab.device_by_name("fetcher-0").is_some());
    assert!(lab.device_by_name("fetcher-1").is_some());
    assert!(lab.device_by_name("fetcher").is_none());
    Ok(())
}

// ── 5a: TCP reflector smoke ──────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn tcp_reflector_basic() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 13_000);

    dc.spawn(move |_| async move { spawn_tcp_reflector(r).await })
        .await
        .context("tcp reflector task panicked")??;

    let obs = dev
        .spawn(move |_| async move { probe_tcp(r).await })
        .await
        .context("probe_tcp task panicked")??;
    assert_ne!(obs.observed.port(), 0, "expected non-zero port");
    Ok(())
}

// ── 5b: Reflexive IP — full matrix ───────────────────────────────────

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn reflexive_ip_all_combos() -> Result<()> {
    use strum::IntoEnumIterator;

    // NatMode::None + Via*Isp is skipped: with no NAT the device gets a public
    // IP, but the nat router sits behind an ISP router (not directly on IX),
    // so no return route is installed from DC → device subnet.  DC's reply
    // is dropped and all probes time out.  The meaningful None case is
    // DirectIx where the return route IS set up.
    let combos: Vec<_> = NatMode::iter()
        .flat_map(|m| UplinkWiring::iter().map(move |w| (m, w)))
        .filter(|(m, w)| {
            !(*m == NatMode::None
                && matches!(w, UplinkWiring::ViaPublicIsp | UplinkWiring::ViaCgnatIsp))
        })
        .flat_map(|(m, w)| Proto::iter().map(move |p| (m, w, p)))
        .flat_map(|(m, w, p)| BindMode::iter().map(move |b| (m, w, p, b)))
        .collect();

    let mut port_base = 14_000u16;
    let mut failures = Vec::new();
    for (mode, wiring, proto, bind) in combos {
        let result: Result<()> = async {
            let (_lab, ctx) = build_nat_case(mode, wiring, port_base).await?;
            let obs = probe_reflexive(&ctx.dev, proto, bind, &ctx).await?;
            if obs.observed.ip() != IpAddr::V4(ctx.expected_ip) {
                bail!("expected {} got {}", ctx.expected_ip, obs.observed.ip());
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            let label = format!("{mode}/{wiring}/{proto}/{bind}");
            eprintln!("FAIL {label}: {e:#}");
            failures.push(format!("{label}: {e:#}"));
        }
        port_base += 10;
    }
    if !failures.is_empty() {
        bail!("{} combos failed:\n{}", failures.len(), failures.join("\n"));
    }
    Ok(())
}

// ── 5c: Port mapping behavior ────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn port_mapping_eim_stable() -> Result<()> {
    use strum::IntoEnumIterator;
    let mut port_base = 16_000u16;
    let mut failures = Vec::new();
    for wiring in UplinkWiring::iter() {
        let result: Result<()> = async {
            let (lab, ctx) =
                build_nat_case(NatMode::DestinationIndependent, wiring, port_base).await?;
            let dev = lab.device_by_name("dev").unwrap();
            let o1 = dev.probe_udp_mapping(ctx.r_dc)?;
            let o2 = dev.probe_udp_mapping(ctx.r_ix)?;
            if o1.observed.port() != o2.observed.port() {
                bail!(
                    "EIM: external port changed: r_dc={} r_ix={}",
                    o1.observed.port(),
                    o2.observed.port()
                );
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            failures.push(format!("DestIndep/{wiring}: {e:#}"));
        }
        port_base += 10;
    }
    if !failures.is_empty() {
        bail!("{} combos failed:\n{}", failures.len(), failures.join("\n"));
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn port_mapping_edm_changes() -> Result<()> {
    use strum::IntoEnumIterator;
    let mut port_base = 16_100u16;
    let mut failures = Vec::new();
    for wiring in UplinkWiring::iter() {
        let result: Result<()> = async {
            let (lab, ctx) =
                build_nat_case(NatMode::DestinationDependent, wiring, port_base).await?;
            let dev = lab.device_by_name("dev").unwrap();
            let o1 = dev.probe_udp_mapping(ctx.r_dc)?;
            let o2 = dev.probe_udp_mapping(ctx.r_ix)?;
            if o1.observed.port() == o2.observed.port() {
                bail!(
                    "EDM: external port must change: r_dc={} r_ix={}",
                    o1.observed.port(),
                    o2.observed.port()
                );
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            failures.push(format!("DestDep/{wiring}: {e:#}"));
        }
        port_base += 10;
    }
    if !failures.is_empty() {
        bail!("{} combos failed:\n{}", failures.len(), failures.join("\n"));
    }
    Ok(())
}

// ── 5d: Route switching + reflexive IP ───────────────────────────────

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn switch_route_reflexive_ip() -> Result<()> {
    use strum::IntoEnumIterator;
    let DualNatLab {
        _lab: _,
        dev,
        nat_a,
        nat_b,
        reflector,
        dc: _,
    } = build_dual_nat_lab(
        NatMode::DestinationIndependent,
        NatMode::DestinationDependent,
        16_200,
    )
    .await?;

    let wan_a = nat_a.uplink_ip().context("no uplink ip")?;
    let wan_b = nat_b.uplink_ip().context("no uplink ip")?;

    let mut failures = Vec::new();
    for proto in Proto::iter() {
        for bind in BindMode::iter() {
            // SpecificIp must use the IP of the currently-active interface;
            // device_ip() returns the default_via interface IP, which changes on switch_route.
            let dev_ip = dev.ip();
            let obs = probe_reflexive_addr(&dev, proto, bind, dev_ip, reflector).await;
            match obs {
                Ok(o) if o.observed.ip() == IpAddr::V4(wan_a) => {}
                Ok(o) => failures.push(format!(
                    "{proto}/{bind} before switch: expected {wan_a} got {}",
                    o.observed.ip()
                )),
                Err(e) => failures.push(format!("{proto}/{bind} before switch: {e:#}")),
            }

            dev.switch_route("eth1").await?;
            tokio::time::sleep(Duration::from_millis(50)).await;

            let dev_ip = dev.ip();
            let obs = probe_reflexive_addr(&dev, proto, bind, dev_ip, reflector).await;
            match obs {
                Ok(o) if o.observed.ip() == IpAddr::V4(wan_b) => {}
                Ok(o) => failures.push(format!(
                    "{proto}/{bind} after switch: expected {wan_b} got {}",
                    o.observed.ip()
                )),
                Err(e) => failures.push(format!("{proto}/{bind} after switch: {e:#}")),
            }

            dev.switch_route("eth0").await?;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    if !failures.is_empty() {
        bail!("{} failures:\n{}", failures.len(), failures.join("\n"));
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn switch_route_multiple() -> Result<()> {
    let DualNatLab {
        _lab: _,
        dc: _,
        dev,
        nat_a,
        nat_b,
        reflector,
    } = build_dual_nat_lab(
        NatMode::DestinationIndependent,
        NatMode::DestinationIndependent,
        16_300,
    )
    .await?;

    let wan_a = nat_a.uplink_ip().context("no uplink ip")?;
    let wan_b = nat_b.uplink_ip().context("no uplink ip")?;

    let o = dev.run_sync(move || crate::test_utils::udp_roundtrip(reflector))?;
    assert_eq!(
        o.observed.ip(),
        IpAddr::V4(wan_a),
        "expected nat_a WAN on eth0"
    );

    dev.switch_route("eth1").await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let o = dev.run_sync(move || crate::test_utils::udp_roundtrip(reflector))?;
    assert_eq!(
        o.observed.ip(),
        IpAddr::V4(wan_b),
        "expected nat_b WAN on eth1"
    );

    dev.switch_route("eth0").await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let o = dev.run_sync(move || crate::test_utils::udp_roundtrip(reflector))?;
    assert_eq!(
        o.observed.ip(),
        IpAddr::V4(wan_a),
        "expected nat_a WAN after switch back"
    );

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn switch_route_tcp_roundtrip() -> Result<()> {
    let DualNatLab {
        _lab: _,
        dc,
        dev,
        nat_a: _,
        nat_b: _,
        reflector: _,
    } = build_dual_nat_lab(
        NatMode::DestinationIndependent,
        NatMode::DestinationDependent,
        16_400,
    )
    .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    let r = SocketAddr::new(IpAddr::V4(dc_ip), 16_410);
    dc.spawn(move |_| async move { spawn_tcp_echo_server(r).await })
        .await
        .context("tcp echo server task panicked")??;
    tokio::time::sleep(Duration::from_millis(200)).await;
    dev.spawn(move |_| async move { tcp_roundtrip(r).await })
        .await
        .context("tcp roundtrip task panicked")??;

    dev.switch_route("eth1").await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    dev.spawn(move |_| async move { tcp_roundtrip(r).await })
        .await
        .context("tcp roundtrip task panicked")??;

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn switch_route_udp_reflexive_change() -> Result<()> {
    let DualNatLab {
        _lab: _,
        dc: _,
        dev,
        nat_a,
        nat_b,
        reflector,
    } = build_dual_nat_lab(
        NatMode::DestinationIndependent,
        NatMode::DestinationIndependent,
        16_500,
    )
    .await?;

    let wan_a = nat_a.uplink_ip().context("no uplink ip")?;
    let wan_b = nat_b.uplink_ip().context("no uplink ip")?;

    let before = dev.run_sync(move || crate::test_utils::udp_roundtrip(reflector))?;
    assert_eq!(
        before.observed.ip(),
        IpAddr::V4(wan_a),
        "before switch: expected nat_a WAN"
    );

    dev.switch_route("eth1").await?;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let after = dev.run_sync(move || crate::test_utils::udp_roundtrip(reflector))?;
    assert_eq!(
        after.observed.ip(),
        IpAddr::V4(wan_b),
        "after switch: expected nat_b WAN"
    );
    assert_ne!(
        before.observed.ip(),
        after.observed.ip(),
        "reflexive IP must change after route switch"
    );
    Ok(())
}

// ── 5d2: Uplink switching (switch_uplink) ────────────────────────────

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn switch_uplink_udp_smoke() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let nat_a = lab
        .add_router("nat-a")
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;
    let nat_b = lab
        .add_router("nat-b")
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", nat_a.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 17_100);
    dc.spawn_reflector(reflector)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Connectivity through nat_a works.
    dev.run_sync(move || crate::test_utils::udp_roundtrip(reflector))
        .context("udp before switch_uplink")?;

    // Move eth0 from nat_a → nat_b.
    dev.switch_uplink("eth0", nat_b.id()).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connectivity through nat_b works.
    dev.run_sync(move || crate::test_utils::udp_roundtrip(reflector))
        .context("udp after switch_uplink")?;

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn switch_uplink_reflexive_ip_changes() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let nat_a = lab
        .add_router("nat-a")
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;
    let nat_b = lab
        .add_router("nat-b")
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", nat_a.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 17_200);
    dc.spawn_reflector(reflector)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let wan_a = nat_a.uplink_ip().context("no nat_a uplink ip")?;
    let wan_b = nat_b.uplink_ip().context("no nat_b uplink ip")?;

    let before = dev.run_sync(move || crate::test_utils::udp_roundtrip(reflector))?;
    assert_eq!(
        before.observed.ip(),
        IpAddr::V4(wan_a),
        "before switch: expected nat_a WAN IP"
    );

    dev.switch_uplink("eth0", nat_b.id()).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let after = dev.run_sync(move || crate::test_utils::udp_roundtrip(reflector))?;
    assert_eq!(
        after.observed.ip(),
        IpAddr::V4(wan_b),
        "after switch: expected nat_b WAN IP"
    );
    assert_ne!(
        before.observed.ip(),
        after.observed.ip(),
        "reflexive IP must change after uplink switch"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn custom_downstream_cidr() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let custom = lab
        .add_router("custom")
        .nat(NatMode::DestinationIndependent)
        .downstream_cidr("172.30.99.0/24".parse()?)
        .build()
        .await?;

    // Router's downstream gateway should be .1 of the custom CIDR.
    assert_eq!(
        custom.downstream_gw(),
        Some(Ipv4Addr::new(172, 30, 99, 1)),
        "router gateway should be 172.30.99.1"
    );
    assert_eq!(
        custom.downstream_cidr().unwrap().to_string(),
        "172.30.99.0/24",
    );

    // Device gets .2 from the custom subnet.
    let dev = lab
        .add_device("dev")
        .iface("eth0", custom.id(), None)
        .build()
        .await?;
    assert_eq!(
        dev.ip(),
        Ipv4Addr::new(172, 30, 99, 2),
        "first device should get 172.30.99.2"
    );

    // Verify connectivity through the custom subnet.
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 17_300);
    dc.spawn_reflector(reflector)?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    dev.run_sync(move || crate::test_utils::udp_roundtrip(reflector))
        .context("udp roundtrip through custom cidr")?;

    Ok(())
}

// ── 5e: Link down/up ─────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn link_down_up_connectivity() -> Result<()> {
    use strum::IntoEnumIterator;
    let mut port_base = 16_600u16;
    let mut failures = Vec::new();
    for proto in Proto::iter() {
        let result: Result<()> = async {
            let lab = Lab::new();
            let dc = lab.add_router("dc").build().await?;
            let dev = lab
                .add_device("dev")
                .iface("eth0", dc.id(), None)
                .build()
                .await?;

            let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
            let r = SocketAddr::new(IpAddr::V4(dc_ip), port_base);
            let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);

            let dev_handle = lab.device_by_name("dev").unwrap();
            match proto {
                Proto::Udp => {
                    dc.spawn_reflector(r)?;
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    dev.run_sync(move || probe_udp_from(r, bind))
                        .context("before link_down")?;
                    dev_handle.link_down("eth0").await?;
                    if dev.run_sync(move || probe_udp_from(r, bind)).is_ok() {
                        bail!("probe should fail after link_down");
                    }
                    dev_handle.link_up("eth0").await?;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    dev.run_sync(move || probe_udp_from(r, bind))
                        .context("after link_up")?;
                }
                Proto::Tcp => {
                    // Persistent echo server: handles all connections for the whole test.
                    dc.spawn(move |_| async move { spawn_tcp_echo_server(r).await })
                        .await
                        .context("tcp echo server task panicked")??;
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    dev.spawn(move |_| async move { tcp_roundtrip(r).await })
                        .await
                        .context("tcp roundtrip panicked")?
                        .context("before link_down")?;
                    dev_handle.link_down("eth0").await?;
                    if dev
                        .spawn(move |_| async move { tcp_roundtrip(r).await })
                        .await
                        .map(|r| r.is_ok())
                        .unwrap_or(false)
                    {
                        bail!("tcp should fail after link_down");
                    }
                    dev_handle.link_up("eth0").await?;
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    dev.spawn(move |_| async move { tcp_roundtrip(r).await })
                        .await
                        .context("tcp roundtrip panicked")?
                        .context("after link_up")?;
                }
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            failures.push(format!("{proto}: {e:#}"));
        }
        port_base += 10;
    }
    if !failures.is_empty() {
        bail!("{} failures:\n{}", failures.len(), failures.join("\n"));
    }
    Ok(())
}

// ── 5f: NAT rebinding ────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn nat_rebind_mode_port() -> Result<()> {
    // DestIndep→DestDep: port changes; DestDep→DestIndep: port stabilises.
    let cases: &[(NatMode, NatMode, bool)] = &[
        (
            NatMode::DestinationIndependent,
            NatMode::DestinationDependent,
            false,
        ),
        (
            NatMode::DestinationDependent,
            NatMode::DestinationIndependent,
            true,
        ),
    ];
    let mut port_base = 16_800u16;
    let mut failures = Vec::new();
    for &(from, to, expect_stable) in cases {
        let result: Result<()> = async {
            let (lab, ctx) = build_nat_case(from, UplinkWiring::DirectIx, port_base).await?;
            let nat_handle = lab.router_by_name("nat").context("missing nat")?;
            nat_handle.set_nat_mode(to)?;
            tokio::time::sleep(Duration::from_millis(50)).await;
            let dev = lab.device_by_name("dev").unwrap();
            let o1 = dev.probe_udp_mapping(ctx.r_dc)?;
            let o2 = dev.probe_udp_mapping(ctx.r_ix)?;
            let port_stable = o1.observed.port() == o2.observed.port();
            if port_stable != expect_stable {
                bail!(
                    "{from}→{to}: expected stable={expect_stable} got stable={port_stable} \
                         (r_dc port={}, r_ix port={})",
                    o1.observed.port(),
                    o2.observed.port()
                );
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            failures.push(format!("{from}→{to}: {e:#}"));
        }
        port_base += 10;
    }
    if !failures.is_empty() {
        bail!("{} failures:\n{}", failures.len(), failures.join("\n"));
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn nat_rebind_mode_ip() -> Result<()> {
    // DestinationIndependent→None is omitted: with NAT=None, the device's
    // private IP appears as the packet source; the DC has no return route, so
    // the UDP probe times out rather than completing.
    let cases: &[(NatMode, NatMode)] = &[(NatMode::None, NatMode::DestinationIndependent)];
    let mut port_base = 16_900u16;
    let mut failures = Vec::new();
    for &(from, to) in cases {
        let result: Result<()> = async {
            let (lab, ctx) = build_nat_case(from, UplinkWiring::DirectIx, port_base).await?;
            let nat_handle = lab.router_by_name("nat").context("missing nat")?;
            let wan_ip = nat_handle.uplink_ip().context("no uplink ip")?;
            nat_handle.set_nat_mode(to)?;
            tokio::time::sleep(Duration::from_millis(50)).await;
            let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
            let r_dc = ctx.r_dc;
            let o = ctx.dev.run_sync(move || probe_udp_from(r_dc, bind))?;
            let expected = match to {
                NatMode::DestinationIndependent => IpAddr::V4(wan_ip),
                NatMode::None => IpAddr::V4(ctx.dev_ip),
                _ => unreachable!(),
            };
            if o.observed.ip() != expected {
                bail!("{from}→{to}: expected {expected} got {}", o.observed.ip());
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            failures.push(format!("{from}→{to}: {e:#}"));
        }
        port_base += 10;
    }
    if !failures.is_empty() {
        bail!("{} failures:\n{}", failures.len(), failures.join("\n"));
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn nat_rebind_conntrack_flush() -> Result<()> {
    // Skip if conntrack-tools is not installed.
    if std::process::Command::new("conntrack")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping nat_rebind_conntrack_flush: conntrack not found");
        return Ok(());
    }
    let (lab, ctx) = build_nat_case(
        NatMode::DestinationDependent,
        UplinkWiring::DirectIx,
        17_000,
    )
    .await?;
    let nat_handle = lab.router_by_name("nat").context("missing nat")?;
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
    let r_dc = ctx.r_dc;
    let o1 = ctx.dev.run_sync(move || probe_udp_from(r_dc, bind))?;
    nat_handle.rebind_nats()?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let o2 = ctx.dev.run_sync(move || probe_udp_from(r_dc, bind))?;
    assert_ne!(
        o1.observed.port(),
        o2.observed.port(),
        "expected new external port after conntrack flush"
    );
    Ok(())
}

// ── 5g: Multi-device cross-NAT ───────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn devices_same_nat_share_ip() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let nat = lab
        .add_router("nat")
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;
    let dev_a = lab
        .add_device("dev-a")
        .iface("eth0", nat.id(), None)
        .build()
        .await?;
    let dev_b = lab
        .add_device("dev-b")
        .iface("eth0", nat.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 17_100);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let oa = dev_a.run_sync(move || crate::test_utils::udp_roundtrip(r))?;
    let ob = dev_b.run_sync(move || crate::test_utils::udp_roundtrip(r))?;
    assert_eq!(
        oa.observed.ip(),
        ob.observed.ip(),
        "devices behind the same NAT must share the same external IP"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn devices_diff_nat_isolate() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let nat_a = lab
        .add_router("nat-a")
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;
    let nat_b = lab
        .add_router("nat-b")
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;
    let dev_a = lab
        .add_device("dev-a")
        .iface("eth0", nat_a.id(), None)
        .build()
        .await?;
    let dev_b = lab
        .add_device("dev-b")
        .iface("eth0", nat_b.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 17_200);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let ip_a = dev_a.ip();
    let ip_b = dev_b.ip();
    let ip_a_str = ip_a.to_string();
    let ip_b_str = ip_b.to_string();
    let dc_ip_str = dc_ip.to_string();
    let dc_ip_str2 = dc_ip_str.clone();

    dev_a.run_sync(move || ping_fails(&ip_b_str))?;
    dev_b.run_sync(move || ping_fails(&ip_a_str))?;
    dev_a.run_sync(move || ping(&dc_ip_str))?;
    dev_b.run_sync(move || ping(&dc_ip_str2))?;

    let oa = dev_a.run_sync(move || crate::test_utils::udp_roundtrip(r))?;
    let ob = dev_b.run_sync(move || crate::test_utils::udp_roundtrip(r))?;
    assert_ne!(
        oa.observed.ip(),
        ob.observed.ip(),
        "devices behind different NATs must have different external IPs"
    );
    Ok(())
}

// ── 5h: Hairpinning — TODO ───────────────────────────────────────────
// Implementing ct-dnat-based hairpin in nftables requires per-port DNAT
// rules derived from the live conntrack table. Deferred.

// ── 5i: Rate limiting ────────────────────────────────────────────────

fn join_sink(join: thread::JoinHandle<Result<()>>) -> Result<()> {
    join.join()
        .map_err(|_| anyhow!("tcp sink thread panicked"))?
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn rate_limit_tcp_upload() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc.id(),
            Some(Impair::Manual {
                rate: 2000,
                loss: 0.0,
                latency: 0,
            }),
        )
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let addr = SocketAddr::new(IpAddr::V4(dc_ip), 17_300);

    let sink = dc.spawn_thread(move || tcp_sink(addr))?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_elapsed, kbps) = dev.run_sync(move || tcp_measure_throughput(addr, 256 * 1024))?;
    join_sink(sink)?;

    assert!(kbps >= 1400, "expected ≥ 1400 kbit/s, got {kbps}");
    assert!(kbps <= 3000, "expected ≤ 3000 kbit/s, got {kbps}");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn rate_limit_tcp_download() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let dev_id = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    dc.impair_downlink(Some(Impair::Manual {
        rate: 2000,
        loss: 0.0,
        latency: 0,
    }))?;

    let dev_ip = dev_id.ip();
    let addr = SocketAddr::new(IpAddr::V4(dev_ip), 17_400);

    // Client (DC) writes to server (device) — bytes travel the download path.
    let sink = dev_id.spawn_thread(move || tcp_sink(addr))?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_elapsed, kbps) = dc.run_sync(move || tcp_measure_throughput(addr, 256 * 1024))?;
    join_sink(sink)?;

    assert!(kbps >= 1400, "expected ≥ 1400 kbit/s, got {kbps}");
    assert!(kbps <= 3000, "expected ≤ 3000 kbit/s, got {kbps}");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn rate_limit_udp_upload() -> Result<()> {
    use std::time::Instant;
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc.id(),
            Some(Impair::Manual {
                rate: 2000,
                loss: 0.0,
                latency: 0,
            }),
        )
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 17_500);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // ~300 KB at 2 Mbit/s ≈ 1.2 s.
    let start = Instant::now();
    dev.run_sync(move || udp_send_recv_count(r, 300, 1024, Duration::from_secs(5)))?;
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1000),
        "expected ≥ 1.0 s for 300 KB at 2 Mbit/s, got {elapsed:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn rate_limit_udp_download() -> Result<()> {
    use std::time::Instant;
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let dev_id = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    dc.impair_downlink(Some(Impair::Manual {
        rate: 2000,
        loss: 0.0,
        latency: 0,
    }))?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 17_600);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Replies travel the download path (DC → device) which is throttled.
    let start = Instant::now();
    dev_id.run_sync(move || udp_send_recv_count(r, 300, 1024, Duration::from_secs(5)))?;
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1000),
        "expected ≥ 1.0 s for 300 KB download at 2 Mbit/s, got {elapsed:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn rate_limit_asymmetric() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let dev_id = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc.id(),
            Some(Impair::Manual {
                rate: 1000,
                loss: 0.0,
                latency: 0,
            }),
        )
        .build()
        .await?;

    dc.impair_downlink(Some(Impair::Manual {
        rate: 4000,
        loss: 0.0,
        latency: 0,
    }))?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let up_addr = SocketAddr::new(IpAddr::V4(dc_ip), 17_700);
    let dev_ip = dev_id.ip();
    let down_addr = SocketAddr::new(IpAddr::V4(dev_ip), 17_710);

    let sink_up = dc.spawn_thread(move || tcp_sink(up_addr))?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_e, kbps_up) = dev_id.run_sync(move || tcp_measure_throughput(up_addr, 128 * 1024))?;
    join_sink(sink_up)?;

    let sink_down = dev_id.spawn_thread(move || tcp_sink(down_addr))?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_e, kbps_down) = dc.run_sync(move || tcp_measure_throughput(down_addr, 128 * 1024))?;
    join_sink(sink_down)?;

    assert!(
        kbps_up <= 1500,
        "expected upload ≤ 1500 kbit/s, got {kbps_up}"
    );
    assert!(
        kbps_down >= 2000,
        "expected download ≥ 2000 kbit/s, got {kbps_down}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn rate_limit_multihop_bottleneck() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let isp = lab.add_router("isp").build().await?;
    let nat = lab
        .add_router("nat")
        .upstream(isp.id())
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", nat.id(), None)
        .build()
        .await?;

    lab.impair_link(
        nat.id(),
        isp.id(),
        Some(Impair::Manual {
            rate: 1000,
            loss: 0.0,
            latency: 0,
        }),
    )?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let addr = SocketAddr::new(IpAddr::V4(dc_ip), 17_800);

    let sink = dc.spawn_thread(move || tcp_sink(addr))?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_e, kbps) = dev.run_sync(move || tcp_measure_throughput(addr, 128 * 1024))?;
    join_sink(sink)?;

    assert!(
        kbps <= 1500,
        "NAT WAN bottleneck: expected ≤ 1500 kbit/s, got {kbps}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn rate_limit_two_hops_stack() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc.id(),
            Some(Impair::Manual {
                rate: 2000,
                loss: 0.0,
                latency: 0,
            }),
        )
        .build()
        .await?;

    dc.impair_downlink(Some(Impair::Manual {
        rate: 2000,
        loss: 0.0,
        latency: 0,
    }))?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let addr = SocketAddr::new(IpAddr::V4(dc_ip), 17_900);

    let sink = dc.spawn_thread(move || tcp_sink(addr))?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_e, kbps) = dev.run_sync(move || tcp_measure_throughput(addr, 256 * 1024))?;
    join_sink(sink)?;

    // Both hops at 2 Mbit/s → effective rate ≤ 2 Mbit/s.
    assert!(kbps <= 3000, "expected ≤ 3000 kbit/s, got {kbps}");
    Ok(())
}

// ── 5j: Packet loss ──────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn loss_udp_moderate() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc.id(),
            Some(Impair::Manual {
                rate: 0,
                loss: 50.0,
                latency: 0,
            }),
        )
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 18_000);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (_, received) =
        dev.run_sync(move || udp_send_recv_count(r, 100, 64, Duration::from_secs(3)))?;
    assert!(
        received >= 20,
        "expected ≥ 20 received at 50% loss, got {received}"
    );
    assert!(
        received <= 80,
        "expected ≤ 80 received at 50% loss, got {received}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn loss_udp_high() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc.id(),
            Some(Impair::Manual {
                rate: 0,
                loss: 90.0,
                latency: 0,
            }),
        )
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 18_100);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (_, received) =
        dev.run_sync(move || udp_send_recv_count(r, 100, 64, Duration::from_secs(3)))?;
    assert!(
        received <= 30,
        "expected ≤ 30 received at 90% loss, got {received}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn loss_tcp_integrity() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc.id(),
            Some(Impair::Manual {
                rate: 0,
                loss: 5.0,
                latency: 0,
            }),
        )
        .build()
        .await?;

    const BYTES: usize = 200 * 1024;
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let addr = SocketAddr::new(IpAddr::V4(dc_ip), 18_200);

    // Server in DC writes BYTES to client; client counts received bytes.
    let server = dc.spawn_thread(move || {
        let listener = std::net::TcpListener::bind(addr)?;
        let (mut stream, _) = listener.accept()?;
        let data = vec![0xABu8; BYTES];
        stream.write_all(&data)?;
        Ok(())
    })?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let n = dev.run_sync(move || {
        let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        let mut buf = Vec::with_capacity(BYTES);
        stream.read_to_end(&mut buf)?;
        Ok(buf.len())
    })?;

    server
        .join()
        .map_err(|_| anyhow!("server thread panicked"))??;
    assert_eq!(n, BYTES, "TCP must deliver all bytes despite 5% loss");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn loss_udp_both_directions() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc.id(),
            Some(Impair::Manual {
                rate: 0,
                loss: 30.0,
                latency: 0,
            }),
        )
        .build()
        .await?;

    dc.impair_downlink(Some(Impair::Manual {
        rate: 0,
        loss: 30.0,
        latency: 0,
    }))?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 18_300);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Round-trip delivery ≈ (1-0.3)×(1-0.3) = 49 %; expect < 80.
    let (_, received) =
        dev.run_sync(move || udp_send_recv_count(r, 100, 64, Duration::from_secs(3)))?;
    assert!(
        received <= 80,
        "expected < 80 echoes with bidirectional loss, got {received}"
    );
    Ok(())
}

// ── 5k: Latency ──────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
#[traced_test]
#[ignore = "hangs — download-direction impair path needs async worker fix"]
async fn latency_download_direction() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 18_400);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let base = dev.run_sync(move || crate::test_utils::udp_rtt(r))?;

    dc.impair_downlink(Some(Impair::Manual {
        rate: 0,
        loss: 0.0,
        latency: 50,
    }))?;

    let impaired = dev.run_sync(move || crate::test_utils::udp_rtt(r))?;
    assert!(
        impaired >= base + Duration::from_millis(40),
        "expected RTT +40ms after 50ms download latency, base={base:?} impaired={impaired:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn latency_upload_and_download() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc.id(),
            Some(Impair::Manual {
                rate: 0,
                loss: 0.0,
                latency: 20,
            }),
        )
        .build()
        .await?;

    dc.impair_downlink(Some(Impair::Manual {
        rate: 0,
        loss: 0.0,
        latency: 30,
    }))?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 18_500);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Each packet traverses: upload(20ms) + download(30ms) = 50ms one-way → RTT ≥ 100ms.
    let rtt = dev.run_sync(move || crate::test_utils::udp_rtt(r))?;
    assert!(
        rtt >= Duration::from_millis(90),
        "expected RTT ≥ 90ms with 20ms upload + 30ms download, got {rtt:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn latency_device_plus_region() -> Result<()> {
    let lab = Lab::new();
    lab.set_region_latency("eu", "us", 40);
    lab.set_region_latency("us", "eu", 40);
    let dc_eu = lab.add_router("dc-eu").region("eu").build().await?;
    let dc_us = lab.add_router("dc-us").region("us").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc_eu.id(),
            Some(Impair::Manual {
                rate: 0,
                loss: 0.0,
                latency: 30,
            }),
        )
        .build()
        .await?;

    let r_us = SocketAddr::new(
        IpAddr::V4(dc_us.uplink_ip().context("no uplink ip")?),
        18_600,
    );
    let r_eu = SocketAddr::new(
        IpAddr::V4(dc_eu.uplink_ip().context("no uplink ip")?),
        18_601,
    );
    dc_us.spawn_reflector(r_us)?;
    dc_eu.spawn_reflector(r_eu)?;
    tokio::time::sleep(Duration::from_millis(250)).await;

    // eu→us: device(30ms) + region(40ms) = 70ms one-way → RTT ≥ 140ms.
    let rtt_eu_us = dev.run_sync(move || crate::test_utils::udp_rtt(r_us))?;
    assert!(
        rtt_eu_us >= Duration::from_millis(130),
        "expected eu→us RTT ≥ 130ms, got {rtt_eu_us:?}"
    );

    // eu→eu: only device upload impair (30ms egress on dev eth0) → RTT ≈ 30ms.
    // Download path has no qdisc, so we expect at least 25ms to allow slack.
    let rtt_eu_eu = dev.run_sync(move || crate::test_utils::udp_rtt(r_eu))?;
    assert!(
        rtt_eu_eu >= Duration::from_millis(25),
        "expected eu→eu RTT ≥ 25ms (device upload impair only), got {rtt_eu_eu:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn latency_multihop_chain() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let isp = lab.add_router("isp").build().await?;
    let nat = lab
        .add_router("nat")
        .upstream(isp.id())
        .nat(NatMode::DestinationIndependent)
        .build()
        .await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            nat.id(),
            Some(Impair::Manual {
                rate: 0,
                loss: 0.0,
                latency: 20,
            }),
        )
        .build()
        .await?;

    lab.impair_link(
        nat.id(),
        isp.id(),
        Some(Impair::Manual {
            rate: 0,
            loss: 0.0,
            latency: 30,
        }),
    )?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 18_700);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // One-way: device(20ms) + nat WAN(30ms) = 50ms → RTT ≥ 100ms.
    let rtt = dev.run_sync(move || crate::test_utils::udp_rtt(r))?;
    assert!(
        rtt >= Duration::from_millis(90),
        "expected RTT ≥ 90ms for multi-hop chain, got {rtt:?}"
    );
    Ok(())
}

// ── 5l: Dynamic rate and latency changes ─────────────────────────────

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn rate_dynamic_decrease() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc.id(),
            Some(Impair::Manual {
                rate: 5000,
                loss: 0.0,
                latency: 0,
            }),
        )
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    let sink = dc.spawn_thread(move || tcp_sink(SocketAddr::new(IpAddr::V4(dc_ip), 18_800)))?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_e, kbps_fast) = dev.run_sync(move || {
        tcp_measure_throughput(SocketAddr::new(IpAddr::V4(dc_ip), 18_800), 256 * 1024)
    })?;
    join_sink(sink)?;

    let dev_handle = lab.device_by_name("dev").unwrap();
    let default_if = dev_handle.default_iface().name().to_string();
    dev_handle.set_impair(
        &default_if,
        Some(Impair::Manual {
            rate: 500,
            loss: 0.0,
            latency: 0,
        }),
    )?;

    let sink = dc.spawn_thread(move || tcp_sink(SocketAddr::new(IpAddr::V4(dc_ip), 18_801)))?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_e, kbps_slow) = dev.run_sync(move || {
        tcp_measure_throughput(SocketAddr::new(IpAddr::V4(dc_ip), 18_801), 64 * 1024)
    })?;
    join_sink(sink)?;

    assert!(
        kbps_fast >= 3000,
        "expected fast ≥ 3000 kbit/s, got {kbps_fast}"
    );
    assert!(
        kbps_slow <= 700,
        "expected slow ≤ 700 kbit/s, got {kbps_slow}"
    );
    assert!(
        kbps_slow <= kbps_fast / 4,
        "expected slow ≤ fast/4: slow={kbps_slow} fast={kbps_fast}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn rate_dynamic_remove() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc.id(),
            Some(Impair::Manual {
                rate: 1000,
                loss: 0.0,
                latency: 0,
            }),
        )
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    let sink = dc.spawn_thread(move || tcp_sink(SocketAddr::new(IpAddr::V4(dc_ip), 18_900)))?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_e, kbps_throttled) = dev.run_sync(move || {
        tcp_measure_throughput(SocketAddr::new(IpAddr::V4(dc_ip), 18_900), 128 * 1024)
    })?;
    join_sink(sink)?;

    let dev_handle = lab.device_by_name("dev").unwrap();
    let default_if = dev_handle.default_iface().name().to_string();
    dev_handle.set_impair(&default_if, None)?;

    let sink = dc.spawn_thread(move || tcp_sink(SocketAddr::new(IpAddr::V4(dc_ip), 18_901)))?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_e, kbps_free) = dev.run_sync(move || {
        tcp_measure_throughput(SocketAddr::new(IpAddr::V4(dc_ip), 18_901), 256 * 1024)
    })?;
    join_sink(sink)?;

    assert!(
        kbps_free >= kbps_throttled * 3,
        "expected unthrottled ≥ 3× throttled: free={kbps_free} throttled={kbps_throttled}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn latency_dynamic_add_remove() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 19_000);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let baseline = dev.run_sync(move || crate::test_utils::udp_rtt(r))?;

    let dev_handle = lab.device_by_name("dev").unwrap();
    let default_if = dev_handle.default_iface().name().to_string();
    dev_handle.set_impair(
        &default_if,
        Some(Impair::Manual {
            rate: 0,
            loss: 0.0,
            latency: 100,
        }),
    )?;
    let high = dev.run_sync(move || crate::test_utils::udp_rtt(r))?;
    assert!(
        high >= baseline + Duration::from_millis(90),
        "expected RTT +90ms after 100ms impair, baseline={baseline:?} high={high:?}"
    );

    dev_handle.set_impair(&default_if, None)?;
    let recovered = dev.run_sync(move || crate::test_utils::udp_rtt(r))?;
    assert!(
        recovered < baseline + Duration::from_millis(30),
        "expected RTT to recover near baseline, baseline={baseline:?} recovered={recovered:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn rate_presets() -> Result<()> {
    let cases = [
        (Impair::Wifi, 20u64, 0.0f32),
        (Impair::Mobile, 50u64, 1.0f32),
    ];
    let mut port_base = 19_100u16;
    let mut failures = Vec::new();
    for (preset, min_latency_ms, loss_pct) in cases {
        let result: Result<()> = async {
            let lab = Lab::new();
            let dc = lab.add_router("dc").build().await?;
            let dev = lab
                .add_device("dev")
                .iface("eth0", dc.id(), Some(preset))
                .build()
                .await?;

            let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
            let r = SocketAddr::new(IpAddr::V4(dc_ip), port_base);
            dc.spawn_reflector(r)?;
            tokio::time::sleep(Duration::from_millis(200)).await;

            let rtt = dev.run_sync(move || crate::test_utils::udp_rtt(r))?;
            if rtt < Duration::from_millis(min_latency_ms) {
                bail!("preset {preset:?}: expected RTT ≥ {min_latency_ms}ms, got {rtt:?}");
            }
            if loss_pct > 0.0 {
                // 1000 packets: P(zero loss at 1%) ≈ 0.000045 — reliably detects loss.
                let (_, received) =
                    dev.run_sync(move || udp_send_recv_count(r, 1000, 64, Duration::from_secs(5)))?;
                if received == 1000 {
                    bail!(
                        "preset {preset:?}: expected some loss at {loss_pct}%, got {received}/1000"
                    );
                }
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            failures.push(format!("{preset:?}: {e:#}"));
        }
        port_base += 10;
    }
    if !failures.is_empty() {
        bail!("{} failures:\n{}", failures.len(), failures.join("\n"));
    }
    Ok(())
}

// ── DNS entries ─────────────────────────────────────────────────────

/// Lab-wide dns_entry is visible in a spawned command's /etc/hosts.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn dns_entry_visible_in_spawned_cmd() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("dc uplink ip")?;
    lab.dns_entry("myserver.test", IpAddr::V4(dc_ip))?;

    // Spawn `getent hosts myserver.test` and check it resolves.
    let mut cmd = std::process::Command::new("getent");
    cmd.args(["hosts", "myserver.test"]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = dev.spawn_command(cmd)?;
    let output = child.wait_with_output().context("wait getent")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    info!(%stdout, "getent output");
    assert!(
        output.status.success(),
        "getent failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&dc_ip.to_string()),
        "expected {dc_ip} in getent output: {stdout}"
    );
    Ok(())
}

/// Lab-wide dns_entry is visible from two different devices.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn dns_entry_lab_wide() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let dev1 = lab
        .add_device("dev1")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;
    let dev2 = lab
        .add_device("dev2")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    lab.dns_entry("shared.test", IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)))?;

    for dev in [&dev1, &dev2] {
        let mut cmd = std::process::Command::new("getent");
        cmd.args(["hosts", "shared.test"]);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let child = dev.spawn_command(cmd)?;
        let output = child.wait_with_output()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("1.2.3.4"),
            "device should see shared.test: {stdout}"
        );
    }
    Ok(())
}

/// Device-specific dns_entry is only visible to that device.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn dns_entry_device_specific() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let dev1 = lab
        .add_device("dev1")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;
    let dev2 = lab
        .add_device("dev2")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    dev1.dns_entry("secret.test", IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1)))?;

    // dev1 should see it.
    let mut cmd = std::process::Command::new("getent");
    cmd.args(["hosts", "secret.test"]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = dev1.spawn_command(cmd)?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "dev1 should resolve secret.test"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("10.99.0.1"));

    // dev2 should NOT see it (no dns_entry for dev2, so no hosts overlay).
    let mut cmd2 = std::process::Command::new("getent");
    cmd2.args(["hosts", "secret.test"]);
    cmd2.stdout(std::process::Stdio::piped());
    cmd2.stderr(std::process::Stdio::piped());
    let child2 = dev2.spawn_command(cmd2)?;
    let output2 = child2.wait_with_output()?;
    assert!(
        !output2.status.success(),
        "dev2 should NOT resolve secret.test"
    );
    Ok(())
}

/// In-process resolve() returns correct IPs for lab-wide + device entries.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn dns_resolve_in_process() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    let ip1 = IpAddr::V4(Ipv4Addr::new(10, 0, 1, 1));
    let ip2 = IpAddr::V4(Ipv4Addr::new(10, 0, 2, 2));

    lab.dns_entry("global.test", ip1)?;
    dev.dns_entry("local.test", ip2)?;

    // Lab resolve sees global but not device-specific.
    assert_eq!(lab.resolve("global.test"), Some(ip1));
    assert_eq!(lab.resolve("local.test"), None);

    // Device resolve sees both.
    assert_eq!(dev.resolve("global.test"), Some(ip1));
    assert_eq!(dev.resolve("local.test"), Some(ip2));

    // Device-specific shadows global with same name.
    let ip3 = IpAddr::V4(Ipv4Addr::new(10, 0, 3, 3));
    dev.dns_entry("global.test", ip3)?;
    assert_eq!(dev.resolve("global.test"), Some(ip3));
    // Lab still returns the original.
    assert_eq!(lab.resolve("global.test"), Some(ip1));

    Ok(())
}

/// dns_entry added after build is visible in subsequent spawn_command.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn dns_entry_after_build() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    // No entries yet — getent should fail (no hosts overlay injected).
    let mut cmd = std::process::Command::new("getent");
    cmd.args(["hosts", "late.test"]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = dev.spawn_command(cmd)?;
    let output = child.wait_with_output()?;
    assert!(!output.status.success(), "should not resolve before dns_entry");

    // Add entry after build.
    lab.dns_entry("late.test", IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)))?;

    // Now it should resolve.
    let mut cmd2 = std::process::Command::new("getent");
    cmd2.args(["hosts", "late.test"]);
    cmd2.stdout(std::process::Stdio::piped());
    cmd2.stderr(std::process::Stdio::piped());
    let child2 = dev.spawn_command(cmd2)?;
    let output2 = child2.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output2.stdout);
    assert!(
        output2.status.success(),
        "should resolve after dns_entry: {}",
        String::from_utf8_lossy(&output2.stderr)
    );
    assert!(stdout.contains("192.168.1.1"));
    Ok(())
}

/// Verify that the generated hosts file content is correct.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn dns_hosts_file_content() -> Result<()> {
    let lab = Lab::new();
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    lab.dns_entry("alpha.test", IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)))?;
    dev.dns_entry("beta.test", IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)))?;

    // Read the hosts file via cat in the device namespace.
    let mut cmd = std::process::Command::new("cat");
    cmd.arg("/etc/hosts");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = dev.spawn_command(cmd)?;
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    info!(%stdout, "hosts file content");

    assert!(stdout.contains("127.0.0.1\tlocalhost"), "should have localhost");
    assert!(stdout.contains("::1\tlocalhost"), "should have ipv6 localhost");
    assert!(stdout.contains("10.0.0.1\talpha.test"), "should have global entry");
    assert!(stdout.contains("10.0.0.2\tbeta.test"), "should have device entry");
    Ok(())
}

// ── IPv6 tests ──────────────────────────────────────────────────────

/// Smoke test: dual-stack DC + device, v6 UDP roundtrip succeeds.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn smoke_dual_stack_roundtrip() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let dc = lab
        .add_router("dc")
        .region("eu")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;
    let dev = lab
        .add_device("dev1")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    // Verify device got both v4 and v6 addresses.
    assert_ne!(
        dev.ip(),
        Ipv4Addr::UNSPECIFIED,
        "device should have v4 addr"
    );
    assert!(dev.ip6().is_some(), "device should have v6 addr");

    // v4 roundtrip
    let dc_ip_v4 = dc.uplink_ip().expect("dc should have v4 uplink");
    let r_v4 = SocketAddr::new(IpAddr::V4(dc_ip_v4), 3480);
    dc.spawn_reflector(r_v4)?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let o_v4 = dev.run_sync(move || crate::test_utils::udp_roundtrip(r_v4))?;
    assert_eq!(
        o_v4.observed.ip(),
        IpAddr::V4(dev.ip()),
        "v4 reflexive should be device IP (no NAT)"
    );

    // v6 roundtrip
    let dc_ip_v6 = dc.uplink_ip_v6().expect("dc should have v6 uplink");
    let r_v6 = SocketAddr::new(IpAddr::V6(dc_ip_v6), 3481);
    dc.spawn_reflector(r_v6)?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let o_v6 = dev.run_sync(move || crate::test_utils::udp_roundtrip(r_v6))?;
    assert!(o_v6.observed.ip().is_ipv6(), "v6 reflexive should be IPv6");

    Ok(())
}

/// Smoke test: v6-only DC + device, v6 roundtrip succeeds, v4 probe fails.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn smoke_v6_only_roundtrip() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let dc = lab
        .add_router("dc")
        .ip_support(IpSupport::V6Only)
        .build()
        .await?;
    let dev = lab
        .add_device("dev1")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    // Device must have v6 but no v4.
    let dev_ip6 = dev.ip6().expect("device should have v6 addr");
    assert!(!dev_ip6.is_unspecified(), "v6 addr must not be unspecified");
    assert_eq!(
        dev.ip(),
        Ipv4Addr::UNSPECIFIED,
        "V6Only device should have no v4 addr"
    );
    assert!(
        dc.uplink_ip().is_none(),
        "V6Only router should have no v4 uplink"
    );

    // v6 roundtrip
    let dc_ip_v6 = dc.uplink_ip_v6().expect("dc v6 uplink");
    let r_v6 = SocketAddr::new(IpAddr::V6(dc_ip_v6), 3490);
    dc.spawn_reflector(r_v6)?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let o = dev.run_sync(move || crate::test_utils::udp_roundtrip(r_v6))?;
    assert!(o.observed.ip().is_ipv6(), "reflexive should be v6");
    Ok(())
}

/// Dual-stack sub-router with v6 masquerade NAT.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn nat_v6_masquerade() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let dc = lab
        .add_router("dc")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;
    let isp = lab
        .add_router("isp")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;
    let home = lab
        .add_router("nat")
        .upstream(isp.id())
        .nat(NatMode::DestinationIndependent)
        .ip_support(IpSupport::DualStack)
        .nat_v6(NatV6Mode::Masquerade)
        .build()
        .await?;
    let dev = lab
        .add_device("dev1")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    // v6 reflector in DC.
    let dc_ip_v6 = dc.uplink_ip_v6().expect("dc v6 uplink");
    let r_v6 = SocketAddr::new(IpAddr::V6(dc_ip_v6), 3500);
    dc.spawn_reflector(r_v6)?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let o = dev.run_sync(move || crate::test_utils::udp_roundtrip(r_v6))?;
    // With masquerade, the reflexive address should be the router's WAN IP.
    let home_wan_v6 = home.uplink_ip_v6().expect("home v6 uplink");
    assert_eq!(
        o.observed.ip(),
        IpAddr::V6(home_wan_v6),
        "v6 masquerade: reflexive should be router WAN IP"
    );
    Ok(())
}

/// Router handle v6 accessors return correct values.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn router_v6_accessors() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let dc = lab
        .add_router("dc")
        .ip_support(IpSupport::DualStack)
        .nat_v6(NatV6Mode::Masquerade)
        .build()
        .await?;

    assert_eq!(dc.ip_support(), IpSupport::DualStack);
    assert_eq!(dc.nat_v6_mode(), NatV6Mode::Masquerade);
    assert!(dc.uplink_ip_v6().is_some(), "should have v6 uplink");
    assert!(
        dc.downstream_cidr_v6().is_some(),
        "should have v6 downstream CIDR"
    );
    assert!(
        dc.downstream_gw_v6().is_some(),
        "should have v6 downstream gw"
    );

    // V4-only router should not have v6 addresses.
    let dc4 = lab.add_router("dc4").build().await?;
    assert_eq!(dc4.ip_support(), IpSupport::V4Only);
    assert!(
        dc4.uplink_ip_v6().is_none(),
        "v4-only should have no v6 uplink"
    );
    assert!(
        dc4.downstream_cidr_v6().is_none(),
        "v4-only should have no v6 downstream"
    );
    Ok(())
}

/// Device handle v6 accessor.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn device_v6_accessors() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let dc = lab
        .add_router("dc")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;
    let dev = lab
        .add_device("dev1")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    assert!(dev.ip6().is_some(), "dual-stack device should have v6");
    let iface = dev.default_iface();
    assert!(iface.ip6().is_some(), "dual-stack iface should have v6");

    // V4-only device
    let dc4 = lab.add_router("dc4").build().await?;
    let dev4 = lab
        .add_device("dev4")
        .iface("eth0", dc4.id(), None)
        .build()
        .await?;
    assert!(dev4.ip6().is_none(), "v4-only device should have no v6");
    Ok(())
}

/// Smoke: v6-only DC + device, v6 roundtrip, v4 ping fails.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn v6_only_no_v4() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let dc = lab
        .add_router("dc")
        .ip_support(IpSupport::V6Only)
        .build()
        .await?;
    let dev = lab
        .add_device("dev1")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    // v6 roundtrip succeeds.
    let dc_ip_v6 = dc.uplink_ip_v6().expect("dc v6 uplink");
    let r_v6 = SocketAddr::new(IpAddr::V6(dc_ip_v6), 3491);
    dc.spawn_reflector(r_v6)?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let o = dev.run_sync(move || crate::test_utils::udp_roundtrip(r_v6))?;
    assert!(o.observed.ip().is_ipv6(), "reflexive should be v6");

    // v4 ping to the IX gateway should fail (no v4 routes).
    let res = dev.run_sync(|| ping("203.0.113.1"));
    assert!(res.is_err(), "v4 ping should fail under V6Only");

    Ok(())
}

/// Dual-stack DC, no NAT: v4 reflexive is v4, v6 reflexive is v6.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn dual_stack_public_addrs() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let dc = lab
        .add_router("dc")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;
    let dev = lab
        .add_device("dev1")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    // v4 reflector
    let dc_ip_v4 = dc.uplink_ip().expect("dc v4 uplink");
    let r_v4 = SocketAddr::new(IpAddr::V4(dc_ip_v4), 3492);
    dc.spawn_reflector(r_v4)?;

    // v6 reflector
    let dc_ip_v6 = dc.uplink_ip_v6().expect("dc v6 uplink");
    let r_v6 = SocketAddr::new(IpAddr::V6(dc_ip_v6), 3493);
    dc.spawn_reflector(r_v6)?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    let o_v4 = dev.run_sync(move || crate::test_utils::udp_roundtrip(r_v4))?;
    assert!(o_v4.observed.ip().is_ipv4(), "v4 reflexive should be v4");

    let o_v6 = dev.run_sync(move || crate::test_utils::udp_roundtrip(r_v6))?;
    assert!(o_v6.observed.ip().is_ipv6(), "v6 reflexive should be v6");

    Ok(())
}

/// NAT v6 none: reflexive = device's global v6 address (no translation).
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn nat_v6_none_global() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    let dc = lab
        .add_router("dc")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;
    let isp = lab
        .add_router("isp")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;
    let home = lab
        .add_router("home")
        .upstream(isp.id())
        .nat(NatMode::DestinationIndependent)
        .ip_support(IpSupport::DualStack)
        .nat_v6(NatV6Mode::None)
        .build()
        .await?;
    let dev = lab
        .add_device("dev1")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    // v6 reflector in DC
    let dc_ip_v6 = dc.uplink_ip_v6().expect("dc v6 uplink");
    let r_v6 = SocketAddr::new(IpAddr::V6(dc_ip_v6), 3494);
    dc.spawn_reflector(r_v6)?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let o_v6 = dev.run_sync(move || crate::test_utils::udp_roundtrip(r_v6))?;
    // No v6 NAT → reflexive ip should be the device's own ULA address.
    let dev_ip6 = dev.ip6().expect("device v6 addr");
    assert_eq!(
        o_v6.observed.ip(),
        IpAddr::V6(dev_ip6),
        "v6 reflexive should be device's own v6 address (no NAT)"
    );

    Ok(())
}

/// V6-only region latency: v6 inter-region RTT includes latency.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn latency_v6_region() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    lab.set_region_latency("eu", "us", 65);
    lab.set_region_latency("us", "eu", 65);

    let dc_eu = lab
        .add_router("dc-eu")
        .region("eu")
        .ip_support(IpSupport::V6Only)
        .build()
        .await?;
    let dc_us = lab
        .add_router("dc-us")
        .region("us")
        .ip_support(IpSupport::V6Only)
        .build()
        .await?;

    // v6 reflector
    let eu_ip_v6 = dc_eu.uplink_ip_v6().expect("eu v6 uplink");
    let r_v6 = SocketAddr::new(IpAddr::V6(eu_ip_v6), 3495);
    dc_eu.spawn_reflector(r_v6)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let rtt_v6 = dc_us.run_sync(move || crate::test_utils::udp_rtt(r_v6))?;
    assert!(
        rtt_v6.as_millis() >= 120,
        "v6 RTT {}ms should be >= 120ms (2x65ms)",
        rtt_v6.as_millis()
    );

    Ok(())
}

/// Dual-stack inter-region latency applies to both v4 and v6.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn latency_dual_stack_region() -> Result<()> {
    check_caps()?;
    let lab = Lab::new();
    lab.set_region_latency("eu", "us", 65);
    lab.set_region_latency("us", "eu", 65);

    let dc_eu = lab
        .add_router("dc-eu")
        .region("eu")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;
    let dc_us = lab
        .add_router("dc-us")
        .region("us")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;

    // v4 reflector
    let eu_ip_v4 = dc_eu.uplink_ip().expect("eu v4 uplink");
    let r_v4 = SocketAddr::new(IpAddr::V4(eu_ip_v4), 3510);
    dc_eu.spawn_reflector(r_v4)?;

    // v6 reflector
    let eu_ip_v6 = dc_eu.uplink_ip_v6().expect("eu v6 uplink");
    let r_v6 = SocketAddr::new(IpAddr::V6(eu_ip_v6), 3511);
    dc_eu.spawn_reflector(r_v6)?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    // v4 RTT
    let rtt_v4 = dc_us.run_sync(move || crate::test_utils::udp_rtt(r_v4))?;
    assert!(
        rtt_v4.as_millis() >= 120,
        "v4 RTT {}ms should be >= 120ms (2×65ms)",
        rtt_v4.as_millis()
    );

    // v6 RTT
    let rtt_v6 = dc_us.run_sync(move || crate::test_utils::udp_rtt(r_v6))?;
    assert!(
        rtt_v6.as_millis() >= 120,
        "v6 RTT {}ms should be >= 120ms (2×65ms)",
        rtt_v6.as_millis()
    );

    Ok(())
}

#[tokio::test]
#[traced_test]
async fn netsim_basic_holepunch() -> Result<()> {
    let lab = Lab::default();
    let nat_mode = NatMode::DestinationIndependent;
    let dc = lab.add_router("dc").build().await?;
    let nat1 = lab.add_router("nat1").nat(nat_mode).build().await?;
    let nat2 = lab.add_router("nat2").nat(nat_mode).build().await?;
    let stun = lab.add_device("stun").uplink(dc.id()).build().await?;
    let dev1 = lab.add_device("dev1").uplink(nat1.id()).build().await?;
    let dev2 = lab.add_device("dev2").uplink(nat2.id()).build().await?;

    let (stun_tx, stun_rx) = oneshot::channel();
    let _task_relay = stun.spawn({
        async move |ctx| {
            let addr = SocketAddr::from((ctx.ip(), 9999));
            ctx.spawn_reflector(addr)?;
            stun_tx.send(addr).unwrap();
            anyhow::Ok(())
        }
    });
    let stun_addr = stun_rx.await.unwrap();

    info!("NOW START");

    let timeout = Duration::from_secs(10);

    // spawn acceptor endpoint on dev1
    let (addr1_tx, addr1_rx) = oneshot::channel();
    let (addr2_tx, addr2_rx) = oneshot::channel();
    let task1 = dev1.spawn({
        async move |_| {
            span_log_timeout("ep1", timeout, async {
                let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
                let public_addr = get_public_addr(&socket, stun_addr).await?;
                info!("src {public_addr}");

                addr1_tx.send(public_addr).unwrap();
                let dst = addr2_rx.await.unwrap();
                info!("got addr1 {dst}");

                send_recv(&socket, dst, Duration::ZERO).await?;
                anyhow::Ok(())
            })
            .await
        }
    });

    // spawn connector endpoint on dev2
    let task2 = dev2.spawn(async move |_| {
        span_log_timeout("ep2", timeout, async {
            let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
            let public_addr = get_public_addr(&socket, stun_addr).await?;
            info!("src {public_addr}");

            addr2_tx.send(public_addr).unwrap();
            let dst = addr1_rx.await.unwrap();
            info!("got addr1 {dst}");

            send_recv(&socket, dst, Duration::from_millis(1000)).await?;

            anyhow::Ok(())
        })
        .await
    });
    tokio::try_join!(async { task1.await.unwrap() }, async {
        task2.await.unwrap()
    },)?;
    Ok(())
}

async fn get_public_addr(socket: &UdpSocket, reflector: SocketAddr) -> Result<SocketAddr> {
    socket.send_to(b"PROBE", reflector).await?;
    let mut buf = [0u8; 256];
    let (n, _) = socket.recv_from(&mut buf).await?;
    let s = std::str::from_utf8(&buf[..n])?;
    let addr_str = s
        .strip_prefix("OBSERVED ")
        .ok_or_else(|| anyhow!("unexpected reflector reply: {:?}", s))?;
    Ok(addr_str.parse()?)
}

async fn send_recv(socket: &UdpSocket, dst: SocketAddr, wait_before_send: Duration) -> Result<()> {
    let send_fut = async {
        // Even with a large delay (e.g. 500ms), fullcone NAT allows this
        // to work — the mapping is populated from the initial STUN probe.
        tokio::time::sleep(wait_before_send).await;
        for i in 0..10 {
            info!("send to {dst} {i}");
            let msg = format!("punch {i}");
            socket.send_to(msg.as_bytes(), dst).await?;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        anyhow::Ok(())
    };

    let recv_fut = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut buf = vec![0u8; 1024];
            let (len, from) = socket.recv_from(&mut buf).await?;
            let msg = std::str::from_utf8(&buf[..len])?;
            info!("recv from {from}: {msg}");
            anyhow::Ok(())
        })
        .await??;
        anyhow::Ok(())
    };
    tokio::try_join!(send_fut, recv_fut)?;
    Ok(())
}

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
