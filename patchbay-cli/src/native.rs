//! Native Linux backend: sim execution and interactive namespace inspection.
//!
//! Everything in this module requires Linux user namespaces (patchbay core).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use patchbay::check_caps;
use patchbay_runner::sim;
use serde::{Deserialize, Serialize};

/// Bootstrap user namespaces before main() — required for test binaries
/// where main() is not our code (nextest spawns each test as a process).
#[ctor::ctor(unsafe)]
fn _init_userns() {
    // Safety: called from .init_array before main() and before any threads.
    unsafe { patchbay::init_userns_for_ctor() };
}

/// Initialize user namespaces (called from main() as well for the CLI binary).
pub fn init() -> Result<()> {
    patchbay::init_userns()
}

/// Run one or more sims locally.
pub async fn run_sims(
    sims: Vec<PathBuf>,
    work_dir: PathBuf,
    binary_overrides: Vec<String>,
    verbose: bool,
    project_root: Option<PathBuf>,
    no_build: bool,
    timeout: Option<Duration>,
) -> Result<()> {
    sim::run_sims(
        sims,
        work_dir,
        binary_overrides,
        verbose,
        project_root,
        no_build,
        timeout,
    )
    .await
}

/// Resolve sims and build all required assets without running.
pub async fn prepare_sims(
    sims: Vec<PathBuf>,
    work_dir: PathBuf,
    binary_overrides: Vec<String>,
    project_root: Option<PathBuf>,
    no_build: bool,
) -> Result<()> {
    sim::prepare_sims(sims, work_dir, binary_overrides, project_root, no_build).await
}

/// Parse a duration string like "120s" or "5m".
pub fn parse_duration(s: &str) -> Result<Duration> {
    sim::steps::parse_duration(s)
}

// ── Inspect / RunIn ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectSession {
    pub prefix: String,
    pub root_ns: String,
    pub node_namespaces: HashMap<String, String>,
    pub node_ips_v4: HashMap<String, String>,
    pub node_keeper_pids: HashMap<String, u32>,
}

pub fn inspect_dir(work_dir: &Path) -> PathBuf {
    work_dir.join("inspect")
}

pub fn inspect_session_path(work_dir: &Path, prefix: &str) -> PathBuf {
    inspect_dir(work_dir).join(format!("{prefix}.json"))
}

pub fn env_key_suffix(name: &str) -> String {
    patchbay::util::sanitize_for_env_key(name)
}

pub fn load_topology_for_inspect(input: &Path) -> Result<(patchbay::config::LabConfig, bool)> {
    let text =
        std::fs::read_to_string(input).with_context(|| format!("read {}", input.display()))?;
    let value: toml::Value =
        toml::from_str(&text).with_context(|| format!("parse TOML {}", input.display()))?;
    let is_sim =
        value.get("sim").is_some() || value.get("step").is_some() || value.get("binary").is_some();
    if is_sim {
        let sim_file: sim::SimFile =
            toml::from_str(&text).with_context(|| format!("parse sim {}", input.display()))?;
        let topo = sim::topology::load_topology(&sim_file, input)
            .with_context(|| format!("load topology from sim {}", input.display()))?;
        Ok((topo, true))
    } else {
        let topo: patchbay::config::LabConfig =
            toml::from_str(&text).with_context(|| format!("parse topology {}", input.display()))?;
        Ok((topo, false))
    }
}

