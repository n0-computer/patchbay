//! Background event writer: debounces both events.jsonl flushes and state.json writes.

use std::{
    fs,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::{
    consts,
    event::{LabEvent, LabState},
};

/// How often events.jsonl is flushed and state.json is written.
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

/// Writes events to `events.jsonl` and maintains `state.json`.
pub(crate) struct LabWriter {
    outdir: PathBuf,
    state: LabState,
    events_file: BufWriter<fs::File>,
}

impl LabWriter {
    pub(crate) fn new(outdir: &Path) -> Result<Self> {
        fs::create_dir_all(outdir)?;
        let events_path = outdir.join(consts::EVENTS_JSONL);
        let events_file = BufWriter::new(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&events_path)?,
        );
        Ok(Self {
            outdir: outdir.to_path_buf(),
            state: LabState::default(),
            events_file,
        })
    }

    /// Append event to events.jsonl buffer and update in-memory state.
    /// Does NOT flush — call [`flush`] to persist to disk.
    fn append_event(&mut self, event: &LabEvent) -> Result<()> {
        serde_json::to_writer(&mut self.events_file, event)?;
        self.events_file.write_all(b"\n")?;
        self.state.apply(event);
        Ok(())
    }

    /// Flush buffered events to disk.
    fn flush_events(&mut self) -> Result<()> {
        self.events_file.flush()?;
        Ok(())
    }

    /// Atomically write current state to state.json.
    fn write_state(&self) -> Result<()> {
        let tmp = self.outdir.join(consts::STATE_JSON_TMP);
        let dst = self.outdir.join(consts::STATE_JSON);
        fs::write(&tmp, serde_json::to_string_pretty(&self.state)?)?;
        fs::rename(&tmp, &dst)?;
        Ok(())
    }
}

/// Spawns a background task that writes events to disk.
///
/// Events are buffered in memory and flushed to `events.jsonl` + `state.json`
/// at most once per [`FLUSH_INTERVAL`], with a final flush on channel close.
pub(crate) fn spawn_writer(
    outdir: PathBuf,
    mut rx: tokio::sync::broadcast::Receiver<LabEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut writer = match LabWriter::new(&outdir) {
            Ok(w) => w,
            Err(e) => {
                tracing::error!("LabWriter init failed: {e}");
                return;
            }
        };

        let mut dirty = false;
        let mut interval = tokio::time::interval(FLUSH_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Consume the first immediate tick.
        interval.tick().await;

        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(event) => {
                            if let Err(e) = writer.append_event(&event) {
                                tracing::error!("LabWriter append error: {e}");
                            }
                            dirty = true;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("LabWriter lagged {n} events");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = interval.tick() => {
                    if dirty {
                        if let Err(e) = writer.flush_events() {
                            tracing::error!("LabWriter events flush error: {e}");
                        }
                        if let Err(e) = writer.write_state() {
                            tracing::error!("LabWriter state write error: {e}");
                        }
                        dirty = false;
                    }
                }
            }
        }

        // Final flush on close.
        if dirty {
            if let Err(e) = writer.flush_events() {
                tracing::error!("LabWriter final events flush error: {e}");
            }
            if let Err(e) = writer.write_state() {
                tracing::error!("LabWriter final state write error: {e}");
            }
        }
    })
}

// ── Run discovery ──────────────────────────────────────────────────

/// Metadata for a single Lab run directory.
#[derive(Debug, Clone, Serialize)]
pub struct RunInfo {
    /// Directory name (e.g. `"20260303_143001-my-lab"`).
    pub name: String,
    /// Full path to the run directory.
    pub path: PathBuf,
    /// Human-readable label from `state.json`, if available.
    pub label: Option<String>,
    /// Lab status from `state.json` (e.g. `"running"`, `"stopping"`).
    pub status: Option<String>,
}

/// Lists Lab output directories under `base`, newest-first.
///
/// A directory is considered a run if it contains `events.jsonl`.
/// Label and status are read from `state.json` if present.
pub fn discover_runs(base: &Path) -> Result<Vec<RunInfo>> {
    let mut runs = Vec::new();
    let entries = fs::read_dir(base).context("read outdir base")?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if !path.join(consts::EVENTS_JSONL).exists() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let (label, status) = read_run_metadata(&path);
        runs.push(RunInfo {
            name,
            path,
            label,
            status,
        });
    }
    // Sort newest-first (lexicographic descending works for YYYYMMDD_HHMMSS prefix).
    runs.sort_by(|a, b| b.name.cmp(&a.name));
    Ok(runs)
}

/// Reads label and status from `state.json` in a run directory.
fn read_run_metadata(run_dir: &Path) -> (Option<String>, Option<String>) {
    let state_path = run_dir.join(consts::STATE_JSON);
    let Ok(contents) = fs::read_to_string(&state_path) else {
        return (None, None);
    };
    let Ok(state) = serde_json::from_str::<LabState>(&contents) else {
        return (None, None);
    };
    (state.label, Some(state.status))
}
