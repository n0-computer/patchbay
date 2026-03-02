//! Tests for RouterPreset and related builder features.

use super::*;

/// RouterPreset::Home builds a dual-stack router with NAT and BlockInbound firewall.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn preset_home() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab
        .add_router("dc")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    let home = lab
        .add_router("home")
        .preset(RouterPreset::Home)
        .build()
        .await?;

    let dev = lab
        .add_device("dev")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    // Home preset should give dual-stack.
    assert_eq!(home.ip_support(), Some(IpSupport::DualStack));
    assert!(dev.ip().is_some(), "device should have v4");
    assert!(dev.ip6().is_some(), "device should have v6");

    // Home has NAT — device IP should be private.
    let dev_ip = dev.ip().unwrap();
    assert!(
        dev_ip.is_private(),
        "home device should have private v4, got {dev_ip}"
    );

    // Outbound UDP to DC should work through NAT.
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 9220);
    dc.spawn_reflector(reflector)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let rtt = dev.run_sync(move || test_utils::udp_rtt_sync(reflector))?;
    assert!(rtt < Duration::from_millis(100), "outbound should work");

    Ok(())
}

/// RouterPreset::Datacenter builds a dual-stack router with no NAT and no firewall.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn preset_datacenter() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab
        .add_router("dc")
        .preset(RouterPreset::Datacenter)
        .build()
        .await?;

    let dev = lab
        .add_device("srv")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    // Datacenter preset: dual-stack, no NAT.
    assert_eq!(dc.ip_support(), Some(IpSupport::DualStack));
    assert!(dev.ip().is_some(), "device should have v4");
    assert!(dev.ip6().is_some(), "device should have v6");

    // Device should have public IP (Nat::None → DownstreamPool::Public).
    let dev_ip = dev.ip().unwrap();
    assert!(
        !dev_ip.is_private(),
        "datacenter device should have public v4, got {dev_ip}"
    );

    Ok(())
}

/// RouterPreset::Mobile builds a dual-stack CGNAT router.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn preset_mobile() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab.add_router("dc").build().await?;
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    let carrier = lab
        .add_router("carrier")
        .preset(RouterPreset::Mobile)
        .build()
        .await?;

    let phone = lab
        .add_device("phone")
        .iface("eth0", carrier.id(), None)
        .build()
        .await?;

    // Mobile: dual-stack with CGNAT.
    assert_eq!(carrier.ip_support(), Some(IpSupport::DualStack));
    assert!(phone.ip().is_some(), "should have v4");
    assert!(phone.ip6().is_some(), "should have v6");

    // Outbound should work.
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 9222);
    dc.spawn_reflector(reflector)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let rtt = phone.run_sync(move || test_utils::udp_rtt_sync(reflector))?;
    assert!(rtt < Duration::from_millis(100));

    Ok(())
}

/// RouterPreset::Corporate blocks non-web UDP (same as Firewall::Corporate).
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn preset_corporate_blocks_udp() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab.add_router("dc").build().await?;
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    let corp = lab
        .add_router("corp")
        .preset(RouterPreset::Corporate)
        .build()
        .await?;

    let dev = lab
        .add_device("ws")
        .iface("eth0", corp.id(), None)
        .build()
        .await?;

    // Corporate firewall should block arbitrary UDP.
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 9223);
    dc.spawn_reflector(reflector)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let blocked = dev.run_sync(move || test_utils::udp_rtt_sync(reflector));
    assert!(
        blocked.is_err(),
        "corporate preset should block UDP, got: {:?}",
        blocked
    );

    // TCP 443 should work.
    let tcp_bind = SocketAddr::new(IpAddr::V4(dc_ip), 443);
    dc.spawn(async move |_| spawn_tcp_echo_server(tcp_bind).await)?
        .await??;
    dev.spawn(async move |_| tcp_roundtrip(tcp_bind).await)?
        .await??;

    Ok(())
}

/// Preset with override: Home preset with Nat::FullCone overrides only NAT.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn preset_override() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab.add_router("dc").build().await?;
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    // Home preset + FullCone NAT override.
    let home = lab
        .add_router("home")
        .preset(RouterPreset::Home)
        .nat(Nat::FullCone)
        .build()
        .await?;

    let dev = lab
        .add_device("dev")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    // Should still be dual-stack (from preset).
    assert_eq!(home.ip_support(), Some(IpSupport::DualStack));

    // Outbound should work (FullCone + BlockInbound).
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 9225);
    dc.spawn_reflector(reflector)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let rtt = dev.run_sync(move || test_utils::udp_rtt_sync(reflector))?;
    assert!(rtt < Duration::from_millis(100));

    Ok(())
}

