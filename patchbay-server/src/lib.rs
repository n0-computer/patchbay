//! Devtools HTTP server for patchbay labs.
//!
//! Serves the embedded UI, run discovery, per-run state/events/logs.
//! Reads from an output directory only, watching for new runs.

use std::{
    convert::Infallible,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use axum::{
    extract::{Path as AxPath, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse,
    },
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_stream::StreamExt;

// ── Mirrored constants ──────────────────────────────────────────────
//
// These are mirrored from `patchbay::consts`. If that module changes,
// update here too.

/// Lab-level event log (NDJSON).
const EVENTS_JSONL: &str = "events.jsonl";

/// Accumulated lab state snapshot.
const STATE_JSON: &str = "state.json";

/// Per-node full tracing log suffix.
const TRACING_JSONL_EXT: &str = "tracing.jsonl";

/// Default bind address for the devtools server.
pub const DEFAULT_UI_BIND: &str = "127.0.0.1:7421";

/// How often to poll events.jsonl.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How often to re-scan for new runs.
const RUN_SCAN_INTERVAL: Duration = Duration::from_secs(2);

// ── Run discovery ───────────────────────────────────────────────────
//
// Moved here from `patchbay::writer`. patchbay itself does not need
// run discovery; the server is the only consumer.

/// Metadata for a single Lab run directory.
///
/// A directory is a run if it contains `events.jsonl`.
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
pub fn discover_runs(base: &Path) -> anyhow::Result<Vec<RunInfo>> {
    let mut runs = Vec::new();
    let entries = fs::read_dir(base).map_err(|e| anyhow::anyhow!("read outdir base: {e}"))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if !path.join(EVENTS_JSONL).exists() {
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
    runs.sort_by(|a, b| b.name.cmp(&a.name));
    Ok(runs)
}

/// Minimal subset of `state.json` needed for run listing.
#[derive(Deserialize)]
struct StateJson {
    label: Option<String>,
    status: Option<String>,
}

fn read_run_metadata(run_dir: &Path) -> (Option<String>, Option<String>) {
    let Ok(contents) = fs::read_to_string(run_dir.join(STATE_JSON)) else {
        return (None, None);
    };
    let Ok(state) = serde_json::from_str::<StateJson>(&contents) else {
        return (None, None);
    };
    (state.label, state.status)
}

// ── Event record ────────────────────────────────────────────────────
//
// The server only needs `opid` for cursor-based filtering. The rest of
// the event is forwarded opaquely as JSON.

#[derive(Deserialize, Serialize)]
struct EventRecord {
    opid: u64,
    #[serde(flatten)]
    rest: serde_json::Value,
}

// ── Shared state ────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    base: PathBuf,
    runs_tx: broadcast::Sender<()>,
}

// ── Router construction ─────────────────────────────────────────────

fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index_html))
        .route("/api/runs", get(get_runs))
        .route("/api/runs/subscribe", get(runs_sse))
        .route("/api/runs/{run}/state", get(get_run_state))
        .route("/api/runs/{run}/events", get(run_events_sse))
        .route("/api/runs/{run}/logs", get(get_run_logs))
        .route("/api/runs/{run}/logs/{*path}", get(get_run_log_file))
        .route("/api/runs/{run}/files/{*path}", get(get_run_file))
        .with_state(state)
}

/// Creates an axum [`Router`] for serving a lab output directory.
pub fn router(base: PathBuf) -> Router {
    let (runs_tx, _) = broadcast::channel(16);
    let state = AppState {
        base: base.clone(),
        runs_tx: runs_tx.clone(),
    };

    // Background run scanner: notifies SSE subscribers when new runs appear.
    let scan_base = base;
    let scan_tx = runs_tx;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(RUN_SCAN_INTERVAL);
        let mut last_count = 0usize;
        loop {
            interval.tick().await;
            if let Ok(runs) = discover_runs(&scan_base) {
                if runs.len() != last_count {
                    last_count = runs.len();
                    let _ = scan_tx.send(());
                }
            }
        }
    });

    build_router(state)
}

