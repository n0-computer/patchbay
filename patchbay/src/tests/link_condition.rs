//! Rate limiting, packet loss, latency, presets, and dynamic link changes.

use super::*;

/// Switching default route from clean to impaired path increases RTT.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn route_switch_changes_impairment() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc1").build().await?;
    let isp = lab.add_router("isp1").build().await?;
    let dev = lab
        .add_device("dev1")
        .iface("eth0", dc.id(), None)
        .iface("eth1", isp.id(), Some(LinkCondition::Mobile4G))
        .default_via("eth0")
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 9200);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let fast_rtt = dev.run_sync(move || test_utils::udp_rtt_sync(r))?;

    lab.device_by_name("dev1")
        .unwrap()
        .set_default_route("eth1")
        .await?;
    let slow_rtt = dev.run_sync(move || test_utils::udp_rtt_sync(r))?;

    assert!(
        slow_rtt >= fast_rtt + Duration::from_millis(30),
        "expected slow RTT >= fast + 30ms, fast={fast_rtt:?} slow={slow_rtt:?}"
    );
    Ok(())
}

/// Link down breaks connectivity, link up restores it (UDP and TCP).
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn link_down_up() -> Result<()> {
    use strum::IntoEnumIterator;
    let mut port_base = 16_600u16;
    let mut failures = Vec::new();
    for proto in Proto::iter() {
        let result: Result<()> = async {
            let lab = Lab::new().await?;
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
                    dev.run_sync(move || {
                        test_utils::probe_udp(r, Duration::from_millis(500), Some(bind))
                    })
                    .context("before link_down")?;
                    dev_handle.link_down("eth0").await?;
                    if dev
                        .run_sync(move || {
                            test_utils::probe_udp(r, Duration::from_millis(500), Some(bind))
                        })
                        .is_ok()
                    {
                        bail!("probe should fail after link_down");
                    }
                    dev_handle.link_up("eth0").await?;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    dev.run_sync(move || {
                        test_utils::probe_udp(r, Duration::from_millis(500), Some(bind))
                    })
                    .context("after link_up")?;
                }
                Proto::Tcp => {
                    dc.spawn(move |_| async move { spawn_tcp_echo_server(r).await })?
                        .await
                        .context("tcp echo server task panicked")??;
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    dev.spawn(move |_| async move { tcp_roundtrip(r).await })?
                        .await
                        .context("tcp roundtrip panicked")?
                        .context("before link_down")?;
                    dev_handle.link_down("eth0").await?;
                    if dev
                        .spawn(move |_| async move { tcp_roundtrip(r).await })?
                        .await
                        .map(|r| r.is_ok())
                        .unwrap_or(false)
                    {
                        bail!("tcp should fail after link_down");
                    }
                    dev_handle.link_up("eth0").await?;
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    dev.spawn(move |_| async move { tcp_roundtrip(r).await })?
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

// ── Rate limiting ────────────────────────────────────────────────────

/// 2 Mbit/s upload cap via tc on device interface.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn rate_tcp_upload() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc.id(),
            Some(LinkCondition::Manual(LinkLimits {
                rate_kbit: 2000,
                loss_pct: 0.0,
                latency_ms: 0,
                ..Default::default()
            })),
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

/// 2 Mbit/s download cap via router downlink condition.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn rate_tcp_download() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev_id = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    dc.set_downlink_condition(Some(LinkCondition::Manual(LinkLimits {
        rate_kbit: 2000,
        loss_pct: 0.0,
        latency_ms: 0,
        ..Default::default()
    })))
    .await?;

    let dev_ip = dev_id.ip().unwrap();
    let addr = SocketAddr::new(IpAddr::V4(dev_ip), 17_400);

    let sink = dev_id.spawn_thread(move || tcp_sink(addr))?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_elapsed, kbps) = dc.run_sync(move || tcp_measure_throughput(addr, 256 * 1024))?;
    join_sink(sink)?;

    assert!(kbps >= 1400, "expected ≥ 1400 kbit/s, got {kbps}");
    assert!(kbps <= 3000, "expected ≤ 3000 kbit/s, got {kbps}");
    Ok(())
}

