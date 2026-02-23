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
    /// Duration value from capture (raw string, e.g. microseconds).
    pub duration_raw: Option<String>,
    /// Upload bytes value from capture.
    pub up_bytes_raw: Option<String>,
    /// Download bytes value from capture.
    pub down_bytes_raw: Option<String>,
}

impl StepResultRecord {
    /// Parse duration as either microseconds (legacy) or seconds (float).
    pub fn elapsed_s(&self) -> Option<f64> {
        let s = self.duration_raw.as_deref()?;
        if let Ok(us) = s.parse::<u64>() {
            return Some(us as f64 / 1_000_000.0);
        }
        s.parse::<f64>().ok()
    }

    /// Parse down_bytes as u64.
    pub fn down_bytes(&self) -> Option<u64> {
        self.down_bytes_raw.as_deref()?.parse().ok()
    }

    /// Parse up_bytes as u64.
    pub fn up_bytes(&self) -> Option<u64> {
        self.up_bytes_raw.as_deref()?.parse().ok()
    }

    /// Compute downlink Mbit/s from bytes and duration.
    pub fn down_mbps(&self) -> Option<f64> {
        let bytes = self.down_bytes()? as f64;
        let secs = self.elapsed_s()?;
        if secs > 0.0 {
            Some(bytes * 8.0 / (secs * 1_000_000.0))
        } else {
            None
        }
    }

    /// Compute uplink Mbit/s from bytes and duration.
    pub fn up_mbps(&self) -> Option<f64> {
        let bytes = self.up_bytes()? as f64;
        let secs = self.elapsed_s()?;
        if secs > 0.0 {
            Some(bytes * 8.0 / (secs * 1_000_000.0))
        } else {
            None
        }
    }
}

/// Transfer result row synthesized from step results (for UI `results.json`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TransferResult {
    pub id: String,
    pub provider: String,
    pub fetcher: String,
    /// Bytes transferred.
    pub size_bytes: Option<u64>,
    /// Transfer duration in seconds.
    pub elapsed_s: Option<f64>,
    /// Throughput in Mbit/s (downlink alias for backward compatibility).
    pub mbps: Option<f64>,
    /// Uplink throughput in Mbit/s.
    pub up_mbps: Option<f64>,
    /// Downlink throughput in Mbit/s.
    pub down_mbps: Option<f64>,
    /// Was the final connection direct (not relay)?
    pub final_conn_direct: Option<bool>,
    /// Did the connection ever upgrade to direct?
    pub conn_upgrade: Option<bool>,
    /// Total number of ConnectionTypeChanged events observed.
    pub conn_events: usize,
}

