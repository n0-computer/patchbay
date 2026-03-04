//! Tests for UDP hole punching between NATted peers.
//!
//! Both peers discover their public address via a STUN reflector, exchange
//! addresses through a signaling channel, then attempt to establish direct
//! UDP connectivity by simultaneously sending probes through their NATs.

use super::*;

/// FullCone NAT holepunch: one side sends first (creating the mapping),
/// the other follows.  With FullCone (EIM+EIF) the second side's packet is
/// always admitted — no simultaneous open required.
#[tokio::test]
#[traced_test]
async fn fullcone_holepunch() -> Result<()> {
    let lab = Lab::new().await?;
    let nat_mode = Nat::FullCone;
    let dc = lab.add_router("dc").build().await?;
    let nat1 = lab.add_router("nat1").nat(nat_mode).build().await?;
    let nat2 = lab.add_router("nat2").nat(nat_mode).build().await?;
    let stun = lab.add_device("stun").uplink(dc.id()).build().await?;
    let dev1 = lab.add_device("dev1").uplink(nat1.id()).build().await?;
    let dev2 = lab.add_device("dev2").uplink(nat2.id()).build().await?;

    let stun_addr = SocketAddr::from((stun.ip().unwrap(), 9999));
    let _r = stun.spawn_reflector(stun_addr).await?;

    info!("NOW START");

    let timeout = Duration::from_secs(10);

    // spawn acceptor endpoint on dev1
    let (addr1_tx, addr1_rx) = oneshot::channel();
    let (addr2_tx, addr2_rx) = oneshot::channel();
    let task1 = dev1.spawn({
        async move |_| {
            span_log_timeout("ep1", timeout, async {
                let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
                    .await
                    .context("holepunch ep1 udp bind")?;
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
            let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
                .await
                .context("holepunch ep2 udp bind")?;
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
    tokio::try_join!(async { task1?.await.unwrap() }, async {
        task2?.await.unwrap()
    },)?;
    Ok(())
}

/// Home NAT (EIM+APDF) holepunch with simultaneous open.
///
/// Both sides discover their public address via STUN, exchange addresses
/// through a signaling channel, then both start sending simultaneously.
/// With Home NAT (EIM+APDF), both sides must create outbound conntrack
/// entries for each other before either can receive — no unsolicited inbound.
///
/// Also tests a "slightly staggered" variant (200ms delay on one side),
/// simulating real-world timing where simultaneous open is approximate.
/// Home routers typically allow this because the first probe from the faster
/// side creates the conntrack entry before the second side's probe times out.
#[tokio::test]
#[traced_test]
async fn home_nat_holepunch() -> Result<()> {
    check_caps()?;
    for (label, stagger_ms) in [("simultaneous", 0u64), ("staggered-200ms", 200)] {
        info!("--- {label} ---");
        let lab = Lab::new().await?;
        let dc = lab.add_router("dc").build().await?;
        let nat1 = lab.add_router("nat1").nat(Nat::Home).build().await?;
        let nat2 = lab.add_router("nat2").nat(Nat::Home).build().await?;
        let stun = lab.add_device("stun").uplink(dc.id()).build().await?;
        let dev1 = lab.add_device("dev1").uplink(nat1.id()).build().await?;
        let dev2 = lab.add_device("dev2").uplink(nat2.id()).build().await?;

        let stun_addr = SocketAddr::from((stun.ip().unwrap(), 9999));
        let _r = stun.spawn_reflector(stun_addr).await?;

        let timeout = Duration::from_secs(15);
        let stagger = Duration::from_millis(stagger_ms);

        // Use a barrier-style sync: both sides exchange addresses, then
        // a "go" signal ensures ep1 starts immediately while ep2 waits
        // `stagger` ms.
        let (addr1_tx, addr1_rx) = oneshot::channel();
        let (addr2_tx, addr2_rx) = oneshot::channel();

        let task1 = dev1.spawn({
            async move |_| {
                span_log_timeout("ep1", timeout, async {
                    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
                        .await
                        .context("holepunch home ep1 udp bind")?;
                    let public_addr = get_public_addr(&socket, stun_addr).await?;
                    info!("ep1 public {public_addr}");
                    addr1_tx.send(public_addr).unwrap();
                    let dst = addr2_rx.await.unwrap();
                    info!("ep1 peer {dst}");
                    // ep1 sends immediately — creates conntrack for ep2's addr
                    holepunch_send_recv(&socket, dst).await?;
                    anyhow::Ok(())
                })
                .await
            }
        });

        let task2 = dev2.spawn(async move |_| {
            span_log_timeout("ep2", timeout, async {
                let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
                    .await
                    .context("holepunch home ep2 udp bind")?;
                let public_addr = get_public_addr(&socket, stun_addr).await?;
                info!("ep2 public {public_addr}");
                addr2_tx.send(public_addr).unwrap();
                let dst = addr1_rx.await.unwrap();
                info!("ep2 peer {dst}");
                tokio::time::sleep(stagger).await;
                holepunch_send_recv(&socket, dst).await?;
                anyhow::Ok(())
            })
            .await
        });
        tokio::try_join!(async { task1?.await.unwrap() }, async {
            task2?.await.unwrap()
        },)?;
        info!("--- {label} OK ---");
    }
    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Sends probes and waits for a response from the peer. Used for holepunching.
///
/// Sends packets every 200ms for up to 8s, succeeds as soon as one response arrives.
/// After receiving, continues sending probes briefly so the peer also receives.
async fn holepunch_send_recv(socket: &UdpSocket, dst: SocketAddr) -> Result<()> {
    let mut buf = [0u8; 512];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    let mut i = 0u32;
    loop {
        let msg = format!("punch {i}");
        socket.send_to(msg.as_bytes(), dst).await?;
        if i.is_multiple_of(5) {
            info!("sent probe {i} to {dst}");
        }
        i += 1;
        match tokio::time::timeout(Duration::from_millis(200), socket.recv_from(&mut buf)).await {
            Ok(Ok((len, from))) => {
                let msg = std::str::from_utf8(&buf[..len])?;
                info!("recv from {from}: {msg} (after {i} probes)");
                // Send a few more probes so the peer also receives
                for j in 0..3 {
                    let msg = format!("ack {j}");
                    let _ = socket.send_to(msg.as_bytes(), dst).await;
                }
                return Ok(());
            }
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                if tokio::time::Instant::now() > deadline {
                    bail!("holepunch timed out after 8s ({i} probes sent to {dst})");
                }
            }
        }
    }
}

/// Sends a PROBE to the reflector and parses the "OBSERVED <addr>" reply.
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

/// Sends 10 probe packets to `dst` (with optional initial delay) and waits
/// for at least one response to arrive within 5 seconds.
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
