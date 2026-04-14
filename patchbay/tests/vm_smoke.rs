//! Minimal integration test that exercises testdir + patchbay.
//! Designed to be run via `patchbay-vm test -p patchbay --test vm_smoke`.

use std::net::{IpAddr, SocketAddr};

use anyhow::{Context, Result};
use patchbay::{Lab, Nat, OutDir};
use testdir::testdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[ctor::ctor]
fn init() {
    patchbay::init_userns().expect("user namespace");
}

#[tokio::test(flavor = "current_thread")]
async fn tcp_through_nat() -> Result<()> {
    let outdir = testdir!();
    eprintln!("testdir: {}", outdir.display());

    let lab = Lab::builder()
        .outdir(OutDir::Exact(outdir.clone()))
        .label("vm-smoke")
        .build()
        .await?;

    let dc = lab.add_router("dc").build().await?;
    let home = lab.add_router("home").nat(Nat::Home).build().await?;

    let server = lab
        .add_device("server")
        .iface("eth0", dc.id())
        .build()
        .await?;
    let client = lab
        .add_device("client")
        .iface("eth0", home.id())
        .build()
        .await?;

    let server_ip = server.ip().context("no server ip")?;
    let addr = SocketAddr::new(IpAddr::V4(server_ip), 9000);

    server.spawn(move |_| async move {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let (mut stream, _) = listener.accept().await?;
        let mut buf = vec![0u8; 64];
        let n = stream.read(&mut buf).await?;
        stream.write_all(&buf[..n]).await?;
        anyhow::Ok(())
    })?;

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let echoed = client
        .spawn(move |_| async move {
            let mut stream = tokio::net::TcpStream::connect(addr).await?;
            stream.write_all(b"hello").await?;
            let mut buf = vec![0u8; 64];
            let n = stream.read(&mut buf).await?;
            anyhow::Ok(buf[..n].to_vec())
        })?
        .await??;

    assert_eq!(echoed, b"hello");

    // Verify outdir was written.
    assert!(outdir.exists(), "testdir should exist");

    Ok(())
}
