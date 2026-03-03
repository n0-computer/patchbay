//! Devtools HTTP server for patchbay labs.
//!
//! Serves the embedded UI, run discovery, per-run state/events/logs.
//! Supports two modes:
//!
//! - **Live**: one or more running [`Lab`]s register via [`ServerHandle`].
//! - **Static**: reads from an output directory only, watches for new runs.

use std::{
    collections::HashMap,
    convert::Infallible,
    path::{Path, PathBuf},
    sync::Arc,
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
use patchbay::{consts, discover_runs, Lab, LabEvent};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, RwLock};
use tokio_stream::StreamExt;

/// Default bind address for the devtools server.
pub const DEFAULT_UI_BIND: &str = "127.0.0.1:7421";

/// How often to poll events.jsonl in static mode.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How often to re-scan for new runs.
const RUN_SCAN_INTERVAL: Duration = Duration::from_secs(2);

// ── Shared state ───────────────────────────────────────────────────

/// Shared state for route handlers.
#[derive(Clone)]
struct AppState {
    /// Base output directory containing run subdirectories.
    base: PathBuf,
    /// Live labs keyed by run dir name (empty in static mode).
    live: Arc<RwLock<HashMap<String, Lab>>>,
    /// Broadcast channel for run list updates (SSE).
    runs_tx: broadcast::Sender<()>,
}

/// Handle for registering/unregistering live labs with the server.
#[derive(Clone)]
pub struct ServerHandle {
    live: Arc<RwLock<HashMap<String, Lab>>>,
    runs_tx: broadcast::Sender<()>,
}

impl ServerHandle {
    /// Register a live lab with the server. The `name` should match
    /// the run directory name (e.g. `20260303_143001-my-lab`).
    pub async fn register(&self, name: String, lab: Lab) {
        self.live.write().await.insert(name, lab);
        let _ = self.runs_tx.send(());
    }

    /// Unregister a live lab.
    pub async fn unregister(&self, name: &str) {
        self.live.write().await.remove(name);
        let _ = self.runs_tx.send(());
    }
}

// ── Log entry type ─────────────────────────────────────────────────

/// Kind of log file exposed by the devtools API.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum LogKind {
    /// JSON lines tracing output (`*.tracing.jsonl`).
    TracingJsonl,
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

// ── Path safety ────────────────────────────────────────────────────

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

// ── Router construction ────────────────────────────────────────────

fn build_state(base: PathBuf) -> (AppState, ServerHandle) {
    let live = Arc::new(RwLock::new(HashMap::new()));
    let (runs_tx, _) = broadcast::channel(16);
    let state = AppState {
        base,
        live: live.clone(),
        runs_tx: runs_tx.clone(),
    };
    let handle = ServerHandle { live, runs_tx };
    (state, handle)
}

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

/// Creates an axum [`Router`] and a [`ServerHandle`] for registering live labs.
///
/// Must be called from within an async (tokio) context.
pub fn router(base: PathBuf) -> (Router, ServerHandle) {
    let (state, handle) = build_state(base);
    // Spawn background run scanner.
    let scan_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(RUN_SCAN_INTERVAL);
        let mut last_count = 0usize;
        loop {
            interval.tick().await;
            if let Ok(runs) = discover_runs(&scan_state.base) {
                if runs.len() != last_count {
                    last_count = runs.len();
                    let _ = scan_state.runs_tx.send(());
                }
            }
        }
    });
    (build_router(state), handle)
}

/// Creates an axum [`Router`] for static mode (no live labs).
pub fn router_static(base: PathBuf) -> Router {
    let (router, _handle) = router(base);
    router
}

/// Starts the devtools server on the given bind address.
pub async fn serve(base: PathBuf, bind: &str) -> anyhow::Result<()> {
    let app = router_static(base);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!("devtools server listening on {bind}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Starts the devtools server with a live lab.
pub async fn serve_live(lab: &Lab, base: PathBuf, bind: &str) -> anyhow::Result<()> {
    let (app, handle) = router(base);
    // Auto-register the lab if it has a run_dir.
    if let Some(run_dir) = lab.run_dir() {
        if let Some(name) = run_dir.file_name().and_then(|n| n.to_str()) {
            handle.register(name.to_string(), lab.clone()).await;
        }
    }
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!("devtools server listening on {bind}");
    axum::serve(listener, app).await?;
    Ok(())
}

// ── Route handlers ─────────────────────────────────────────────────

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
    let path = run_dir.join(consts::STATE_JSON);
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

    // Read historical events from events.jsonl.
    let mut historical = Vec::new();
    let events_path = run_dir.join(consts::EVENTS_JSONL);
    let contents = tokio::fs::read_to_string(&events_path)
        .await
        .unwrap_or_default();
    let file_len = contents.len() as u64;
    for line in contents.lines() {
        if let Ok(event) = serde_json::from_str::<LabEvent>(line) {
            if event.opid > after {
                historical.push(event);
            }
        }
    }

    let historical_stream = tokio_stream::iter(historical);

    // Check if we have a live lab for this run.
    let live_lab = state.live.read().await.get(&run).cloned();

    if let Some(lab) = live_lab {
        // Live mode: subscribe to broadcast channel.
        let rx = lab.subscribe();
        let live_stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(
            move |result| match result {
                Ok(event) if event.opid > after => Some(event),
                _ => None,
            },
        );
        let combined = historical_stream.chain(live_stream);
        let stream = combined.map(|event| {
            let data = serde_json::to_string(&event).unwrap_or_default();
            Ok::<_, Infallible>(Event::default().data(data))
        });
        Sse::new(Box::pin(stream)
            as std::pin::Pin<
                Box<dyn tokio_stream::Stream<Item = Result<Event, Infallible>> + Send>,
            >)
        .keep_alive(KeepAlive::default())
    } else {
        // Static mode: poll events.jsonl for new lines.
        let poll_stream = async_stream::stream! {
            let events_path = run_dir.join(consts::EVENTS_JSONL);
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
                // Only advance cursor to the last complete newline to avoid
                // consuming a partial line written concurrently.
                let advance = match new_bytes.iter().rposition(|&b| b == b'\n') {
                    Some(idx) => idx + 1,
                    None => continue, // no complete line yet
                };
                let complete = &new_bytes[..advance];
                pos += advance as u64;
                let text = String::from_utf8_lossy(complete);
                for line in text.lines() {
                    if let Ok(event) = serde_json::from_str::<LabEvent>(line) {
                        if event.opid > after {
                            yield event;
                        }
                    }
                }
            }
        };
        let combined = historical_stream.chain(poll_stream);
        let stream = combined.map(|event| {
            let data = serde_json::to_string(&event).unwrap_or_default();
            Ok::<_, Infallible>(Event::default().data(data))
        });
        Sse::new(Box::pin(stream)
            as std::pin::Pin<
                Box<dyn tokio_stream::Stream<Item = Result<Event, Infallible>> + Send>,
            >)
        .keep_alive(KeepAlive::default())
    }
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

/// Serve a specific log file with optional byte offset.
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

/// Serve any file within a run directory.
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

// ── Helpers ────────────────────────────────────────────────────────

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

fn detect_log_kind(filename: &str, sample: &[u8]) -> Option<LogKind> {
    if filename.ends_with(&format!(".{}", consts::TRACING_JSONL_EXT)) {
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

    // For unknown extensions, include UTF-8 files as plain text.
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
