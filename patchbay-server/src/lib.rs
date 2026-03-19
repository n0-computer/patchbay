//! Devtools HTTP server for patchbay labs.
//!
//! Serves the embedded UI, run discovery, per-run state/events/logs.
//! Reads from an output directory only, watching for new runs.

use std::{
    convert::Infallible,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use axum::{
    body::Bytes,
    extract::{Path as AxPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse,
    },
    routing::{get, post},
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
    #[serde(skip)]
    pub path: PathBuf,
    /// Human-readable label from `state.json`, if available.
    pub label: Option<String>,
    /// Lab status from `state.json` (e.g. `"running"`, `"stopping"`).
    pub status: Option<String>,
    /// Invocation group (first path component for nested runs, `None` for flat/direct).
    pub invocation: Option<String>,
}

/// Maximum directory depth to scan for run directories.
const MAX_SCAN_DEPTH: usize = 3;

/// Lists Lab output directories under `base`, newest-first.
///
/// If `base` itself contains `events.jsonl`, it is served as the sole run.
/// Otherwise, scans up to [`MAX_SCAN_DEPTH`] levels deep for directories
/// that contain `events.jsonl`.
pub fn discover_runs(base: &Path) -> anyhow::Result<Vec<RunInfo>> {
    // If the base dir itself is a run, serve only that.
    if base.join(EVENTS_JSONL).exists() {
        let name = base
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".".to_string());
        let (label, status) = read_run_metadata(base);
        return Ok(vec![RunInfo {
            name,
            path: base.to_path_buf(),
            label,
            status,
            invocation: None,
        }]);
    }

    let mut runs = Vec::new();
    scan_runs_recursive(base, base, 1, &mut runs)?;
    runs.sort_by(|a, b| b.name.cmp(&a.name));
    Ok(runs)
}

