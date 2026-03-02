//! Name resolution: /etc/hosts overlay, resolv.conf, in-process resolve.

use super::*;

/// Lab-wide dns_entry is visible in a spawned command's /etc/hosts.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn entry_visible_in_command() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("dc uplink ip")?;
    lab.dns_entry("myserver.test", IpAddr::V4(dc_ip))?;

    let mut cmd = std::process::Command::new("getent");
    cmd.args(["hosts", "myserver.test"]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = dev.spawn_command(cmd)?;
    let output = child.wait_with_output().context("wait getent")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    info!(%stdout, "getent output");
    assert!(
        output.status.success(),
        "getent failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&dc_ip.to_string()),
        "expected {dc_ip} in getent output: {stdout}"
    );
    Ok(())
}

/// Lab-wide dns_entry is visible from two different devices.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn entry_lab_wide() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev1 = lab
        .add_device("dev1")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;
    let dev2 = lab
        .add_device("dev2")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    lab.dns_entry("shared.test", IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)))?;

    for dev in [&dev1, &dev2] {
        let mut cmd = std::process::Command::new("getent");
        cmd.args(["hosts", "shared.test"]);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let child = dev.spawn_command(cmd)?;
        let output = child.wait_with_output()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("1.2.3.4"),
            "device should see shared.test: {stdout}"
        );
    }
    Ok(())
}

/// Device-specific dns_entry is only visible to that device.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn entry_device_specific() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev1 = lab
        .add_device("dev1")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;
    let dev2 = lab
        .add_device("dev2")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    dev1.dns_entry("secret.test", IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1)))?;

    let mut cmd = std::process::Command::new("getent");
    cmd.args(["hosts", "secret.test"]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = dev1.spawn_command(cmd)?;
    let output = child.wait_with_output()?;
    assert!(output.status.success(), "dev1 should resolve secret.test");
    assert!(String::from_utf8_lossy(&output.stdout).contains("10.99.0.1"));

    let mut cmd2 = std::process::Command::new("getent");
    cmd2.args(["hosts", "secret.test"]);
    cmd2.stdout(std::process::Stdio::piped());
    cmd2.stderr(std::process::Stdio::piped());
    let child2 = dev2.spawn_command(cmd2)?;
    let output2 = child2.wait_with_output()?;
    assert!(
        !output2.status.success(),
        "dev2 should NOT resolve secret.test"
    );
    Ok(())
}

/// In-process resolve() returns correct IPs, including shadowing semantics.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn resolve_in_process() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    let ip1 = IpAddr::V4(Ipv4Addr::new(10, 0, 1, 1));
    let ip2 = IpAddr::V4(Ipv4Addr::new(10, 0, 2, 2));

    lab.dns_entry("global.test", ip1)?;
    dev.dns_entry("local.test", ip2)?;

    assert_eq!(lab.resolve("global.test"), Some(ip1));
    assert_eq!(lab.resolve("local.test"), None);

    assert_eq!(dev.resolve("global.test"), Some(ip1));
    assert_eq!(dev.resolve("local.test"), Some(ip2));

    // Device-specific shadows global with same name.
    let ip3 = IpAddr::V4(Ipv4Addr::new(10, 0, 3, 3));
    dev.dns_entry("global.test", ip3)?;
    assert_eq!(dev.resolve("global.test"), Some(ip3));
    assert_eq!(lab.resolve("global.test"), Some(ip1));

    Ok(())
}

/// dns_entry added after build is visible in subsequent spawn_command.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn entry_after_build() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    let mut cmd = std::process::Command::new("getent");
    cmd.args(["hosts", "late.test"]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = dev.spawn_command(cmd)?;
    let output = child.wait_with_output()?;
    assert!(
        !output.status.success(),
        "should not resolve before dns_entry"
    );

    lab.dns_entry("late.test", IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)))?;

    let mut cmd2 = std::process::Command::new("getent");
    cmd2.args(["hosts", "late.test"]);
    cmd2.stdout(std::process::Stdio::piped());
    cmd2.stderr(std::process::Stdio::piped());
    let child2 = dev.spawn_command(cmd2)?;
    let output2 = child2.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output2.stdout);
    assert!(
        output2.status.success(),
        "should resolve after dns_entry: {}",
        String::from_utf8_lossy(&output2.stderr)
    );
    assert!(stdout.contains("192.168.1.1"));
    Ok(())
}