/// Public GUA v6 pool gives addresses from 2001:db8:1::/48, not ULA fd10::/48.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn public_v6_pool_is_gua() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    // Datacenter preset uses DownstreamPool::Public.
    let dc = lab
        .add_router("dc")
        .preset(RouterPreset::Datacenter)
        .build()
        .await?;

    let dev = lab
        .add_device("srv")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    let v6 = dev.ip6().context("no v6 address")?;
    let segs = v6.segments();
    // Public GUA pool is 2001:db8:1::/48.
    assert_eq!(segs[0], 0x2001, "v6 should be from GUA pool, got {v6}");
    assert_eq!(segs[1], 0x0db8, "v6 should be from GUA pool, got {v6}");
    assert_eq!(
        segs[2], 0x0001,
        "v6 third segment should be 1 (public pool), got {v6}"
    );

    // ULA check: the address should NOT start with fd.
    assert_ne!(
        segs[0] >> 8,
        0xfd,
        "public pool should not give ULA address, got {v6}"
    );

    Ok(())
}

/// Private v6 pool (Home preset default) gives ULA fd10::/48 addresses.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn private_v6_pool_is_ula() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let home = lab
        .add_router("home")
        .preset(RouterPreset::Home)
        .build()
        .await?;

    let dev = lab
        .add_device("laptop")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    let v6 = dev.ip6().context("no v6 address")?;
    let segs = v6.segments();
    // Private ULA pool is fd10::/48.
    assert_eq!(
        segs[0], 0xfd10,
        "home device v6 should be ULA fd10::, got {v6}"
    );

    Ok(())
}

/// RouterPreset::Hotel builds v4-only (no v6).
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn preset_hotel_v4_only() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let hotel = lab
        .add_router("hotel")
        .preset(RouterPreset::Hotel)
        .build()
        .await?;

    let dev = lab
        .add_device("guest")
        .iface("eth0", hotel.id(), None)
        .build()
        .await?;

    assert_eq!(hotel.ip_support(), Some(IpSupport::V4Only));
    assert!(dev.ip().is_some(), "hotel device should have v4");
    assert!(dev.ip6().is_none(), "hotel device should have no v6");

    Ok(())
}

/// RouterPreset::IspV4 builds v4-only with no NAT.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn preset_isp_v4() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let isp = lab
        .add_router("isp")
        .preset(RouterPreset::IspV4)
        .build()
        .await?;

    let dev = lab
        .add_device("srv")
        .iface("eth0", isp.id(), None)
        .build()
        .await?;

    assert_eq!(isp.ip_support(), Some(IpSupport::V4Only));
    // No NAT → public IP.
    let dev_ip = dev.ip().unwrap();
    assert!(
        !dev_ip.is_private(),
        "IspV4 device should have public IP, got {dev_ip}"
    );
    assert!(dev.ip6().is_none(), "IspV4 should have no v6");

    Ok(())
}

/// RouterPreset::MobileV6 builds an IPv6-only carrier with NAT64.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn preset_mobile_v6() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab.add_router("dc").build().await?;
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    let carrier = lab
        .add_router("carrier")
        .preset(RouterPreset::MobileV6)
        .build()
        .await?;

    let phone = lab
        .add_device("phone")
        .iface("eth0", carrier.id(), None)
        .build()
        .await?;

    // MobileV6: IPv6-only.
    assert_eq!(carrier.ip_support(), Some(IpSupport::V6Only));
    assert!(phone.ip6().is_some(), "should have v6");

    // Phone can reach v4 server via NAT64 prefix.
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 9350);
    dc.spawn_reflector(reflector)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let nat64_addr = crate::nat64::embed_v4_in_nat64(dc_ip);
    let nat64_target = SocketAddr::new(IpAddr::V6(nat64_addr), 9350);

    let rtt = phone.run_sync(move || test_utils::udp_rtt_sync(nat64_target))?;
    assert!(
        rtt < Duration::from_millis(500),
        "NAT64 should work via preset"
    );

    Ok(())
}
