//! Handles the `kind = "iroh-transfer"` spawn step.
//!
//! Execution sequence (synchronous / blocking for the MVP):
//! 1. Spawn provider subprocess in provider's netns; pipe stdout.
//! 2. Read provider stdout until `EndpointBound` → extract `endpoint_id`.
//! 3. Spawn fetcher subprocess with `endpoint_id`; pipe stdout.
//! 4. Wait for fetcher process to exit.
//! 5. Send SIGINT to provider; drain its stdout.
//! 6. Parse fetcher log file for DownloadComplete + ConnectionTypeChanged.
//! 7. Return `TransferResult`.

use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::Stdio;
use std::thread;

use crate::sim::report::TransferResult;
use crate::sim::runner::SimState;
use crate::sim::Step;

#[derive(Debug, Clone)]
struct EndpointBoundInfo {
    endpoint_id: String,
    direct_addr: Option<String>,
}

pub struct TransferHandle {
    pub join: thread::JoinHandle<Result<TransferResult>>,
}

pub fn start_transfer(
    state: &mut SimState,
    step: &Step,
    log_dir: &Path,
    binary: &Path,
) -> Result<TransferHandle> {
    // Compile-first behavior: execute immediately and hand back a completed join handle.
    let result = run_transfer(state, step, log_dir, binary)?;
    let join = thread::spawn(move || Ok(result));
    Ok(TransferHandle { join })
}

pub fn run_transfer(
    state: &mut SimState,
    step: &Step,
    log_dir: &Path,
    binary: &Path,
) -> Result<TransferResult> {
    let step_id = step.id.as_deref().context("iroh-transfer: missing id")?;
    let provider_dev = step
        .provider
        .as_deref()
        .context("iroh-transfer: missing provider")?;
    let fetcher_dev = step
        .fetcher
        .as_deref()
        .context("iroh-transfer: missing fetcher")?;

    let provider_log = log_dir.join(format!("xfer_{}_provider.ndjson", step_id));
    let fetcher_log = log_dir.join(format!("xfer_{}_fetcher.ndjson", step_id));

    // ── 1. Spawn provider ────────────────────────────────────────────────
    let mut provider_cmd = std::process::Command::new(binary);
    provider_cmd
        .args(["--output", "json", "--log-path"])
        .arg(&provider_log)
        .arg("provide")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    add_env_to_cmd(&mut provider_cmd, state);
    if let Some(relay_url) = &step.relay_url {
        let url = state.env.interpolate_str(relay_url)?;
        provider_cmd.args(["--relay-url", &url]);
    }

    let mut provider = state
        .lab
        .spawn_unmanaged_on(provider_dev, provider_cmd)
        .context("spawn provider")?;

    // Wait for the log file to appear (binary writes it before EndpointBound).
    wait_for_file(&provider_log, 30)?;

    // ── 2. Read provider log until EndpointBound ─────────────────────────
    let bound = read_until_endpoint_bound(&provider_log, 30)?;
    tracing::info!(
        step_id,
        endpoint_id = %bound.endpoint_id,
        direct_addr = ?bound.direct_addr,
        "iroh-transfer: provider ready"
    );

    // Store capture so later steps can use `${step_id.endpoint_id}`.
    state
        .env
        .set_capture(step_id, "endpoint_id", bound.endpoint_id.clone());

    // ── 3. Spawn fetcher ─────────────────────────────────────────────────
    let mut fetcher_cmd = std::process::Command::new(binary);
    fetcher_cmd
        .args(["--output", "json", "--log-path"])
        .arg(&fetcher_log)
        .arg("fetch");
    if step.strategy.as_deref() == Some("endpoint_id_with_direct_addrs") {
        if let Some(addr) = &bound.direct_addr {
            fetcher_cmd.args(["--remote-direct-address", addr]);
        }
    }
    fetcher_cmd.arg(&bound.endpoint_id);
    if let Some(relay_url) = &step.relay_url {
        let url = state.env.interpolate_str(relay_url)?;
        fetcher_cmd.args(["--relay-url", &url]);
    }
    if let Some(extra) = &step.fetch_args {
        let extra = state.env.interpolate(extra)?;
        fetcher_cmd.args(extra);
    }
    fetcher_cmd.stdout(Stdio::null()).stderr(Stdio::null());

    add_env_to_cmd(&mut fetcher_cmd, state);

    let mut fetcher = state
        .lab
        .spawn_unmanaged_on(fetcher_dev, fetcher_cmd)
        .context("spawn fetcher")?;

    // ── 4. Wait for fetcher to exit ──────────────────────────────────────
    let status = fetcher.wait().context("wait fetcher")?;
    if !status.success() {
        tracing::warn!(step_id, ?status, "fetcher exited with non-zero status");
    }

    // ── 5. Wait for provider PathStats then SIGINT ────────────────────────
    let _ = read_until_path_stats(&provider_log, 60);
    #[cfg(unix)]
    {
        let pid = provider.id();
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGINT,
        );
    }
    let _ = provider.wait();

    // ── 6 + 7. Parse fetcher log, return result ───────────────────────────
    let mut result = TransferResult {
        id: step_id.to_string(),
        provider: provider_dev.to_string(),
        fetcher: fetcher_dev.to_string(),
        ..Default::default()
    };
    if fetcher_log.exists() {
        result.parse_fetcher_log(&fetcher_log)?;
    }
    Ok(result)
}