/// Starts the devtools server on the given bind address.
pub async fn serve(base: PathBuf, bind: &str) -> anyhow::Result<()> {
    let app = router(base);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!("devtools server listening on {bind}");
    axum::serve(listener, app).await?;
    Ok(())
}

// ── Path safety ─────────────────────────────────────────────────────

/// Returns `None` if the resolved path escapes `base`.
///
/// Canonicalizes both paths to defeat symlink traversal.
fn safe_run_dir(base: &Path, run: &str) -> Option<PathBuf> {
    if run.contains("..") || run.starts_with('/') {
        return None;
    }
    let p = base.join(run);
    let canonical = p.canonicalize().ok()?;
    let canonical_base = base.canonicalize().ok()?;
    if !canonical.starts_with(&canonical_base) {
        return None;
    }
    Some(canonical)
}

/// Returns `None` if the sub-path escapes `run_dir`.
///
/// Canonicalizes both paths to defeat symlink traversal.
fn safe_sub_path(run_dir: &Path, sub: &str) -> Option<PathBuf> {
    if sub.contains("..") {
        return None;
    }
    let p = run_dir.join(sub);
    let canonical = p.canonicalize().ok()?;
    let canonical_run = run_dir.canonicalize().ok()?;
    if !canonical.starts_with(&canonical_run) {
        return None;
    }
    Some(canonical)
}

// ── Route handlers ──────────────────────────────────────────────────

async fn index_html() -> Html<&'static str> {
    Html(include_str!("../../ui/dist/index.html"))
}

async fn get_runs(State(state): State<AppState>) -> impl IntoResponse {
    let runs = discover_runs(&state.base).unwrap_or_default();
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        serde_json::to_string(&runs).unwrap_or_else(|_| "[]".to_string()),
    )
}

async fn runs_sse(
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.runs_tx.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|result| {
        result
            .ok()
            .map(|_| Ok::<_, Infallible>(Event::default().data("update")))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Deserialize)]
struct EventsQuery {
    after: Option<u64>,
}

async fn get_run_state(
    AxPath(run): AxPath<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let Some(run_dir) = safe_run_dir(&state.base, &run) else {
        return (
            StatusCode::FORBIDDEN,
            [("content-type", "application/json")],
            r#"{"error":"forbidden"}"#.to_string(),
        );
    };
    let path = run_dir.join(STATE_JSON);
    match tokio::fs::read_to_string(&path).await {
        Ok(contents) => (
            StatusCode::OK,
            [("content-type", "application/json")],
            contents,
        ),
        Err(_) => (
            StatusCode::NOT_FOUND,
            [("content-type", "application/json")],
            r#"{"error":"state.json not found"}"#.to_string(),
        ),
    }
}

async fn run_events_sse(
    AxPath(run): AxPath<String>,
    Query(params): Query<EventsQuery>,
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let after = params.after.unwrap_or(0);
    let Some(run_dir) = safe_run_dir(&state.base, &run) else {
        let empty = tokio_stream::empty();
        return Sse::new(Box::pin(empty)
            as std::pin::Pin<
                Box<dyn tokio_stream::Stream<Item = Result<Event, Infallible>> + Send>,
            >)
        .keep_alive(KeepAlive::default());
    };

    let events_path = run_dir.join(EVENTS_JSONL);
    let contents = tokio::fs::read_to_string(&events_path)
        .await
        .unwrap_or_default();
    let file_len = contents.len() as u64;

    let mut historical = Vec::new();
    for line in contents.lines() {
        if let Ok(event) = serde_json::from_str::<EventRecord>(line) {
            if event.opid > after {
                historical.push(event);
            }
        }
    }

    let historical_stream = tokio_stream::iter(historical);

    // Poll events.jsonl for new lines appended after the initial read.
    let poll_stream = async_stream::stream! {
        let mut pos = file_len;
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        loop {
            interval.tick().await;
            let Ok(contents) = tokio::fs::read(&events_path).await else {
                continue;
            };
            let len = contents.len() as u64;
            if len <= pos {
                continue;
            }
            let new_bytes = &contents[pos as usize..];
            // Advance only to the last complete newline to avoid partial lines.
            let advance = match new_bytes.iter().rposition(|&b| b == b'\n') {
                Some(idx) => idx + 1,
                None => continue,
            };
            let complete = &new_bytes[..advance];
            pos += advance as u64;
            let text = String::from_utf8_lossy(complete);
            for line in text.lines() {
                if let Ok(event) = serde_json::from_str::<EventRecord>(line) {
                    if event.opid > after {
                        yield event;
                    }
                }
            }
        }
    };

    let stream = historical_stream.chain(poll_stream).map(|event| {
        let data = serde_json::to_string(&event).unwrap_or_default();
        Ok::<_, Infallible>(Event::default().data(data))
    });
    Sse::new(Box::pin(stream)
        as std::pin::Pin<
            Box<dyn tokio_stream::Stream<Item = Result<Event, Infallible>> + Send>,
        >)
    .keep_alive(KeepAlive::default())
}