/// 2 Mbit/s upload cap enforced for UDP traffic.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn rate_udp_upload() -> Result<()> {
    use std::time::Instant;
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc.id(),
            Some(LinkCondition::Manual(LinkLimits {
                rate_kbit: 2000,
                loss_pct: 0.0,
                latency_ms: 0,
                ..Default::default()
            })),
        )
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 17_500);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // ~300 KB at 2 Mbit/s ≈ 1.2 s.
    let start = Instant::now();
    dev.spawn(move |_| async move {
        test_utils::udp_send_recv_count(r, 300, 1024, Duration::from_secs(5)).await
    })?
    .await??;
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1000),
        "expected ≥ 1.0 s for 300 KB at 2 Mbit/s, got {elapsed:?}"
    );
    Ok(())
}

/// 2 Mbit/s download cap enforced for UDP replies.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn rate_udp_download() -> Result<()> {
    use std::time::Instant;
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev_id = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    dc.set_downlink_condition(Some(LinkCondition::Manual(LinkLimits {
        rate_kbit: 2000,
        loss_pct: 0.0,
        latency_ms: 0,
        ..Default::default()
    })))
    .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 17_600);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let start = Instant::now();
    dev_id
        .spawn(move |_| async move {
            test_utils::udp_send_recv_count(r, 300, 1024, Duration::from_secs(5)).await
        })?
        .await??;
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1000),
        "expected ≥ 1.0 s for 300 KB download at 2 Mbit/s, got {elapsed:?}"
    );
    Ok(())
}

/// Asymmetric: 1 Mbit/s upload, 4 Mbit/s download.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn rate_asymmetric() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev_id = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc.id(),
            Some(LinkCondition::Manual(LinkLimits {
                rate_kbit: 1000,
                loss_pct: 0.0,
                latency_ms: 0,
                ..Default::default()
            })),
        )
        .build()
        .await?;

    dc.set_downlink_condition(Some(LinkCondition::Manual(LinkLimits {
        rate_kbit: 4000,
        loss_pct: 0.0,
        latency_ms: 0,
        ..Default::default()
    })))
    .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let up_addr = SocketAddr::new(IpAddr::V4(dc_ip), 17_700);
    let dev_ip = dev_id.ip().unwrap();
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

/// NAT WAN link at 1 Mbit/s is the bottleneck for multi-hop traffic.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn rate_multihop_bottleneck() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let isp = lab.add_router("isp").build().await?;
    let nat = lab
        .add_router("nat")
        .upstream(isp.id())
        .nat(Nat::Home)
        .build()
        .await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", nat.id(), None)
        .build()
        .await?;

    lab.set_link_condition(
        nat.id(),
        isp.id(),
        Some(LinkCondition::Manual(LinkLimits {
            rate_kbit: 1000,
            loss_pct: 0.0,
            latency_ms: 0,
            ..Default::default()
        })),
    )
    .await?;

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

/// Both upload and download at 2 Mbit/s — effective rate ≤ 2 Mbit/s.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn rate_two_hops_stacked() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc.id(),
            Some(LinkCondition::Manual(LinkLimits {
                rate_kbit: 2000,
                loss_pct: 0.0,
                latency_ms: 0,
                ..Default::default()
            })),
        )
        .build()
        .await?;

    dc.set_downlink_condition(Some(LinkCondition::Manual(LinkLimits {
        rate_kbit: 2000,
        loss_pct: 0.0,
        latency_ms: 0,
        ..Default::default()
    })))
    .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let addr = SocketAddr::new(IpAddr::V4(dc_ip), 17_900);

    let sink = dc.spawn_thread(move || tcp_sink(addr))?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_e, kbps) = dev.run_sync(move || tcp_measure_throughput(addr, 256 * 1024))?;
    join_sink(sink)?;

    assert!(kbps <= 3000, "expected ≤ 3000 kbit/s, got {kbps}");
    Ok(())
}

// ── Packet loss ──────────────────────────────────────────────────────

/// 50% egress loss drops roughly half the outbound packets.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn loss_udp_moderate() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc.id(),
            Some(LinkCondition::Manual(LinkLimits {
                rate_kbit: 0,
                loss_pct: 50.0,
                latency_ms: 0,
                ..Default::default()
            })),
        )
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 18_000);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // tc netem loss is on the device egress, so ~50% of probes reach the
    // reflector and responses come back unlossed. Wide bounds account for
    // statistical variance.
    let (_, received) = dev
        .spawn(move |_| async move {
            test_utils::udp_send_recv_count(r, 1000, 64, Duration::from_secs(3)).await
        })?
        .await??;
    assert!(
        received >= 100,
        "expected ≥ 100 received at 50% egress loss (of 1000 sent), got {received}"
    );
    assert!(
        received <= 900,
        "expected ≤ 900 received at 50% egress loss (of 1000 sent), got {received}"
    );
    Ok(())
}

