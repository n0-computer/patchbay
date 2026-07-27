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
    let home = lab.add_router("home").nat(Nat::Moderate).build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", home.id())
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 17_300);
    let _r = dc.spawn_reflector(reflector).await?;

    // Device initially has one interface.
    assert_eq!(dev.interfaces().len(), 1);

    // Add a second interface on the dc router.
    dev.add_iface("eth1", dc.id()).await?;
    assert_eq!(dev.interfaces().len(), 2);
    assert!(dev.iface("eth1").is_some(), "eth1 should exist after add");

    // eth1 got a public IP from dc's pool.
    let eth1_ip = dev
        .iface("eth1")
        .unwrap()
        .ip()
        .expect("eth1 should have an IP");
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
    dev.iface("eth0").unwrap().remove().await?;
    assert_eq!(dev.interfaces().len(), 1);
    assert!(dev.iface("eth0").is_none(), "eth0 should be gone");

    // Connectivity still works through the remaining eth1.
    let obs2 = dev.run_sync(move || test_utils::udp_roundtrip(reflector))?;
    assert_eq!(obs2.ip(), IpAddr::V4(eth1_ip));

    // Cannot remove the last interface.
    let err = dev.iface("eth1").unwrap().remove().await;
    assert!(err.is_err(), "removing last interface should fail");

    // Duplicate name rejected.
    let err = dev.add_iface("eth1", dc.id()).await;
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
    let new_ip = dev.iface("eth0").unwrap().renew_ip().await?;

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
    dev.iface("eth0")
        .unwrap()
        .add_ip(secondary, cidr.prefix_len())
        .await?;

    // Both addresses should be reachable.
    let relay = lab.add_device("relay").uplink(dc.id()).build().await?;
    let p = primary.to_string();
    relay.run_sync(move || ping(&p))?;
    let s = secondary.to_string();
    relay.run_sync(move || ping(&s))?;

    Ok(())
}

/// Build-time dummy interface has the configured address and no gateway.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn dummy_build_time() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let addr: Ipv4Net = "172.17.0.2/16".parse()?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id())
        .iface("docker0", IfaceConfig::dummy().addr(addr))
        .build()
        .await?;

    let docker0 = dev.iface("docker0").expect("docker0 should exist");
    assert!(docker0.is_dummy());
    assert!(!docker0.is_routed());
    assert_eq!(docker0.ip(), Some(addr.addr()));

    let eth0 = dev.iface("eth0").expect("eth0 should exist");
    assert!(eth0.is_routed());
    assert!(!eth0.is_dummy());

    Ok(())
}

/// Runtime dummy interface with explicit address.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn dummy_runtime() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    let addr: Ipv4Net = "10.8.0.1/24".parse()?;
    let tun = dev
        .add_iface("tun0", IfaceConfig::dummy().addr(addr))
        .await?;

    assert!(tun.is_dummy());
    assert_eq!(tun.ip(), Some(addr.addr()));
    assert_eq!(dev.interfaces().len(), 2);

    Ok(())
}

/// Bare dummy interface (no address) can be created.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn dummy_bare() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id())
        .iface("bare0", IfaceConfig::dummy())
        .build()
        .await?;

    let bare = dev.iface("bare0").expect("bare0 should exist");
    assert!(bare.is_dummy());
    assert!(bare.ip().is_none());

    Ok(())
}

/// Build-time conditions are applied correctly.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn build_time_conditions() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            IfaceConfig::routed(dc.id())
                .condition(LinkCondition::mobile_4g(), LinkDirection::Egress),
        )
        .build()
        .await?;

    let eth0 = dev.iface("eth0").expect("eth0 should exist");
    assert_eq!(eth0.egress(), Some(LinkCondition::mobile_4g()));
    assert!(eth0.ingress().is_none());

    Ok(())
}

/// Asymmetric conditions: different egress and ingress.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn asymmetric_conditions() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface(
            "eth0",
            IfaceConfig::routed(dc.id())
                .condition(LinkCondition::mobile_4g(), LinkDirection::Egress)
                .condition(LinkCondition::mobile_3g(), LinkDirection::Ingress),
        )
        .build()
        .await?;

    let eth0 = dev.iface("eth0").expect("eth0 should exist");
    assert_eq!(eth0.egress(), Some(LinkCondition::mobile_4g()));
    assert_eq!(eth0.ingress(), Some(LinkCondition::mobile_3g()));

    Ok(())
}

