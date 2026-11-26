//! Tests for containers as lab devices.
//!
//! The tests pull small public images. They skip where podman or a
//! subordinate id range is missing, for example in a minimal CI image.

use std::{
    io::{Read, Write},
    process::Stdio,
};

use super::*;

const ALPINE: &str = "docker.io/library/alpine:latest";

/// An image name that no registry has, for builds that must fail.
const MISSING_IMAGE: &str = "localhost/patchbay-test/no-such-image:1";

/// Returns whether to skip a container test, and prints why.
///
/// Fails without the lab capabilities, like the other lab tests.
fn skip_without_containers() -> Result<bool> {
    check_caps()?;
    let supported = containers_supported();
    if !supported {
        eprintln!("skipping: podman or a subordinate id range is missing");
    }
    Ok(!supported)
}

/// Creates a lab with one router named `net`.
async fn lab_with_router() -> Result<(Lab, Router)> {
    let lab = Lab::new().await?;
    let net = lab.add_router("net").build().await?;
    Ok((lab, net))
}

/// Returns a builder for an alpine container on `net` that sleeps.
fn sleeping_alpine(lab: &Lab, name: &str, net: NodeId) -> ContainerBuilder {
    lab.add_container(name, ALPINE)
        .uplink(net)
        .args(["sleep", "infinity"])
}

/// Returns the names of the containers that match a `podman ps` filter.
async fn podman_names(filter: &str) -> Result<Vec<String>> {
    let args = ["ps", "-a", "--filter", filter, "--format", "{{.Names}}"];
    let output = crate::container::podman(args).await?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(String::from)
        .collect())
}

/// Returns the container names of the `podman run` processes of this process.
fn podman_runs() -> Vec<String> {
    let me = std::process::id().to_string();
    let pids = std::fs::read_dir("/proc").into_iter().flatten().flatten();
    pids.filter_map(|entry| {
        let pid = entry.file_name().into_string().ok()?;
        let parent = crate::container::proc_stat_field(&pid, 4)?;
        let state = crate::container::proc_stat_field(&pid, 3)?;
        if parent != me || state == "Z" {
            return None;
        }
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let args: Vec<&[u8]> = cmdline.split(|byte| *byte == 0).collect();
        let name = args.windows(2).find(|pair| pair[0] == b"--name")?[1];
        args.contains(&&b"run"[..])
            .then(|| String::from_utf8_lossy(name).into_owned())
    })
    .collect()
}

/// Waits up to 30 seconds for a `podman run` of a name with `suffix`, and returns the name.
async fn wait_podman_run(suffix: &str) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let runs = podman_runs();
        if let Some(name) = runs.into_iter().find(|name| name.ends_with(suffix)) {
            return Ok(name);
        }
        anyhow::ensure!(Instant::now() < deadline, "podman run did not start");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Waits up to 30 seconds until no `podman run` of this process has `pattern` in its name.
