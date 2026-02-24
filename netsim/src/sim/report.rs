use anyhow::{Context, Result};
use comfy_table::{presets::UTF8_FULL, Table};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

/// A step result record collected from `[step.results]` capture mappings.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StepResultRecord {
    /// Step identifier.
    pub id: String,
    /// Duration value from capture (raw string, e.g. microseconds or seconds).
    pub duration: Option<String>,
    /// Upload bytes value from capture.
    pub up_bytes: Option<String>,
    /// Download bytes value from capture.
    pub down_bytes: Option<String>,
}

impl StepResultRecord {
    /// Parse duration as microseconds (integer) or seconds (float).
    pub fn elapsed_s(&self) -> Option<f64> {
        let s = self.duration.as_deref()?;
        if let Ok(us) = s.parse::<u64>() {
            return Some(us as f64 / 1_000_000.0);
        }
        s.parse::<f64>().ok()
    }

    /// Parse down_bytes as u64.
    pub fn size_bytes(&self) -> Option<u64> {
        self.down_bytes.as_deref()?.parse().ok()
    }

    /// Compute download MB/s from bytes and duration.
    pub fn mb_s(&self) -> Option<f64> {
        let bytes = self.size_bytes()? as f64;
        let secs = self.elapsed_s()?;
        if secs > 0.0 {
            Some(bytes / (secs * 1_000_000.0))
        } else {
            None
        }
    }

    /// Compute upload MB/s from up_bytes and duration.
    pub fn up_mb_s(&self) -> Option<f64> {
        let bytes = self.up_bytes.as_deref()?.parse::<u64>().ok()? as f64;
        let secs = self.elapsed_s()?;
        if secs > 0.0 {
            Some(bytes / (secs * 1_000_000.0))
        } else {
            None
        }
    }
}

/// Write results to `<work_dir>/results.json` and `<work_dir>/results.md`.
pub async fn write_results(
    work_dir: &Path,
    sim_name: &str,
    step_results: &[StepResultRecord],
) -> Result<()> {
    if step_results.is_empty() {
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&serde_json::json!({
        "sim": sim_name,
        "steps": step_results,
    }))
    .context("serialize results")?;
    tokio::fs::write(work_dir.join("results.json"), json)
        .await
        .context("write results.json")?;

    let md = build_steps_md_table(sim_name, step_results);
    tokio::fs::write(work_dir.join("results.md"), md)
        .await
        .context("write results.md")?;

    Ok(())
}

