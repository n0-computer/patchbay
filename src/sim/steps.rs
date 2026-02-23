use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::sim::env::SimEnv;
use crate::sim::report::{parse_iperf3_json_log, IperfResult, TransferResult};
use crate::sim::transfer::start_transfer;
use crate::sim::{CaptureSpec, Parser, Step};
use netsim::Impair;

use crate::sim::runner::SimState;

#[derive(Clone)]
pub(crate) struct ParserConfig {
    pub(crate) parser: Parser,
    pub(crate) result_id: String,
    pub(crate) device: String,
    pub(crate) log_path: PathBuf,
    pub(crate) baseline: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct RelayRuntimeAssets {
    config_path: PathBuf,
}

pub(crate) fn step_action(step: &Step) -> &'static str {
    match step {
        Step::Run { .. } => "run",
        Step::Spawn { .. } => "spawn",
        Step::Wait { .. } => "wait",
        Step::WaitFor { .. } => "wait-for",
        Step::SetImpair { .. } => "set-impair",
        Step::SwitchRoute { .. } => "switch-route",
        Step::LinkDown { .. } => "link-down",
        Step::LinkUp { .. } => "link-up",
        Step::Assert { .. } => "assert",
    }
}

pub(crate) fn step_id(step: &Step) -> Option<&str> {
    match step {
        Step::Run { id, .. } => id.as_deref(),
        Step::Spawn { id, .. } => Some(id),
        Step::WaitFor { id, .. } => Some(id),
        _ => None,
    }
}

pub(crate) fn step_device(step: &Step) -> Option<&str> {
    match step {
        Step::Run { device, .. } => Some(device),
        Step::Spawn {
            device,
            kind,
            provider,
            ..
        } => {
            let device = device.as_deref();
            if device.is_some() {
                device
            } else if kind.as_deref() == Some("iroh-transfer") {
                provider.as_deref()
            } else {
                None
            }
        }
        Step::SetImpair { device, .. } => Some(device),
        Step::SwitchRoute { device, .. } => Some(device),
        Step::LinkDown { device, .. } => Some(device),
        Step::LinkUp { device, .. } => Some(device),
        _ => None,
    }
}