async fn wait_podman_runs_done(pattern: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while podman_runs().iter().any(|name| name.contains(pattern)) {
        anyhow::ensure!(Instant::now() < deadline, "podman run does not exit");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

/// Returns whether podman knows a container of this name.
async fn container_exists(name: &str) -> Result<bool> {
    Ok(!podman_names(&format!("name=^{name}$")).await?.is_empty())
}

/// Waits up to five seconds until no container matches a `podman ps` filter.
async fn wait_none(filter: &str) -> Result<()> {
    for _ in 0..50 {
        if podman_names(filter).await?.is_empty() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("containers matching {filter} still exist")
}

/// Waits up to five seconds for a container to be gone.
async fn wait_gone(name: &str) -> Result<()> {
    wait_none(&format!("name=^{name}$")).await
}

/// Returns whether a process with `pattern` in its command line runs in the container.
async fn container_runs(container: &Container, pattern: &str) -> Result<bool> {
    let out = container.exec(["pgrep", "-f", pattern]).await?;
    Ok(out.status.success())
}

/// Waits up to ten seconds for a process with `pattern` in its command line.
async fn wait_running(container: &Container, pattern: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !container_runs(container, pattern).await? {
        anyhow::ensure!(Instant::now() < deadline, "{pattern} did not start");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

/// A container attached to a router is reachable from another device on it.
///
/// Runs `nginx:alpine`, waits for port 80 to accept, then fetches `/` from a
/// separate client device and checks for an nginx response.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_service_reachable_from_device() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }

    let (lab, net) = lab_with_router().await?;
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
        status.starts_with("HTTP/1.1 200"),
        "expected a 200 from nginx, got status line: {status:?}"
    );

    // Inside the container, root owns the image files, as with plain podman.
    let out = web
        .exec(["stat", "-c", "%u", "/etc/nginx/nginx.conf"])
        .await?;
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "0",
        "nginx.conf not owned by root: {out:?}"
    );
    Ok(())
}

/// Read-only bind mounts are visible inside the container.
///
/// The first host path holds `:` and `,`, which separate fields in podman's
/// mount options. The second is relative to the working directory, where
/// podman alone would read a bare `src` as the name of a named volume.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_bind_mounts_host_paths() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }

    let dir = testdir::testdir!().join("a:b,c");
    std::fs::create_dir(&dir).context("create mount dir")?;
    std::fs::write(dir.join("hello.txt"), "patchbay-volume").context("write mount file")?;

    let (lab, net) = lab_with_router().await?;
    // `cargo test` runs in the package root, which has `src/lib.rs`.
    let box_ = sleeping_alpine(&lab, "box", net.id())
        .volume_ro(&dir, "/data")
        .volume_ro("src", "/src")
        .build()
        .await?;

    let out = box_.exec(["cat", "/data/hello.txt"]).await?;
    assert!(
        out.status.success(),
        "cat failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "patchbay-volume"
    );
    let out = box_.exec(["test", "-f", "/src/lib.rs"]).await?;
    assert!(out.status.success(), "/src/lib.rs is missing: {out:?}");
    Ok(())
}

/// `write_file`, `read_file`, `copy_to` and `copy_from` work on a running container.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_file_transfer() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }

    let dir = testdir::testdir!();

    let (lab, net) = lab_with_router().await?;
    let box_ = sleeping_alpine(&lab, "box", net.id()).build().await?;

    box_.write_file("/tmp/greeting", "hello-from-host").await?;
    assert_eq!(box_.read_file("/tmp/greeting").await?, b"hello-from-host");

    box_.copy_from("/tmp/greeting", dir.join("out.txt")).await?;
    assert_eq!(std::fs::read(dir.join("out.txt"))?, b"hello-from-host");

    std::fs::write(dir.join("in.txt"), "from-copy-to")?;
    box_.copy_to(dir.join("in.txt"), "/tmp/in.txt").await?;
    assert_eq!(box_.read_file("/tmp/in.txt").await?, b"from-copy-to");

    // podman reads a relative path with `:` as `container:path`, and one
    // that starts with `-` as a flag. `cargo test` runs in the package root.
    struct RemoveOnDrop(String);
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let local = RemoveOnDrop(format!("-copy:{}", std::process::id()));
    std::fs::write(&local.0, "dash-colon")?;
    box_.copy_to(&local.0, "/tmp/dash.txt").await?;
    std::fs::remove_file(&local.0)?;
    box_.copy_from("/tmp/dash.txt", &local.0).await?;
    assert_eq!(std::fs::read(&local.0)?, b"dash-colon");
    Ok(())
}

/// `entrypoint` replaces the image's entrypoint, and the args follow it.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_entrypoint_replaces_the_image_entrypoint() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }
    let (lab, net) = lab_with_router().await?;
    let box_ = lab
        .add_container("box", ALPINE)
        .uplink(net.id())
        .entrypoint(["/bin/sh", "-c"])
        .args(["echo started > /tmp/marker; exec sleep infinity"])
        .build()
        .await?;
    // The command line of the first process holds the entrypoint and args.
    let out = box_.exec(["cat", "/proc/1/cmdline", "/tmp/marker"]).await?;
    let text = String::from_utf8_lossy(&out.stdout).replace('\0', " ");
    assert!(text.contains("sleep infinity"), "unexpected: {text:?}");
    assert!(text.contains("started"), "entrypoint did not run: {text:?}");
    Ok(())
}

