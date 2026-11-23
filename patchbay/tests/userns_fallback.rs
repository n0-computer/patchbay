//! The user-namespace bootstrap falls back to a single-id map when the
//! `newuidmap`/`newgidmap` helpers fail.
//!
//! The test runs its own binary again with broken helpers first on `PATH`. The
//! child bootstraps from a ctor, before the test harness starts threads.

use std::sync::OnceLock;

use anyhow::{ensure, Context, Result};

/// Set in the child process.
const CHILD_ENV: &str = "PATCHBAY_TEST_FALLBACK_CHILD";

static CHILD_RESULT: OnceLock<Result<(), String>> = OnceLock::new();

#[ctor::ctor(unsafe)]
fn init() {
    if std::env::var_os(CHILD_ENV).is_some() {
        let result = patchbay::init_userns().map_err(|e| format!("{e:#}"));
        let _ = CHILD_RESULT.set(result);
    }
}

/// A failing `newuidmap` costs the subordinate range, not the namespace.
#[tokio::test(flavor = "current_thread")]
async fn broken_newuidmap_falls_back_to_single_id_map() -> Result<()> {
    if std::env::var_os(CHILD_ENV).is_some() {
        return child().await;
    }
    if !has_subuid_entry() {
        eprintln!("skipping: no /etc/subuid entry, so the helpers never run");
        return Ok(());
    }

    let dir = testdir::testdir!();
    for helper in ["newuidmap", "newgidmap"] {
        let path = dir.join(helper);
        std::fs::write(&path, "#!/bin/sh\nexit 1\n")?;
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    }
    let path = format!("{}:{}", dir.display(), std::env::var("PATH")?);
    let output = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "broken_newuidmap_falls_back_to_single_id_map",
            "--nocapture",
        ])
        .env(CHILD_ENV, "1")
        .env("PATH", path)
        .output()
        .context("run child")?;
    ensure!(
        output.status.success(),
        "child failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

/// Checks the bootstrap result and that a lab works in the child.
async fn child() -> Result<()> {
    CHILD_RESULT
        .get()
        .context("bootstrap did not run")?
        .clone()
        .map_err(anyhow::Error::msg)?;
    let map = std::fs::read_to_string("/proc/self/uid_map")?;
    ensure!(
        map.lines().count() == 1,
        "expected a single-id map: {map:?}"
    );
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    ensure!(
        map.split_whitespace()
            .take(2)
            .all(|id| id == uid.to_string()),
        "uid not mapped to itself: {map:?}"
    );
    patchbay::check_caps()?;
    let lab = patchbay::Lab::new().await?;
    let dc = lab.add_router("dc").build().await?;
    let dev = lab.add_device("dev").uplink(dc.id()).build().await?;
    ensure!(dev.ip().is_some(), "device has no ip");
    Ok(())
}

/// Returns whether `/etc/subuid` has an entry for the current user.
fn has_subuid_entry() -> bool {
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() }.to_string();
    let user = std::env::var("USER").unwrap_or_default();
    std::fs::read_to_string("/etc/subuid").is_ok_and(|subuid| {
        subuid.lines().any(|line| {
            let login = line.split(':').next().unwrap_or_default();
            login == uid || (!user.is_empty() && login == user)
        })
    })
}
