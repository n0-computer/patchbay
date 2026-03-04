//! NAT mapping types, reflexive IPs, CGNAT, port behavior, multi-device isolation.

use super::*;

/// Device behind CGNAT sees the ISP's public IX IP as its reflexive address.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn cgnat_reflexive_ip() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let isp = lab.add_router("isp1").nat(Nat::Cgnat).build().await?;
    let dc = lab.add_router("dc1").build().await?;
    let home = lab
        .add_router("home1")
        .upstream(isp.id())
        .nat(Nat::Home)
        .build()
        .await?;
    lab.add_device("dev1")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 5478);
    let _r = dc.spawn_reflector(r).await?;

    let dev1 = lab.device_by_name("dev1").unwrap();
    let o = dev1.probe_udp_mapping(r)?;
    let isp_public = IpAddr::V4(isp.uplink_ip().context("no uplink ip")?);

    assert_eq!(
        o.ip(),
        isp_public,
        "with CGNAT the observed IP must be the ISP's IX IP",
    );
    Ok(())
}

/// Two devices behind two Home NATs under a CGNAT ISP both report
/// the ISP's public IP as their reflexive address.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn cgnat_shared_reflexive_ip() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let isp = lab.add_router("isp").nat(Nat::Cgnat).build().await?;
    let lan_provider = lab
        .add_router("lan-provider")
        .upstream(isp.id())
        .nat(Nat::Home)
        .build()
        .await?;
    let lan_fetcher = lab
        .add_router("lan-fetcher")
        .upstream(isp.id())
        .nat(Nat::Home)
        .build()
        .await?;
    lab.add_device("provider")
        .iface("eth0", lan_provider.id(), None)
        .build()
        .await?;
    lab.add_device("fetcher")
        .iface("eth0", lan_fetcher.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 6478);
    let _r = dc.spawn_reflector(reflector).await?;

    let provider = lab.device_by_name("provider").unwrap();
    let fetcher = lab.device_by_name("fetcher").unwrap();
    let provider_obs = provider.probe_udp_mapping(reflector)?;
    let fetcher_obs = fetcher.probe_udp_mapping(reflector)?;
    let isp_public = isp.uplink_ip().context("no uplink ip")?;

    let provider_ip = match provider_obs.ip() {
        IpAddr::V4(ip) => ip,
        IpAddr::V6(ip) => bail!("expected provider observed IPv4 address, got {ip}"),
    };
    let fetcher_ip = match fetcher_obs.ip() {
        IpAddr::V4(ip) => ip,
        IpAddr::V6(ip) => bail!("expected fetcher observed IPv4 address, got {ip}"),
    };

    assert_eq!(
        provider_ip.octets()[0],
        198,
        "provider STUN report should be public 198.18.* mapped IP, got {}",
        provider_obs
    );
    assert_eq!(
        fetcher_ip.octets()[0],
        198,
        "fetcher STUN report should be public 198.18.* mapped IP, got {}",
        fetcher_obs
    );
    assert_eq!(
        provider_ip, isp_public,
        "provider should be mapped behind ISP public address"
    );
    assert_eq!(
        fetcher_ip, isp_public,
        "fetcher should be mapped behind ISP public address"
    );
    assert_ne!(
        provider_obs.port(),
        0,
        "provider mapped port should be non-zero"
    );
    assert_ne!(
        fetcher_obs.port(),
        0,
        "fetcher mapped port should be non-zero"
    );
    Ok(())
}