/// `exec` runs in the device's network namespace.
///
/// It sees the lab address and reaches another lab device by ping.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_exec_uses_the_lab_network() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }
    let (lab, net) = lab_with_router().await?;
    let box_ = sleeping_alpine(&lab, "box", net.id()).build().await?;
    let client = lab.add_device("client").uplink(net.id()).build().await?;

    let box_ip = box_.ip().context("container has no ip")?.to_string();
    let out = box_.exec(["ip", "-o", "-4", "addr"]).await?;
    let addrs = String::from_utf8_lossy(&out.stdout);
    assert!(
        addrs.contains(&box_ip),
        "exec does not see {box_ip}: {addrs}"
    );

    let client_ip = client.ip().context("client has no ip")?.to_string();
    let out = box_
        .exec(["ping", "-c", "1", "-W", "2", &client_ip])
        .await?;
    assert!(
        out.status.success(),
        "ping from the container failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

/// The lab keeps a container until `stop`, `remove_device` or lab drop.
///
/// Dropping a handle does not remove it, and `container_by_name` finds it.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_follows_the_lab_lifetime() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }
    let (lab, net) = lab_with_router().await?;
    let build = |name: &'static str| sleeping_alpine(&lab, name, net.id()).build();
    let first = build("first").await?;
    let second = build("second").await?;
    let third = build("third").await?;

    let found = lab.container_by_name("first").context("first not found")?;
    assert_eq!(found.podman_name(), first.podman_name());
    drop(found);

    // A dropped handle keeps its container, and the lab still finds it.
    let third_name = third.podman_name().to_string();
    drop(third);
    assert!(container_exists(&third_name).await?);
    let found = lab.container_by_name("third").context("third not found")?;
    assert_eq!(found.podman_name(), third_name);
    drop(found);

    // A stopped container is gone from the lab while a handle lives.
    let fourth = build("fourth").await?;
    fourth.stop().await?;
    assert!(lab.container_by_name("fourth").is_none());

    // remove_device removes the container although `first` is alive.
    lab.remove_device(first.id())?;
    wait_gone(first.podman_name()).await?;
    assert!(lab.container_by_name("first").is_none());
    assert!(container_exists(second.podman_name()).await?);

    // Dropping the lab removes the containers, also those with handles.
    drop(lab);
    wait_gone(second.podman_name()).await?;
    wait_gone(&third_name).await?;
    // A stop after the lab removed the container is not an error.
    second.stop().await?;
    first.stop().await?;
    Ok(())
}

/// `remove_device` cuts a container off the lab network at once.
///
/// The container's processes keep the namespace until podman removes them.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn remove_device_cuts_the_container_off_at_once() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }
    let (lab, net) = lab_with_router().await?;
    let client = lab.add_device("client").uplink(net.id()).build().await?;
    // As PID 1, nc ignores `SIGTERM`, so podman takes a second to remove it.
    let server = lab
        .add_container("server", ALPINE)
        .uplink(net.id())
        .args(["nc", "-lk", "-p", "8080", "-e", "true"])
        .ready_tcp(8080)
        .build()
        .await?;
    let target = SocketAddr::new(IpAddr::V4(server.ip().context("no ip")?), 8080);
    let connects = || {
        client.run_sync(move || {
            let timeout = Duration::from_millis(300);
            Ok(std::net::TcpStream::connect_timeout(&target, timeout).is_ok())
        })
    };
    assert!(connects()?, "unreachable before the removal");
    lab.remove_device(server.id())?;
    assert!(!connects()?, "reachable after the removal");
    Ok(())
}

