//! Firewall presets and custom rules.

use super::*;

/// Corporate firewall blocks non-whitelisted UDP but allows TCP 443.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn corporate_blocks_udp() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab.add_router("dc").build().await?;
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    let corp = lab
        .add_router("corp")
        .nat(Nat::Home)
        .firewall(Firewall::Corporate)
        .build()
        .await?;

    let dev = lab
        .add_device("laptop")
        .iface("eth0", corp.id(), None)
        .build()
        .await?;

    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 9200);
    dc.spawn_reflector(reflector)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let udp_result = dev.run_sync(move || test_utils::udp_rtt(reflector));
    assert!(
        udp_result.is_err(),
        "expected UDP to be blocked by corporate firewall, got: {:?}",
        udp_result
    );

    let tcp_bind = SocketAddr::new(IpAddr::V4(dc_ip), 443);
    dc.spawn(async move |_| spawn_tcp_echo_server(tcp_bind).await)?
        .await??;
    dev.spawn(async move |_| tcp_roundtrip(tcp_bind).await)?
        .await??;

    Ok(())
}

/// Captive portal blocks all UDP but allows TCP on any port.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn captive_portal_blocks_udp() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab.add_router("dc").build().await?;
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    let portal = lab
        .add_router("portal")
        .nat(Nat::Home)
        .firewall(Firewall::CaptivePortal)
        .build()
        .await?;

    let dev = lab
        .add_device("phone")
        .iface("eth0", portal.id(), None)
        .build()
        .await?;

    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 9201);
    dc.spawn_reflector(reflector)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let udp_result = dev.run_sync(move || test_utils::udp_rtt(reflector));
    assert!(
        udp_result.is_err(),
        "expected UDP to be blocked by captive portal, got: {:?}",
        udp_result
    );

    let tcp_bind = SocketAddr::new(IpAddr::V4(dc_ip), 8080);
    dc.spawn(async move |_| spawn_tcp_echo_server(tcp_bind).await)?
        .await??;
    dev.spawn(async move |_| tcp_roundtrip(tcp_bind).await)?
        .await??;

    Ok(())
}

/// No firewall (default) — all traffic passes.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn none_allows_all() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab.add_router("dc").build().await?;
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    let home = lab.add_router("home").nat(Nat::Home).build().await?;

    let dev = lab
        .add_device("laptop")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 9202);
    dc.spawn_reflector(reflector)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let rtt = dev.run_sync(move || test_utils::udp_rtt(reflector))?;
    assert!(
        rtt < Duration::from_millis(100),
        "expected low RTT, got {rtt:?}"
    );

    Ok(())
}

/// Custom firewall allowing only UDP port 5000, blocking everything else.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn custom_selective() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab.add_router("dc").build().await?;
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    let fw = lab
        .add_router("fw")
        .nat(Nat::Home)
        .firewall_custom(|f| f.allow_udp(&[5000]).block_tcp())
        .build()
        .await?;

    let dev = lab
        .add_device("dev")
        .iface("eth0", fw.id(), None)
        .build()
        .await?;

    let reflector_blocked = SocketAddr::new(IpAddr::V4(dc_ip), 9203);
    dc.spawn_reflector(reflector_blocked)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let blocked = dev.run_sync(move || test_utils::udp_rtt(reflector_blocked));
    assert!(
        blocked.is_err(),
        "expected UDP on port 9203 to be blocked, got: {:?}",
        blocked
    );

    let reflector_allowed = SocketAddr::new(IpAddr::V4(dc_ip), 5000);
    dc.spawn_reflector(reflector_allowed)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let rtt = dev.run_sync(move || test_utils::udp_rtt(reflector_allowed))?;
    assert!(
        rtt < Duration::from_millis(100),
        "expected low RTT on allowed port, got {rtt:?}"
    );

    Ok(())
}

/// Apply firewall at runtime, then remove it.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn runtime_change() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab.add_router("dc").build().await?;
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    let home = lab.add_router("home").nat(Nat::Home).build().await?;

    let dev = lab
        .add_device("laptop")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 9204);
    dc.spawn_reflector(reflector)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let rtt = dev.run_sync(move || test_utils::udp_rtt(reflector))?;
    assert!(rtt < Duration::from_millis(100));

    home.set_firewall(Firewall::Corporate).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let blocked = dev.run_sync(move || test_utils::udp_rtt(reflector));
    assert!(
        blocked.is_err(),
        "expected UDP to be blocked after applying firewall, got: {:?}",
        blocked
    );

    home.set_firewall(Firewall::None).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let rtt = dev.run_sync(move || test_utils::udp_rtt(reflector))?;
    assert!(rtt < Duration::from_millis(100));

    Ok(())
}
