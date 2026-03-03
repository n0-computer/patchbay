//! Per-namespace tracing file writer.
//!
//! Each namespace worker thread gets a thread-local subscriber wrapper via
//! [`install_namespace_subscriber`]. This wrapper **delegates all span tracking**
//! to the existing global subscriber (preserving a single `Registry`) and only
//! adds file writing for events.
//!
//! This avoids the classic `sharded_slab` panic ("tried to drop a ref to Id(N),
//! but no such span exists!") that occurs when multiple `Registry` instances
//! coexist and spans cross thread/subscriber boundaries.
//!
//! When a `run_dir` is provided, two files are written:
//! - `{prefix}.tracing.jsonl` — all events as JSON (level-filtered)
//! - `{prefix}.events.jsonl` — only `_events::` targets as simple NDJSON

use std::{
    collections::HashMap,
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use crate::consts;

/// A file writer that defers creation until the first write.
/// This avoids creating empty files for namespaces that never log.
struct LazyFile {
    path: PathBuf,
    inner: Option<BufWriter<File>>,
}

impl LazyFile {
    fn new(path: PathBuf) -> Self {
        Self { path, inner: None }
    }

    fn get_or_create(&mut self) -> std::io::Result<&mut BufWriter<File>> {
        if self.inner.is_none() {
            let file = File::create(&self.path)?;
            self.inner = Some(BufWriter::new(file));
        }
        Ok(self.inner.as_mut().unwrap())
    }
}

impl Write for LazyFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.get_or_create()?.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(ref mut w) = self.inner {
            w.flush()
        } else {
            Ok(())
        }
    }
}

/// Visitor that collects tracing event fields into a JSON map.
struct JsonFieldVisitor {
    fields: serde_json::Map<String, serde_json::Value>,
}

impl JsonFieldVisitor {
    fn new() -> Self {
        Self {
            fields: serde_json::Map::new(),
        }
    }
}

impl tracing::field::Visit for JsonFieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::String(format!("{:?}", value)),
        );
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        if let Some(n) = serde_json::Number::from_f64(value) {
            self.fields
                .insert(field.name().to_string(), serde_json::Value::Number(n));
        } else {
            self.fields.insert(
                field.name().to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), serde_json::Value::Bool(value));
    }
}

#[derive(Clone)]
struct SpanInfo {
    name: String,
    fields: serde_json::Map<String, serde_json::Value>,
    parent: Option<u64>,
}

#[derive(Default)]
struct SpanState {
    spans: HashMap<u64, SpanInfo>,
    stacks: HashMap<std::thread::ThreadId, Vec<u64>>,
}

/// A subscriber wrapper that delegates **all span tracking** to the global
/// subscriber and adds file writing for events.
///
/// This is installed as the thread-default via `tracing::subscriber::set_default`.
/// Because all span IDs live in the single global `Registry`, spans can freely
/// cross thread boundaries without panicking.
struct NsWriterSubscriber {
    /// The existing global subscriber — handles all span lifecycle.
    inner: tracing::Dispatch,
    /// JSON tracing log writer (all events above file_level).
    tracing_writer: Mutex<LazyFile>,
    /// Extracted `_events::` NDJSON writer.
    events_writer: Mutex<LazyFile>,
    /// Minimum level for the tracing file (from PATCHBAY_LOG / RUST_LOG).
    file_level: tracing::level_filters::LevelFilter,
    /// Local span metadata storage to emit tracing-subscriber-compatible
    /// `span` and `spans` fields in JSON logs.
    span_state: Mutex<SpanState>,
}

impl NsWriterSubscriber {
    fn thread_id() -> std::thread::ThreadId {
        std::thread::current().id()
    }

    fn current_span_id(&self, state: &SpanState) -> Option<u64> {
        state
            .stacks
            .get(&Self::thread_id())
            .and_then(|s| s.last().copied())
            .or_else(|| self.inner.current_span().id().map(|id| id.into_u64()))
    }