/// Sweeps NAT modes × uplink wirings, verifying ping/UDP connectivity
/// and that the reflexive IP matches expectations.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn matrix_connectivity_and_reflexive_ip() -> Result<()> {
    check_caps()?;
    let cases = [
        (Nat::None, UplinkWiring::DirectIx),
        (Nat::Cgnat, UplinkWiring::DirectIx),
        (Nat::Home, UplinkWiring::DirectIx),
        (Nat::Home, UplinkWiring::ViaPublicIsp),
        (Nat::Home, UplinkWiring::ViaCgnatIsp),
        (Nat::Corporate, UplinkWiring::DirectIx),
        (Nat::Corporate, UplinkWiring::ViaPublicIsp),
        (Nat::Corporate, UplinkWiring::ViaCgnatIsp),
    ];

    let mut case_idx = 0u16;
    for (mode, wiring) in cases {
        let port_base = 10000 + case_idx * 10;
        case_idx = case_idx.saturating_add(1);
        let (lab, _dev_ns, r_dc, _r_ix, expected_ip, _guards) =
            build_single_nat_case(mode, wiring, port_base).await?;
        let dev = lab.device_by_name("dev").unwrap();
        let r_dc_ip_str = r_dc.ip().to_string();
        dev.run_sync(move || ping(&r_dc_ip_str))?;
        let _ = dev.run_sync(move || test_utils::udp_roundtrip(r_dc))?;
        let observed = dev.probe_udp_mapping(r_dc)?;
        assert_eq!(
            observed.ip(),
            IpAddr::V4(expected_ip),
            "unexpected reflexive IP for mode={mode:?} wiring={}",
            wiring.label()
        );
    }
    Ok(())
}

/// Devices in different NATs cannot ping each other's private IPs but
/// can reach the relay and see their NAT's WAN address.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn private_isolation_public_reachable() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let nat_a = lab.add_router("nat-a").nat(Nat::Home).build().await?;
    let nat_b = lab.add_router("nat-b").nat(Nat::Home).build().await?;

    let relay = lab
        .add_device("relay")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;
    let a1 = lab
        .add_device("a1")
        .iface("eth0", nat_a.id(), None)
        .build()
        .await?;
    let a2 = lab
        .add_device("a2")
        .iface("eth0", nat_a.id(), None)
        .build()
        .await?;
    let b1 = lab
        .add_device("b1")
        .iface("eth0", nat_b.id(), None)
        .build()
        .await?;

    let a2_ip = a2.ip().unwrap();
    let b1_ip = b1.ip().unwrap();
    let a1_ip = a1.ip().unwrap();
    let relay_ip = relay.ip().unwrap();

    let a2_ip_str = a2_ip.to_string();
    let b1_ip_str = b1_ip.to_string();
    let a1_ip_str = a1_ip.to_string();
    let relay_ip_str = relay_ip.to_string();
    let relay_ip_str2 = relay_ip_str.clone();

    a1.run_sync(move || ping(&a2_ip_str))?;
    a1.run_sync(move || ping_fails(&b1_ip_str))?;
    b1.run_sync(move || ping_fails(&a1_ip_str))?;

    a1.run_sync(move || ping(&relay_ip_str))?;
    b1.run_sync(move || ping(&relay_ip_str2))?;

    let nat_a_public = nat_a.uplink_ip().context("no uplink ip")?;
    let nat_b_public = nat_b.uplink_ip().context("no uplink ip")?;
    let nat_b_str = nat_b_public.to_string();
    let nat_a_str = nat_a_public.to_string();
    a1.run_sync(move || ping(&nat_b_str))?;
    b1.run_sync(move || ping(&nat_a_str))?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 12000);
    let _r = dc.spawn_reflector(reflector).await?;

    let a1_map = a1.probe_udp_mapping(reflector)?;
    let a2_map = a2.probe_udp_mapping(reflector)?;
    let b1_map = b1.probe_udp_mapping(reflector)?;
    assert_eq!(a1_map.ip(), IpAddr::V4(nat_a_public));
    assert_eq!(a2_map.ip(), IpAddr::V4(nat_a_public));
    assert_eq!(b1_map.ip(), IpAddr::V4(nat_b_public));
    Ok(())
}

