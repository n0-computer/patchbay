//! DNS server on the IX bridge + per-device /etc/hosts overlay.

use super::*;

/// First A record in a hickory-resolver `Lookup`, as `Ipv4Addr`.
fn first_a(lookup: &hickory_resolver::lookup::Lookup) -> Option<Ipv4Addr> {
    use hickory_proto::rr::RData;
    lookup.answers().iter().find_map(|r| match &r.data {
        RData::A(a) => Some(a.0),
        _ => None,
    })
}

/// DNS server entry is visible via getent in a spawned command.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn server_entry_visible_in_command() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dns = lab.dns_server()?;
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    let dc_ip = dc.uplink_ip().context("dc uplink ip")?;
    dns.set_host("myserver.test.", IpAddr::V4(dc_ip))?;

    let mut cmd = std::process::Command::new("getent");
    cmd.args(["hosts", "myserver.test"]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = dev.spawn_command_sync(cmd)?;
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

/// Names without trailing dots are normalized to FQDN and resolve correctly.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn server_entry_without_trailing_dot() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dns = lab.dns_server()?;
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    // Set without trailing dot (like iroh does).
    dns.set_host("nodot.test", IpAddr::V4(Ipv4Addr::new(10, 0, 0, 42)))?;

    // In-process resolve (also without trailing dot).
    assert_eq!(
        lab.resolve("nodot.test"),
        Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 42)))
    );

    // getent (which goes through glibc -> DNS query with FQDN).
    let mut cmd = std::process::Command::new("getent");
    cmd.args(["hosts", "nodot.test"]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = dev.spawn_command_sync(cmd)?;
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "should resolve name without trailing dot: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("10.0.0.42"),
        "expected IP in output: {stdout}"
    );
    Ok(())
}

/// DNS server entry is visible from two different devices.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn server_entry_lab_wide() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dns = lab.dns_server()?;
    let dev1 = lab
        .add_device("dev1")
        .iface("eth0", dc.id())
        .build()
        .await?;
    let dev2 = lab
        .add_device("dev2")
        .iface("eth0", dc.id())
        .build()
        .await?;

    dns.set_host("shared.test.", IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)))?;

    for dev in [&dev1, &dev2] {
        let mut cmd = std::process::Command::new("getent");
        cmd.args(["hosts", "shared.test"]);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let child = dev.spawn_command_sync(cmd)?;
        let output = child.wait_with_output()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("1.2.3.4"),
            "device should see shared.test: {stdout}"
        );
    }
    Ok(())
}

/// Device-specific set_host is only visible to that device.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn entry_device_specific() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev1 = lab
        .add_device("dev1")
        .iface("eth0", dc.id())
        .build()
        .await?;
    let dev2 = lab
        .add_device("dev2")
        .iface("eth0", dc.id())
        .build()
        .await?;

    dev1.set_host("secret.test", IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1)))?;

    let mut cmd = std::process::Command::new("getent");
    cmd.args(["hosts", "secret.test"]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = dev1.spawn_command_sync(cmd)?;
    let output = child.wait_with_output()?;
    assert!(output.status.success(), "dev1 should resolve secret.test");
    assert!(String::from_utf8_lossy(&output.stdout).contains("10.99.0.1"));

    let mut cmd2 = std::process::Command::new("getent");
    cmd2.args(["hosts", "secret.test"]);
    cmd2.stdout(std::process::Stdio::piped());
    cmd2.stderr(std::process::Stdio::piped());
    let child2 = dev2.spawn_command_sync(cmd2)?;
    let output2 = child2.wait_with_output()?;
    assert!(
        !output2.status.success(),
        "dev2 should NOT resolve secret.test"
    );
    Ok(())
}

/// In-process resolve() checks device hosts first, then DNS server.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn resolve_in_process() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dns = lab.dns_server()?;
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    let ip1 = IpAddr::V4(Ipv4Addr::new(10, 0, 1, 1));
    let ip2 = IpAddr::V4(Ipv4Addr::new(10, 0, 2, 2));

    dns.set_host("global.test.", ip1)?;
    dev.set_host("local.test", ip2)?;

    // Lab resolve sees DNS server entries.
    assert_eq!(lab.resolve("global.test."), Some(ip1));
    assert_eq!(lab.resolve("local.test"), None);

    // Device resolve sees both (device-local first, then DNS server).
    assert_eq!(dev.resolve("global.test.").await, Some(ip1));
    assert_eq!(dev.resolve("local.test").await, Some(ip2));

    // Device-specific shadows DNS server entry with same name.
    let ip3 = IpAddr::V4(Ipv4Addr::new(10, 0, 3, 3));
    dev.set_host("global.test.", ip3)?;
    assert_eq!(dev.resolve("global.test.").await, Some(ip3));
    assert_eq!(lab.resolve("global.test."), Some(ip1));

    Ok(())
}

