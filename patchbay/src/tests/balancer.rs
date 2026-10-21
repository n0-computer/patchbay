//! L4 load balancer tests.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use anyhow::{Context, Result};
use n0_tracing_test::traced_test;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tracing::debug;

use super::*;
use crate::LbProtocol;

/// Spawns a TCP server that replies with `name` on each accepted connection.
async fn spawn_named_tcp_server(bind: SocketAddr, name: &str) -> Result<()> {
    let name = name.to_string();
    let (ready_tx, ready_rx) = oneshot::channel::<Result<()>>();
    tokio::spawn(async move {
        match TcpListener::bind(bind).await {
            Ok(listener) => {
                let _ = ready_tx.send(Ok(()));
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        break;
                    };
                    let msg = name.clone();
                    let _ = stream.write_all(msg.as_bytes()).await;
                }
            }
            Err(e) => {
                let _ = ready_tx.send(Err(anyhow::anyhow!("tcp bind {bind}: {e}")));
            }
        }
    });
    ready_rx
        .await
        .map_err(|_| anyhow::anyhow!("server task dropped before ready"))?
}

/// Spawns a UDP server that replies with `name` on each received datagram.
async fn spawn_named_udp_server(bind: SocketAddr, name: &str) -> Result<()> {
    let name = name.to_string();
    let (ready_tx, ready_rx) = oneshot::channel::<Result<()>>();
    tokio::spawn(async move {
        match UdpSocket::bind(bind).await {
            Ok(sock) => {
                let _ = ready_tx.send(Ok(()));
                let mut buf = [0u8; 64];
                loop {
                    let Ok((n, peer)) = sock.recv_from(&mut buf).await else {
                        break;
                    };
                    let _ = sock.send_to(name.as_bytes(), peer).await;
                    debug!(recv = n, %peer, "udp echo");
                }
            }
            Err(e) => {
                let _ = ready_tx.send(Err(anyhow::anyhow!("udp bind {bind}: {e}")));
            }
        }
    });
    ready_rx
        .await
        .map_err(|_| anyhow::anyhow!("udp server task dropped before ready"))?
}

/// TCP connect, read reply, return the reply string.
async fn tcp_query(target: SocketAddr) -> Result<String> {
    let timeout = Duration::from_millis(1000);
    let mut stream = tokio::time::timeout(timeout, TcpStream::connect(target))
        .await
        .context("tcp connect timeout")?
        .context("tcp connect")?;
    let mut buf = vec![0u8; 256];
    let n = tokio::time::timeout(timeout, stream.read(&mut buf))
        .await
        .context("tcp read timeout")?
        .context("tcp read")?;
    Ok(String::from_utf8_lossy(&buf[..n]).to_string())
}

/// UDP send/recv, return the reply string.
async fn udp_query(target: SocketAddr) -> Result<String> {
    let timeout = Duration::from_millis(1000);
    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    sock.send_to(b"hello", target).await?;
    let mut buf = [0u8; 256];
    let (n, _) = tokio::time::timeout(timeout, sock.recv_from(&mut buf))
        .await
        .context("udp recv timeout")?
        .context("udp recv")?;
    Ok(String::from_utf8_lossy(&buf[..n]).to_string())
}

