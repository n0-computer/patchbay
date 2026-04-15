//! ECN (Explicit Congestion Notification) transparency tests.
//!
//! Verifies that ECN bits in the IP TOS field survive end-to-end through
//! patchbay's network stack (veth pairs, bridges, NAT).
//!
//! UDP tests use noq-udp to set ECN via sendmsg cmsg and read it back via
//! recvmsg cmsg on the receiving side, matching how QUIC implementations
//! handle ECN in production.

use std::{
    io::IoSliceMut,
    net::{IpAddr, SocketAddr, UdpSocket},
    os::fd::AsRawFd,
};

use noq_udp::{EcnCodepoint, RecvMeta, Transmit, UdpSocketState};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use super::*;

fn udp4_pair(bind: SocketAddr) -> Result<(Socket, UdpSocketState)> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.bind(&SockAddr::from(bind))?;
    sock.set_nonblocking(true)?;
    let state = UdpSocketState::new((&sock).into())?;
    Ok((sock, state))
}

fn send_ecn(
    sock: &Socket,
    state: &UdpSocketState,
    dst: SocketAddr,
    ecn: EcnCodepoint,
) -> Result<()> {
    state.send(
        sock.into(),
        &Transmit {
            destination: dst,
            ecn: Some(ecn),
            contents: b"ECN_PROBE",
            segment_size: None,
            src_ip: None,
        },
    )?;
    Ok(())
}

fn recv_ecn(sock: &Socket, state: &UdpSocketState) -> Result<Option<EcnCodepoint>> {
    let mut buf = [0u8; 128];
    let mut meta = RecvMeta::default();
    state.recv(
        sock.into(),
        &mut [IoSliceMut::new(&mut buf)],
        std::slice::from_mut(&mut meta),
    )?;
    Ok(meta.ecn)
}

/// ECN bits are preserved through a direct (no NAT) path, verified via
/// noq-udp sendmsg/recvmsg cmsg handling (the same path QUIC uses).
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn ecn_bits_preserved_direct() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let sender = lab.add_device("sender").uplink(dc.id()).build().await?;
    let receiver = lab.add_device("receiver").uplink(dc.id()).build().await?;

    let recv_ip = receiver.ip().unwrap();
    let recv_addr = SocketAddr::new(IpAddr::V4(recv_ip), 22_000);

    let (bound_tx, bound_rx) = oneshot::channel::<Result<()>>();

    let recv_task = receiver.spawn(move |_| async move {
        let (sock, state) = udp4_pair(recv_addr)?;
        let tokio_sock = tokio::net::UdpSocket::from_std(UdpSocket::from(sock.try_clone()?))?;
        let _ = bound_tx.send(Ok(()));

        tokio::time::timeout(Duration::from_secs(2), tokio_sock.readable()).await??;
        let ecn = recv_ecn(&sock, &state)?;
        assert_eq!(
            ecn,
            Some(EcnCodepoint::Ect0),
            "ECN should be ECT(0), got {ecn:?}"
        );
        anyhow::Ok(())
    })?;

    bound_rx.await??;

    let send_task = sender.spawn(move |_| async move {
        let (sock, state) = udp4_pair("0.0.0.0:0".parse()?)?;
        send_ecn(&sock, &state, recv_addr, EcnCodepoint::Ect0)?;
        anyhow::Ok(())
    })?;

    send_task.await??;
    recv_task.await??;
    Ok(())
}

/// ECN bits are preserved through NAT (masquerade).
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn ecn_bits_preserved_through_nat() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let nat = lab.add_router("nat").nat(Nat::Home).build().await?;
    let server = lab.add_device("server").uplink(dc.id()).build().await?;
    let client = lab.add_device("client").uplink(nat.id()).build().await?;

    let server_ip = server.ip().unwrap();
    let server_addr = SocketAddr::new(IpAddr::V4(server_ip), 22_001);

    let (bound_tx, bound_rx) = oneshot::channel::<Result<()>>();

    let server_task = server.spawn(move |_| async move {
        let (sock, state) = udp4_pair(server_addr)?;
        let tokio_sock = tokio::net::UdpSocket::from_std(UdpSocket::from(sock.try_clone()?))?;
        let _ = bound_tx.send(Ok(()));

        tokio::time::timeout(Duration::from_secs(2), tokio_sock.readable()).await??;
        let ecn = recv_ecn(&sock, &state)?;
        assert_eq!(
            ecn,
            Some(EcnCodepoint::Ect0),
            "ECN through NAT should be ECT(0), got {ecn:?}"
        );
        anyhow::Ok(())
    })?;

    bound_rx.await??;

    let client_task = client.spawn(move |_| async move {
        let (sock, state) = udp4_pair("0.0.0.0:0".parse()?)?;
        send_ecn(&sock, &state, server_addr, EcnCodepoint::Ect0)?;
        anyhow::Ok(())
    })?;

    client_task.await??;
    server_task.await??;
    Ok(())
}