/// Home NAT behind public ISP gets a public WAN IP;
/// Home NAT behind CGNAT ISP gets a private 10.x WAN IP.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn wan_pool_selection() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let isp_public = lab.add_router("isp-public").build().await?;
    let isp_cgnat = lab.add_router("isp-cgnat").nat(Nat::Cgnat).build().await?;
    let home_public = lab
        .add_router("home-public")
        .upstream(isp_public.id())
        .nat(Nat::Home)
        .build()
        .await?;
    let home_cgnat = lab
        .add_router("home-cgnat")
        .upstream(isp_cgnat.id())
        .nat(Nat::Home)
        .build()
        .await?;

    let wan_public = home_public.uplink_ip().context("no uplink ip")?;
    let wan_cgnat = home_cgnat.uplink_ip().context("no uplink ip")?;

    let is_private_10 = |ip: Ipv4Addr| ip.octets()[0] == 10;
    assert!(
        !is_private_10(wan_public),
        "expected public WAN for non-CGNAT home, got {wan_public}"
    );
    assert!(
        is_private_10(wan_cgnat),
        "expected private WAN for CGNAT home, got {wan_cgnat}"
    );
    Ok(())
}

/// Full sweep of NAT mode × uplink wiring × protocol × bind mode.
/// Asserts each reflexive IP matches the expected WAN address.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn reflexive_ip_all_combos() -> Result<()> {
    use futures::StreamExt as _;
    use futures_buffered::BufferedStreamExt;
    use strum::IntoEnumIterator;

    // Nat::None + Via*Isp is skipped: no return route from DC → device subnet.
    let combos: Vec<_> = Nat::iter()
        .flat_map(|m| UplinkWiring::iter().map(move |w| (m, w)))
        .filter(|(m, w)| {
            !(*m == Nat::None
                && matches!(w, UplinkWiring::ViaPublicIsp | UplinkWiring::ViaCgnatIsp))
        })
        .flat_map(|(m, w)| Proto::iter().map(move |p| (m, w, p)))
        .flat_map(|(m, w, p)| BindMode::iter().map(move |b| (m, w, p, b)))
        .collect();

    let failures: Vec<String> = futures::stream::iter(combos.into_iter().enumerate().map(
        |(i, (mode, wiring, proto, bind))| {
            let port_base = 14_000u16 + (i as u16) * 10;
            async move {
                let result: Result<()> = async {
                    let (_lab, ctx) = build_nat_case(mode, wiring, port_base).await?;
                    let obs = probe_reflexive(&ctx.dev, proto, bind, &ctx).await?;
                    if obs.ip() != IpAddr::V4(ctx.expected_ip) {
                        bail!("expected {} got {}", ctx.expected_ip, obs.ip());
                    }
                    Ok(())
                }
                .await;
                match result {
                    Ok(()) => None,
                    Err(e) => {
                        let label = format!("{mode}/{wiring}/{proto}/{bind}");
                        eprintln!("FAIL {label}: {e:#}");
                        Some(format!("{label}: {e:#}"))
                    }
                }
            }
        },
    ))
    .buffered_unordered(8)
    .filter_map(|x| async { x })
    .collect()
    .await;

    if !failures.is_empty() {
        bail!("{} combos failed:\n{}", failures.len(), failures.join("\n"));
    }
    Ok(())
}

/// EIM (Home NAT): external port is identical for two different reflectors.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn port_mapping_eim_stable() -> Result<()> {
    use strum::IntoEnumIterator;
    let mut port_base = 16_000u16;
    let mut failures = Vec::new();
    for wiring in UplinkWiring::iter() {
        let result: Result<()> = async {
            let (lab, ctx) = build_nat_case(Nat::Home, wiring, port_base).await?;
            let dev = lab.device_by_name("dev").unwrap();
            let o1 = dev.probe_udp_mapping(ctx.r_dc)?;
            let o2 = dev.probe_udp_mapping(ctx.r_ix)?;
            if o1.port() != o2.port() {
                bail!(
                    "EIM: external port changed: r_dc={} r_ix={}",
                    o1.port(),
                    o2.port()
                );
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            failures.push(format!("DestIndep/{wiring}: {e:#}"));
        }
        port_base += 10;
    }
    if !failures.is_empty() {
        bail!("{} combos failed:\n{}", failures.len(), failures.join("\n"));
    }
    Ok(())
}

