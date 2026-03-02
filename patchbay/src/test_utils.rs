//! Probe and reflector helpers for integration tests.
//!
//! All functions are namespace-free: they assume the calling thread/task is
//! already inside the target network namespace. Callers use
//! `device.spawn(|_| async { ... })` or `device.run_sync(|| ...)` to wrap them.

use std::{
    io::ErrorKind,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket},
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::ObservedAddr;

/// Runs an async UDP reflector. Loops until `cancel` is triggered.
///
/// Spawned via `device.spawn_reflector(bind)` which uses the namespace's
/// tokio runtime.
pub async fn run_reflector(bind: SocketAddr, cancel: CancellationToken) -> Result<()> {
    let sock = tokio::net::UdpSocket::bind(bind).await?;
    let mut buf = [0u8; 512];
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            result = sock.recv_from(&mut buf) => {
                let (_, peer) = result?;
                let msg = format!("OBSERVED {}", peer);
                let _ = sock.send_to(msg.as_bytes(), &peer).await;
            }
        }
    }
    Ok(())
}

/// Sends a UDP probe to `reflector` and returns the observed external address.
///
/// Assumes the calling thread is already in the target namespace.
/// Pass `bind` to specify an explicit local address; `None` uses the unspecified
/// address matching the reflector's address family.
pub fn probe_udp(
    reflector: SocketAddr,
    timeout: Duration,
    bind: Option<SocketAddr>,
) -> Result<ObservedAddr> {
    let bind_addr = bind.unwrap_or_else(|| {
        let unspecified = if reflector.is_ipv4() {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        } else {
            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        };
        SocketAddr::new(unspecified, 0)
    });
    let sock = UdpSocket::bind(bind_addr)?;
    sock.set_read_timeout(Some(timeout))?;
    let mut buf = [0u8; 512];
    for attempt in 1..=3 {
        sock.send_to(b"PROBE", reflector)?;
        match sock.recv_from(&mut buf) {
            Ok((n, _)) => {
                let s = std::str::from_utf8(&buf[..n])?;
                let addr_str = s
                    .strip_prefix("OBSERVED ")
                    .ok_or_else(|| anyhow!("unexpected reflector reply: {:?}", s))?;
                return Ok(addr_str.parse()?);
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                debug!(attempt, "probe timeout waiting for reflector reply");
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(anyhow!("probe timed out after 3 attempts"))
}

/// One-shot UDP roundtrip probe. Returns the observed external address.
pub fn udp_roundtrip(reflector: SocketAddr) -> Result<ObservedAddr> {
    probe_udp(reflector, Duration::from_millis(500), None)
}

/// Returns UDP round-trip time to `reflector`.
pub fn udp_rtt(reflector: SocketAddr) -> Result<Duration> {
    let bind = if reflector.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let sock = UdpSocket::bind(bind)?;
    sock.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut buf = [0u8; 256];
    let start = Instant::now();
    sock.send_to(b"PING", reflector)?;
    let _ = sock.recv_from(&mut buf)?;
    Ok(start.elapsed())
}

/// Async UDP round-trip time measurement.
///
/// Use inside `handle.spawn(|_| async move { udp_rtt_async(r).await })`.
pub async fn udp_rtt_async(reflector: SocketAddr) -> Result<Duration> {
    let bind = SocketAddr::new(
        if reflector.is_ipv4() {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        } else {
            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        },
        0,
    );
    let sock = tokio::net::UdpSocket::bind(bind).await?;
    let mut buf = [0u8; 256];
    let start = Instant::now();
    sock.send_to(b"PING", reflector).await?;
    tokio::time::timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
        .await
        .context("udp_rtt timeout")?
        .context("udp_rtt recv")?;
    Ok(start.elapsed())
}

/// Async UDP send/recv with paced sending for loss measurement.
///
/// Sends `total` packets of `payload` bytes at a paced rate (1ms apart)
/// while concurrently collecting responses. Returns `(sent, received)` after
/// all packets are sent and `wait` has elapsed since the last send.
///
/// Before the main burst, sends warmup probes to confirm the reflector is
/// reachable (retries up to 2 seconds). This prevents false zeros from
/// reflector startup races.
///
/// Use inside `handle.spawn(|_| async move { udp_send_recv_count(r, 1000, 64, dur).await })`.
pub async fn udp_send_recv_count(
    target: SocketAddr,
    total: usize,
    payload: usize,
    wait: Duration,
) -> Result<(usize, usize)> {
    let sock = tokio::net::UdpSocket::bind("0.0.0.0:0")
        .await
        .context("udp bind")?;

    // Warmup: confirm the reflector is live before starting the measured burst.
    // Probes may traverse a lossy link, so we retry aggressively (50ms apart)
    // for up to 5 seconds to handle both reflector startup delay and packet loss.
    let mut warmup_buf = [0u8; 64];
    let warmup_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let _ = sock.send_to(b"WARMUP", target).await;
        match tokio::time::timeout(Duration::from_millis(50), sock.recv_from(&mut warmup_buf)).await
        {
            Ok(Ok(_)) => break,
            _ if tokio::time::Instant::now() >= warmup_deadline => {
                anyhow::bail!("reflector at {target} did not respond within 5s warmup");
            }
            _ => continue,
        }
    }

    let buf = vec![0u8; payload];
    let mut recv_buf = vec![0u8; payload + 64];
    let mut received = 0usize;
    let mut sent = 0usize;

    // Send and receive concurrently so the socket buffer never overflows.
    let mut next_send = tokio::time::Instant::now();
    let mut deadline: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            result = sock.recv_from(&mut recv_buf) => {
                match result {
                    Ok(_) => received += 1,
                    Err(_) => break,
                }
            }
            _ = tokio::time::sleep_until(next_send), if sent < total => {
                let _ = sock.send_to(&buf, target).await;
                sent += 1;
                if sent >= total {
                    deadline = Some(tokio::time::Instant::now() + wait);
                }
                next_send = tokio::time::Instant::now() + Duration::from_millis(1);
            }
            _ = tokio::time::sleep_until(deadline.unwrap_or(next_send)), if deadline.is_some() => {
                break;
            }
        }
    }

    Ok((total, received))
}