/// All ECN codepoints (ECT(0), ECT(1), CE) survive a direct path.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn ecn_all_codepoints() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let sender = lab.add_device("sender").uplink(dc.id()).build().await?;
    let receiver = lab.add_device("receiver").uplink(dc.id()).build().await?;

    let recv_ip = receiver.ip().unwrap();
    let recv_addr = SocketAddr::new(IpAddr::V4(recv_ip), 22_003);

    let (bound_tx, bound_rx) = oneshot::channel::<Result<()>>();

    let recv_task = receiver.spawn(move |_| async move {
        let (sock, state) = udp4_pair(recv_addr)?;
        let tokio_sock = tokio::net::UdpSocket::from_std(UdpSocket::from(sock.try_clone()?))?;
        let _ = bound_tx.send(Ok(()));

        for expected in [EcnCodepoint::Ect0, EcnCodepoint::Ect1, EcnCodepoint::Ce] {
            loop {
                tokio::time::timeout(Duration::from_secs(2), tokio_sock.readable()).await??;
                match recv_ecn(&sock, &state) {
                    Ok(ecn) => {
                        assert_eq!(ecn, Some(expected), "expected {expected:?}, got {ecn:?}");
                        break;
                    }
                    Err(e)
                        if e.downcast_ref::<std::io::Error>()
                            .is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock) =>
                    {
                        continue
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        anyhow::Ok(())
    })?;

    bound_rx.await??;

    let send_task = sender.spawn(move |_| async move {
        let (sock, state) = udp4_pair("0.0.0.0:0".parse()?)?;
        for codepoint in [EcnCodepoint::Ect0, EcnCodepoint::Ect1, EcnCodepoint::Ce] {
            send_ecn(&sock, &state, recv_addr, codepoint)?;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        anyhow::Ok(())
    })?;

    send_task.await??;
    recv_task.await??;
    Ok(())
}

/// TCP ECN negotiation (SYN with ECE+CWR) works through patchbay.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn tcp_ecn_negotiation() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let server = lab.add_device("server").uplink(dc.id()).build().await?;
    let client = lab.add_device("client").uplink(dc.id()).build().await?;

    let server_ip = server.ip().unwrap();
    let server_addr = SocketAddr::new(IpAddr::V4(server_ip), 22_002);

    server.run_sync(|| {
        std::fs::write("/proc/sys/net/ipv4/tcp_ecn", "1")?;
        Ok(())
    })?;
    client.run_sync(|| {
        std::fs::write("/proc/sys/net/ipv4/tcp_ecn", "1")?;
        Ok(())
    })?;

    let server_task = server.spawn(move |_| async move {
        let listener = tokio::net::TcpListener::bind(server_addr).await?;
        let (mut stream, _) = listener.accept().await?;
        use tokio::io::AsyncWriteExt;
        stream.write_all(b"ECN_OK").await?;
        stream.shutdown().await?;
        anyhow::Ok(())
    })?;

    let client_result = client.spawn(move |_| async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut stream = tokio::net::TcpStream::connect(server_addr).await?;
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 16];
        let n = stream.read(&mut buf).await?;
        assert_eq!(&buf[..n], b"ECN_OK");

        let raw_fd = stream.as_raw_fd();
        let mut info: libc::tcp_info = unsafe { std::mem::zeroed() };
        let mut len = size_of::<libc::tcp_info>() as libc::socklen_t;
        let ret = unsafe {
            libc::getsockopt(
                raw_fd,
                libc::IPPROTO_TCP,
                libc::TCP_INFO,
                &mut info as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        assert_eq!(
            ret,
            0,
            "getsockopt TCP_INFO: {}",
            std::io::Error::last_os_error()
        );
        // tcpi_options bit 3 (0x08) is TCPI_OPT_ECN.
        let ecn_negotiated = info.tcpi_options & 0x08 != 0;
        assert!(
            ecn_negotiated,
            "TCP ECN should be negotiated (tcpi_options={:#04x})",
            info.tcpi_options
        );

        anyhow::Ok(())
    })?;

    client_result.await??;
    server_task.await??;
    Ok(())
}
