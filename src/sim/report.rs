use anyhow::{Context, Result};
use comfy_table::{presets::UTF8_FULL, Table};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

/// Parsed result from one iroh-transfer run.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TransferResult {
    pub id: String,
    pub provider: String,
    pub fetcher: String,
    /// Bytes transferred.
    pub size_bytes: Option<u64>,
    /// Transfer duration in seconds.
    pub elapsed_s: Option<f64>,
    /// Throughput in Mbit/s.
    pub mbps: Option<f64>,
    /// Was the final connection direct (not relay)?
    pub final_conn_direct: Option<bool>,
    /// Did the connection ever upgrade to direct?
    pub conn_upgrade: Option<bool>,
    /// Total number of ConnectionTypeChanged events observed.
    pub conn_events: usize,
}

/// Parsed result from one `iperf3 -J` run.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IperfResult {
    /// Step identifier.
    pub id: String,
    /// Device where the iperf client command ran.
    pub device: String,
    /// Bytes transferred according to iperf summary.
    pub bytes: Option<u64>,
    /// Duration in seconds.
    pub seconds: Option<f64>,
    /// Throughput in bits per second.
    pub bits_per_second: Option<f64>,
    /// Throughput in Mbit/s.
    pub mbps: Option<f64>,
    /// TCP retransmits if present.
    pub retransmits: Option<u64>,
    /// Optional baseline result id used for comparison.
    pub baseline: Option<String>,
    /// `mbps - baseline_mbps` when baseline is available.
    pub delta_mbps: Option<f64>,
    /// Relative delta percent from baseline throughput.
    pub delta_pct: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct IperfMetrics {
    pub bytes: Option<u64>,
    pub seconds: Option<f64>,
    pub bits_per_second: Option<f64>,
    pub retransmits: Option<u64>,
}

/// Parse an `iperf3 -J` log file into summary metrics.
///
/// The parser is tolerant of mixed stdout/stderr logs and extracts the first
/// top-level JSON object found in the file.
pub fn parse_iperf3_json_log(log_path: &Path) -> Result<IperfMetrics> {
    let text = std::fs::read_to_string(log_path)
        .with_context(|| format!("read iperf log {}", log_path.display()))?;
    let json = extract_json_object(&text).ok_or_else(|| {
        anyhow::anyhow!(
            "no JSON object found in iperf log {}; ensure iperf3 is run with -J",
            log_path.display()
        )
    })?;
    let v: serde_json::Value = serde_json::from_str(json).context("parse iperf JSON output")?;
    let end = v
        .get("end")
        .ok_or_else(|| anyhow::anyhow!("iperf JSON missing 'end' section"))?;

    let sum = end
        .get("sum_received")
        .or_else(|| end.get("sum"))
        .or_else(|| end.get("sum_sent"));
    let bits_per_second = sum
        .and_then(|s| s.get("bits_per_second"))
        .and_then(|v| v.as_f64());
    let seconds = sum.and_then(|s| s.get("seconds")).and_then(|v| v.as_f64());
    let bytes = sum.and_then(|s| s.get("bytes")).and_then(|v| v.as_u64());
    let retransmits = sum
        .and_then(|s| s.get("retransmits"))
        .and_then(|v| v.as_u64());

    if bits_per_second.is_none() {
        return Err(anyhow::anyhow!(
            "iperf JSON missing throughput fields in end.sum_*"
        ));
    }

    Ok(IperfMetrics {
        bytes,
        seconds,
        bits_per_second,
        retransmits,
    })
}

fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end < start {
        return None;
    }
    Some(&text[start..=end])
}