    fn on_new_span(&self, id: &tracing::span::Id, attrs: &tracing::span::Attributes<'_>) {
        let mut visitor = JsonFieldVisitor::new();
        attrs.record(&mut visitor);
        let mut state = match self.span_state.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        let parent = if let Some(parent) = attrs.parent() {
            Some(parent.into_u64())
        } else if attrs.is_root() {
            None
        } else if attrs.is_contextual() {
            self.current_span_id(&state)
        } else {
            None
        };
        state.spans.insert(
            id.into_u64(),
            SpanInfo {
                name: attrs.metadata().name().to_string(),
                fields: visitor.fields,
                parent,
            },
        );
    }

    fn on_record(&self, span: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        let mut visitor = JsonFieldVisitor::new();
        values.record(&mut visitor);
        let mut state = match self.span_state.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        if let Some(info) = state.spans.get_mut(&span.into_u64()) {
            for (k, v) in visitor.fields {
                info.fields.insert(k, v);
            }
        }
    }

    fn on_enter(&self, span: &tracing::span::Id) {
        let mut state = match self.span_state.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        state
            .stacks
            .entry(Self::thread_id())
            .or_default()
            .push(span.into_u64());
    }

    fn on_exit(&self, span: &tracing::span::Id) {
        let mut state = match self.span_state.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        if let Some(stack) = state.stacks.get_mut(&Self::thread_id()) {
            if let Some(pos) = stack.iter().rposition(|sid| *sid == span.into_u64()) {
                stack.remove(pos);
            }
            if stack.is_empty() {
                state.stacks.remove(&Self::thread_id());
            }
        }
    }

    fn span_chain_for_event(
        &self,
        event: &tracing::Event<'_>,
    ) -> Vec<serde_json::Map<String, serde_json::Value>> {
        let state = match self.span_state.lock() {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let mut chain = Vec::new();
        let mut current = if let Some(parent) = event.parent() {
            Some(parent.into_u64())
        } else if event.is_root() {
            None
        } else if event.is_contextual() {
            self.current_span_id(&state)
        } else {
            None
        };
        while let Some(id) = current {
            let Some(info) = state.spans.get(&id) else {
                break;
            };
            let mut span_obj = serde_json::Map::new();
            span_obj.insert(
                "name".to_string(),
                serde_json::Value::String(info.name.clone()),
            );
            for (k, v) in &info.fields {
                span_obj.insert(k.clone(), v.clone());
            }
            chain.push(span_obj);
            current = info.parent;
        }
        chain.reverse();
        chain
    }

    fn write_event_to_files(&self, event: &tracing::Event<'_>) {
        let meta = event.metadata();
        let target = meta.target();
        let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true);

        // Write to .events.jsonl — only _events:: targets.
        if let Some(kind) = target.split_once("_events::").map(|(_, k)| k) {
            let mut visitor = JsonFieldVisitor::new();
            event.record(&mut visitor);
            visitor.fields.remove("message");
            visitor.fields.insert(
                "kind".to_string(),
                serde_json::Value::String(kind.to_string()),
            );
            visitor.fields.insert(
                "timestamp".to_string(),
                serde_json::Value::String(timestamp.clone()),
            );
            if let Ok(mut w) = self.events_writer.lock() {
                let _ = serde_json::to_writer(&mut *w, &visitor.fields);
                let _ = w.write_all(b"\n");
                let _ = w.flush();
            }
        }

        // Write to .tracing.jsonl — matching tracing-subscriber's JSON format:
        // {"timestamp":"...","level":"INFO","fields":{"message":"...","key":"val"},"target":"mod::path"}
        if *meta.level() <= self.file_level || target.contains("_events::") {
            let mut visitor = JsonFieldVisitor::new();
            event.record(&mut visitor);
            let mut obj = serde_json::Map::new();
            obj.insert(
                "timestamp".to_string(),
                serde_json::Value::String(timestamp),
            );
            obj.insert(
                "level".to_string(),
                serde_json::Value::String(meta.level().to_string()),
            );
            // All event fields (including message) go into "fields" — same as
            // tracing_subscriber::fmt::layer().json().
            obj.insert(
                "fields".to_string(),
                serde_json::Value::Object(visitor.fields),
            );
            obj.insert(
                "target".to_string(),
                serde_json::Value::String(target.to_string()),
            );
            let span_chain = self.span_chain_for_event(event);
            if !span_chain.is_empty() {
                let current = span_chain[span_chain.len() - 1].clone();
                obj.insert("span".to_string(), serde_json::Value::Object(current));
                obj.insert(
                    "spans".to_string(),
                    serde_json::Value::Array(
                        span_chain
                            .into_iter()
                            .map(serde_json::Value::Object)
                            .collect(),
                    ),
                );
            }
            if let Ok(mut w) = self.tracing_writer.lock() {
                let _ = serde_json::to_writer(&mut *w, &obj);
                let _ = w.write_all(b"\n");
                let _ = w.flush();
            }
        }
    }
}