/// Containers can join an existing device.
///
/// They share its network namespace with each other and with the device's
/// own tasks, and `remove_device` removes them.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn containers_join_an_existing_device() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }
    let (lab, net) = lab_with_router().await?;
    let host = lab.add_device("host").uplink(net.id()).build().await?;

    // A Rust task in the device serves one HTTP response on loopback.
    let listener = host
        .spawn(|_| async { tokio::net::TcpListener::bind("127.0.0.1:0").await })?
        .await??;
    let url = format!("http://{}/", listener.local_addr()?);
    let server = host.spawn(move |_| async move {
        let (mut stream, _) = listener.accept().await?;
        let mut request = [0u8; 512];
        let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut request).await?;
        let response = b"HTTP/1.0 200 OK\r\nContent-Length: 9\r\n\r\nfrom-rust";
        tokio::io::AsyncWriteExt::write_all(&mut stream, response).await?;
        anyhow::Ok(())
    })?;

    let build = |name| {
        host.add_container(name, ALPINE)
            .args(["sleep", "infinity"])
            .build()
    };
    let first = build("first").await?;
    let second = build("second").await?;
    assert_eq!(first.id(), host.id());
    assert_eq!(second.ip(), host.ip());
    assert_ne!(first.podman_name(), second.podman_name());
    assert!(lab.container_by_name("second").is_some());

    let out = second.exec(["wget", "-q", "-O", "-", url.as_str()]).await?;
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "from-rust",
        "container did not reach the device's task: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    server.await??;

    // Wiring methods do not apply to an existing device.
    let err = host
        .add_container("third", ALPINE)
        .uplink(net.id())
        .build()
        .await
        .expect_err("uplink on an existing device should fail");
    assert!(format!("{err:#}").contains("existing device"), "{err:#}");

    // A container name is unique in the lab, and a failed build keeps the
    // device.
    let err = host
        .add_container("first", ALPINE)
        .build()
        .await
        .expect_err("a second container named first should fail");
    assert!(format!("{err:#}").contains("already exists"), "{err:#}");
    let err = lab
        .add_container("second", ALPINE)
        .uplink(net.id())
        .build()
        .await
        .expect_err("a container device named second should fail");
    assert!(format!("{err:#}").contains("already exists"), "{err:#}");
    assert!(lab.device_by_name("host").is_some());
    assert!(lab.device_by_name("second").is_none());

    lab.remove_device(host.id())?;
    wait_gone(first.podman_name()).await?;
    wait_gone(second.podman_name()).await
}

/// `ready_tcp` fails if another listener holds the port before the start.
///
/// The gate could not tell that listener from the container's.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn ready_tcp_fails_on_a_port_in_use() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }
    let (lab, net) = lab_with_router().await?;
    let host = lab.add_device("host").uplink(net.id()).build().await?;
    let _listener = host
        .spawn(|_| async { tokio::net::TcpListener::bind("0.0.0.0:8080").await })?
        .await??;
    let err = host
        .add_container("svc", ALPINE)
        .args(["sleep", "infinity"])
        .ready_tcp(8080)
        .build()
        .await
        .expect_err("ready_tcp passed on the device's listener");
    assert!(
        format!("{err:#}").contains("before the container"),
        "{err:#}"
    );
    Ok(())
}

/// A container name can be used again right after its container is stopped.
///
/// The stop removes a device that `Lab::add_container` created, and keeps a
/// device that the container joined.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_name_reusable_after_stop() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }
    let (lab, net) = lab_with_router().await?;
    let host = lab.add_device("host").uplink(net.id()).build().await?;
    let build = || {
        host.add_container("sidecar", ALPINE)
            .args(["sleep", "infinity"])
            .build()
    };
    let stopped = build().await?;
    stopped.stop().await?;
    assert!(
        lab.device_by_name("host").is_some(),
        "stop removed the device"
    );
    let again = build().await?;
    assert_ne!(again.podman_name(), stopped.podman_name());

    sleeping_alpine(&lab, "box", net.id())
        .build()
        .await?
        .stop()
        .await?;
    assert!(lab.device_by_name("box").is_none(), "stop kept the device");
    sleeping_alpine(&lab, "box", net.id()).build().await?;
    Ok(())
}

