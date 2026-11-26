//! Tests the user namespace from [`patchbay::init_userns`].

use std::net::{IpAddr, SocketAddr};

use anyhow::{ensure, Context, Result};
use patchbay::{IfaceConfig, Lab, LinkCondition, LinkDirection, Nat};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[ctor::ctor(unsafe)]
fn init() {
    patchbay::init_userns().expect("user namespace");
}

/// Parses `/proc/self/uid_map` into `[inner, outer, count]` lines.
fn uid_map() -> Result<Vec<[u32; 3]>> {
    let map = std::fs::read_to_string("/proc/self/uid_map")?;
    map.lines()
        .map(|line| {
            let fields: Vec<u32> = line
                .split_whitespace()
                .map(str::parse)
                .collect::<Result<_, _>>()?;
            let [inner, outer, count] = fields[..] else {
                anyhow::bail!("unexpected uid_map line: {line:?}");
            };
            Ok([inner, outer, count])
        })
        .collect()
}

/// Whether the host is set up for a subordinate range: executable
/// `newuidmap`/`newgidmap` on `PATH` and `/etc/subuid` and `/etc/subgid`
/// entries for the user (by name or uid).
fn host_has_range_setup() -> bool {
    use std::os::unix::fs::PermissionsExt;
    let on_path = |name: &str| {
        std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| {
                std::fs::metadata(dir.join(name))
                    .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            })
        })
    };
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() }.to_string();
    let user = std::env::var("USER").unwrap_or_default();
    let has_entry = |path: &str| {
        std::fs::read_to_string(path).is_ok_and(|text| {
            text.lines().any(|line| {
                let login = line.split(':').next().unwrap_or_default();
                login == uid || (!user.is_empty() && login == user)
            })
        })
    };
    on_path("newuidmap")
        && on_path("newgidmap")
        && has_entry("/etc/subuid")
        && has_entry("/etc/subgid")
}

/// Runs `sh -c script` in `dev`, as root or as the user, and returns stdout.
fn sh(dev: &patchbay::Device, root: bool, script: &str) -> Result<String> {
    let mut cmd = std::process::Command::new("sh");
    cmd.args(["-c", script])
        .stdout(std::process::Stdio::piped());
    let child = if root {
        dev.as_root().spawn_command_sync(cmd)?
    } else {
        dev.spawn_command_sync(cmd)?
    };
    let output = child.wait_with_output()?;
    ensure!(output.status.success(), "{script:?} failed: {output:?}");
    Ok(String::from_utf8(output.stdout)?)
}

/// The process keeps its uid. With a subordinate range, the ids from 0 up to
/// the range size, except the user's, are mapped too.
#[test]
fn uid_maps_to_itself() -> Result<()> {
    let map = uid_map()?;
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    ensure!(map[0] == [uid, uid, 1], "uid not mapped to itself: {map:?}");
    if map.len() == 1 {
        return Ok(());
    }
    let mapped = |id: u32| {
        map.iter()
            .any(|&[inner, _, count]| id >= inner && id - inner < count)
    };
    for id in [0, 1, 65534] {
        ensure!(mapped(id), "uid {id} not mapped: {map:?}");
    }
    Ok(())
}