/// Write results to `<work_dir>/results.json` and `<work_dir>/results.md`.
///
/// Synthesizes `TransferResult` entries from `step_results` so the UI's
/// `transfers` field stays populated.
pub async fn write_results(
    work_dir: &Path,
    sim_name: &str,
    step_results: &[StepResultRecord],
) -> Result<()> {
    if step_results.is_empty() {
        return Ok(());
    }

    let transfers: Vec<TransferResult> = step_results
        .iter()
        .map(|r| TransferResult {
            id: r.id.clone(),
            provider: String::new(),
            fetcher: String::new(),
            size_bytes: r.down_bytes(),
            elapsed_s: r.elapsed_s(),
            mbps: r.down_mbps(),
            up_mbps: r.up_mbps(),
            down_mbps: r.down_mbps(),
            final_conn_direct: None,
            conn_upgrade: None,
            conn_events: 0,
        })
        .collect();

    let json = serde_json::to_string_pretty(&serde_json::json!({
        "sim": sim_name,
        "transfers": transfers,
        "iperf": [],
    }))
    .context("serialize results")?;
    tokio::fs::write(work_dir.join("results.json"), json)
        .await
        .context("write results.json")?;

    let mut md = String::new();
    md.push_str("| sim | id | size_bytes | elapsed_s | up_mbps | down_mbps |\n");
    md.push_str("| --- | -- | ---------- | --------- | ------- | --------- |\n");
    for r in &transfers {
        md.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            sim_name,
            r.id,
            r.size_bytes.map(|v| v.to_string()).unwrap_or_default(),
            r.elapsed_s.map(|v| format!("{:.3}", v)).unwrap_or_default(),
            r.up_mbps.map(|v| format!("{:.1}", v)).unwrap_or_default(),
            r.down_mbps.map(|v| format!("{:.1}", v)).unwrap_or_default(),
        ));
    }
    tokio::fs::write(work_dir.join("results.md"), md)
        .await
        .context("write results.md")?;

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RunResults {
    run: String,
    sim_dir: String,
    sim: String,
    transfers: Vec<TransferResult>,
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

    let mut transfer_by_sim: BTreeMap<String, Vec<&TransferResult>> = BTreeMap::new();
    for run in &runs {
        for t in &run.transfers {
            transfer_by_sim.entry(run.sim.clone()).or_default().push(t);
        }
    }

    let mut md = String::new();
    md.push_str("| sim | transfers | avg_mbps | direct_final_pct |\n");
    md.push_str("| --- | --------- | -------- | ---------------- |\n");
    for (sim, transfers) in &transfer_by_sim {
        let mut mbps_sum = 0.0f64;
        let mut mbps_count = 0usize;
        let mut direct_total = 0usize;
        let mut direct_yes = 0usize;
        for t in transfers {
            if let Some(v) = t.mbps {
                mbps_sum += v;
                mbps_count += 1;
            }
            if let Some(v) = t.final_conn_direct {
                direct_total += 1;
                if v {
                    direct_yes += 1;
                }
            }
        }
        let avg_mbps = if mbps_count > 0 {
            format!("{:.1}", mbps_sum / mbps_count as f64)
        } else {
            String::new()
        };
        let direct_pct = if direct_total > 0 {
            format!(
                "{:.0}%",
                100.0 * (direct_yes as f64) / (direct_total as f64)
            )
        } else {
            String::new()
        };
        md.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            sim,
            transfers.len(),
            avg_mbps,
            direct_pct
        ));
    }
    md.push('\n');
    md.push_str("| run | sim | id | provider | fetcher | size_bytes | elapsed_s | mbps | final_conn_direct | conn_upgrade | conn_events |\n");
    md.push_str("| --- | --- | -- | -------- | ------- | ---------- | --------- | ---- | ----------------- | ------------ | ----------- |\n");
    for run in &runs {
        for r in &run.transfers {
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                run.run,
                run.sim,
                r.id,
                r.provider,
                r.fetcher,
                r.size_bytes.map(|v| v.to_string()).unwrap_or_default(),
                r.elapsed_s.map(|v| format!("{:.3}", v)).unwrap_or_default(),
                r.mbps.map(|v| format!("{:.1}", v)).unwrap_or_default(),
                r.final_conn_direct
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
                r.conn_upgrade.map(|v| v.to_string()).unwrap_or_default(),
                r.conn_events,
            ));
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

    #[derive(Deserialize)]
    struct SimStatus {
        status: Option<String>,
    }

    let mut table = Table::new();
    table.load_preset(UTF8_FULL);
    table.set_header(vec!["sim", "status", "down_mbps"]);
    for run in &runs {
        let status = std::fs::read_to_string(work_root.join(&run.sim_dir).join("sim.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<SimStatus>(&text).ok())
            .and_then(|s| s.status)
            .unwrap_or_else(|| "unknown".to_string());
        let (down_sum, down_n) = run
            .transfers
            .iter()
            .filter_map(|r| r.mbps)
            .fold((0.0f64, 0usize), |(sum, n), v| (sum + v, n + 1));
        let down = if down_n > 0 {
            format!("{:.1}", down_sum / down_n as f64)
        } else {
            "-".to_string()
        };
        table.add_row(vec![run.sim.clone(), status, down]);
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
        let transfers: Vec<TransferResult> = serde_json::from_value(
            v.get("transfers")
                .cloned()
                .unwrap_or_else(|| serde_json::Value::Array(vec![])),
        )
        .context("parse transfers array")?;
        runs.push(RunResults {
            run: run_name.clone(),
            sim_dir: name.to_string(),
            sim,
            transfers,
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
    fn step_result_record_computes_mbps() {
        let r = StepResultRecord {
            id: "xfer".to_string(),
            duration_raw: Some("2000000".to_string()), // 2s in microseconds
            up_bytes_raw: None,
            down_bytes_raw: Some("1000000".to_string()), // 1 MB
        };
        assert_eq!(r.elapsed_s(), Some(2.0));
        assert_eq!(r.down_bytes(), Some(1_000_000));
        // 1MB in 2s = 4 Mbit/s
        assert_eq!(r.down_mbps(), Some(4.0));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn write_results_writes_json_and_markdown() {
        let dir = temp_dir("report-write");
        let step_results = vec![StepResultRecord {
            id: "xfer".to_string(),
            duration_raw: Some("2000000".to_string()),
            up_bytes_raw: None,
            down_bytes_raw: Some("1000000".to_string()),
        }];
        write_results(&dir, "sim-a", &step_results).await.unwrap();

        let json = std::fs::read_to_string(dir.join("results.json")).unwrap();
        let md = std::fs::read_to_string(dir.join("results.md")).unwrap();
        assert!(json.contains("\"sim\": \"sim-a\""));
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
            duration_raw: Some("1000000".to_string()),
            up_bytes_raw: None,
            down_bytes_raw: Some("1000000".to_string()),
        }];
        let r2 = vec![StepResultRecord {
            id: "xfer-b".to_string(),
            duration_raw: Some("2000000".to_string()),
            up_bytes_raw: None,
            down_bytes_raw: Some("1000000".to_string()),
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