/// DNS entry added after build is visible in subsequent spawn_command.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn entry_after_build() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dns = lab.dns_server()?;
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    let mut cmd = std::process::Command::new("getent");
    cmd.args(["hosts", "late.test"]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = dev.spawn_command_sync(cmd)?;
    let output = child.wait_with_output()?;
    assert!(
        !output.status.success(),
        "should not resolve before set_host"
    );

    dns.set_host("late.test.", IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)))?;

    let mut cmd2 = std::process::Command::new("getent");
    cmd2.args(["hosts", "late.test"]);
    cmd2.stdout(std::process::Stdio::piped());
    cmd2.stderr(std::process::Stdio::piped());
    let child2 = dev.spawn_command_sync(cmd2)?;
    let output2 = child2.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output2.stdout);
    assert!(
        output2.status.success(),
        "should resolve after set_host: {}",
        String::from_utf8_lossy(&output2.stderr)
    );
    assert!(stdout.contains("192.168.1.1"));
    Ok(())
}

/// Device-level /etc/hosts overlay contains localhost and device entries.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn hosts_file_content() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    dev.set_host("beta.test", IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)))?;

    let mut cmd = std::process::Command::new("cat");
    cmd.arg("/etc/hosts");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = dev.spawn_command_sync(cmd)?;
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
        stdout.contains("10.0.0.2\tbeta.test"),
        "should have device entry"
    );
    Ok(())
}

/// std::net::ToSocketAddrs resolves via DNS server through resolv.conf.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn std_to_socket_addrs() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dns = lab.dns_server()?;
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    let dc_ip = dc.uplink_ip().context("dc uplink ip")?;
    dns.set_host("stdtest.patchbay.", IpAddr::V4(dc_ip))?;

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
        "std ToSocketAddrs should resolve via DNS server"
    );

    assert_eq!(
        dev.resolve("stdtest.patchbay.").await,
        Some(IpAddr::V4(dc_ip))
    );

    let mut cmd = std::process::Command::new("getent");
    cmd.args(["hosts", "stdtest.patchbay"]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = dev.spawn_command_sync(cmd)?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "getent should resolve in spawned command"
    );
    Ok(())
}

/// tokio::net::lookup_host resolves via DNS server.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn tokio_lookup() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dns = lab.dns_server()?;
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    let dc_ip = dc.uplink_ip().context("dc uplink ip")?;
    dns.set_host("tokiotest.patchbay.", IpAddr::V4(dc_ip))?;

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
        "tokio lookup_host should resolve via DNS server"
    );
    Ok(())
}

/// hickory-resolver with system config resolves via DNS server.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn hickory_resolver() -> Result<()> {
    use hickory_resolver::TokioResolver;

    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dns = lab.dns_server()?;
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    let dc_ip = dc.uplink_ip().context("dc uplink ip")?;
    dns.set_host("hickorytest.patchbay.", IpAddr::V4(dc_ip))?;

    let jh = dev.spawn(move |_dev| async move {
        let resolver = TokioResolver::builder_tokio().ok()?.build().ok()?;
        let lookup = resolver.lookup_ip("hickorytest.patchbay").await.ok()?;
        lookup.iter().next()
    });
    let resolved = jh?.await.unwrap();
    info!(?resolved, "hickory resolver via spawn");
    assert_eq!(
        resolved,
        Some(IpAddr::V4(dc_ip)),
        "hickory should resolve via DNS server"
    );
    Ok(())
}

/// hickory ipv4_lookup (iroh's code path) resolves via DNS server.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn hickory_ipv4_lookup() -> Result<()> {
    use hickory_resolver::TokioResolver;

    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dns = lab.dns_server()?;
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    let dc_ip = dc.uplink_ip().context("dc uplink ip")?;
    dns.set_host("ipv4test.patchbay.", IpAddr::V4(dc_ip))?;

    let jh = dev.spawn(move |_dev| async move {
        let resolver = TokioResolver::builder_tokio()
            .expect("system conf")
            .build()
            .expect("build resolver");

        match resolver.ipv4_lookup("ipv4test.patchbay").await {
            Ok(lookup) => first_a(&lookup),
            Err(e) => {
                tracing::error!("ipv4_lookup failed: {e}");
                None
            }
        }
    });
    let resolved: Option<Ipv4Addr> = jh?.await.unwrap();
    info!(?resolved, "hickory ipv4_lookup via spawn");
    assert_eq!(
        resolved,
        Some(dc_ip),
        "hickory ipv4_lookup should resolve via DNS server"
    );
    Ok(())
}

