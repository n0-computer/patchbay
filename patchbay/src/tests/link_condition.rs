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
        .iface("eth0", dc.id())
        .iface("eth1", isp.id())
        .default_via("eth0")
        .build()
        .await?;

    dev.iface("eth1")
        .unwrap()
        .set_condition(LinkCondition::mobile_4g(), LinkDirection::Egress)
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 9200);
    let _r = dc.spawn_reflector(r).await?;

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
            let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

            let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
            let r = SocketAddr::new(IpAddr::V4(dc_ip), port_base);
            let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);

            let eth0 = lab.device_by_name("dev").unwrap().iface("eth0").unwrap();
            match proto {
                Proto::Udp => {
                    let _r = dc.spawn_reflector(r).await?;
                    dev.run_sync(move || {
                        test_utils::probe_udp(r, Duration::from_millis(500), Some(bind))
                    })
                    .context("before link_down")?;
                    eth0.link_down().await?;
                    if dev
                        .run_sync(move || {
                            test_utils::probe_udp(r, Duration::from_millis(500), Some(bind))
                        })
                        .is_ok()
                    {
                        bail!("probe should fail after link_down");
                    }
                    eth0.link_up().await?;
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
                    eth0.link_down().await?;
                    if dev
                        .spawn(move |_| async move { tcp_roundtrip(r).await })?
                        .await
                        .map(|r| r.is_ok())
                        .unwrap_or(false)
                    {
                        bail!("tcp should fail after link_down");
                    }
                    eth0.link_up().await?;
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
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    dev.iface("eth0")
        .unwrap()
        .set_condition(LinkCondition::new().rate_kbit(2000), LinkDirection::Egress)
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
    let dev_id = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    dc.set_downlink_condition(Some(LinkCondition::new().rate_kbit(2000)))
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
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    dev.iface("eth0")
        .unwrap()
        .set_condition(LinkCondition::new().rate_kbit(2000), LinkDirection::Egress)
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 17_500);
    let _r = dc.spawn_reflector(r).await?;

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
    let dev_id = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    dc.set_downlink_condition(Some(LinkCondition::new().rate_kbit(2000)))
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 17_600);
    let _r = dc.spawn_reflector(r).await?;

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
    let dev_id = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    dev_id
        .iface("eth0")
        .unwrap()
        .set_condition(
            LinkCondition::new().rate_kbit(1000),
            // Egress only: cap upload at 1000, download unimpaired by this rule.
            LinkDirection::Egress,
        )
        .await?;

    dc.set_downlink_condition(Some(LinkCondition::new().rate_kbit(4000)))
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
        .nat(Nat::Moderate)
        .build()
        .await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", nat.id())
        .build()
        .await?;

    lab.set_link_condition(
        nat.id(),
        isp.id(),
        Some(LinkCondition::new().rate_kbit(1000)),
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
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    dev.iface("eth0")
        .unwrap()
        .set_condition(LinkCondition::new().rate_kbit(2000), LinkDirection::Egress)
        .await?;

    dc.set_downlink_condition(Some(LinkCondition::new().rate_kbit(2000)))
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
    // Build with a clean link so ARP resolves without interference from netem.
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 18_000);
    let _r = dc.spawn_reflector(r).await?;

    // Warmup: populate ARP cache before applying loss.
    dev.run_sync(move || test_utils::probe_udp(r, Duration::from_secs(2), None))
        .context("warmup probe failed")?;

    // Now apply 50% loss on the device egress.
    let default_iface = lab.device_by_name("dev").unwrap().default_iface().unwrap();
    default_iface
        .set_condition(LinkCondition::new().loss_pct(50.0), LinkDirection::Egress)
        .await?;

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
    // Build with a clean link so ARP resolves without interference from netem.
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 18_100);
    let _r = dc.spawn_reflector(r).await?;

    // Warmup: populate ARP cache before applying loss.
    dev.run_sync(move || test_utils::probe_udp(r, Duration::from_secs(2), None))
        .context("warmup probe failed")?;

    // Now apply 90% loss on the device egress.
    let default_iface = lab.device_by_name("dev").unwrap().default_iface().unwrap();
    default_iface
        .set_condition(LinkCondition::new().loss_pct(90.0), LinkDirection::Egress)
        .await?;

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
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    dev.iface("eth0")
        .unwrap()
        .set_condition(LinkCondition::new().loss_pct(5.0), LinkDirection::Egress)
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
    // Build with a clean link so ARP resolves without interference from netem.
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 18_300);
    let _r = dc.spawn_reflector(r).await?;

    // Warmup: populate ARP cache before applying loss.
    dev.run_sync(move || test_utils::probe_udp(r, Duration::from_secs(2), None))
        .context("warmup probe failed")?;

    // Now apply 30% loss on both upload and download.
    let default_iface = lab.device_by_name("dev").unwrap().default_iface().unwrap();
    default_iface
        .set_condition(LinkCondition::new().loss_pct(30.0), LinkDirection::Egress)
        .await?;

    dc.set_downlink_condition(Some(LinkCondition::new().loss_pct(30.0)))
        .await?;

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
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    dev.iface("eth0")
        .unwrap()
        .set_condition(LinkCondition::new().latency_ms(20), LinkDirection::Egress)
        .await?;

    dc.set_downlink_condition(Some(LinkCondition::new().latency_ms(30)))
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 18_500);
    let _r = dc.spawn_reflector(r).await?;

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
        .nat(Nat::Moderate)
        .build()
        .await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", nat.id())
        .build()
        .await?;

    dev.iface("eth0")
        .unwrap()
        .set_condition(LinkCondition::new().latency_ms(20), LinkDirection::Egress)
        .await?;

    lab.set_link_condition(
        nat.id(),
        isp.id(),
        Some(LinkCondition::new().latency_ms(30)),
    )
    .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 18_700);
    let _r = dc.spawn_reflector(r).await?;

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
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    dev.iface("eth0")
        .unwrap()
        .set_condition(LinkCondition::new().rate_kbit(5000), LinkDirection::Egress)
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    let sink = dc.spawn_thread(move || tcp_sink(SocketAddr::new(IpAddr::V4(dc_ip), 18_800)))?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_e, kbps_fast) = dev.run_sync(move || {
        tcp_measure_throughput(SocketAddr::new(IpAddr::V4(dc_ip), 18_800), 256 * 1024)
    })?;
    join_sink(sink)?;

    let default_iface = lab.device_by_name("dev").unwrap().default_iface().unwrap();
    default_iface
        .set_condition(LinkCondition::new().rate_kbit(500), LinkDirection::Egress)
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
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    dev.iface("eth0")
        .unwrap()
        .set_condition(LinkCondition::new().rate_kbit(1000), LinkDirection::Egress)
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    let sink = dc.spawn_thread(move || tcp_sink(SocketAddr::new(IpAddr::V4(dc_ip), 18_900)))?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_e, kbps_throttled) = dev.run_sync(move || {
        tcp_measure_throughput(SocketAddr::new(IpAddr::V4(dc_ip), 18_900), 128 * 1024)
    })?;
    join_sink(sink)?;

    let default_iface = lab.device_by_name("dev").unwrap().default_iface().unwrap();
    default_iface.clear_condition(LinkDirection::Both).await?;

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
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 19_000);
    let _r = dc.spawn_reflector(r).await?;

    let baseline = dev.run_sync(move || test_utils::udp_rtt_sync(r))?;

    let default_iface = lab.device_by_name("dev").unwrap().default_iface().unwrap();
    default_iface
        .set_condition(LinkCondition::new().latency_ms(100), LinkDirection::Egress)
        .await?;
    let high = dev.run_sync(move || test_utils::udp_rtt_sync(r))?;
    assert!(
        high >= baseline + Duration::from_millis(90),
        "expected RTT +90ms after 100ms impair, baseline={baseline:?} high={high:?}"
    );

    default_iface.clear_condition(LinkDirection::Both).await?;
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
    // (preset, min one-way latency ms, loss % to smoke-test). The presets now
    // use bursty Gilbert-Elliott loss, which concentrates drops into rare bursts,
    // so a small sample can see zero loss even at a few percent. We only
    // smoke-test loss on the two presets whose rate is high enough to fire
    // reliably over the sample below, and use a large sample for those.
    let cases: Vec<(LinkCondition, u64, f32)> = vec![
        (LinkCondition::lan(), 0, 0.0),
        (LinkCondition::wifi(), 4, 0.0),
        (LinkCondition::wifi_bad(), 25, 1.5),
        (LinkCondition::mobile_4g(), 25, 0.0),
        (LinkCondition::mobile_3g(), 80, 2.0),
        (LinkCondition::satellite(), 22, 0.0),
        (LinkCondition::satellite_geo(), 300, 0.0),
    ];
    let mut port_base = 19_100u16;
    let mut failures = Vec::new();
    for (preset, min_latency_ms, loss_pct) in cases {
        let result: Result<()> = async {
            let lab = Lab::new().await?;
            let dc = lab.add_router("dc").build().await?;
            let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

            dev.iface("eth0")
                .unwrap()
                .set_condition(preset, LinkDirection::Egress)
                .await?;

            let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
            let r = SocketAddr::new(IpAddr::V4(dc_ip), port_base);
            let _r = dc.spawn_reflector(r).await?;

            let rtt = dev.run_sync(move || test_utils::udp_rtt_sync(r))?;
            if rtt < Duration::from_millis(min_latency_ms) {
                bail!("preset {preset:?}: expected RTT ≥ {min_latency_ms}ms, got {rtt:?}");
            }
            if loss_pct > 0.0 {
                // Large enough that bursty loss at ~1.5% or more reliably drops at
                // least one packet (P(zero loss) is well under 0.1%).
                let total = 5000usize;
                let (_, received) = dev
                    .spawn(move |_| async move {
                        test_utils::udp_send_recv_count(r, total, 64, Duration::from_secs(8)).await
                    })?
                    .await??;
                if received == total {
                    bail!(
                        "preset {preset:?}: expected some loss at {loss_pct}%, got {received}/{total}"
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

/// Every preset carries finite (non-NaN) percentages and a matching label.
#[test]
fn presets_have_valid_values() {
    let presets = [
        LinkCondition::lan(),
        LinkCondition::wifi(),
        LinkCondition::wifi_bad(),
        LinkCondition::mobile_4g(),
        LinkCondition::mobile_3g(),
        LinkCondition::satellite(),
        LinkCondition::satellite_geo(),
    ];
    for preset in presets {
        assert!(!preset.loss_pct.is_nan(), "{preset:?}: loss_pct is NaN");
        assert!(
            !preset.reorder_pct.is_nan(),
            "{preset:?}: reorder_pct is NaN"
        );
        assert!(
            !preset.duplicate_pct.is_nan(),
            "{preset:?}: duplicate_pct is NaN"
        );
        assert!(
            !preset.corrupt_pct.is_nan(),
            "{preset:?}: corrupt_pct is NaN"
        );
        assert!(preset.label.is_some(), "{preset:?}: preset carries a label");
    }
    // `new()` is the unimpaired baseline; `lan()` adds a small switch-hop delay.
    assert_eq!(LinkCondition::new().latency_ms, 0);
    assert_eq!(LinkCondition::lan().latency_ms, 1);
}

/// Router builder's downlink_condition applies latency at build time.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn downlink_builder_latency() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab
        .add_router("dc")
        .downlink_condition(LinkCondition::new().latency_ms(50))
        .build()
        .await?;
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 19_200);
    let _r = dc.spawn_reflector(r).await?;

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
    assert_eq!(impair.rate_kbit, Some(5000));
    assert!((impair.loss_pct - 1.5).abs() < f32::EPSILON);
    assert_eq!(impair.latency_ms, 40);
    Ok(())
}

/// Dynamically change loss rate at runtime: start with 0% loss (all packets
/// arrive), add 90% loss (most dropped), then remove (all arrive again).
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn loss_dynamic_change() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 20_500);
    let _r = dc.spawn_reflector(r).await?;

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
    let default_iface = lab.device_by_name("dev").unwrap().default_iface().unwrap();
    default_iface
        .set_condition(LinkCondition::new().loss_pct(90.0), LinkDirection::Egress)
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
    default_iface.clear_condition(LinkDirection::Both).await?;

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

// ── Direction permutations ──────────────────────────────────────────

/// Measure median UDP round-trip time over `n` probes (blocking).
fn median_udp_rtt_sync(reflector: SocketAddr, n: usize) -> Result<Duration> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").context("median_rtt bind")?;
    sock.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut buf = [0u8; 256];
    let mut samples = Vec::with_capacity(n);
    for _ in 0..n {
        let start = Instant::now();
        sock.send_to(b"PING", reflector)?;
        let _ = sock.recv_from(&mut buf)?;
        samples.push(start.elapsed());
        // Small gap to avoid burst effects.
        thread::sleep(Duration::from_millis(5));
    }
    samples.sort();
    Ok(samples[samples.len() / 2])
}

/// Egress, ingress, and both directions produce expected RTT differences.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn direction_permutations() -> Result<()> {
    check_caps()?;

    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let sender = lab
        .add_device("sender")
        .iface("eth0", dc.id())
        .build()
        .await?;
    let receiver = lab
        .add_device("receiver")
        .iface("eth0", dc.id())
        .build()
        .await?;

    let recv_ip = receiver.ip().unwrap();
    let reflector_addr = SocketAddr::new(IpAddr::V4(recv_ip), 21_000);
    let _r = receiver.spawn_reflector(reflector_addr).await?;

    // Use 500ms latency so single-direction (~500ms) and both-direction (~1000ms)
    // are far enough apart that no CI jitter can confuse them. Each assertion uses
    // a ±150ms tolerance around the expected value.
    let latency_ms = 500;
    let condition = LinkCondition::new().latency_ms(latency_ms);

    // Baseline: no impairment, RTT should be near zero.
    let rtt_ms = sender
        .run_sync(move || median_udp_rtt_sync(reflector_addr, 5).map(|d| d.as_millis() as u64))
        .context("baseline")?;
    assert!(
        rtt_ms < 100,
        "baseline RTT should be < 100ms, got {rtt_ms}ms"
    );

    let sender_eth0 = sender.iface("eth0").unwrap();

    // ── Egress only ──
    // Outbound packets delayed 500ms, inbound clean. RTT ~ 500ms.
    sender_eth0
        .set_condition(condition, LinkDirection::Egress)
        .await?;
    let egress_rtt = sender
        .run_sync(move || median_udp_rtt_sync(reflector_addr, 7).map(|d| d.as_millis() as u64))
        .context("egress")?;
    assert!(
        (350..750).contains(&egress_rtt),
        "egress-only RTT should be ~500ms (350-750), got {egress_rtt}ms"
    );
    sender_eth0.clear_condition(LinkDirection::Both).await?;

    // ── Ingress only ──
    // Inbound packets delayed 500ms, outbound clean. RTT ~ 500ms.
    sender_eth0
        .set_condition(condition, LinkDirection::Ingress)
        .await?;
    let ingress_rtt = sender
        .run_sync(move || median_udp_rtt_sync(reflector_addr, 7).map(|d| d.as_millis() as u64))
        .context("ingress")?;
    assert!(
        (350..750).contains(&ingress_rtt),
        "ingress-only RTT should be ~500ms (350-750), got {ingress_rtt}ms"
    );
    sender_eth0.clear_condition(LinkDirection::Both).await?;

    // ── Both directions ──
    // Both paths delayed 500ms each. RTT ~ 1000ms.
    // The lower bound (800) is above the single-direction upper bound (750),
    // so this proves both directions are actually impaired.
    sender_eth0
        .set_condition(condition, LinkDirection::Both)
        .await?;
    let both_rtt = sender
        .run_sync(move || median_udp_rtt_sync(reflector_addr, 7).map(|d| d.as_millis() as u64))
        .context("both")?;
    assert!(
        (800..1300).contains(&both_rtt),
        "both-direction RTT should be ~1000ms (800-1300), got {both_rtt}ms"
    );

    // Verify that both > egress and both > ingress (proves additivity).
    assert!(
        both_rtt > egress_rtt && both_rtt > ingress_rtt,
        "both ({both_rtt}ms) should exceed egress ({egress_rtt}ms) and ingress ({ingress_rtt}ms)"
    );

    sender_eth0.clear_condition(LinkDirection::Both).await?;

    Ok(())
}