/// 90% loss: very few of 100 packets received.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn loss_udp_high() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc.id(),
            Some(LinkCondition::Manual(LinkLimits {
                rate_kbit: 0,
                loss_pct: 90.0,
                latency_ms: 0,
                ..Default::default()
            })),
        )
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 18_100);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (_, received) = dev
        .spawn(move |_| async move {
            test_utils::udp_send_recv_count(r, 100, 64, Duration::from_secs(2)).await
        })?
        .await??;
    assert!(
        received <= 30,
        "expected ≤ 30 received at 90% loss, got {received}"
    );
    Ok(())
}

/// TCP delivers all bytes despite 5% loss (retransmissions).
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn loss_tcp_integrity() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc.id(),
            Some(LinkCondition::Manual(LinkLimits {
                rate_kbit: 0,
                loss_pct: 5.0,
                latency_ms: 0,
                ..Default::default()
            })),
        )
        .build()
        .await?;

    const BYTES: usize = 200 * 1024;
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let addr = SocketAddr::new(IpAddr::V4(dc_ip), 18_200);

    let server = dc.spawn_thread(move || {
        use std::io::Write as _;
        let listener = std::net::TcpListener::bind(addr).context("link_condition tcp bind")?;
        let (mut stream, _) = listener.accept()?;
        let data = vec![0xABu8; BYTES];
        stream.write_all(&data)?;
        Ok(())
    })?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let n = dev.run_sync(move || {
        use std::io::Read as _;
        let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5))
            .context("link_condition tcp connect")?;
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

/// 30% loss on both upload and download.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn loss_udp_bidirectional() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc.id(),
            Some(LinkCondition::Manual(LinkLimits {
                rate_kbit: 0,
                loss_pct: 30.0,
                latency_ms: 0,
                ..Default::default()
            })),
        )
        .build()
        .await?;

    dc.set_downlink_condition(Some(LinkCondition::Manual(LinkLimits {
        rate_kbit: 0,
        loss_pct: 30.0,
        latency_ms: 0,
        ..Default::default()
    })))
    .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 18_300);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Round-trip delivery ≈ (1-0.3)×(1-0.3) = 49 %; expect < 80.
    let (_, received) = dev
        .spawn(move |_| async move {
            test_utils::udp_send_recv_count(r, 100, 64, Duration::from_secs(3)).await
        })?
        .await??;
    assert!(
        received <= 80,
        "expected < 80 echoes with bidirectional loss, got {received}"
    );
    Ok(())
}

// ── Latency ──────────────────────────────────────────────────────────

/// 20ms upload + 30ms download latency stack to ~50ms one-way.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn latency_upload_download() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc.id(),
            Some(LinkCondition::Manual(LinkLimits {
                rate_kbit: 0,
                loss_pct: 0.0,
                latency_ms: 20,
                ..Default::default()
            })),
        )
        .build()
        .await?;

    dc.set_downlink_condition(Some(LinkCondition::Manual(LinkLimits {
        rate_kbit: 0,
        loss_pct: 0.0,
        latency_ms: 30,
        ..Default::default()
    })))
    .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 18_500);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let rtt = dev.run_sync(move || test_utils::udp_rtt_sync(r))?;
    assert!(
        rtt >= Duration::from_millis(90),
        "expected RTT ≥ 90ms with 20ms upload + 30ms download, got {rtt:?}"
    );
    Ok(())
}

/// Device latency (20ms) + NAT WAN latency (30ms) stack in multi-hop chain.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn latency_multihop_chain() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let isp = lab.add_router("isp").build().await?;
    let nat = lab
        .add_router("nat")
        .upstream(isp.id())
        .nat(Nat::Home)
        .build()
        .await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            nat.id(),
            Some(LinkCondition::Manual(LinkLimits {
                rate_kbit: 0,
                loss_pct: 0.0,
                latency_ms: 20,
                ..Default::default()
            })),
        )
        .build()
        .await?;

    lab.set_link_condition(
        nat.id(),
        isp.id(),
        Some(LinkCondition::Manual(LinkLimits {
            rate_kbit: 0,
            loss_pct: 0.0,
            latency_ms: 30,
            ..Default::default()
        })),
    )
    .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 18_700);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let rtt = dev.run_sync(move || test_utils::udp_rtt_sync(r))?;
    assert!(
        rtt >= Duration::from_millis(90),
        "expected RTT ≥ 90ms for multi-hop chain, got {rtt:?}"
    );
    Ok(())
}

