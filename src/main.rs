//! Runs the `netsim` CLI entrypoint.

mod sim;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

use netsim::check_caps;
use netsim::serve::start_ui_server;

#[derive(Parser)]
#[command(name = "netsim", about = "Run a netsim simulation")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run one or more sims locally.
    Run {
        /// One or more sim TOML files or directories containing `*.toml`.
        #[arg(required = true)]
        sims: Vec<PathBuf>,

        /// Work directory for logs, binaries, and results.
        #[arg(long, default_value = ".netsim-work")]
        work_dir: PathBuf,

        /// Binary override in `<name>:<mode>:<value>` form.
        #[arg(long = "binary")]
        binary_overrides: Vec<String>,

        /// Start embedded UI server and open browser.
        #[arg(long, default_value_t = false)]
        open: bool,

        /// Bind address for embedded UI server.
        #[arg(long, default_value = "127.0.0.1:0")]
        bind: String,
    },
    /// Serve embedded UI + work directory over HTTP.
    Serve {
        /// Work directory containing run outputs.
        #[arg(long, default_value = ".netsim-work")]
        work_dir: PathBuf,
        /// Bind address for HTTP server.
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: String,
        /// Open browser after server start.
        #[arg(long, default_value_t = false)]
        open: bool,
    },
    /// Clean leaked labs by prefix.
    Cleanup {
        /// Resource name prefix to clean (repeatable).
        ///
        /// Defaults to `lab-p` and `br-p` when omitted.
        #[arg(long = "prefix")]
        prefixes: Vec<String>,
    },
    /// Build topology from sim/topology config for interactive namespace debugging.
    Inspect {
        /// Sim TOML or topology TOML file path.
        input: PathBuf,
        /// Work directory for inspect session metadata.
        #[arg(long, default_value = ".netsim-work")]
        work_dir: PathBuf,
    },
    /// Run a command inside a node namespace from an inspect session.
    RunIn {
        /// Device or router name from the inspected topology.
        node: String,
        /// Inspect session id (defaults to `$NETSIM_INSPECT`).
        #[arg(long)]
        inspect: Option<String>,
        /// Work directory containing inspect session metadata.
        #[arg(long, default_value = ".netsim-work")]
        work_dir: PathBuf,
        /// Command and args to execute in the node namespace.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
}

fn main() -> Result<()> {
    netsim::bootstrap_userns()?;
    tokio_main()
}