pub(crate) fn execute_step(state: &mut SimState, step: &Step) -> Result<()> {
    tracing::info!(
        action = %step_action(step),
        id = ?step_id(step),
        device = ?step_device(step),
        "sim: step"
    );
    if let Step::Run {
        parser: Some(parser),
        ..
    }
    | Step::Spawn {
        parser: Some(parser),
        ..
    } = step
    {
        tracing::debug!(parser = ?parser, id = ?step_id(step), "sim: parser configured");
    }

    match step {
        // ── run ──────────────────────────────────────────────────────────
        Step::Run {
            shared,
            id,
            device,
            cmd,
            parser,
            baseline,
        } => {
            let cmd_parts = state.env.interpolate(cmd)?;
            tracing::info!(
                device,
                cmd = %shell_join(&cmd_parts),
                "sim: run command"
            );
            let mut cmd = prepare_cmd(&cmd_parts, &shared.env, state)?;
            let logs = node_stdio_log_paths(&state.work_dir, device)?;
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = state
                .lab
                .spawn_unmanaged_on(device, cmd)
                .with_context(|| format!("spawn run on '{}'", device))?;
            let stdout = child.stdout.take().context("take run stdout")?;
            let stderr = child.stderr.take().context("take run stderr")?;
            let out_pump = spawn_pipe_pump(
                stdout,
                logs.stdout.clone(),
                verbose_prefix(device, "out"),
                state.verbose,
                None,
            );
            let err_pump = spawn_pipe_pump(
                stderr,
                logs.stderr.clone(),
                verbose_prefix(device, "err"),
                state.verbose,
                None,
            );
            let status = child.wait().context("wait run child")?;
            join_pump(out_pump, "run stdout pump")?;
            join_pump(err_pump, "run stderr pump")?;
            if !status.success() {
                bail!("'run' on '{}' failed: {:?}", device, status);
            }
            if let Some(parser_cfg) =
                build_parser_config(parser, baseline, id.as_deref(), device, &logs.stdout)?
            {
                apply_parser_result(state, parser_cfg)?;
            }
        }

        // ── spawn ─────────────────────────────────────────────────────────
        Step::Spawn {
            shared,
            id,
            device,
            cmd,
            ready_after,
            captures,
            kind,
            parser,
            baseline,
            ..
        } => {
            if kind.as_deref() == Some("iroh-transfer") {
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
            let device = device.as_deref().context("spawn: missing device")?;
            let mut cmd_parts = state
                .env
                .interpolate(cmd.as_deref().context("spawn: missing cmd")?)?;
            maybe_inject_relay_config_path(state, device, &mut cmd_parts)?;
            tracing::info!(
                id,
                device,
                cmd = %shell_join(&cmd_parts),
                "sim: spawn command"
            );
            let mut cmd = prepare_cmd(&cmd_parts, &shared.env, state)?;
            let logs = node_stdio_log_paths(&state.work_dir, device)?;
            let mut stdout_pump = None;
            let mut stderr_pump = None;
            if captures.is_empty() {
                if state.verbose {
                    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
                } else {
                    let out_log = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&logs.stdout)
                        .with_context(|| {
                            format!("open step stdout log {}", logs.stdout.display())
                        })?;
                    let err_log = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&logs.stderr)
                        .with_context(|| {
                            format!("open step stderr log {}", logs.stderr.display())
                        })?;
                    cmd.stdout(Stdio::from(out_log))
                        .stderr(Stdio::from(err_log));
                }
            } else {
                cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
            }

            let mut child = state
                .lab
                .spawn_unmanaged_on(device, cmd)
                .with_context(|| format!("spawn '{}'", id))?;

            if let Some(after) = ready_after {
                std::thread::sleep(parse_duration(after)?);
            }

            if !captures.is_empty() {
                let stdout = child.stdout.take().context("take child stdout")?;
                let stderr = child.stderr.take().context("take child stderr")?;
                let (tx, rx) = mpsc::channel();
                stdout_pump = Some(spawn_pipe_pump(
                    stdout,
                    logs.stdout.clone(),
                    verbose_prefix(device, "out"),
                    state.verbose,
                    Some(tx),
                ));
                stderr_pump = Some(spawn_pipe_pump(
                    stderr,
                    logs.stderr.clone(),
                    verbose_prefix(device, "err"),
                    state.verbose,
                    None,
                ));
                read_captures(rx, captures, id, &mut state.env)?;
            } else if state.verbose {
                let stdout = child.stdout.take().context("take child stdout")?;
                let stderr = child.stderr.take().context("take child stderr")?;
                stdout_pump = Some(spawn_pipe_pump(
                    stdout,
                    logs.stdout.clone(),
                    verbose_prefix(device, "out"),
                    state.verbose,
                    None,
                ));
                stderr_pump = Some(spawn_pipe_pump(
                    stderr,
                    logs.stderr.clone(),
                    verbose_prefix(device, "err"),
                    state.verbose,
                    None,
                ));
            }

            let parser = build_parser_config(parser, baseline, Some(id), device, &logs.stdout)?;
            state.spawned.insert(
                id.to_string(),
                crate::sim::runner::GenericProcess {
                    child,
                    parser,
                    stdout_pump,
                    stderr_pump,
                },
            );
        }

        // ── wait ─────────────────────────────────────────────────────────
        Step::Wait { duration } => {
            let dur = parse_duration(duration)?;
            std::thread::sleep(dur);
        }

        // ── wait-for ──────────────────────────────────────────────────────
        Step::WaitFor { id, timeout } => {
            let timeout = timeout
                .as_deref()
                .map(parse_duration)
                .transpose()?
                .unwrap_or(Duration::from_secs(300));

            if let Some(handle) = state.transfers.remove(id) {
                let results = crate::sim::transfer::finish_transfer(handle, timeout)?;
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
                if let Some(sp) = state.spawned.get_mut(id) {
                    if let Some(h) = sp.stdout_pump.take() {
                        join_pump(h, "spawn stdout pump")?;
                    }
                    if let Some(h) = sp.stderr_pump.take() {
                        join_pump(h, "spawn stderr pump")?;
                    }
                }
                if let Some(parser_cfg) = parser {
                    apply_parser_result(state, parser_cfg)?;
                }
            }
            // If id is not found, assume it completed inline — no-op.
        }

        // ── set-impair ────────────────────────────────────────────────────
        Step::SetImpair {
            device,
            interface,
            impair,
        } => {
            let impair = parse_impair(impair)?;
            state.lab.set_impair(device, interface.as_deref(), impair)?;
        }

        // ── switch-route ──────────────────────────────────────────────────
        Step::SwitchRoute { device, to } => {
            state.lab.switch_route(device, to)?;
        }

        // ── link-down / link-up ───────────────────────────────────────────
        Step::LinkDown { device, interface } => {
            state.lab.link_down(device, interface)?;
        }
        Step::LinkUp { device, interface } => {
            state.lab.link_up(device, interface)?;
        }

        // ── assert ────────────────────────────────────────────────────────
        Step::Assert { check } => {
            evaluate_assert(state, check)?;
        }
    }
    Ok(())
}

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
    let rust_log = std::env::var("NETSIM_RUST_LOG")
        .unwrap_or_else(|_| "iroh=info,iroh::_events=debug".to_string());
    cmd.env("RUST_LOG", rust_log);
    for (k, v) in extra_env {
        cmd.env(k, state.env.interpolate_str(v)?);
    }
    Ok(cmd)
}

