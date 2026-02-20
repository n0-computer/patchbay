use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use netsim::{config::LabConfig, Impair, Lab};

use crate::sim::build::build_or_fetch_binary;
use crate::sim::env::SimEnv;
use crate::sim::report::{write_results, TransferResult};
use crate::sim::transfer::{start_transfer, TransferHandle};
use crate::sim::{SimFile, Step};

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

/// Parse `sim_path`, build the network lab, and execute all steps.
pub async fn run_sim(sim_path: PathBuf, work_dir: PathBuf) -> Result<()> {
    let text = std::fs::read_to_string(&sim_path)
        .with_context(|| format!("read sim file {}", sim_path.display()))?;
    let sim: SimFile = toml::from_str(&text).context("parse sim file")?;

    tokio::fs::create_dir_all(&work_dir)
        .await
        .context("create work dir")?;
    let log_dir = work_dir.join("logs");
    tokio::fs::create_dir_all(&log_dir)
        .await
        .context("create log dir")?;

    // ── Resolve binaries ─────────────────────────────────────────────────
    let mut binary_paths: HashMap<String, PathBuf> = HashMap::new();
    for spec in &sim.binaries {
        let path = build_or_fetch_binary(spec, &work_dir).await?;
        tracing::info!(name = %spec.name, path = %path.display(), "binary ready");
        binary_paths.insert(spec.name.clone(), path);
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
        work_dir: work_dir.clone(),
        sim_name: sim.sim.name.clone(),
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

    Ok(())
}

fn load_topology(sim: &SimFile, sim_path: &Path) -> Result<LabConfig> {
    if let Some(name) = &sim.sim.topology {
        if !sim.router.is_empty() || !sim.device.is_empty() || sim.region.is_some() {
            bail!(
                "sim.topology is set to '{}'; inline router/device/region tables are not allowed",
                name
            );
        }
        let fallback_root = std::env::current_dir()
            .context("resolve current dir for topology fallback")?
            .join("topos")
            .join(format!("{name}.toml"));
        let topo_file = sim_path
            .parent()
            .unwrap_or(Path::new("."))
            .join(format!("../topos/{name}.toml"));
        let chosen = if topo_file.exists() {
            topo_file
        } else if fallback_root.exists() {
            fallback_root
        } else {
            bail!(
                "topology '{}' not found in '{}' or '{}'",
                name,
                sim_path.parent().unwrap_or(Path::new(".")).display(),
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join("topos")
                    .display()
            );
        };
        let text = std::fs::read_to_string(&chosen)
            .with_context(|| format!("read topology file {}", chosen.display()))?;
        toml::from_str::<LabConfig>(&text).context("parse topology file")
    } else {
        Ok(LabConfig {
            router: sim.router.clone(),
            device: sim.device.clone(),
            region: sim.region.clone(),
        })
    }
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

    match step.action.as_str() {
        // ── run ──────────────────────────────────────────────────────────
        "run" => {
            let device = step.device.as_deref().context("run: missing device")?;
            let cmd_parts = state
                .env
                .interpolate(step.cmd.as_deref().context("run: missing cmd")?)?;
            let cmd = prepare_cmd(&cmd_parts, &step.env, state)?;
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
            let mut cmd = prepare_cmd(&cmd_parts, &step.env, state)?;
            cmd.stdout(Stdio::piped()).stderr(Stdio::null());

            let mut child = state
                .lab
                .spawn_unmanaged_on(device, cmd)
                .with_context(|| format!("spawn '{}'", id))?;

            if let Some(after) = &step.ready_after {
                std::thread::sleep(parse_duration(after)?);
            }

            if !step.captures.is_empty() {
                let stdout = child.stdout.take().context("take child stdout")?;
                read_captures(stdout, step, id, &mut state.env)?;
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
                let result = handle
                    .join
                    .join()
                    .map_err(|_| anyhow!("transfer thread '{}' panicked", id))??;
                state.results.push(result);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp_file(dir: &Path, rel: &str, body: &str) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn load_topology_prefers_adjacent_parent_topos() {
        let root = std::env::temp_dir().join(format!(
            "netsim-runner-topo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let sim_file = write_temp_file(
            &root,
            "sims/one/sim.toml",
            "[sim]\nname='x'\ntopology='a'\n",
        );
        let topo = r#"
[[router]]
name = "r1"
[device.d1.eth0]
gateway = "r1"
"#;
        write_temp_file(&root, "sims/topos/a.toml", topo);

        let sim = SimFile {
            sim: crate::sim::SimMeta {
                name: "x".into(),
                topology: Some("a".into()),
            },
            ..Default::default()
        };

        let cfg = load_topology(&sim, &sim_file).unwrap();
        assert_eq!(cfg.router.len(), 1);
        assert!(cfg.device.contains_key("d1"));
    }

    #[test]
    fn parse_duration_formats() {
        assert_eq!(parse_duration("200ms").unwrap(), Duration::from_millis(200));
        assert_eq!(parse_duration("2s").unwrap(), Duration::from_secs(2));
        assert_eq!(parse_duration("3m").unwrap(), Duration::from_secs(180));
        assert!(parse_duration("3h").is_err());
    }

    #[test]
    fn load_topology_rejects_inline_when_topology_ref_set() {
        let sim = SimFile {
            sim: crate::sim::SimMeta {
                name: "x".into(),
                topology: Some("a".into()),
            },
            router: vec![netsim::config::RouterCfg {
                name: "r1".into(),
                region: None,
                upstream: None,
                nat: netsim::NatMode::None,
            }],
            ..Default::default()
        };
        let err = match load_topology(&sim, Path::new("sims/sim.toml")) {
            Ok(_) => panic!("expected inline-topology rejection error"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("inline router/device/region"));
    }
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
