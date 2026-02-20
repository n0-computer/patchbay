//! Shared asset and binary-path helpers used by CLI frontends.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Parsed override mode from `--binary <name>:<mode>:<value>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinaryOverride {
    /// Build from a local source directory.
    Build(PathBuf),
    /// Download from URL.
    Fetch(String),
    /// Use/stage a concrete binary path.
    Path(PathBuf),
}

/// Path expansion mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathResolveMode {
    /// Local host execution.
    Local,
    /// VM-oriented host-side staging (prefer musl artifact path first).
    Vm,
}

impl PathResolveMode {
    /// Derive mode from `NETSIM_IN_VM`.
    pub fn from_env() -> Self {
        match std::env::var("NETSIM_IN_VM").ok().as_deref() {
            Some("1") | Some("true") | Some("yes") => Self::Vm,
            _ => Self::Local,
        }
    }
}

/// Parse repeatable `--binary` override arguments.
pub fn parse_binary_overrides(raw: &[String]) -> Result<HashMap<String, BinaryOverride>> {
    let mut out = HashMap::new();
    for item in raw {
        let mut parts = item.splitn(3, ':');
        let name = parts
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("invalid --binary override '{}': missing name", item))?;
        let mode = parts
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("invalid --binary override '{}': missing mode", item))?;
        let value = parts
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("invalid --binary override '{}': missing value", item))?;

        if out.contains_key(name) {
            bail!("duplicate --binary override for '{}'", name);
        }
        let parsed = match mode {
            "build" => BinaryOverride::Build(PathBuf::from(value)),
            "fetch" => BinaryOverride::Fetch(value.to_string()),
            "path" => BinaryOverride::Path(PathBuf::from(value)),
            _ => bail!(
                "invalid --binary override mode '{}' in '{}'; expected build|fetch|path",
                mode,
                item
            ),
        };
        out.insert(name.to_string(), parsed);
    }
    Ok(out)
}

/// Resolve an input path, expanding `target:<kind>/<name>` shortcuts.
pub fn resolve_binary_source_path(path: &Path, mode: PathResolveMode) -> Result<PathBuf> {
    let Some(raw) = path.to_str() else {
        return Ok(path.to_path_buf());
    };
    let Some(spec) = raw.strip_prefix("target:") else {
        return Ok(path.to_path_buf());
    };

    let (kind, name) = spec.split_once('/').ok_or_else(|| {
        anyhow!(
            "invalid target shortcut '{}': expected target:<kind>/<name>",
            raw
        )
    })?;
    if name.is_empty() {
        bail!("invalid target shortcut '{}': empty artifact name", raw);
    }

    let target_dir = resolve_target_dir()?;
    let mut candidates = Vec::new();

    if mode == PathResolveMode::Vm {
        candidates.push(target_artifact_path(
            &target_dir,
            Some("x86_64-unknown-linux-musl"),
            kind,
            name,
        )?);
    }

    if let Ok(rt) = std::env::var("RUST_TARGET") {
        if !rt.trim().is_empty() && mode != PathResolveMode::Vm {
            candidates.push(target_artifact_path(
                &target_dir,
                Some(rt.trim()),
                kind,
                name,
            )?);
        }
    }

    candidates.push(target_artifact_path(&target_dir, None, kind, name)?);

    for cand in &candidates {
        if cand.exists() {
            return Ok(cand.clone());
        }
    }

    bail!(
        "target shortcut '{}' could not be resolved; checked: {}",
        raw,
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
}

fn target_artifact_path(
    target_dir: &Path,
    target: Option<&str>,
    kind: &str,
    name: &str,
) -> Result<PathBuf> {
    let mut out = target_dir.to_path_buf();
    if let Some(t) = target {
        out.push(t);
    }
    out.push("release");
    match kind {
        "examples" => out.push("examples"),
        "bin" => {}
        other => bail!(
            "invalid target shortcut kind '{}': expected 'examples' or 'bin'",
            other
        ),
    }
    out.push(name);
    Ok(out)
}

fn resolve_target_dir() -> Result<PathBuf> {
    if let Ok(v) = std::env::var("NETSIM_TARGET_DIR") {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }

    #[derive(Deserialize)]
    struct CargoMeta {
        target_directory: String,
    }

    let out = std::process::Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .context("run cargo metadata for target dir resolution")?;
    if !out.status.success() {
        bail!("cargo metadata failed while resolving target dir");
    }
    let meta: CargoMeta =
        serde_json::from_slice(&out.stdout).context("parse cargo metadata output")?;
    if meta.target_directory.trim().is_empty() {
        bail!("cargo metadata returned an empty target_directory");
    }
    Ok(PathBuf::from(meta.target_directory))
}
