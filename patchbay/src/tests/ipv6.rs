//! Tests for IPv6 dual-stack and v6-only behavior.
//!
//! Verifies that routers and devices expose correct v6 accessors, that
//! v6-only configurations carry no v4 routes, and that dual-stack labs
//! produce valid public addresses on both protocol families.

use super::*;

/// Router handle v6 accessors return correct values for dual-stack and v4-only routers.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn router_accessors() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab
        .add_router("dc")
        .ip_support(IpSupport::DualStack)
        .nat_v6(NatV6Mode::Masquerade)
        .build()
        .await?;

    assert_eq!(dc.ip_support(), Some(IpSupport::DualStack));
    assert_eq!(dc.nat_v6_mode(), Some(NatV6Mode::Masquerade));
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
    assert_eq!(dc4.ip_support(), Some(IpSupport::V4Only));
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

/// Device handle v6 accessor returns an address for dual-stack and None for v4-only.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn device_accessors() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
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
    let iface = dev.default_iface().unwrap();
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

/// V6-only DC + device: v6 roundtrip succeeds and v4 ping fails (no v4 routes).
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn v6_only_no_v4_routes() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
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
    let _r = dc.spawn_reflector(r_v6).await?;
    let o = dev.run_sync(move || test_utils::udp_roundtrip(r_v6))?;
    assert!(o.ip().is_ipv6(), "reflexive should be v6");

    // v4 ping to the IX gateway should fail (no v4 routes).
    let res = dev.run_sync(|| ping("203.0.113.1"));
    assert!(res.is_err(), "v4 ping should fail under V6Only");

    Ok(())
}

/// Dual-stack DC with no NAT: v4 reflexive is v4 and v6 reflexive is v6.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn dual_stack_public_addrs() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
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
    let _r = dc.spawn_reflector(r_v4).await?;

    // v6 reflector
    let dc_ip_v6 = dc.uplink_ip_v6().expect("dc v6 uplink");
    let r_v6 = SocketAddr::new(IpAddr::V6(dc_ip_v6), 3493);
    let _r = dc.spawn_reflector(r_v6).await?;

    let o_v4 = dev.run_sync(move || test_utils::udp_roundtrip(r_v4))?;
    assert!(o_v4.ip().is_ipv4(), "v4 reflexive should be v4");

    let o_v6 = dev.run_sync(move || test_utils::udp_roundtrip(r_v6))?;
    assert!(o_v6.ip().is_ipv6(), "v6 reflexive should be v6");

    Ok(())
}

/// TCP echo roundtrip works over a v6-only network.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn v6_only_tcp_roundtrip() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab
        .add_router("dc")
        .ip_support(IpSupport::V6Only)
        .build()
        .await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    let dc_v6 = dc.uplink_ip_v6().expect("dc v6 uplink");
    let bind = SocketAddr::new(IpAddr::V6(dc_v6), 20_200);
    dc.spawn(move |_| async move { spawn_tcp_echo_server(bind).await })?
        .await
        .context("tcp echo server task panicked")??;
    tokio::time::sleep(Duration::from_millis(200)).await;
    dev.spawn(move |_| async move { tcp_roundtrip(bind).await })?
        .await
        .context("tcp roundtrip task panicked")??;

    Ok(())
}

/// Verify raw UDP connectivity (v4 + v6) through a dual-stack home NAT with NPTv6.
#[tokio::test]
#[traced_test]
async fn dual_stack_home_nat_udp() -> Result<()> {
    let lab = Lab::new().await?;

    // DC router: dual-stack, no NAT (hosting the reflector server).
    let dc = lab
        .add_router("dc")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;

    // Reflector server device on dc.
    let server = lab.add_device("server").uplink(dc.id()).build().await?;
    let server_v4 = server.ip().expect("server has IPv4");
    let server_v6 = server.ip6().expect("server has IPv6");
    info!(%server_v4, %server_v6, "reflector server");

    // Spawn reflectors on both address families.
    let reflector_v4: SocketAddr = (server_v4, 3478).into();
    let reflector_v6: SocketAddr = (server_v6, 3479).into();
    let _r = server.spawn_reflector(reflector_v4).await?;
    let _r = server.spawn_reflector(reflector_v6).await?;

    // Home NAT router: dual-stack with NPTv6.
    let nat = lab
        .add_router("nat")
        .nat(Nat::Home)
        .nat_v6(NatV6Mode::Nptv6)
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;

    // Client device behind NAT.
    let dev = lab.add_device("dev").uplink(nat.id()).build().await?;
    let dev_v4 = dev.ip().expect("device has IPv4");
    let dev_v6 = dev.ip6().expect("device has IPv6");
    info!(%dev_v4, %dev_v6, "client device");

    // Probe v4: should succeed and show a NATted address.
    let observed_v4 = dev.probe_udp_mapping(reflector_v4)?;
    info!(%observed_v4, "observed v4 address");
    assert!(observed_v4.is_ipv4(), "v4 probe should return IPv4 addr");
    assert_ne!(
        observed_v4.ip(),
        IpAddr::V4(dev_v4),
        "v4 address should be NATted (differ from device private IP)"
    );

    // Probe v6: should succeed through NPTv6.
    let observed_v6 = dev.probe_udp_mapping(reflector_v6)?;
    info!(%observed_v6, "observed v6 address");
    assert!(observed_v6.is_ipv6(), "v6 probe should return IPv6 addr");
    // With NPTv6, the prefix is translated, so the observed address differs from the device's.
    info!(
        dev_v6 = %dev_v6,
        observed_v6 = %observed_v6.ip(),
        "v6 addresses (should differ with NPTv6 prefix translation)"
    );

    Ok(())
}
