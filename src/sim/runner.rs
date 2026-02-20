use anyhow::{anyhow, bail, Context, Result};
use netsim::assets::{
    parse_binary_overrides, resolve_binary_source_path, BinaryOverride, PathResolveMode,
};
use std::collections::{BTreeSet, HashMap};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use netsim::config::LabConfig;
use netsim::{Impair, Lab};
use serde::Serialize;

use crate::sim::build::build_local_binary;
use crate::sim::build::build_or_fetch_binary;
use crate::sim::env::SimEnv;
use crate::sim::report::{
    parse_iperf3_json_log, print_run_summary_table_for_runs, write_combined_results_for_runs,
    write_results, IperfResult, TransferResult,
};
use crate::sim::topology::load_topology;
use crate::sim::transfer::{finish_transfer, start_transfer, TransferHandle};
use crate::sim::{BinarySpec, SimFile, Step};

// ─────────────────────────────────────────────
// State
// ─────────────────────────────────────────────

/// Mutable state threaded through the step executor.
pub struct SimState {
    pub lab: Lab,
    pub env: SimEnv,
    /// Processes spawned by generic `spawn` steps, keyed by step `id`.
    spawned: HashMap<String, GenericProcess>,
    /// In-progress iroh-transfer handles, keyed by step `id`.
    transfers: HashMap<String, TransferHandle>,
    /// Completed transfer results.
    pub results: Vec<TransferResult>,
    /// Parsed iperf results collected from `step.parser = "iperf3-json"`.
    pub iperf_results: Vec<IperfResult>,
    /// Paths to resolved binaries, keyed by `[[binary]] name`.
    pub binaries: HashMap<String, PathBuf>,
    pub work_dir: PathBuf,
    pub sim_name: String,
}

struct GenericProcess {
    child: std::process::Child,
    parser: Option<ParserConfig>,
}

#[derive(Clone)]
struct ParserConfig {
    parser: StepParser,
    result_id: String,
    device: String,
    log_path: PathBuf,
    baseline: Option<String>,
}

#[derive(Clone, Copy)]
enum StepParser {
    Iperf3Json,
}

