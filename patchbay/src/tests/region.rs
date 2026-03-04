//! Multi-region latency, break/restore links, transit routing.

use super::*;

/// Two regions linked at 50ms: inter-region RTT >= 90ms.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn basic_latency() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let us = lab.add_region("us").await?;
    let eu = lab.add_region("eu").await?;
    lab.link_regions(&us, &eu, RegionLink::good(50)).await?;

    let dc_us = lab.add_router("dc-us").region(&us).build().await?;
    let dc_eu = lab.add_router("dc-eu").region(&eu).build().await?;

    let dev_us = lab
        .add_device("dev-us")
        .iface("eth0", dc_us.id(), None)
        .build()
        .await?;

    let eu_ip = dc_eu.uplink_ip().context("no uplink ip")?;
    let r_eu = SocketAddr::new(IpAddr::V4(eu_ip), 9100);
    let _r = dc_eu.spawn_reflector(r_eu).await?;

    let rtt = dev_us.run_sync(move || test_utils::udp_rtt_sync(r_eu))?;
    assert!(
        rtt >= Duration::from_millis(90),
        "expected inter-region RTT >= 90ms (2×50ms), got {rtt:?}"
    );
    Ok(())
}

/// add_default_regions creates us/eu/asia; us-eu RTT >= 70ms.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn default_regions() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let regions = lab.add_default_regions().await?;

    let dc_us = lab.add_router("dc-us").region(&regions.us).build().await?;
    let dc_eu = lab.add_router("dc-eu").region(&regions.eu).build().await?;

    let dev_us = lab
        .add_device("dev-us")
        .iface("eth0", dc_us.id(), None)
        .build()
        .await?;

    let eu_ip = dc_eu.uplink_ip().context("no uplink ip")?;
    let r_eu = SocketAddr::new(IpAddr::V4(eu_ip), 9101);
    let _r = dc_eu.spawn_reflector(r_eu).await?;

    let rtt = dev_us.run_sync(move || test_utils::udp_rtt_sync(r_eu))?;
    assert!(
        rtt >= Duration::from_millis(70),
        "expected us↔eu RTT >= 70ms (2×40ms), got {rtt:?}"
    );
    Ok(())
}

/// Break eu-asia link forces reroute through us (higher RTT); restore returns to direct path.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn break_restore_link() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let regions = lab.add_default_regions().await?;

    let dc_eu = lab.add_router("dc-eu").region(&regions.eu).build().await?;
    let dc_asia = lab
        .add_router("dc-asia")
        .region(&regions.asia)
        .build()
        .await?;

    let asia_ip = dc_asia.uplink_ip().context("no uplink ip")?;
    let r_asia = SocketAddr::new(IpAddr::V4(asia_ip), 9102);
    let _r = dc_asia.spawn_reflector(r_asia).await?;

    let rtt_direct = dc_eu.run_sync(move || test_utils::udp_rtt_sync(r_asia))?;
    assert!(
        rtt_direct >= Duration::from_millis(220),
        "expected direct eu↔asia RTT >= 220ms, got {rtt_direct:?}"
    );

    lab.break_region_link(&regions.eu, &regions.asia)?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let rtt_broken = dc_eu.run_sync(move || test_utils::udp_rtt_sync(r_asia))?;
    assert!(
        rtt_broken >= Duration::from_millis(240),
        "expected broken eu↔asia RTT >= 240ms (via us), got {rtt_broken:?}"
    );

    lab.restore_region_link(&regions.eu, &regions.asia)?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let rtt_restored = dc_eu.run_sync(move || test_utils::udp_rtt_sync(r_asia))?;
    assert!(
        rtt_restored >= Duration::from_millis(220),
        "expected restored eu↔asia RTT >= 220ms, got {rtt_restored:?}"
    );
    Ok(())
}