/// A failed or cancelled build removes the container and its new device.
///
/// The name can then be used again.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_failed_build_cleans_up() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }
    let (lab, net) = lab_with_router().await?;
    let result = sleeping_alpine(&lab, "box", net.id())
        .run_arg("--no-such-flag")
        .build()
        .await;
    assert!(result.is_err(), "build with a bad flag should fail");
    assert!(
        lab.device_by_name("box").is_none(),
        "device of failed build remains"
    );

    let result = sleeping_alpine(&lab, "box", NodeId(9999)).build().await;
    assert!(result.is_err(), "build with a missing uplink should fail");
    assert!(
        lab.device_by_name("box").is_none(),
        "device of failed wiring remains"
    );

    // A name no other test uses, which `wait_podman_run` can tell apart.
    let build = tokio::spawn(sleeping_alpine(&lab, "aborted", net.id()).build());
    let aborted = wait_podman_run("-aborted").await?;
    build.abort();
    assert!(build.await.is_err_and(|err| err.is_cancelled()));
    assert!(
        lab.device_by_name("aborted").is_none(),
        "device of cancelled build remains"
    );
    wait_podman_runs_done(&aborted).await?;
    wait_gone(&aborted).await?;

    // A builder that is dropped unbuilt frees the device name too.
    drop(sleeping_alpine(&lab, "box", net.id()));

    let box_ = sleeping_alpine(&lab, "box", net.id()).build().await?;
    assert!(container_exists(box_.podman_name()).await?);
    Ok(())
}

/// A build cancelled during `podman run` leaves no container behind.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_cancelled_build_cleans_up() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }
    let (lab, net) = lab_with_router().await?;
    let host = lab.add_device("host").uplink(net.id()).build().await?;
    let build = |name: &str| {
        host.add_container(name, ALPINE)
            .args(["sleep", "infinity"])
            .build()
    };
    // The first build also pulls the image and starts the watcher. Time the
    // second one, which spends most of its time in `podman run`.
    build("warm").await?.stop().await?;
    let start = Instant::now();
    let timed = build("timed").await?;
    let full = start.elapsed();
    timed.stop().await?;

    for percent in [30, 50, 70] {
        let name = format!("cancelled{percent}");
        if let Ok(container) = tokio::time::timeout(full * percent / 100, build(&name)).await {
            container?.stop().await?;
        }
    }
    wait_podman_runs_done("-cancelled").await?;
    wait_none("name=cancelled").await
}

/// Removing the device during `podman run` leaves no container behind.
///
/// The removal stops the device's runtime, but the build must still wait for
/// `podman run` to exit before it removes the container.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn device_removal_during_start_leaves_no_container() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }
    let (lab, net) = lab_with_router().await?;
    lab.pull_image(ALPINE).await?;
    let build = tokio::spawn(sleeping_alpine(&lab, "early", net.id()).build());
    let name = wait_podman_run("-early").await?;
    let device = lab.device_by_name("early").context("no device")?;
    lab.remove_device(device.id())?;
    assert!(build.await?.is_err(), "the build survived the removal");
    wait_podman_runs_done(&name).await?;
    wait_gone(&name).await
}

/// A container that exits fails readiness before the timeout, with its logs.
///
/// The timeout never ends, so only the exit can fail the build.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_exit_fails_readiness_early() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }
    let (lab, net) = lab_with_router().await?;
    let build = lab
        .add_container("box", ALPINE)
        .uplink(net.id())
        .args(["sh", "-c", "echo bye; exit 3"])
        .ready_tcp(80)
        .ready_timeout(Duration::MAX)
        .build();
    let err = tokio::time::timeout(Duration::from_secs(120), build)
        .await
        .context("the exit was not noticed")?
        .expect_err("readiness should fail");
    let err = format!("{err:#}");
    assert!(err.contains("exited"), "unexpected error: {err}");
    assert!(err.contains("bye"), "error lacks the logs: {err}");
    Ok(())
}

/// A container can be built from inside another device's task.
///
/// The task's worker threads carry a stacked `/proc/net` bind mount. A build
/// there cannot pull, so it fails clearly when the image is missing.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_build_from_device_task() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }
    let (lab, net) = lab_with_router().await?;
    lab.pull_image(ALPINE).await?;
    let client = lab.add_device("client").uplink(net.id()).build().await?;
    let net_id = net.id();
    let inner_lab = lab.clone();
    let name = client
        .spawn(move |_| async move {
            let err = inner_lab
                .add_container("missing", MISSING_IMAGE)
                .uplink(net_id)
                .build()
                .await
                .map(drop)
                .expect_err("build should fail");
            let err = format!("{err:#}");
            assert!(err.contains("cannot pull image"), "unexpected: {err}");

            let box_ = sleeping_alpine(&inner_lab, "box", net_id).build().await?;
            anyhow::Ok(box_.podman_name().to_string())
        })?
        .await??;
    assert!(container_exists(&name).await?);
    drop(lab);
    wait_gone(&name).await
}