fn scan_runs_recursive(
    root: &Path,
    dir: &Path,
    depth: usize,
    runs: &mut Vec<RunInfo>,
) -> anyhow::Result<()> {
    if depth > MAX_SCAN_DEPTH {
        return Ok(());
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries {
        let entry = entry?;
        // Skip symlinks (e.g. runner's "latest" symlink) to avoid duplicates.
        if entry.file_type()?.is_symlink() {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.join(EVENTS_JSONL).exists() {
            // Use the relative path from root as the run name so nested
            // runs are addressable via the API (e.g. "sim-20260305/ping-e2e").
            let name = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let (label, status) = read_run_metadata(&path);
            // Derive invocation from the first path component (the timestamped
            // directory) when the run is nested more than one level deep.
            let invocation = name
                .split('/')
                .next()
                .filter(|first| *first != name)
                .map(str::to_string);
            runs.push(RunInfo {
                name,
                path,
                label,
                status,
                invocation,
            });
        } else {
            scan_runs_recursive(root, &path, depth + 1, runs)?;
        }
    }
    Ok(())
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

// ── Push configuration ──────────────────────────────────────────────

/// Configuration for the push endpoint.
#[derive(Clone)]
pub struct PushConfig {
    /// API key required in Authorization header.
    pub api_key: String,
    /// Directory where pushed runs are stored.
    pub run_dir: PathBuf,
}

// ── Shared state ────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    base: PathBuf,
    runs_tx: broadcast::Sender<()>,
    push: Option<Arc<PushConfig>>,
}

// ── Router construction ─────────────────────────────────────────────

fn build_router(state: AppState) -> Router {
    let mut r = Router::new()
        .route("/", get(index_html))
        .route("/runs", get(runs_index_html))
        .route("/api/runs", get(get_runs))
        .route("/api/runs/subscribe", get(runs_sse))
        .route("/api/runs/{run}/state", get(get_run_state))
        .route("/api/runs/{run}/events", get(run_events_sse))
        .route("/api/runs/{run}/logs", get(get_run_logs))
        .route("/api/runs/{run}/logs/{*path}", get(get_run_log_file))
        .route("/api/runs/{run}/files/{*path}", get(get_run_file))
        .route(
            "/api/invocations/{name}/combined-results",
            get(get_invocation_combined),
        );
    if state.push.is_some() {
        r = r.route("/api/push/{project}", post(push_run));
    }
    r.with_state(state)
}

/// Creates an axum [`Router`] for serving a lab output directory.
pub fn router(base: PathBuf) -> Router {
    build_app(base, None)
}

/// Creates an axum [`Router`] with optional push support.
pub fn build_app(base: PathBuf, push: Option<PushConfig>) -> Router {
    let (runs_tx, _) = broadcast::channel(16);
    let state = AppState {
        base: base.clone(),
        runs_tx: runs_tx.clone(),
        push: push.map(Arc::new),
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
/// Handles both nested runs (`base/{name}`) and single-run mode where
/// `base` itself is the run directory (name matches base's dirname).
fn safe_run_dir(base: &Path, run: &str) -> Option<PathBuf> {
    if run.contains("..") || run.starts_with('/') {
        return None;
    }
    // Single-run mode: base itself contains events.jsonl and run name
    // matches the base directory name.
    if base.join(EVENTS_JSONL).exists() {
        let base_name = base.file_name()?.to_string_lossy();
        if run == base_name || run == "." {
            return base.canonicalize().ok();
        }
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

/// Serve `combined-results.json` from an invocation directory.
async fn get_invocation_combined(
    AxPath(name): AxPath<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    if name.contains("..") || name.starts_with('/') {
        return (
            StatusCode::FORBIDDEN,
            [("content-type", "application/json")],
            r#"{"error":"forbidden"}"#.to_string(),
        );
    }
    let inv_dir = state.base.join(&name);
    let file = inv_dir.join("combined-results.json");
    // Verify the resolved path stays under base.
    let ok = file
        .canonicalize()
        .ok()
        .and_then(|c| state.base.canonicalize().ok().map(|b| c.starts_with(&b)))
        .unwrap_or(false);
    if !ok {
        return (
            StatusCode::NOT_FOUND,
            [("content-type", "application/json")],
            r#"{"runs":[]}"#.to_string(),
        );
    }
    match tokio::fs::read_to_string(&file).await {
        Ok(contents) => (
            StatusCode::OK,
            [("content-type", "application/json")],
            contents,
        ),
        Err(_) => (
            StatusCode::NOT_FOUND,
            [("content-type", "application/json")],
            r#"{"runs":[]}"#.to_string(),
        ),
    }
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

/// Check if content looks like tracing-subscriber JSON output.
///
/// These lines have `timestamp`, `level`, and `target` keys — the
/// standard shape emitted by `tracing_subscriber::fmt::json()`.
fn looks_like_tracing_jsonl(text: &str) -> bool {
    for line in text.lines().take(5) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            return false;
        };
        let obj = match v.as_object() {
            Some(o) => o,
            None => return false,
        };
        if obj.contains_key("timestamp") && obj.contains_key("level") && obj.contains_key("target")
        {
            return true;
        }
    }
    false
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

    if looks_like_tracing_jsonl(text) {
        return Some(LogKind::TracingJsonl);
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

// ── Run manifest (run.json) ─────────────────────────────────────────

/// Manifest included with pushed runs, providing CI context.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RunManifest {
    /// Project name (from URL path).
    #[serde(default)]
    pub project: String,
    /// Git branch name.
    #[serde(default)]
    pub branch: Option<String>,
    /// Git commit SHA.
    #[serde(default)]
    pub commit: Option<String>,
    /// PR number.
    #[serde(default)]
    pub pr: Option<u64>,
    /// PR URL.
    #[serde(default)]
    pub pr_url: Option<String>,
    /// When this run was created.
    #[serde(default)]
    pub created_at: Option<String>,
    /// Human-readable run title/label.
    #[serde(default)]
    pub title: Option<String>,
}

const RUN_JSON: &str = "run.json";

fn read_run_json(dir: &Path) -> Option<RunManifest> {
    let text = fs::read_to_string(dir.join(RUN_JSON)).ok()?;
    serde_json::from_str(&text).ok()
}

// ── Runs index page ─────────────────────────────────────────────────

/// Metadata for a run entry on the index page.
#[derive(Serialize)]
struct RunIndexEntry {
    /// Relative path within run_dir.
    path: String,
    /// Project name (first path component).
    project: String,
    /// run.json manifest if present.
    manifest: Option<RunManifest>,
    /// Timestamp from directory name.
    date: Option<String>,
}

/// Discover pushed runs for the index page.
/// Structure: run_dir/{project}/{date}-{uuid}/...
fn discover_pushed_runs(run_dir: &Path) -> Vec<RunIndexEntry> {
    let mut entries = Vec::new();
    let Ok(projects) = fs::read_dir(run_dir) else {
        return entries;
    };
    for project_entry in projects.flatten() {
        if !project_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let project = project_entry.file_name().to_string_lossy().to_string();
        let Ok(runs) = fs::read_dir(project_entry.path()) else {
            continue;
        };
        for run_entry in runs.flatten() {
            if !run_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let run_name = run_entry.file_name().to_string_lossy().to_string();
            let path = format!("{project}/{run_name}");
            let manifest = read_run_json(&run_entry.path());
            // Extract date from dirname: YYYYMMDD_HHMMSS-uuid
            let date = run_name.get(..15).map(|s| s.to_string());
            entries.push(RunIndexEntry {
                path,
                project: project.clone(),
                manifest,
                date,
            });
        }
    }
    entries.sort_by(|a, b| b.path.cmp(&a.path));
    entries
}

async fn runs_index_html(State(state): State<AppState>) -> Html<String> {
    let entries = discover_pushed_runs(&state.base);

    let mut html = String::from(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>patchbay runs</title>
<style>
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', system-ui, sans-serif;
         background: #0d1117; color: #c9d1d9; padding: 2rem; max-width: 1000px; margin: 0 auto; }
  h1 { margin-bottom: 1.5rem; color: #f0f6fc; font-size: 1.5rem; }
  .run { padding: 0.75rem 1rem; border: 1px solid #21262d; border-radius: 6px;
         margin-bottom: 0.5rem; display: flex; align-items: center; gap: 1rem;
         background: #161b22; }
  .run:hover { border-color: #388bfd; }
  .project { font-weight: 600; color: #58a6ff; min-width: 120px; }
  .meta { flex: 1; font-size: 0.875rem; color: #8b949e; }
  .meta a { color: #58a6ff; text-decoration: none; }
  .meta a:hover { text-decoration: underline; }
  .date { font-size: 0.8rem; color: #484f58; }
  .view-link { color: #58a6ff; text-decoration: none; font-size: 0.875rem; white-space: nowrap; }
  .view-link:hover { text-decoration: underline; }
  .empty { color: #484f58; padding: 2rem; text-align: center; }
  .badge { display: inline-block; padding: 0.1rem 0.5rem; border-radius: 3px;
           font-size: 0.75rem; background: #1f6feb33; color: #58a6ff; }
</style>
</head>
<body>
<h1>patchbay runs</h1>
"#,
    );

    if entries.is_empty() {
        html.push_str(r#"<div class="empty">No runs yet. Push results using the API.</div>"#);
    } else {
        for entry in &entries {
            html.push_str(r#"<div class="run">"#);
            html.push_str(&format!(
                r#"<span class="project">{}</span>"#,
                html_escape(&entry.project)
            ));

            html.push_str(r#"<div class="meta">"#);
            if let Some(m) = &entry.manifest {
                if let Some(branch) = &m.branch {
                    html.push_str(&format!(
                        r#"<span class="badge">{}</span> "#,
                        html_escape(branch)
                    ));
                }
                if let Some(commit) = &m.commit {
                    let short = &commit[..commit.len().min(7)];
                    html.push_str(&format!("<code>{short}</code> "));
                }
                if let Some(pr) = m.pr {
                    if let Some(url) = &m.pr_url {
                        html.push_str(&format!(
                            r#"<a href="{}">PR #{pr}</a> "#,
                            html_escape(url)
                        ));
                    } else {
                        html.push_str(&format!("PR #{pr} "));
                    }
                }
                if let Some(title) = &m.title {
                    html.push_str(&html_escape(title));
                }
            }
            html.push_str("</div>");

            if let Some(date) = &entry.date {
                html.push_str(&format!(r#"<span class="date">{date}</span>"#));
            }

            // Link into the devtools UI — the run path is the base for discover_runs
            html.push_str(&format!(
                r#" <a class="view-link" href="/?run={}">View &rarr;</a>"#,
                html_escape(&entry.path)
            ));

            html.push_str("</div>\n");
        }
    }

    html.push_str("</body></html>");
    Html(html)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ── Push endpoint ───────────────────────────────────────────────────

async fn push_run(
    AxPath(project): AxPath<String>,
    headers: HeaderMap,
    State(state): State<AppState>,
    body: Bytes,
) -> impl IntoResponse {
    let Some(push) = &state.push else {
        return (StatusCode::NOT_FOUND, "push not enabled".to_string());
    };

    // Validate API key
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let expected = format!("Bearer {}", push.api_key);
    if auth != expected {
        return (StatusCode::UNAUTHORIZED, "invalid api key".to_string());
    }

    // Validate project name (alphanumeric, hyphens, underscores)
    if project.is_empty()
        || !project
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return (
            StatusCode::BAD_REQUEST,
            "invalid project name".to_string(),
        );
    }

    // Create run directory: {run_dir}/{project}/{date}-{uuid}
    let now = chrono::Utc::now();
    let date = now.format("%Y%m%d_%H%M%S").to_string();
    let uuid = uuid::Uuid::new_v4();
    let run_name = format!("{date}-{uuid}");
    let run_dir = push.run_dir.join(&project).join(&run_name);

    if let Err(e) = std::fs::create_dir_all(&run_dir) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to create run dir: {e}"),
        );
    }

    // Extract tar.gz
    let decoder = flate2::read::GzDecoder::new(&body[..]);
    let mut archive = tar::Archive::new(decoder);
    if let Err(e) = archive.unpack(&run_dir) {
        // Clean up on failure
        let _ = std::fs::remove_dir_all(&run_dir);
        return (
            StatusCode::BAD_REQUEST,
            format!("failed to extract archive: {e}"),
        );
    }

    // Notify subscribers about new run
    let _ = state.runs_tx.send(());

    let view_path = format!("{project}/{run_name}");
    let result = serde_json::json!({
        "ok": true,
        "project": project,
        "run": run_name,
        "path": view_path,
    });

    (StatusCode::OK, serde_json::to_string(&result).unwrap())
}

// ── Retention watcher ───────────────────────────────────────────────

/// Background task that enforces a total size limit on the runs directory.
/// Deletes oldest runs (by directory name sort) when total exceeds `max_bytes`.
pub async fn retention_watcher(run_dir: PathBuf, max_bytes: u64) {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
        interval.tick().await;
        if let Err(e) = enforce_retention(&run_dir, max_bytes) {
            tracing::warn!("retention check failed: {e}");
        }
    }
}

fn enforce_retention(run_dir: &Path, max_bytes: u64) -> anyhow::Result<()> {
    // Collect all run dirs with their sizes, sorted oldest first
    let mut runs: Vec<(PathBuf, u64)> = Vec::new();

    let projects = fs::read_dir(run_dir)?;
    for project_entry in projects.flatten() {
        if !project_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(run_entries) = fs::read_dir(project_entry.path()) else {
            continue;
        };
        for run_entry in run_entries.flatten() {
            if !run_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let size = dir_size(&run_entry.path());
            runs.push((run_entry.path(), size));
        }
    }

    // Sort oldest first (by path, which includes date)
    runs.sort_by(|a, b| a.0.cmp(&b.0));

    let total: u64 = runs.iter().map(|(_, s)| s).sum();
    if total <= max_bytes {
        return Ok(());
    }

    let mut to_free = total - max_bytes;
    for (path, size) in &runs {
        if to_free == 0 {
            break;
        }
        tracing::info!("retention: removing {}", path.display());
        let _ = fs::remove_dir_all(path);
        to_free = to_free.saturating_sub(*size);
    }

    // Clean up empty project directories
    if let Ok(projects) = fs::read_dir(run_dir) {
        for entry in projects.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let Ok(mut contents) = fs::read_dir(entry.path()) else {
                    continue;
                };
                if contents.next().is_none() {
                    let _ = fs::remove_dir(entry.path());
                }
            }
        }
    }

    Ok(())
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let ft = entry.file_type().unwrap_or_else(|_| unreachable!());
            if ft.is_file() {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            } else if ft.is_dir() {
                total += dir_size(&entry.path());
            }
        }
    }
    total
}
