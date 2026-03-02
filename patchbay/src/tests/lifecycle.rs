//! Tests for lab construction, TOML loading, and device/router removal.
//!
//! Covers loading a lab from a TOML file, expanding `count` device configs,
//! custom downstream CIDRs, TCP reflector smoke, and the remove_device /
//! remove_router APIs including the guard that blocks removing a router that
//! still has connected devices.

use super::*;

/// Loading a lab from a TOML file produces the expected devices and routers.
#[tokio::test(flavor = "current_thread")]
async fn load_from_toml() -> Result<()> {
    check_caps()?;
    let toml = r#"
[[router]]
name   = "isp1"
region = "eu"

[[router]]
name   = "dc1"
region = "eu"

[[router]]
name     = "lan1"
upstream = "isp1"
nat      = "home"

[device.dev1.eth0]
gateway = "lan1"
"#;
    let tmp = std::env::temp_dir().join("patchbay_test_lab.toml");
    std::fs::write(&tmp, toml)?;

    let lab = Lab::load(&tmp).await?;
    assert!(lab.device_by_name("dev1").is_some());
    Ok(())
}

/// A device config with `count = 2` expands into two numbered devices.
#[tokio::test(flavor = "current_thread")]
async fn config_expands_count() -> Result<()> {
    let cfg = r#"
[[router]]
name = "dc1"

[device.fetcher]
count = 2
default_via = "eth0"

[device.fetcher.eth0]
gateway = "dc1"
"#;
    let parsed: config::LabConfig = toml::from_str(cfg)?;
    let lab = Lab::from_config(parsed).await?;
    assert!(lab.device_by_name("fetcher-0").is_some());
    assert!(lab.device_by_name("fetcher-1").is_some());
    assert!(lab.device_by_name("fetcher").is_none());
    Ok(())
}

/// A router built with a custom downstream CIDR assigns addresses from that subnet.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn custom_downstream_cidr() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let custom = lab
        .add_router("custom")
        .nat(Nat::Home)
        .downstream_cidr("172.30.99.0/24".parse()?)
        .build()
        .await?;

    // Router's downstream gateway should be .1 of the custom CIDR.
    assert_eq!(
        custom.downstream_gw(),
        Some(Ipv4Addr::new(172, 30, 99, 1)),
        "router gateway should be 172.30.99.1"
    );
    assert_eq!(
        custom.downstream_cidr().unwrap().to_string(),
        "172.30.99.0/24",
    );

    // Device gets .2 from the custom subnet.
    let dev = lab
        .add_device("dev")
        .iface("eth0", custom.id(), None)
        .build()
        .await?;
    assert_eq!(
        dev.ip().unwrap(),
        Ipv4Addr::new(172, 30, 99, 2),
        "first device should get 172.30.99.2"
    );

    // Verify connectivity through the custom subnet.
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 17_300);
    dc.spawn_reflector(reflector)?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    dev.run_sync(move || test_utils::udp_roundtrip(reflector))
        .context("udp roundtrip through custom cidr")?;

    Ok(())
}

/// A TCP reflector spawned in a namespace correctly reports the peer's address.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn tcp_reflector_basic() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 13_000);

    dc.spawn(move |_| async move { spawn_tcp_reflector(r).await })?
        .await
        .context("tcp reflector task panicked")??;

    let obs = dev
        .spawn(move |_| async move { probe_tcp(r).await })?
        .await
        .context("probe_tcp task panicked")??;
    assert_ne!(obs.port(), 0, "expected non-zero port");
    Ok(())
}

/// Removing a device clears its runtime state but preserves cached metadata.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn remove_device() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab.add_device("dev").uplink(dc.id()).build().await?;

    // Device works before removal.
    let ns = dev.ns();
    assert!(!ns.is_empty());

    // Remove it.
    lab.remove_device(dev.id())?;

    // After removal, cached fields still work but data accessors return None.
    assert_eq!(dev.name(), "dev");
    assert!(dev.ip().is_none(), "ip() should be None after removal");

    Ok(())
}

/// Removing a router that still has connected devices returns an error.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn remove_router_blocked() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let _dev = lab.add_device("dev").uplink(dc.id()).build().await?;

    // Should fail because device is still connected.
    let err = lab.remove_router(dc.id());
    assert!(err.is_err(), "expected error removing router with device");
    assert!(
        format!("{:?}", err).contains("still connected"),
        "error should mention connected device"
    );

    Ok(())
}

/// Removing a device then its router succeeds; router accessors return None afterward.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn remove_router() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab.add_device("dev").uplink(dc.id()).build().await?;

    // Remove device first, then router.
    lab.remove_device(dev.id())?;
    lab.remove_router(dc.id())?;

    // Router is gone — cached fields still work, data accessors return None.
    assert_eq!(dc.name(), "dc");
    assert!(dc.uplink_ip().is_none(), "uplink_ip() should be None after removal");

    Ok(())
}

/// Devices can be added to an existing lab after the initial build,
/// and the new device has full connectivity.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn add_device_after_build() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev1 = lab.add_device("dev1").uplink(dc.id()).build().await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 20_600);
    dc.spawn_reflector(reflector)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // dev1 works.
    dev1.run_sync(move || test_utils::udp_roundtrip(reflector))
        .context("dev1 roundtrip")?;

    // Add a second device after initial build.
    let dev2 = lab.add_device("dev2").uplink(dc.id()).build().await?;
    assert!(dev2.ip().is_some(), "dev2 should have an IP");
    assert_ne!(dev1.ip(), dev2.ip(), "devices should get different IPs");

    // dev2 has connectivity too.
    dev2.run_sync(move || test_utils::udp_roundtrip(reflector))
        .context("dev2 roundtrip")?;

    // Cross-device ping works (both on same dc subnet).
    let dev1_ip_str = dev1.ip().unwrap().to_string();
    dev2.run_sync(move || ping(&dev1_ip_str))?;

    Ok(())
}