/// A lab dropped while its runtime shuts down still removes its containers.
#[test]
fn container_removed_when_runtime_shuts_down() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let name = runtime.block_on(async {
        let (lab, net) = lab_with_router().await?;
        let box_ = sleeping_alpine(&lab, "box", net.id()).build().await?;
        let name = box_.podman_name().to_string();
        // Drop the last lab handle after `drop(runtime)` below started the
        // shutdown. From then on, the runtime refuses new blocking tasks.
        tokio::task::spawn_blocking(move || {
            thread::sleep(Duration::from_millis(200));
            drop(lab);
        });
        anyhow::Ok(name)
    })?;
    drop(runtime);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(wait_gone(&name))
}

/// `pull_image` pulls a missing image, and `PullPolicy::Never` never pulls.
///
/// With `Never`, a build from a present image works and a build from a
/// missing one fails.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_pull_image_and_policy_never() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }
    let (lab, net) = lab_with_router().await?;
    lab.pull_image(ALPINE).await?;
    let box_ = sleeping_alpine(&lab, "box", net.id())
        .pull(PullPolicy::Never)
        .build()
        .await?;
    assert!(container_exists(box_.podman_name()).await?);

    let err = lab
        .add_container("missing", MISSING_IMAGE)
        .uplink(net.id())
        .pull(PullPolicy::Never)
        .build()
        .await
        .expect_err("build without the image should fail");
    let err = format!("{err:#}");
    assert!(err.contains("pull policy is `Never`"), "unexpected: {err}");

    let err = lab
        .pull_image(MISSING_IMAGE)
        .await
        .expect_err("pull of a missing image should fail");
    let err = format!("{err:#}");
    assert!(err.contains("pull image"), "unexpected: {err}");

    // A podman error is not reported as a missing image.
    let err = lab
        .add_container("invalid", "INVALID//ref")
        .uplink(net.id())
        .pull(PullPolicy::Never)
        .build()
        .await
        .expect_err("build with an invalid reference should fail");
    let err = format!("{err:#}");
    assert!(err.contains("invalid reference"), "unexpected: {err}");
    Ok(())
}

/// A `container` table in a lab config starts each device instance as a container.
///
/// The lab keeps config containers like any other. `container_by_name`
/// returns them, and `remove_device` removes only the device's own one.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_from_lab_config() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }
    let cfg: config::LabConfig = toml::from_str(
        r#"
        [[router]]
        name = "net"

        [device.box]
        count = 2

        [device.box.container]
        image = "docker.io/library/alpine:latest"
        args = ["sleep", "infinity"]
        env = { GREETING = "hello" }
        tmpfs = ["/scratch"]

        [device.box.eth0]
        gateway = "net"
        "#,
    )?;
    let lab = Lab::builder().config(cfg).build().await?;
    assert!(lab.container_by_name("net").is_none());
    let first = lab.container_by_name("box-0").context("no box-0")?;
    assert!(first.ip().is_some(), "container has no lab ip");
    let out = first.exec(["sh", "-c", "echo $GREETING"]).await?;
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
    let first_name = first.podman_name().to_string();
    let first_id = first.id();
    drop(first);
    let second_name = lab
        .container_by_name("box-1")
        .context("no box-1")?
        .podman_name()
        .to_string();
    assert_ne!(first_name, second_name);
    assert!(
        container_exists(&first_name).await?,
        "lab must keep the container"
    );

    lab.remove_device(first_id)?;
    wait_gone(&first_name).await?;
    assert!(
        container_exists(&second_name).await?,
        "removing box-0 must keep box-1"
    );

    // A stop removes the container from the lab too.
    let second = lab.container_by_name("box-1").context("no box-1")?;
    second.stop().await?;
    assert!(lab.container_by_name("box-1").is_none());
    wait_gone(&second_name).await
}

