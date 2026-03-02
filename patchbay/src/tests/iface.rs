//! Tests for interface management at runtime.
//!
//! Covers adding and removing interfaces on a live device, renewing a device's
//! IP address via the router's DHCP pool, and assigning secondary IP addresses
//! to an existing interface.

use super::*;

/// Adding a second interface and removing the first works correctly.
///
/// Validates IP assignment, default-route switching, connectivity through
/// the new interface, and that removing the last interface is rejected.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn add_remove_runtime() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let home = lab.add_router("home").nat(Nat::Home).build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 17_300);
    dc.spawn_reflector(reflector)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Device initially has one interface.
    assert_eq!(dev.interfaces().len(), 1);

    // Add a second interface on the dc router.
    dev.add_iface("eth1", dc.id(), None).await?;
    assert_eq!(dev.interfaces().len(), 2);
    assert!(dev.iface("eth1").is_some(), "eth1 should exist after add");

    // eth1 got a public IP from dc's pool.
    let eth1_ip = dev.iface("eth1").unwrap().ip().expect("eth1 should have an IP");
    assert!(
        eth1_ip.octets()[0] == 198,
        "eth1 IP should be in the public range, got {eth1_ip}"
    );

    // Switch default route to eth1 and verify connectivity through dc.
    dev.set_default_route("eth1").await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let obs = dev.run_sync(move || test_utils::udp_roundtrip(reflector))?;
    assert_eq!(
        obs.ip(),
        IpAddr::V4(eth1_ip),
        "reflector should see device's eth1 IP (dc has no NAT)"
    );

    // Remove the original interface.
    dev.remove_iface("eth0").await?;
    assert_eq!(dev.interfaces().len(), 1);
    assert!(dev.iface("eth0").is_none(), "eth0 should be gone");

    // Connectivity still works through the remaining eth1.
    let obs2 = dev.run_sync(move || test_utils::udp_roundtrip(reflector))?;
    assert_eq!(obs2.ip(), IpAddr::V4(eth1_ip));

    // Cannot remove the last interface.
    let err = dev.remove_iface("eth1").await;
    assert!(err.is_err(), "removing last interface should fail");

    // Duplicate name rejected.
    let err = dev.add_iface("eth1", dc.id(), None).await;
    assert!(err.is_err(), "duplicate interface name should fail");

    Ok(())
}

/// Renewing a device's IP returns a new address and the handle reflects the change.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn renew_ip() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab.add_device("dev").uplink(dc.id()).build().await?;

    let old_ip = dev.ip().unwrap();
    let new_ip = dev.renew_ip("eth0").await?;

    assert_ne!(old_ip, new_ip, "renewed IP should differ from old");
    assert_eq!(dev.ip().unwrap(), new_ip, "handle should reflect new IP");

    // Verify the new IP is reachable from DC side.
    let relay = lab.add_device("relay").uplink(dc.id()).build().await?;
    let target = new_ip.to_string();
    relay.run_sync(move || ping(&target))?;

    Ok(())
}

/// Adding a secondary IP to an interface makes both addresses reachable.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn add_secondary_ip() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab.add_device("dev").uplink(dc.id()).build().await?;

    let primary = dev.ip().unwrap();
    // Pick a secondary IP in the same subnet.
    let cidr = dc.downstream_cidr().unwrap();
    let octets = cidr.addr().octets();
    let secondary = Ipv4Addr::new(octets[0], octets[1], octets[2], 200);
    dev.add_ip("eth0", secondary, cidr.prefix_len()).await?;

    // Both addresses should be reachable.
    let relay = lab.add_device("relay").uplink(dc.id()).build().await?;
    let p = primary.to_string();
    relay.run_sync(move || ping(&p))?;
    let s = secondary.to_string();
    relay.run_sync(move || ping(&s))?;

    Ok(())
}

/// Replugging an interface to a different router assigns a new IP from
/// the new router's subnet and establishes connectivity through it.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn replug_to_different_subnet() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc_a = lab.add_router("dc-a").build().await?;
    let dc_b = lab
        .add_router("dc-b")
        .downstream_cidr("172.20.0.0/24".parse()?)
        .nat(Nat::Home)
        .build()
        .await?;

    let dev = lab
        .add_device("dev")
        .iface("eth0", dc_a.id(), None)
        .build()
        .await?;

    let old_ip = dev.ip().unwrap();
    // Old IP should be in dc_a's public range.
    assert_eq!(old_ip.octets()[0], 198, "initially in dc_a range");

    // Replug to dc_b.
    dev.replug_iface("eth0", dc_b.id()).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let new_ip = dev.ip().unwrap();
    assert_eq!(
        new_ip.octets()[0..3],
        [172, 20, 0],
        "after replug should be in dc_b's 172.20.0.0/24 subnet, got {new_ip}"
    );
    assert_ne!(old_ip, new_ip);

    // Connectivity through dc_b works.
    let dc_a_ip = dc_a.uplink_ip().context("dc_a uplink")?;
    let reflector = SocketAddr::new(IpAddr::V4(dc_a_ip), 20_300);
    dc_a.spawn_reflector(reflector)?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    dev.run_sync(move || test_utils::udp_roundtrip(reflector))
        .context("udp roundtrip after replug")?;

    Ok(())
}