fn keeper_command() -> ProcessCommand {
    let mut cmd = ProcessCommand::new("sh");
    cmd.args(["-lc", "while :; do sleep 3600; done"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd
}

pub async fn inspect_command(input: PathBuf, work_dir: PathBuf) -> Result<()> {
    check_caps()?;

    let (topo, is_sim) = load_topology_for_inspect(&input)?;
    let lab = patchbay_runner::Lab::builder()
        .config(topo.clone())
        .build()
        .await
        .with_context(|| format!("build lab config from {}", input.display()))?;

    let mut node_namespaces = HashMap::new();
    let mut node_ips_v4 = HashMap::new();
    let mut node_keeper_pids = HashMap::new();

    for router in &topo.router {
        let name = router.name.clone();
        let r = lab
            .router_by_name(&name)
            .with_context(|| format!("unknown router '{name}'"))?;
        let child = r.spawn_command_sync(keeper_command())?;
        node_keeper_pids.insert(name.clone(), child.id());
        node_namespaces.insert(name.clone(), r.ns().to_string());
        if let Some(ip) = r.uplink_ip() {
            node_ips_v4.insert(name, ip.to_string());
        }
    }
    for name in topo.device.keys() {
        let d = lab
            .device_by_name(name)
            .with_context(|| format!("unknown device '{name}'"))?;
        let child = d.spawn_command_sync(keeper_command())?;
        node_keeper_pids.insert(name.clone(), child.id());
        node_namespaces.insert(name.clone(), d.ns().to_string());
        if let Some(ip) = d.ip() {
            node_ips_v4.insert(name.clone(), ip.to_string());
        }
    }

    let prefix = lab.prefix().to_string();
    let session = InspectSession {
        prefix: prefix.clone(),
        root_ns: lab.ix().ns(),
        node_namespaces,
        node_ips_v4,
        node_keeper_pids,
    };

    let session_dir = inspect_dir(&work_dir);
    std::fs::create_dir_all(&session_dir)
        .with_context(|| format!("create {}", session_dir.display()))?;
    let session_path = inspect_session_path(&work_dir, &prefix);
    std::fs::write(&session_path, serde_json::to_vec_pretty(&session)?)
        .with_context(|| format!("write {}", session_path.display()))?;

    let mut keys: Vec<_> = session.node_namespaces.keys().map(String::as_str).collect();
    keys.sort();

    println!(
        "inspect ready: {} ({})",
        session.prefix,
        if is_sim { "sim" } else { "topology" }
    );
    println!("session file: {}", session_path.display());
    println!("export NETSIM_INSPECT={}", session.prefix);
    println!("export NETSIM_INSPECT_FILE={}", session_path.display());
    for key in &keys {
        if let Some(ns) = session.node_namespaces.get(*key) {
            println!("export NETSIM_NS_{}={ns}", env_key_suffix(key));
        }
        if let Some(ip) = session.node_ips_v4.get(*key) {
            println!("export NETSIM_IP_{}={ip}", env_key_suffix(key));
        }
    }
    println!("inspect active; press Ctrl-C to stop and clean up");
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

pub fn resolve_inspect_ref(inspect: Option<String>) -> Result<String> {
    if let Some(value) = inspect {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            bail!("--inspect must not be empty");
        }
        return Ok(trimmed.to_string());
    }
    let from_env = std::env::var("NETSIM_INSPECT")
        .context("missing inspect session; set --inspect or NETSIM_INSPECT")?;
    let trimmed = from_env.trim();
    if trimmed.is_empty() {
        bail!("NETSIM_INSPECT is set but empty");
    }
    Ok(trimmed.to_string())
}

pub fn load_inspect_session(work_dir: &Path, inspect_ref: &str) -> Result<InspectSession> {
    let as_path = PathBuf::from(inspect_ref);
    let session_path = if as_path.extension().and_then(|v| v.to_str()) == Some("json")
        || inspect_ref.contains('/')
    {
        as_path
    } else {
        inspect_session_path(work_dir, inspect_ref)
    };
    let bytes = std::fs::read(&session_path)
        .with_context(|| format!("read inspect session {}", session_path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parse inspect session {}", session_path.display()))
}

pub fn run_in_command(
    node: String,
    inspect: Option<String>,
    work_dir: PathBuf,
    cmd: Vec<String>,
) -> Result<()> {
    check_caps()?;
    if cmd.is_empty() {
        bail!("run-in: missing command");
    }
    let inspect_ref = resolve_inspect_ref(inspect)?;
    let session = load_inspect_session(&work_dir, &inspect_ref)?;
    let pid = *session.node_keeper_pids.get(&node).ok_or_else(|| {
        anyhow!(
            "node '{}' is not in inspect session '{}'",
            node,
            session.prefix
        )
    })?;

    let mut proc = ProcessCommand::new("nsenter");
    proc.arg("-U")
        .arg("-t")
        .arg(pid.to_string())
        .arg("-n")
        .arg("--")
        .arg(&cmd[0]);
    if cmd.len() > 1 {
        proc.args(&cmd[1..]);
    }
    let status = proc
        .status()
        .context("run command with nsenter for inspect session")?;
    if !status.success() {
        bail!("run-in command exited with status {}", status);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_key_suffix_normalizes_names() {
        assert_eq!(env_key_suffix("relay"), "relay");
        assert_eq!(env_key_suffix("fetcher-1"), "fetcher_1");
    }

    #[test]
    fn inspect_session_path_uses_prefix_json() {
        let base = PathBuf::from("/tmp/patchbay-work");
        let path = inspect_session_path(&base, "lab-p123");
        assert!(path.ends_with("inspect/lab-p123.json"));
    }

    fn write_temp_file(dir: &Path, rel: &str, body: &str) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&path, body).expect("write file");
        path
    }

    #[test]
    fn inspect_loader_detects_sim_input() {
        let root = std::env::temp_dir().join(format!(
            "patchbay-inspect-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let sim_path = write_temp_file(
            &root,
            "sims/case.toml",
            "[sim]\nname='x'\n\n[[router]]\nname='relay'\n\n[device.fetcher.eth0]\ngateway='relay'\n",
        );
        let (_topo, is_sim) = load_topology_for_inspect(&sim_path).expect("load sim topology");
        assert!(is_sim);
    }

    #[test]
    fn inspect_loader_detects_topology_input() {
        let root = std::env::temp_dir().join(format!(
            "patchbay-inspect-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let topo_path = write_temp_file(
            &root,
            "topos/lab.toml",
            "[[router]]\nname='relay'\n\n[device.fetcher.eth0]\ngateway='relay'\n",
        );
        let (_topo, is_sim) = load_topology_for_inspect(&topo_path).expect("load direct topology");
        assert!(!is_sim);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn iperf_sim_writes_results_with_mbps() {
        let root = std::env::temp_dir().join(format!(
            "patchbay-iperf-run-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("create temp workdir");
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let sim_path = workspace_root.join("patchbay-cli/tests/fixtures/iperf-1to1-public.toml");
        run_sims(
            vec![sim_path],
            root.clone(),
            vec![],
            false,
            Some(workspace_root),
            false,
            None,
        )
        .await
        .expect("run iperf sim");

        let run_root = std::fs::canonicalize(root.join("latest")).expect("resolve latest");
        let results_path = run_root
            .join("iperf-1to1-public-baseline")
            .join("results.json");
        let text = std::fs::read_to_string(&results_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", results_path.display()));
        let json: serde_json::Value = serde_json::from_str(&text).expect("parse results");
        let step = &json["steps"][0];
        let down_bytes: f64 = step["down_bytes"]
            .as_str()
            .expect("down_bytes should be present")
            .parse()
            .expect("down_bytes should be numeric");
        let duration: f64 = step["duration"]
            .as_str()
            .expect("duration should be present")
            .parse::<u64>()
            .map(|us| us as f64 / 1_000_000.0)
            .unwrap_or_else(|_| {
                step["duration"]
                    .as_str()
                    .unwrap()
                    .parse::<f64>()
                    .expect("duration as float")
            });
        let mb_s = down_bytes / (duration * 1_000_000.0);
        assert!(mb_s > 0.0, "expected mb_s > 0, got {mb_s}");
    }
}