impl TransferResult {
    /// Parse a fetcher NDJSON log file and fill in transfer stats.
    pub fn parse_fetcher_log(&mut self, log_path: &Path) -> Result<()> {
        let text = std::fs::read_to_string(log_path)
            .with_context(|| format!("read fetcher log {}", log_path.display()))?;
        let mut conn_events = 0usize;
        let mut ever_direct = false;
        let mut last_direct: Option<bool> = None;

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let v: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match v.get("kind").and_then(|k| k.as_str()) {
                Some("DownloadComplete") => {
                    if let Some(size) = v.get("size").and_then(|s| s.as_u64()) {
                        self.size_bytes = Some(size);
                    }
                    if let Some(dur_us) = v.get("duration").and_then(|d| d.as_u64()) {
                        let elapsed = dur_us as f64 / 1_000_000.0;
                        self.elapsed_s = Some(elapsed);
                        if let Some(size) = self.size_bytes {
                            self.mbps = Some(size as f64 * 8.0 / (elapsed * 1_000_000.0));
                        }
                    }
                }
                Some("ConnectionTypeChanged") => {
                    if v.get("status").and_then(|s| s.as_str()) == Some("Selected") {
                        conn_events += 1;
                        let is_direct = v.get("addr").map(is_direct_addr).unwrap_or(false);
                        if is_direct {
                            ever_direct = true;
                        }
                        last_direct = Some(is_direct);
                    }
                }
                _ => {}
            }
        }

        self.conn_events = conn_events;
        self.final_conn_direct = last_direct;
        self.conn_upgrade = Some(ever_direct);
        Ok(())
    }
}

fn is_direct_addr(addr: &serde_json::Value) -> bool {
    if let Some(s) = addr.as_str() {
        return s.contains("Ip(");
    }
    if let Some(obj) = addr.as_object() {
        return obj.contains_key("Ip");
    }
    false
}

/// Write results to `<work_dir>/results.json` and `<work_dir>/results.md`.
pub async fn write_results(
    work_dir: &Path,
    sim_name: &str,
    results: &[TransferResult],
    iperf_results: &[IperfResult],
) -> Result<()> {
    if results.is_empty() && iperf_results.is_empty() {
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&serde_json::json!({
        "sim": sim_name,
        "transfers": results,
        "iperf": iperf_results,
    }))
    .context("serialize results")?;
    tokio::fs::write(work_dir.join("results.json"), json)
        .await
        .context("write results.json")?;

    let mut md = String::new();
    md.push_str("| sim | id | provider | fetcher | size_bytes | elapsed_s | mbps | final_conn_direct | conn_upgrade | conn_events |\n");
    md.push_str("| --- | -- | -------- | ------- | ---------- | --------- | ---- | ----------------- | ------------ | ----------- |\n");
    for r in results {
        md.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            sim_name,
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
    if !iperf_results.is_empty() {
        md.push('\n');
        md.push_str("| sim | id | device | bytes | seconds | mbps | retransmits | baseline | delta_mbps | delta_pct |\n");
        md.push_str("| --- | -- | ------ | ----- | ------- | ---- | ----------- | -------- | ---------- | --------- |\n");
        for r in iperf_results {
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                sim_name,
                r.id,
                r.device,
                r.bytes.map(|v| v.to_string()).unwrap_or_default(),
                r.seconds.map(|v| format!("{:.3}", v)).unwrap_or_default(),
                r.mbps.map(|v| format!("{:.3}", v)).unwrap_or_default(),
                r.retransmits.map(|v| v.to_string()).unwrap_or_default(),
                r.baseline.clone().unwrap_or_default(),
                r.delta_mbps
                    .map(|v| format!("{:.3}", v))
                    .unwrap_or_default(),
                r.delta_pct.map(|v| format!("{:.1}", v)).unwrap_or_default(),
            ));
        }
    }
    tokio::fs::write(work_dir.join("results.md"), md)
        .await
        .context("write results.md")?;

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RunResults {
    run: String,
    sim: String,
    transfers: Vec<TransferResult>,
    iperf: Vec<IperfResult>,
}