/// List log files in a run directory.
async fn get_run_logs(
    AxPath(run): AxPath<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let Some(run_dir) = safe_run_dir(&state.base, &run) else {
        return (
            StatusCode::FORBIDDEN,
            [("content-type", "application/json")],
            "[]".to_string(),
        );
    };
    let logs = scan_log_files(&run_dir).await;
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        serde_json::to_string(&logs).unwrap_or_else(|_| "[]".to_string()),
    )
}

#[derive(Deserialize)]
struct LogQuery {
    after: Option<u64>,
}

async fn get_run_log_file(
    AxPath((run, path)): AxPath<(String, String)>,
    Query(params): Query<LogQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let Some(run_dir) = safe_run_dir(&state.base, &run) else {
        return (StatusCode::FORBIDDEN, String::new());
    };
    let Some(file_path) = safe_sub_path(&run_dir, &path) else {
        return (StatusCode::FORBIDDEN, String::new());
    };
    tail_file(&file_path, params.after.unwrap_or(0)).await
}

async fn get_run_file(
    AxPath((run, path)): AxPath<(String, String)>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let Some(run_dir) = safe_run_dir(&state.base, &run) else {
        return (StatusCode::FORBIDDEN, String::new());
    };
    let Some(file_path) = safe_sub_path(&run_dir, &path) else {
        return (StatusCode::FORBIDDEN, String::new());
    };
    serve_file(&file_path).await
}

// ── Helpers ─────────────────────────────────────────────────────────

async fn tail_file(path: &Path, after_byte: u64) -> (StatusCode, String) {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return (StatusCode::NOT_FOUND, String::new()),
    };
    if after_byte > 0 {
        let _ = file.seek(std::io::SeekFrom::Start(after_byte)).await;
    }
    let mut buf = String::new();
    let _ = file.read_to_string(&mut buf).await;
    (StatusCode::OK, buf)
}

async fn serve_file(path: &Path) -> (StatusCode, String) {
    match tokio::fs::read_to_string(path).await {
        Ok(contents) => (StatusCode::OK, contents),
        Err(_) => (StatusCode::NOT_FOUND, String::new()),
    }
}

/// Parse the node name from `{kind}.{name}.<rest>`.
///
/// If no node prefix is present, returns `_run` for run-level files.
fn parse_node_name(filename: &str) -> String {
    let mut parts = filename.splitn(3, '.');
    let kind = parts.next();
    let name = parts.next();
    match (kind, name) {
        (Some("device" | "router"), Some(name)) if !name.is_empty() => name.to_string(),
        _ => "_run".to_string(),
    }
}

fn looks_like_json(text: &str) -> bool {
    let t = text.trim();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(t).is_ok()
}

fn looks_like_jsonl(text: &str) -> bool {
    let mut saw = false;
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{1e}');
        if line.is_empty() {
            continue;
        }
        if serde_json::from_str::<serde_json::Value>(line).is_err() {
            return false;
        }
        saw = true;
    }
    saw
}

