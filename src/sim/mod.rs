pub mod build;
pub mod env;
pub mod report;
pub mod runner;
pub mod topology;
pub mod transfer;

pub use runner::run_sims;

use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

// ── Sim TOML types ────────────────────────────────────────────────────────────

/// The top-level sim file.
///
/// Both inline topology (router/device/region tables) and external topology
/// (via `sim.topology`) are supported.
#[derive(Deserialize, Default)]
pub struct SimFile {
    #[serde(default)]
    pub sim: SimMeta,

    /// Named binary sources — `${binary.<name>}` in step commands.
    ///
    /// The first entry (or the one named `"transfer"`) is the default transfer
    /// binary used by `kind = "iroh-transfer"` steps.
    #[serde(default, rename = "binary")]
    pub binaries: Vec<BinarySpec>,

    // ── Inline topology (used when `sim.topology` is None) ────────────────
    #[serde(default)]
    pub router: Vec<netsim::config::RouterCfg>,
    #[serde(default)]
    pub device: HashMap<String, toml::Value>,
    pub region: Option<HashMap<String, netsim::config::RegionConfig>>,

    // ── Steps (`[[step]]` array) ──────────────────────────────────────────
    #[serde(default, rename = "step")]
    pub steps: Vec<Step>,
}

/// Metadata block at `[sim]`.
#[derive(Deserialize, Default)]
pub struct SimMeta {
    #[serde(default)]
    pub name: String,
    /// If set, the topology is loaded from `../topos/<topology>.toml`
    /// relative to the sim file; inline router/device tables are ignored.
    pub topology: Option<String>,
    /// Optional shared binary manifest file.
    ///
    /// This is loaded in addition to inline `[[binary]]` entries.
    pub binaries: Option<String>,
}

/// Binary source specification inside a `[[binary]]` entry.
#[derive(Deserialize, Clone)]
pub struct BinarySpec {
    /// Identifier used in `${binary.<name>}` substitutions.
    pub name: String,
    /// Local (possibly relative) path to a prebuilt binary.
    pub path: Option<PathBuf>,
    /// HTTP(S) URL to a tar.gz archive or bare binary.
    pub url: Option<String>,
    /// Git repository URL (combined with `commit` and `example`/`bin`).
    pub repo: Option<String>,
    /// Branch, tag, or SHA to check out (default: `"main"`).
    pub commit: Option<String>,
    /// `cargo --example <name>` to build.
    pub example: Option<String>,
    /// `cargo --bin <name>` to build (for secondary binaries in the same repo).
    pub bin: Option<String>,
}

/// One step in the sim sequence.
///
/// Fields that do not apply to a given `action` are silently ignored.
#[derive(Deserialize, Clone, Default)]
pub struct Step {
    /// Action type: `"run"`, `"spawn"`, `"wait"`, `"wait-for"`,
    /// `"set-impair"`, `"switch-route"`, `"link-down"`, `"link-up"`,
    /// `"assert"`.
    pub action: String,

    /// Step identifier — required for `spawn` steps; referenced by `wait-for`.
    pub id: Option<String>,
    /// Target device name.
    pub device: Option<String>,
    /// Command to execute (supports `$NETSIM_*` and `${binary.<name>}` interpolation).
    pub cmd: Option<Vec<String>>,
    /// Duration for `wait` steps (e.g. `"5s"`, `"300ms"`).
    pub duration: Option<String>,
    /// Timeout override for `wait-for` (default: `"300s"`).
    pub timeout: Option<String>,
    /// Static delay after spawn before the step is considered ready.
    pub ready_after: Option<String>,
    /// Named regex captures from stdout; all captures must resolve before the
    /// step is marked ready.
    #[serde(default)]
    pub captures: HashMap<String, CaptureSpec>,
    /// Extra environment variables injected into the spawned process.
    #[serde(default)]
    pub env: HashMap<String, String>,

    // ── iroh-transfer fields ──────────────────────────────────────────────
    /// `"iroh-transfer"` for the managed transfer spawn.
    pub kind: Option<String>,
    /// Provider device name (for `kind = "iroh-transfer"`).
    pub provider: Option<String>,
    /// Single fetcher device name.
    pub fetcher: Option<String>,
    /// Multiple fetcher device names (Phase 4 `count` expansion).
    pub fetchers: Option<Vec<String>>,
    /// Optional relay URL passed to both sides.
    pub relay_url: Option<String>,
    /// Extra CLI arguments passed to the fetcher binary.
    pub fetch_args: Option<Vec<String>>,
    /// Connection strategy: `"endpoint_id"` (default) or
    /// `"iroh_transfer_with_addrs"`.
    pub strategy: Option<String>,

    // ── network-op fields ─────────────────────────────────────────────────
    /// Impairment spec for `set-impair` (TOML string preset or inline table).
    pub impair: Option<toml::Value>,
    /// Interface name for `link-down`, `link-up`, or `set-impair`.
    pub interface: Option<String>,
    /// Target interface for `switch-route`.
    pub to: Option<String>,

    // ── assert ────────────────────────────────────────────────────────────
    /// Simple boolean expression, e.g. `"xfer.final_conn_direct == true"`.
    pub check: Option<String>,

    // ── post-process parser for command output ────────────────────────────
    /// Optional parser applied to the step log after completion.
    ///
    /// Supported values:
    /// - `iperf3-json`: parse `iperf3 -J` output into report rows.
    pub parser: Option<String>,
    /// Optional baseline result id used by parsers that support comparisons.
    ///
    /// For `parser = "iperf3-json"`, this computes `delta_mbps` and `delta_pct`
    /// relative to a previous iperf result id in the same sim run.
    pub baseline: Option<String>,
}

/// Spec for a named capture read from process stdout.
#[derive(Deserialize, Clone)]
pub struct CaptureSpec {
    /// Regex pattern; capture group 1 (or the full match) becomes the value.
    pub stdout_regex: Option<String>,
}
