//! Tests a lab after [`patchbay::init_userns_keep_uid`].

use std::net::{IpAddr, SocketAddr};

use anyhow::{ensure, Context, Result};
use patchbay::{IfaceConfig, Lab, LinkCondition, LinkDirection, Nat};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[ctor::ctor(unsafe)]
fn init() {
    patchbay::init_userns_keep_uid().expect("user namespace");
}

/// The uid inside the namespace is the uid outside it.
#[test]
fn uid_maps_to_itself() -> Result<()> {
    let map = std::fs::read_to_string("/proc/self/uid_map")?;
    let fields: Vec<&str> = map.split_whitespace().collect();
    ensure!(fields.len() == 3, "unexpected uid_map: {map:?}");
    ensure!(fields[0] == fields[1], "uid not mapped to itself: {map:?}");
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    ensure!(fields[0] == uid.to_string(), "getuid {uid} != {map:?}");
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