/// Build a data-driven markdown table for step results.
/// Columns: sim, id, down_bytes, elapsed_s, mb_s, up_bytes, up_mb_s.
/// Only emits columns with at least one non-empty value.
fn build_steps_md_table(sim_name: &str, step_results: &[StepResultRecord]) -> String {
    let headers = ["sim", "id", "down_bytes", "elapsed_s", "mb_s", "up_bytes", "up_mb_s"];

    let rows: Vec<Vec<String>> = step_results
        .iter()
        .map(|r| {
            vec![
                sim_name.to_string(),
                r.id.clone(),
                r.down_bytes.clone().unwrap_or_default(),
                r.elapsed_s()
                    .map(|v| format!("{:.3}", v))
                    .unwrap_or_default(),
                r.mb_s().map(|v| format!("{:.2}", v)).unwrap_or_default(),
                r.up_bytes.clone().unwrap_or_default(),
                r.up_mb_s()
                    .map(|v| format!("{:.2}", v))
                    .unwrap_or_default(),
            ]
        })
        .collect();

    let active: Vec<usize> = (0..headers.len())
        .filter(|&ci| rows.iter().any(|row| !row[ci].is_empty()))
        .collect();

    if active.is_empty() {
        return String::new();
    }

    let mut md = String::new();
    md.push('|');
    for &ci in &active {
        md.push_str(&format!(" {} |", headers[ci]));
    }
    md.push('\n');
    md.push('|');
    for &ci in &active {
        md.push_str(&format!(" {} |", "-".repeat(headers[ci].len())));
    }
    md.push('\n');
    for row in &rows {
        md.push('|');
        for &ci in &active {
            md.push_str(&format!(" {} |", row[ci]));
        }
        md.push('\n');
    }
    md
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RunResults {
    run: String,
    sim_dir: String,
    sim: String,
    steps: Vec<StepResultRecord>,
}

/// Scan per-sim result directories under one run root and emit combined reports.
///
/// If `run_names` is non-empty, only those directory names are included.
pub async fn write_combined_results_for_runs(work_root: &Path, run_names: &[String]) -> Result<()> {
    let mut runs = load_runs(work_root, run_names)?;
    runs.sort_by(|a, b| a.run.cmp(&b.run));

    let all_json = serde_json::to_string_pretty(&serde_json::json!({
        "runs": runs,
    }))
    .context("serialize combined results")?;
    tokio::fs::write(work_root.join("combined-results.json"), all_json)
        .await
        .context("write combined-results.json")?;

    // Summary table: one row per sim, aggregating all runs.
    let mut steps_by_sim: BTreeMap<String, Vec<&StepResultRecord>> = BTreeMap::new();
    for run in &runs {
        for s in &run.steps {
            steps_by_sim.entry(run.sim.clone()).or_default().push(s);
        }
    }

    let mut md = String::new();
    md.push_str("| Sim | N | Max Down (MB/s) | Max Up (MB/s) |\n");
    md.push_str("| --- | - | --------------- | ------------- |\n");
    for (sim, steps) in &steps_by_sim {
        let max_down = steps
            .iter()
            .filter_map(|r| r.mb_s())
            .reduce(f64::max)
            .map(|v| format!("{:.2}", v))
            .unwrap_or_default();
        let max_up = steps
            .iter()
            .filter_map(|r| r.up_mb_s())
            .reduce(f64::max)
            .map(|v| format!("{:.2}", v))
            .unwrap_or_default();
        md.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            sim,
            steps.len(),
            max_down,
            max_up,
        ));
    }
    md.push('\n');

    // Detail table: data-driven, one row per step across all runs.
    let detail_headers = [
        "run",
        "sim",
        "id",
        "down_bytes",
        "elapsed_s",
        "mb_s",
        "up_bytes",
        "up_mb_s",
    ];
    let detail_rows: Vec<Vec<String>> = runs
        .iter()
        .flat_map(|run| {
            run.steps.iter().map(move |r| {
                vec![
                    run.run.clone(),
                    run.sim.clone(),
                    r.id.clone(),
                    r.down_bytes.clone().unwrap_or_default(),
                    r.elapsed_s()
                        .map(|v| format!("{:.3}", v))
                        .unwrap_or_default(),
                    r.mb_s().map(|v| format!("{:.2}", v)).unwrap_or_default(),
                    r.up_bytes.clone().unwrap_or_default(),
                    r.up_mb_s()
                        .map(|v| format!("{:.2}", v))
                        .unwrap_or_default(),
                ]
            })
        })
        .collect();

    let active: Vec<usize> = (0..detail_headers.len())
        .filter(|&ci| detail_rows.iter().any(|row| !row[ci].is_empty()))
        .collect();

    if !active.is_empty() {
        md.push('|');
        for &ci in &active {
            md.push_str(&format!(" {} |", detail_headers[ci]));
        }
        md.push('\n');
        md.push('|');
        for &ci in &active {
            md.push_str(&format!(" {} |", "-".repeat(detail_headers[ci].len())));
        }
        md.push('\n');
        for row in &detail_rows {
            md.push('|');
            for &ci in &active {
                md.push_str(&format!(" {} |", row[ci]));
            }
            md.push('\n');
        }
    }

    tokio::fs::write(work_root.join("combined-results.md"), md)
        .await
        .context("write combined-results.md")?;
    Ok(())
}

/// Print a concise per-sim summary for one invocation run.
pub fn print_run_summary_table_for_runs(work_root: &Path, run_names: &[String]) -> Result<()> {
    let runs = load_runs(work_root, run_names)?;
    if runs.is_empty() {
        return Ok(());
    }

    let mut table = Table::new();
    table.load_preset(UTF8_FULL);
    table.set_header(vec!["Sim", "N", "Max Down (MB/s)", "Max Up (MB/s)"]);
    for run in &runs {
        let n = run.steps.len();
        let max_down = run
            .steps
            .iter()
            .filter_map(|r| r.mb_s())
            .reduce(f64::max)
            .map(|v| format!("{:.2}", v))
            .unwrap_or_else(|| "-".to_string());
        let max_up = run
            .steps
            .iter()
            .filter_map(|r| r.up_mb_s())
            .reduce(f64::max)
            .map(|v| format!("{:.2}", v))
            .unwrap_or_else(|| "-".to_string());
        table.add_row(vec![run.sim.clone(), n.to_string(), max_down, max_up]);
    }
    println!("\nRun Summary:");
    println!("{table}");
    Ok(())
}

