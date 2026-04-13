//! Background event writer: debounces both events.jsonl flushes and state.json writes.

use std::{
    fs,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Result;

use crate::{
    consts,
    event::{LabEvent, LabState},
};

/// How often events.jsonl is flushed and state.json is written.
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

/// Test outcome communicated from [`TestGuard`] to the writer on shutdown.
pub(crate) const STATUS_UNKNOWN: u8 = 0;
pub(crate) const STATUS_SUCCESS: u8 = 1;
pub(crate) const STATUS_FAILED: u8 = 2;

// ── FlushRegistry ──────────────────────────────────────────────────────

/// A resource that can be flushed synchronously (buffered file writers, etc.).
pub(crate) trait Flushable: Send + Sync {
    fn flush(&self);
}

/// Collects [`Flushable`] resources so they can all be flushed in one shot
/// during lab shutdown.
pub(crate) struct FlushRegistry {
    writers: Mutex<Vec<Arc<dyn Flushable>>>,
}

impl FlushRegistry {
    pub(crate) fn new() -> Self {
        Self {
            writers: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn register(&self, writer: Arc<dyn Flushable>) {
        self.writers.lock().expect("poisoned").push(writer);
    }

    pub(crate) fn flush_all(&self) {
        let writers = self.writers.lock().expect("poisoned");
        for writer in writers.iter() {
            writer.flush();
        }
    }
}

// ── SharedEventsFile ───────────────────────────────────────────────────

/// Thread-safe wrapper around the `events.jsonl` `BufWriter`, shared between
/// the async writer task and the [`FlushRegistry`] so the drop guard can
/// flush buffered bytes even when the async runtime is gone.
pub(crate) struct SharedEventsFile(Mutex<BufWriter<fs::File>>);

impl SharedEventsFile {
    pub(crate) fn new(outdir: &Path) -> Result<Self> {
        fs::create_dir_all(outdir)?;
        let events_path = outdir.join(consts::EVENTS_JSONL);
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&events_path)?;
        Ok(Self(Mutex::new(BufWriter::new(file))))
    }

    pub(crate) fn append(&self, event: &LabEvent) -> Result<()> {
        let mut guard = self.0.lock().expect("poisoned");
        serde_json::to_writer(&mut *guard, event)?;
        guard.write_all(b"\n")?;
        Ok(())
    }
}

impl Flushable for SharedEventsFile {
    fn flush(&self) {
        let _ = self.0.lock().expect("poisoned").flush();
    }
}

// ── LabWriter ──────────────────────────────────────────────────────────

/// Writes events to `events.jsonl` and maintains `state.json`.
struct LabWriter {
    outdir: PathBuf,
    shared_state: Arc<Mutex<LabState>>,
    events_file: Arc<SharedEventsFile>,
}

impl LabWriter {
    fn new(
        outdir: &Path,
        shared_state: Arc<Mutex<LabState>>,
        events_file: Arc<SharedEventsFile>,
    ) -> Self {
        Self {
            outdir: outdir.to_path_buf(),
            shared_state,
            events_file,
        }
    }

    /// Append event to events.jsonl buffer and update shared state.
    /// Does NOT flush -- call [`flush_events`] to persist to disk.
    fn append_event(&self, event: &LabEvent) -> Result<()> {
        self.events_file.append(event)?;
        self.shared_state.lock().expect("poisoned").apply(event);
        Ok(())
    }

    /// Flush buffered events to disk.
    fn flush_events(&self) {
        self.events_file.flush();
    }

    /// Atomically write current state to state.json.
    fn write_state(&self) -> Result<()> {
        let tmp = self.outdir.join(consts::STATE_JSON_TMP);
        let dst = self.outdir.join(consts::STATE_JSON);
        let state = self.shared_state.lock().expect("poisoned");
        fs::write(&tmp, serde_json::to_string_pretty(&*state)?)?;
        fs::rename(&tmp, &dst)?;
        Ok(())
    }
}

/// Spawns a background task that writes events to disk.
///
/// Events are buffered in memory and flushed to `events.jsonl` + `state.json`
/// at most once per [`FLUSH_INTERVAL`]. The shared `LabState` is updated on
/// every event so that `LabDropGuard::drop` can always write a final
/// `state.json` synchronously, even if this task never gets to run its
/// shutdown path.
pub(crate) fn spawn_writer(
    outdir: PathBuf,
    mut rx: tokio::sync::broadcast::Receiver<LabEvent>,
    cancel: tokio_util::sync::CancellationToken,
    shared_state: Arc<Mutex<LabState>>,
    events_file: Arc<SharedEventsFile>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let writer = LabWriter::new(&outdir, shared_state, events_file);

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
                            break;
                        }
                    }
                }
                _ = cancel.cancelled() => {
                    // Lab is shutting down -- drain remaining events, then stop.
                    while let Ok(event) = rx.try_recv() {
                        let _ = writer.append_event(&event);
                    }
                    break;
                }
                _ = interval.tick() => {
                    if dirty {
                        writer.flush_events();
                        if let Err(e) = writer.write_state() {
                            tracing::error!("LabWriter state write error: {e}");
                        }
                        dirty = false;
                    }
                }
            }
        }

        // Final flush of events.jsonl. State.json is written by LabDropGuard::drop
        // which sets the final status and writes synchronously.
        writer.flush_events();
        if let Err(e) = writer.write_state() {
            tracing::error!("LabWriter final state write error: {e}");
        }
    })
}

/// Synchronously write the final `state.json` with the given status.
///
/// Called from `LabInner::drop`. Reads the accumulated state from the shared
/// mutex, patches the status, and writes atomically.
pub(crate) fn write_final_state(run_dir: &Path, shared_state: &Mutex<LabState>, status: &str) {
    let tmp = run_dir.join(consts::STATE_JSON_TMP);
    let dst = run_dir.join(consts::STATE_JSON);
    let mut state = shared_state.lock().expect("poisoned");
    state.status = status.into();
    match serde_json::to_string_pretty(&*state) {
        Ok(json) => {
            let _ = fs::write(&tmp, json);
            let _ = fs::rename(&tmp, &dst);
        }
        Err(e) => {
            tracing::error!("write_final_state serialize error: {e}");
        }
    }
}
