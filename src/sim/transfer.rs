//! Handles the `kind = "iroh-transfer"` spawn step.

use anyhow::{anyhow, bail, Context, Result};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
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
}

/// In-progress transfer started by a `spawn` step.
pub struct TransferHandle {
    id: String,
    provider: String,
    provider_child: std::process::Child,
    provider_parse_log: PathBuf,
    fetchers: Vec<FetcherHandle>,
}

/// Start a transfer and return a handle that is finalized in `wait-for`.
pub fn start_transfer(state: &mut SimState, step: &Step, binary: &Path) -> Result<TransferHandle> {
    let step_id = step.id.as_deref().context("iroh-transfer: missing id")?;
    let provider_dev = step
        .provider
        .as_deref()
        .context("iroh-transfer: missing provider")?;
    let fetcher_devs = resolve_fetchers(step)?;

    let provider_logs_dir = node_transfer_dir(&state.work_dir, provider_dev, step_id, "provider");
    std::fs::create_dir_all(&provider_logs_dir)
        .with_context(|| format!("create provider logs dir {}", provider_logs_dir.display()))?;
    let provider_stdio_log = provider_logs_dir.join("out.log");

    let mut provider_cmd = std::process::Command::new(binary);
    let mut provider_args = vec![
        "--output".to_string(),
        "json".to_string(),
        "--logs-path".to_string(),
        provider_logs_dir.display().to_string(),
        "provide".to_string(),
    ];
    provider_cmd
        .args(["--output", "json", "--logs-path"])
        .arg(&provider_logs_dir)
        .arg("provide");
    let p_log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&provider_stdio_log)
        .with_context(|| format!("open provider stdio log {}", provider_stdio_log.display()))?;
    let p_log2 = p_log.try_clone().context("clone provider stdio log")?;
    provider_cmd
        .stdout(Stdio::from(p_log))
        .stderr(Stdio::from(p_log2));

    add_env_to_cmd(&mut provider_cmd, state, &format!("{}_provider", step_id));
    if let Some(relay_url) = &step.relay_url {
        let url = state.env.interpolate_str(relay_url)?;
        if should_use_transfer_dev_env(&url) {
            provider_cmd.args(["--env", "dev"]);
            provider_args.push("--env".to_string());
            provider_args.push("dev".to_string());
        }
        provider_cmd.args(["--relay-url", &url]);
        provider_args.push("--relay-url".to_string());
        provider_args.push(url);
    }
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

    wait_for_file(&provider_stdio_log, 30)?;
    let bound = read_until_endpoint_bound(&provider_stdio_log, 30)?
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
        let fetcher_stdio_log = fetcher_log.join("out.log");

        let mut fetcher_cmd = std::process::Command::new(binary);
        let mut fetcher_args = vec![
            "--output".to_string(),
            "json".to_string(),
            "--logs-path".to_string(),
            fetcher_log.display().to_string(),
            "fetch".to_string(),
        ];
        fetcher_cmd
            .args(["--output", "json", "--logs-path"])
            .arg(&fetcher_log)
            .arg("fetch");
        if step.strategy.as_deref() == Some("endpoint_id_with_direct_addrs") {
            if let Some(addr) = &bound.direct_addr {
                fetcher_cmd.args(["--remote-direct-address", addr]);
                fetcher_args.push("--remote-direct-address".to_string());
                fetcher_args.push(addr.clone());
            }
        }
        fetcher_cmd.arg(&bound.endpoint_id);
        fetcher_args.push(bound.endpoint_id.clone());
        if let Some(relay_url) = &step.relay_url {
            let url = state.env.interpolate_str(relay_url)?;
            if should_use_transfer_dev_env(&url) {
                fetcher_cmd.args(["--env", "dev"]);
                fetcher_args.push("--env".to_string());
                fetcher_args.push("dev".to_string());
            }
            fetcher_cmd.args(["--remote-relay-url", &url]);
            fetcher_cmd.args(["--relay-url", &url]);
            fetcher_args.push("--remote-relay-url".to_string());
            fetcher_args.push(url.clone());
            fetcher_args.push("--relay-url".to_string());
            fetcher_args.push(url);
        }
        if let Some(extra) = &step.fetch_args {
            let extra = state.env.interpolate(extra)?;
            fetcher_cmd.args(extra.clone());
            fetcher_args.extend(extra);
        }
        let f_log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&fetcher_stdio_log)
            .with_context(|| format!("open fetcher stdio log {}", fetcher_stdio_log.display()))?;
        let f_log2 = f_log.try_clone().context("clone fetcher stdio log")?;
        fetcher_cmd
            .stdout(Stdio::from(f_log))
            .stderr(Stdio::from(f_log2));
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
        fetchers.push(FetcherHandle {
            name: fetcher_dev.clone(),
            child,
            parse_log_path: fetcher_stdio_log,
        });
    }

    Ok(TransferHandle {
        id: step_id.to_string(),
        provider: provider_dev.to_string(),
        provider_child: provider,
        provider_parse_log: provider_stdio_log,
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

fn resolve_fetchers(step: &Step) -> Result<Vec<String>> {
    if let Some(fetchers) = &step.fetchers {
        if fetchers.is_empty() {
            bail!("iroh-transfer: fetchers must not be empty");
        }
        return Ok(fetchers.clone());
    }
    if let Some(fetcher) = &step.fetcher {
        return Ok(vec![fetcher.clone()]);
    }
    bail!("iroh-transfer: missing fetcher/fetchers");
}

fn add_env_to_cmd(cmd: &mut std::process::Command, state: &SimState, keylog_suffix: &str) {
    for (k, v) in state.env.process_env() {
        cmd.env(k, v);
    }
    cmd.env("RUST_LOG_STYLE", "never");
    let rust_log = std::env::var("NETSIM_RUST_LOG").unwrap_or_else(|_| "info".to_string());
    cmd.env("RUST_LOG", rust_log);
    let keylog = state
        .work_dir
        .join(format!("keylog_{}.txt", sanitize_for_file(keylog_suffix)));
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
        .join(sanitize_for_file(node))
        .join(format!(
            "transfer-{}-{}",
            sanitize_for_file(step_id),
            sanitize_for_file(role)
        ))
}

fn should_use_transfer_dev_env(relay_url: &str) -> bool {
    relay_url.trim_start().starts_with("http://")
}

fn sanitize_for_file(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
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
    fn transfer_dev_env_selected_for_plain_http_relay_url() {
        assert!(should_use_transfer_dev_env("http://203.0.1.2:3340"));
        assert!(should_use_transfer_dev_env("  http://localhost:3340"));
        assert!(!should_use_transfer_dev_env("https://relay.example.com"));
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
