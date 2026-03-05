//! Shared filename constants for the run output directory.
//!
//! All per-node files use the pattern `{kind}.{name}.{ext}` to avoid
//! conflicts with lab-level files like `events.jsonl`.

/// Lab-level event log (NDJSON).
pub const EVENTS_JSONL: &str = "events.jsonl";

/// Accumulated lab state snapshot.
pub const STATE_JSON: &str = "state.json";

/// Temporary file for atomic state writes.
pub const STATE_JSON_TMP: &str = "state.json.tmp";

/// Per-node full tracing log suffix.
pub const TRACING_JSONL_EXT: &str = "tracing.jsonl";

/// Per-node human-readable ANSI tracing log suffix.
pub const TRACING_LOG_EXT: &str = "tracing.log";

/// Per-node extracted events suffix.
pub const EVENTS_JSONL_EXT: &str = "events.jsonl";

/// Per-node stdout log suffix.
pub const STDOUT_LOG_EXT: &str = "stdout.log";

/// Per-node stderr log suffix.
pub const STDERR_LOG_EXT: &str = "stderr.log";

/// Node kind prefix for devices.
pub const KIND_DEVICE: &str = "device";

/// Node kind prefix for routers.
pub const KIND_ROUTER: &str = "router";

/// Constructs a per-node filename: `{kind}.{name}.{ext}`.
///
/// Example: `node_file("device", "client", "tracing.jsonl")` → `"device.client.tracing.jsonl"`.
pub fn node_file(kind: &str, name: &str, ext: &str) -> String {
    format!("{kind}.{name}.{ext}")
}
