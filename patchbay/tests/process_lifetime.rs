//! Commands that a patchbay process spawns end with it.
//!
//! The test process stays outside any user namespace. It runs its own binary
//! as a patchbay process that spawns commands in a device, in one of these
//! roles:
//!
//! - `hold` spawns the commands, prints `ready <count>` and waits.
//! - `exit` spawns the commands, drops their handles and returns.
//!
//! The commands ignore `SIGINT` and `SIGTERM`, so a test passes only if the
//! kernel kills them.

use std::{
    io::{BufRead, BufReader},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{bail, ensure, Context, Result};
use rustix::process::{kill_process, kill_process_group, Pid, Signal};

/// Role of a child process: `hold` or `exit`.
const ROLE_ENV: &str = "PATCHBAY_TEST_LIFETIME_ROLE";
/// Prefix of the `sleep` durations, which tells one test's commands apart.
const TOKEN_ENV: &str = "PATCHBAY_TEST_LIFETIME_TOKEN";

#[ctor::ctor(unsafe)]
fn init() {
    if std::env::var_os(ROLE_ENV).is_some() {
        patchbay::init_userns().expect("user namespace");
    }
}

/// The entry point of the child processes. Does nothing in the test process.
#[tokio::test(flavor = "current_thread")]
async fn child_role() -> Result<()> {
    let role = std::env::var(ROLE_ENV);
    if !matches!(role.as_deref(), Ok("hold" | "exit")) {
        return Ok(());
    }
    let token = std::env::var(TOKEN_ENV)?;
    let lab = patchbay::Lab::new().await?;
    let net = lab.add_router("net").build().await?;
    let device = lab.add_device("box").uplink(net.id()).build().await?;
    // Null output, so that a command that outlives the child does not keep
    // the test's pipe open.
    let command = |n: u32| {
        let mut command = Command::new("sh");
        let script = format!("trap '' INT TERM; exec sleep {token}{n}");
        command
            .args(["-c", &script])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    };
    let mut children = vec![device.spawn_command_sync(command(1))?];
    let async_child = device.spawn_command(command(2).into())?;
    // Root needs a subordinate id range, which a host may lack.
    children.extend(device.as_root().spawn_command_sync(command(3)).ok());

    if role? == "hold" {
        println!("ready {}", children.len() + 1);
        std::future::pending::<()>().await;
    }
    drop((children, async_child));
    Ok(())
}

/// The kernel kills the commands of a `SIGKILL`ed process.
#[test]
fn commands_end_on_sigkill() -> Result<()> {
    let (mut child, token) = start("hold")?;
    let count = wait_ready(&mut child)?;
    ensure_all_run(&token, count)?;
    kill_process(pid(&child)?, Signal::KILL).context("kill the child")?;
    child.wait()?;
    wait_all_gone(&token)
}

/// The kernel kills the commands of a process that Ctrl-C ends.
#[test]
fn commands_end_on_ctrl_c() -> Result<()> {
    let (mut child, token) = start("hold")?;
    let count = wait_ready(&mut child)?;
    ensure_all_run(&token, count)?;
    // A terminal sends SIGINT to the whole foreground process group.
    kill_process_group(pid(&child)?, Signal::INT).context("interrupt the group")?;
    child.wait()?;
    wait_all_gone(&token)
}

/// The commands end when the process exits, also if it dropped their handles.
#[test]
fn commands_end_on_exit() -> Result<()> {
    let (child, token) = start("exit")?;
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "exit child failed:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    wait_all_gone(&token)
}

/// Starts a child process in `role`, in a process group of its own.
fn start(role: &str) -> Result<(Child, String)> {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
    let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let token = format!("{}{seq}", std::process::id());
    let child = Command::new(std::env::current_exe()?)
        .args(["--exact", "child_role", "--nocapture"])
        .env(ROLE_ENV, role)
        .env(TOKEN_ENV, &token)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()
        .context("start child")?;
    Ok((child, token))
}

/// Waits until a `hold` child prints `ready`, and returns its command count.
fn wait_ready(child: &mut Child) -> Result<usize> {
    let stdout = child.stdout.take().context("no stdout")?;
    for line in BufReader::new(stdout).lines() {
        if let Some(count) = line?.strip_prefix("ready ") {
            return Ok(count.parse()?);
        }
    }
    bail!("the child exited before it was ready: {:?}", child.wait()?)
}

fn pid(child: &Child) -> Result<Pid> {
    Pid::from_raw(child.id() as i32).context("pid 0")
}

/// Returns how many of the test's `sleep` commands run.
fn running(token: &str) -> usize {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            let cmdline = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
            let args: Vec<&[u8]> = cmdline.split(|byte| *byte == 0).collect();
            args.first() == Some(&&b"sleep"[..])
                && args
                    .get(1)
                    .is_some_and(|arg| arg.starts_with(token.as_bytes()))
        })
        .count()
}

fn ensure_all_run(token: &str, count: usize) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while running(token) < count {
        ensure!(Instant::now() < deadline, "the commands did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn wait_all_gone(token: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while running(token) > 0 {
        ensure!(
            Instant::now() < deadline,
            "{} commands outlived their process",
            running(token)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}
