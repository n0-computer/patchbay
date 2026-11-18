use std::{path::Path, time::SystemTime};

use anyhow::{Context, Result};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RunEnvironment {
    pub(crate) os: String,
    pub(crate) arch: String,
    pub(crate) family: String,
    pub(crate) current_dir: String,
    pub(crate) executable: String,
    pub(crate) rust_log: Option<String>,
    pub(crate) patchbay_version: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ManifestSimSummary {
    pub(crate) sim: String,
    pub(crate) sim_dir: String,
    pub(crate) status: String,
    pub(crate) runtime_ms: Option<u128>,
    pub(crate) sim_json: Option<String>,
    pub(crate) error: Option<String>,
}

/// Per-sim execution report written to `manifest.json`.
///
/// Distinct from [`patchbay_utils::manifest::RunManifest`] which is the
/// unified `run.json` metadata (git context, outcome, etc.).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SimRunReport {
    pub(crate) run: String,
    pub(crate) started_at: String,
    pub(crate) status: String,
    pub(crate) ended_at: Option<String>,
    pub(crate) runtime_ms: Option<u128>,
    pub(crate) success: Option<bool>,
    pub(crate) environment: RunEnvironment,
    pub(crate) simulations: Vec<ManifestSimSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProgressSim {
    pub(crate) sim: String,
    pub(crate) status: String,
    pub(crate) sim_dir: Option<String>,
    pub(crate) runtime_ms: Option<u128>,
    pub(crate) sim_json: Option<String>,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RunProgress {
    pub(crate) run: String,
    pub(crate) status: String,
    pub(crate) started_at: String,
    pub(crate) updated_at: String,
    pub(crate) total: usize,
    pub(crate) completed: usize,
    pub(crate) ok: usize,
    pub(crate) error: usize,
    pub(crate) current_sim: Option<String>,
    pub(crate) simulations: Vec<ProgressSim>,
}

pub(crate) fn format_timestamp(ts: SystemTime) -> String {
    time::OffsetDateTime::from(ts)
        .format(time::macros::format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second]Z"
        ))
        .expect("format timestamp")
}

pub(crate) fn now_stamp() -> String {
    time::OffsetDateTime::now_utc()
        .format(time::macros::format_description!(
            "[year repr:last_two][month][day]-[hour][minute][second]"
        ))
        .expect("format timestamp")
}

pub(crate) async fn write_sim_report(run_root: &Path, report: &SimRunReport) -> Result<()> {
    write_json(run_root.join("manifest.json"), report).await
}

pub(crate) async fn write_progress(run_root: &Path, progress: &RunProgress) -> Result<()> {
    write_json(run_root.join("progress.json"), progress).await
}

/// Serialize `value` as pretty JSON and write it atomically to `path`.
pub(crate) async fn write_json(path: impl AsRef<Path>, value: &impl Serialize) -> Result<()> {
    let path = path.as_ref();
    let text = serde_json::to_string_pretty(value)
        .with_context(|| format!("serialize {}", path.display()))?;
    tokio::fs::write(path, text)
        .await
        .with_context(|| format!("write {}", path.display()))
}

pub(crate) fn collect_run_environment() -> Result<RunEnvironment> {
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
        patchbay_version: env!("CARGO_PKG_VERSION").to_string(),
    })
}
