//! Containers of a patchbay process that dies without cleanup are removed.
//!
//! The process's container watcher removes them as soon as the process dies,
//! also on `SIGKILL` or Ctrl-C.
//!
//! The test process itself stays outside any user namespace. It runs its own
//! binary as a patchbay process in one of these roles:
//!
//! - `check` tests for container support.
//! - `hold` starts two containers, prints their names and waits.
//!
//! Host podman then checks the result, like a user's shell would.

use std::{
    io::{BufRead, BufReader},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{bail, ensure, Context, Result};
use rustix::process::{kill_process_group, Pid, Signal};

/// Role of a child process: `check`, `hold` or `clean`.
const ROLE_ENV: &str = "PATCHBAY_TEST_CRASH_ROLE";

#[ctor::ctor(unsafe)]
fn init() {
    if std::env::var_os(ROLE_ENV).is_some() {
        patchbay::init_userns().expect("user namespace");
    }
}

/// The entry point of the child processes. Does nothing in the test process.
#[tokio::test(flavor = "current_thread")]
async fn child_role() -> Result<()> {
    match std::env::var(ROLE_ENV).as_deref() {
        Ok("check") => {
            ensure!(patchbay::containers_supported(), "no container support");
            Ok(())
        }
        Ok("hold") => hold().await,
        _ => Ok(()),
    }
}

/// The watcher removes the containers of a `SIGKILL`ed process at once.
#[test]
fn container_removed_when_owner_killed() -> Result<()> {
    with_holder(false, |holder| {
        ensure_all_exist(&holder.names)?;
        holder.child.kill()?;
        holder.child.wait()?;
        wait_all_gone(&holder.names, Duration::from_secs(20))
    })
}

/// The watcher survives a Ctrl-C and removes the containers.
///
/// A Ctrl-C sends `SIGINT` to the process group of the owner.
#[test]
fn container_removed_on_ctrl_c() -> Result<()> {
    with_holder(true, |holder| {
        ensure_all_exist(&holder.names)?;
        let pgid = Pid::from_raw(i32::try_from(holder.child.id())?).context("no pid")?;
        kill_process_group(pgid, Signal::INT).context("send SIGINT")?;
        let status = holder.child.wait()?;
        ensure!(!status.success(), "holder survived SIGINT: {status:?}");
        wait_all_gone(&holder.names, Duration::from_secs(20))
    })
}

/// The watcher survives a `SIGKILL` to the owner's process group.
///
/// nextest ends a test that times out this way.
#[test]
fn container_removed_when_the_process_group_is_killed() -> Result<()> {
    with_holder(true, |holder| {
        ensure_all_exist(&holder.names)?;
        let pgid = Pid::from_raw(i32::try_from(holder.child.id())?).context("no pid")?;
        kill_process_group(pgid, Signal::KILL).context("send SIGKILL")?;
        holder.child.wait()?;
        wait_all_gone(&holder.names, Duration::from_secs(20))
    })
}

/// A child process that holds containers.
struct Holder {
    child: Child,
    names: Vec<String>,
}

impl Holder {
    /// Starts a `hold` child and waits until it prints its containers' names.
    ///
    /// With `own_group`, the child runs in a process group of its own, so a
    /// signal to that group does not reach the test process.
    fn start(own_group: bool) -> Result<Self> {
        let mut command = child_command("hold")?;
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        if own_group {
            command.process_group(0);
        }
        let mut child = command.spawn().context("run hold child")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let mut names = None;
        for line in BufReader::new(stdout).lines() {
            if let Some(found) = line?.strip_prefix("CONTAINERS ") {
                names = Some(found.split_whitespace().map(String::from).collect());
                break;
            }
        }
        let Some(names) = names else {
            let output = child.wait_with_output()?;
            bail!(
                "hold child printed no container names: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        Ok(Self { child, names })
    }
}

/// Runs `test` with a new holder, and cleans up after it.
///
/// Skips without container support. Afterwards, kills the holder if it
/// still runs, and removes its containers if the test failed.
fn with_holder(own_group: bool, test: impl FnOnce(&mut Holder) -> Result<()>) -> Result<()> {
    if !can_run() {
        return Ok(());
    }
    let mut holder = Holder::start(own_group)?;
    let result = test(&mut holder);
    let _ = holder.child.kill();
    let _ = holder.child.wait();
    if result.is_err() {
        for name in &holder.names {
            cleanup(name);
        }
    }
    result
}

/// Starts two containers in the `hold` child, prints their names and waits.
///
/// One is a container device whose first process runs as `nobody`, the
/// other joins an existing device. The test kills the child.
async fn hold() -> Result<()> {
    let alpine = "docker.io/library/alpine:latest";
    let lab = patchbay::Lab::new().await?;
    let net = lab.add_router("net").build().await?;
    let container = lab
        .add_container("box", alpine)
        .uplink(net.id())
        .run_arg("--user=65534:65534")
        .args(["sleep", "infinity"])
        .build()
        .await?;
    let host = lab.add_device("host").uplink(net.id()).build().await?;
    let sidecar = host
        .add_container("sidecar", alpine)
        .args(["sleep", "infinity"])
        .build()
        .await?;
    println!(
        "CONTAINERS {} {}",
        container.podman_name(),
        sidecar.podman_name()
    );
    std::future::pending::<()>().await;
    unreachable!()
}

/// Returns a command that runs this binary as a child in the given role.
fn child_command(role: &str) -> Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["--exact", "child_role", "--nocapture"])
        .env(ROLE_ENV, role);
    Ok(command)
}

/// Returns whether the test can run, and prints why not otherwise.
///
/// The check needs patchbay's user namespace, so a `check` child runs it.
fn can_run() -> bool {
    let supported = child_command("check")
        .and_then(|mut command| Ok(command.output()?))
        .is_ok_and(|output| output.status.success());
    if !supported {
        eprintln!("skipping: podman or a subordinate id range is missing");
    }
    supported
}

/// Runs podman as the host user, outside any patchbay namespace.
fn host_podman(args: &[&str]) -> Result<std::process::Output> {
    Command::new("podman")
        .args(args)
        .output()
        .context("run podman")
}

/// Returns whether host podman lists a container of this name.
fn exists(name: &str) -> Result<bool> {
    let filter = format!("name=^{name}$");
    let output = host_podman(&["ps", "-a", "--filter", &filter, "--format", "{{.Names}}"])?;
    ensure!(output.status.success(), "podman ps failed: {output:?}");
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line == name))
}

/// Fails unless host podman lists every container in `names`.
fn ensure_all_exist(names: &[String]) -> Result<()> {
    for name in names {
        ensure!(exists(name)?, "container {name} is not there");
    }
    Ok(())
}

/// Waits until host podman lists none of the containers in `names`.
fn wait_all_gone(names: &[String], timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    for name in names {
        wait_gone(name, deadline.saturating_duration_since(Instant::now()))?;
    }
    Ok(())
}

/// Waits until host podman no longer lists the container.
fn wait_gone(name: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while exists(name)? {
        if Instant::now() >= deadline {
            bail!("container {name} still exists after {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// Removes a left-over container after a failure.
///
/// Host podman cannot signal it, but the host user can, so this kills its
/// first process directly.
fn cleanup(name: &str) {
    if let Ok(output) = host_podman(&["inspect", "--format", "{{.State.Pid}}", name]) {
        let pid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if pid != "0" && !pid.is_empty() {
            let _ = Command::new("kill").args(["-9", &pid]).status();
        }
    }
    let _ = host_podman(&["rm", "-f", "-t", "0", name]);
}
