//! Tests for hairpin (loopback) NAT forwarding.
//!
//! Hairpinning lets a LAN device reach a peer on the same router via
//! the router's public IP+port, rather than requiring a direct LAN
//! connection.  FullCone enables it; Home NAT disables it.

use super::*;

/// Two devices behind a FullCone router (hairpin=true). Device A creates a
/// mapping via a DC reflector, then device B sends to A's external addr:port.
/// With hairpin, the router DNAT's the packet to A and masquerades the return.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn fullcone_allows() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let r = lab.add_router("r").nat(Nat::FullCone).build().await?;
    let a = lab
        .add_device("a")
        .iface("eth0", r.id(), None)
        .build()
        .await?;
    let b = lab
        .add_device("b")
        .iface("eth0", r.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("dc has no ip")?;
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 9100);
    let _r = dc.spawn_reflector(reflector).await?;

    // A sends outbound to create a fullcone mapping.
    let a_ext = a.probe_udp_mapping(reflector)?;

    // A listens for UDP on its private port (same port the mapping was created from).
    let a_local_port = a_ext.port();
    let a_ip = a.ip().unwrap();
    let a_listen = SocketAddr::new(IpAddr::V4(a_ip), a_local_port);
    let _r = a.spawn_reflector(a_listen).await?;

    // B sends to A's external address (router's public IP + A's mapped port).
    // With hairpin, the router should DNAT this to A's private IP.
    let reply = b.run_sync(move || {
        let sock =
            std::net::UdpSocket::bind("0.0.0.0:0").context("hairpin fullcone_allows udp bind")?;
        sock.set_read_timeout(Some(Duration::from_secs(2)))?;
        sock.send_to(b"PROBE", a_ext)?;
        let mut buf = [0u8; 512];
        let (n, _) = sock.recv_from(&mut buf)?;
        Ok(String::from_utf8_lossy(&buf[..n]).to_string())
    })?;
    assert!(
        reply.starts_with("OBSERVED "),
        "expected reflector reply, got: {reply:?}"
    );
    Ok(())
}

/// Two devices behind a Home router (hairpin=false). Same setup as above
/// but the LAN→WAN-IP traffic should NOT be forwarded.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn home_nat_blocks() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let r = lab.add_router("r").nat(Nat::Home).build().await?;
    let a = lab
        .add_device("a")
        .iface("eth0", r.id(), None)
        .build()
        .await?;
    let b = lab
        .add_device("b")
        .iface("eth0", r.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("dc has no ip")?;
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 9101);
    let _r = dc.spawn_reflector(reflector).await?;

    // A creates a mapping.
    let a_ext = a.probe_udp_mapping(reflector)?;

    // B tries to reach A via the external addr — should time out.
    let result = b.run_sync(move || {
        let sock =
            std::net::UdpSocket::bind("0.0.0.0:0").context("hairpin home_nat_blocks udp bind")?;
        sock.set_read_timeout(Some(Duration::from_millis(500)))?;
        sock.send_to(b"PROBE", a_ext)?;
        let mut buf = [0u8; 512];
        match sock.recv_from(&mut buf) {
            Ok(_) => bail!("unexpected reply — hairpin should be blocked"),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    });
    assert!(result.is_ok(), "expected timeout, got: {result:?}");
    Ok(())
}

/// Custom NAT config with EIM + APDF + hairpin enabled allows loopback traffic.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn custom_allows() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let r = lab
        .add_router("r")
        .nat(Nat::Custom(
            NatConfig::builder()
                .mapping(NatMapping::EndpointIndependent)
                .filtering(NatFiltering::AddressAndPortDependent)
                .hairpin(true)
                .build(),
        ))
        .build()
        .await?;
    let a = lab
        .add_device("a")
        .iface("eth0", r.id(), None)
        .build()
        .await?;
    let b = lab
        .add_device("b")
        .iface("eth0", r.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("dc has no ip")?;
    let reflector = SocketAddr::new(IpAddr::V4(dc_ip), 9102);
    let _r = dc.spawn_reflector(reflector).await?;

    let a_ext = a.probe_udp_mapping(reflector)?;

    let a_local_port = a_ext.port();
    let a_ip = a.ip().unwrap();
    let a_listen = SocketAddr::new(IpAddr::V4(a_ip), a_local_port);
    let _r = a.spawn_reflector(a_listen).await?;

    let reply = b.run_sync(move || {
        let sock =
            std::net::UdpSocket::bind("0.0.0.0:0").context("hairpin custom_allows udp bind")?;
        sock.set_read_timeout(Some(Duration::from_secs(2)))?;
        sock.send_to(b"PROBE", a_ext)?;
        let mut buf = [0u8; 512];
        let (n, _) = sock.recv_from(&mut buf)?;
        Ok(String::from_utf8_lossy(&buf[..n]).to_string())
    })?;
    assert!(
        reply.starts_with("OBSERVED "),
        "expected reflector reply, got: {reply:?}"
    );
    Ok(())
}
