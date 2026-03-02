use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::{BufRead, BufReader, Read, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use patchbay::LinkCondition;

use crate::sim::{
    capture::CaptureStore, env::SimEnv, report::StepResultRecord, runner::SimState, CaptureSpec,
    Parser, Step, StepResults,
};

pub(crate) fn step_action(step: &Step) -> &'static str {
    match step {
        Step::Run { .. } => "run",
        Step::Spawn { .. } => "spawn",
        Step::Wait { .. } => "wait",
        Step::WaitFor { .. } => "wait-for",
        Step::SetLinkCondition { .. } => "set-link-condition",
        Step::SetDefaultRoute { .. } | Step::SwitchRoute { .. } => "switch-route",
        Step::LinkDown { .. } => "link-down",
        Step::LinkUp { .. } => "link-up",
        Step::Assert { .. } => "assert",
        Step::GenCerts { .. } => "gen-certs",
        Step::GenFile { .. } => "gen-file",
    }
}

pub(crate) fn step_id(step: &Step) -> Option<&str> {
    match step {
        Step::Run { id, .. } => id.as_deref(),
        Step::Spawn { id, .. } => Some(id),
        Step::WaitFor { id, .. } => Some(id),
        Step::GenCerts { id, .. } => Some(id),
        Step::GenFile { id, .. } => Some(id),
        _ => None,
    }
}

pub(crate) fn step_device(step: &Step) -> Option<&str> {
    match step {
        Step::Run { device, .. } => Some(device),
        Step::Spawn { device, .. } => device.as_deref(),
        Step::SetLinkCondition { device, .. } => Some(device),
        Step::SetDefaultRoute { device, .. } | Step::SwitchRoute { device, .. } => Some(device),
        Step::LinkDown { device, .. } => Some(device),
        Step::LinkUp { device, .. } => Some(device),
        Step::GenCerts { device, .. } => device.as_deref(),
        Step::GenFile { device, .. } => device.as_deref(),
        _ => None,
    }
}