fn maybe_inject_relay_config_path(
    state: &mut SimState,
    device: &str,
    cmd_parts: &mut Vec<String>,
) -> Result<()> {
    if cmd_parts.is_empty() {
        return Ok(());
    }
    let Some(relay_bin) = state.binaries.get("relay") else {
        return Ok(());
    };
    if cmd_parts[0] != relay_bin.to_string_lossy() {
        return Ok(());
    }
    if cmd_parts.iter().any(|arg| arg == "--config-path") {
        return Ok(());
    }
    let assets = ensure_relay_runtime_assets(state, device)?;
    cmd_parts.push("--config-path".to_string());
    cmd_parts.push(assets.config_path.display().to_string());
    Ok(())
}

fn ensure_relay_runtime_assets(state: &mut SimState, device: &str) -> Result<RelayRuntimeAssets> {
    if let Some(existing) = state.relay_assets.get(device) {
        return Ok(existing.clone());
    }
    let key_suffix = netsim::util::sanitize_for_env_key(device);
    let relay_ip = state
        .env
        .interpolate_str(&format!("$NETSIM_IP_{key_suffix}"))
        .with_context(|| format!("resolve relay IP for device '{device}'"))?;
    let ip = relay_ip
        .parse::<IpAddr>()
        .with_context(|| format!("parse relay IP '{relay_ip}' for device '{device}'"))?;
    let relay_dir = state
        .work_dir
        .join("relay")
        .join(netsim::util::sanitize_for_path_component(device));
    std::fs::create_dir_all(&relay_dir)
        .with_context(|| format!("create relay runtime dir {}", relay_dir.display()))?;
    let cert_path = relay_dir.join("certificate.crt");
    let key_path = relay_dir.join("certificate.key");
    if !cert_path.exists() || !key_path.exists() {
        let cert = generate_self_signed_relay_cert(ip)?;
        std::fs::write(&cert_path, cert.cert_pem)
            .with_context(|| format!("write {}", cert_path.display()))?;
        std::fs::write(&key_path, cert.key_pem)
            .with_context(|| format!("write {}", key_path.display()))?;
    }
    let config_path = relay_dir.join("relay.cfg");
    if !config_path.exists() {
        let cfg = format!(
            "enable_relay = true\nenable_metrics = true\nenable_quic_addr_discovery = true\n\n[tls]\nmanual_cert_path=\"{}\"\nmanual_key_path=\"{}\"\ncert_mode = \"Manual\"\n",
            cert_path.display(),
            key_path.display()
        );
        std::fs::write(&config_path, cfg)
            .with_context(|| format!("write {}", config_path.display()))?;
    }
    let assets = RelayRuntimeAssets { config_path };
    state
        .relay_assets
        .insert(device.to_string(), assets.clone());
    Ok(assets)
}

