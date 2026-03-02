//! Basic connectivity: ping, UDP/TCP roundtrip, same-LAN reachability.

use super::*;

/// Device pings its Home NAT router's downstream gateway.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn ping_gateway() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let isp = lab.add_router("isp1").build().await?;
    let home = lab
        .add_router("home1")
        .upstream(isp.id())
        .nat(Nat::Home)
        .build()
        .await?;
    let dev = lab
        .add_device("dev1")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    let lan_gw = home.downstream_gw().context("no downstream gw")?;
    let lan_gw_str = lan_gw.to_string();
    dev.run_sync(move || ping(&lan_gw_str))?;
    Ok(())
}

/// UDP roundtrip from a NATted device to a DC reflector.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn udp_roundtrip() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let isp = lab.add_router("isp1").build().await?;
    let dc = lab.add_router("dc1").build().await?;
    let home = lab
        .add_router("home1")
        .upstream(isp.id())
        .nat(Nat::Home)
        .build()
        .await?;
    let dev = lab
        .add_device("dev1")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 3478);
    dc.spawn_reflector(r)?;

    tokio::time::sleep(Duration::from_millis(250)).await;

    let _ = dev.run_sync(move || test_utils::udp_roundtrip(r))?;
    Ok(())
}

/// TCP echo roundtrip from a NATted device to a DC echo server.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn tcp_roundtrip() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let isp = lab.add_router("isp1").build().await?;
    let dc = lab.add_router("dc1").build().await?;
    let home = lab
        .add_router("home1")
        .upstream(isp.id())
        .nat(Nat::Home)
        .build()
        .await?;
    let dev = lab
        .add_device("dev1")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let bind = SocketAddr::new(IpAddr::V4(dc_ip), 9000);
    dc.spawn(move |_| async move { spawn_tcp_echo_server(bind).await })?
        .await
        .context("tcp echo task panicked")??;

    tokio::time::sleep(Duration::from_millis(250)).await;

    dev.spawn(move |_| async move { super::tcp_roundtrip(bind).await })?
        .await
        .context("tcp roundtrip task panicked")??;
    Ok(())
}

/// Home router pings its ISP's downstream gateway.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn ping_router_to_isp() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let isp = lab.add_router("isp1").build().await?;
    let home = lab
        .add_router("home1")
        .upstream(isp.id())
        .nat(Nat::Home)
        .build()
        .await?;

    let isp_wan_ip = isp.downstream_gw().context("no downstream gw")?;
    let isp_wan_str = isp_wan_ip.to_string();
    home.run_sync(move || ping(&isp_wan_str))?;
    Ok(())
}

/// ISP router pings both the IX gateway and a DC router.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn ping_isp_to_ix_and_dc() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let isp = lab.add_router("isp1").build().await?;
    let dc = lab.add_router("dc1").build().await?;

    let ix_gw_str = lab.ix().gw().to_string();
    isp.run_sync(move || ping(&ix_gw_str))?;
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let dc_ip_str = dc_ip.to_string();
    isp.run_sync(move || ping(&dc_ip_str))?;
    Ok(())
}

/// Devices behind separate Home NATs can both ping a public relay.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn ping_through_nat_to_relay() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab.add_router("dc").build().await?;
    let lan_provider = lab
        .add_router("lan-provider")
        .nat(Nat::Home)
        .build()
        .await?;
    let lan_fetcher = lab.add_router("lan-fetcher").nat(Nat::Home).build().await?;

    let relay = lab
        .add_device("relay")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;
    let provider = lab
        .add_device("provider")
        .iface("eth0", lan_provider.id(), None)
        .build()
        .await?;
    let fetcher = lab
        .add_device("fetcher")
        .iface("eth0", lan_fetcher.id(), None)
        .build()
        .await?;

    let relay_ip = relay.ip().unwrap();
    let relay_ip_str = relay_ip.to_string();
    let relay_ip_str2 = relay_ip_str.clone();
    provider.run_sync(move || ping(&relay_ip_str))?;
    fetcher.run_sync(move || ping(&relay_ip_str2))?;
    Ok(())
}

/// Two devices on the same LAN can ping each other directly.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn ping_same_lan() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let isp = lab.add_router("isp1").build().await?;
    let home = lab
        .add_router("home1")
        .upstream(isp.id())
        .nat(Nat::Home)
        .build()
        .await?;
    let dev1 = lab
        .add_device("dev1")
        .iface("eth0", home.id(), None)
        .build()
        .await?;
    let dev2 = lab
        .add_device("dev2")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    let dev2_ip_str = dev2.ip().unwrap().to_string();
    dev1.run_sync(move || ping(&dev2_ip_str))?;
    Ok(())
}

/// Dual-stack DC + device: both v4 and v6 UDP roundtrips succeed.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn dual_stack_roundtrip() -> Result<()> {
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

    assert!(dev.ip().is_some(), "device should have v4 addr");
    assert!(dev.ip6().is_some(), "device should have v6 addr");

    let dc_ip_v4 = dc.uplink_ip().expect("dc should have v4 uplink");
    let r_v4 = SocketAddr::new(IpAddr::V4(dc_ip_v4), 3480);
    dc.spawn_reflector(r_v4)?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let o_v4 = dev.run_sync(move || test_utils::udp_roundtrip(r_v4))?;
    assert_eq!(
        o_v4.ip(),
        IpAddr::V4(dev.ip().unwrap()),
        "v4 reflexive should be device IP (no NAT)"
    );

    let dc_ip_v6 = dc.uplink_ip_v6().expect("dc should have v6 uplink");
    let r_v6 = SocketAddr::new(IpAddr::V6(dc_ip_v6), 3481);
    dc.spawn_reflector(r_v6)?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let o_v6 = dev.run_sync(move || test_utils::udp_roundtrip(r_v6))?;
    assert!(o_v6.ip().is_ipv6(), "v6 reflexive should be IPv6");

    Ok(())
}

/// V6-only DC + device: v6 roundtrip succeeds, no v4 addresses.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn v6_only_roundtrip() -> Result<()> {
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

    let dev_ip6 = dev.ip6().expect("device should have v6 addr");
    assert!(!dev_ip6.is_unspecified(), "v6 addr must not be unspecified");
    assert!(dev.ip().is_none(), "V6Only device should have no v4 addr");
    assert!(
        dc.uplink_ip().is_none(),
        "V6Only router should have no v4 uplink"
    );

    let dc_ip_v6 = dc.uplink_ip_v6().expect("dc v6 uplink");
    let r_v6 = SocketAddr::new(IpAddr::V6(dc_ip_v6), 3490);
    dc.spawn_reflector(r_v6)?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let o = dev.run_sync(move || test_utils::udp_roundtrip(r_v6))?;
    assert!(o.ip().is_ipv6(), "reflexive should be v6");
    Ok(())
}

/// Lab without regions has no netem overhead — RTT under 10ms.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn no_region_overhead() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc1 = lab.add_router("dc1").build().await?;
    let dc2 = lab.add_router("dc2").build().await?;

    let dev = lab
        .add_device("dev")
        .iface("eth0", dc1.id(), None)
        .build()
        .await?;

    let dc2_ip = dc2.uplink_ip().context("no uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc2_ip), 9103);
    dc2.spawn_reflector(r)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let rtt = dev.run_sync(move || test_utils::udp_rtt_sync(r))?;
    assert!(
        rtt < Duration::from_millis(10),
        "expected no-region RTT < 10ms, got {rtt:?}"
    );
    Ok(())
}
