use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

const CAPS: &str = "cap_net_admin,cap_sys_admin,cap_net_raw+ep";
const TOOLS: &[&str] = &["ip", "tc", "nft", "ping", "ping6"];

pub fn setup_caps_for_self_and_tools() -> Result<()> {
    if cfg!(not(target_os = "linux")) {
        bail!("setup-caps is only supported on Linux");
    }
    need_cmd("setcap")?;
    need_cmd("getcap")?;
    let use_sudo = nix::unistd::geteuid().as_raw() != 0;
    if use_sudo {
        need_cmd("sudo")?;
        run_checked(Command::new("sudo").arg("-v"), "sudo -v")?;
    }

    let self_exe = std::env::current_exe().context("resolve current executable path")?;
    eprintln!("setup-caps: applying '{CAPS}'");
    eprintln!("  self: {}", self_exe.display());

    setcap_path(&self_exe, use_sudo)?;
    show_cap(&self_exe, use_sudo)?;

    for tool in TOOLS {
        if let Some(path) = lookup_tool(tool, use_sudo)? {
            eprintln!("  tool: {}", path.display());
            setcap_path(&path, use_sudo)?;
            show_cap(&path, use_sudo)?;
        } else {
            eprintln!("  skip (not found): {tool}");
        }
    }

    eprintln!("setup-caps: done");
    Ok(())
}

fn need_cmd(name: &str) -> Result<()> {
    let status = Command::new("sh")
        .arg("-lc")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .status()
        .with_context(|| format!("check command '{name}'"))?;
    if status.success() {
        Ok(())
    } else {
        bail!("missing required command: {name}")
    }
}

fn lookup_tool(name: &str, use_sudo: bool) -> Result<Option<PathBuf>> {
    let lookup_path = format!(
        "/usr/sbin:/sbin:/usr/bin:/bin:{}",
        std::env::var("PATH").unwrap_or_default()
    );
    let mut cmd = if use_sudo {
        let mut c = Command::new("sudo");
        c.arg("env").arg(format!("PATH={lookup_path}")).arg("which");
        c
    } else {
        let mut c = Command::new("env");
        c.arg(format!("PATH={lookup_path}")).arg("which");
        c
    };
    let out = cmd.arg(name).output().context("run which")?;
    if !out.status.success() {
        return Ok(None);
    }
    let path = String::from_utf8(out.stdout).context("parse which output")?;
    let path = path.trim();
    if path.is_empty() {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(path)))
}

fn setcap_path(path: &Path, use_sudo: bool) -> Result<()> {
    let mut cmd = if use_sudo {
        let mut c = Command::new("sudo");
        c.arg("setcap");
        c
    } else {
        Command::new("setcap")
    };
    cmd.arg(CAPS).arg(path);
    run_checked(&mut cmd, &format!("setcap {}", path.display()))
}

fn show_cap(path: &Path, use_sudo: bool) -> Result<()> {
    let mut cmd = if use_sudo {
        let mut c = Command::new("sudo");
        c.arg("getcap");
        c
    } else {
        Command::new("getcap")
    };
    cmd.arg(path);
    run_checked(&mut cmd, &format!("getcap {}", path.display()))
}

fn run_checked(cmd: &mut Command, label: &str) -> Result<()> {
    let status = cmd.status().with_context(|| format!("run '{label}'"))?;
    if status.success() {
        Ok(())
    } else {
        bail!("command failed: {label} (status {status})")
    }
}