struct GeneratedRelayCert {
    cert_pem: String,
    key_pem: String,
}

fn generate_self_signed_relay_cert(ip: IpAddr) -> Result<GeneratedRelayCert> {
    let mut params = rcgen::CertificateParams::new(vec![])?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "netsim-relay");
    params.subject_alt_names.push(rcgen::SanType::IpAddress(ip));
    params
        .subject_alt_names
        .push(rcgen::SanType::DnsName("localhost".try_into()?));
    let key = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    Ok(GeneratedRelayCert {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
}

fn build_parser_config(
    parser: &Option<Parser>,
    baseline: &Option<String>,
    step_id: Option<&str>,
    device: &str,
    log_path: &Path,
) -> Result<Option<ParserConfig>> {
    let Some(parser) = parser else {
        return Ok(None);
    };
    let result_id = step_id.ok_or_else(|| anyhow!("parser '{:?}' requires step id", parser))?;
    Ok(Some(ParserConfig {
        parser: *parser,
        result_id: result_id.to_string(),
        device: device.to_string(),
        log_path: log_path.to_path_buf(),
        baseline: baseline.clone(),
    }))
}

fn apply_parser_result(state: &mut SimState, parser: ParserConfig) -> Result<()> {
    match parser.parser {
        Parser::Iperf3Json => {
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
    rx: mpsc::Receiver<String>,
    captures: &HashMap<String, CaptureSpec>,
    step_id: &str,
    env: &mut SimEnv,
) -> Result<()> {
    let mut pending: HashMap<String, regex::Regex> = captures
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

    for line in rx {
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

fn spawn_pipe_pump<R: Read + Send + 'static>(
    reader: R,
    path: PathBuf,
    prefix: String,
    verbose: bool,
    forward: Option<mpsc::Sender<String>>,
) -> thread::JoinHandle<Result<()>> {
    thread::spawn(move || {
        let mut out = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open pipe log {}", path.display()))?;
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        while reader.read_line(&mut line)? > 0 {
            let trimmed = line.trim_end().to_string();
            if verbose {
                println!("{}: {}", prefix, trimmed);
            }
            writeln!(out, "{}", trimmed)?;
            if let Some(tx) = &forward {
                let _ = tx.send(trimmed);
            }
            line.clear();
        }
        Ok(())
    })
}

pub(crate) fn join_pump(handle: thread::JoinHandle<Result<()>>, label: &str) -> Result<()> {
    match handle.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(err).context(label.to_string()),
        Err(_) => Err(anyhow!("pump thread panicked: {}", label)),
    }
}

pub(crate) fn parse_duration(s: &str) -> Result<Duration> {
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

fn parse_impair(value: &Option<toml::Value>) -> Result<Option<Impair>> {
    match value {
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
    // Try transfer/result fields: "step_id.field".
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

fn verbose_prefix(device: &str, stream: &str) -> String {
    let mut dev: String = device.chars().take(10).collect();
    let cur = dev.chars().count();
    if cur < 10 {
        dev.push_str(&" ".repeat(10 - cur));
    }
    format!("{dev}{stream}")
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

struct NodeStdioLogs {
    stdout: PathBuf,
    stderr: PathBuf,
}

fn node_stdio_log_paths(work_dir: &Path, node: &str) -> Result<NodeStdioLogs> {
    let node_dir = work_dir
        .join("nodes")
        .join(netsim::util::sanitize_for_path_component(node));
    std::fs::create_dir_all(&node_dir)
        .with_context(|| format!("create node logs dir {}", node_dir.display()))?;
    Ok(NodeStdioLogs {
        stdout: node_dir.join("stdout.log"),
        stderr: node_dir.join("stderr.log"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_formats() {
        assert_eq!(parse_duration("200ms").unwrap(), Duration::from_millis(200));
        assert_eq!(parse_duration("2s").unwrap(), Duration::from_secs(2));
        assert_eq!(parse_duration("3m").unwrap(), Duration::from_secs(180));
        assert!(parse_duration("3h").is_err());
    }
}