#[tokio::main(flavor = "current_thread")]
async fn tokio_main() -> Result<()> {
    netsim::Lab::init_tracing();
    let cli = Cli::parse();
    match cli.command {
        Command::Run {
            sims,
            work_dir,
            binary_overrides,
            open,
            bind,
        } => {
            check_caps()?;
            install_signal_cleanup_handler(vec![])?;
            let _server = if open {
                let srv = start_ui_server(work_dir.clone(), &bind)?;
                println!("netsim UI: {}", srv.url());
                srv.open_browser()?;
                Some(srv)
            } else {
                None
            };
            let res = sim::run_sims(sims, work_dir, binary_overrides).await;
            if open && res.is_ok() {
                println!("run finished; UI server still running (Ctrl-C to exit)");
                loop {
                    std::thread::sleep(Duration::from_secs(60));
                }
            }
            res
        }
        Command::Serve {
            work_dir,
            bind,
            open,
        } => {
            let _server = start_ui_server(work_dir, &bind)?;
            println!("netsim UI: {}", _server.url());
            if open {
                _server.open_browser()?;
            }
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
        Command::Cleanup { prefixes } => cleanup_command(prefixes),
        Command::Inspect { input, work_dir } => inspect_command(input, work_dir).await,
        Command::RunIn {
            node,
            inspect,
            work_dir,
            cmd,
        } => run_in_command(node, inspect, work_dir, cmd),
    }
}

fn default_cleanup_prefixes() -> Vec<String> {
    vec!["lab-p".to_string(), "br-p".to_string()]
}

fn cleanup_command(prefixes: Vec<String>) -> Result<()> {
    check_caps().context("cleanup requires rootless userns bootstrap and network privileges")?;
    let use_prefixes = if prefixes.is_empty() {
        default_cleanup_prefixes()
    } else {
        prefixes
    };
    perform_cleanup(&use_prefixes)
}

fn perform_cleanup(prefixes: &[String]) -> Result<()> {
    if prefixes.is_empty() {
        tracing::debug!("netsim cleanup: starting (prefixes: registered)");
    } else {
        tracing::debug!(
            "netsim cleanup: starting (prefixes: {})",
            prefixes.join(", ")
        );
    }
    let resources = netsim::core::resources();
    if !prefixes.is_empty() {
        for prefix in prefixes {
            resources.cleanup_everything_with_prefix(prefix);
        }
    } else {
        resources.cleanup_all();
    }
    if !prefixes.is_empty() {
        resources.cleanup_everything();
    }
    tracing::info!("netsim cleanup: complete");
    Ok(())
}

fn install_signal_cleanup_handler(prefixes: Vec<String>) -> Result<()> {
    ctrlc::set_handler(move || {
        tracing::debug!("netsim: received interrupt, running cleanup");
        let _ = perform_cleanup(&prefixes);
        // SAFETY: immediate process termination after best-effort cleanup in signal path.
        unsafe { nix::libc::_exit(130) };
    })
    .context("install Ctrl-C cleanup handler")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InspectSession {
    prefix: String,
    root_ns: String,
    node_namespaces: HashMap<String, String>,
    node_ips_v4: HashMap<String, String>,
    node_keeper_pids: HashMap<String, u32>,
}

fn inspect_dir(work_dir: &std::path::Path) -> PathBuf {
    work_dir.join("inspect")
}

fn inspect_session_path(work_dir: &std::path::Path, prefix: &str) -> PathBuf {
    inspect_dir(work_dir).join(format!("{prefix}.json"))
}

fn env_key_suffix(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn load_topology_for_inspect(input: &std::path::Path) -> Result<(netsim::config::LabConfig, bool)> {
    let text =
        std::fs::read_to_string(input).with_context(|| format!("read {}", input.display()))?;
    let value: toml::Value =
        toml::from_str(&text).with_context(|| format!("parse TOML {}", input.display()))?;
    let is_sim =
        value.get("sim").is_some() || value.get("step").is_some() || value.get("binary").is_some();
    if is_sim {
        let sim: sim::SimFile =
            toml::from_str(&text).with_context(|| format!("parse sim {}", input.display()))?;
        let topo = sim::topology::load_topology(&sim, input)
            .with_context(|| format!("load topology from sim {}", input.display()))?;
        Ok((topo, true))
    } else {
        let topo: netsim::config::LabConfig =
            toml::from_str(&text).with_context(|| format!("parse topology {}", input.display()))?;
        Ok((topo, false))
    }
}

fn spawn_keeper_in_namespace(ns: &str) -> Result<u32> {
    let mut cmd = ProcessCommand::new("sh");
    cmd.args(["-lc", "while :; do sleep 3600; done"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let child = netsim::core::spawn_command_in_namespace(ns, cmd)
        .with_context(|| format!("spawn namespace keeper in '{ns}'"))?;
    Ok(child.id())
}

async fn inspect_command(input: PathBuf, work_dir: PathBuf) -> Result<()> {
    check_caps()?;
    install_signal_cleanup_handler(vec![])?;

    let (topo, is_sim) = load_topology_for_inspect(&input)?;
    let mut lab = netsim::Lab::from_config(topo.clone())
        .with_context(|| format!("build lab config from {}", input.display()))?;
    lab.build().await.context("build inspected topology")?;

    let mut node_namespaces = HashMap::new();
    let mut node_ips_v4 = HashMap::new();
    let mut node_keeper_pids = HashMap::new();

    for router in &topo.router {
        let name = router.name.clone();
        let ns = lab
            .router_ns_name(&name)
            .with_context(|| format!("resolve router namespace for '{name}'"))?;
        node_keeper_pids.insert(name.clone(), spawn_keeper_in_namespace(&ns)?);
        node_namespaces.insert(name.clone(), ns);
        if let Some(id) = lab.router_id(&name) {
            node_ips_v4.insert(name, lab.router_uplink_ip(id)?.to_string());
        }
    }
    for name in topo.device.keys() {
        let ns = lab
            .device_ns_name(name)
            .with_context(|| format!("resolve device namespace for '{name}'"))?;
        node_keeper_pids.insert(name.clone(), spawn_keeper_in_namespace(&ns)?);
        node_namespaces.insert(name.clone(), ns);
        if let Some(id) = lab.device_id(name) {
            node_ips_v4.insert(name.clone(), lab.device_ip(id)?.to_string());
        }
    }

    let prefix = lab.prefix().to_string();
    let session = InspectSession {
        prefix: prefix.clone(),
        root_ns: lab.root_namespace_name().to_string(),
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

    let mut keys = session
        .node_namespaces
        .keys()
        .map(|k| k.to_string())
        .collect::<Vec<_>>();
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
        if let Some(ns) = session.node_namespaces.get(key) {
            println!("export NETSIM_NS_{}={ns}", env_key_suffix(key));
        }
        if let Some(ip) = session.node_ips_v4.get(key) {
            println!("export NETSIM_IP_{}={ip}", env_key_suffix(key));
        }
    }
    println!("cleanup: netsim cleanup --prefix {}", session.prefix);
    println!("inspect active; press Ctrl-C to stop and clean up");
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

fn resolve_inspect_ref(inspect: Option<String>) -> Result<String> {
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

fn load_inspect_session(work_dir: &std::path::Path, inspect_ref: &str) -> Result<InspectSession> {
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

fn run_in_command(
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
    use std::path::Path;

    #[test]
    fn env_key_suffix_normalizes_names() {
        assert_eq!(env_key_suffix("relay"), "relay");
        assert_eq!(env_key_suffix("fetcher-1"), "fetcher_1");
    }

    #[test]
    fn inspect_session_path_uses_prefix_json() {
        let base = PathBuf::from("/tmp/netsim-work");
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
            "netsim-inspect-test-{}-{}",
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
            "netsim-inspect-test-{}-{}",
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
}