/// Scan run directories under `work_root` and emit combined reports.
///
/// If `run_names` is non-empty, only those run directories are included.
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
    let mut iperf_by_sim: BTreeMap<String, Vec<&IperfResult>> = BTreeMap::new();
    for run in &runs {
        for t in &run.transfers {
            transfer_by_sim.entry(run.sim.clone()).or_default().push(t);
        }
        for i in &run.iperf {
            iperf_by_sim.entry(run.sim.clone()).or_default().push(i);
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
    if !iperf_by_sim.is_empty() {
        md.push('\n');
        md.push_str("| sim | iperf_runs | avg_mbps |\n");
        md.push_str("| --- | --------- | -------- |\n");
        for (sim, runs) in &iperf_by_sim {
            let mut mbps_sum = 0.0f64;
            let mut mbps_count = 0usize;
            for r in runs {
                if let Some(mbps) = r.mbps {
                    mbps_sum += mbps;
                    mbps_count += 1;
                }
            }
            let avg_mbps = if mbps_count > 0 {
                format!("{:.3}", mbps_sum / mbps_count as f64)
            } else {
                String::new()
            };
            md.push_str(&format!("| {} | {} | {} |\n", sim, runs.len(), avg_mbps));
        }
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
    if runs.iter().any(|run| !run.iperf.is_empty()) {
        md.push('\n');
        md.push_str(
            "| run | sim | id | device | bytes | seconds | mbps | retransmits | baseline | delta_mbps | delta_pct |\n",
        );
        md.push_str(
            "| --- | --- | -- | ------ | ----- | ------- | ---- | ----------- | -------- | ---------- | --------- |\n",
        );
        for run in &runs {
            for r in &run.iperf {
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                    run.run,
                    run.sim,
                    r.id,
                    r.device,
                    r.bytes.map(|v| v.to_string()).unwrap_or_default(),
                    r.seconds.map(|v| format!("{:.3}", v)).unwrap_or_default(),
                    r.mbps.map(|v| format!("{:.3}", v)).unwrap_or_default(),
                    r.retransmits.map(|v| v.to_string()).unwrap_or_default(),
                    r.baseline.clone().unwrap_or_default(),
                    r.delta_mbps
                        .map(|v| format!("{:.3}", v))
                        .unwrap_or_default(),
                    r.delta_pct.map(|v| format!("{:.1}", v)).unwrap_or_default(),
                ));
            }
        }
    }
    tokio::fs::write(work_root.join("combined-results.md"), md)
        .await
        .context("write combined-results.md")?;
    Ok(())
}

/// Print combined results for selected runs as terminal tables.
pub fn print_combined_results_table_for_runs(work_root: &Path, run_names: &[String]) -> Result<()> {
    let runs = load_runs(work_root, run_names)?;
    if runs.is_empty() {
        return Ok(());
    }

    let mut transfer_by_sim: BTreeMap<String, Vec<&TransferResult>> = BTreeMap::new();
    let mut iperf_by_sim: BTreeMap<String, Vec<&IperfResult>> = BTreeMap::new();
    for run in &runs {
        for t in &run.transfers {
            transfer_by_sim.entry(run.sim.clone()).or_default().push(t);
        }
        for i in &run.iperf {
            iperf_by_sim.entry(run.sim.clone()).or_default().push(i);
        }
    }

    let mut summary = Table::new();
    summary.load_preset(UTF8_FULL);
    summary.set_header(vec!["sim", "transfers", "avg_mbps", "direct_final_pct"]);
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
            "-".to_string()
        };
        let direct_pct = if direct_total > 0 {
            format!("{:.0}%", 100.0 * direct_yes as f64 / direct_total as f64)
        } else {
            "-".to_string()
        };
        summary.add_row(vec![
            sim.clone(),
            transfers.len().to_string(),
            avg_mbps,
            direct_pct,
        ]);
    }

    let mut details = Table::new();
    details.load_preset(UTF8_FULL);
    details.set_header(vec![
        "run",
        "sim",
        "id",
        "provider",
        "fetcher",
        "size_bytes",
        "elapsed_s",
        "mbps",
        "final_direct",
        "conn_upgrade",
        "conn_events",
    ]);
    for run in &runs {
        for r in &run.transfers {
            details.add_row(vec![
                run.run.clone(),
                run.sim.clone(),
                r.id.clone(),
                r.provider.clone(),
                r.fetcher.clone(),
                r.size_bytes
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                r.elapsed_s
                    .map(|v| format!("{:.3}", v))
                    .unwrap_or_else(|| "-".to_string()),
                r.mbps
                    .map(|v| format!("{:.1}", v))
                    .unwrap_or_else(|| "-".to_string()),
                r.final_conn_direct
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                r.conn_upgrade
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                r.conn_events.to_string(),
            ]);
        }
    }

    let mut iperf_summary = Table::new();
    iperf_summary.load_preset(UTF8_FULL);
    iperf_summary.set_header(vec!["sim", "iperf_runs", "avg_mbps"]);
    for (sim, rows) in &iperf_by_sim {
        let mut mbps_sum = 0.0f64;
        let mut mbps_count = 0usize;
        for row in rows {
            if let Some(mbps) = row.mbps {
                mbps_sum += mbps;
                mbps_count += 1;
            }
        }
        let avg_mbps = if mbps_count > 0 {
            format!("{:.3}", mbps_sum / mbps_count as f64)
        } else {
            "-".to_string()
        };
        iperf_summary.add_row(vec![sim.clone(), rows.len().to_string(), avg_mbps]);
    }

    let mut iperf_details = Table::new();
    iperf_details.load_preset(UTF8_FULL);
    iperf_details.set_header(vec![
        "run",
        "sim",
        "id",
        "device",
        "bytes",
        "seconds",
        "mbps",
        "retransmits",
        "baseline",
        "delta_mbps",
        "delta_pct",
    ]);
    for run in &runs {
        for r in &run.iperf {
            iperf_details.add_row(vec![
                run.run.clone(),
                run.sim.clone(),
                r.id.clone(),
                r.device.clone(),
                r.bytes
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                r.seconds
                    .map(|v| format!("{:.3}", v))
                    .unwrap_or_else(|| "-".to_string()),
                r.mbps
                    .map(|v| format!("{:.3}", v))
                    .unwrap_or_else(|| "-".to_string()),
                r.retransmits
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                r.baseline.clone().unwrap_or_else(|| "-".to_string()),
                r.delta_mbps
                    .map(|v| format!("{:.3}", v))
                    .unwrap_or_else(|| "-".to_string()),
                r.delta_pct
                    .map(|v| format!("{:.1}", v))
                    .unwrap_or_else(|| "-".to_string()),
            ]);
        }
    }

    if !transfer_by_sim.is_empty() {
        println!("\nCombined Summary:");
        println!("{summary}");
        println!("\nCombined Transfers:");
        println!("{details}");
    }
    if !iperf_by_sim.is_empty() {
        println!("\nCombined Iperf Summary:");
        println!("{iperf_summary}");
        println!("\nCombined Iperf Runs:");
        println!("{iperf_details}");
    }
    Ok(())
}