// ── Dynamic changes ──────────────────────────────────────────────────

/// Dynamically decrease rate from 5 Mbit/s to 500 kbit/s at runtime.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn rate_dynamic_decrease() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc.id(),
            Some(LinkCondition::Manual(LinkLimits {
                rate_kbit: 5000,
                loss_pct: 0.0,
                latency_ms: 0,
                ..Default::default()
            })),
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
    let default_if = dev_handle.default_iface().unwrap().name().to_string();
    dev_handle
        .set_link_condition(
            &default_if,
            Some(LinkCondition::Manual(LinkLimits {
                rate_kbit: 500,
                loss_pct: 0.0,
                latency_ms: 0,
                ..Default::default()
            })),
        )
        .await?;

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

/// Remove link condition at runtime — throughput increases dramatically.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn rate_dynamic_remove() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc.id(),
            Some(LinkCondition::Manual(LinkLimits {
                rate_kbit: 1000,
                loss_pct: 0.0,
                latency_ms: 0,
                ..Default::default()
            })),
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
    let default_if = dev_handle.default_iface().unwrap().name().to_string();
    dev_handle.set_link_condition(&default_if, None).await?;

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

/// Add 100ms latency at runtime, then remove it — RTT recovers.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn latency_dynamic_add_remove() -> Result<()> {
    let lab = Lab::new().await?;
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

    let baseline = dev.run_sync(move || test_utils::udp_rtt_sync(r))?;

    let dev_handle = lab.device_by_name("dev").unwrap();
    let default_if = dev_handle.default_iface().unwrap().name().to_string();
    dev_handle
        .set_link_condition(
            &default_if,
            Some(LinkCondition::Manual(LinkLimits {
                rate_kbit: 0,
                loss_pct: 0.0,
                latency_ms: 100,
                ..Default::default()
            })),
        )
        .await?;
    let high = dev.run_sync(move || test_utils::udp_rtt_sync(r))?;
    assert!(
        high >= baseline + Duration::from_millis(90),
        "expected RTT +90ms after 100ms impair, baseline={baseline:?} high={high:?}"
    );

    dev_handle.set_link_condition(&default_if, None).await?;
    let recovered = dev.run_sync(move || test_utils::udp_rtt_sync(r))?;
    assert!(
        recovered < baseline + Duration::from_millis(30),
        "expected RTT to recover near baseline, baseline={baseline:?} recovered={recovered:?}"
    );
    Ok(())
}

// ── Presets ──────────────────────────────────────────────────────────

