//! Tests for default route switching and interface replug.
//!
//! Verifies that `set_default_route` and `replug_iface` correctly redirect
//! traffic, updating the reflexive IP and allowing TCP roundtrips through
//! the newly active path.

use super::*;

/// Switching the default route changes the reflexive IP for both UDP and TCP,
/// across all bind modes, and switching back restores the original.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn switch_default_reflexive_ip() -> Result<()> {
    use strum::IntoEnumIterator;
    let DualNatLab {
        _lab,
        dev,
        nat_a,
        nat_b,
        reflector,
        dc: _,
        _reflector_guard,
    } = build_dual_nat_lab(Nat::Home, Nat::Corporate, 16_200).await?;

    let wan_a = nat_a.uplink_ip().context("no uplink ip")?;
    let wan_b = nat_b.uplink_ip().context("no uplink ip")?;

    let mut failures = Vec::new();
    for proto in Proto::iter() {
        for bind in BindMode::iter() {
            // SpecificIp must use the IP of the currently-active interface;
            // device_ip() returns the default_via interface IP, which changes on switch_route.
            let dev_ip = dev.ip().unwrap();
            let obs = probe_reflexive_addr(&dev, proto, bind, dev_ip, reflector).await;
            match obs {
                Ok(o) if o.ip() == IpAddr::V4(wan_a) => {}
                Ok(o) => failures.push(format!(
                    "{proto}/{bind} before switch: expected {wan_a} got {}",
                    o.ip()
                )),
                Err(e) => failures.push(format!("{proto}/{bind} before switch: {e:#}")),
            }

            dev.set_default_route("eth1").await?;
            tokio::time::sleep(Duration::from_millis(50)).await;

            let dev_ip = dev.ip().unwrap();
            let obs = probe_reflexive_addr(&dev, proto, bind, dev_ip, reflector).await;
            match obs {
                Ok(o) if o.ip() == IpAddr::V4(wan_b) => {}
                Ok(o) => failures.push(format!(
                    "{proto}/{bind} after switch: expected {wan_b} got {}",
                    o.ip()
                )),
                Err(e) => failures.push(format!("{proto}/{bind} after switch: {e:#}")),
            }

            dev.set_default_route("eth0").await?;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    if !failures.is_empty() {
        bail!("{} failures:\n{}", failures.len(), failures.join("\n"));
    }
    Ok(())
}

/// Switching the default route multiple times always reflects the correct WAN IP.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn switch_default_multiple_times() -> Result<()> {
    let DualNatLab {
        _lab,
        dc: _,
        dev,
        nat_a,
        nat_b,
        reflector,
        _reflector_guard,
    } = build_dual_nat_lab(Nat::Home, Nat::Home, 16_300).await?;

    let wan_a = nat_a.uplink_ip().context("no uplink ip")?;
    let wan_b = nat_b.uplink_ip().context("no uplink ip")?;

    let o = dev.run_sync(move || test_utils::udp_roundtrip(reflector))?;
    assert_eq!(o.ip(), IpAddr::V4(wan_a), "expected nat_a WAN on eth0");

    dev.set_default_route("eth1").await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let o = dev.run_sync(move || test_utils::udp_roundtrip(reflector))?;
    assert_eq!(o.ip(), IpAddr::V4(wan_b), "expected nat_b WAN on eth1");

    dev.set_default_route("eth0").await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let o = dev.run_sync(move || test_utils::udp_roundtrip(reflector))?;
    assert_eq!(
        o.ip(),
        IpAddr::V4(wan_a),
        "expected nat_a WAN after switch back"
    );

    Ok(())
}

/// TCP roundtrip succeeds on the initial route, and again after switching the
/// default route to the second interface.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn switch_default_tcp_roundtrip() -> Result<()> {
    let DualNatLab {
        _lab,
        dc,
        dev,
        nat_a: _,
        nat_b: _,
        reflector: _,
        _reflector_guard,
    } = build_dual_nat_lab(Nat::Home, Nat::Corporate, 16_400).await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    let r = SocketAddr::new(IpAddr::V4(dc_ip), 16_410);
    dc.spawn(move |_| async move { spawn_tcp_echo_server(r).await })?
        .await
        .context("tcp echo server task panicked")??;
    tokio::time::sleep(Duration::from_millis(200)).await;
    dev.spawn(move |_| async move { tcp_roundtrip(r).await })?
        .await
        .context("tcp roundtrip task panicked")??;

    dev.set_default_route("eth1").await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    dev.spawn(move |_| async move { tcp_roundtrip(r).await })?
        .await
        .context("tcp roundtrip task panicked")??;

    Ok(())
}

/// Replugging an interface from one router to another preserves UDP connectivity.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn replug_iface_udp() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let nat_a = lab.add_router("nat-a").nat(Nat::Home).build().await?;
    let nat_b = lab.add_router("nat-b").nat(Nat::Home).build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", nat_a.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 17_100);
    let _r = dc.spawn_reflector(reflector).await?;

    // Connectivity through nat_a works.
    dev.run_sync(move || test_utils::udp_roundtrip(reflector))
        .context("udp before switch_uplink")?;

    // Move eth0 from nat_a → nat_b.
    dev.replug_iface("eth0", nat_b.id()).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connectivity through nat_b works.
    dev.run_sync(move || test_utils::udp_roundtrip(reflector))
        .context("udp after switch_uplink")?;

    Ok(())
}

/// Replugging an interface changes the reflexive IP to the new router's WAN.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn replug_iface_reflexive_ip() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let nat_a = lab.add_router("nat-a").nat(Nat::Home).build().await?;
    let nat_b = lab.add_router("nat-b").nat(Nat::Home).build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", nat_a.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 17_200);
    let _r = dc.spawn_reflector(reflector).await?;

    let wan_a = nat_a.uplink_ip().context("no nat_a uplink ip")?;
    let wan_b = nat_b.uplink_ip().context("no nat_b uplink ip")?;

    let before = dev.run_sync(move || test_utils::udp_roundtrip(reflector))?;
    assert_eq!(
        before.ip(),
        IpAddr::V4(wan_a),
        "before switch: expected nat_a WAN IP"
    );

    dev.replug_iface("eth0", nat_b.id()).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let after = dev.run_sync(move || test_utils::udp_roundtrip(reflector))?;
    assert_eq!(
        after.ip(),
        IpAddr::V4(wan_b),
        "after switch: expected nat_b WAN IP"
    );
    assert_ne!(
        before.ip(),
        after.ip(),
        "reflexive IP must change after uplink switch"
    );
    Ok(())
}