/// 2 backends behind a public router. Client on a different router connects
/// to the VIP:port. Verify both backends get connections.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn round_robin_distribution() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    // DC router (public, no NAT) hosts the load balancer.
    let dc = lab
        .add_router("dc")
        .preset(RouterPreset::Public)
        .build()
        .await?;

    // Two backend servers.
    let web1 = lab
        .add_device("web1")
        .iface("eth0", dc.id())
        .build()
        .await?;
    let web2 = lab
        .add_device("web2")
        .iface("eth0", dc.id())
        .build()
        .await?;

    let web1_ip = web1.ip().context("web1 has no ip")?;
    let web2_ip = web2.ip().context("web2 has no ip")?;

    // Start named TCP servers on each backend.
    web1.spawn(move |_| async move {
        spawn_named_tcp_server(SocketAddr::new(IpAddr::V4(web1_ip), 8080), "web1").await
    })?
    .await
    .context("web1 server task panicked")??;

    web2.spawn(move |_| async move {
        spawn_named_tcp_server(SocketAddr::new(IpAddr::V4(web2_ip), 8080), "web2").await
    })?
    .await
    .context("web2 server task panicked")??;

    // Build the balancer.
    let lb = dc
        .add_balancer("web", 80)
        .backend(web1.id(), 8080)
        .backend(web2.id(), 8080)
        .round_robin()
        .build()
        .await?;

    let vip = lb.ip().context("lb has no VIP")?;
    assert_eq!(vip, dc.uplink_ip().unwrap());
    assert_eq!(lb.port(), 80);
    assert_eq!(lb.name(), "web");

    // Client on a separate router.
    let client_router = lab.add_router("client").build().await?;
    let client = lab
        .add_device("client")
        .iface("eth0", client_router.id())
        .build()
        .await?;

    // Make several connections and tally responses.
    let target = SocketAddr::new(IpAddr::V4(vip), 80);
    let mut counts: HashMap<String, usize> = HashMap::new();
    for _ in 0..6 {
        let reply = client
            .spawn(move |_| async move { tcp_query(target).await })?
            .await
            .context("client query panicked")??;
        *counts.entry(reply).or_default() += 1;
    }

    debug!(?counts, "round robin distribution");
    assert!(counts.contains_key("web1"), "web1 never received traffic");
    assert!(counts.contains_key("web2"), "web2 never received traffic");
    Ok(())
}

/// Client behind NAT, backends behind the LB router (private IPs).
/// Verify the client can reach the LB and gets balanced to backends.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn nat_client_to_lb() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    // DC router hosts the LB.
    let dc = lab
        .add_router("dc")
        .preset(RouterPreset::Public)
        .build()
        .await?;

    // Home router with NAT for the client.
    let home = lab.add_router("home").nat(Nat::Home).build().await?;

    // Backend servers behind dc.
    let srv1 = lab
        .add_device("srv1")
        .iface("eth0", dc.id())
        .build()
        .await?;
    let srv2 = lab
        .add_device("srv2")
        .iface("eth0", dc.id())
        .build()
        .await?;

    let srv1_ip = srv1.ip().context("srv1 has no ip")?;
    let srv2_ip = srv2.ip().context("srv2 has no ip")?;

    srv1.spawn(move |_| async move {
        spawn_named_tcp_server(SocketAddr::new(IpAddr::V4(srv1_ip), 9090), "srv1").await
    })?
    .await
    .context("srv1 server task panicked")??;

    srv2.spawn(move |_| async move {
        spawn_named_tcp_server(SocketAddr::new(IpAddr::V4(srv2_ip), 9090), "srv2").await
    })?
    .await
    .context("srv2 server task panicked")??;

    // Build LB on dc.
    let lb = dc
        .add_balancer("api", 443)
        .backend(srv1.id(), 9090)
        .backend(srv2.id(), 9090)
        .build()
        .await?;

    let vip = lb.ip().context("lb has no VIP")?;

    // Client behind NAT.
    let client = lab
        .add_device("client")
        .iface("eth0", home.id())
        .build()
        .await?;

    let target = SocketAddr::new(IpAddr::V4(vip), 443);
    let mut counts: HashMap<String, usize> = HashMap::new();
    for _ in 0..6 {
        let reply = client
            .spawn(move |_| async move { tcp_query(target).await })?
            .await
            .context("client query panicked")??;
        *counts.entry(reply).or_default() += 1;
    }

    debug!(?counts, "nat client to lb distribution");
    assert!(counts.contains_key("srv1"), "srv1 never received traffic");
    assert!(counts.contains_key("srv2"), "srv2 never received traffic");
    Ok(())
}