/// Config errors in a `container` table fail at load and name the device.
///
/// The load fails before any device is wired, so this needs no podman.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_config_errors_fail_at_load() -> Result<()> {
    check_caps()?;
    async fn load_err(devices: &str) -> String {
        let text = format!(
            "[[router]]\nname = \"net\"\n\n[device.ok.eth0]\ngateway = \"net\"\n\n{devices}"
        );
        let cfg: config::LabConfig = toml::from_str(&text).expect("valid toml");
        let err = Lab::builder()
            .config(cfg)
            .build()
            .await
            .map(drop)
            .expect_err("load should fail");
        format!("{err:#}")
    }

    let err = load_err("[device.web.container]\nimage = \"img\"\n").await;
    assert!(
        err.contains("device 'web' has no interface sub-tables"),
        "{err}"
    );

    let err = load_err(
        "[device.web.container]\nimage = \"img\"\nenv = { A = 1 }\n\n\
         [device.web.eth0]\ngateway = \"net\"\n",
    )
    .await;
    assert!(err.contains("device 'web' invalid container"), "{err}");

    let err = load_err(
        "[device.web.container]\nimage = \"img\"\nvolumes = [\"/a\"]\n\n\
         [device.web.eth0]\ngateway = \"net\"\n",
    )
    .await;
    assert!(err.contains("device 'web' invalid container"), "{err}");
    assert!(err.contains("invalid volume '/a'"), "{err}");
    Ok(())
}

/// A command runs in the image's file system with its env, workdir, user and stdin.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_command_runs_in_the_image() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }
    let (lab, net) = lab_with_router().await?;
    let box_ = sleeping_alpine(&lab, "box", net.id()).build().await?;

    // The file exists only in the alpine image, not on the host.
    let out = box_
        .command("cat")
        .arg("/etc/alpine-release")
        .output()
        .await?;
    assert!(out.status.success(), "cat failed: {out:?}");
    assert!(!out.stdout.is_empty());

    let out = box_
        .command("sh")
        .args(["-c", "echo $GREETING $(pwd) $(id -u)"])
        .env("GREETING", "hello")
        .workdir("/tmp")
        .user("65534")
        .output()
        .await?;
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "hello /tmp 65534",
        "{out:?}"
    );

    let mut child = box_
        .command("cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    tokio::io::AsyncWriteExt::write_all(&mut stdin, b"through stdin").await?;
    drop(stdin);
    let out = child.wait_with_output().await?;
    assert!(out.status.success(), "cat failed: {out:?}");
    assert_eq!(out.stdout, b"through stdin");

    let out = box_.command("sh").args(["-c", "exit 3"]).output().await?;
    assert_eq!(out.status.code(), Some(3));
    Ok(())
}

/// `kill` stops a process and its children in the container.
///
/// The process ignores `SIGTERM` and runs as another user.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_child_kill_stops_the_process() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }
    let (lab, net) = lab_with_router().await?;
    let box_ = sleeping_alpine(&lab, "box", net.id()).build().await?;

    let mut child = box_
        .command("sh")
        .args(["-c", "trap '' TERM INT; sleep 4241; true"])
        .user("65534")
        .spawn()?;
    wait_running(&box_, "sleep 4241").await?;
    assert!(child.try_wait()?.is_none(), "exited early");
    child.kill().await?;
    assert!(child.try_wait()?.is_some(), "podman still runs after kill");
    assert!(!container_runs(&box_, "sleep 4241").await?);
    assert!(!container_runs(&box_, "trap").await?);
    Ok(())
}

