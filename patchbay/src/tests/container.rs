//! Container-as-device integration: a containerized service is reachable from a
//! lab device at the container's lab IP.
//!
//! Gated on `podman` being available and pulls a small public image, so it
//! skips cleanly where podman is absent (for example in a minimal CI image).

use std::io::{Read, Write};

use super::*;

/// Returns `true` if a working `podman` is on `PATH`.
fn podman_available() -> bool {
    std::process::Command::new("podman")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A container attached to a router is reachable from another device on it.
///
/// Runs `nginx:alpine`, waits for port 80 to accept, then fetches `/` from a
/// separate client device and checks for an nginx response.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_service_reachable_from_device() -> Result<()> {
    check_caps()?;
    if !podman_available() {
        eprintln!("skipping container_service_reachable_from_device: podman not on PATH");
        return Ok(());
    }

    let lab = Lab::new().await?;
    let net = lab.add_router("net").build().await?;
    let web = lab
        .add_container("web", "docker.io/library/nginx:alpine")
        .uplink(net.id())
        .ready_tcp(80)
        .build()
        .await?;
    let client = lab.add_device("client").uplink(net.id()).build().await?;

    let web_ip = web.ip().context("container has no ip")?;
    let addr = SocketAddr::new(IpAddr::V4(web_ip), 80);

    // Fetch `/` from inside the client namespace, over the lab network.
    let response = client.run_sync(move || {
        let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5))
            .context("connect to container")?;
        stream.write_all(b"GET / HTTP/1.0\r\nHost: web\r\n\r\n")?;
        let mut buf = String::new();
        stream.read_to_string(&mut buf)?;
        Ok(buf)
    })?;

    let status = response.lines().next().unwrap_or_default();
    assert!(
        status.contains("200") || response.to_lowercase().contains("nginx"),
        "expected an nginx response from the container, got status line: {status:?}"
    );
    Ok(())
}
