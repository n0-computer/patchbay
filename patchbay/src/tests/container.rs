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

/// A read-only bind mount is visible inside the container.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_bind_mount_visible() -> Result<()> {
    check_caps()?;
    if !podman_available() {
        eprintln!("skipping container_bind_mount_visible: podman not on PATH");
        return Ok(());
    }

    let dir = std::env::temp_dir().join(format!("patchbay-vol-{}", std::process::id()));
    std::fs::create_dir_all(&dir).context("create mount dir")?;
    std::fs::write(dir.join("hello.txt"), "patchbay-volume").context("write mount file")?;

    let lab = Lab::new().await?;
    let net = lab.add_router("net").build().await?;
    let box_ = lab
        .add_container("box", "docker.io/library/alpine:latest")
        .uplink(net.id())
        .volume_ro(&dir, "/data")
        .args(["sleep", "infinity"])
        .build()
        .await?;

    let out = box_.exec(["cat", "/data/hello.txt"]).await?;
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "cat failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "patchbay-volume"
    );
    Ok(())
}
