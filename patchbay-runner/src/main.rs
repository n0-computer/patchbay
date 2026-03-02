//! Runs the `patchbay` CLI entrypoint.

mod sim;

use std::{
    collections::HashMap, path::PathBuf, process::Command as ProcessCommand, time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use patchbay::check_caps;
use patchbay_utils::ui::{start_ui_server, DEFAULT_UI_BIND};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Parser)]
#[command(name = "patchbay", about = "Run a patchbay simulation")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run one or more sims locally.
    Run {
        /// One or more sim TOML files or directories containing `*.toml`.
        #[arg()]
        sims: Vec<PathBuf>,

        /// Clone and run from the repo's `patchbay.toml` project config.
        #[arg(long)]
        repo: Option<String>,

        /// Repo ref (branch/tag/commit) used with `--repo`.
        #[arg(long)]
        r#ref: Option<String>,

        /// Clone repo from `patchbay.toml` and run sims from the clone root.
        #[arg(long, default_value_t = false)]
        clone: bool,

        /// Work directory for logs, binaries, and results.
        #[arg(long, default_value = ".patchbay-work")]
        work_dir: PathBuf,

        /// Binary override in `<name>:<mode>:<value>` form.
        #[arg(long = "binary")]
        binary_overrides: Vec<String>,

        /// Do not build binaries; resolve expected artifacts from target dirs.
        #[arg(long, default_value_t = false)]
        no_build: bool,
        /// Stream live stdout/stderr lines with node prefixes.
        #[arg(short = 'v', long, default_value_t = false)]
        verbose: bool,

        /// Start embedded UI server and open browser.
        #[arg(long, default_value_t = false)]
        open: bool,

        /// Bind address for embedded UI server.
        #[arg(long, default_value = DEFAULT_UI_BIND)]
        bind: String,
    },
    /// Resolve sims and build all required assets without running simulations.
    Prepare {
        /// One or more sim TOML files or directories containing `*.toml`.
        #[arg()]
        sims: Vec<PathBuf>,
        /// Clone and run from the repo's `patchbay.toml` project config.
        #[arg(long)]
        repo: Option<String>,
        /// Repo ref (branch/tag/commit) used with `--repo`.
        #[arg(long)]
        r#ref: Option<String>,
        /// Clone repo from `patchbay.toml` and run sims from the clone root.
        #[arg(long, default_value_t = false)]
        clone: bool,
        /// Work directory for caches and prepared outputs.
        #[arg(long, default_value = ".patchbay-work")]
        work_dir: PathBuf,
        /// Binary override in `<name>:<mode>:<value>` form.
        #[arg(long = "binary")]
        binary_overrides: Vec<String>,
        /// Do not build binaries; resolve expected artifacts from target dirs.
        #[arg(long, default_value_t = false)]
        no_build: bool,
    },
    /// Serve embedded UI + work directory over HTTP.
    Serve {
        /// Work directory containing run outputs.
        #[arg(long, default_value = ".patchbay-work")]
        work_dir: PathBuf,
        /// Bind address for HTTP server.
        #[arg(long, default_value = DEFAULT_UI_BIND)]
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
        #[arg(long, default_value = ".patchbay-work")]
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
        #[arg(long, default_value = ".patchbay-work")]
        work_dir: PathBuf,
        /// Command and args to execute in the node namespace.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
}

fn main() -> Result<()> {
    patchbay::init_userns()?;
    tokio_main()
}