/// EDM (Corporate NAT): external port differs between two reflectors.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn port_mapping_edm_changes() -> Result<()> {
    use strum::IntoEnumIterator;
    let mut port_base = 16_100u16;
    let mut failures = Vec::new();
    for wiring in UplinkWiring::iter() {
        let result: Result<()> = async {
            let (lab, ctx) = build_nat_case(Nat::Corporate, wiring, port_base).await?;
            let dev = lab.device_by_name("dev").unwrap();
            let o1 = dev.probe_udp_mapping(ctx.r_dc)?;
            let o2 = dev.probe_udp_mapping(ctx.r_ix)?;
            if o1.port() == o2.port() {
                bail!(
                    "EDM: external port must change: r_dc={} r_ix={}",
                    o1.port(),
                    o2.port()
                );
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            failures.push(format!("DestDep/{wiring}: {e:#}"));
        }
        port_base += 10;
    }
    if !failures.is_empty() {
        bail!("{} combos failed:\n{}", failures.len(), failures.join("\n"));
    }
    Ok(())
}

/// Two devices behind the same Home NAT both see the same external IP.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn same_nat_shared_ip() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let nat = lab.add_router("nat").nat(Nat::Home).build().await?;
    let dev_a = lab
        .add_device("dev-a")
        .iface("eth0", nat.id(), None)
        .build()
        .await?;
    let dev_b = lab
        .add_device("dev-b")
        .iface("eth0", nat.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 17_100);
    let _r = dc.spawn_reflector(r).await?;

    let oa = dev_a.run_sync(move || test_utils::udp_roundtrip(r))?;
    let ob = dev_b.run_sync(move || test_utils::udp_roundtrip(r))?;
    assert_eq!(
        oa.ip(),
        ob.ip(),
        "devices behind the same NAT must share the same external IP"
    );
    Ok(())
}

/// Devices behind different Home NATs cannot ping each other and have
/// different external IPs.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn different_nat_isolation() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let nat_a = lab.add_router("nat-a").nat(Nat::Home).build().await?;
    let nat_b = lab.add_router("nat-b").nat(Nat::Home).build().await?;
    let dev_a = lab
        .add_device("dev-a")
        .iface("eth0", nat_a.id(), None)
        .build()
        .await?;
    let dev_b = lab
        .add_device("dev-b")
        .iface("eth0", nat_b.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let r = SocketAddr::new(IpAddr::V4(dc_ip), 17_200);
    let _r = dc.spawn_reflector(r).await?;

    let ip_a = dev_a.ip().unwrap();
    let ip_b = dev_b.ip().unwrap();
    let ip_a_str = ip_a.to_string();
    let ip_b_str = ip_b.to_string();
    let dc_ip_str = dc_ip.to_string();
    let dc_ip_str2 = dc_ip_str.clone();

    dev_a.run_sync(move || ping_fails(&ip_b_str))?;
    dev_b.run_sync(move || ping_fails(&ip_a_str))?;
    dev_a.run_sync(move || ping(&dc_ip_str))?;
    dev_b.run_sync(move || ping(&dc_ip_str2))?;

    let oa = dev_a.run_sync(move || test_utils::udp_roundtrip(r))?;
    let ob = dev_b.run_sync(move || test_utils::udp_roundtrip(r))?;
    assert_ne!(
        oa.ip(),
        ob.ip(),
        "devices behind different NATs must have different external IPs"
    );
    Ok(())
}

/// NatV6Mode::Masquerade: device's v6 reflexive address is the router's WAN v6.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn v6_masquerade() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab
        .add_router("dc")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;
    let isp = lab
        .add_router("isp")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;
    let home = lab
        .add_router("nat")
        .upstream(isp.id())
        .nat(Nat::Home)
        .ip_support(IpSupport::DualStack)
        .nat_v6(NatV6Mode::Masquerade)
        .build()
        .await?;
    let dev = lab
        .add_device("dev1")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    let dc_ip_v6 = dc.uplink_ip_v6().expect("dc v6 uplink");
    let r_v6 = SocketAddr::new(IpAddr::V6(dc_ip_v6), 3500);
    let _r = dc.spawn_reflector(r_v6).await?;

    let o = dev.run_sync(move || test_utils::udp_roundtrip(r_v6))?;
    let home_wan_v6 = home.uplink_ip_v6().expect("home v6 uplink");
    assert_eq!(
        o.ip(),
        IpAddr::V6(home_wan_v6),
        "v6 masquerade: reflexive should be router WAN IP"
    );
    Ok(())
}