/// Device-level 30ms impairment stacks with 40ms region latency for cross-region,
/// but only device impairment applies for same-region.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn impair_stacks_with_latency() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let eu = lab.add_region("eu").await?;
    let us = lab.add_region("us").await?;
    lab.link_regions(&eu, &us, RegionLink::good(40)).await?;

    let dc_eu = lab.add_router("dc-eu").region(&eu).build().await?;
    let dc_us = lab.add_router("dc-us").region(&us).build().await?;

    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            dc_eu.id(),
            Some(LinkCondition::Manual(LinkLimits {
                latency_ms: 30,
                ..Default::default()
            })),
        )
        .build()
        .await?;

    let us_ip = dc_us.uplink_ip().context("no uplink ip")?;
    let r_us = SocketAddr::new(IpAddr::V4(us_ip), 18_700);
    let _r = dc_us.spawn_reflector(r_us).await?;

    let eu_ip = dc_eu.uplink_ip().context("no uplink ip")?;
    let r_eu = SocketAddr::new(IpAddr::V4(eu_ip), 18_701);
    let _r = dc_eu.spawn_reflector(r_eu).await?;

    let rtt_cross = dev.run_sync(move || test_utils::udp_rtt_sync(r_us))?;
    assert!(
        rtt_cross >= Duration::from_millis(90),
        "expected eu→us RTT ≥ 90ms (device + region), got {rtt_cross:?}"
    );

    let rtt_local = dev.run_sync(move || test_utils::udp_rtt_sync(r_eu))?;
    assert!(
        rtt_local >= Duration::from_millis(25),
        "expected eu→eu RTT ≥ 25ms (device impair only), got {rtt_local:?}"
    );
    assert!(
        rtt_local < rtt_cross - Duration::from_millis(30),
        "expected local RTT much less than cross-region, local={rtt_local:?} cross={rtt_cross:?}"
    );
    Ok(())
}

/// V6-only inter-region traffic is impaired correctly.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn v6_latency() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let eu = lab.add_region("eu").await?;
    let us = lab.add_region("us").await?;
    lab.link_regions(&eu, &us, RegionLink::good(65)).await?;

    let dc_eu = lab
        .add_router("dc-eu")
        .region(&eu)
        .ip_support(IpSupport::V6Only)
        .build()
        .await?;
    let dc_us = lab
        .add_router("dc-us")
        .region(&us)
        .ip_support(IpSupport::V6Only)
        .build()
        .await?;

    let eu_v6 = dc_eu.uplink_ip_v6().expect("eu v6 uplink");
    let r_v6 = SocketAddr::new(IpAddr::V6(eu_v6), 3495);
    let _r = dc_eu.spawn_reflector(r_v6).await?;

    let rtt = dc_us.run_sync(move || test_utils::udp_rtt_sync(r_v6))?;
    assert!(
        rtt >= Duration::from_millis(120),
        "expected v6 RTT >= 120ms (2×65ms), got {rtt:?}"
    );
    Ok(())
}

/// Dual-stack: both v4 and v6 inter-region traffic are impaired.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn dual_stack_latency() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let eu = lab.add_region("eu").await?;
    let us = lab.add_region("us").await?;
    lab.link_regions(&eu, &us, RegionLink::good(65)).await?;

    let dc_eu = lab
        .add_router("dc-eu")
        .region(&eu)
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;
    let dc_us = lab
        .add_router("dc-us")
        .region(&us)
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;

    let eu_v4 = dc_eu.uplink_ip().expect("eu v4 uplink");
    let r_v4 = SocketAddr::new(IpAddr::V4(eu_v4), 3510);
    let _r = dc_eu.spawn_reflector(r_v4).await?;

    let eu_v6 = dc_eu.uplink_ip_v6().expect("eu v6 uplink");
    let r_v6 = SocketAddr::new(IpAddr::V6(eu_v6), 3511);
    let _r = dc_eu.spawn_reflector(r_v6).await?;

    let rtt_v4 = dc_us.run_sync(move || test_utils::udp_rtt_sync(r_v4))?;
    assert!(
        rtt_v4 >= Duration::from_millis(120),
        "expected v4 RTT >= 120ms (2×65ms), got {rtt_v4:?}"
    );

    let rtt_v6 = dc_us.run_sync(move || test_utils::udp_rtt_sync(r_v6))?;
    assert!(
        rtt_v6 >= Duration::from_millis(120),
        "expected v6 RTT >= 120ms (2×65ms), got {rtt_v6:?}"
    );
    Ok(())
}