/// Bidirectional impairment between two devices is achieved by calling
/// `Lab::set_link_condition` once per device-router link. Each call applies
/// netem on the device interface's egress qdisc, so impairing both the
/// sender's and receiver's link doubles the observed round-trip latency.
///
/// This is the Lab-level pattern for bidirectional impairment — call once per
/// side rather than using `LinkDirection::Both` (which is only available on
/// [`Device::set_link_condition`]).
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn lab_bidirectional_via_two_calls() -> Result<()> {
    check_caps()?;

    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let sender = lab
        .add_device("sender")
        .iface("eth0", dc.id())
        .build()
        .await?;
    let receiver = lab
        .add_device("receiver")
        .iface("eth0", dc.id())
        .build()
        .await?;

    let recv_ip = receiver.ip().unwrap();
    let reflector_addr = SocketAddr::new(IpAddr::V4(recv_ip), 19_100);
    let _r = receiver.spawn_reflector(reflector_addr).await?;

    let condition = LinkCondition::new().latency_ms(500);

    // Baseline: no impairment. RTT should be well under 100ms.
    let baseline = sender.run_sync(move || test_utils::udp_rtt_sync(reflector_addr))?;
    assert!(
        baseline < Duration::from_millis(100),
        "baseline RTT should be < 100ms, got {baseline:?}"
    );

    // Impair sender's link only. Netem applies to egress, so the outgoing
    // packet from sender is delayed 500ms, but the reply arrives unimpaired.
    // RTT ~ 500ms.
    lab.set_link_condition(sender.id(), dc.id(), Some(condition))
        .await?;
    let one_side = sender
        .run_sync(move || test_utils::udp_rtt_sync(reflector_addr))?
        .as_millis() as u64;
    assert!(
        (300..800).contains(&one_side),
        "one-side RTT should be ~500ms (300-800), got {one_side}ms"
    );

    // Also impair receiver's link. Now the reply from receiver is also
    // delayed 500ms on its egress. RTT ~ 1000ms (500ms each way).
    lab.set_link_condition(receiver.id(), dc.id(), Some(condition))
        .await?;
    let both_sides = sender
        .run_sync(move || test_utils::udp_rtt_sync(reflector_addr))?
        .as_millis() as u64;
    assert!(
        (800..1300).contains(&both_sides),
        "both-sides RTT should be ~1000ms (800-1300), got {both_sides}ms"
    );

    // Impairing the second link added measurably more latency.
    assert!(
        both_sides > one_side,
        "both-sides ({both_sides}ms) should exceed one-side ({one_side}ms)"
    );

    Ok(())
}
