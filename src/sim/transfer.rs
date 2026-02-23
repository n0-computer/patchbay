//! Handles the `kind = "iroh-transfer"` spawn step.

use anyhow::{anyhow, bail, Context, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use crate::sim::report::TransferResult;
use crate::sim::runner::SimState;
use crate::sim::Step;

#[derive(Debug, Clone)]
struct EndpointBoundInfo {
    endpoint_id: String,
    direct_addr: Option<String>,
}

struct FetcherHandle {
    name: String,
    child: std::process::Child,
    parse_log_path: PathBuf,
    stdout_pump: Option<thread::JoinHandle<Result<()>>>,
    stderr_pump: Option<thread::JoinHandle<Result<()>>>,
}

/// In-progress transfer started by a `spawn` step.
pub struct TransferHandle {
    id: String,
    provider: String,
    provider_child: std::process::Child,
    provider_parse_log: PathBuf,
    provider_stdout_pump: Option<thread::JoinHandle<Result<()>>>,
    provider_stderr_pump: Option<thread::JoinHandle<Result<()>>>,
    fetchers: Vec<FetcherHandle>,
}

/// Start a transfer and return a handle that is finalized in `wait-for`.
pub fn start_transfer(state: &mut SimState, step: &Step, binary: &Path) -> Result<TransferHandle> {
    let Step::Spawn {
        id,
        kind,
        provider,
        fetcher,
        fetchers,
        relay_url,
        fetch_args,
        strategy,
        ..
    } = step
    else {
        bail!("iroh-transfer: expected spawn step");
    };
    if kind.as_deref() != Some("iroh-transfer") {
        bail!("iroh-transfer: expected spawn kind");
    }
    let step_id = id.as_str();
    let provider_dev = provider
        .as_deref()
        .context("iroh-transfer: missing provider")?;
    let fetcher_devs = resolve_fetchers(fetcher, fetchers)?;

    let provider_logs_dir = node_transfer_dir(&state.work_dir, provider_dev, step_id, "provider");
    std::fs::create_dir_all(&provider_logs_dir)
        .with_context(|| format!("create provider logs dir {}", provider_logs_dir.display()))?;
    let provider_stdout_log = provider_logs_dir.join("stdout.log");
    let provider_stderr_log = provider_logs_dir.join("stderr.log");

    let mut provider_cmd = std::process::Command::new(binary);
    provider_cmd.args(["--output", "json", "provide"]);
    provider_cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    add_env_to_cmd(&mut provider_cmd, state, &format!("{}_provider", step_id));
    provider_cmd.args(["--env", "dev"]);
    if let Some(relay_url) = relay_url {
        let url = state.env.interpolate_str(relay_url)?;
        provider_cmd.args(["--relay-url", &url]);
    }
    let provider_args: Vec<String> = provider_cmd
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    tracing::info!(
        step_id,
        device = provider_dev,
        cmd = %format_cmd(binary, &provider_args),
        "sim: iroh-transfer provider command"
    );

    let provider = state
        .lab
        .spawn_unmanaged_on(provider_dev, provider_cmd)
        .context("spawn provider")?;
    let mut provider = provider;
    let provider_stdout = provider.stdout.take().context("take provider stdout")?;
    let provider_stderr = provider.stderr.take().context("take provider stderr")?;
    let provider_stdout_pump = spawn_pipe_pump(
        provider_stdout,
        provider_stdout_log.clone(),
        verbose_prefix(provider_dev, "out"),
        state.verbose,
    );
    let provider_stderr_pump = spawn_pipe_pump(
        provider_stderr,
        provider_stderr_log.clone(),
        verbose_prefix(provider_dev, "err"),
        state.verbose,
    );

    wait_for_file(&provider_stdout_log, 30)?;
    let bound = read_until_endpoint_bound(&provider_stdout_log, 30)?
        .ok_or_else(|| anyhow!("EOF before EndpointBound in provider log"))?;
    tracing::info!(
        step_id,
        endpoint_id = %bound.endpoint_id,
        direct_addr = ?bound.direct_addr,
        "iroh-transfer: provider ready"
    );
    state
        .env
        .set_capture(step_id, "endpoint_id", bound.endpoint_id.clone());

    let mut fetchers = Vec::with_capacity(fetcher_devs.len());
    for (idx, fetcher_dev) in fetcher_devs.iter().enumerate() {
        let fetcher_log = node_transfer_dir(
            &state.work_dir,
            fetcher_dev,
            step_id,
            &format!("fetcher-{}", idx),
        );
        std::fs::create_dir_all(&fetcher_log)
            .with_context(|| format!("create fetcher logs dir {}", fetcher_log.display()))?;
        let fetcher_stdout_log = fetcher_log.join("stdout.log");
        let fetcher_stderr_log = fetcher_log.join("stderr.log");

        let mut fetcher_cmd = std::process::Command::new(binary);
        let mut fetcher_args = vec![
            "--output".to_string(),
            "json".to_string(),
            "fetch".to_string(),
        ];
        fetcher_cmd.args(["--output", "json", "fetch"]);
        if strategy.as_deref() == Some("endpoint_id_with_direct_addrs") {
            if let Some(addr) = &bound.direct_addr {
                fetcher_cmd.args(["--remote-direct-address", addr]);
                fetcher_args.push("--remote-direct-address".to_string());
                fetcher_args.push(addr.clone());
            }
        }
        fetcher_cmd.arg(&bound.endpoint_id);
        fetcher_args.push(bound.endpoint_id.clone());
        fetcher_cmd.args(["--env", "dev"]);
        fetcher_args.push("--env".to_string());
        fetcher_args.push("dev".to_string());
        if let Some(relay_url) = relay_url {
            let url = state.env.interpolate_str(relay_url)?;
            fetcher_cmd.args(["--remote-relay-url", &url]);
            fetcher_cmd.args(["--relay-url", &url]);
            fetcher_args.push("--remote-relay-url".to_string());
            fetcher_args.push(url.clone());
            fetcher_args.push("--relay-url".to_string());
            fetcher_args.push(url);
        }
        if let Some(extra) = fetch_args {
            let extra = state.env.interpolate(extra)?;
            fetcher_cmd.args(extra.clone());
            fetcher_args.extend(extra);
        }
        fetcher_cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        add_env_to_cmd(
            &mut fetcher_cmd,
            state,
            &format!("{}_fetcher_{}", step_id, idx),
        );
        tracing::info!(
            step_id,
            device = %fetcher_dev,
            cmd = %format_cmd(binary, &fetcher_args),
            "sim: iroh-transfer fetcher command"
        );

        let child = state
            .lab
            .spawn_unmanaged_on(fetcher_dev, fetcher_cmd)
            .with_context(|| format!("spawn fetcher '{}'", fetcher_dev))?;
        let mut child = child;
        let fetch_stdout = child.stdout.take().context("take fetcher stdout")?;
        let fetch_stderr = child.stderr.take().context("take fetcher stderr")?;
        let stdout_pump = spawn_pipe_pump(
            fetch_stdout,
            fetcher_stdout_log.clone(),
            verbose_prefix(fetcher_dev, "out"),
            state.verbose,
        );
        let stderr_pump = spawn_pipe_pump(
            fetch_stderr,
            fetcher_stderr_log,
            verbose_prefix(fetcher_dev, "err"),
            state.verbose,
        );
        fetchers.push(FetcherHandle {
            name: fetcher_dev.clone(),
            child,
            parse_log_path: fetcher_stdout_log,
            stdout_pump: Some(stdout_pump),
            stderr_pump: Some(stderr_pump),
        });
    }

    Ok(TransferHandle {
        id: step_id.to_string(),
        provider: provider_dev.to_string(),
        provider_child: provider,
        provider_parse_log: provider_stdout_log,
        provider_stdout_pump: Some(provider_stdout_pump),
        provider_stderr_pump: Some(provider_stderr_pump),
        fetchers,
    })
}

/// Finalize a transfer started earlier by [`start_transfer`].
pub fn finish_transfer(
    mut handle: TransferHandle,
    timeout: Duration,
) -> Result<Vec<TransferResult>> {
    let deadline = Instant::now() + timeout;

    for fetcher in &mut handle.fetchers {
        wait_for_child_with_timeout(&mut fetcher.child, deadline)
            .with_context(|| format!("wait fetcher '{}'", fetcher.name))?;
        if let Some(h) = fetcher.stdout_pump.take() {
            join_pump(h, "fetcher stdout pump")?;
        }
        if let Some(h) = fetcher.stderr_pump.take() {
            join_pump(h, "fetcher stderr pump")?;
        }
    }

    let remain = deadline.saturating_duration_since(Instant::now());
    let _ = read_until_path_stats(&handle.provider_parse_log, remain);
    #[cfg(unix)]
    {
        let pid = handle.provider_child.id();
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGINT,
        );
    }
    let _ = handle.provider_child.wait();
    if let Some(h) = handle.provider_stdout_pump.take() {
        join_pump(h, "provider stdout pump")?;
    }
    if let Some(h) = handle.provider_stderr_pump.take() {
        join_pump(h, "provider stderr pump")?;
    }

    let mut results = Vec::with_capacity(handle.fetchers.len());
    for fetcher in &handle.fetchers {
        let mut result = TransferResult {
            id: if handle.fetchers.len() == 1 {
                handle.id.clone()
            } else {
                format!("{}.{}", handle.id, fetcher.name)
            },
            provider: handle.provider.clone(),
            fetcher: fetcher.name.clone(),
            ..Default::default()
        };
        if fetcher.parse_log_path.exists() {
            result.parse_fetcher_log(&fetcher.parse_log_path)?;
        }
        results.push(result);
    }
    Ok(results)
}