/// A kill or a drop also stops processes that cleared their environment.
///
/// They lack the marker variable, but run below the command's podman client.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_child_kill_finds_processes_without_the_marker() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }
    let (lab, net) = lab_with_router().await?;
    let box_ = sleeping_alpine(&lab, "box", net.id()).build().await?;
    let spawn = |first: u32| {
        let script = format!("env -i sleep {first} & exec env -i sleep {}", first + 1);
        box_.command("sh").args(["-c", &script]).spawn()
    };

    let mut child = spawn(4250)?;
    wait_running(&box_, "sleep 4251").await?;
    child.kill().await?;
    assert!(!container_runs(&box_, "sleep 425[01]").await?);

    let child = spawn(4252)?;
    wait_running(&box_, "sleep 4253").await?;
    drop(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    while container_runs(&box_, "sleep 425[23]").await? {
        anyhow::ensure!(Instant::now() < deadline, "the drop did not kill");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

/// Dropping a child kills its process, also before the process started.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_child_drop_stops_the_process() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }
    let (lab, net) = lab_with_router().await?;
    let box_ = sleeping_alpine(&lab, "box", net.id()).build().await?;

    let child = box_.command("sleep").arg("4242").spawn()?;
    drop(child);
    let child = box_.command("sleep").arg("4243").spawn()?;
    wait_running(&box_, "sleep 4243").await?;
    drop(child);

    // The drop kills in the background. Give a leaked process time to start.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!container_runs(&box_, "sleep 4242").await?);
    assert!(!container_runs(&box_, "sleep 4243").await?);
    Ok(())
}

/// The blocking variant captures output and kills like the async one.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_command_sync() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }
    let (lab, net) = lab_with_router().await?;
    let box_ = sleeping_alpine(&lab, "box", net.id()).build().await?;

    let blocking_box = box_.clone();
    let output = tokio::task::spawn_blocking(move || -> Result<String> {
        let mut child = blocking_box
            .command("cat")
            .arg("/etc/os-release")
            .stdout(Stdio::piped())
            .spawn_sync()?;
        let mut stdout = String::new();
        child
            .stdout
            .take()
            .context("no stdout")?
            .read_to_string(&mut stdout)?;
        assert!(child.wait()?.success());
        Ok(stdout)
    })
    .await??;
    assert!(output.contains("Alpine"), "unexpected: {output}");

    let blocking_box = box_.clone();
    let mut child =
        tokio::task::spawn_blocking(move || blocking_box.command("sleep").arg("4244").spawn_sync())
            .await??;
    wait_running(&box_, "sleep 4244").await?;
    tokio::task::spawn_blocking(move || child.kill()).await??;
    assert!(!container_runs(&box_, "sleep 4244").await?);
    Ok(())
}

/// Commands resolve user names in the image.
///
/// They run as the image's `USER` when it is a name, and `user` takes a
/// name from the image's `/etc/passwd`.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_command_resolves_user_names() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }
    let (lab, net) = lab_with_router().await?;
    // The image sets `USER curl_user`.
    let box_ = lab
        .add_container("box", "docker.io/curlimages/curl:8.23.0")
        .uplink(net.id())
        .entrypoint(["sleep", "infinity"])
        .build()
        .await?;

    let out = box_.exec(["id", "-un"]).await?;
    assert!(out.status.success(), "id failed: {out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "curl_user");

    // The device's DNS files are owned by root in the container, so a
    // non-root user needs them readable by others whatever the umask.
    let out = box_.exec(["cat", "/etc/hosts", "/etc/resolv.conf"]).await?;
    assert!(out.status.success(), "cat failed: {out:?}");

    let out = box_
        .command("id")
        .arg("-un")
        .user("nobody")
        .output()
        .await?;
    assert!(out.status.success(), "id failed: {out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "nobody");
    Ok(())
}

/// The container sees the device's DNS files, also after it starts.
///
/// These are `/etc/hosts` and `/etc/resolv.conf` of the device, not the host's.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_gets_the_device_dns_files() -> Result<()> {
    if skip_without_containers()? {
        return Ok(());
    }
    let (lab, net) = lab_with_router().await?;
    let host = lab.add_device("host").uplink(net.id()).build().await?;
    host.set_host("only-in-the-lab", "192.0.2.7".parse()?)?;
    let device_resolv = host.run_sync(|| Ok(std::fs::read_to_string("/etc/resolv.conf")?))?;

    let box_ = host
        .add_container("box", ALPINE)
        .args(["sleep", "infinity"])
        .build()
        .await?;
    host.set_host("added-after-the-start", "192.0.2.8".parse()?)?;
    let out = box_.exec(["cat", "/etc/hosts"]).await?;
    let hosts = String::from_utf8_lossy(&out.stdout);
    assert!(hosts.contains("only-in-the-lab"), "{hosts}");
    assert!(hosts.contains("added-after-the-start"), "{hosts}");
    let out = box_.exec(["cat", "/etc/resolv.conf"]).await?;
    assert_eq!(String::from_utf8_lossy(&out.stdout), device_resolv);
    Ok(())
}
