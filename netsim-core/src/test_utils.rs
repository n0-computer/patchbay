//! Probe and reflector helpers for integration tests.
//!
//! All functions are namespace-free: they assume the calling thread/task is
//! already inside the target network namespace. Callers use
//! `device.run_sync(|| ...)` or `device.spawn_thread(|| ...)` to wrap them.

use std::{
    io::ErrorKind,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket},
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use tracing::debug;

pub use crate::core::TaskHandle;
use crate::ObservedAddr;

/// Runs a UDP reflector on the current thread (blocking). Loops until `stop_rx`
/// receives a message.
///
/// Typically called via `device.spawn_thread(|| run_reflector(bind))` or
/// wrapped in a `TaskHandle` for controlled shutdown.
pub fn run_reflector(bind: SocketAddr, stop_rx: std::sync::mpsc::Receiver<()>) -> Result<()> {
    let sock = UdpSocket::bind(bind).context("reflector bind")?;
    let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
    let mut buf = [0u8; 512];
    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        match sock.recv_from(&mut buf) {
            Ok((_, peer)) => {
                let msg = format!("OBSERVED {}", peer);
                let _ = sock.send_to(msg.as_bytes(), peer);
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                continue;
            }
            Err(_) => break,
        }
    }
    Ok(())
}

/// Sends a UDP probe to `reflector` and returns the observed external address.
///
/// Assumes the calling thread is already in the target namespace.
pub fn probe_udp(
    reflector: SocketAddr,
    timeout: Duration,
    bind_port: Option<u16>,
) -> Result<ObservedAddr> {
    let unspecified = if reflector.is_ipv4() {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
    };
    let bind_addr = SocketAddr::new(unspecified, bind_port.unwrap_or(0));
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
                return Ok(ObservedAddr {
                    observed: addr_str.parse()?,
                });
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