fn load_runs(work_root: &Path, run_names: &[String]) -> Result<Vec<RunResults>> {
    let run_name = work_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("run")
        .to_string();
    let include: Option<HashSet<&str>> = if run_names.is_empty() {
        None
    } else {
        Some(run_names.iter().map(String::as_str).collect())
    };
    let mut runs = Vec::new();
    for ent in
        std::fs::read_dir(work_root).with_context(|| format!("read {}", work_root.display()))?
    {
        let ent = ent?;
        let path = ent.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name == "latest" {
            continue;
        }
        if let Some(filter) = &include {
            if !filter.contains(name) {
                continue;
            }
        }
        let results_json = path.join("results.json");
        if !results_json.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&results_json)
            .with_context(|| format!("read {}", results_json.display()))?;
        let v: serde_json::Value = serde_json::from_str(&text).context("parse run results json")?;
        let sim = v
            .get("sim")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let steps: Vec<StepResultRecord> = serde_json::from_value(
            v.get("steps")
                .cloned()
                .unwrap_or_else(|| serde_json::Value::Array(vec![])),
        )
        .context("parse steps array")?;
        runs.push(RunResults {
            run: run_name.clone(),
            sim_dir: name.to_string(),
            sim,
            steps,
        });
    }
    Ok(runs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(prefix: &str) -> std::path::PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("netsim-{prefix}-{ts}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn step_result_record_computes_mb_s() {
        let r = StepResultRecord {
            id: "xfer".to_string(),
            duration: Some("2000000".to_string()), // 2s in microseconds
            up_bytes: None,
            down_bytes: Some("1000000".to_string()), // 1 MB
        };
        assert_eq!(r.elapsed_s(), Some(2.0));
        assert_eq!(r.size_bytes(), Some(1_000_000));
        // 1MB in 2s = 0.5 MB/s
        assert_eq!(r.mb_s(), Some(0.5));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn write_results_writes_json_and_markdown() {
        let dir = temp_dir("report-write");
        let step_results = vec![StepResultRecord {
            id: "xfer".to_string(),
            duration: Some("2000000".to_string()),
            up_bytes: None,
            down_bytes: Some("1000000".to_string()),
        }];
        write_results(&dir, "sim-a", &step_results).await.unwrap();

        let json = std::fs::read_to_string(dir.join("results.json")).unwrap();
        let md = std::fs::read_to_string(dir.join("results.md")).unwrap();
        assert!(json.contains("\"sim\": \"sim-a\""));
        assert!(json.contains("\"steps\""));
        assert!(json.contains("\"id\": \"xfer\""));
        assert!(md.contains("sim-a"));
        assert!(md.contains("xfer"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn write_combined_results_filters_runs_and_writes_summary() {
        let root = temp_dir("report-combined");
        let run_a = root.join("sim-a");
        let run_b = root.join("sim-b");
        std::fs::create_dir_all(&run_a).unwrap();
        std::fs::create_dir_all(&run_b).unwrap();

        let r1 = vec![StepResultRecord {
            id: "xfer-a".to_string(),
            duration: Some("1000000".to_string()),
            up_bytes: None,
            down_bytes: Some("1000000".to_string()),
        }];
        let r2 = vec![StepResultRecord {
            id: "xfer-b".to_string(),
            duration: Some("2000000".to_string()),
            up_bytes: None,
            down_bytes: Some("1000000".to_string()),
        }];
        write_results(&run_a, "sim-a", &r1).await.unwrap();
        write_results(&run_b, "sim-b", &r2).await.unwrap();

        write_combined_results_for_runs(&root, &["sim-a".to_string()])
            .await
            .unwrap();

        let json = std::fs::read_to_string(root.join("combined-results.json")).unwrap();
        let md = std::fs::read_to_string(root.join("combined-results.md")).unwrap();
        assert!(json.contains("\"sim\": \"sim-a\""));
        assert!(!json.contains("\"sim\": \"sim-b\""));
        assert!(md.contains("sim-a"));
        assert!(!md.contains("sim-b"));
    }
}