// ─────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────

fn add_env_to_cmd(cmd: &mut std::process::Command, state: &SimState) {
    for (k, v) in state.env.process_env() {
        cmd.env(k, v);
    }
    cmd.env("RUST_LOG_STYLE", "never");
}

/// Poll until `path` exists (up to `timeout_secs`).
fn wait_for_file(path: &Path, timeout_secs: u64) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        if path.exists() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!("timed out waiting for {}", path.display());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Tail `path` until a line with `"kind":"EndpointBound"` is found.
/// Returns endpoint metadata from the first `EndpointBound` line.
fn read_until_endpoint_bound(path: &Path, timeout_secs: u64) -> Result<EndpointBoundInfo> {
    tail_until(path, timeout_secs, |line| parse_endpoint_bound_line(line))
        .context("waiting for EndpointBound")?
        .ok_or_else(|| anyhow::anyhow!("EOF before EndpointBound in provider log"))
}

/// Tail `path` until a line with `"kind":"PathStats"` is found.
fn read_until_path_stats(path: &Path, timeout_secs: u64) -> Result<()> {
    tail_until(path, timeout_secs, |line| {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        if v.get("kind")?.as_str()? == "PathStats" {
            Some(())
        } else {
            None
        }
    })
    .map(|_| ())
}

/// Generic log-file tailer: read new lines as they appear and call `f` on each.
/// Returns `Ok(Some(R))` when `f` returns `Some(R)`, or `Ok(None)` on EOF/timeout.
fn tail_until<F, R>(path: &Path, timeout_secs: u64, mut f: F) -> Result<Option<R>>
where
    F: FnMut(&str) -> Option<R>,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let file =
        std::fs::File::open(path).with_context(|| format!("open log file {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                // EOF — check for timeout, then sleep and retry.
                if std::time::Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_endpoint_bound_with_direct_addr() {
        let line =
            r#"{"kind":"EndpointBound","endpoint_id":"abc","direct_addresses":["1.2.3.4:7777"]}"#;
        let parsed = parse_endpoint_bound_line(line).unwrap();
        assert_eq!(parsed.endpoint_id, "abc");
        assert_eq!(parsed.direct_addr.as_deref(), Some("1.2.3.4:7777"));
    }

    #[test]
    fn parse_endpoint_bound_without_direct_addr() {
        let line = r#"{"kind":"EndpointBound","endpoint_id":"abc"}"#;
        let parsed = parse_endpoint_bound_line(line).unwrap();
        assert_eq!(parsed.endpoint_id, "abc");
        assert!(parsed.direct_addr.is_none());
    }
}
