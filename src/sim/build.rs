use anyhow::{anyhow, bail, Context, Result};
use netsim::assets::{
    resolve_binary_source_path, resolve_target_artifact, resolve_target_dir, PathResolveMode,
};
use netsim::binary_cache::cached_binary_for_url;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::sim::BinarySpec;

/// Resolve a binary spec to a local path, building or downloading as needed.
pub async fn build_or_fetch_binary(
    spec: &BinarySpec,
    work_dir: &Path,
    build_root: &Path,
    no_build: bool,
) -> Result<PathBuf> {
    let mode = binary_mode(spec)?;
    match mode {
        "path" => {
            let path = spec
                .path
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("binary '{}' mode=path requires path", spec.name))?;
            let resolved = resolve_binary_source_path(path, PathResolveMode::from_env())?;
            if !resolved.exists() {
                bail!("binary path does not exist: {}", resolved.display());
            }
            Ok(resolved)
        }
        "fetch" => {
            let url = spec
                .url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("binary '{}' mode=fetch requires url", spec.name))?;
            download_binary(url, work_dir).await
        }
        "target" => {
            let (name, is_example) = artifact_name_kind(&BuildArtifact::from_spec(spec));
            let kind = if is_example { "examples" } else { "bin" };
            let path = resolve_target_artifact(kind, &name, PathResolveMode::from_env())?;
            if !path.exists() {
                bail!(
                    "target artifact for '{}' not found at {}",
                    spec.name,
                    path.display()
                );
            }
            Ok(path)
        }
        "build" => {
            let artifact = BuildArtifact::from_spec(spec);
            if no_build {
                return expected_existing_build_artifact(spec, build_root);
            }
            if let Some(repo) = &spec.repo {
                let commit = spec.commit.as_deref().unwrap_or("main");
                build_from_git(repo, commit, &artifact, work_dir).await
            } else {
                let source_dir = if let Some(path) = &spec.path {
                    resolve_binary_source_path(path, PathResolveMode::from_env())?
                } else {
                    build_root.to_path_buf()
                };
                build_local_binary(&artifact, &source_dir, work_dir).await
            }
        }
        other => bail!(
            "unsupported binary mode '{}' for '{}'; expected build|path|fetch|target",
            other,
            spec.name
        ),
    }
}

/// Build a named binary from a local checkout directory.
///
/// Tries `cargo build --example <name>` first, then falls back to
/// `cargo build --bin <name>`.
pub async fn build_local_binary(
    artifact: &BuildArtifact,
    source_dir: &Path,
    work_dir: &Path,
) -> Result<PathBuf> {
    let paths = build_local_binaries(std::slice::from_ref(artifact), source_dir, work_dir).await?;
    paths
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no artifact path returned for '{}'", artifact.name))
}

/// Build multiple local artifacts in one cargo invocation when possible.
pub async fn build_local_binaries(
    artifacts: &[BuildArtifact],
    source_dir: &Path,
    work_dir: &Path,
) -> Result<Vec<PathBuf>> {
    let source = source_dir.to_path_buf();
    let work = work_dir.to_path_buf();
    let artifacts = artifacts.to_vec();
    tokio::task::spawn_blocking(move || build_local_binaries_blocking(&artifacts, &source, &work))
        .await
        .context("join local binary build task")?
}

async fn download_binary(url: &str, work_dir: &Path) -> Result<PathBuf> {
    let url = url.to_string();
    let work_dir = work_dir.to_path_buf();
    tokio::task::spawn_blocking(move || cached_binary_for_url(&url, &work_dir))
        .await
        .context("join cached URL binary task")?
}

#[derive(Debug, Clone)]
pub struct BuildArtifact {
    pub name: String,
    pub example: Option<String>,
    pub bin: Option<String>,
    pub features: Vec<String>,
    pub all_features: bool,
}

impl BuildArtifact {
    fn from_spec(spec: &BinarySpec) -> Self {
        Self {
            name: spec.name.clone(),
            example: spec.example.clone(),
            bin: spec.bin.clone(),
            features: spec.features.clone(),
            all_features: spec.all_features,
        }
    }
}

fn build_local_binary_blocking(
    artifact: &BuildArtifact,
    source_dir: &Path,
    work_dir: &Path,
) -> Result<PathBuf> {
    let mut out =
        build_local_binaries_blocking(std::slice::from_ref(artifact), source_dir, work_dir)?;
    out.pop()
        .ok_or_else(|| anyhow!("no artifact path returned for '{}'", artifact.name))
}

