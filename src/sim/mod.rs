pub mod build;
pub mod capture;
pub mod env;
pub mod progress;
pub mod report;
pub mod runner;
pub mod steps;
pub mod topology;

pub use runner::run_sims;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

// ── Sim TOML types ────────────────────────────────────────────────────────────

/// The top-level sim file.
#[derive(Deserialize, Default)]
pub struct SimFile {
    #[serde(default)]
    pub sim: SimMeta,

    /// `[[extends]]` entries: each names a TOML file to inherit templates/groups/binaries from.
    #[serde(default)]
    pub extends: Vec<ExtendsEntry>,

    /// Named binary sources — `${binary.<name>}` in step commands.
    #[serde(default, rename = "binary")]
    pub binaries: Vec<BinarySpec>,

    /// Named step templates — `[[step-template]]`.
    #[serde(default, rename = "step-template")]
    pub step_templates: Vec<StepTemplateDef>,

    /// Named step groups — `[[step-group]]`.
    #[serde(default, rename = "step-group")]
    pub step_groups: Vec<StepGroupDef>,

    // ── Inline topology ────────────────────────────────────────────────────
    #[serde(default)]
    pub router: Vec<netsim::config::RouterCfg>,
    #[serde(default)]
    pub device: HashMap<String, toml::Value>,
    pub region: Option<HashMap<String, netsim::config::RegionConfig>>,

    // ── Steps (`[[step]]` array) ───────────────────────────────────────────
    /// Raw step entries — either `UseTemplate` (has `use` key) or `Concrete` (has `kind`/`action`).
    /// Expanded into `Vec<Step>` at load time by `expand_steps`.
    #[serde(default, rename = "step")]
    pub raw_steps: Vec<StepEntry>,
}

/// Metadata block at `[sim]`.
#[derive(Deserialize, Default)]
pub struct SimMeta {
    #[serde(default)]
    pub name: String,
    /// If set, the topology is loaded from `../topos/<topology>.toml` relative to the sim file.
    pub topology: Option<String>,
    /// Optional shared binary manifest file (legacy).
    pub binaries: Option<String>,
}

/// `[[extends]]` entry.
#[derive(Deserialize, Clone)]
pub struct ExtendsEntry {
    pub file: String,
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
    /// `cargo --bin <name>` to build.
    pub bin: Option<String>,
}

/// `[[step-template]]` entry: name + raw TOML table for merge-then-parse.
#[derive(Deserialize, Clone)]
pub struct StepTemplateDef {
    pub name: String,
    /// The remaining fields, stored raw for merging.
    #[serde(flatten)]
    pub raw: toml::value::Table,
}

/// `[[step-group]]` entry: name + sequence of raw step tables.
#[derive(Deserialize, Clone)]
pub struct StepGroupDef {
    pub name: String,
    #[serde(default, rename = "step")]
    pub steps: Vec<toml::value::Table>,
}

/// Top-level `[[step]]` entry.
///
/// `#[serde(untagged)]` tries `UseTemplate` first (succeeds when `use` key is
/// present), then falls back to `Concrete`.
#[derive(Deserialize, Clone)]
#[serde(untagged)]
pub enum StepEntry {
    UseTemplate(UseStep),
    Concrete(Step),
}

/// Call-site fields for `use = "template-or-group-name"`.
#[derive(Deserialize, Clone)]
pub struct UseStep {
    #[serde(rename = "use")]
    pub use_name: String,
    /// Group substitution variables (`${group.key}` tokens).
    #[serde(default)]
    pub vars: HashMap<String, String>,
    /// Override fields merged on top of the template.
    pub id: Option<String>,
    pub device: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub requires: Vec<String>,
    pub results: Option<StepResults>,
    pub timeout: Option<String>,
    #[serde(default)]
    pub captures: HashMap<String, CaptureSpec>,
}

/// Normalized result mapping for a step.
#[derive(Deserialize, Clone, Default)]
pub struct StepResults {
    /// `"step_id.capture_name"` or `".capture_name"` (relative to this step's id).
    pub duration: Option<String>,
    pub up_bytes: Option<String>,
    pub down_bytes: Option<String>,
}

/// One step in the sim sequence (after template/group expansion).
///
/// Tagged on `"action"` for backward compatibility with existing TOML files.
/// Template/group steps that use `kind = "..."` are normalized to `action = "..."`
/// during TOML table merge before deserialization (see `expand_steps`).
#[derive(Deserialize, Clone)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum Step {
    Run {
        id: Option<String>,
        device: String,
        cmd: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default)]
        parser: Parser,
        #[serde(default)]
        captures: HashMap<String, CaptureSpec>,
        #[serde(default)]
        requires: Vec<String>,
        results: Option<StepResults>,
    },
    Spawn {
        id: String,
        device: Option<String>,
        cmd: Option<Vec<String>>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default)]
        parser: Parser,
        ready_after: Option<String>,
        #[serde(default)]
        captures: HashMap<String, CaptureSpec>,
        #[serde(default)]
        requires: Vec<String>,
        results: Option<StepResults>,
    },
    Wait {
        duration: String,
    },
    WaitFor {
        id: String,
        timeout: Option<String>,
    },
    SetImpair {
        device: String,
        interface: Option<String>,
        impair: Option<toml::Value>,
    },
    /// Generic route switch (replaces `SwitchRoute`).
    SetDefaultRoute {
        device: String,
        to: String,
    },
    /// Legacy alias kept during porting.
    SwitchRoute {
        device: String,
        to: String,
    },
    LinkDown {
        device: String,
        interface: String,
    },
    LinkUp {
        device: String,
        interface: String,
    },
    Assert {
        check: Option<String>,
        #[serde(default)]
        checks: Vec<String>,
    },
    GenCerts {
        id: String,
        device: Option<String>,
        cn: Option<String>,
        san: Option<Vec<String>>,
    },
    GenFile {
        id: String,
        device: Option<String>,
        content: String,
    },
}

/// Output parser mode for `spawn`/`run` steps.
#[derive(Deserialize, Clone, Default, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum Parser {
    #[default]
    Text,
    Ndjson,
    Json,
    /// Legacy iperf3 parser (now handled by `Json` + capture picks).
    #[serde(alias = "iperf3-json")]
    Iperf3Json,
}

/// Spec for a named capture from a process pipe.
#[derive(Deserialize, Serialize, Clone, Default)]
pub struct CaptureSpec {
    /// Which pipe to read: `"stdout"` (default) or `"stderr"`.
    #[serde(default = "pipe_default")]
    pub pipe: String,
    /// Regex pattern; capture group 1 (or full match) becomes the value. All parsers.
    pub regex: Option<String>,
    /// Legacy field name — treated as `regex`.
    pub stdout_regex: Option<String>,
    /// Key=value guards on parsed JSON. `ndjson`/`json` only.
    #[serde(rename = "match", default)]
    pub match_fields: HashMap<String, String>,
    /// Dot-path into parsed JSON. `ndjson`/`json` only.
    pub pick: Option<String>,
}

impl CaptureSpec {
    /// Return the effective regex pattern (prefers `regex` over `stdout_regex`).
    pub fn effective_regex(&self) -> Option<&str> {
        self.regex.as_deref().or(self.stdout_regex.as_deref())
    }
}

fn pipe_default() -> String {
    "stdout".to_string()
}
