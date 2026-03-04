//! MTU configuration and PMTU blackhole.

use super::*;

/// MTU 1400 set on router and device is reflected in `ip link show`.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn set_and_verify() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").mtu(1400).build().await?;
    let dev = lab
        .add_device("dev")
        .uplink(dc.id())
        .mtu(1400)
        .build()
        .await?;

    assert_eq!(dc.mtu(), Some(1400));
    assert_eq!(dev.mtu(), Some(1400));

    let output = dev.run_sync(|| {
        let out = std::process::Command::new("ip")
            .args(["link", "show", "eth0"])
            .output()?;
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    })?;
    assert!(
        output.contains("mtu 1400"),
        "expected mtu 1400 in: {output}"
    );

    let output = dc.run_sync(|| {
        let out = std::process::Command::new("ip")
            .args(["link", "show"])
            .output()?;
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    })?;
    let mtu_count = output.matches("mtu 1400").count();
    assert!(
        mtu_count >= 2,
        "expected at least 2 interfaces with mtu 1400, got {mtu_count} in: {output}"
    );

    Ok(())
}

/// `block_icmp_frag_needed()` installs an nftables rule matching frag-needed.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn block_icmp_frag_rule() -> Result<()> {
    let lab = Lab::new().await?;
    let rtr = lab
        .add_router("rtr")
        .block_icmp_frag_needed()
        .build()
        .await?;
    let _dev = lab.add_device("d").uplink(rtr.id()).build().await?;

    let output = rtr.run_sync(|| {
        let out = std::process::Command::new("nft")
            .args(["list", "ruleset"])
            .output()?;
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    })?;
    assert!(
        output.contains("frag-needed")
            || output.contains("fragmentation-needed")
            || output.contains("code 4"),
        "expected ICMP frag-needed drop rule in: {output}"
    );

    Ok(())
}

/// Large UDP packets with DF bit are silently dropped when router MTU is low
/// and ICMP frag-needed is blocked (PMTU blackhole).
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn blackhole_drops_large() -> Result<()> {
    check_caps()?;
    let lab = Lab::new().await?;

    let rtr = lab
        .add_router("rtr")
        .mtu(500)
        .block_icmp_frag_needed()
        .build()
        .await?;
    let dc = lab.add_router("dc").build().await?;

    let dev = lab.add_device("dev").uplink(rtr.id()).build().await?;
    let server = lab.add_device("server").uplink(dc.id()).build().await?;

    let server_ip = server.ip().unwrap();
    let bind_addr: SocketAddr = SocketAddr::new(IpAddr::V4(server_ip), 18_900);

    let server_thread = server.spawn_thread(move || {
        let sock = std::net::UdpSocket::bind(bind_addr).context("mtu server udp bind")?;
        sock.set_read_timeout(Some(Duration::from_secs(3)))?;
        let mut buf = [0u8; 2048];
        match sock.recv_from(&mut buf) {
            Ok((n, _)) => bail!("unexpectedly received {n} bytes — packet should be blackholed"),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                Ok(())
            }
            Err(e) => bail!("unexpected error: {e}"),
        }
    })?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    dev.run_sync(move || {
        use std::os::unix::io::AsRawFd;
        let sock = std::net::UdpSocket::bind("0.0.0.0:0").context("mtu dev udp bind")?;
        // IP_PMTUDISC_DO = 2
        let val: libc::c_int = 2;
        unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                libc::IPPROTO_IP,
                libc::IP_MTU_DISCOVER,
                &val as *const _ as *const libc::c_void,
                size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
        let payload = vec![0xABu8; 1200];
        for _ in 0..3 {
            let _ = sock.send_to(&payload, bind_addr);
            thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    })?;

    tokio::task::spawn_blocking(move || server_thread.join().unwrap()).await??;

    Ok(())
}