/// NatV6Mode::None: device's own ULA v6 address appears as reflexive.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn v6_no_translation() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab
        .add_router("dc")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;
    let isp = lab
        .add_router("isp")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;
    let home = lab
        .add_router("home")
        .upstream(isp.id())
        .nat(Nat::Home)
        .ip_support(IpSupport::DualStack)
        .nat_v6(NatV6Mode::None)
        .build()
        .await?;
    let dev = lab
        .add_device("dev1")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    let dc_ip_v6 = dc.uplink_ip_v6().expect("dc v6 uplink");
    let r_v6 = SocketAddr::new(IpAddr::V6(dc_ip_v6), 3494);
    let _r = dc.spawn_reflector(r_v6).await?;

    let o_v6 = dev.run_sync(move || test_utils::udp_roundtrip(r_v6))?;
    let dev_ip6 = dev.ip6().expect("device v6 addr");
    assert_eq!(
        o_v6.ip(),
        IpAddr::V6(dev_ip6),
        "v6 reflexive should be device's own v6 address (no NAT)"
    );

    Ok(())
}

/// FullCone NAT allows unsolicited inbound: external host sends to mapped
/// address and device receives it without prior outbound to that sender.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn fullcone_external_reachable() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let fc = lab.add_router("fc").nat(Nat::FullCone).build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", fc.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("no dc uplink ip")?;
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 20_000);
    let _r = dc.spawn_reflector(reflector).await?;

    // Create a mapping via the reflector.
    let mapped = dev.probe_udp_mapping(reflector)?;

    // Now have a *different* host (dc itself, but from a different port) send to
    // the mapped address. Under FullCone, this should reach the device.
    let mapped_addr = mapped;
    let dev_listen_port = mapped.port();
    let dev_ip = dev.ip().unwrap();
    let listen = SocketAddr::new(IpAddr::V4(dev_ip), dev_listen_port);
    let _r = dev.spawn_reflector(listen).await?;

    let reply = dc.run_sync(move || {
        let sock = std::net::UdpSocket::bind("0.0.0.0:0").context("nat fullcone dc udp bind")?;
        sock.set_read_timeout(Some(Duration::from_secs(2)))?;
        sock.send_to(b"HELLO", mapped_addr)?;
        let mut buf = [0u8; 512];
        let (n, _) = sock.recv_from(&mut buf)?;
        Ok(String::from_utf8_lossy(&buf[..n]).to_string())
    })?;
    assert!(
        reply.starts_with("OBSERVED "),
        "expected reflector reply from FullCone NAT, got: {reply:?}"
    );
    Ok(())
}

/// V6 masquerade NAT: external v6 address differs from device's private v6.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn v6_masquerade_port_mapping() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab
        .add_router("dc")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;
    let nat = lab
        .add_router("nat")
        .ip_support(IpSupport::DualStack)
        .nat(Nat::Home)
        .nat_v6(NatV6Mode::Masquerade)
        .build()
        .await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", nat.id(), None)
        .build()
        .await?;

    let dc_v6 = dc.uplink_ip_v6().context("dc v6 uplink")?;
    let r_v6 = SocketAddr::new(IpAddr::V6(dc_v6), 20_100);
    let _r = dc.spawn_reflector(r_v6).await?;

    let o = dev.run_sync(move || test_utils::udp_roundtrip(r_v6))?;
    let nat_v6 = nat.uplink_ip_v6().context("nat v6 uplink")?;
    assert_eq!(
        o.ip(),
        IpAddr::V6(nat_v6),
        "v6 masquerade should show NAT's v6 uplink IP"
    );
    Ok(())
}
