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

/// Kind of per-node log file.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum LogKind {
    /// Full JSON tracing output (`*.tracing.jsonl`).
    Tracing,
    /// Extracted `_events` NDJSON (`*.events.jsonl`).
    Events,
    /// Plain text (stdout/stderr from runner).
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

/// Spawns the background run scanner task.
///
/// Must be called from within a tokio runtime context.
fn spawn_run_scanner(state: AppState) {
    let scan_state = state;
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
}

/// Creates an axum [`Router`] and a [`ServerHandle`] for registering live labs.
///
/// The background run scanner is started lazily on first request, so
/// the router can be constructed from a sync context (e.g. [`start_server`]).
pub fn router(base: PathBuf) -> (Router, ServerHandle) {
    let (state, handle) = build_state(base);
    let router = build_router(state.clone());
    // Wrap in a middleware that starts the scanner on first request.
    let scanner_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let router = router.layer(axum::middleware::from_fn(move |req, next: axum::middleware::Next| {
        let state = state.clone();
        let started = scanner_started.clone();
        async move {
            if !started.swap(true, std::sync::atomic::Ordering::Relaxed) {
                spawn_run_scanner(state);
            }
            next.run(req).await
        }
    }));
    (router, handle)
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

/// Handle returned by [`start_server`].
///
/// The server runs on a background thread and is shut down when this handle
/// is dropped.
pub struct RunningServer {
    addr: std::net::SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl RunningServer {
    /// Base HTTP URL of the running server.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Open the server URL in the default browser.
    pub fn open_browser(&self) -> anyhow::Result<()> {
        open::that(self.url())?;
        Ok(())
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        // Signal the server to shut down.
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        // Wait for the thread to finish.
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Start the devtools server on a background thread (sync-friendly).
///
/// Returns a handle that keeps the server alive. The server stops when
/// the handle is dropped.
pub fn start_server(base: PathBuf, bind: &str) -> anyhow::Result<RunningServer> {
    let listener = std::net::TcpListener::bind(bind)?;
    let addr = listener.local_addr()?;
    listener.set_nonblocking(true)?;
    let app = router_static(base);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let join = std::thread::Builder::new()
        .name("patchbay-server".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build tokio runtime for server");
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener.try_clone().unwrap())
                    .expect("convert TcpListener");
                axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .ok();
            });
        })?;

    Ok(RunningServer {
        addr,
        shutdown: Some(shutdown_tx),
        join: Some(join),
    })
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

/// Per-node log file suffixes we scan for.
const LOG_SUFFIXES: &[(&str, LogKind)] = &[
    (consts::TRACING_JSONL_EXT, LogKind::Tracing),
    (consts::EVENTS_JSONL_EXT, LogKind::Events),
    (consts::STDOUT_LOG_EXT, LogKind::Text),
    (consts::STDERR_LOG_EXT, LogKind::Text),
];

/// Parse a filename like `device.client.tracing.jsonl` into `(node, kind, path)`.
///
/// The format is `{node_kind}.{node_name}.{ext}` where ext matches one of [`LOG_SUFFIXES`].
fn parse_log_filename(filename: &str) -> Option<LogEntry> {
    for &(ext, kind) in LOG_SUFFIXES {
        if let Some(prefix) = filename.strip_suffix(&format!(".{ext}")) {
            // prefix is like "device.client" — extract just the node name (after first dot).
            let node = prefix
                .split_once('.')
                .map(|(_, name)| name)
                .unwrap_or(prefix);
            return Some(LogEntry {
                node: node.to_string(),
                kind,
                path: filename.to_string(),
            });
        }
    }
    None
}

/// Scan a run directory for log files and return structured entries.
///
/// All per-node files follow the flat `{kind}.{name}.{ext}` pattern.
async fn scan_log_files(run_dir: &Path) -> Vec<LogEntry> {
    let mut logs = Vec::new();

    let Ok(mut entries) = tokio::fs::read_dir(run_dir).await else {
        return logs;
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if let Some(log) = parse_log_filename(&name_str) {
            logs.push(log);
        }
    }

    logs.sort_by(|a, b| a.node.cmp(&b.node).then(a.kind.cmp(&b.kind)));
    logs
}
