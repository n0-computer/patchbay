//! Background event writer: debounces both events.jsonl flushes and state.json writes.

use std::{
    fs,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Result;

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
/// at most once per [`FLUSH_INTERVAL`], with a final flush when the lab is
/// cancelled or the channel closes.
pub(crate) fn spawn_writer(
    outdir: PathBuf,
    mut rx: tokio::sync::broadcast::Receiver<LabEvent>,
    cancel: tokio_util::sync::CancellationToken,
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
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            // Channel closed means the lab was dropped — treat as stop.
                            writer.state.status = "stopped".into();
                            dirty = true;
                            break;
                        }
                    }
                }
                _ = cancel.cancelled() => {
                    // Lab is shutting down — drain remaining events, then stop.
                    while let Ok(event) = rx.try_recv() {
                        let _ = writer.append_event(&event);
                    }
                    writer.state.status = "stopped".into();
                    dirty = true;
                    break;
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
