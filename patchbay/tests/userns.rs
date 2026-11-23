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

/// Whether the host gave the user a subordinate range, so root is mapped.
fn has_range() -> Result<bool> {
    Ok(uid_map()?.len() > 1)
}

/// The process keeps its uid. With a subordinate range, 0 maps to the first
/// subordinate id and the rest of the range to itself.
#[test]
fn uid_maps_to_itself() -> Result<()> {
    let map = uid_map()?;
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    ensure!(map[0] == [uid, uid, 1], "uid not mapped to itself: {map:?}");
    if let [_, [0, root, 1], rest @ ..] = &map[..] {
        for &[inner, outer, _] in rest {
            ensure!(inner == outer, "range not mapped to itself: {map:?}");
            ensure!(inner == root + 1, "range does not follow root: {map:?}");
        }
    } else {
        ensure!(map.len() == 1, "unexpected uid_map: {map:?}");
    }
    Ok(())
}

/// `root()` spawns commands as uid 0 with all capabilities; plain spawns run
/// as the user.
#[tokio::test(flavor = "current_thread")]
async fn root_command_runs_as_uid_zero() -> Result<()> {
    patchbay::check_caps()?;
    let lab = Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab.add_device("dev").uplink(dc.id()).build().await?;

    let id = |root: bool| -> Result<String> {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "id -u; grep CapEff /proc/self/status"])
            .stdout(std::process::Stdio::piped());
        let child = if root {
            dev.root().spawn_command_sync(cmd)?
        } else {
            dev.spawn_command_sync(cmd)?
        };
        let output = child.wait_with_output()?;
        ensure!(output.status.success(), "command failed: {output:?}");
        Ok(String::from_utf8(output.stdout)?)
    };

    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let as_user = id(false)?;
    ensure!(
        as_user.starts_with(&format!("{uid}\n")),
        "unexpected: {as_user:?}"
    );

    if !has_range()? {
        let err = dev
            .root()
            .spawn_command_sync(std::process::Command::new("true"));
        ensure!(err.is_err(), "root spawn without a range should fail");
        return Ok(());
    }
    let as_root = id(true)?;
    ensure!(as_root.starts_with("0\n"), "unexpected: {as_root:?}");
    ensure!(
        as_root.contains("CapEff:\t000001ffffffffff"),
        "root lacks capabilities: {as_root:?}"
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