fn build_local_binaries_blocking(
    artifacts: &[BuildArtifact],
    source_dir: &Path,
    _work_dir: &Path,
) -> Result<Vec<PathBuf>> {
    if artifacts.is_empty() {
        return Ok(Vec::new());
    }
    if !source_dir.is_dir() {
        bail!(
            "local binary source is not a directory: {}",
            source_dir.display()
        );
    }
    let explicit = artifacts
        .iter()
        .all(|artifact| artifact.example.is_some() || artifact.bin.is_some());
    if explicit {
        return build_in_workspace(source_dir, artifacts);
    }

    // Legacy fallback for specs without explicit bin/example.
    if artifacts.len() != 1 {
        bail!("building multiple artifacts requires explicit example or bin for each entry");
    }
    let artifact = &artifacts[0];
    let fallback = BuildArtifact {
        name: artifact.name.clone(),
        example: Some(artifact.name.clone()),
        bin: Some(artifact.name.clone()),
        features: artifact.features.clone(),
        all_features: artifact.all_features,
    };
    if let Ok(path) = build_local_binary_blocking(
        &BuildArtifact {
            bin: None,
            ..fallback.clone()
        },
        source_dir,
        _work_dir,
    ) {
        return Ok(vec![path]);
    }
    build_local_binary_blocking(
        &BuildArtifact {
            example: None,
            ..fallback
        },
        source_dir,
        _work_dir,
    )
    .map(|path| vec![path])
}

async fn build_from_git(
    repo: &str,
    commit: &str,
    artifact: &BuildArtifact,
    work_dir: &Path,
) -> Result<PathBuf> {
    let src_dir = work_dir.join("src").join(
        repo.rsplit('/')
            .next()
            .unwrap_or("repo")
            .trim_end_matches(".git"),
    );
    tokio::fs::create_dir_all(&src_dir)
        .await
        .context("create src dir")?;

    let src = src_dir.clone();
    let repo = repo.to_owned();
    let commit = commit.to_owned();
    let artifact = artifact.clone();

    tokio::task::spawn_blocking(move || {
        git_clone_or_update(&repo, &commit, &src)?;
        let mut paths = build_in_workspace(&src, std::slice::from_ref(&artifact))?;
        paths
            .pop()
            .ok_or_else(|| anyhow!("no artifact path returned for '{}'", artifact.name))
    })
    .await
    .context("join build task")?
}

fn build_in_workspace(source_dir: &Path, artifacts: &[BuildArtifact]) -> Result<Vec<PathBuf>> {
    let args = cargo_build_args(artifacts)?;
    let mut cmd = std::process::Command::new("cargo");
    cmd.args(&args).current_dir(source_dir);
    tracing::info!("Building: cargo {}", args.join(" "));
    let status = cmd.status().context("spawn cargo build")?;
    if !status.success() {
        bail!("cargo build failed in {}", source_dir.display());
    }

    let target_dir = metadata_target_dir(source_dir)?;
    let rust_target = std::env::var("RUST_TARGET").ok().filter(|s| !s.is_empty());
    Ok(artifacts
        .iter()
        .map(|artifact| {
            let (name, is_example) = artifact_name_kind(artifact);
            local_target_artifact_path(&target_dir, &name, is_example, rust_target.as_deref())
        })
        .collect())
}

fn cargo_build_args(artifacts: &[BuildArtifact]) -> Result<Vec<String>> {
    if artifacts.is_empty() {
        bail!("no artifacts to build");
    }
    let mut args: Vec<String> = vec!["build".into(), "--release".into()];
    if let Ok(target) = std::env::var("RUST_TARGET") {
        if !target.trim().is_empty() {
            args.push("--target".into());
            args.push(target.trim().to_string());
        }
    }
    let all_features = artifacts[0].all_features;
    if artifacts
        .iter()
        .any(|artifact| artifact.all_features != all_features)
    {
        bail!("all artifacts in one build call must share all-features setting");
    }
    let features = artifacts[0].features.clone();
    if artifacts
        .iter()
        .any(|artifact| artifact.features != features)
    {
        bail!("all artifacts in one build call must share feature list");
    }
    if all_features {
        args.push("--all-features".into());
    } else if !features.is_empty() {
        args.push("--features".into());
        args.push(features.join(","));
    }
    let mut seen = BTreeSet::new();
    for artifact in artifacts {
        let (name, is_example) = artifact_name_kind(artifact);
        let key = format!("{}:{}", if is_example { "example" } else { "bin" }, name);
        if !seen.insert(key) {
            continue;
        }
        if is_example {
            args.push("--example".into());
            args.push(name);
        } else {
            args.push("--bin".into());
            args.push(name);
        }
    }
    Ok(args)
}

fn artifact_name_kind(artifact: &BuildArtifact) -> (String, bool) {
    if let Some(example) = artifact.example.clone() {
        return (example, true);
    }
    if let Some(bin) = artifact.bin.clone() {
        return (bin, false);
    }
    (artifact.name.clone(), true)
}

fn metadata_target_dir(source_dir: &Path) -> Result<PathBuf> {
    let out = std::process::Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(source_dir)
        .output()
        .context("cargo metadata")?;
    if !out.status.success() {
        bail!("cargo metadata failed in {}", source_dir.display());
    }
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("parse cargo metadata")?;
    let target_dir = json["target_directory"]
        .as_str()
        .ok_or_else(|| anyhow!("missing target_directory"))?;
    Ok(PathBuf::from(target_dir))
}

fn binary_mode(spec: &BinarySpec) -> Result<&str> {
    if let Some(mode) = spec.mode.as_deref() {
        return Ok(mode);
    }
    if spec.path.is_some() {
        return Ok("path");
    }
    if spec.url.is_some() {
        return Ok("fetch");
    }
    if spec.repo.is_some() || spec.example.is_some() || spec.bin.is_some() {
        return Ok("build");
    }
    bail!(
        "binary '{}' has no mode and no source fields (expected mode=build|path|fetch)",
        spec.name
    )
}