/// Replug and renew_ip return errors on dummy interfaces.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn dummy_guards() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id())
        .iface("tun0", IfaceConfig::dummy().addr("10.8.0.1/24".parse()?))
        .build()
        .await?;

    let tun = dev.iface("tun0").unwrap();

    // Cannot replug a dummy interface.
    let err = tun.replug(dc.id()).await;
    assert!(err.is_err(), "replug on dummy should fail");

    // Cannot renew IP on a dummy interface.
    let err = tun.renew_ip().await;
    assert!(err.is_err(), "renew_ip on dummy should fail");

    // Cannot set ingress condition on a dummy interface.
    let err = tun
        .set_condition(LinkCondition::mobile_4g(), LinkDirection::Ingress)
        .await;
    assert!(err.is_err(), "set_condition ingress on dummy should fail");

    // Egress condition works on a dummy interface.
    tun.set_condition(LinkCondition::mobile_4g(), LinkDirection::Egress)
        .await?;
    assert_eq!(tun.egress(), Some(LinkCondition::mobile_4g()));

    Ok(())
}

/// Iface handle returns errors after the interface is removed.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn iface_handle_after_removal() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id())
        .iface("eth1", dc.id())
        .build()
        .await?;

    let eth0 = dev.iface("eth0").unwrap();
    eth0.remove().await?;

    // The handle is now stale.
    assert!(eth0.ip().is_none());
    let err = eth0.link_down().await;
    assert!(err.is_err(), "link_down on removed iface should fail");

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
        .nat(Nat::Moderate)
        .build()
        .await?;

    let dev = lab
        .add_device("dev")
        .iface("eth0", dc_a.id())
        .build()
        .await?;

    let old_ip = dev.ip().unwrap();
    // Old IP should be in dc_a's public range.
    assert_eq!(old_ip.octets()[0], 198, "initially in dc_a range");

    // Replug to dc_b.
    dev.iface("eth0").unwrap().replug(dc_b.id()).await?;
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
    let _r = dc_a.spawn_reflector(reflector).await?;
    dev.run_sync(move || test_utils::udp_roundtrip(reflector))
        .context("udp roundtrip after replug")?;

    Ok(())
}

/// Build-time down() creates an interface in link-down state.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn build_time_down() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", IfaceConfig::routed(dc.id()).down())
        .build()
        .await?;

    // Interface should exist but link should be down.
    let eth0 = dev.iface("eth0").expect("eth0 exists");
    assert!(eth0.ip().is_some(), "eth0 should have an IP");

    // Verify link is DOWN by checking `ip link show eth0` output.
    let mut cmd = std::process::Command::new("ip");
    cmd.args(["link", "show", "eth0"]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = dev.spawn_command_sync(cmd)?;
    let output = child.wait_with_output().context("ip link show")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("state DOWN"),
        "interface should be in DOWN state, got: {stdout}"
    );

    // Bring it up and verify link comes up.
    eth0.link_up().await?;

    let mut cmd2 = std::process::Command::new("ip");
    cmd2.args(["link", "show", "eth0"]);
    cmd2.stdout(std::process::Stdio::piped());
    cmd2.stderr(std::process::Stdio::piped());
    let child2 = dev.spawn_command_sync(cmd2)?;
    let output2 = child2.wait_with_output().context("ip link show after up")?;
    let stdout2 = String::from_utf8_lossy(&output2.stdout);
    assert!(
        !stdout2.contains("state DOWN"),
        "interface should no longer be DOWN after link_up, got: {stdout2}"
    );

    Ok(())
}

/// Build-time addr() on a routed interface overrides pool allocation.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn routed_explicit_addr() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;

    let explicit_ip: Ipv4Net = "198.18.1.99/24".parse()?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", IfaceConfig::routed(dc.id()).addr(explicit_ip))
        .build()
        .await?;

    let eth0 = dev.iface("eth0").expect("eth0 exists");
    assert_eq!(
        eth0.ip(),
        Some(explicit_ip.addr()),
        "should have the explicitly set IP, not a pool-allocated one"
    );

    Ok(())
}

/// Duplicate interface names at build time are rejected.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn duplicate_iface_name_rejected() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;

    let result = lab
        .add_device("dev")
        .iface("eth0", dc.id())
        .iface("eth0", dc.id())
        .build()
        .await;

    assert!(
        result.is_err(),
        "duplicate interface name should be rejected"
    );
    Ok(())
}
