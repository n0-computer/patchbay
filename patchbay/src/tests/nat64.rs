//! NAT64 integration tests.

use super::*;

/// A device behind a NAT64 router can reach a v4-only server via the
/// well-known NAT64 prefix `64:ff9b::<ipv4>`.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn nat64_udp_v6_to_v4() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    // v4-only datacenter router and server.
    let dc = lab.add_router("dc").build().await?;
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    // NAT64 carrier router: DualStack with NAT64.
    let carrier = lab
        .add_router("carrier")
        .ip_support(IpSupport::DualStack)
        .nat(Nat::Home)
        .nat_v6(NatV6Mode::Nat64)
        .build()
        .await?;

    let phone = lab
        .add_device("phone")
        .iface("eth0", carrier.id(), None)
        .build()
        .await?;

    // Phone should have a v6 address.
    let _phone_v6 = phone.ip6().context("phone should have v6 address")?;
    assert!(
        phone.ip().is_some(),
        "phone should also have v4 (dual-stack)"
    );

    // Start a UDP reflector on the datacenter server.
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 9300);
    let _r = dc.spawn_reflector(reflector).await?;

    // Build the NAT64 address: embed the dc's IPv4 into 64:ff9b::/96.
    let nat64_addr = crate::nat64::embed_v4_in_nat64(dc_ip);
    let nat64_target = SocketAddr::new(IpAddr::V6(nat64_addr), 9300);

    // Phone sends UDP via the NAT64 prefix — should reach the v4 server.
    let rtt = phone.run_sync(move || test_utils::udp_rtt_sync(nat64_target))?;
    assert!(
        rtt < Duration::from_millis(500),
        "NAT64 UDP roundtrip should work, got {rtt:?}"
    );

    Ok(())
}

/// NAT64 works for TCP too — phone connects to v4 server via NAT64 prefix.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn nat64_tcp_v6_to_v4() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab.add_router("dc").build().await?;
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    let carrier = lab
        .add_router("carrier")
        .ip_support(IpSupport::DualStack)
        .nat(Nat::Home)
        .nat_v6(NatV6Mode::Nat64)
        .build()
        .await?;

    let phone = lab
        .add_device("phone")
        .iface("eth0", carrier.id(), None)
        .build()
        .await?;

    // Start a TCP echo server on the dc.
    let tcp_bind = SocketAddr::new(IpAddr::V4(dc_ip), 9301);
    dc.spawn(async move |_| spawn_tcp_echo_server(tcp_bind).await)?
        .await??;

    // Phone connects via NAT64 prefix.
    let nat64_addr = crate::nat64::embed_v4_in_nat64(dc_ip);
    let nat64_target = SocketAddr::new(IpAddr::V6(nat64_addr), 9301);

    phone
        .spawn(async move |_| tcp_roundtrip(nat64_target).await)?
        .await??;

    Ok(())
}

/// Verify that regular IPv6 traffic (not via NAT64 prefix) still works
/// alongside NAT64.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn nat64_preserves_native_v6() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab
        .add_router("dc")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    let carrier = lab
        .add_router("carrier")
        .ip_support(IpSupport::DualStack)
        .nat(Nat::Home)
        .nat_v6(NatV6Mode::Nat64)
        .build()
        .await?;

    let phone = lab
        .add_device("phone")
        .iface("eth0", carrier.id(), None)
        .build()
        .await?;

    // Regular v4 via NAT should still work.
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 9302);
    let _r = dc.spawn_reflector(reflector).await?;

    let rtt = phone.run_sync(move || test_utils::udp_rtt_sync(reflector))?;
    assert!(
        rtt < Duration::from_millis(500),
        "regular v4 via NAT should still work"
    );

    Ok(())
}
