use anyhow::{anyhow, bail, Context, Result};
use netsim::assets::{
    parse_binary_overrides, resolve_binary_source_path, BinaryOverride, PathResolveMode,
};
use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use netsim::config::LabConfig;
use netsim::Lab;
use serde::Serialize;

use crate::sim::build::build_local_binary;
use crate::sim::build::build_or_fetch_binary;
use crate::sim::env::SimEnv;
use crate::sim::progress::{
    collect_run_environment, format_timestamp, now_stamp, write_progress, write_run_manifest,
    ManifestSimSummary, ProgressSim, RunManifest, RunProgress,
};
use crate::sim::report::{
    print_run_summary_table_for_runs, write_combined_results_for_runs, write_results, IperfResult,
    TransferResult,
};
use crate::sim::steps::{
    execute_step, join_pump, step_action, step_device, step_id, ParserConfig, RelayRuntimeAssets,
};
use crate::sim::topology::load_topology;
use crate::sim::transfer::TransferHandle;
use crate::sim::{BinarySpec, SimFile};

// ─────────────────────────────────────────────
// State
// ─────────────────────────────────────────────

/// Mutable state threaded through the step executor.
pub struct SimState {
    pub(crate) lab: Lab,
    pub(crate) env: SimEnv,
    /// Processes spawned by generic `spawn` steps, keyed by step `id`.
    pub(crate) spawned: HashMap<String, GenericProcess>,
    /// In-progress iroh-transfer handles, keyed by step `id`.
    pub(crate) transfers: HashMap<String, TransferHandle>,
    /// Completed transfer results.
    pub(crate) results: Vec<TransferResult>,
    /// Parsed iperf results collected from `step.parser = "iperf3-json"`.
    pub(crate) iperf_results: Vec<IperfResult>,
    /// Paths to resolved binaries, keyed by `[[binary]] name`.
    pub(crate) binaries: HashMap<String, PathBuf>,
    pub(crate) work_dir: PathBuf,
    pub(crate) sim_name: String,
    pub(crate) verbose: bool,
    pub(crate) relay_assets: HashMap<String, RelayRuntimeAssets>,
}

pub(crate) struct GenericProcess {
    pub(crate) child: std::process::Child,
    pub(crate) parser: Option<ParserConfig>,
    pub(crate) stdout_pump: Option<thread::JoinHandle<Result<()>>>,
    pub(crate) stderr_pump: Option<thread::JoinHandle<Result<()>>>,
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
    error_line: Option<String>,
    error: Option<SimFailureInfo>,
}

#[derive(Debug, Clone)]
struct SimRunOutcome {
    sim_dir_name: String,
    summary: SimSummary,
    success: bool,
}

