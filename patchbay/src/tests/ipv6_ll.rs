//! IPv6 link-local focused tests.

use std::net::Ipv6Addr;

use super::*;

fn is_link_local(ip: Ipv6Addr) -> bool {
    ip.segments()[0] & 0xffc0 == 0xfe80
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn link_local_presence_on_all_ipv6_ifaces() -> Result<()> {
    check_caps()?;

    let lab = Lab::with_opts(LabOpts::default().ipv6_dad_mode(Ipv6DadMode::Disabled)).await?;
    let dc = lab
        .add_router("dc")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;
    let dev = lab.add_device("dev").uplink(dc.id()).build().await?;

    let iface = dev.default_iface().context("missing default iface")?;
    let ll6 = iface.ll6().context("missing device ll6")?;
    assert!(
        is_link_local(ll6),
        "device ll6 should be fe80::/10, got {ll6}"
    );

    let ifaces = dc.interfaces();
    assert!(!ifaces.is_empty(), "router should expose interfaces");
    for rif in ifaces {
        let ll = rif.ll6().context("missing router ll6")?;
        assert!(
            is_link_local(ll),
            "router iface {} ll6 should be fe80::/10, got {ll}",
            rif.name()
        );
    }

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn router_iface_api_exposes_ll6_consistently() -> Result<()> {
    check_caps()?;

    let lab = Lab::new().await?;
    let dc = lab
        .add_router("dc")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;

    let all = dc.interfaces();
    assert!(
        all.len() >= 2,
        "router should expose wan and bridge interfaces"
    );

    for iface in &all {
        let by_name = dc
            .iface(iface.name())
            .context("iface lookup by name failed")?;
        assert_eq!(
            iface.ll6(),
            by_name.ll6(),
            "ll6 mismatch for iface {}",
            iface.name()
        );
    }

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn dad_disabled_deterministic_mode() -> Result<()> {
    check_caps()?;

    let lab = Lab::with_opts(LabOpts::default().ipv6_dad_mode(Ipv6DadMode::Disabled)).await?;
    let dc = lab
        .add_router("dc")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;
    let dev = lab.add_device("dev").uplink(dc.id()).build().await?;

    // Deterministic mode expectation for now: IPv6 and LL are immediately usable.
    assert!(dev.ip6().is_some(), "global/ULA IPv6 should exist");
    assert!(
        dev.default_iface().and_then(|i| i.ll6()).is_some(),
        "link-local IPv6 should exist"
    );

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
#[ignore = "RA/RS provisioning engine follow-up"]
async fn ra_source_is_link_local() -> Result<()> {
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
#[ignore = "RA/RS provisioning engine follow-up"]
async fn host_learns_default_router_from_ra_link_local() -> Result<()> {
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
#[ignore = "RA/RS provisioning engine follow-up"]
async fn router_lifetime_zero_withdraws_default_router() -> Result<()> {
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
#[ignore = "RA/RS provisioning engine follow-up"]
async fn rio_local_routes_without_default_router() -> Result<()> {
    Ok(())
}