/// Regionless router reaches region devices with low RTT (no netem).
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn regionless_to_region() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let us = lab.add_region("us").await?;

    let dc = lab.add_router("dc").build().await?;
    let dc_us = lab.add_router("dc-us").region(&us).build().await?;

    let us_ip = dc_us.uplink_ip().context("no uplink ip")?;
    let r_us = SocketAddr::new(IpAddr::V4(us_ip), 9104);
    let _r = dc_us.spawn_reflector(r_us).await?;

    let rtt = dc.run_sync(move || test_utils::udp_rtt_sync(r_us))?;
    assert!(
        rtt < Duration::from_millis(50),
        "expected regionless→region RTT < 50ms (no netem), got {rtt:?}"
    );
    Ok(())
}

/// NATted device in EU reaches US DC with combined NAT + region latency.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn mixed_nat_region() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let us = lab.add_region("us").await?;
    let eu = lab.add_region("eu").await?;
    lab.link_regions(&us, &eu, RegionLink::good(50)).await?;

    let dc_us = lab.add_router("dc-us").region(&us).build().await?;
    let home_eu = lab
        .add_router("home-eu")
        .region(&eu)
        .nat(Nat::Home)
        .build()
        .await?;

    let dev = lab
        .add_device("dev")
        .iface("eth0", home_eu.id(), None)
        .build()
        .await?;

    let us_ip = dc_us.uplink_ip().context("no uplink ip")?;
    let r_us = SocketAddr::new(IpAddr::V4(us_ip), 9105);
    let _r = dc_us.spawn_reflector(r_us).await?;

    let rtt = dev.run_sync(move || test_utils::udp_rtt_sync(r_us))?;
    assert!(
        rtt >= Duration::from_millis(90),
        "expected NAT+region RTT >= 90ms, got {rtt:?}"
    );
    Ok(())
}

/// Router name starting with 'region_' is rejected as reserved.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn reserved_name_rejected() -> Result<()> {
    let lab = Lab::new().await?;
    let result = lab.add_router("region_foo").build().await;
    assert!(
        result.is_err(),
        "expected error for reserved name 'region_foo'"
    );
    assert!(
        result.unwrap_err().to_string().contains("reserved"),
        "error should mention 'reserved'"
    );
    Ok(())
}

/// Three regions all linked: A↔C direct at 100ms is slower than A↔B (30ms)
/// + B↔C (40ms) transit. Both A↔B and A↔C are independently impaired.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn three_region_triangle() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let a = lab.add_region("a").await?;
    let b = lab.add_region("b").await?;
    let c = lab.add_region("c").await?;
    lab.link_regions(&a, &b, RegionLink::good(30)).await?;
    lab.link_regions(&b, &c, RegionLink::good(40)).await?;
    lab.link_regions(&a, &c, RegionLink::good(100)).await?;

    let dc_a = lab.add_router("dc-a").region(&a).build().await?;
    let dc_b = lab.add_router("dc-b").region(&b).build().await?;
    let dc_c = lab.add_router("dc-c").region(&c).build().await?;

    let b_ip = dc_b.uplink_ip().context("no b uplink ip")?;
    let r_b = SocketAddr::new(IpAddr::V4(b_ip), 20_400);
    let _r = dc_b.spawn_reflector(r_b).await?;

    let c_ip = dc_c.uplink_ip().context("no c uplink ip")?;
    let r_c = SocketAddr::new(IpAddr::V4(c_ip), 20_401);
    let _r = dc_c.spawn_reflector(r_c).await?;

    // A↔B: 30ms one-way → RTT ≥ 50ms.
    let rtt_ab = dc_a.run_sync(move || test_utils::udp_rtt_sync(r_b))?;
    assert!(
        rtt_ab >= Duration::from_millis(50),
        "expected A↔B RTT >= 50ms (2×30ms), got {rtt_ab:?}"
    );

    // A↔C: 100ms one-way → RTT ≥ 180ms.
    let rtt_ac = dc_a.run_sync(move || test_utils::udp_rtt_sync(r_c))?;
    assert!(
        rtt_ac >= Duration::from_millis(180),
        "expected A↔C RTT >= 180ms (2×100ms), got {rtt_ac:?}"
    );

    // A↔C should be notably slower than A↔B.
    assert!(
        rtt_ac > rtt_ab + Duration::from_millis(80),
        "expected A↔C much slower than A↔B: ac={rtt_ac:?} ab={rtt_ab:?}"
    );
    Ok(())
}