const DEFAULT_CAPTURE_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) async fn execute_step(state: &mut SimState, step: &Step) -> Result<()> {
    tracing::info!(
        action = %step_action(step),
        id = ?step_id(step),
        device = ?step_device(step),
        "sim: step"
    );

    // Block on `requires` captures before executing.
    let requires = step_requires(step);
    for key in requires {
        state
            .captures
            .wait(key, DEFAULT_CAPTURE_TIMEOUT)
            .with_context(|| {
                format!(
                    "step '{}': requires '{}'",
                    step_id(step).unwrap_or("?"),
                    key
                )
            })?;
    }

    match step {
        // ── run ──────────────────────────────────────────────────────────
        Step::Run {
            id,
            device,
            cmd,
            env,
            parser,
            captures,
            results,
            ..
        } => {
            let cmd_parts = interpolate_with_captures(cmd, &state.env, &state.captures)?;
            tracing::info!(
                device,
                cmd = %shell_join(&cmd_parts),
                "sim: run command"
            );
            let mut cmd = prepare_cmd(&cmd_parts, env, state)?;
            let logs = node_stdio_log_paths(&state.work_dir, device)?;
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = state
                .lab
                .device_by_name(device)
                .ok_or_else(|| anyhow::anyhow!("unknown device '{}'", device))?
                .spawn_command_sync(cmd)
                .with_context(|| format!("spawn run on '{}'", device))?;
            let stdout = child.stdout.take().context("take run stdout")?;
            let stderr = child.stderr.take().context("take run stderr")?;

            let (tx, rx) = mpsc::channel::<String>();
            let out_pump = spawn_pipe_pump(
                stdout,
                logs.stdout.clone(),
                verbose_prefix(device, "out"),
                state.verbose,
                Some(tx.clone()),
            );
            let err_pump = spawn_pipe_pump(
                stderr,
                logs.stderr.clone(),
                verbose_prefix(device, "err"),
                state.verbose,
                if captures.iter().any(|(_, s)| s.pipe == "stderr") {
                    Some(tx.clone())
                } else {
                    None
                },
            );
            drop(tx); // So the channel closes when pumps finish.

            // Spawn capture reader.
            let sid = id.clone().unwrap_or_default();
            let cap_reader = if !captures.is_empty() {
                Some(spawn_capture_reader(
                    rx,
                    parser.clone(),
                    captures.clone(),
                    sid.clone(),
                    state.captures.clone(),
                ))
            } else {
                drop(rx);
                None
            };

            let status = child.wait().context("wait run child")?;
            join_pump(out_pump, "run stdout pump")?;
            join_pump(err_pump, "run stderr pump")?;
            if let Some(h) = cap_reader {
                join_pump(h, "run capture reader")?;
            }

            if !status.success() {
                bail!("'run' on '{}' failed: {:?}", device, status);
            }

            // Collect results from captures.
            if let Some(step_results) = results {
                if let Some(record) = collect_step_results(&sid, step_results, &state.captures) {
                    state.step_results.push(record);
                }
            }
        }

        // ── spawn ─────────────────────────────────────────────────────────
        Step::Spawn {
            id,
            device,
            cmd,
            env,
            parser,
            ready_after,
            captures,
            results,
            ..
        } => {
            let device = device.as_deref().context("spawn: missing device")?;
            let cmd_parts_final = interpolate_with_captures(
                cmd.as_deref().context("spawn: missing cmd")?,
                &state.env,
                &state.captures,
            )?;
            tracing::info!(
                id,
                device,
                cmd = %shell_join(&cmd_parts_final),
                "sim: spawn command"
            );
            let mut cmd = prepare_cmd(&cmd_parts_final, env, state)?;
            let logs = node_stdio_log_paths(&state.work_dir, device)?;

            let needs_pipes = !captures.is_empty() || state.verbose;
            if needs_pipes {
                cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
            } else {
                let out_log = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&logs.stdout)
                    .with_context(|| format!("open step stdout log {}", logs.stdout.display()))?;
                let err_log = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&logs.stderr)
                    .with_context(|| format!("open step stderr log {}", logs.stderr.display()))?;
                cmd.stdout(Stdio::from(out_log))
                    .stderr(Stdio::from(err_log));
            }

            let mut child = state
                .lab
                .device_by_name(device)
                .ok_or_else(|| anyhow::anyhow!("unknown device '{}'", device))?
                .spawn_command_sync(cmd)
                .with_context(|| format!("spawn '{}'", id))?;

            let (out_pump, err_pump, cap_reader) = if needs_pipes {
                let stdout = child.stdout.take().context("take child stdout")?;
                let stderr = child.stderr.take().context("take child stderr")?;

                let (out_tx, out_rx) = mpsc::channel::<String>();
                let (err_tx, err_rx) = mpsc::channel::<String>();

                let sp = spawn_pipe_pump(
                    stdout,
                    logs.stdout.clone(),
                    verbose_prefix(device, "out"),
                    state.verbose,
                    Some(out_tx),
                );
                let ep = spawn_pipe_pump(
                    stderr,
                    logs.stderr.clone(),
                    verbose_prefix(device, "err"),
                    state.verbose,
                    Some(err_tx),
                );

                let cr = if !captures.is_empty() {
                    // Merge out_rx + err_rx into one channel for the capture reader.
                    let (merged_tx, merged_rx) = mpsc::channel::<String>();
                    let fwd_out_tx = merged_tx.clone();
                    thread::spawn(move || {
                        for line in out_rx {
                            let _ = fwd_out_tx.send(line);
                        }
                    });
                    let fwd_err_tx = merged_tx.clone();
                    drop(merged_tx);
                    thread::spawn(move || {
                        for line in err_rx {
                            let _ = fwd_err_tx.send(line);
                        }
                    });
                    Some(spawn_capture_reader(
                        merged_rx,
                        parser.clone(),
                        captures.clone(),
                        id.clone(),
                        state.captures.clone(),
                    ))
                } else {
                    // Drain channels so pumps don't block.
                    thread::spawn(move || for _ in out_rx {});
                    thread::spawn(move || for _ in err_rx {});
                    None
                };

                (Some(sp), Some(ep), cr)
            } else {
                (None, None, None)
            };

            if let Some(after) = ready_after {
                std::thread::sleep(parse_duration(after)?);
            }

            state.spawned.insert(
                id.to_string(),
                crate::sim::runner::GenericProcess {
                    child,
                    stdout_pump: out_pump,
                    stderr_pump: err_pump,
                    capture_reader: cap_reader,
                },
            );

            // Stash results spec for post-wait-for collection.
            if let Some(step_results) = results {
                state
                    .spawn_results
                    .insert(id.clone(), (step_results.clone(), device.to_string()));
            }
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

            if state.spawned.contains_key(id) {
                {
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
                }
                if let Some(sp) = state.spawned.get_mut(id) {
                    if let Some(h) = sp.stdout_pump.take() {
                        join_pump(h, "spawn stdout pump")?;
                    }
                    if let Some(h) = sp.stderr_pump.take() {
                        join_pump(h, "spawn stderr pump")?;
                    }
                    if let Some(h) = sp.capture_reader.take() {
                        join_pump(h, "spawn capture reader")?;
                    }
                }
                // Collect step results from captures.
                if let Some((step_results, _device)) = state.spawn_results.remove(id) {
                    if let Some(record) = collect_step_results(id, &step_results, &state.captures) {
                        state.step_results.push(record);
                    }
                }
            }
            // If id is not found, assume it completed inline — no-op.
        }

        // ── set-link-condition ────────────────────────────────────────────
        Step::SetLinkCondition {
            device,
            interface,
            condition,
        } => {
            let condition = parse_link_condition(condition)?;
            let dev = state
                .lab
                .device_by_name(device)
                .ok_or_else(|| anyhow::anyhow!("unknown device '{}'", device))?;
            let ifname = match interface.as_deref() {
                Some(n) => n.to_string(),
                None => dev
                    .default_iface()
                    .context("device removed")?
                    .name()
                    .to_string(),
            };
            dev.set_link_condition(&ifname, condition).await?;
        }

        // ── switch-route / set-default-route ──────────────────────────────
        Step::SwitchRoute { device, to } | Step::SetDefaultRoute { device, to } => {
            state
                .lab
                .device_by_name(device)
                .ok_or_else(|| anyhow::anyhow!("unknown device '{}'", device))?
                .set_default_route(to)
                .await?;
        }

        // ── link-down / link-up ───────────────────────────────────────────
        Step::LinkDown { device, interface } => {
            state
                .lab
                .device_by_name(device)
                .ok_or_else(|| anyhow::anyhow!("unknown device '{}'", device))?
                .link_down(interface)
                .await?;
        }
        Step::LinkUp { device, interface } => {
            state
                .lab
                .device_by_name(device)
                .ok_or_else(|| anyhow::anyhow!("unknown device '{}'", device))?
                .link_up(interface)
                .await?;
        }

        // ── assert ────────────────────────────────────────────────────────
        Step::Assert { check, checks } => {
            if let Some(expr) = check {
                evaluate_assert(state, expr)?;
            }
            for expr in checks {
                evaluate_assert(state, expr)?;
            }
        }

        // ── gen-certs ─────────────────────────────────────────────────────
        Step::GenCerts {
            id,
            device,
            cn,
            san,
        } => {
            let device_name = device.as_deref().unwrap_or(id.as_str());
            let key_suffix = patchbay::util::sanitize_for_env_key(device_name);
            let relay_ip = state
                .env
                .interpolate_str(&format!("$NETSIM_IP_{key_suffix}"))
                .with_context(|| format!("resolve IP for gen-certs device '{device_name}'"))?;
            let ip = relay_ip
                .parse::<IpAddr>()
                .with_context(|| format!("parse IP '{relay_ip}' for gen-certs '{id}'"))?;

            let certs_dir = state
                .work_dir
                .join("certs")
                .join(patchbay::util::sanitize_for_path_component(id));
            std::fs::create_dir_all(&certs_dir)
                .with_context(|| format!("create certs dir {}", certs_dir.display()))?;

            let cert_pem_path = certs_dir.join("cert.pem");
            let key_pem_path = certs_dir.join("key.pem");

            let cn_val = cn.as_deref().unwrap_or("patchbay");
            let mut params = rcgen::CertificateParams::new(vec![])?;
            params
                .distinguished_name
                .push(rcgen::DnType::CommonName, cn_val);
            params.subject_alt_names.push(rcgen::SanType::IpAddress(ip));
            if let Some(extra_sans) = san {
                for s in extra_sans {
                    let interpolated = state.env.interpolate_str(s)?;
                    if let Ok(ip_addr) = interpolated.parse::<IpAddr>() {
                        params
                            .subject_alt_names
                            .push(rcgen::SanType::IpAddress(ip_addr));
                    } else {
                        params
                            .subject_alt_names
                            .push(rcgen::SanType::DnsName(interpolated.try_into()?));
                    }
                }
            }
            let key = rcgen::KeyPair::generate()?;
            let cert = params.self_signed(&key)?;

            std::fs::write(&cert_pem_path, cert.pem())
                .with_context(|| format!("write cert {}", cert_pem_path.display()))?;
            std::fs::write(&key_pem_path, key.serialize_pem())
                .with_context(|| format!("write key {}", key_pem_path.display()))?;

            state.captures.record(
                &format!("{id}.cert_pem_path"),
                cert_pem_path.display().to_string(),
            );
            state.captures.record(
                &format!("{id}.key_pem_path"),
                key_pem_path.display().to_string(),
            );
            tracing::info!(id, cert_pem_path = %cert_pem_path.display(), "gen-certs: done");
        }

        // ── gen-file ──────────────────────────────────────────────────────
        Step::GenFile {
            id,
            device: _,
            content,
        } => {
            let interpolated = interpolate_with_captures(
                std::slice::from_ref(content),
                &state.env,
                &state.captures,
            )?;
            let text = interpolated.into_iter().next().unwrap_or_default();

            let files_dir = state
                .work_dir
                .join("files")
                .join(patchbay::util::sanitize_for_path_component(id));
            std::fs::create_dir_all(&files_dir)
                .with_context(|| format!("create files dir {}", files_dir.display()))?;
            let file_path = files_dir.join("content");
            std::fs::write(&file_path, &text)
                .with_context(|| format!("write gen-file {}", file_path.display()))?;

            state
                .captures
                .record(&format!("{id}.path"), file_path.display().to_string());
            tracing::info!(id, path = %file_path.display(), "gen-file: done");
        }
    }
    Ok(())
}