fn looks_like_qlog_json_seq(text: &str) -> bool {
    let mut lines = text.lines();
    let Some(first_line) = lines.next() else {
        return false;
    };
    let first_line = first_line.trim().trim_start_matches('\u{1e}');
    if first_line.is_empty() {
        return false;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(first_line) else {
        return false;
    };
    let Some(obj) = v.as_object() else {
        return false;
    };
    let schema_ok = obj
        .get("file_schema")
        .and_then(|x| x.as_str())
        .map(|s| s.contains("qlog:file"))
        .unwrap_or(false);
    let format_ok = obj
        .get("serialization_format")
        .and_then(|x| x.as_str())
        .map(|s| s.eq_ignore_ascii_case("JSON-SEQ"))
        .unwrap_or(false);
    schema_ok && format_ok
}

/// Kind of log file exposed by the devtools API.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum LogKind {
    /// JSON lines tracing output (`*.tracing.jsonl`).
    TracingJsonl,
    /// Lab-level event log (`events.jsonl`).
    LabEvents,
    /// Generic JSON lines file (`*.jsonl`).
    Jsonl,
    /// Single JSON document (`*.json`).
    Json,
    /// qlog JSON sequence stream (`*.qlog`).
    Qlog,
    /// Text containing ANSI escape sequences.
    AnsiText,
    /// Plain UTF-8 text.
    Text,
}

#[derive(Serialize)]
struct LogEntry {
    node: String,
    kind: LogKind,
    path: String,
}

fn detect_log_kind(filename: &str, sample: &[u8]) -> Option<LogKind> {
    if filename == EVENTS_JSONL {
        return Some(LogKind::LabEvents);
    }
    if filename.ends_with(&format!(".{TRACING_JSONL_EXT}")) {
        return Some(LogKind::TracingJsonl);
    }

    let text = std::str::from_utf8(sample).ok()?;
    let text = text.trim_start_matches('\u{feff}');
    if filename.ends_with(".qlog") || filename.contains(".qlog-") || looks_like_qlog_json_seq(text)
    {
        return Some(LogKind::Qlog);
    }

    if filename.ends_with(".jsonl") || looks_like_jsonl(text) {
        return Some(LogKind::Jsonl);
    }
    if filename.ends_with(".json") || looks_like_json(text) {
        return Some(LogKind::Json);
    }
    if text.contains("\u{1b}[") {
        return Some(LogKind::AnsiText);
    }
    if filename.ends_with(".log") || filename.ends_with(".txt") {
        return Some(LogKind::Text);
    }

    Some(LogKind::Text)
}

async fn read_sample(path: &Path, max_bytes: usize) -> Option<Vec<u8>> {
    use tokio::io::AsyncReadExt;

    let mut file = tokio::fs::File::open(path).await.ok()?;
    let mut buf = vec![0u8; max_bytes];
    let n = file.read(&mut buf).await.ok()?;
    buf.truncate(n);
    Some(buf)
}

/// Scan a run directory for log files and return structured entries.
///
/// All per-node files follow the flat `{kind}.{name}.{ext}` pattern.
async fn scan_log_files(run_dir: &Path) -> Vec<LogEntry> {
    const SAMPLE_BYTES: usize = 16 * 1024;
    let mut logs = Vec::new();

    let Ok(mut entries) = tokio::fs::read_dir(run_dir).await else {
        return logs;
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let Ok(file_type) = entry.file_type().await else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let path = entry.path();
        let Some(sample) = read_sample(&path, SAMPLE_BYTES).await else {
            continue;
        };
        if sample.is_empty() {
            continue;
        }
        let Some(kind) = detect_log_kind(&name_str, &sample) else {
            continue;
        };
        logs.push(LogEntry {
            node: parse_node_name(&name_str),
            kind,
            path: name_str.to_string(),
        });
    }

    logs.sort_by(|a, b| {
        a.node
            .cmp(&b.node)
            .then(a.kind.cmp(&b.kind))
            .then(a.path.cmp(&b.path))
    });
    logs
}