impl tracing::Subscriber for NsWriterSubscriber {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        self.inner.enabled(metadata)
            || *metadata.level() <= self.file_level
            || metadata.target().contains("_events::")
    }

    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let id = self.inner.new_span(span);
        self.on_new_span(&id, span);
        id
    }

    fn record(&self, span: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        self.inner.record(span, values);
        self.on_record(span, values);
    }

    fn record_follows_from(&self, span: &tracing::span::Id, follows: &tracing::span::Id) {
        self.inner.record_follows_from(span, follows);
    }

    fn event(&self, event: &tracing::Event<'_>) {
        self.inner.event(event);
        self.write_event_to_files(event);
    }

    fn enter(&self, span: &tracing::span::Id) {
        self.inner.enter(span);
        self.on_enter(span);
    }

    fn exit(&self, span: &tracing::span::Id) {
        self.inner.exit(span);
        self.on_exit(span);
    }

    fn clone_span(&self, id: &tracing::span::Id) -> tracing::span::Id {
        self.inner.clone_span(id)
    }

    fn try_close(&self, id: tracing::span::Id) -> bool {
        let raw = id.into_u64();
        let closed = self.inner.try_close(id);
        if closed {
            if let Ok(mut state) = self.span_state.lock() {
                state.spans.remove(&raw);
                for stack in state.stacks.values_mut() {
                    stack.retain(|sid| *sid != raw);
                }
            }
        }
        closed
    }

    fn current_span(&self) -> tracing_core::span::Current {
        self.inner.current_span()
    }
}

/// Installs a thread-local tracing subscriber for a namespace worker.
///
/// `log_prefix` is the file stem like `"device.client"` or `"router.home"`.
/// Files are named `{log_prefix}.tracing.jsonl` and `{log_prefix}.events.jsonl`.
///
/// Returns a `DefaultGuard` that must be held for the thread's lifetime.
/// When `run_dir` is `Some`, installs a wrapper that delegates span tracking
/// to the global subscriber and adds file writing. When `None`, returns `None`
/// (the thread inherits the global subscriber as-is).
pub(crate) fn install_namespace_subscriber(
    log_prefix: &str,
    run_dir: Option<&Path>,
) -> Option<tracing::subscriber::DefaultGuard> {
    let run_dir = run_dir?;

    // Ensure run_dir exists (writer may not have created it yet).
    let _ = std::fs::create_dir_all(run_dir);

    let tracing_path = run_dir.join(format!("{log_prefix}.{}", consts::TRACING_JSONL_EXT));
    let events_path = run_dir.join(format!("{log_prefix}.{}", consts::EVENTS_JSONL_EXT));

    let file_level_str = std::env::var("PATCHBAY_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| "info".to_string());
    let file_level: tracing::level_filters::LevelFilter = file_level_str
        .parse()
        .unwrap_or(tracing::level_filters::LevelFilter::INFO);

    // Capture the current (global) subscriber's Dispatch — all span tracking
    // is delegated to it, keeping a single Registry for the whole process.
    let inner = tracing::dispatcher::get_default(|d| d.clone());

    let subscriber = NsWriterSubscriber {
        inner,
        tracing_writer: Mutex::new(LazyFile::new(tracing_path)),
        events_writer: Mutex::new(LazyFile::new(events_path)),
        file_level,
        span_state: Mutex::new(SpanState::default()),
    };

    Some(tracing::subscriber::set_default(subscriber))
}