/// Resolve the `requires` keys from a step.
fn step_requires(step: &Step) -> &[String] {
    match step {
        Step::Run { requires, .. } => requires,
        Step::Spawn { requires, .. } => requires,
        _ => &[],
    }
}

/// Interpolate a slice of strings, blocking on `${step_id.capture}` tokens.
pub(crate) fn interpolate_with_captures(
    parts: &[String],
    env: &SimEnv,
    captures: &CaptureStore,
) -> Result<Vec<String>> {
    parts
        .iter()
        .map(|s| interpolate_str_with_captures(s, env, captures))
        .collect()
}

fn interpolate_str_with_captures(s: &str, env: &SimEnv, captures: &CaptureStore) -> Result<String> {
    // Pre-check: if no `${` tokens, fast path via env interpolation.
    if !s.contains("${") {
        return env.interpolate_str(s);
    }

    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while !rest.is_empty() {
        if let Some(idx) = rest.find("${") {
            out.push_str(&rest[..idx]);
            rest = &rest[idx + 2..];
            let end = rest
                .find('}')
                .ok_or_else(|| anyhow!("unclosed '{{' in {:?}", s))?;
            let key = &rest[..end];
            rest = &rest[end + 1..];

            if key.starts_with("binary.") {
                out.push_str(&env.interpolate_str(&format!("${{{}}}", key))?);
            } else if key.contains('.') {
                // Capture reference: block until available.
                let val = captures.wait(key, DEFAULT_CAPTURE_TIMEOUT)?;
                out.push_str(&val);
            } else {
                // Lab var.
                out.push_str(&env.interpolate_str(&format!("${{{}}}", key))?);
            }
        } else if let Some(idx) = rest.find('$') {
            out.push_str(&rest[..idx]);
            rest = &rest[idx + 1..];
            let end = rest
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(rest.len());
            let key = &rest[..end];
            rest = &rest[end..];
            out.push_str(&env.interpolate_str(&format!("${}", key))?);
        } else {
            out.push_str(rest);
            break;
        }
    }
    Ok(out)
}

