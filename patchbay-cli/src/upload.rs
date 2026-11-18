//! Upload run/compare directories to a patchbay-server instance.

use std::path::Path;

use anyhow::{bail, Context, Result};
use patchbay_utils::manifest::{RunKind, RunManifest};

/// Build a RunManifest from CI environment variables.
pub fn manifest_from_env(project: &str) -> RunManifest {
    RunManifest {
        kind: RunKind::Sim, // default; overridden if run.json already exists
        project: Some(project.to_string()),
        branch: std::env::var("GITHUB_REF_NAME")
            .ok()
            .or_else(|| std::env::var("GITHUB_HEAD_REF").ok()),
        commit: std::env::var("GITHUB_SHA").ok(),
        pr: std::env::var("GITHUB_PR_NUMBER")
            .ok()
            .and_then(|s| s.parse().ok()),
        pr_url: None,
        title: std::env::var("GITHUB_PR_TITLE").ok(),
        outcome: None,
        started_at: Some(time::OffsetDateTime::now_utc()),
        ended_at: None,
        runtime: None,
        dirty: false,
        pass: None,
        fail: None,
        total: None,
        tests: Vec::new(),
        os: None,
        arch: None,
        patchbay_version: None,
    }
}

/// Create a tar.gz archive of a directory in memory.
fn tar_gz_dir(dir: &Path) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let gz = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::fast());
        let mut archive = tar::Builder::new(gz);
        archive.append_dir_all(".", dir).context("tar directory")?;
        let gz = archive.into_inner().context("finish tar")?;
        gz.finish().context("finish gzip")?;
    }
    Ok(buf)
}

/// Upload a directory to patchbay-server.
///
/// Creates a `run.json` manifest in the directory before uploading.
pub fn upload(dir: &Path, project: &str, url: &str, api_key: &str) -> Result<()> {
    // Write run.json manifest if not already present
    let manifest_path = dir.join("run.json");
    if !manifest_path.exists() {
        let manifest = manifest_from_env(project);
        let json = serde_json::to_string_pretty(&manifest)?;
        std::fs::write(&manifest_path, json).context("write run.json")?;
    }

    let body = tar_gz_dir(dir)?;
    let push_url = format!("{}/api/push/{}", url.trim_end_matches('/'), project);

    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(&push_url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/gzip")
        .body(body)
        .send()
        .context("upload request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        bail!("upload failed ({status}): {body}");
    }

    let result: serde_json::Value = resp.json().context("parse response")?;
    let base = url.trim_end_matches('/');
    if let Some(run) = result.get("run").and_then(serde_json::Value::as_str) {
        let view_url = format!("{base}/run/{run}");
        println!("{view_url}");
    }
    Ok(())
}