fn load_runs(work_root: &Path, run_names: &[String]) -> Result<Vec<RunResults>> {
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
        let iperf: Vec<IperfResult> = serde_json::from_value(
            v.get("iperf")
                .cloned()
                .unwrap_or_else(|| serde_json::Value::Array(vec![])),
        )
        .context("parse iperf array")?;
        runs.push(RunResults {
            run: name.to_string(),
            sim,
            transfers,
            iperf,
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
    fn parse_fetcher_log_extracts_transfer_and_conn_fields() {
        let dir = temp_dir("report-parse");
        let log = dir.join("fetcher.ndjson");
        let data = r#"{"kind":"ConnectionTypeChanged","status":"Selected","addr":"Relay(http://r)"}
{"kind":"ConnectionTypeChanged","status":"Selected","addr":"Ip(1.2.3.4:9999)"}
{"kind":"DownloadComplete","size":1000,"duration":2000000}
"#;
        std::fs::write(&log, data).unwrap();

        let mut r = TransferResult::default();
        r.parse_fetcher_log(&log).unwrap();

        assert_eq!(r.size_bytes, Some(1000));
        assert_eq!(r.elapsed_s, Some(2.0));
        assert_eq!(r.mbps, Some(0.004));
        assert_eq!(r.final_conn_direct, Some(true));
        assert_eq!(r.conn_upgrade, Some(true));
        assert_eq!(r.conn_events, 2);
    }

    #[test]
    fn parse_fetcher_log_supports_structured_addr() {
        let dir = temp_dir("report-parse-structured");
        let log = dir.join("fetcher.ndjson");
        let data = r#"{"kind":"ConnectionTypeChanged","status":"Selected","addr":{"Relay":"https://r"}}
{"kind":"ConnectionTypeChanged","status":"Selected","addr":{"Ip":"1.2.3.4:9999"}}
{"kind":"DownloadComplete","size":1000,"duration":1000000}
"#;
        std::fs::write(&log, data).unwrap();

        let mut r = TransferResult::default();
        r.parse_fetcher_log(&log).unwrap();

        assert_eq!(r.final_conn_direct, Some(true));
        assert_eq!(r.conn_upgrade, Some(true));
        assert_eq!(r.conn_events, 2);
    }

    #[test]
    fn parse_iperf3_json_log_extracts_summary() {
        let dir = temp_dir("report-iperf-parse");
        let log = dir.join("iperf.log");
        let data = r#"{
  "end": {
    "sum_received": {
      "seconds": 5.0,
      "bytes": 62500000,
      "bits_per_second": 100000000.0,
      "retransmits": 3
    }
  }
}
"#;
        std::fs::write(&log, data).unwrap();

        let parsed = parse_iperf3_json_log(&log).unwrap();
        assert_eq!(parsed.seconds, Some(5.0));
        assert_eq!(parsed.bytes, Some(62_500_000));
        assert_eq!(parsed.bits_per_second, Some(100_000_000.0));
        assert_eq!(parsed.retransmits, Some(3));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn write_results_writes_json_and_markdown() {
        let dir = temp_dir("report-write");
        let transfers = vec![TransferResult {
            id: "xfer".to_string(),
            provider: "p".to_string(),
            fetcher: "f".to_string(),
            size_bytes: Some(42),
            elapsed_s: Some(1.5),
            mbps: Some(0.2),
            final_conn_direct: Some(false),
            conn_upgrade: Some(false),
            conn_events: 1,
        }];
        let iperf = vec![IperfResult {
            id: "iperf-baseline".to_string(),
            device: "fetcher".to_string(),
            bytes: Some(1_000_000),
            seconds: Some(1.0),
            bits_per_second: Some(8_000_000.0),
            mbps: Some(8.0),
            retransmits: Some(0),
            baseline: None,
            delta_mbps: None,
            delta_pct: None,
        }];
        write_results(&dir, "sim-a", &transfers, &iperf)
            .await
            .unwrap();

        let json = std::fs::read_to_string(dir.join("results.json")).unwrap();
        let md = std::fs::read_to_string(dir.join("results.md")).unwrap();
        assert!(json.contains("\"sim\": \"sim-a\""));
        assert!(json.contains("\"id\": \"xfer\""));
        assert!(json.contains("\"id\": \"iperf-baseline\""));
        assert!(md.contains("| sim-a | xfer | p | f | 42 |"));
        assert!(md.contains("| sim-a | iperf-baseline | fetcher | 1000000 |"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn write_combined_results_filters_runs_and_writes_summary() {
        let root = temp_dir("report-combined");
        let run_a = root.join("sim-a-260220-120000");
        let run_b = root.join("sim-b-260220-120500");
        std::fs::create_dir_all(&run_a).unwrap();
        std::fs::create_dir_all(&run_b).unwrap();

        let r1 = vec![TransferResult {
            id: "xfer-a".to_string(),
            provider: "provider".to_string(),
            fetcher: "fetcher".to_string(),
            size_bytes: Some(100),
            elapsed_s: Some(1.0),
            mbps: Some(0.8),
            final_conn_direct: Some(true),
            conn_upgrade: Some(true),
            conn_events: 1,
        }];
        let r2 = vec![TransferResult {
            id: "xfer-b".to_string(),
            provider: "provider".to_string(),
            fetcher: "fetcher".to_string(),
            size_bytes: Some(100),
            elapsed_s: Some(2.0),
            mbps: Some(0.4),
            final_conn_direct: Some(false),
            conn_upgrade: Some(false),
            conn_events: 1,
        }];
        write_results(&run_a, "sim-a", &r1, &[]).await.unwrap();
        write_results(&run_b, "sim-b", &r2, &[]).await.unwrap();

        write_combined_results_for_runs(&root, &["sim-a-260220-120000".to_string()])
            .await
            .unwrap();

        let json = std::fs::read_to_string(root.join("combined-results.json")).unwrap();
        let md = std::fs::read_to_string(root.join("combined-results.md")).unwrap();
        assert!(json.contains("\"run\": \"sim-a-260220-120000\""));
        assert!(!json.contains("\"run\": \"sim-b-260220-120500\""));
        assert!(md.contains("| sim-a | 1 | 0.8 | 100% |"));
        assert!(!md.contains("sim-b"));
    }
}