/// Generated /etc/hosts file contains localhost, global, and device entries.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn hosts_file_content() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    lab.dns_entry("alpha.test", IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)))?;
    dev.dns_entry("beta.test", IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)))?;

    let mut cmd = std::process::Command::new("cat");
    cmd.arg("/etc/hosts");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = dev.spawn_command(cmd)?;
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    info!(%stdout, "hosts file content");

    assert!(
        stdout.contains("127.0.0.1\tlocalhost"),
        "should have localhost"
    );
    assert!(
        stdout.contains("::1\tlocalhost"),
        "should have ipv6 localhost"
    );
    assert!(
        stdout.contains("10.0.0.1\talpha.test"),
        "should have global entry"
    );
    assert!(
        stdout.contains("10.0.0.2\tbeta.test"),
        "should have device entry"
    );
    Ok(())
}

/// std::net::ToSocketAddrs resolves custom DNS names via the sync worker's
/// bind-mounted /etc/hosts overlay.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn std_to_socket_addrs() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("dc uplink ip")?;
    lab.dns_entry("stdtest.patchbay", IpAddr::V4(dc_ip))?;

    let resolved_ip = dev.run_sync(|| {
        use std::net::ToSocketAddrs;
        let addr = ("stdtest.patchbay", 80u16)
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| addrs.next())
            .map(|a| a.ip());
        Ok(addr)
    })?;
    info!(?resolved_ip, "std::net::ToSocketAddrs via run_sync");
    assert_eq!(
        resolved_ip,
        Some(IpAddr::V4(dc_ip)),
        "std ToSocketAddrs should resolve via sync worker /etc/hosts overlay"
    );

    assert_eq!(dev.resolve("stdtest.patchbay"), Some(IpAddr::V4(dc_ip)));

    let mut cmd = std::process::Command::new("getent");
    cmd.args(["hosts", "stdtest.patchbay"]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = dev.spawn_command(cmd)?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "getent should resolve in spawned command"
    );
    Ok(())
}

/// tokio::net::lookup_host resolves via the blocking pool's /etc/hosts overlay.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn tokio_lookup() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("dc uplink ip")?;
    lab.dns_entry("tokiotest.patchbay", IpAddr::V4(dc_ip))?;

    let jh = dev.spawn(move |_dev| async move {
        tokio::net::lookup_host("tokiotest.patchbay:80")
            .await
            .ok()
            .and_then(|mut addrs| addrs.next())
            .map(|a| a.ip())
    });
    let resolved = jh?.await.unwrap();
    info!(?resolved, "tokio lookup_host via spawn");
    assert_eq!(
        resolved,
        Some(IpAddr::V4(dc_ip)),
        "tokio lookup_host should resolve via blocking pool /etc/hosts overlay"
    );
    Ok(())
}

/// hickory-resolver with system config resolves via the async worker's
/// /etc/hosts overlay.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn hickory_resolver() -> Result<()> {
    use hickory_resolver::TokioResolver;

    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    let dc_ip = dc.uplink_ip().context("dc uplink ip")?;
    lab.dns_entry("hickorytest.patchbay", IpAddr::V4(dc_ip))?;

    let jh = dev.spawn(move |_dev| async move {
        let resolver = TokioResolver::builder_tokio().ok()?.build();
        let lookup = resolver.lookup_ip("hickorytest.patchbay").await.ok()?;
        lookup.iter().next()
    });
    let resolved = jh?.await.unwrap();
    info!(?resolved, "hickory resolver via spawn");
    assert_eq!(
        resolved,
        Some(IpAddr::V4(dc_ip)),
        "hickory should resolve via async worker /etc/hosts overlay"
    );
    Ok(())
}

/// set_nameserver writes resolv.conf visible to spawned commands.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn set_nameserver() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    lab.set_nameserver(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)))?;

    let mut cmd = std::process::Command::new("cat");
    cmd.arg("/etc/resolv.conf");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = dev.spawn_command(cmd)?;
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    info!(%stdout, "resolv.conf content");
    assert!(
        stdout.contains("nameserver 8.8.8.8"),
        "resolv.conf should contain nameserver line: {stdout}"
    );
    Ok(())
}

/// IPv6 DNS entries are visible via getent and in-process resolve.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn v6_entry() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab
        .add_device("dev")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    let v6_addr = IpAddr::V6(std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0x42));
    lab.dns_entry("v6host.test", v6_addr)?;

    // In-process resolve returns the v6 address.
    assert_eq!(lab.resolve("v6host.test"), Some(v6_addr));
    assert_eq!(dev.resolve("v6host.test"), Some(v6_addr));

    // getent sees the v6 address.
    let mut cmd = std::process::Command::new("getent");
    cmd.args(["hosts", "v6host.test"]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = dev.spawn_command(cmd)?;
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "getent should resolve v6 entry: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("2001:db8::42"),
        "expected v6 address in output: {stdout}"
    );
    Ok(())
}
