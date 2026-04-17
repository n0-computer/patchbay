//! Tests for RouterPreset and related builder features.

use super::*;

/// RouterPreset::Home builds a dual-stack router with NAT and BlockInbound firewall.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn preset_home() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab
        .add_router("dc")
        .preset(RouterPreset::Public)
        .build()
        .await?;
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    let home = lab
        .add_router("home")
        .preset(RouterPreset::Home)
        .build()
        .await?;

    let dev = lab
        .add_device("dev")
        .iface("eth0", home.id())
        .build()
        .await?;

    // Home preset should give dual-stack.
    assert_eq!(home.ip_support(), Some(IpSupport::DualStack));
    assert!(dev.ip().is_some(), "device should have v4");
    assert!(dev.ip6().is_some(), "device should have v6");

    // Home has NAT — device IP should be private.
    let dev_ip = dev.ip().unwrap();
    assert!(
        dev_ip.is_private(),
        "home device should have private v4, got {dev_ip}"
    );

    // Outbound UDP to DC should work through NAT.
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 9220);
    let _r = dc.spawn_reflector(reflector).await?;

    let rtt = dev.run_sync(move || test_utils::udp_rtt_sync(reflector))?;
    assert!(rtt < Duration::from_millis(500), "outbound should work");

    Ok(())
}

/// RouterPreset::Public builds a dual-stack router with no NAT and no firewall.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn preset_public() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab
        .add_router("dc")
        .preset(RouterPreset::Public)
        .build()
        .await?;

    let dev = lab.add_device("srv").iface("eth0", dc.id()).build().await?;

    assert_eq!(dc.ip_support(), Some(IpSupport::DualStack));
    assert!(dev.ip().is_some(), "device should have v4");
    assert!(dev.ip6().is_some(), "device should have v6");

    // No NAT → public IP.
    let dev_ip = dev.ip().unwrap();
    assert!(
        !dev_ip.is_private(),
        "public device should have public v4, got {dev_ip}"
    );

    Ok(())
}

/// RouterPreset::Corporate blocks non-web UDP (same as Firewall::Corporate).
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn preset_corporate_blocks_udp() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab
        .add_router("dc")
        .preset(RouterPreset::Public)
        .build()
        .await?;
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    let corp = lab
        .add_router("corp")
        .preset(RouterPreset::Corporate)
        .build()
        .await?;

    let dev = lab
        .add_device("ws")
        .iface("eth0", corp.id())
        .build()
        .await?;

    // Corporate firewall should block arbitrary UDP.
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 9223);
    let _r = dc.spawn_reflector(reflector).await?;

    let blocked = dev.run_sync(move || test_utils::udp_rtt_sync(reflector));
    assert!(
        blocked.is_err(),
        "corporate preset should block UDP, got: {:?}",
        blocked
    );

    // TCP 443 should work.
    let tcp_bind = SocketAddr::new(IpAddr::V4(dc_ip), 443);
    dc.spawn(async move |_| spawn_tcp_echo_server(tcp_bind).await)?
        .await??;
    dev.spawn(async move |_| tcp_roundtrip(tcp_bind).await)?
        .await??;

    Ok(())
}

/// Preset with override: Home preset with Nat::Easiest overrides only NAT.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn preset_override() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab
        .add_router("dc")
        .preset(RouterPreset::Public)
        .build()
        .await?;
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    // Home preset with full-cone NAT override.
    let home = lab
        .add_router("home")
        .preset(RouterPreset::Home)
        .nat(Nat::Easiest)
        .build()
        .await?;

    let dev = lab
        .add_device("dev")
        .iface("eth0", home.id())
        .build()
        .await?;

    // Should still be dual-stack (from preset).
    assert_eq!(home.ip_support(), Some(IpSupport::DualStack));

    // Outbound should work (FullCone + BlockInbound).
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 9225);
    let _r = dc.spawn_reflector(reflector).await?;

    let rtt = dev.run_sync(move || test_utils::udp_rtt_sync(reflector))?;
    assert!(rtt < Duration::from_millis(500));

    Ok(())
}

