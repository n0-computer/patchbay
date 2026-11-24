//! The user-namespace bootstrap falls back to a single-id map when the
//! `newuidmap`/`newgidmap` helpers fail.
//!
//! Each test runs its own binary again with broken helpers first on `PATH`.
//! The child bootstraps from a ctor, before the test harness starts threads.

use std::sync::OnceLock;

use anyhow::{ensure, Context, Result};

/// Set in the child process, to the name of the test it runs.
const CHILD_ENV: &str = "PATCHBAY_TEST_FALLBACK_CHILD";

static CHILD_RESULT: OnceLock<Result<(), String>> = OnceLock::new();

#[ctor::ctor(unsafe)]
fn init() {
    if std::env::var_os(CHILD_ENV).is_some() {
        let result = patchbay::init_userns().map_err(|e| format!("{e:#}"));
        let _ = CHILD_RESULT.set(result);
    }
}

/// Failing helpers cost root, not the namespace.
#[tokio::test(flavor = "current_thread")]
async fn broken_helpers_fall_back_to_single_id_map() -> Result<()> {
    run(
        "broken_helpers_fall_back_to_single_id_map",
        &["newuidmap", "newgidmap"],
    )
    .await
}

/// A `newgidmap` that fails after `newuidmap` worked still leaves a usable
/// namespace: the gid map falls back, and root is not available.
#[tokio::test(flavor = "current_thread")]
async fn broken_newgidmap_completes_the_gid_map() -> Result<()> {
    run("broken_newgidmap_completes_the_gid_map", &["newgidmap"]).await
}

/// In the child, checks the namespace. In the parent, runs `test` in a child
/// with the `broken` helpers shadowed by scripts that fail.
async fn run(test: &str, broken: &[&str]) -> Result<()> {
    if std::env::var_os(CHILD_ENV).is_some() {
        return child().await;
    }
    if !host_has_range_setup() {
        eprintln!("skipping: no /etc/subuid and /etc/subgid entries, so the helpers never run");
        return Ok(());
    }

    let dir = testdir::testdir!();
    for helper in broken {
        let path = dir.join(helper);
        std::fs::write(&path, "#!/bin/sh\necho broken helper >&2\nexit 1\n")?;
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    }
    let path = format!("{}:{}", dir.display(), std::env::var("PATH")?);
    let output = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, test)
        .env("PATH", path)
        .output()
        .context("run child")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    ensure!(
        output.status.success() && stdout.contains("1 passed"),
        "child failed:\n{stdout}{stderr}"
    );
    Ok(())
}

/// Checks the bootstrap result, the maps, the root error and that a lab works.
async fn child() -> Result<()> {
    CHILD_RESULT
        .get()
        .context("bootstrap did not run")?
        .clone()
        .map_err(anyhow::Error::msg)?;
    // SAFETY: getgid has no preconditions.
    let gid = unsafe { libc::getgid() };
    let gid_map = std::fs::read_to_string("/proc/self/gid_map")?;
    ensure!(
        gid_map.split_whitespace().collect::<Vec<_>>()
            == [gid.to_string(), gid.to_string(), "1".into()],
        "expected a single-id gid map: {gid_map:?}"
    );

    patchbay::check_caps()?;
    let lab = patchbay::Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab.add_device("dev").uplink(dc.id()).build().await?;
    ensure!(dev.ip().is_some(), "device has no ip");

    let err = dev
        .root()
        .spawn_command_sync(std::process::Command::new("true"))
        .err()
        .context("root spawn should fail without a gid 0")?;
    let err = format!("{err:#}");
    ensure!(
        err.contains("broken helper"),
        "root error does not say why: {err}"
    );
    Ok(())
}

/// Returns whether `/etc/subuid` and `/etc/subgid` have an entry for the
/// current user, by name or uid.
fn host_has_range_setup() -> bool {
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
    has_entry("/etc/subuid") && has_entry("/etc/subgid")
}
