use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;
use std::time::SystemTime;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RunEnvironment {
    pub(crate) os: String,
    pub(crate) arch: String,
    pub(crate) family: String,
    pub(crate) current_dir: String,
    pub(crate) executable: String,
    pub(crate) rust_log: Option<String>,
    pub(crate) netsim_version: String,
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

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RunManifest {
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
    chrono::DateTime::<chrono::Utc>::from(ts)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

pub(crate) fn now_stamp() -> String {
    chrono::Utc::now().format("%y%m%d-%H%M%S").to_string()
}

pub(crate) async fn write_run_manifest(run_root: &Path, manifest: &RunManifest) -> Result<()> {
    let text = serde_json::to_string_pretty(manifest).context("serialize run manifest")?;
    tokio::fs::write(run_root.join("manifest.json"), text)
        .await
        .with_context(|| format!("write {}", run_root.join("manifest.json").display()))?;
    Ok(())
}

pub(crate) async fn write_progress(run_root: &Path, progress: &RunProgress) -> Result<()> {
    let text = serde_json::to_string_pretty(progress).context("serialize run progress")?;
    tokio::fs::write(run_root.join("progress.json"), text)
        .await
        .with_context(|| format!("write {}", run_root.join("progress.json").display()))?;
    Ok(())
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
        netsim_version: env!("CARGO_PKG_VERSION").to_string(),
    })
}