#[derive(Debug, Clone, Serialize)]
struct StepFailureInfo {
    index: usize,
    action: String,
    id: Option<String>,
    device: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SimFailureInfo {
    phase: String,
    message: String,
    step: Option<StepFailureInfo>,
}

#[derive(Debug, Clone, Serialize)]
struct SimSetupSummary {
    sim_path: String,
    topology_ref: Option<String>,
    topology_mode: String,
    routers: usize,
    devices: usize,
    regions: usize,
    steps: usize,
}

#[derive(Debug, Clone, Serialize)]
struct SimLogEntry {
    node: String,
    kind: String,
    path: String,
}

#[derive(Debug, Clone, Serialize)]
struct SimSummary {
    sim: String,
    sim_dir: String,
    status: String,
    started_at: String,
    ended_at: String,
    runtime_ms: u128,
    setup: SimSetupSummary,
    logs: Vec<SimLogEntry>,
    error: Option<SimFailureInfo>,
}

#[derive(Debug, Clone)]
struct SimRunOutcome {
    sim_dir_name: String,
    summary: SimSummary,
    success: bool,
}

#[derive(Debug, Clone, Serialize)]
struct RunEnvironment {
    os: String,
    arch: String,
    family: String,
    current_dir: String,
    executable: String,
    rust_log: Option<String>,
    netsim_version: String,
}

#[derive(Debug, Clone, Serialize)]
struct ManifestSimSummary {
    sim: String,
    sim_dir: String,
    status: String,
    runtime_ms: Option<u128>,
    sim_json: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RunManifest {
    run: String,
    started_at: String,
    status: String,
    ended_at: Option<String>,
    runtime_ms: Option<u128>,
    success: Option<bool>,
    environment: RunEnvironment,
    simulations: Vec<ManifestSimSummary>,
}

#[derive(Debug, Clone, Serialize)]
struct ProgressSim {
    sim: String,
    status: String,
    sim_dir: Option<String>,
    runtime_ms: Option<u128>,
    sim_json: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RunProgress {
    run: String,
    status: String,
    started_at: String,
    updated_at: String,
    total: usize,
    completed: usize,
    ok: usize,
    error: usize,
    current_sim: Option<String>,
    simulations: Vec<ProgressSim>,
}

impl Drop for SimState {
    fn drop(&mut self) {
        for sp in self.spawned.values_mut() {
            let _ = sp.child.kill();
            let _ = sp.child.wait();
        }
    }
}

// ─────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────

/// Expand one or more sim paths, run each sim, and write combined reports.
pub async fn run_sims(
    sim_inputs: Vec<PathBuf>,
    work_dir: PathBuf,
    binary_overrides: Vec<String>,
) -> Result<()> {
    let sims = expand_sim_inputs(&sim_inputs)?;
    if sims.is_empty() {
        bail!("no sim files found");
    }
    let run_root = prepare_run_root(&work_dir)?;
    let run_start = SystemTime::now();
    let run_start_instant = Instant::now();
    let run_name = run_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("sim-run")
        .to_string();
    let sim_names: Vec<String> = sims
        .iter()
        .map(|sim| {
            sim.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("sim")
                .to_string()
        })
        .collect();
    let mut progress = RunProgress {
        run: run_name.clone(),
        status: "running".to_string(),
        started_at: format_timestamp(run_start),
        updated_at: format_timestamp(run_start),
        total: sims.len(),
        completed: 0,
        ok: 0,
        error: 0,
        current_sim: sim_names.first().cloned(),
        simulations: sim_names
            .iter()
            .map(|sim| ProgressSim {
                sim: sim.clone(),
                status: "pending".to_string(),
                sim_dir: None,
                runtime_ms: None,
                sim_json: None,
            })
            .collect(),
    };
    write_progress(&run_root, &progress).await?;
    let initial_manifest =
        build_run_manifest(&run_root, run_start, None, None, None, &progress, &[])?;
    write_run_manifest(&run_root, &initial_manifest).await?;

    let mut sim_dir_names = Vec::new();
    let mut outcomes = Vec::new();
    for (idx, sim) in sims.into_iter().enumerate() {
        if let Some(item) = progress.simulations.get_mut(idx) {
            item.status = "running".to_string();
        }
        progress.current_sim = progress.simulations.get(idx).map(|s| s.sim.clone());
        progress.updated_at = format_timestamp(SystemTime::now());
        write_progress(&run_root, &progress).await?;

        let outcome = run_single_sim(sim, run_root.clone(), binary_overrides.clone()).await?;
        sim_dir_names.push(outcome.sim_dir_name.clone());
        if let Some(item) = progress.simulations.get_mut(idx) {
            item.status = outcome.summary.status.clone();
            item.sim_dir = Some(outcome.summary.sim_dir.clone());
            item.runtime_ms = Some(outcome.summary.runtime_ms);
            item.sim_json = Some(format!("{}/sim.json", outcome.summary.sim_dir));
            item.sim = outcome.summary.sim.clone();
        }
        progress.completed = outcomes.len() + 1;
        if outcome.success {
            progress.ok += 1;
        } else {
            progress.error += 1;
        }
        progress.current_sim = progress
            .simulations
            .iter()
            .find(|s| s.status == "pending")
            .map(|s| s.sim.clone());
        progress.updated_at = format_timestamp(SystemTime::now());
        write_progress(&run_root, &progress).await?;
        outcomes.push(outcome);
        write_combined_results_for_runs(&run_root, &sim_dir_names)
            .await
            .context("write incremental combined results")?;
        let running_manifest =
            build_run_manifest(&run_root, run_start, None, None, None, &progress, &outcomes)?;
        write_run_manifest(&run_root, &running_manifest).await?;
    }
    write_combined_results_for_runs(&run_root, &sim_dir_names)
        .await
        .context("write combined results")?;
    print_run_summary_table_for_runs(&run_root, &sim_dir_names)
        .context("print run summary table")?;
    let run_end = SystemTime::now();
    progress.status = "done".to_string();
    progress.updated_at = format_timestamp(run_end);
    write_progress(&run_root, &progress).await?;
    let run_manifest = build_run_manifest(
        &run_root,
        run_start,
        Some(run_end),
        Some(run_start_instant.elapsed()),
        Some(outcomes.iter().all(|o| o.success)),
        &progress,
        &outcomes,
    )?;
    write_run_manifest(&run_root, &run_manifest).await?;
    if outcomes.iter().any(|outcome| !outcome.success) {
        bail!(
            "one or more simulations failed; see {}",
            run_root.join("manifest.json").display()
        );
    }
    Ok(())
}

async fn run_single_sim(
    sim_path: PathBuf,
    run_root: PathBuf,
    binary_overrides: Vec<String>,
) -> Result<SimRunOutcome> {
    let started_at = SystemTime::now();
    let started_at_str = format_timestamp(started_at);
    let started_instant = Instant::now();
    let fallback_sim_name = sim_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("sim")
        .to_string();

    let parsed_sim = match std::fs::read_to_string(&sim_path) {
        Ok(text) => match toml::from_str::<SimFile>(&text) {
            Ok(sim) => sim,
            Err(err) => {
                return finalize_failed_sim(
                    &run_root,
                    &sim_path,
                    &fallback_sim_name,
                    started_at_str,
                    started_instant.elapsed(),
                    SimFailureInfo {
                        phase: "parse-sim".to_string(),
                        message: err.to_string(),
                        step: None,
                    },
                    base_setup_summary(&sim_path),
                )
                .await;
            }
        },
        Err(err) => {
            return finalize_failed_sim(
                &run_root,
                &sim_path,
                &fallback_sim_name,
                started_at_str,
                started_instant.elapsed(),
                SimFailureInfo {
                    phase: "read-sim".to_string(),
                    message: err.to_string(),
                    step: None,
                },
                base_setup_summary(&sim_path),
            )
            .await;
        }
    };

    let sim_name = if parsed_sim.sim.name.is_empty() {
        fallback_sim_name.clone()
    } else {
        parsed_sim.sim.name.clone()
    };
    let setup = setup_summary_from_sim(&sim_path, &parsed_sim);
    let run_work_dir = prepare_sim_dir(&run_root, &sim_name)?;
    tokio::fs::create_dir_all(&run_work_dir)
        .await
        .context("create work dir")?;

    let execute = execute_single_sim(
        &sim_path,
        &run_work_dir,
        &sim_name,
        parsed_sim,
        setup.clone(),
        binary_overrides,
    )
    .await;

    match execute {
        Ok(resolved_setup) => {
            let summary = SimSummary {
                sim: sim_name,
                sim_dir: run_work_dir
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("sim")
                    .to_string(),
                status: "ok".to_string(),
                started_at: started_at_str,
                ended_at: format_timestamp(SystemTime::now()),
                runtime_ms: started_instant.elapsed().as_millis(),
                setup: resolved_setup,
                logs: collect_sim_logs(&run_work_dir)?,
                error: None,
            };
            write_sim_summary(&run_work_dir, &summary).await?;
            Ok(SimRunOutcome {
                sim_dir_name: summary.sim_dir.clone(),
                summary,
                success: true,
            })
        }
        Err(err) => {
            let failure = extract_failure_info(&err);
            let resolved_setup = setup_topology_summary(&setup, None);
            let summary = SimSummary {
                sim: sim_name,
                sim_dir: run_work_dir
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("sim")
                    .to_string(),
                status: "error".to_string(),
                started_at: started_at_str,
                ended_at: format_timestamp(SystemTime::now()),
                runtime_ms: started_instant.elapsed().as_millis(),
                setup: resolved_setup,
                logs: collect_sim_logs(&run_work_dir).unwrap_or_default(),
                error: Some(failure),
            };
            write_sim_summary(&run_work_dir, &summary).await?;
            Ok(SimRunOutcome {
                sim_dir_name: summary.sim_dir.clone(),
                summary,
                success: false,
            })
        }
    }
}

fn expand_sim_inputs(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut sims = Vec::new();
    for input in inputs {
        if input.is_file() {
            sims.push(input.clone());
            continue;
        }
        if input.is_dir() {
            let mut dir_sims = Vec::new();
            for ent in std::fs::read_dir(input)
                .with_context(|| format!("read sim dir {}", input.display()))?
            {
                let ent = ent?;
                let path = ent.path();
                if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("toml") {
                    dir_sims.push(path);
                }
            }
            dir_sims.sort();
            sims.extend(dir_sims);
            continue;
        }
        bail!("sim input path does not exist: {}", input.display());
    }
    sims.sort();
    sims.dedup();
    Ok(sims)
}

fn prepare_run_root(work_root: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(work_root)
        .with_context(|| format!("create work root {}", work_root.display()))?;
    let stamp = now_stamp()?;
    let run_base = format!("sim-{}", stamp);
    let mut run_name = run_base.clone();
    let mut run_dir = work_root.join(&run_name);
    let mut n = 1u32;
    loop {
        match std::fs::create_dir(&run_dir) {
            Ok(()) => break,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                run_name = format!("{run_base}-{n}");
                run_dir = work_root.join(&run_name);
                n += 1;
            }
            Err(err) => {
                return Err(err).with_context(|| format!("create run dir {}", run_dir.display()))
            }
        }
    }