fn git_clone_or_update(repo: &str, commit: &str, dir: &Path) -> Result<()> {
    if dir.join(".git").exists() {
        let status = std::process::Command::new("git")
            .args(["fetch", "origin"])
            .current_dir(dir)
            .status()
            .context("git fetch")?;
        if !status.success() {
            bail!("git fetch failed");
        }
    } else {
        let status = std::process::Command::new("git")
            .args(["clone", repo, "."])
            .current_dir(dir)
            .status()
            .context("git clone")?;
        if !status.success() {
            bail!("git clone failed");
        }
    }
    let status = std::process::Command::new("git")
        .args(["checkout", commit])
        .current_dir(dir)
        .status()
        .context("git checkout")?;
    if !status.success() {
        bail!("git checkout '{}' failed", commit);
    }
    Ok(())
}

fn local_target_artifact_path(
    target_base: &Path,
    binary_name: &str,
    is_example: bool,
    rust_target: Option<&str>,
) -> PathBuf {
    let mut path = target_base.to_path_buf();
    if let Some(t) = rust_target {
        path.push(t);
    }
    path.push("release");
    if is_example {
        path.push("examples");
    }
    path.push(binary_name);
    path
}

fn expected_existing_build_artifact(spec: &BinarySpec, build_root: &Path) -> Result<PathBuf> {
    if spec.repo.is_some() {
        bail!(
            "--no-build is not supported for repo-based build spec '{}'; use prepare first",
            spec.name
        );
    }
    let source_dir = if let Some(path) = &spec.path {
        resolve_binary_source_path(path, PathResolveMode::from_env())?
    } else {
        build_root.to_path_buf()
    };
    let target_dir = metadata_target_dir(&source_dir).or_else(|_| resolve_target_dir())?;
    let artifact = BuildArtifact::from_spec(spec);
    let (name, is_example) = artifact_name_kind(&artifact);
    let rust_target = std::env::var("RUST_TARGET").ok().filter(|s| !s.is_empty());
    let path = local_target_artifact_path(&target_dir, &name, is_example, rust_target.as_deref());
    if !path.exists() {
        bail!(
            "--no-build: expected artifact for '{}' not found at {}",
            spec.name,
            path.display()
        );
    }
    Ok(path)
}

#[cfg(test)]
fn extract_first_binary(archive: &Path, extract_dir: &Path) -> Result<PathBuf> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    let file = std::fs::File::open(archive).context("open archive")?;
    let gz = GzDecoder::new(file);
    let mut tar = Archive::new(gz);

    for entry in tar.entries().context("read tar entries")? {
        let mut entry = entry.context("read tar entry")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().context("entry path")?.into_owned();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.is_empty() || name.starts_with('.') {
            continue;
        }
        let ext = path.extension().unwrap_or_default().to_string_lossy();
        if ext.is_empty() || ext == "bin" {
            let dest = extract_dir.join(&*name);
            entry.unpack(&dest).context("unpack entry")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&dest)?.permissions();
                perms.set_mode(perms.mode() | 0o111);
                std::fs::set_permissions(&dest, perms)?;
            }
            return Ok(dest);
        }
    }
    bail!("no binary found in archive {}", archive.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tar::{Builder, Header};

    fn temp_dir(prefix: &str) -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("netsim-{prefix}-{ts}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn extract_first_binary_skips_directory_entries() {
        let dir = temp_dir("build-extract");
        let archive = dir.join("relay.tar.gz");
        let extract_dir = dir.join("bins");
        std::fs::create_dir_all(&extract_dir).expect("create extract dir");

        let file = std::fs::File::create(&archive).expect("create archive");
        let enc = GzEncoder::new(file, Compression::default());
        let mut tar = Builder::new(enc);

        // Directory entry comes first and must be ignored.
        let mut dir_hdr = Header::new_gnu();
        dir_hdr.set_entry_type(tar::EntryType::Directory);
        dir_hdr.set_mode(0o755);
        dir_hdr.set_size(0);
        dir_hdr.set_cksum();
        tar.append_data(&mut dir_hdr, "bundle/", std::io::empty())
            .expect("append dir entry");

        let payload = b"#!/bin/sh\necho relay\n";
        let mut file_hdr = Header::new_gnu();
        file_hdr.set_entry_type(tar::EntryType::Regular);
        file_hdr.set_mode(0o755);
        file_hdr.set_size(payload.len() as u64);
        file_hdr.set_cksum();
        tar.append_data(&mut file_hdr, "bundle/iroh-relay", &payload[..])
            .expect("append file entry");

        tar.into_inner()
            .expect("finish tar")
            .finish()
            .expect("finish gzip");

        let out = extract_first_binary(&archive, &extract_dir).expect("extract first binary");
        assert!(out.is_file(), "expected file output, got {}", out.display());
        assert_eq!(out.file_name().and_then(|s| s.to_str()), Some("iroh-relay"));
    }
}