/// Add/remove backends at runtime, verify redistribution.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn backend_add_remove() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab
        .add_router("dc")
        .preset(RouterPreset::Public)
        .build()
        .await?;

    let web1 = lab
        .add_device("web1")
        .iface("eth0", dc.id())
        .build()
        .await?;
    let web2 = lab
        .add_device("web2")
        .iface("eth0", dc.id())
        .build()
        .await?;
    let web3 = lab
        .add_device("web3")
        .iface("eth0", dc.id())
        .build()
        .await?;

    let web1_ip = web1.ip().context("no ip")?;
    let web2_ip = web2.ip().context("no ip")?;
    let web3_ip = web3.ip().context("no ip")?;

    web1.spawn(move |_| async move {
        spawn_named_tcp_server(SocketAddr::new(IpAddr::V4(web1_ip), 8080), "web1").await
    })?
    .await??;
    web2.spawn(move |_| async move {
        spawn_named_tcp_server(SocketAddr::new(IpAddr::V4(web2_ip), 8080), "web2").await
    })?
    .await??;
    web3.spawn(move |_| async move {
        spawn_named_tcp_server(SocketAddr::new(IpAddr::V4(web3_ip), 8080), "web3").await
    })?
    .await??;

    // Start with 2 backends.
    let lb = dc
        .add_balancer("web", 80)
        .backend(web1.id(), 8080)
        .backend(web2.id(), 8080)
        .build()
        .await?;

    let vip = lb.ip().context("no VIP")?;

    let client_router = lab.add_router("client").build().await?;
    let client = lab
        .add_device("client")
        .iface("eth0", client_router.id())
        .build()
        .await?;

    let target = SocketAddr::new(IpAddr::V4(vip), 80);

    // Verify initial 2-backend distribution.
    let mut counts: HashMap<String, usize> = HashMap::new();
    for _ in 0..4 {
        let reply = client
            .spawn(move |_| async move { tcp_query(target).await })?
            .await??;
        *counts.entry(reply).or_default() += 1;
    }
    assert!(counts.contains_key("web1"));
    assert!(counts.contains_key("web2"));

    // Add web3.
    lb.add_backend(web3.id(), 8080).await?;

    // Flush conntrack to ensure new rules take effect immediately.
    dc.run_sync(|| {
        let _ = std::process::Command::new("conntrack")
            .args(["-F"])
            .status();
        Ok(())
    })?;

    let mut counts: HashMap<String, usize> = HashMap::new();
    for _ in 0..6 {
        let reply = client
            .spawn(move |_| async move { tcp_query(target).await })?
            .await??;
        *counts.entry(reply).or_default() += 1;
    }
    debug!(?counts, "after adding web3");
    assert!(
        counts.contains_key("web3"),
        "web3 should receive traffic after add"
    );

    // Remove web1.
    lb.remove_backend(web1.id()).await?;

    dc.run_sync(|| {
        let _ = std::process::Command::new("conntrack")
            .args(["-F"])
            .status();
        Ok(())
    })?;

    let mut counts: HashMap<String, usize> = HashMap::new();
    for _ in 0..4 {
        let reply = client
            .spawn(move |_| async move { tcp_query(target).await })?
            .await??;
        *counts.entry(reply).or_default() += 1;
    }
    debug!(?counts, "after removing web1");
    assert!(
        !counts.contains_key("web1"),
        "web1 should not receive traffic after removal"
    );

    Ok(())
}

/// UDP round-robin balancing.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn udp_balancing() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab
        .add_router("dc")
        .preset(RouterPreset::Public)
        .build()
        .await?;

    let udp1 = lab
        .add_device("udp1")
        .iface("eth0", dc.id())
        .build()
        .await?;
    let udp2 = lab
        .add_device("udp2")
        .iface("eth0", dc.id())
        .build()
        .await?;

    let udp1_ip = udp1.ip().context("no ip")?;
    let udp2_ip = udp2.ip().context("no ip")?;

    udp1.spawn(move |_| async move {
        spawn_named_udp_server(SocketAddr::new(IpAddr::V4(udp1_ip), 5000), "udp1").await
    })?
    .await??;
    udp2.spawn(move |_| async move {
        spawn_named_udp_server(SocketAddr::new(IpAddr::V4(udp2_ip), 5000), "udp2").await
    })?
    .await??;

    let _lb = dc
        .add_balancer("dns", 53)
        .backend(udp1.id(), 5000)
        .backend(udp2.id(), 5000)
        .protocol(LbProtocol::Udp)
        .build()
        .await?;

    let vip = dc.uplink_ip().context("no VIP")?;

    let client_router = lab.add_router("client").build().await?;
    let client = lab
        .add_device("client")
        .iface("eth0", client_router.id())
        .build()
        .await?;

    let target = SocketAddr::new(IpAddr::V4(vip), 53);
    let mut counts: HashMap<String, usize> = HashMap::new();
    // Use different source ports to get different numgen slots.
    for _ in 0..6 {
        let reply = client
            .spawn(move |_| async move { udp_query(target).await })?
            .await??;
        *counts.entry(reply).or_default() += 1;
    }

    debug!(?counts, "udp distribution");
    assert!(counts.contains_key("udp1"), "udp1 never received traffic");
    assert!(counts.contains_key("udp2"), "udp2 never received traffic");
    Ok(())
}