/// Spawn a thread that reads lines from `rx` and records capture matches into `store`.
fn spawn_capture_reader(
    rx: mpsc::Receiver<String>,
    parser: Parser,
    specs: HashMap<String, CaptureSpec>,
    step_id: String,
    store: CaptureStore,
) -> thread::JoinHandle<Result<()>> {
    thread::spawn(move || {
        match parser {
            Parser::Json | Parser::Iperf3Json => {
                // Post-exit: collect all lines, parse as one JSON object.
                let full: String = rx.into_iter().collect::<Vec<_>>().join("\n");
                let v: serde_json::Value = match serde_json::from_str(&full) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(step_id, "capture reader: json parse error: {e}");
                        return Ok(());
                    }
                };
                for (name, spec) in &specs {
                    if let Some(val) = apply_json_pick(&v, spec) {
                        store.record(&format!("{step_id}.{name}"), val);
                    }
                }
            }
            Parser::Text | Parser::Ndjson => {
                // Streaming: process line by line.
                for line in rx {
                    for (name, spec) in &specs {
                        if let Some(val) = apply_line_match(&line, spec, &parser) {
                            store.record(&format!("{step_id}.{name}"), val);
                        }
                    }
                }
            }
        }
        Ok(())
    })
}

fn apply_line_match(line: &str, spec: &CaptureSpec, parser: &Parser) -> Option<String> {
    // Try regex first.
    if let Some(re_str) = spec.effective_regex() {
        let re = regex::Regex::new(re_str).ok()?;
        if let Some(caps) = re.captures(line) {
            let val = caps
                .get(1)
                .map(|m| m.as_str())
                .unwrap_or_else(|| caps.get(0).unwrap().as_str());
            return Some(val.to_string());
        }
    }
    // Try JSON pick (ndjson mode).
    if matches!(parser, Parser::Ndjson) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            return apply_json_pick(&v, spec);
        }
    }
    None
}

