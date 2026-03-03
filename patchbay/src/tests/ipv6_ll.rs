//! IPv6 link-local focused tests.

use std::{
    fs,
    net::Ipv6Addr,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use super::*;

fn is_link_local(ip: Ipv6Addr) -> bool {
    ip.segments()[0] & 0xffc0 == 0xfe80
}

async fn wait_for_file_contains(path: &Path, needle: &str, timeout: Duration) -> Result<bool> {
    let start = tokio::time::Instant::now();
    while start.elapsed() < timeout {
        if let Ok(content) = fs::read_to_string(path) {
            if content.contains(needle) {
                return Ok(true);
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(false)
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
async fn radriven_default_route_uses_scoped_ll_and_switches_iface() -> Result<()> {
    check_caps()?;

    let lab = Lab::with_opts(
        LabOpts::default()
            .ipv6_dad_mode(Ipv6DadMode::Disabled)
            .ipv6_provisioning_mode(Ipv6ProvisioningMode::RaDriven),
    )
    .await?;
    let r1 = lab
        .add_router("r1")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;
    let r2 = lab
        .add_router("r2")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", r1.id(), None)
        .iface("eth1", r2.id(), None)
        .default_via("eth0")
        .build()
        .await?;

    let route0 = dev.run_sync(|| {
        let out = std::process::Command::new("ip")
            .args(["-6", "route", "show", "default"])
            .output()?;
        if !out.status.success() {
            anyhow::bail!("ip -6 route failed with status {}", out.status);
        }
        Ok::<_, anyhow::Error>(String::from_utf8_lossy(&out.stdout).to_string())
    })?;
    assert!(
        route0.contains("via fe80:"),
        "expected link-local default route, got: {route0:?}"
    );
    assert!(
        route0.contains("dev eth0"),
        "expected default route via eth0, got: {route0:?}"
    );

    dev.set_default_route("eth1").await?;

    let route1 = dev.run_sync(|| {
        let out = std::process::Command::new("ip")
            .args(["-6", "route", "show", "default"])
            .output()?;
        if !out.status.success() {
            anyhow::bail!("ip -6 route failed with status {}", out.status);
        }
        Ok::<_, anyhow::Error>(String::from_utf8_lossy(&out.stdout).to_string())
    })?;
    assert!(
        route1.contains("via fe80:"),
        "expected link-local default route after switch, got: {route1:?}"
    );
    assert!(
        route1.contains("dev eth1"),
        "expected default route via eth1 after switch, got: {route1:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn radriven_link_up_restores_scoped_ll_default_route() -> Result<()> {
    check_caps()?;

    let lab = Lab::with_opts(
        LabOpts::default()
            .ipv6_dad_mode(Ipv6DadMode::Disabled)
            .ipv6_provisioning_mode(Ipv6ProvisioningMode::RaDriven),
    )
    .await?;
    let r1 = lab
        .add_router("r1")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", r1.id(), None)
        .default_via("eth0")
        .build()
        .await?;

    let before = dev.run_sync(|| {
        let out = std::process::Command::new("ip")
            .args(["-6", "route", "show", "default"])
            .output()?;
        if !out.status.success() {
            anyhow::bail!("ip -6 route failed with status {}", out.status);
        }
        Ok::<_, anyhow::Error>(String::from_utf8_lossy(&out.stdout).to_string())
    })?;
    assert!(before.contains("via fe80:"), "expected LL default route");
    assert!(
        before.contains("dev eth0"),
        "expected default route via eth0"
    );

    dev.link_down("eth0").await?;
    let during = dev.run_sync(|| {
        let out = std::process::Command::new("ip")
            .args(["-6", "route", "show", "default"])
            .output()?;
        if !out.status.success() {
            anyhow::bail!("ip -6 route failed with status {}", out.status);
        }
        Ok::<_, anyhow::Error>(String::from_utf8_lossy(&out.stdout).to_string())
    })?;
    assert!(
        during.trim().is_empty(),
        "expected no default v6 route while link is down, got: {during:?}"
    );

    dev.link_up("eth0").await?;
    let after = dev.run_sync(|| {
        let out = std::process::Command::new("ip")
            .args(["-6", "route", "show", "default"])
            .output()?;
        if !out.status.success() {
            anyhow::bail!("ip -6 route failed with status {}", out.status);
        }
        Ok::<_, anyhow::Error>(String::from_utf8_lossy(&out.stdout).to_string())
    })?;
    assert!(after.contains("via fe80:"), "expected LL default route");
    assert!(
        after.contains("dev eth0"),
        "expected default route via eth0"
    );

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn radriven_ra_worker_respects_router_enable_flag() -> Result<()> {
    check_caps()?;

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let outdir = std::env::temp_dir().join(format!("patchbay-ra-worker-{unique}"));
    fs::create_dir_all(&outdir)?;
    std::env::set_var("PATCHBAY_LOG", "trace");

    let lab_enabled = Lab::with_opts(
        LabOpts::default()
            .outdir(&outdir)
            .label("ra-enabled")
            .ipv6_dad_mode(Ipv6DadMode::Disabled)
            .ipv6_provisioning_mode(Ipv6ProvisioningMode::RaDriven),
    )
    .await?;
    let r_enabled = lab_enabled
        .add_router("r-enabled")
        .ip_support(IpSupport::DualStack)
        .ra_enabled(true)
        .ra_interval_secs(1)
        .build()
        .await?;
    let _dev_enabled = lab_enabled
        .add_device("d-enabled")
        .uplink(r_enabled.id())
        .build()
        .await?;
    let enabled_trace = r_enabled
        .filepath("tracing.jsonl")
        .context("missing enabled router tracing path")?;
    let has_tick =
        wait_for_file_contains(&enabled_trace, "ra-worker: tick", Duration::from_secs(3)).await?;
    assert!(has_tick, "expected RA worker tick in tracing log");
    drop(lab_enabled);

    let lab_disabled = Lab::with_opts(
        LabOpts::default()
            .outdir(&outdir)
            .label("ra-disabled")
            .ipv6_dad_mode(Ipv6DadMode::Disabled)
            .ipv6_provisioning_mode(Ipv6ProvisioningMode::RaDriven),
    )
    .await?;
    let r_disabled = lab_disabled
        .add_router("r-disabled")
        .ip_support(IpSupport::DualStack)
        .ra_enabled(false)
        .ra_interval_secs(1)
        .build()
        .await?;
    let _dev_disabled = lab_disabled
        .add_device("d-disabled")
        .uplink(r_disabled.id())
        .build()
        .await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let disabled_trace = r_disabled
        .filepath("tracing.jsonl")
        .context("missing disabled router tracing path")?;
    let disabled_content = fs::read_to_string(&disabled_trace).unwrap_or_default();
    assert!(
        !disabled_content.contains("ra-worker: tick"),
        "unexpected RA worker tick while RA is disabled"
    );

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn ra_source_is_link_local() -> Result<()> {
    check_caps()?;

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let outdir = std::env::temp_dir().join(format!("patchbay-ra-events-{unique}"));
    fs::create_dir_all(&outdir)?;
    std::env::set_var("PATCHBAY_LOG", "trace");

    let lab = Lab::with_opts(
        LabOpts::default()
            .outdir(&outdir)
            .label("ra-src-ll")
            .ipv6_dad_mode(Ipv6DadMode::Disabled)
            .ipv6_provisioning_mode(Ipv6ProvisioningMode::RaDriven),
    )
    .await?;
    let r = lab
        .add_router("r")
        .ip_support(IpSupport::DualStack)
        .ra_enabled(true)
        .ra_interval_secs(1)
        .build()
        .await?;
    let _dev = lab.add_device("d").uplink(r.id()).build().await?;

    let events = r
        .filepath("events.jsonl")
        .context("missing router events path")?;
    let has_ra_kind = wait_for_file_contains(
        &events,
        "\"kind\":\"RouterAdvertisement\"",
        Duration::from_secs(3),
    )
    .await?;
    assert!(
        has_ra_kind,
        "expected RouterAdvertisement event in events log"
    );
    let has_ll_src =
        wait_for_file_contains(&events, "\"src\":\"fe80:", Duration::from_secs(3)).await?;
    assert!(has_ll_src, "expected link-local RA source in events log");
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