fn spawn_pipe_pump<R: Read + Send + 'static>(
    reader: R,
    path: PathBuf,
    prefix: String,
    verbose: bool,
) -> thread::JoinHandle<Result<()>> {
    thread::spawn(move || -> Result<()> {
        let mut out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open append log {}", path.display()))?;
        let mut reader = BufReader::new(reader);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let n = reader
                .read_until(b'\n', &mut buf)
                .with_context(|| format!("read pipe for {}", path.display()))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf)
                .with_context(|| format!("append log {}", path.display()))?;
            if verbose {
                let line = String::from_utf8_lossy(&buf)
                    .trim_end_matches('\n')
                    .to_string();
                println!("{prefix} {line}");
            }
        }
        Ok(())
    })
}

fn join_pump(handle: thread::JoinHandle<Result<()>>, label: &str) -> Result<()> {
    handle
        .join()
        .map_err(|_| anyhow!("{label} panicked"))?
        .with_context(|| label.to_string())
}

fn verbose_prefix(device: &str, stream: &str) -> String {
    let mut dev: String = device.chars().take(10).collect();
    let cur = dev.chars().count();
    if cur < 10 {
        dev.push_str(&" ".repeat(10 - cur));
    }
    format!("{dev}{stream}")
}

fn resolve_fetchers(
    fetcher: &Option<String>,
    fetchers: &Option<Vec<String>>,
) -> Result<Vec<String>> {
    if let Some(fetchers) = fetchers {
        if fetchers.is_empty() {
            bail!("iroh-transfer: fetchers must not be empty");
        }
        return Ok(fetchers.clone());
    }
    if let Some(fetcher) = fetcher {
        return Ok(vec![fetcher.clone()]);
    }
    bail!("iroh-transfer: missing fetcher/fetchers");
}

