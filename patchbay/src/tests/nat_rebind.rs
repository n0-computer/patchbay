//! Tests for dynamic NAT mode changes and conntrack flush.
//!
//! Verifies that changing a router's NAT mode at runtime takes effect
//! immediately (port stability, reflexive IP) and that flushing the
//! conntrack table forces new port allocations.

use super::*;

/// Changing NAT mode between Home (EIM) and Corporate (EDM) flips port stability.
///
/// Home → Corporate: previously stable external port now varies per destination.
/// Corporate → Home: previously varying port now stays stable.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn mode_port_change() -> Result<()> {
    // Home→Corporate: port changes (EIM→EDM); Corporate→Home: port stabilises.
    let cases: &[(Nat, Nat, bool)] = &[
        (Nat::Home, Nat::Corporate, false),
        (Nat::Corporate, Nat::Home, true),
    ];
    let mut port_base = 16_800u16;
    let mut failures = Vec::new();
    for &(from, to, expect_stable) in cases {
        let result: Result<()> = async {
            let (lab, ctx) = build_nat_case(from, UplinkWiring::DirectIx, port_base).await?;
            let nat_handle = lab.router_by_name("nat").context("missing nat")?;
            nat_handle.set_nat_mode(to).await?;
            tokio::time::sleep(Duration::from_millis(50)).await;
            let dev = lab.device_by_name("dev").unwrap();
            let o1 = dev.probe_udp_mapping(ctx.r_dc)?;
            let o2 = dev.probe_udp_mapping(ctx.r_ix)?;
            let port_stable = o1.port() == o2.port();
            if port_stable != expect_stable {
                bail!(
                    "{from}→{to}: expected stable={expect_stable} got stable={port_stable} \
                         (r_dc port={}, r_ix port={})",
                    o1.port(),
                    o2.port()
                );
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            failures.push(format!("{from}→{to}: {e:#}"));
        }
        port_base += 10;
    }
    if !failures.is_empty() {
        bail!("{} failures:\n{}", failures.len(), failures.join("\n"));
    }
    Ok(())
}

/// Switching from NAT=None to NAT=Home changes the observed reflexive IP
/// from the device's private address to the router's WAN IP.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn mode_ip_change() -> Result<()> {
    // Home→None is omitted: with NAT=None, the device's private IP appears
    // as the packet source; the DC has no return route, so the UDP probe
    // times out rather than completing.
    let cases: &[(Nat, Nat)] = &[(Nat::None, Nat::Home)];
    let mut port_base = 16_900u16;
    let mut failures = Vec::new();
    for &(from, to) in cases {
        let result: Result<()> = async {
            let (lab, ctx) = build_nat_case(from, UplinkWiring::DirectIx, port_base).await?;
            let nat_handle = lab.router_by_name("nat").context("missing nat")?;
            let wan_ip = nat_handle.uplink_ip().context("no uplink ip")?;
            nat_handle.set_nat_mode(to).await?;
            tokio::time::sleep(Duration::from_millis(50)).await;
            let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
            let r_dc = ctx.r_dc;
            let o = ctx.dev.run_sync(move || {
                test_utils::probe_udp(r_dc, Duration::from_millis(500), Some(bind))
            })?;
            let expected = match to {
                Nat::Home => IpAddr::V4(wan_ip),
                Nat::None => IpAddr::V4(ctx.dev_ip),
                _ => unreachable!(),
            };
            if o.ip() != expected {
                bail!("{from}→{to}: expected {expected} got {}", o.ip());
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            failures.push(format!("{from}→{to}: {e:#}"));
        }
        port_base += 10;
    }
    if !failures.is_empty() {
        bail!("{} failures:\n{}", failures.len(), failures.join("\n"));
    }
    Ok(())
}

/// Flushing the NAT conntrack table causes a Corporate NAT to allocate a
/// new external port for the next probe (old mapping is gone).
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn conntrack_flush() -> Result<()> {
    // Skip if conntrack-tools is not installed.
    if std::process::Command::new("conntrack")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping nat_rebind_conntrack_flush: conntrack not found");
        return Ok(());
    }
    let (lab, ctx) = build_nat_case(Nat::Corporate, UplinkWiring::DirectIx, 17_000).await?;
    let nat_handle = lab.router_by_name("nat").context("missing nat")?;
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
    let r_dc = ctx.r_dc;
    let o1 = ctx
        .dev
        .run_sync(move || test_utils::probe_udp(r_dc, Duration::from_millis(500), Some(bind)))?;
    nat_handle.flush_nat_state().await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let o2 = ctx
        .dev
        .run_sync(move || test_utils::probe_udp(r_dc, Duration::from_millis(500), Some(bind)))?;
    assert_ne!(
        o1.port(),
        o2.port(),
        "expected new external port after conntrack flush"
    );
    Ok(())
}
