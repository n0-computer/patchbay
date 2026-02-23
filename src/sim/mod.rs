pub mod build;
pub mod env;
pub mod progress;
pub mod report;
pub mod runner;
pub mod steps;
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

/// Shared step fields.
#[derive(Deserialize, Clone, Default)]
pub struct StepShared {
    /// Extra environment variables injected into the spawned process.
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// One step in the sim sequence.
#[derive(Deserialize, Clone)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum Step {
    Run {
        #[serde(flatten)]
        shared: StepShared,
        /// Optional step id, required when a parser is configured.
        id: Option<String>,
        /// Target device name.
        device: String,
        /// Command to execute (supports `$NETSIM_*` and `${binary.<name>}` interpolation).
        cmd: Vec<String>,
        /// Optional parser applied to the step log after completion.
        parser: Option<Parser>,
        /// Optional baseline result id used by parsers that support comparisons.
        baseline: Option<String>,
    },
    Spawn {
        #[serde(flatten)]
        shared: StepShared,
        /// Step identifier — required for `spawn` steps; referenced by `wait-for`.
        id: String,
        /// Target device name.
        device: Option<String>,
        /// Command to execute for generic spawns.
        cmd: Option<Vec<String>>,
        /// Static delay after spawn before the step is considered ready.
        ready_after: Option<String>,
        /// Named regex captures from stdout; all captures must resolve before the
        /// step is marked ready.
        #[serde(default)]
        captures: HashMap<String, CaptureSpec>,
        /// `"iroh-transfer"` for the managed transfer spawn.
        kind: Option<String>,
        /// Provider device name (for `kind = "iroh-transfer"`).
        provider: Option<String>,
        /// Single fetcher device name.
        fetcher: Option<String>,
        /// Multiple fetcher device names (Phase 4 `count` expansion).
        fetchers: Option<Vec<String>>,
        /// Optional relay URL passed to both sides.
        relay_url: Option<String>,
        /// Extra CLI arguments passed to the fetcher binary.
        ///
        /// Use this for transfer-runtime knobs (for example, `--duration=20`).
        fetch_args: Option<Vec<String>>,
        /// Connection strategy: `"endpoint_id"` (default) or
        /// `"iroh_transfer_with_addrs"`.
        strategy: Option<String>,
        /// Optional parser applied to the step log after completion.
        parser: Option<Parser>,
        /// Optional baseline result id used by parsers that support comparisons.
        baseline: Option<String>,
    },
    Wait {
        duration: String,
    },
    WaitFor {
        id: String,
        /// Timeout override for `wait-for` (default: `"300s"`).
        timeout: Option<String>,
    },
    SetImpair {
        device: String,
        /// Interface name for `link-down`, `link-up`, or `set-impair`.
        interface: Option<String>,
        /// Impairment spec for `set-impair` (TOML string preset or inline table).
        impair: Option<toml::Value>,
    },
    SwitchRoute {
        device: String,
        /// Target interface for `switch-route`.
        to: String,
    },
    LinkDown {
        device: String,
        /// Interface name for `link-down`.
        interface: String,
    },
    LinkUp {
        device: String,
        /// Interface name for `link-up`.
        interface: String,
    },
    Assert {
        /// Simple boolean expression, e.g. `"xfer.final_conn_direct == true"`.
        check: String,
    },
}

/// Supported post-run parsers.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Parser {
    Iperf3Json,
}

/// Spec for a named capture read from process stdout.
#[derive(Deserialize, Clone)]
pub struct CaptureSpec {
    /// Regex pattern; capture group 1 (or the full match) becomes the value.
    pub stdout_regex: Option<String>,
}