impl Drop for SimState {
    fn drop(&mut self) {
        for sp in self.spawned.values_mut() {
            let _ = sp.child.kill();
            let _ = sp.child.wait();
            if let Some(h) = sp.stdout_pump.take() {
                let _ = join_pump(h, "drop stdout pump");
            }
            if let Some(h) = sp.stderr_pump.take() {
                let _ = join_pump(h, "drop stderr pump");
            }
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
    verbose: bool,
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
                error: None,
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

        let outcome =
            run_single_sim(sim, run_root.clone(), binary_overrides.clone(), verbose).await?;
        sim_dir_names.push(outcome.sim_dir_name.clone());
        if let Some(item) = progress.simulations.get_mut(idx) {
            item.status = outcome.summary.status.clone();
            item.sim_dir = Some(outcome.summary.sim_dir.clone());
            item.runtime_ms = Some(outcome.summary.runtime_ms);
            item.sim_json = Some(format!("{}/sim.json", outcome.summary.sim_dir));
            item.sim = outcome.summary.sim.clone();
            item.error = summarized_sim_error(&outcome.summary);
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
    let failed: Vec<&SimRunOutcome> = outcomes.iter().filter(|outcome| !outcome.success).collect();
    if !failed.is_empty() {
        let mut msg = String::from("one or more simulations failed:");
        for outcome in failed {
            let detail = summarized_sim_error(&outcome.summary)
                .unwrap_or_else(|| "unknown error".to_string());
            msg.push_str(&format!(
                "\n- {} ({}): {}",
                outcome.summary.sim, outcome.summary.sim_dir, detail
            ));
        }
        msg.push_str(&format!(
            "\nsee {}",
            run_root.join("manifest.json").display()
        ));
        bail!("{msg}");
    }
    Ok(())
}

async fn run_single_sim(
    sim_path: PathBuf,
    run_root: PathBuf,
    binary_overrides: Vec<String>,
    verbose: bool,
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
        verbose,
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
                error_line: None,
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
            let error_line = find_last_error_line_in_out_logs(&run_work_dir);
            let mut failure = extract_failure_info(&err);
            if let Some(line) = error_line.clone() {
                failure.message = line;
            }
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
                error_line,
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
    let stamp = now_stamp();
    let run_base = format!("sim-{}", stamp);
    let run_dir = create_unique_dir(work_root, &run_base)?;
    let run_name = run_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&run_base)
        .to_string();

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
    let sim_base = netsim::util::sanitize_for_path_component(sim_name);
    create_unique_dir(run_root, &sim_base)
}

fn create_unique_dir(parent: &Path, base: &str) -> Result<PathBuf> {
    let mut name = base.to_string();
    let mut path = parent.join(&name);
    let mut n = 1u32;
    loop {
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                name = format!("{base}-{n}");
                path = parent.join(&name);
                n += 1;
            }
            Err(err) => return Err(err).with_context(|| format!("create dir {}", path.display())),
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
    verbose: bool,
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
        verbose,
        relay_assets: HashMap::new(),
    };

    // ── Execute steps ────────────────────────────────────────────────────
    for (idx, step) in sim.steps.iter().enumerate() {
        if let Err(err) = execute_step(&mut state, step) {
            let step_info = StepFailureInfo {
                index: idx,
                action: step_action(step).to_string(),
                id: step_id(step).map(|s| s.to_string()),
                device: step_device(step).map(|s| s.to_string()),
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
    mut failure: SimFailureInfo,
    setup: SimSetupSummary,
) -> Result<SimRunOutcome> {
    let run_work_dir = prepare_sim_dir(run_root, sim_name)?;
    tokio::fs::create_dir_all(&run_work_dir)
        .await
        .context("create failed sim work dir")?;
    let error_line = find_last_error_line_in_out_logs(&run_work_dir);
    if let Some(line) = error_line.clone() {
        failure.message = line;
    }
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
        error_line,
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
                error: sim.error.clone().or_else(|| {
                    sim.sim_dir
                        .as_deref()
                        .and_then(|dir| by_sim_dir.get(dir))
                        .and_then(|outcome| summarized_sim_error(&outcome.summary))
                }),
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

fn summarized_sim_error(summary: &SimSummary) -> Option<String> {
    summary
        .error_line
        .clone()
        .or_else(|| summary.error.as_ref().map(|e| e.message.clone()))
}

fn find_last_error_line_in_out_logs(run_work_dir: &Path) -> Option<String> {
    let mut logs = Vec::new();
    collect_error_log_paths(run_work_dir, &mut logs);
    logs.sort();
    let mut last: Option<String> = None;
    for path in logs {
        if let Some(line) = last_error_line_in_file(&path) {
            last = Some(line);
        }
    }
    last
}

fn collect_error_log_paths(dir: &Path, out: &mut Vec<PathBuf>) {
    let read = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for ent in read.flatten() {
        let path = ent.path();
        if path.is_dir() {
            collect_error_log_paths(&path, out);
            continue;
        }
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            if name == "stderr.log" || name == "stdout.log" || name == "out.log" {
                out.push(path);
            }
        }
    }
}

fn last_error_line_in_file(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut last = None;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.to_ascii_lowercase().contains("error") {
            last = Some(line);
        }
    }
    last
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