/// `as_root()` spawns commands as uid 0 with every capability, and root can
/// switch to other ids such as `nobody`. Plain spawns run as the user.
///
/// `nobody` can read the device's DNS files also under a strict umask.
///
/// Requires root where the host is set up for it, unless the test runs in a
/// nested user namespace. Set `PATCHBAY_EXPECT_ROOT` to require it always.
#[tokio::test(flavor = "current_thread")]
async fn root_command_runs_as_uid_zero() -> Result<()> {
    patchbay::check_caps()?;
    // The lab writes the DNS files under this umask. It applies to the whole
    // test process, which only makes the other tests' files private.
    // SAFETY: umask has no preconditions.
    unsafe { libc::umask(0o077) };
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab.add_device("dev").uplink(dc.id()).build().await?;

    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let as_user = sh(&dev, false, "id -u")?;
    ensure!(as_user.trim() == uid.to_string(), "unexpected: {as_user:?}");

    if let Err(err) = dev
        .as_root()
        .spawn_command_sync(std::process::Command::new("true"))
    {
        let err = format!("{err:#}");
        let nested = err.contains("nested user namespace");
        let required = std::env::var_os("PATCHBAY_EXPECT_ROOT").is_some()
            || (host_has_range_setup() && !nested);
        ensure!(!required, "root not available: {err}");
        eprintln!("skipping root checks: {err}");
        return Ok(());
    }

    let as_root = sh(
        &dev,
        true,
        "id -u; grep -E '^Cap(Eff|Bnd)' /proc/self/status",
    )?;
    let mut lines = as_root.lines();
    ensure!(lines.next() == Some("0"), "not uid 0: {as_root:?}");
    let eff = lines
        .next()
        .and_then(|l| l.strip_prefix("CapEff:"))
        .map(str::trim);
    let bnd = lines
        .next()
        .and_then(|l| l.strip_prefix("CapBnd:"))
        .map(str::trim);
    ensure!(
        eff.is_some() && eff == bnd,
        "root lacks capabilities: {as_root:?}"
    );

    let as_nobody = sh(
        &dev,
        true,
        "setpriv --reuid=65534 --regid=65534 --clear-groups \
         sh -c 'cat /etc/hosts /etc/resolv.conf >/dev/null && id -u'",
    )?;
    ensure!(as_nobody.trim() == "65534", "unexpected: {as_nobody:?}");

    // The IX root namespace has a root handle too.
    let mut cmd = std::process::Command::new("id");
    cmd.arg("-u").stdout(std::process::Stdio::piped());
    let output = lab
        .ix()
        .as_root()
        .spawn_command_sync(cmd)?
        .wait_with_output()?;
    ensure!(
        String::from_utf8(output.stdout)?.trim() == "0",
        "lab root spawn not root"
    );

    // The async variant runs as root too.
    let mut cmd = tokio::process::Command::new("id");
    cmd.arg("-u").stdout(std::process::Stdio::piped());
    let output = dev.as_root().spawn_command(cmd)?.wait_with_output().await?;
    ensure!(
        String::from_utf8(output.stdout)?.trim() == "0",
        "async spawn not root"
    );
    Ok(())
}

/// NAT, link conditions, low ports and a spawned `ping` work without uid 0.
#[tokio::test(flavor = "current_thread")]
async fn lab_works_without_root() -> Result<()> {
    patchbay::check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let home = lab.add_router("home").nat(Nat::Moderate).build().await?;
    let server = lab
        .add_device("server")
        .iface("eth0", dc.id())
        .build()
        .await?;
    let client = lab
        .add_device("client")
        .iface(
            "eth0",
            IfaceConfig::routed(home.id())
                .condition(LinkCondition::mobile_4g(), LinkDirection::Egress),
        )
        .build()
        .await?;

    let server_ip = server.ip().context("no server ip")?;
    // A port below 1024 needs CAP_NET_BIND_SERVICE in the namespace.
    let addr = SocketAddr::new(IpAddr::V4(server_ip), 80);
    let listener = server
        .spawn(move |_| async move { tokio::net::TcpListener::bind(addr).await })?
        .await??;
    server.spawn(move |_| async move {
        let (mut stream, _) = listener.accept().await?;
        let mut buf = vec![0u8; 64];
        let n = stream.read(&mut buf).await?;
        stream.write_all(&buf[..n]).await?;
        anyhow::Ok(())
    })?;
    let echoed = client
        .spawn(move |_| async move {
            let mut stream = tokio::net::TcpStream::connect(addr).await?;
            stream.write_all(b"hello").await?;
            let mut buf = vec![0u8; 64];
            let n = stream.read(&mut buf).await?;
            anyhow::Ok(buf[..n].to_vec())
        })?
        .await??;
    ensure!(echoed == b"hello", "unexpected echo: {echoed:?}");

    // `ping` runs as a child process, which keeps only the ambient capabilities.
    let mut cmd = tokio::process::Command::new("ping");
    cmd.args(["-c", "1", "-W", "2", &server_ip.to_string()]);
    let status = client.spawn_command(cmd)?.wait().await?;
    ensure!(status.success(), "ping failed: {status}");
    Ok(())
}