fn apply_json_pick(v: &serde_json::Value, spec: &CaptureSpec) -> Option<String> {
    // Check match_fields guards.
    if !spec.match_fields.is_empty() {
        let obj = v.as_object()?;
        for (k, expected) in &spec.match_fields {
            let actual = obj.get(k)?.as_str().unwrap_or("");
            if actual != expected {
                return None;
            }
        }
    }
    // Extract pick path.
    if let Some(pick) = &spec.pick {
        return extract_json_path(v, pick);
    }
    // No pick: return raw regex match if any.
    None
}

fn extract_json_path(v: &serde_json::Value, path: &str) -> Option<String> {
    let mut cur = v;
    for seg in path.split('.').filter(|s| !s.is_empty()) {
        if let Ok(idx) = seg.parse::<usize>() {
            cur = cur.get(idx)?;
        } else {
            cur = cur.get(seg)?;
        }
    }
    Some(match cur {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

/// Collect a `StepResultRecord` from `StepResults` capture mappings.
fn collect_step_results(
    step_id: &str,
    results: &StepResults,
    captures: &CaptureStore,
) -> Option<StepResultRecord> {
    let resolve = |opt: &Option<String>| -> Option<String> {
        let key = opt.as_deref()?;
        captures.get(key)
    };
    let duration = resolve(&results.duration);
    let up_bytes = resolve(&results.up_bytes);
    let down_bytes = resolve(&results.down_bytes);

    if duration.is_none() && up_bytes.is_none() && down_bytes.is_none() {
        return None;
    }
    Some(StepResultRecord {
        id: step_id.to_string(),
        duration,
        up_bytes,
        down_bytes,
    })
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
    // Only set RUST_LOG if NETSIM_RUST_LOG is set; otherwise leave to process.
    if let Ok(rust_log) = std::env::var("NETSIM_RUST_LOG") {
        cmd.env("RUST_LOG", rust_log);
    }
    for (k, v) in extra_env {
        cmd.env(k, state.env.interpolate_str(v)?);
    }
    Ok(cmd)
}

pub(crate) fn spawn_pipe_pump<R: Read + Send + 'static>(
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

fn parse_link_condition(value: &Option<toml::Value>) -> Result<Option<LinkCondition>> {
    match value {
        None => Ok(None),
        Some(v) => {
            let cond: LinkCondition = v
                .clone()
                .try_into()
                .map_err(|e: toml::de::Error| anyhow!("{}", e))?;
            Ok(Some(cond))
        }
    }
}

fn evaluate_assert(state: &SimState, check: &str) -> Result<()> {
    let (lhs, op, rhs) = if let Some(idx) = check.find(" == ") {
        (check[..idx].trim(), "==", check[idx + 4..].trim())
    } else if let Some(idx) = check.find(" != ") {
        (check[..idx].trim(), "!=", check[idx + 4..].trim())
    } else if let Some(idx) = check.find(" contains ") {
        (check[..idx].trim(), "contains", check[idx + 10..].trim())
    } else if let Some(idx) = check.find(" matches ") {
        (check[..idx].trim(), "matches", check[idx + 9..].trim())
    } else {
        bail!("assert: unrecognised check expression: {:?}", check);
    };

    let lhs_val = resolve_assert_lhs(state, lhs)?;
    let pass = match op {
        "==" => lhs_val == rhs,
        "!=" => lhs_val != rhs,
        "contains" => lhs_val.contains(rhs),
        "matches" => regex::Regex::new(rhs)
            .with_context(|| format!("assert: compile regex {:?}", rhs))?
            .is_match(&lhs_val),
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
    // Try CaptureStore first.
    if let Some(v) = state.captures.get(lhs) {
        return Ok(v);
    }
    // Try legacy env captures.
    if let Some(v) = state.env.get_capture(lhs) {
        return Ok(v.to_string());
    }
    bail!(
        "assert: cannot resolve '{}' — not a capture or known result field",
        lhs
    );
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
        .join(patchbay::util::sanitize_for_path_component(node));
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