/// Snapshot of each preset's NAT config. Changes here are intentional and
/// should be matched by a corresponding update in the docs
/// (`docs/guide/nat-and-firewalls.md` preset table) and in
/// `plans/nat-breaking-changes.md`.
#[test]
fn preset_nat_snapshots() {
    use crate::{Nat, NatMapping, PortPreservation};

    // (preset, expected_nat_kind_or_none, udp_stream_seconds_or_none)
    let cases = [
        (RouterPreset::Home, Some(Nat::Easy), Some(300u32)),
        (RouterPreset::IspCgnat, Some(Nat::Easy), Some(300)),
        (RouterPreset::IspCgnatSymmetric, Some(Nat::Hard), Some(180)),
        (RouterPreset::MobileCarrier, Some(Nat::Hard), Some(60)),
        (RouterPreset::Corporate, Some(Nat::Hardest), Some(120)),
        (RouterPreset::Hotel, Some(Nat::Hardest), Some(120)),
        (RouterPreset::Cloud, Some(Nat::Hardest), Some(350)),
        (RouterPreset::Public, None, None),
        (RouterPreset::PublicV4, None, None),
        (RouterPreset::IspV6, None, None),
    ];
    for (preset, expected_kind, expected_udp_stream) in cases {
        let cfg = preset.nat_config();
        match (cfg, expected_kind) {
            (None, None) => {}
            (Some(actual), Some(kind)) => {
                let expected_cfg = kind.to_config().expect("kind is not Nat::None");
                assert_eq!(
                    actual.mapping, expected_cfg.mapping,
                    "{preset:?}: mapping mismatch"
                );
                assert_eq!(
                    actual.filtering, expected_cfg.filtering,
                    "{preset:?}: filtering mismatch"
                );
                let udp_stream = expected_udp_stream.expect("set when kind is Some");
                assert_eq!(
                    actual.timeouts.udp_stream, udp_stream,
                    "{preset:?}: udp_stream mismatch"
                );
            }
            (actual, expected) => {
                panic!(
                    "{preset:?}: config mismatch: actual={actual:?}, expected_kind={expected:?}"
                );
            }
        }
        // Cross-check that EDM presets carry the correct port-preservation
        // tag: Hard → Preserve, Hardest → Random.
        if let Some(actual) = cfg {
            if let NatMapping::EndpointDependent(pp) = actual.mapping {
                let expected_pp = match expected_kind {
                    Some(Nat::Hard) => PortPreservation::Preserve,
                    Some(Nat::Hardest) => PortPreservation::Random,
                    _ => unreachable!("EDM presets map to Hard or Hardest only"),
                };
                assert_eq!(pp, expected_pp, "{preset:?}: port preservation mismatch");
            }
        }
    }
}

/// All presets recommend Ipv6Profile::Realistic.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn preset_recommended_ipv6_profiles() -> Result<()> {
    for preset in [
        RouterPreset::Home,
        RouterPreset::Public,
        RouterPreset::PublicV4,
        RouterPreset::IspCgnat,
        RouterPreset::IspCgnatSymmetric,
        RouterPreset::MobileCarrier,
        RouterPreset::IspV6,
        RouterPreset::Corporate,
        RouterPreset::Hotel,
        RouterPreset::Cloud,
    ] {
        assert_eq!(
            preset.recommended_ipv6_profile(),
            Ipv6Profile::Realistic,
            "{preset:?} should recommend Realistic"
        );
    }

    Ok(())
}

/// Public GUA v6 pool gives addresses from 2001:db8:1::/48, not ULA fd10::/48.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn public_v6_pool_is_gua() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab
        .add_router("dc")
        .preset(RouterPreset::Public)
        .build()
        .await?;

    let dev = lab.add_device("srv").iface("eth0", dc.id()).build().await?;

    let v6 = dev.ip6().context("no v6 address")?;
    let segs = v6.segments();
    // Public GUA pool is 2001:db8:1::/48.
    assert_eq!(segs[0], 0x2001, "v6 should be from GUA pool, got {v6}");
    assert_eq!(segs[1], 0x0db8, "v6 should be from GUA pool, got {v6}");
    assert_eq!(
        segs[2], 0x0001,
        "v6 third segment should be 1 (public pool), got {v6}"
    );

    // ULA check: the address should NOT start with fd.
    assert_ne!(
        segs[0] >> 8,
        0xfd,
        "public pool should not give ULA address, got {v6}"
    );

    Ok(())
}

