pub mod build;
pub mod capture;
pub mod env;
pub mod progress;
pub mod report;
pub mod runner;
pub mod steps;
pub mod topology;

use std::collections::HashMap;

// Re-export BinarySpec from the library so callers need only import one place.
pub use patchbay_utils::assets::BinarySpec;
pub use runner::{prepare_sims, run_sims};
use serde::{Deserialize, Deserializer, Serialize};

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

    /// Optional bulk build preparation configuration.
    #[serde(default, deserialize_with = "deserialize_prepare_specs")]
    pub prepare: Vec<PrepareSpec>,

    /// Named step templates — `[[step-template]]`.
    #[serde(default, rename = "step-template")]
    pub step_templates: Vec<StepTemplateDef>,

    /// Named step groups — `[[step-group]]`.
    #[serde(default, rename = "step-group")]
    pub step_groups: Vec<StepGroupDef>,

    // ── Inline topology (flattened from LabConfig) ──────────────────────────
    /// Inline router/device/region topology; mutually exclusive with `sim.topology`.
    #[serde(flatten)]
    pub topology: patchbay::config::LabConfig,

    // ── Steps (`[[step]]` array) ───────────────────────────────────────────
    /// Raw step entries — either `UseTemplate` (has `use` key) or `Concrete` (has `kind`/`action`).
    /// Expanded into `Vec<Step>` at load time by `expand_steps`.
    #[serde(default, rename = "step")]
    pub raw_steps: Vec<StepEntry>,
}

/// Metadata block at `[sim]`.
#[derive(Deserialize, Default)]
pub struct SimMeta {
    /// Human-readable name for this simulation (used in reports and logs).
    #[serde(default)]
    pub name: String,
    /// If set, the topology is loaded from `../topos/<topology>.toml` relative to the sim file.
    pub topology: Option<String>,
}

/// `[[extends]]` entry.
#[derive(Deserialize, Clone)]
pub struct ExtendsEntry {
    pub file: String,
}

/// Optional `[[prepare]]` entries for prebuilding workspace binaries.
#[derive(Deserialize, Clone, PartialEq, Eq, Default)]
pub struct PrepareSpec {
    /// Preparation mode (currently supports `build`).
    pub mode: Option<String>,
    /// Optional cargo feature list for prepare builds.
    #[serde(default)]
    pub features: Vec<String>,
    /// Build with all features for prepare builds.
    #[serde(default, rename = "all-features")]
    pub all_features: bool,
    /// Examples to prebuild in release mode.
    #[serde(default)]
    pub examples: Vec<String>,
    /// Binaries to prebuild in release mode.
    #[serde(default)]
    pub bins: Vec<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PrepareField {
    One(PrepareSpec),
    Many(Vec<PrepareSpec>),
}

fn deserialize_prepare_specs<'de, D>(deserializer: D) -> Result<Vec<PrepareSpec>, D::Error>
where
    D: Deserializer<'de>,
{
    let parsed = Option::<PrepareField>::deserialize(deserializer)?;
    Ok(match parsed {
        None => Vec::new(),
        Some(PrepareField::One(spec)) => vec![spec],
        Some(PrepareField::Many(specs)) => specs,
    })
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
    /// Capture key for the step duration (`"step_id.capture_name"` or `".capture_name"`).
    pub duration: Option<String>,
    /// Capture key for bytes uploaded.
    pub up_bytes: Option<String>,
    /// Capture key for bytes downloaded.
    pub down_bytes: Option<String>,
    /// Capture key for latency in milliseconds.
    pub latency_ms: Option<String>,
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
    #[serde(alias = "set-impair")]
    SetLinkCondition {
        device: String,
        interface: Option<String>,
        #[serde(alias = "impair")]
        condition: Option<toml::Value>,
    },
    SetDefaultRoute {
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
}

/// Spec for a named capture from a process pipe.
#[derive(Deserialize, Serialize, Clone, Default)]
pub struct CaptureSpec {
    /// Which pipe to read: `"stdout"` (default) or `"stderr"`.
    #[serde(default = "pipe_default")]
    pub pipe: String,
    /// Regex pattern; capture group 1 (or full match) becomes the value. All parsers.
    pub regex: Option<String>,
    /// Key=value guards on parsed JSON. `ndjson`/`json` only.
    #[serde(rename = "match", default)]
    pub match_fields: HashMap<String, String>,
    /// Dot-path into parsed JSON. `ndjson`/`json` only.
    pub pick: Option<String>,
}

fn pipe_default() -> String {
    "stdout".to_string()
}