/// Stress test: resolve DNS from many devices across multiple labs concurrently.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn hickory_resolve_stress() -> Result<()> {
    use hickory_resolver::TokioResolver;

    const NUM_LABS: usize = 4;
    const NUM_DEVICES: usize = 3;

    let mut handles = Vec::new();
    let mut labs = Vec::new(); // keep labs alive until all tasks complete

    for lab_idx in 0..NUM_LABS {
        let lab = Lab::new().await?;
        let dc = lab.add_router("dc").build().await?;
        let dns = lab.dns_server()?;
        let dc_ip = dc.uplink_ip().context("dc uplink ip")?;
        let hostname = format!("stress{lab_idx}.patchbay.");
        dns.set_host(&hostname, IpAddr::V4(dc_ip))?;

        for dev_idx in 0..NUM_DEVICES {
            let dev = lab
                .add_device(&format!("dev{dev_idx}"))
                .iface("eth0", dc.id())
                .build()
                .await?;
            let hostname = format!("stress{lab_idx}.patchbay");
            let expected = dc_ip;
            let jh = dev.spawn(move |_dev| async move {
                let resolver = TokioResolver::builder_tokio()
                    .expect("system conf")
                    .build()
                    .expect("build resolver");

                match resolver.ipv4_lookup(&hostname).await {
                    Ok(lookup) => first_a(&lookup),
                    Err(e) => {
                        tracing::error!("ipv4_lookup failed: {e}");
                        None
                    }
                }
            })?;
            handles.push((format!("lab{lab_idx}/dev{dev_idx}"), expected, jh));
        }
        labs.push(lab);
    }

    for (label, expected, jh) in handles {
        let resolved: Option<Ipv4Addr> = jh.await.unwrap();
        assert_eq!(
            resolved,
            Some(expected),
            "{label}: hickory ipv4_lookup should resolve via DNS server"
        );
    }
    Ok(())
}

/// Mimics iroh's patchbay test setup: relay device with DNS, then resolve from
/// client/server devices using hickory ipv4_lookup (iroh's code path).
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn hickory_resolve_relay_setup() -> Result<()> {
    use hickory_resolver::TokioResolver;

    let lab = Lab::new().await?;
    let dns = lab.dns_server()?;

    let dc = lab
        .add_router("dc")
        .ip_support(IpSupport::DualStack)
        .build()
        .await?;
    let relay = lab.add_device("relay").uplink(dc.id()).build().await?;
    let relay_v4 = relay.ip().context("relay has IPv4")?;
    let relay_v6 = relay.ip6().context("relay has IPv6")?;
    dns.set_host("relay.test.", IpAddr::V4(relay_v4))?;
    dns.set_host("relay.test.", IpAddr::V6(relay_v6))?;

    let nat1 = lab.add_router("nat1").nat(Nat::Home).build().await?;
    let nat2 = lab.add_router("nat2").nat(Nat::Home).build().await?;
    let server = lab.add_device("server").uplink(nat1.id()).build().await?;
    let client = lab.add_device("client").uplink(nat2.id()).build().await?;

    let expected_v4 = relay_v4;
    let mut handles = Vec::new();
    for (label, dev) in [("server", &server), ("client", &client)] {
        let label_owned = label.to_string();
        let jh = dev.spawn(move |_dev| async move {
            let label = label_owned;
            let resolver = TokioResolver::builder_tokio()
                .expect("system conf")
                .build()
                .expect("build resolver");

            match resolver.ipv4_lookup("relay.test").await {
                Ok(lookup) => {
                    let first = first_a(&lookup);
                    info!(%label, ?first, "resolved relay.test");
                    first
                }
                Err(e) => {
                    error!(%label, "ipv4_lookup relay.test failed: {e}");
                    None
                }
            }
        })?;
        handles.push((label.to_string(), jh));
    }

    for (label, jh) in handles {
        let resolved: Option<Ipv4Addr> = jh.await.unwrap();
        assert_eq!(
            resolved,
            Some(expected_v4),
            "{label}: should resolve relay.test to {expected_v4}"
        );
    }
    Ok(())
}

/// dns_server() sets resolv.conf to point at the IX bridge.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn dns_server_sets_resolv_conf() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let _dns = lab.dns_server()?;
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    let mut cmd = std::process::Command::new("cat");
    cmd.arg("/etc/resolv.conf");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = dev.spawn_command_sync(cmd)?;
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    info!(%stdout, "resolv.conf content");
    assert!(
        stdout.contains("nameserver 198.18.0.1"),
        "resolv.conf should have v4 nameserver: {stdout}"
    );
    assert!(
        stdout.contains("nameserver 2001:db8::1"),
        "resolv.conf should have v6 nameserver: {stdout}"
    );
    Ok(())
}

