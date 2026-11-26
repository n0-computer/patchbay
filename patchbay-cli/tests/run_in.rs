//! Tests `patchbay run-in` against a running `patchbay inspect` session.
//!
//! The test process stays in the initial user namespace, like a user's shell.
#![cfg(target_os = "linux")]

use std::{
    io::{BufRead, BufReader},
    os::unix::{fs::MetadataExt, process::CommandExt},
    process::{Child, Command, Stdio},
};

use nix::{
    sys::signal::{killpg, Signal},
    unistd::Pid,
};

/// Kills an inspect session on drop.
///
/// The inspect process runs in a process group of its own, which its keeper
/// processes and their children inherit. A killed inspect process does not
/// stop its keepers, so the guard kills the whole group.
struct Session(Child);

impl Drop for Session {
    fn drop(&mut self) {
        let _ = killpg(Pid::from_raw(self.0.id() as i32), Signal::SIGKILL);
        let _ = self.0.wait();
    }
}

/// Whether `nsenter` supports `--keep-caps` (util-linux 2.39 or later).
fn nsenter_keeps_caps() -> bool {
    Command::new("nsenter")
        .arg("--help")
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("--keep-caps"))
}

/// `run-in` runs the command in the node's network namespace as the user,
/// also on hosts where root is mapped, and with the network capabilities
/// where `nsenter` can keep them.
#[test]
fn run_in_keeps_the_uid() {
    let dir = tempfile::tempdir().expect("temp dir");
    let topology = dir.path().join("topo.toml");
    std::fs::write(
        &topology,
        "[[router]]\nname = \"r1\"\n\n[device.node.eth0]\ngateway = \"r1\"\n",
    )
    .expect("write topology");
    let work_dir = dir.path().join("work");

    let mut session = Session(
        Command::new(env!("CARGO_BIN_EXE_patchbay"))
            .arg("inspect")
            .arg(&topology)
            .arg("--work-dir")
            .arg(&work_dir)
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .expect("start inspect"),
    );
    let stdout = session.0.stdout.take().expect("inspect stdout");
    let (mut prefix, mut node_ip) = (None, None);
    for line in BufReader::new(stdout).lines() {
        let line = line.expect("read inspect output");
        if let Some(value) = line.strip_prefix("export NETSIM_INSPECT=") {
            prefix = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("export NETSIM_IP_node=") {
            node_ip = Some(value.to_string());
        } else if line.starts_with("inspect active") {
            break;
        }
    }
    let prefix = prefix.expect("inspect exited before it was ready");
    let node_ip = node_ip.expect("inspect printed no ip for node");

    let output = Command::new(env!("CARGO_BIN_EXE_patchbay"))
        .args(["run-in", "node", "--inspect", &prefix, "--work-dir"])
        .arg(&work_dir)
        .args([
            "--",
            "sh",
            "-c",
            "id -u; ip -br addr show eth0; grep CapEff /proc/self/status",
        ])
        .output()
        .expect("run run-in");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "run-in failed:\n{stdout}{stderr}");

    let mut lines = stdout.lines();
    // The owner of /proc/self is the effective uid.
    let uid = std::fs::metadata("/proc/self")
        .expect("stat /proc/self")
        .uid();
    assert_eq!(lines.next(), Some(uid.to_string().as_str()), "{stdout}");
    assert!(
        lines.next().is_some_and(|line| line.contains(&node_ip)),
        "not in the node's namespace: {stdout}"
    );
    if nsenter_keeps_caps() {
        let eff = lines
            .next()
            .and_then(|line| line.strip_prefix("CapEff:"))
            .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
            .expect("CapEff line");
        const CAP_NET_ADMIN: u32 = 12;
        assert!(
            eff & (1 << CAP_NET_ADMIN) != 0,
            "no CAP_NET_ADMIN: {stdout}"
        );
    }
}