    let latest = work_root.join("latest");
    if latest.exists() || std::fs::symlink_metadata(&latest).is_ok() {
        let _ = std::fs::remove_file(&latest);
        let _ = std::fs::remove_dir_all(&latest);
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(Path::new(&run_name), &latest)
        .with_context(|| format!("create latest symlink {}", latest.display()))?;
    #[cfg(not(unix))]
    {
        let _ = std::fs::remove_dir_all(&latest);
        std::fs::create_dir_all(&latest)
            .with_context(|| format!("create latest dir {}", latest.display()))?;
    }
    Ok(run_dir)
}

fn prepare_sim_dir(run_root: &Path, sim_name: &str) -> Result<PathBuf> {
    let sim_base = sanitize_for_filename(sim_name);
    let mut sim_name = sim_base.clone();
    let mut sim_dir = run_root.join(&sim_name);
    let mut n = 1u32;
    loop {
        match std::fs::create_dir(&sim_dir) {
            Ok(()) => return Ok(sim_dir),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                sim_name = format!("{sim_base}-{n}");
                sim_dir = run_root.join(&sim_name);
                n += 1;
            }
            Err(err) => {
                return Err(err).with_context(|| format!("create sim dir {}", sim_dir.display()))
            }
        }
    }
}

async fn execute_single_sim(
    sim_path: &Path,
    run_work_dir: &Path,
    sim_name: &str,
    sim: SimFile,
    setup_base: SimSetupSummary,
    binary_overrides: Vec<String>,
) -> Result<SimSetupSummary> {
    // ── Resolve binaries ─────────────────────────────────────────────────
    let shared_binaries = load_shared_binaries(&sim, sim_path)
        .with_context(|| "step=resolve-binaries".to_string())?;
    let merged_specs = merge_binary_specs(shared_binaries, sim.binaries.clone());
    let overrides = parse_binary_overrides(&binary_overrides)
        .with_context(|| "step=parse-binary-overrides".to_string())?;
    let binary_names = merged_binary_names(&merged_specs, &overrides);

    let mut binary_paths: HashMap<String, PathBuf> = HashMap::new();
    for name in binary_names {
        let path = resolve_binary_path(&name, &merged_specs, &overrides, run_work_dir)
            .await
            .with_context(|| format!("step=resolve-binary name={name}"))?;
        tracing::info!(name = %name, path = %path.display(), "binary ready");
        binary_paths.insert(name, path);
    }

    // ── Load topology ────────────────────────────────────────────────────
    let topo = load_topology(&sim, sim_path).with_context(|| "step=load-topology".to_string())?;
    let setup = setup_topology_summary(&setup_base, Some(&topo));

    // ── Build lab ────────────────────────────────────────────────────────
    let mut lab = Lab::from_config(topo).context("step=configure-lab")?;
    lab.build().await.context("step=build-lab-network")?;

    // ── Build env vars ───────────────────────────────────────────────────
    let bin_strs: HashMap<String, String> = binary_paths
        .iter()
        .map(|(k, v)| (k.clone(), v.to_string_lossy().into_owned()))
        .collect();
    let env = SimEnv::new(lab.env_vars(), bin_strs);

    let mut state = SimState {
        lab,
        env,
        spawned: HashMap::new(),
        transfers: HashMap::new(),
        results: vec![],
        iperf_results: vec![],
        binaries: binary_paths,
        work_dir: run_work_dir.to_path_buf(),
        sim_name: sim_name.to_string(),
    };

    // ── Execute steps ────────────────────────────────────────────────────
    for (idx, step) in sim.steps.iter().enumerate() {
        if let Err(err) = execute_step(&mut state, step) {
            let step_info = StepFailureInfo {
                index: idx,
                action: step.action.clone(),
                id: step.id.clone(),
                device: step.device.clone(),
            };
            return Err(err).context(format!(
                "step-failed:{}",
                serialize_step_failure(&step_info)
            ));
        }
    }

    // Kill any dangling spawned processes.
    for sp in state.spawned.values_mut() {
        let _ = sp.child.kill();
        let _ = sp.child.wait();
    }

    // ── Write results ────────────────────────────────────────────────────
    write_results(
        &state.work_dir,
        &state.sim_name,
        &state.results,
        &state.iperf_results,
    )
    .await
    .context("step=write-results")?;

    Ok(setup)
}

fn base_setup_summary(sim_path: &Path) -> SimSetupSummary {
    SimSetupSummary {
        sim_path: sim_path.display().to_string(),
        topology_ref: None,
        topology_mode: "inline".to_string(),
        routers: 0,
        devices: 0,
        regions: 0,
        steps: 0,
    }
}

fn setup_summary_from_sim(sim_path: &Path, sim: &SimFile) -> SimSetupSummary {
    let mut setup = base_setup_summary(sim_path);
    setup.topology_ref = sim.sim.topology.clone();
    setup.topology_mode = if setup.topology_ref.is_some() {
        "external".to_string()
    } else {
        "inline".to_string()
    };
    setup.routers = sim.router.len();
    setup.devices = sim.device.len();
    setup.regions = sim.region.as_ref().map(|r| r.len()).unwrap_or(0);
    setup.steps = sim.steps.len();
    setup
}

fn setup_topology_summary(base: &SimSetupSummary, topo: Option<&LabConfig>) -> SimSetupSummary {
    let mut setup = base.clone();
    if let Some(topo) = topo {
        setup.routers = topo.router.len();
        setup.devices = topo.device.len();
        setup.regions = topo.region.as_ref().map(|r| r.len()).unwrap_or(0);
    }
    setup
}

async fn finalize_failed_sim(
    run_root: &Path,
    sim_path: &Path,
    sim_name: &str,
    started_at_str: String,
    elapsed: Duration,
    failure: SimFailureInfo,
    setup: SimSetupSummary,
) -> Result<SimRunOutcome> {
    let run_work_dir = prepare_sim_dir(run_root, sim_name)?;
    tokio::fs::create_dir_all(&run_work_dir)
        .await
        .context("create failed sim work dir")?;
    let summary = SimSummary {
        sim: sim_name.to_string(),
        sim_dir: run_work_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("sim")
            .to_string(),
        status: "error".to_string(),
        started_at: started_at_str,
        ended_at: format_timestamp(SystemTime::now()),
        runtime_ms: elapsed.as_millis(),
        setup: SimSetupSummary {
            sim_path: sim_path.display().to_string(),
            ..setup
        },
        logs: collect_sim_logs(&run_work_dir).unwrap_or_default(),
        error: Some(failure),
    };
    write_sim_summary(&run_work_dir, &summary).await?;
    Ok(SimRunOutcome {
        sim_dir_name: summary.sim_dir.clone(),
        summary,
        success: false,
    })
}

async fn write_sim_summary(run_work_dir: &Path, summary: &SimSummary) -> Result<()> {
    let text = serde_json::to_string_pretty(summary).context("serialize sim summary")?;
    tokio::fs::write(run_work_dir.join("sim.json"), text)
        .await
        .with_context(|| format!("write {}", run_work_dir.join("sim.json").display()))?;
    Ok(())
}

fn build_run_manifest(
    run_root: &Path,
    started_at: SystemTime,
    ended_at: Option<SystemTime>,
    elapsed: Option<Duration>,
    success: Option<bool>,
    progress: &RunProgress,
    outcomes: &[SimRunOutcome],
) -> Result<RunManifest> {
    let run = run_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("sim-run")
        .to_string();
    let mut by_sim_dir: HashMap<&str, &SimRunOutcome> = HashMap::new();
    for outcome in outcomes {
        by_sim_dir.insert(outcome.summary.sim_dir.as_str(), outcome);
    }
    let simulations = progress
        .simulations
        .iter()
        .map(|sim| {
            let runtime_ms = sim.runtime_ms.or_else(|| {
                sim.sim_dir.as_deref().and_then(|dir| {
                    by_sim_dir
                        .get(dir)
                        .map(|outcome| outcome.summary.runtime_ms)
                })
            });
            ManifestSimSummary {
                sim: sim.sim.clone(),
                sim_dir: sim.sim_dir.clone().unwrap_or_default(),
                status: sim.status.clone(),
                runtime_ms,
                sim_json: sim.sim_json.clone(),
            }
        })
        .collect();
    Ok(RunManifest {
        run,
        started_at: format_timestamp(started_at),
        status: progress.status.clone(),
        ended_at: ended_at.map(format_timestamp),
        runtime_ms: elapsed.map(|e| e.as_millis()),
        success,
        environment: collect_run_environment()?,
        simulations,
    })
}

async fn write_run_manifest(run_root: &Path, manifest: &RunManifest) -> Result<()> {
    let text = serde_json::to_string_pretty(manifest).context("serialize run manifest")?;
    tokio::fs::write(run_root.join("manifest.json"), text)
        .await
        .with_context(|| format!("write {}", run_root.join("manifest.json").display()))?;
    Ok(())
}

async fn write_progress(run_root: &Path, progress: &RunProgress) -> Result<()> {
    let text = serde_json::to_string_pretty(progress).context("serialize run progress")?;
    tokio::fs::write(run_root.join("progress.json"), text)
        .await
        .with_context(|| format!("write {}", run_root.join("progress.json").display()))?;
    Ok(())
}

fn collect_run_environment() -> Result<RunEnvironment> {
    Ok(RunEnvironment {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        family: std::env::consts::FAMILY.to_string(),
        current_dir: std::env::current_dir()
            .context("get current dir")?
            .display()
            .to_string(),
        executable: std::env::current_exe()
            .ok()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        rust_log: std::env::var("RUST_LOG").ok(),
        netsim_version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

fn format_timestamp(ts: SystemTime) -> String {
    let secs = ts
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs();
    match std::process::Command::new("date")
        .args(["-u", &format!("-d@{secs}"), "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8(out.stdout)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| secs.to_string()),
        _ => secs.to_string(),
    }
}

fn serialize_step_failure(step: &StepFailureInfo) -> String {
    format!(
        "index={};action={};id={};device={}",
        step.index,
        step.action,
        step.id.as_deref().unwrap_or(""),
        step.device.as_deref().unwrap_or("")
    )
}

fn parse_step_failure(raw: &str) -> Option<StepFailureInfo> {
    let mut index = None;
    let mut action = None;
    let mut id = None;
    let mut device = None;
    for part in raw.split(';') {
        let (k, v) = part.split_once('=')?;
        match k {
            "index" => index = v.parse::<usize>().ok(),
            "action" => action = Some(v.to_string()),
            "id" => {
                if !v.is_empty() {
                    id = Some(v.to_string());
                }
            }
            "device" => {
                if !v.is_empty() {
                    device = Some(v.to_string());
                }
            }
            _ => {}
        }
    }
    Some(StepFailureInfo {
        index: index?,
        action: action?,
        id,
        device,
    })
}

fn extract_failure_info(err: &anyhow::Error) -> SimFailureInfo {
    let mut phase = "run".to_string();
    let mut step = None;
    for cause in err.chain() {
        let msg = cause.to_string();
        if let Some(raw) = msg.strip_prefix("step-failed:") {
            if let Some(parsed) = parse_step_failure(raw) {
                phase = "step".to_string();
                step = Some(parsed);
                break;
            }
        } else if let Some(raw) = msg.strip_prefix("step=") {
            phase = raw.to_string();
        }
    }
    SimFailureInfo {
        phase,
        message: format!("{err:#}"),
        step,
    }
}

fn now_stamp() -> Result<String> {
    let out = std::process::Command::new("date")
        .arg("+%y%m%d-%H%M%S")
        .output()
        .context("run date for workdir timestamp")?;
    if out.status.success() {
        let s = String::from_utf8(out.stdout).context("parse date output")?;
        Ok(s.trim().to_string())
    } else {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("system time before epoch")?
            .as_secs();
        Ok(secs.to_string())
    }
}

fn load_shared_binaries(sim: &SimFile, sim_path: &Path) -> Result<Vec<BinarySpec>> {
    #[derive(serde::Deserialize, Default)]
    struct BinaryFile {
        #[serde(default, rename = "binary")]
        binaries: Vec<BinarySpec>,
    }

    let Some(ref_name) = sim.sim.binaries.as_deref() else {
        return Ok(vec![]);
    };
    let sim_dir = sim_path.parent().unwrap_or(Path::new("."));
    let candidates = [
        sim_dir.join(ref_name),
        sim_dir.join("..").join(ref_name),
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(ref_name),
    ];

    let chosen = candidates
        .iter()
        .find(|p| p.exists())
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "shared binaries file '{}' not found (checked: {}, {}, {})",
                ref_name,
                candidates[0].display(),
                candidates[1].display(),
                candidates[2].display()
            )
        })?;
    let text = std::fs::read_to_string(&chosen)
        .with_context(|| format!("read shared binaries file {}", chosen.display()))?;
    let parsed: BinaryFile = toml::from_str(&text).context("parse shared binaries file")?;
    Ok(parsed.binaries)
}

fn merge_binary_specs(
    shared: Vec<BinarySpec>,
    inline: Vec<BinarySpec>,
) -> HashMap<String, BinarySpec> {
    let mut merged = HashMap::new();
    for spec in shared.into_iter().chain(inline) {
        merged.insert(spec.name.clone(), spec);
    }
    merged
}

fn merged_binary_names(
    specs: &HashMap<String, BinarySpec>,
    overrides: &HashMap<String, BinaryOverride>,
) -> Vec<String> {
    let mut names = BTreeSet::new();
    names.extend(specs.keys().cloned());
    names.extend(overrides.keys().cloned());
    names.into_iter().collect()
}

async fn resolve_binary_path(
    name: &str,
    specs: &HashMap<String, BinarySpec>,
    overrides: &HashMap<String, BinaryOverride>,
    work_dir: &Path,
) -> Result<PathBuf> {
    if let Some(override_mode) = overrides.get(name) {
        return match override_mode {
            BinaryOverride::Build(src) => build_local_binary(name, src, work_dir).await,
            BinaryOverride::Fetch(url) => {
                let spec = BinarySpec {
                    name: name.to_string(),
                    path: None,
                    url: Some(url.clone()),
                    repo: None,
                    commit: None,
                    example: None,
                    bin: None,
                };
                build_or_fetch_binary(&spec, work_dir).await
            }
            BinaryOverride::Path(src) => stage_override_binary(name, src, work_dir).await,
        };
    }

    let spec = specs
        .get(name)
        .ok_or_else(|| anyhow!("no binary source configured for '{}'", name))?;
    build_or_fetch_binary(spec, work_dir).await
}

async fn stage_override_binary(name: &str, source: &Path, work_dir: &Path) -> Result<PathBuf> {
    let resolved = resolve_binary_source_path(source, PathResolveMode::from_env())?;
    if !resolved.exists() {
        bail!(
            "binary override path for '{}' does not exist: {}",
            name,
            resolved.display()
        );
    }
    if resolved.is_dir() {
        bail!(
            "binary override path for '{}' is a directory; use mode=build for directories",
            name
        );
    }
    let bins_dir = work_dir.join("bins");
    tokio::fs::create_dir_all(&bins_dir)
        .await
        .context("create bins dir for override")?;
    let staged = bins_dir.join(format!(
        "{}-override{}",
        name,
        resolved
            .extension()
            .map(|ext| format!(".{}", ext.to_string_lossy()))
            .unwrap_or_default()
    ));
    tokio::fs::copy(&resolved, &staged)
        .await
        .with_context(|| format!("copy override binary {}", resolved.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&staged)
            .context("stat staged override binary")?
            .permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(&staged, perms).context("chmod staged override binary")?;
    }
    Ok(staged)
}

// ─────────────────────────────────────────────
// Step executor
// ─────────────────────────────────────────────

fn execute_step(state: &mut SimState, step: &Step) -> Result<()> {
    tracing::info!(
        action = %step.action,
        id = ?step.id,
        device = ?step.device,
        "sim: step"
    );
    if let Some(parser) = step.parser.as_deref() {
        tracing::debug!(parser, id = ?step.id, "sim: parser configured");
    }

    match step.action.as_str() {
        // ── run ──────────────────────────────────────────────────────────
        "run" => {
            let device = step.device.as_deref().context("run: missing device")?;
            let cmd_parts = state
                .env
                .interpolate(step.cmd.as_deref().context("run: missing cmd")?)?;
            tracing::info!(
                device,
                cmd = %shell_join(&cmd_parts),
                "sim: run command"
            );
            let mut cmd = prepare_cmd(&cmd_parts, &step.env, state)?;
            let log_path = node_out_log_path(&state.work_dir, device)?;
            let log = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .with_context(|| format!("open step log {}", log_path.display()))?;
            let log2 = log.try_clone().context("clone run log file")?;
            cmd.stdout(Stdio::from(log)).stderr(Stdio::from(log2));
            let status = state.lab.run_on(device, cmd)?;
            if !status.success() {
                bail!("'run' on '{}' failed: {:?}", device, status);
            }
            if let Some(parser_cfg) = build_parser_config(step, device, &log_path)? {
                apply_parser_result(state, parser_cfg)?;
            }
        }

        // ── spawn ─────────────────────────────────────────────────────────
        "spawn" => {
            let id = step.id.as_deref().context("spawn: missing id")?;

            if step.kind.as_deref() == Some("iroh-transfer") {
                // Resolve transfer binary (named "transfer" by convention).
                let binary = state
                    .binaries
                    .get("transfer")
                    .cloned()
                    .ok_or_else(|| anyhow!("iroh-transfer: no binary named 'transfer'"))?;
                let handle = start_transfer(state, step, &binary)?;
                state.transfers.insert(id.to_string(), handle);
                return Ok(());
            }

            // Generic spawn.
            let device = step.device.as_deref().context("spawn: missing device")?;
            let cmd_parts = state
                .env
                .interpolate(step.cmd.as_deref().context("spawn: missing cmd")?)?;
            tracing::info!(
                id,
                device,
                cmd = %shell_join(&cmd_parts),
                "sim: spawn command"
            );
            let mut cmd = prepare_cmd(&cmd_parts, &step.env, state)?;
            let log_path = node_out_log_path(&state.work_dir, device)?;
            if step.captures.is_empty() {
                let log = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                    .with_context(|| format!("open step log {}", log_path.display()))?;
                let log2 = log.try_clone().context("clone spawn log file")?;
                cmd.stdout(Stdio::from(log)).stderr(Stdio::from(log2));
            } else {
                cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
            }

            let mut child = state
                .lab
                .spawn_unmanaged_on(device, cmd)
                .with_context(|| format!("spawn '{}'", id))?;

            if let Some(after) = &step.ready_after {
                std::thread::sleep(parse_duration(after)?);
            }

            if !step.captures.is_empty() {
                let stdout = child.stdout.take().context("take child stdout")?;
                if let Some(stderr) = child.stderr.take() {
                    let err_log = log_path.clone();
                    std::thread::spawn(move || -> Result<()> {
                        let mut reader = BufReader::new(stderr);
                        let mut line = String::new();
                        loop {
                            line.clear();
                            let n = reader.read_line(&mut line)?;
                            if n == 0 {
                                break;
                            }
                            append_line(&err_log, &line)?;
                        }
                        Ok(())
                    });
                }
                read_captures(stdout, step, id, &mut state.env, &log_path)?;
            }

            let parser = build_parser_config(step, device, &log_path)?;
            state
                .spawned
                .insert(id.to_string(), GenericProcess { child, parser });
        }

        // ── wait ─────────────────────────────────────────────────────────
        "wait" => {
            let dur = parse_duration(step.duration.as_deref().context("wait: missing duration")?)?;
            std::thread::sleep(dur);
        }

        // ── wait-for ──────────────────────────────────────────────────────
        "wait-for" => {
            let id = step.id.as_deref().context("wait-for: missing id")?;
            let timeout = step
                .timeout
                .as_deref()
                .map(parse_duration)
                .transpose()?
                .unwrap_or(Duration::from_secs(300));

            if let Some(handle) = state.transfers.remove(id) {
                let results = finish_transfer(handle, timeout)?;
                state.results.extend(results);
            } else if state.spawned.contains_key(id) {
                let parser = {
                    let sp = state
                        .spawned
                        .get_mut(id)
                        .ok_or_else(|| anyhow!("wait-for '{}' missing spawned process", id))?;
                    let deadline = std::time::Instant::now() + timeout;
                    loop {
                        match sp.child.try_wait().context("try_wait")? {
                            Some(status) => {
                                if !status.success() {
                                    tracing::warn!(id, ?status, "spawned process exited non-zero");
                                }
                                break;
                            }
                            None => {
                                if std::time::Instant::now() >= deadline {
                                    bail!("wait-for '{}' timed out", id);
                                }
                                std::thread::sleep(Duration::from_millis(200));
                            }
                        }
                    }
                    sp.parser.clone()
                };
                if let Some(parser_cfg) = parser {
                    apply_parser_result(state, parser_cfg)?;
                }
            }
            // If id is not found, assume it completed inline — no-op.
        }

        // ── set-impair ────────────────────────────────────────────────────
        "set-impair" => {
            let device = step
                .device
                .as_deref()
                .context("set-impair: missing device")?;
            let ifname = step.interface.as_deref();
            let impair = parse_impair(step)?;
            state.lab.set_impair(device, ifname, impair)?;
        }

        // ── switch-route ──────────────────────────────────────────────────
        "switch-route" => {
            let device = step
                .device
                .as_deref()
                .context("switch-route: missing device")?;
            let to = step.to.as_deref().context("switch-route: missing to")?;
            state.lab.switch_route(device, to)?;
        }

        // ── link-down / link-up ───────────────────────────────────────────
        "link-down" => {
            let device = step
                .device
                .as_deref()
                .context("link-down: missing device")?;
            let iface = step
                .interface
                .as_deref()
                .context("link-down: missing interface")?;
            state.lab.link_down(device, iface)?;
        }
        "link-up" => {
            let device = step.device.as_deref().context("link-up: missing device")?;
            let iface = step
                .interface
                .as_deref()
                .context("link-up: missing interface")?;
            state.lab.link_up(device, iface)?;
        }

        // ── assert ────────────────────────────────────────────────────────
        "assert" => {
            let check = step.check.as_deref().context("assert: missing check")?;
            evaluate_assert(state, check)?;
        }

        other => bail!("unknown step action: '{}'", other),
    }
    Ok(())
}

// ─────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────

fn prepare_cmd(
    parts: &[String],
    extra_env: &HashMap<String, String>,
    state: &SimState,
) -> Result<std::process::Command> {
    if parts.is_empty() {
        bail!("empty cmd");
    }
    let mut cmd = std::process::Command::new(&parts[0]);
    cmd.args(&parts[1..]);
    for (k, v) in state.env.process_env() {
        cmd.env(k, v);
    }
    let rust_log = std::env::var("NETSIM_RUST_LOG").unwrap_or_else(|_| "info".to_string());
    cmd.env("RUST_LOG", rust_log);
    for (k, v) in extra_env {
        cmd.env(k, state.env.interpolate_str(v)?);
    }
    Ok(cmd)
}

fn build_parser_config(step: &Step, device: &str, log_path: &Path) -> Result<Option<ParserConfig>> {
    let Some(parser_raw) = step.parser.as_deref() else {
        return Ok(None);
    };
    let parser = match parser_raw {
        "iperf3-json" | "iperf-json" => StepParser::Iperf3Json,
        other => bail!("unknown parser '{}' (expected iperf3-json)", other),
    };
    let result_id = step
        .id
        .as_deref()
        .ok_or_else(|| anyhow!("parser '{}' requires step id", parser_raw))?;
    Ok(Some(ParserConfig {
        parser,
        result_id: result_id.to_string(),
        device: device.to_string(),
        log_path: log_path.to_path_buf(),
        baseline: step.baseline.clone(),
    }))
}

fn apply_parser_result(state: &mut SimState, parser: ParserConfig) -> Result<()> {
    match parser.parser {
        StepParser::Iperf3Json => {
            let metrics = parse_iperf3_json_log(&parser.log_path)?;
            let mbps = metrics.bits_per_second.map(|bps| bps / 1_000_000.0);
            let baseline_id = parser.baseline.clone();
            let (delta_mbps, delta_pct) = if let Some(ref baseline_id) = baseline_id {
                let base = state
                    .iperf_results
                    .iter()
                    .find(|r| r.id == *baseline_id)
                    .ok_or_else(|| anyhow!("baseline result '{}' not found", baseline_id))?;
                match (mbps, base.mbps) {
                    (Some(cur), Some(base_mbps)) if base_mbps > 0.0 => {
                        let delta = cur - base_mbps;
                        (Some(delta), Some(delta * 100.0 / base_mbps))
                    }
                    (Some(cur), Some(base_mbps)) => (Some(cur - base_mbps), None),
                    _ => (None, None),
                }
            } else {
                (None, None)
            };
            state.iperf_results.push(IperfResult {
                id: parser.result_id,
                device: parser.device,
                bytes: metrics.bytes,
                seconds: metrics.seconds,
                bits_per_second: metrics.bits_per_second,
                mbps,
                retransmits: metrics.retransmits,
                baseline: baseline_id,
                delta_mbps,
                delta_pct,
            });
        }
    }
    Ok(())
}

fn read_captures(
    stdout: std::process::ChildStdout,
    step: &Step,
    step_id: &str,
    env: &mut SimEnv,
    log_path: &Path,
) -> Result<()> {
    let mut pending: HashMap<String, regex::Regex> = step
        .captures
        .iter()
        .filter_map(|(name, spec)| {
            let re_str = spec.stdout_regex.as_ref()?;
            let re = regex::Regex::new(re_str)
                .with_context(|| format!("compile regex for capture '{}'", name))
                .ok()?;
            Some((name.clone(), re))
        })
        .collect();

    if pending.is_empty() {
        return Ok(());
    }

    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        let line = line.context("read spawn stdout")?;
        append_line(log_path, &format!("{}\n", line))?;
        let mut matched = vec![];
        for (name, re) in &pending {
            if let Some(caps) = re.captures(&line) {
                let val = caps
                    .get(1)
                    .map(|m| m.as_str())
                    .unwrap_or_else(|| caps.get(0).unwrap().as_str());
                env.set_capture(step_id, name, val.to_string());
                matched.push(name.clone());
                tracing::debug!(step_id, name, val, "capture resolved");
            }
        }
        for name in matched {
            pending.remove(&name);
        }
        if pending.is_empty() {
            break;
        }
    }

    if !pending.is_empty() {
        bail!(
            "spawn '{}': EOF before captures resolved: {:?}",
            step_id,
            pending.keys().collect::<Vec<_>>()
        );
    }
    Ok(())
}

fn append_line(path: &Path, line: &str) -> Result<()> {
    use std::io::Write;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open append log {}", path.display()))?;
    f.write_all(line.as_bytes())
        .with_context(|| format!("append log {}", path.display()))?;
    Ok(())
}

fn shell_join(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| shell_escape(p))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_escape(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.bytes().all(|b| {
        matches!(
            b,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'/' | b':'
        )
    }) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

fn node_out_log_path(work_dir: &Path, node: &str) -> Result<PathBuf> {
    let node_dir = work_dir.join("nodes").join(sanitize_for_filename(node));
    std::fs::create_dir_all(&node_dir)
        .with_context(|| format!("create node log dir {}", node_dir.display()))?;
    Ok(node_dir.join("out.log"))
}

fn collect_sim_logs(sim_dir: &Path) -> Result<Vec<SimLogEntry>> {
    let nodes_dir = sim_dir.join("nodes");
    if !nodes_dir.is_dir() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for node_entry in std::fs::read_dir(&nodes_dir)
        .with_context(|| format!("read node dir {}", nodes_dir.display()))?
    {
        let node_entry = node_entry?;
        let node_path = node_entry.path();
        if !node_path.is_dir() {
            continue;
        }
        let Some(node_name) = node_path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        collect_node_logs(sim_dir, node_name, &node_path, &mut out)?;
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn collect_node_logs(
    sim_dir: &Path,
    node_name: &str,
    dir: &Path,
    out: &mut Vec<SimLogEntry>,
) -> Result<()> {
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("read node logs {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_node_logs(sim_dir, node_name, &path, out)?;
            continue;
        }
        let rel = path
            .strip_prefix(sim_dir)
            .with_context(|| format!("compute relative path for {}", path.display()))?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let kind = if rel_str.ends_with(".qlog") || rel_str.contains("/qlog/") {
            "qlog"
        } else if rel_str.contains("/transfer-") {
            "transfer"
        } else {
            "text"
        };
        out.push(SimLogEntry {
            node: node_name.to_string(),
            kind: kind.to_string(),
            path: rel_str,
        });
    }
    Ok(())
}

fn sanitize_for_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Parse a duration string like `"5s"`, `"300ms"`, `"1m"`.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix("ms") {
        return Ok(Duration::from_millis(
            n.trim().parse().context("parse milliseconds")?,
        ));
    }
    if let Some(n) = s.strip_suffix('s') {
        return Ok(Duration::from_secs(
            n.trim().parse().context("parse seconds")?,
        ));
    }
    if let Some(n) = s.strip_suffix('m') {
        return Ok(Duration::from_secs(
            n.trim().parse::<u64>().context("parse minutes")? * 60,
        ));
    }
    bail!("unknown duration format: {:?}", s);
}

fn parse_impair(step: &Step) -> Result<Option<Impair>> {
    match &step.impair {
        None => Ok(None),
        Some(v) => {
            let impair: Impair = v
                .clone()
                .try_into()
                .map_err(|e: toml::de::Error| anyhow!("{}", e))?;
            Ok(Some(impair))
        }
    }
}

fn evaluate_assert(state: &SimState, check: &str) -> Result<()> {
    let (lhs, op, rhs) = if let Some(idx) = check.find(" == ") {
        (check[..idx].trim(), "==", check[idx + 4..].trim())
    } else if let Some(idx) = check.find(" != ") {
        (check[..idx].trim(), "!=", check[idx + 4..].trim())
    } else {
        bail!("assert: unrecognised check expression: {:?}", check);
    };

    let lhs_val = resolve_assert_lhs(state, lhs)?;
    let pass = match op {
        "==" => lhs_val == rhs,
        "!=" => lhs_val != rhs,
        _ => unreachable!(),
    };
    if pass {
        tracing::info!(check, "assert: PASS");
        Ok(())
    } else {
        bail!(
            "assert FAILED: '{}' (got '{}') {} '{}'",
            lhs,
            lhs_val,
            op,
            rhs
        );
    }
}

fn resolve_assert_lhs(state: &SimState, lhs: &str) -> Result<String> {
    // Try captures first.
    if let Some(v) = state.env.get_capture(lhs) {
        return Ok(v.to_string());
    }
    // Try transfer result fields: "step_id.field".
    if let Some((id, field)) = lhs.split_once('.') {
        if let Some(result) = state.results.iter().find(|r| r.id == id) {
            return result_field(result, field);
        }
        if let Some(result) = state.iperf_results.iter().find(|r| r.id == id) {
            return iperf_result_field(result, field);
        }
    }
    bail!(
        "assert: cannot resolve '{}' — not a capture or known result field",
        lhs
    );
}

fn result_field(r: &TransferResult, field: &str) -> Result<String> {
    match field {
        "mbps" => Ok(r.mbps.map(|v| format!("{:.1}", v)).unwrap_or_default()),
        "elapsed_s" => Ok(r.elapsed_s.map(|v| format!("{:.3}", v)).unwrap_or_default()),
        "size_bytes" => Ok(r.size_bytes.map(|v| v.to_string()).unwrap_or_default()),
        "final_conn_direct" => Ok(r
            .final_conn_direct
            .map(|v| v.to_string())
            .unwrap_or_default()),
        "conn_upgrade" => Ok(r.conn_upgrade.map(|v| v.to_string()).unwrap_or_default()),
        "conn_events" => Ok(r.conn_events.to_string()),
        other => bail!("unknown result field '{}.{}'", r.id, other),
    }
}

fn iperf_result_field(r: &IperfResult, field: &str) -> Result<String> {
    match field {
        "mbps" => Ok(r.mbps.map(|v| format!("{:.3}", v)).unwrap_or_default()),
        "seconds" => Ok(r.seconds.map(|v| format!("{:.3}", v)).unwrap_or_default()),
        "bytes" => Ok(r.bytes.map(|v| v.to_string()).unwrap_or_default()),
        "retransmits" => Ok(r.retransmits.map(|v| v.to_string()).unwrap_or_default()),
        "delta_mbps" => Ok(r
            .delta_mbps
            .map(|v| format!("{:.3}", v))
            .unwrap_or_default()),
        "delta_pct" => Ok(r.delta_pct.map(|v| format!("{:.1}", v)).unwrap_or_default()),
        other => bail!("unknown iperf result field '{}.{}'", r.id, other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(prefix: &str) -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("netsim-{prefix}-{ts}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn parse_duration_formats() {
        assert_eq!(parse_duration("200ms").unwrap(), Duration::from_millis(200));
        assert_eq!(parse_duration("2s").unwrap(), Duration::from_secs(2));
        assert_eq!(parse_duration("3m").unwrap(), Duration::from_secs(180));
        assert!(parse_duration("3h").is_err());
    }

    #[test]
    fn parse_binary_overrides_formats() {
        let parsed = parse_binary_overrides(&[
            "transfer:build:.".to_string(),
            "relay:fetch:https://example.com/relay.tar.gz".to_string(),
            "alt:path:./bin/alt".to_string(),
        ])
        .expect("parse overrides");
        assert!(matches!(
            parsed.get("transfer").expect("transfer override"),
            BinaryOverride::Build(_)
        ));
        assert!(matches!(
            parsed.get("relay").expect("relay override"),
            BinaryOverride::Fetch(_)
        ));
        assert!(matches!(
            parsed.get("alt").expect("alt override"),
            BinaryOverride::Path(_)
        ));
    }

    #[test]
    fn parse_binary_overrides_rejects_duplicate() {
        let err = parse_binary_overrides(&[
            "transfer:build:.".to_string(),
            "transfer:path:./transfer".to_string(),
        ])
        .expect_err("duplicate should fail");
        assert!(err.to_string().contains("duplicate --binary override"));
    }

    #[test]
    fn expand_sim_inputs_loads_dirs_and_deduplicates() {
        let root = temp_dir("expand-sims");
        std::fs::write(root.join("b.toml"), "x").expect("write b");
        std::fs::write(root.join("a.toml"), "x").expect("write a");
        std::fs::write(root.join("skip.txt"), "x").expect("write txt");

        let a = root.join("a.toml");
        let got = expand_sim_inputs(&[root.clone(), a.clone()]).expect("expand sims");
        assert_eq!(got, vec![a, root.join("b.toml")]);
    }

    #[test]
    fn prepare_run_root_sets_relative_latest_and_unique_suffix() {
        let root = temp_dir("prepare-run");

        let first = prepare_run_root(&root).expect("first run dir");
        let second = prepare_run_root(&root).expect("second run dir");
        assert_ne!(first, second, "second run should not reuse same dir");

        let latest = root.join("latest");
        let target = std::fs::read_link(&latest).expect("read latest symlink");
        assert!(
            !target.is_absolute(),
            "latest symlink target should be relative: {}",
            target.display()
        );
        assert_eq!(
            target,
            second
                .file_name()
                .map(PathBuf::from)
                .expect("second basename"),
            "latest should point at newest run dir"
        );
    }

    #[test]
    fn prepare_sim_dir_sets_unique_suffix() {
        let root = temp_dir("prepare-sim-dir");
        std::fs::create_dir_all(&root).expect("create root");

        let first = prepare_sim_dir(&root, "sim-a").expect("first sim dir");
        let second = prepare_sim_dir(&root, "sim-a").expect("second sim dir");
        assert_ne!(first, second, "second sim should not reuse same dir");
        assert_eq!(
            first.file_name().and_then(|s| s.to_str()),
            Some("sim-a"),
            "first sim dir name"
        );
        assert_eq!(
            second.file_name().and_then(|s| s.to_str()),
            Some("sim-a-1"),
            "second sim dir suffix"
        );
    }

    #[test]
    fn parse_step_failure_roundtrip() {
        let src = StepFailureInfo {
            index: 3,
            action: "wait-for".to_string(),
            id: Some("xfer".to_string()),
            device: Some("fetcher".to_string()),
        };
        let raw = serialize_step_failure(&src);
        let parsed = parse_step_failure(&raw).expect("parse step failure");
        assert_eq!(parsed.index, 3);
        assert_eq!(parsed.action, "wait-for");
        assert_eq!(parsed.id.as_deref(), Some("xfer"));
        assert_eq!(parsed.device.as_deref(), Some("fetcher"));
    }

    #[test]
    fn extract_failure_info_reads_step_context() {
        let err = anyhow!("root cause")
            .context("step=build-lab-network")
            .context("step-failed:index=1;action=assert;id=check;device=fetcher");
        let info = extract_failure_info(&err);
        assert_eq!(info.phase, "step");
        let step = info.step.expect("step info");
        assert_eq!(step.index, 1);
        assert_eq!(step.action, "assert");
        assert_eq!(step.id.as_deref(), Some("check"));
        assert_eq!(step.device.as_deref(), Some("fetcher"));
    }
}