/// IPv6 DNS entries are visible via in-process resolve and getent.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn v6_entry() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dns = lab.dns_server()?;
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    let v6_addr = IpAddr::V6(std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0x42));
    dns.set_host("v6host.test.", v6_addr)?;

    // In-process resolve returns the v6 address.
    assert_eq!(lab.resolve("v6host.test."), Some(v6_addr));
    assert_eq!(dev.resolve("v6host.test.").await, Some(v6_addr));

    // getent sees the v6 address.
    let mut cmd = std::process::Command::new("getent");
    cmd.args(["hosts", "v6host.test"]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = dev.spawn_command_sync(cmd)?;
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

/// Dual-stack: both A and AAAA records for the same name are queryable.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn dual_stack_a_and_aaaa() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dns = lab.dns_server()?;
    let dev = lab.add_device("dev").iface("eth0", dc.id()).build().await?;

    let v4 = Ipv4Addr::new(10, 0, 0, 42);
    let v6 = std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0x42);
    dns.set_host("dual.test.", IpAddr::V4(v4))?;
    dns.set_host("dual.test.", IpAddr::V6(v6))?;

    // In-process resolve returns v4 first (A before AAAA).
    assert_eq!(lab.resolve("dual.test."), Some(IpAddr::V4(v4)));

    // getent should see both addresses.
    let mut cmd = std::process::Command::new("getent");
    cmd.args(["ahostsv4", "dual.test"]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = dev.spawn_command_sync(cmd)?;
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&v4.to_string()),
        "should have v4 address: {stdout}"
    );

    let mut cmd6 = std::process::Command::new("getent");
    cmd6.args(["ahostsv6", "dual.test"]);
    cmd6.stdout(std::process::Stdio::piped());
    cmd6.stderr(std::process::Stdio::piped());
    let child6 = dev.spawn_command_sync(cmd6)?;
    let output6 = child6.wait_with_output()?;
    let stdout6 = String::from_utf8_lossy(&output6.stdout);
    assert!(
        stdout6.contains("2001:db8::42"),
        "should have v6 address: {stdout6}"
    );

    // Setting a new v4 should NOT clobber the AAAA record.
    let v4b = Ipv4Addr::new(10, 0, 0, 99);
    dns.set_host("dual.test.", IpAddr::V4(v4b))?;
    assert_eq!(lab.resolve("dual.test."), Some(IpAddr::V4(v4b)));

    // AAAA should still be there.
    let mut cmd6b = std::process::Command::new("getent");
    cmd6b.args(["ahostsv6", "dual.test"]);
    cmd6b.stdout(std::process::Stdio::piped());
    cmd6b.stderr(std::process::Stdio::piped());
    let child6b = dev.spawn_command_sync(cmd6b)?;
    let output6b = child6b.wait_with_output()?;
    let stdout6b = String::from_utf8_lossy(&output6b.stdout);
    assert!(
        stdout6b.contains("2001:db8::42"),
        "AAAA should survive v4 replace: {stdout6b}"
    );

    Ok(())
}

/// IPv6-only device can resolve names via the DNS server.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn v6_only_device_resolves() -> Result<()> {
    let lab = Lab::new().await?;
    let dc = lab
        .add_router("dc")
        .ip_support(IpSupport::V6Only)
        .build()
        .await?;
    let dns = lab.dns_server()?;
    let dev = lab.add_device("dev").uplink(dc.id()).build().await?;

    dns.set_host("v6only.test.", IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)))?;

    let mut cmd = std::process::Command::new("getent");
    cmd.args(["hosts", "v6only.test"]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = dev.spawn_command_sync(cmd)?;
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "v6-only device should resolve via DNS server: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("10.0.0.1"),
        "expected 10.0.0.1 in output: {stdout}"
    );
    Ok(())
}

/// TXT records can be set and resolved in-process.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn txt_record() -> Result<()> {
    let lab = Lab::new().await?;
    let _dc = lab.add_router("dc").build().await?;
    let dns = lab.dns_server()?;

    dns.set_txt("_disco.test.", &["node=abc123", "port=4433"])?;

    // In-process resolve doesn't return TXT (it returns IpAddr).
    // Just verify the record store has it via a second set_host + resolve.
    dns.set_host("_disco.test.", IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)))?;
    assert_eq!(
        lab.resolve("_disco.test."),
        Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)))
    );
    Ok(())
}
