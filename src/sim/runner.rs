use anyhow::{anyhow, bail, Context, Result};
use std::collections::{BTreeSet, HashMap};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use netsim::{Impair, Lab};

use crate::sim::build::build_local_binary;
use crate::sim::build::build_or_fetch_binary;
use crate::sim::env::SimEnv;
use crate::sim::report::{
    print_combined_results_table_for_runs, write_combined_results_for_runs, write_results,
    TransferResult,
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
    /// Paths to resolved binaries, keyed by `[[binary]] name`.
    pub binaries: HashMap<String, PathBuf>,
    pub work_dir: PathBuf,
    pub sim_name: String,
}

struct GenericProcess {
    child: std::process::Child,
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
    let mut run_names = Vec::new();
    for sim in sims {
        let run_dir = run_single_sim(sim, work_dir.clone(), binary_overrides.clone()).await?;
        if let Some(run_name) = run_dir.file_name().and_then(|s| s.to_str()) {
            run_names.push(run_name.to_string());
        }
    }
    write_combined_results_for_runs(&work_dir, &run_names)
        .await
        .context("write combined results")?;
    print_combined_results_table_for_runs(&work_dir, &run_names)
        .context("print combined results table")?;
    Ok(())
}

async fn run_single_sim(
    sim_path: PathBuf,
    work_dir: PathBuf,
    binary_overrides: Vec<String>,
) -> Result<PathBuf> {
    let text = std::fs::read_to_string(&sim_path)
        .with_context(|| format!("read sim file {}", sim_path.display()))?;
    let sim: SimFile = toml::from_str(&text).context("parse sim file")?;
    let sim_name = if sim.sim.name.is_empty() {
        sim_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("sim")
            .to_string()
    } else {
        sim.sim.name.clone()
    };
    let run_work_dir = prepare_run_dir(&work_dir, &sim_name)?;

    tokio::fs::create_dir_all(&run_work_dir)
        .await
        .context("create work dir")?;
    let log_dir = run_work_dir.join("logs");
    tokio::fs::create_dir_all(&log_dir)
        .await
        .context("create log dir")?;

    // ── Resolve binaries ─────────────────────────────────────────────────
    let shared_binaries = load_shared_binaries(&sim, &sim_path)?;
    let merged_specs = merge_binary_specs(shared_binaries, sim.binaries.clone());
    let overrides = parse_binary_overrides(&binary_overrides)?;
    let binary_names = merged_binary_names(&merged_specs, &overrides);

    let mut binary_paths: HashMap<String, PathBuf> = HashMap::new();
    for name in binary_names {
        let path = resolve_binary_path(&name, &merged_specs, &overrides, &run_work_dir).await?;
        tracing::info!(name = %name, path = %path.display(), "binary ready");
        binary_paths.insert(name, path);
    }

    // ── Load topology ────────────────────────────────────────────────────
    let topo = load_topology(&sim, &sim_path)?;

    // ── Build lab ────────────────────────────────────────────────────────
    let mut lab = Lab::from_config(topo).context("configure lab")?;
    lab.build().await.context("build lab network")?;

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
        binaries: binary_paths,
        work_dir: run_work_dir.clone(),
        sim_name: sim_name.clone(),
    };

    // ── Execute steps ────────────────────────────────────────────────────
    for step in &sim.steps {
        execute_step(&mut state, step, &log_dir)?;
    }

    // Kill any dangling spawned processes.
    for sp in state.spawned.values_mut() {
        let _ = sp.child.kill();
        let _ = sp.child.wait();
    }

    // ── Write results ────────────────────────────────────────────────────
    write_results(&state.work_dir, &state.sim_name, &state.results)
        .await
        .context("write results")?;

    Ok(run_work_dir)
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

fn prepare_run_dir(work_root: &Path, sim_name: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(work_root)
        .with_context(|| format!("create work root {}", work_root.display()))?;
    let stamp = now_stamp()?;
    let run_base = format!("{}-{}", sanitize_for_filename(sim_name), stamp);
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

#[derive(Debug, Clone)]
enum BinaryOverride {
    Build(PathBuf),
    Fetch(String),
    Path(PathBuf),
}

fn parse_binary_overrides(raw: &[String]) -> Result<HashMap<String, BinaryOverride>> {
    let mut out = HashMap::new();
    for item in raw {
        let mut parts = item.splitn(3, ':');
        let name = parts
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("invalid --binary override '{}': missing name", item))?;
        let mode = parts
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("invalid --binary override '{}': missing mode", item))?;
        let value = parts
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("invalid --binary override '{}': missing value", item))?;

        if out.contains_key(name) {
            bail!("duplicate --binary override for '{}'", name);
        }
        let parsed = match mode {
            "build" => BinaryOverride::Build(PathBuf::from(value)),
            "fetch" => BinaryOverride::Fetch(value.to_string()),
            "path" => BinaryOverride::Path(PathBuf::from(value)),
            _ => bail!(
                "invalid --binary override mode '{}' in '{}'; expected build|fetch|path",
                mode,
                item
            ),
        };
        out.insert(name.to_string(), parsed);
    }
    Ok(out)
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
    if !source.exists() {
        bail!(
            "binary override path for '{}' does not exist: {}",
            name,
            source.display()
        );
    }
    if source.is_dir() {
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
        source
            .extension()
            .map(|ext| format!(".{}", ext.to_string_lossy()))
            .unwrap_or_default()
    ));
    tokio::fs::copy(source, &staged)
        .await
        .with_context(|| format!("copy override binary {}", source.display()))?;
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

fn execute_step(state: &mut SimState, step: &Step, log_dir: &Path) -> Result<()> {
    tracing::info!(
        action = %step.action,
        id = ?step.id,
        device = ?step.device,
        "sim: step"
    );
    if let Some(parser) = step.parser.as_deref() {
        tracing::debug!(parser, id = ?step.id, "sim: parser hint");
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
            let log_path = step_log_path(step, log_dir);
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
                let handle = start_transfer(state, step, log_dir, &binary)?;
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
            let log_path = step_log_path(step, log_dir);
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

            state
                .spawned
                .insert(id.to_string(), GenericProcess { child });
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
            } else if let Some(sp) = state.spawned.get_mut(id) {
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

fn step_log_path(step: &Step, log_dir: &Path) -> PathBuf {
    let file_stem = if let Some(id) = &step.id {
        id.clone()
    } else {
        let action = sanitize_for_filename(&step.action);
        let dev = step
            .device
            .as_deref()
            .map(sanitize_for_filename)
            .unwrap_or_else(|| "step".to_string());
        format!("{}_{}", action, dev)
    };
    log_dir.join(format!("{}.log", sanitize_for_filename(&file_stem)))
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
    fn prepare_run_dir_sets_relative_latest_and_unique_suffix() {
        let root = temp_dir("prepare-run");

        let first = prepare_run_dir(&root, "sim-a").expect("first run dir");
        let second = prepare_run_dir(&root, "sim-a").expect("second run dir");
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
}