#[tokio::main(flavor = "current_thread")]
async fn tokio_main() -> Result<()> {
    patchbay_utils::init_tracing();
    let cli = Cli::parse();
    match cli.command {
        Command::Run {
            sims,
            repo,
            r#ref,
            clone,
            work_dir,
            binary_overrides,
            no_build,
            verbose,
            open,
            bind,
        } => {
            check_caps()?;
            install_signal_cleanup_handler(vec![])?;
            let _server = if open {
                let srv = start_ui_server(work_dir.clone(), &bind)?;
                println!("patchbay UI: {}", srv.url());
                srv.open_browser()?;
                Some(srv)
            } else {
                None
            };
            let run_spec = resolve_run_spec(sims, repo, r#ref, clone, &work_dir)?;
            let res = sim::run_sims(
                run_spec.sims,
                work_dir,
                binary_overrides,
                verbose,
                Some(run_spec.project_root),
                no_build,
            )
            .await;
            if open && res.is_ok() {
                println!("run finished; UI server still running (Ctrl-C to exit)");
                loop {
                    std::thread::sleep(Duration::from_secs(60));
                }
            }
            res
        }
        Command::Prepare {
            sims,
            repo,
            r#ref,
            clone,
            work_dir,
            binary_overrides,
            no_build,
        } => {
            let run_spec = resolve_run_spec(sims, repo, r#ref, clone, &work_dir)?;
            sim::prepare_sims(
                run_spec.sims,
                work_dir,
                binary_overrides,
                Some(run_spec.project_root),
                no_build,
            )
            .await
        }
        Command::Serve {
            work_dir,
            bind,
            open,
        } => {
            let _server = start_ui_server(work_dir, &bind)?;
            println!("patchbay UI: {}", _server.url());
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

#[derive(Debug)]
struct RunSpec {
    sims: Vec<PathBuf>,
    project_root: PathBuf,
}

#[derive(Debug, Deserialize)]
struct PatchbayProjectConfig {
    repo: Option<String>,
    simulations: String,
}

fn resolve_run_spec(
    sims: Vec<PathBuf>,
    repo: Option<String>,
    repo_ref: Option<String>,
    clone_mode: bool,
    work_dir: &std::path::Path,
) -> Result<RunSpec> {
    if clone_mode {
        if repo.is_some() {
            bail!("--clone cannot be combined with --repo");
        }
        let config_path = if sims.is_empty() {
            let cwd = std::env::current_dir().context("resolve current directory")?;
            find_patchbay_toml(&cwd).ok_or_else(|| {
                anyhow!(
                    "no patchbay.toml found from {} upward (required by --clone)",
                    cwd.display()
                )
            })?
        } else {
            if sims.len() != 1 {
                bail!("--clone accepts at most one path argument");
            }
            find_patchbay_toml(&sims[0]).ok_or_else(|| {
                anyhow!(
                    "no patchbay.toml found from {} upward (required by --clone)",
                    sims[0].display()
                )
            })?
        };
        let cfg = load_patchbay_config(&config_path)?;
        let repo_url = cfg.repo.clone().ok_or_else(|| {
            anyhow!(
                "patchbay.toml at {} is missing 'repo' (required by --clone)",
                config_path.display()
            )
        })?;
        let checkout = clone_or_update_repo(work_dir, &repo_url, repo_ref.as_deref())?;
        let local_root = config_path
            .parent()
            .ok_or_else(|| anyhow!("config has no parent: {}", config_path.display()))?;
        let mut spec = run_spec_from_config(&cfg, local_root)?;
        spec.project_root = checkout;
        return Ok(spec);
    }

    if repo.is_none() && repo_ref.is_some() {
        bail!("--ref requires --repo (or use --clone with patchbay.toml repo)");
    }

    if !sims.is_empty() {
        if repo.is_some() || repo_ref.is_some() {
            bail!("--repo/--ref cannot be combined with explicit sim paths");
        }
        return Ok(RunSpec {
            sims,
            project_root: std::env::current_dir().context("resolve current directory")?,
        });
    }

    if let Some(repo_url) = repo {
        let checkout = clone_or_update_repo(work_dir, &repo_url, repo_ref.as_deref())?;
        let config_path = find_patchbay_toml(&checkout).ok_or_else(|| {
            anyhow!(
                "no patchbay.toml found under cloned repo root {}",
                checkout.display()
            )
        })?;
        let cfg = load_patchbay_config(&config_path)?;
        return run_spec_from_config(&cfg, &checkout);
    }

    let cwd = std::env::current_dir().context("resolve current directory")?;
    let config_path = find_patchbay_toml(&cwd).ok_or_else(|| {
        anyhow!(
            "no sims provided and no patchbay.toml found from {} upward",
            cwd.display()
        )
    })?;
    let cfg = load_patchbay_config(&config_path)?;
    let root = config_path
        .parent()
        .ok_or_else(|| anyhow!("config has no parent: {}", config_path.display()))?;
    run_spec_from_config(&cfg, root)
}

fn load_patchbay_config(config_path: &std::path::Path) -> Result<PatchbayProjectConfig> {
    let text = std::fs::read_to_string(config_path)
        .with_context(|| format!("read {}", config_path.display()))?;
    let cfg: PatchbayProjectConfig =
        toml::from_str(&text).with_context(|| format!("parse {}", config_path.display()))?;
    Ok(cfg)
}

fn run_spec_from_config(
    cfg: &PatchbayProjectConfig,
    sim_root: &std::path::Path,
) -> Result<RunSpec> {
    let sims = expand_sim_glob(sim_root, &cfg.simulations)?;
    if sims.is_empty() {
        bail!(
            "simulations glob '{}' matched no .toml files from {}",
            cfg.simulations,
            sim_root.display()
        );
    }
    Ok(RunSpec {
        sims,
        project_root: sim_root.to_path_buf(),
    })
}

fn expand_sim_glob(root: &std::path::Path, pattern: &str) -> Result<Vec<PathBuf>> {
    let abs_pattern = root.join(pattern);
    let abs_pattern = abs_pattern.to_string_lossy().into_owned();
    let mut sims = Vec::new();
    let options = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };
    for entry in glob::glob_with(&abs_pattern, options)
        .with_context(|| format!("invalid simulations glob '{}'", pattern))?
    {
        let path = entry.with_context(|| format!("expand simulations glob '{}'", pattern))?;
        if path.is_dir() {
            collect_toml_files_recursive(&path, &mut sims)?;
        } else if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("toml") {
            sims.push(path);
        }
    }
    if pattern.ends_with("**") {
        let base_rel = pattern
            .trim_end_matches("**")
            .trim_end_matches(std::path::MAIN_SEPARATOR);
        let base_dir = root.join(base_rel);
        if base_dir.is_dir() {
            collect_toml_files_recursive(&base_dir, &mut sims)?;
        }
    }
    sims.sort();
    sims.dedup();
    Ok(sims)
}

fn collect_toml_files_recursive(dir: &std::path::Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("read simulation dir {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_toml_files_recursive(&path, out)?;
        } else if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("toml") {
            out.push(path);
        }
    }
    Ok(())
}

fn find_patchbay_toml(start: &std::path::Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    if dir.is_file() {
        dir = dir.parent()?.to_path_buf();
    }
    loop {
        let candidate = dir.join("patchbay.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn clone_or_update_repo(
    work_dir: &std::path::Path,
    repo_url: &str,
    repo_ref: Option<&str>,
) -> Result<PathBuf> {
    let clones_root = work_dir.join("clones");
    std::fs::create_dir_all(&clones_root)
        .with_context(|| format!("create {}", clones_root.display()))?;
    let checkout = clones_root.join(repo_checkout_dir_name(repo_url));
    if !checkout.join(".git").is_dir() {
        let status = ProcessCommand::new("git")
            .args(["clone", repo_url])
            .arg(&checkout)
            .status()
            .context("spawn git clone")?;
        if !status.success() {
            bail!("git clone failed for {}", repo_url);
        }
    }

    run_git(&checkout, &["fetch", "--all", "--prune"], "git fetch")?;
    if let Some(r) = repo_ref {
        checkout_ref(&checkout, r)?;
    } else {
        checkout_default_remote_head(&checkout)?;
    }
    Ok(checkout)
}

fn repo_checkout_dir_name(repo_url: &str) -> String {
    let base = repo_url
        .rsplit('/')
        .next()
        .unwrap_or("repo")
        .trim_end_matches(".git");
    let mut hasher = Sha256::new();
    hasher.update(repo_url.as_bytes());
    let digest = hasher.finalize();
    let hash = hex_prefix(&digest, 10);
    format!("{}-{}", sanitize_repo_component(base), hash)
}

fn sanitize_repo_component(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    if out.is_empty() {
        "repo".to_string()
    } else {
        out
    }
}

fn hex_prefix(bytes: &[u8], n: usize) -> String {
    let mut out = String::with_capacity(n);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
        if out.len() >= n {
            out.truncate(n);
            break;
        }
    }
    out
}

fn checkout_ref(repo_dir: &std::path::Path, repo_ref: &str) -> Result<()> {
    let remote_ref = format!("refs/remotes/origin/{repo_ref}");
    let status = ProcessCommand::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["show-ref", "--verify", "--quiet", &remote_ref])
        .status()
        .context("git show-ref")?;
    if status.success() {
        run_git(
            repo_dir,
            &["checkout", "-B", repo_ref, &remote_ref],
            "git checkout remote branch",
        )
    } else {
        run_git(repo_dir, &["checkout", repo_ref], "git checkout ref")
    }
}

fn checkout_default_remote_head(repo_dir: &std::path::Path) -> Result<()> {
    let out = ProcessCommand::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
        .output()
        .context("git symbolic-ref origin/HEAD")?;
    if !out.status.success() {
        bail!("unable to resolve origin/HEAD in {}", repo_dir.display());
    }
    let remote = String::from_utf8(out.stdout)
        .context("parse git symbolic-ref output")?
        .trim()
        .to_string();
    let branch = remote
        .strip_prefix("refs/remotes/origin/")
        .ok_or_else(|| anyhow!("unexpected origin HEAD ref '{}'", remote))?;
    run_git(
        repo_dir,
        &["checkout", "-B", branch, &remote],
        "git checkout default branch",
    )
}

fn run_git(repo_dir: &std::path::Path, args: &[&str], op: &str) -> Result<()> {
    let status = ProcessCommand::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(args)
        .status()
        .with_context(|| format!("spawn {op}"))?;
    if !status.success() {
        bail!("{op} failed in {}", repo_dir.display());
    }
    Ok(())
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

fn perform_cleanup(_prefixes: &[String]) -> Result<()> {
    tracing::info!(
        "patchbay cleanup: fd-based namespaces are automatically cleaned up on drop; nothing to do"
    );
    Ok(())
}

fn install_signal_cleanup_handler(prefixes: Vec<String>) -> Result<()> {
    ctrlc::set_handler(move || {
        tracing::debug!("patchbay: received interrupt, running cleanup");
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
    patchbay::util::sanitize_for_env_key(name)
}

fn load_topology_for_inspect(
    input: &std::path::Path,
) -> Result<(patchbay::config::LabConfig, bool)> {
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
        let topo: patchbay::config::LabConfig =
            toml::from_str(&text).with_context(|| format!("parse topology {}", input.display()))?;
        Ok((topo, false))
    }
}

fn keeper_commmand() -> ProcessCommand {
    let mut cmd = ProcessCommand::new("sh");
    cmd.args(["-lc", "while :; do sleep 3600; done"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd
}

async fn inspect_command(input: PathBuf, work_dir: PathBuf) -> Result<()> {
    check_caps()?;
    install_signal_cleanup_handler(vec![])?;

    let (topo, is_sim) = load_topology_for_inspect(&input)?;
    let lab = patchbay_runner::Lab::from_config(topo.clone())
        .await
        .with_context(|| format!("build lab config from {}", input.display()))?;

    let mut node_namespaces = HashMap::new();
    let mut node_ips_v4 = HashMap::new();
    let mut node_keeper_pids = HashMap::new();

    for router in &topo.router {
        let name = router.name.clone();
        let r = lab
            .router_by_name(&name)
            .with_context(|| "unknown router '{name}'")?;
        let child = r.spawn_command_sync(keeper_commmand())?;
        node_keeper_pids.insert(name.clone(), child.id());
        node_namespaces.insert(name.clone(), r.ns().to_string());
        if let Some(ip) = r.uplink_ip() {
            node_ips_v4.insert(name, ip.to_string());
        }
    }
    for name in topo.device.keys() {
        let d = lab
            .device_by_name(name)
            .with_context(|| "unknown device '{name}'")?;
        let child = d.spawn_command_sync(keeper_commmand())?;
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
    println!("cleanup: patchbay cleanup --prefix {}", session.prefix);
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
    use std::path::Path;

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

    #[test]
    fn find_patchbay_toml_walks_parents() {
        let root = std::env::temp_dir().join(format!(
            "patchbay-config-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let config = write_temp_file(
            &root,
            "proj/patchbay.toml",
            "repo = \"https://example.com/repo.git\"\nsimulations = \"patchbay/sims/**\"\n",
        );
        let nested = root.join("proj").join("a").join("b");
        std::fs::create_dir_all(&nested).expect("create nested");
        let found = find_patchbay_toml(&nested).expect("find config");
        assert_eq!(found, config);
    }

    #[test]
    fn expand_sim_glob_ignores_non_toml() {
        let root = std::env::temp_dir().join(format!(
            "patchbay-config-glob-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        write_temp_file(&root, "proj/patchbay/sims/a.toml", "x");
        write_temp_file(&root, "proj/patchbay/sims/b.txt", "x");
        let sims = expand_sim_glob(&root.join("proj"), "patchbay/sims/**").expect("expand glob");
        assert_eq!(sims.len(), 1);
        assert_eq!(sims[0].file_name().and_then(|s| s.to_str()), Some("a.toml"));
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
        let sim_path = workspace_root.join("iroh-integration/patchbay/sims/iperf-1to1-public.toml");
        let project_root = workspace_root;
        sim::run_sims(
            vec![sim_path],
            root.clone(),
            vec![],
            false,
            Some(project_root),
            false,
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