fn add_env_to_cmd(cmd: &mut std::process::Command, state: &SimState, keylog_suffix: &str) {
    for (k, v) in state.env.process_env() {
        cmd.env(k, v);
    }
    cmd.env("RUST_LOG_STYLE", "never");
    let rust_log = std::env::var("NETSIM_RUST_LOG")
        .unwrap_or_else(|_| "iroh=info,iroh::_events=debug".to_string());
    cmd.env("RUST_LOG", rust_log);
    let keylog = state.work_dir.join(format!(
        "keylog_{}.txt",
        netsim::util::sanitize_for_path_component(keylog_suffix)
    ));
    cmd.env("SSLKEYLOGFILE", keylog);
}

fn format_cmd(binary: &Path, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(binary.display().to_string());
    parts.extend(args.iter().cloned());
    shell_join(&parts)
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

fn wait_for_child_with_timeout(child: &mut std::process::Child, deadline: Instant) -> Result<()> {
    loop {
        if let Some(status) = child.try_wait().context("try_wait child")? {
            if !status.success() {
                tracing::warn!(?status, "fetcher exited with non-zero status");
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("transfer wait timed out");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn wait_for_file(path: &Path, timeout_secs: u64) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if path.exists() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn read_until_endpoint_bound(path: &Path, timeout_secs: u64) -> Result<Option<EndpointBoundInfo>> {
    tail_until(
        path,
        Duration::from_secs(timeout_secs),
        parse_endpoint_bound_line,
    )
    .context("waiting for EndpointBound")
}

fn read_until_path_stats(path: &Path, timeout: Duration) -> Result<()> {
    let _ = tail_until(path, timeout, |line| {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        if v.get("kind")?.as_str()? == "PathStats" {
            Some(())
        } else {
            None
        }
    })?;
    Ok(())
}

fn tail_until<F, R>(path: &Path, timeout: Duration, mut f: F) -> Result<Option<R>>
where
    F: FnMut(&str) -> Option<R>,
{
    let deadline = Instant::now() + timeout;
    let file =
        std::fs::File::open(path).with_context(|| format!("open log file {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(_) => {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    if let Some(result) = f(trimmed) {
                        return Ok(Some(result));
                    }
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn parse_endpoint_bound_line(line: &str) -> Option<EndpointBoundInfo> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("kind")?.as_str()? != "EndpointBound" {
        return None;
    }
    let endpoint_id = v.get("endpoint_id")?.as_str()?.to_string();
    let direct_addr = v
        .get("direct_addresses")
        .and_then(|a| a.as_array())
        .and_then(|arr| arr.first())
        .and_then(|x| x.as_str())
        .map(ToString::to_string);
    Some(EndpointBoundInfo {
        endpoint_id,
        direct_addr,
    })
}

fn node_transfer_dir(work_dir: &Path, node: &str, step_id: &str, role: &str) -> PathBuf {
    work_dir
        .join("nodes")
        .join(netsim::util::sanitize_for_path_component(node))
        .join(format!(
            "transfer-{}-{}",
            netsim::util::sanitize_for_path_component(step_id),
            netsim::util::sanitize_for_path_component(role)
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_has_qadv4_timeout(text: &str) -> bool {
        text.lines()
            .any(|line| line.contains("QADv4") && line.contains("probe timed out"))
    }

    fn log_generated_report_has_no_global_v4(text: &str) -> bool {
        text.lines()
            .filter(|line| line.contains("iroh::net_report") && line.contains("generated report"))
            .any(|line| line.contains("global_v4: None"))
    }

    fn relay_log_shows_quic_disabled(text: &str) -> bool {
        text.contains("ServerConfig {") && text.contains("quic: None")
    }

    #[test]
    fn parse_endpoint_bound_with_direct_addr() {
        let line =
            r#"{"kind":"EndpointBound","endpoint_id":"abc","direct_addresses":["1.2.3.4:7777"]}"#;
        let parsed = parse_endpoint_bound_line(line).expect("endpoint bound");
        assert_eq!(parsed.endpoint_id, "abc");
        assert_eq!(parsed.direct_addr.as_deref(), Some("1.2.3.4:7777"));
    }

    #[test]
    fn parse_endpoint_bound_without_direct_addr() {
        let line = r#"{"kind":"EndpointBound","endpoint_id":"abc"}"#;
        let parsed = parse_endpoint_bound_line(line).expect("endpoint bound");
        assert_eq!(parsed.endpoint_id, "abc");
        assert!(parsed.direct_addr.is_none());
    }

    #[test]
    fn net_report_qad_timeout_matches_relay_without_quic() {
        let provider_net_report_log = r#"
2026-02-20T13:24:06.151668Z DEBUG iroh::net_report: v4 QAD probe relay.url=RelayUrl("http://203.0.1.2:3340/")
2026-02-20T13:24:09.152654Z DEBUG QADv4: iroh::net_report: probe timed out
2026-02-20T13:24:09.152696Z DEBUG iroh::net_report: generated report in 3001ms report=Report { udp_v4: false, global_v4: None, global_v6: None }
"#;
        let relay_log = r#"
2026-02-20T13:24:04.067558Z DEBUG iroh_relay: ServerConfig {
    relay: Some(...),
    quic: None,
}
"#;

        assert!(log_has_qadv4_timeout(provider_net_report_log));
        assert!(log_generated_report_has_no_global_v4(
            provider_net_report_log
        ));
        assert!(relay_log_shows_quic_disabled(relay_log));
    }
}