/// Private v6 pool (Home preset default) gives ULA fd10::/48 addresses.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn private_v6_pool_is_ula() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let home = lab
        .add_router("home")
        .preset(RouterPreset::Home)
        .build()
        .await?;

    let dev = lab
        .add_device("laptop")
        .iface("eth0", home.id())
        .build()
        .await?;

    let v6 = dev.ip6().context("no v6 address")?;
    let segs = v6.segments();
    assert_eq!(
        segs[0], 0xfd10,
        "home device v6 should be ULA fd10::, got {v6}"
    );

    Ok(())
}

/// RouterPreset::Hotel builds v4-only (no v6).
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn preset_hotel_v4_only() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let hotel = lab
        .add_router("hotel")
        .preset(RouterPreset::Hotel)
        .build()
        .await?;

    let dev = lab
        .add_device("guest")
        .iface("eth0", hotel.id())
        .build()
        .await?;

    assert_eq!(hotel.ip_support(), Some(IpSupport::V4Only));
    assert!(dev.ip().is_some(), "hotel device should have v4");
    assert!(dev.ip6().is_none(), "hotel device should have no v6");

    Ok(())
}

/// RouterPreset::PublicV4 builds v4-only with no NAT and public IPs.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn preset_public_v4() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let isp = lab
        .add_router("isp")
        .preset(RouterPreset::PublicV4)
        .build()
        .await?;

    let dev = lab
        .add_device("srv")
        .iface("eth0", isp.id())
        .build()
        .await?;

    assert_eq!(isp.ip_support(), Some(IpSupport::V4Only));
    let dev_ip = dev.ip().unwrap();
    assert!(
        !dev_ip.is_private(),
        "PublicV4 device should have public IP, got {dev_ip}"
    );
    assert!(dev.ip6().is_none(), "PublicV4 should have no v6");

    Ok(())
}

/// RouterPreset::IspCgnat builds dual-stack with CGNAT and private downstream.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn preset_isp_cgnat() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab
        .add_router("dc")
        .preset(RouterPreset::Public)
        .build()
        .await?;
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    let isp = lab
        .add_router("isp")
        .preset(RouterPreset::IspCgnat)
        .build()
        .await?;

    let dev = lab
        .add_device("sub")
        .iface("eth0", isp.id())
        .build()
        .await?;

    assert_eq!(isp.ip_support(), Some(IpSupport::DualStack));
    let dev_ip = dev.ip().unwrap();
    assert!(
        dev_ip.is_private(),
        "IspCgnat device should have private v4, got {dev_ip}"
    );

    // Outbound should work through CGNAT.
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 9226);
    let _r = dc.spawn_reflector(reflector).await?;

    let rtt = dev.run_sync(move || test_utils::udp_rtt_sync(reflector))?;
    assert!(rtt < Duration::from_millis(500));

    Ok(())
}

/// RouterPreset::IspV6 builds an IPv6-only carrier with NAT64.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn preset_isp_v6() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let dc = lab
        .add_router("dc")
        .preset(RouterPreset::Public)
        .build()
        .await?;
    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;

    let carrier = lab
        .add_router("carrier")
        .preset(RouterPreset::IspV6)
        .build()
        .await?;

    let phone = lab
        .add_device("phone")
        .iface("eth0", carrier.id())
        .build()
        .await?;

    // IspV6: IPv6-only with public GUA.
    assert_eq!(carrier.ip_support(), Some(IpSupport::V6Only));
    assert!(phone.ip6().is_some(), "should have v6");

    // Phone can reach v4 server via NAT64 prefix.
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 9350);
    let _r = dc.spawn_reflector(reflector).await?;

    let nat64_addr = crate::nat64::embed_v4_in_nat64(dc_ip);
    let nat64_target = SocketAddr::new(IpAddr::V6(nat64_addr), 9350);

    let rtt = phone.run_sync(move || test_utils::udp_rtt_sync(nat64_target))?;
    assert!(
        rtt < Duration::from_millis(500),
        "NAT64 should work via preset"
    );

    Ok(())
}