/// Each preset produces expected minimum RTT and loss characteristics.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn presets_rtt_and_loss() -> Result<()> {
    let cases: Vec<(LinkCondition, u64, f32)> = vec![
        (LinkCondition::Lan, 0, 0.0),
        (LinkCondition::Wifi, 5, 0.0),
        (LinkCondition::WifiBad, 40, 2.0),
        (LinkCondition::Mobile4G, 25, 0.0),
        (LinkCondition::Mobile3G, 100, 2.0),
        (LinkCondition::Satellite, 40, 1.0),
        (LinkCondition::SatelliteGeo, 300, 0.0),
    ];
    let mut port_base = 19_100u16;
    let mut failures = Vec::new();
    for (preset, min_latency_ms, loss_pct) in cases {
        let result: Result<()> = async {
            let lab = Lab::new().await?;
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

            let rtt = dev.run_sync(move || test_utils::udp_rtt_sync(r))?;
            if rtt < Duration::from_millis(min_latency_ms) {
                bail!("preset {preset:?}: expected RTT ≥ {min_latency_ms}ms, got {rtt:?}");
            }
            if loss_pct > 0.0 {
                let (_, received) = dev
                    .spawn(move |_| async move {
                        test_utils::udp_send_recv_count(r, 1000, 64, Duration::from_secs(5)).await
                    })?
                    .await??;
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

/// All preset to_limits() calls produce valid (non-NaN) values;
/// Lan is zero; Manual round-trips.
#[test]
fn presets_to_limits_roundtrip() {
    let presets = [
        LinkCondition::Lan,
        LinkCondition::Wifi,
        LinkCondition::WifiBad,
        LinkCondition::Mobile4G,
        LinkCondition::Mobile3G,
        LinkCondition::Satellite,
        LinkCondition::SatelliteGeo,
    ];
    for preset in presets {
        let limits = preset.to_limits();
        assert!(!limits.loss_pct.is_nan(), "{preset:?}: loss_pct is NaN");
        assert!(
            !limits.reorder_pct.is_nan(),
            "{preset:?}: reorder_pct is NaN"
        );
        assert!(
            !limits.duplicate_pct.is_nan(),
            "{preset:?}: duplicate_pct is NaN"
        );
        assert!(
            !limits.corrupt_pct.is_nan(),
            "{preset:?}: corrupt_pct is NaN"
        );
    }
    assert_eq!(LinkCondition::Lan.to_limits(), LinkLimits::default());
    let custom = LinkLimits {
        rate_kbit: 1000,
        loss_pct: 5.0,
        latency_ms: 100,
        jitter_ms: 10,
        reorder_pct: 1.0,
        duplicate_pct: 0.5,
        corrupt_pct: 0.1,
    };
    assert_eq!(LinkCondition::Manual(custom).to_limits(), custom);
}

/// Router builder's downlink_condition applies latency at build time.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn downlink_builder_latency() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab
        .add_router("dc")
        .downlink_condition(LinkCondition::Manual(LinkLimits {
            latency_ms: 50,
            ..Default::default()
        }))
        .build()
        .await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 19_200);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let rtt = dev.run_sync(move || test_utils::udp_rtt_sync(r))?;
    assert!(
        rtt >= Duration::from_millis(30),
        "expected RTT >= 30ms from builder downlink impairment, got {rtt:?}"
    );
    Ok(())
}

/// TOML config with manual impair values deserializes correctly.
#[test]
fn manual_preset_deserialize() -> Result<()> {
    let cfg = r#"
[[router]]
name = "dc1"
region = "eu"

[device.dev1.eth0]
gateway = "dc1"
impair = { rate_kbit = 5000, loss_pct = 1.5, latency_ms = 40 }
"#;
    let parsed: config::LabConfig = toml::from_str(cfg)?;
    let dev1 = parsed.device.get("dev1").context("missing dev1")?;
    let eth0 = dev1.get("eth0").context("missing eth0")?;
    let impair: LinkCondition = eth0
        .get("impair")
        .context("missing impair")?
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| anyhow!("{}", e))?;
    match impair {
        LinkCondition::Manual(limits) => {
            assert_eq!(limits.rate_kbit, 5000);
            assert!((limits.loss_pct - 1.5).abs() < f32::EPSILON);
            assert_eq!(limits.latency_ms, 40);
        }
        other => bail!("unexpected impair: {:?}", other),
    }
    Ok(())
}

/// Dynamically change loss rate at runtime: start with 0% loss (all packets
/// arrive), add 90% loss (most dropped), then remove (all arrive again).
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn loss_dynamic_change() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 20_500);
    dc.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Baseline: no loss, all 50 packets should arrive.
    let (_, recv_baseline) = dev
        .spawn(move |_| async move {
            test_utils::udp_send_recv_count(r, 50, 64, Duration::from_secs(3)).await
        })?
        .await??;
    assert!(
        recv_baseline >= 45,
        "expected ≥ 45/50 with no loss, got {recv_baseline}"
    );

    // Apply 90% loss.
    let dev_handle = lab.device_by_name("dev").unwrap();
    let iface_name = dev_handle.default_iface().unwrap().name().to_string();
    dev_handle
        .set_link_condition(
            &iface_name,
            Some(LinkCondition::Manual(LinkLimits {
                rate_kbit: 0,
                loss_pct: 90.0,
                latency_ms: 0,
                ..Default::default()
            })),
        )
        .await?;

    let (_, recv_lossy) = dev
        .spawn(move |_| async move {
            test_utils::udp_send_recv_count(r, 50, 64, Duration::from_secs(3)).await
        })?
        .await??;
    assert!(
        recv_lossy <= 15,
        "expected ≤ 15/50 with 90% loss, got {recv_lossy}"
    );

    // Remove loss.
    dev_handle.set_link_condition(&iface_name, None).await?;

    let (_, recv_recovered) = dev
        .spawn(move |_| async move {
            test_utils::udp_send_recv_count(r, 50, 64, Duration::from_secs(3)).await
        })?
        .await??;
    assert!(
        recv_recovered >= 45,
        "expected ≥ 45/50 after removing loss, got {recv_recovered}"
    );
    Ok(())
}
