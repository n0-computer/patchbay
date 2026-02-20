use anyhow::{bail, Context, Result};
use netsim::assets::{resolve_binary_source_path, PathResolveMode};
use std::path::{Path, PathBuf};

use crate::sim::BinarySpec;

/// Resolve a binary spec to a local path, building or downloading as needed.
pub async fn build_or_fetch_binary(spec: &BinarySpec, work_dir: &Path) -> Result<PathBuf> {
    if let Some(path) = &spec.path {
        let resolved = resolve_binary_source_path(path, PathResolveMode::from_env())?;
        if !resolved.exists() {
            bail!("binary path does not exist: {}", resolved.display());
        }
        return Ok(resolved);
    }

    if let Some(url) = &spec.url {
        return download_binary(url, work_dir).await;
    }

    if let Some(repo) = &spec.repo {
        let commit = spec.commit.as_deref().unwrap_or("main");
        return build_from_git(
            repo,
            commit,
            spec.example.as_deref(),
            spec.bin.as_deref(),
            work_dir,
        )
        .await;
    }

    bail!("binary spec must have url, path, or repo");
}

/// Build a named binary from a local checkout directory.
///
/// Tries `cargo build --example <name>` first, then falls back to
/// `cargo build --bin <name>`.
pub async fn build_local_binary(name: &str, source_dir: &Path, work_dir: &Path) -> Result<PathBuf> {
    let source = source_dir.to_path_buf();
    let work = work_dir.to_path_buf();
    let name = name.to_string();
    tokio::task::spawn_blocking(move || build_local_binary_blocking(&name, &source, &work))
        .await
        .context("join local binary build task")?
}

async fn download_binary(url: &str, work_dir: &Path) -> Result<PathBuf> {
    let bins_dir = work_dir.join("bins");
    tokio::fs::create_dir_all(&bins_dir)
        .await
        .context("create bins dir")?;

    // Derive a local filename from the URL.
    let filename = url
        .rsplit('/')
        .next()
        .unwrap_or("binary")
        .split('?')
        .next()
        .unwrap_or("binary");
    let dest = bins_dir.join(filename);

    if dest.exists() {
        tracing::debug!(?dest, "binary already cached, skipping download");
        // If it's an archive, find the actual binary inside.
        if filename.ends_with(".tar.gz") || filename.ends_with(".tgz") {
            return find_in_archive(&dest, &bins_dir).await;
        }
        return Ok(dest);
    }

    tracing::info!(url, dest = %dest.display(), "downloading binary");
    let response = reqwest::get(url).await.context("GET binary url")?;
    if !response.status().is_success() {
        bail!("download failed: {} {}", url, response.status());
    }
    let bytes = response.bytes().await.context("read binary response")?;
    tokio::fs::write(&dest, &bytes)
        .await
        .context("write binary")?;

    if filename.ends_with(".tar.gz") || filename.ends_with(".tgz") {
        find_in_archive(&dest, &bins_dir).await
    } else {
        // Mark as executable.
        set_executable(&dest)?;
        Ok(dest)
    }
}

async fn find_in_archive(archive: &Path, extract_dir: &Path) -> Result<PathBuf> {
    let archive = archive.to_owned();
    let extract_dir = extract_dir.to_owned();
    tokio::task::spawn_blocking(move || extract_first_binary(&archive, &extract_dir))
        .await
        .context("join extract task")?
}

fn extract_first_binary(archive: &Path, extract_dir: &Path) -> Result<PathBuf> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    let file = std::fs::File::open(archive).context("open archive")?;
    let gz = GzDecoder::new(file);
    let mut tar = Archive::new(gz);

    let mut found: Option<PathBuf> = None;
    for entry in tar.entries().context("read tar entries")? {
        let mut entry = entry.context("read tar entry")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().context("entry path")?.into_owned();
        // Skip directories and dotfiles
        if path.components().count() == 0 {
            continue;
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        // Take the first regular file that looks like a binary (no extension
        // or known extension list)
        let ext = path.extension().unwrap_or_default().to_string_lossy();
        if ext.is_empty() || ext == "bin" {
            let dest = extract_dir.join(&*name);
            entry.unpack(&dest).context("unpack entry")?;
            set_executable(&dest)?;
            found = Some(dest);
            break;
        }
    }

    found.ok_or_else(|| anyhow::anyhow!("no binary found in archive {}", archive.display()))
}

fn build_local_binary_blocking(name: &str, source_dir: &Path, work_dir: &Path) -> Result<PathBuf> {
    if !source_dir.is_dir() {
        bail!(
            "local binary source is not a directory: {}",
            source_dir.display()
        );
    }
    let target = std::env::var("RUST_TARGET").ok().filter(|s| !s.is_empty());
    let target_base = work_dir.join("build-target");
    std::fs::create_dir_all(&target_base).context("create local build target dir")?;

    let mut example_args = vec!["build", "--release", "--example", name];
    if let Some(t) = target.as_deref() {
        example_args.extend(["--target", t]);
    }
    let example_status = std::process::Command::new("cargo")
        .args(&example_args)
        .env("CARGO_TARGET_DIR", &target_base)
        .current_dir(source_dir)
        .status()
        .context("spawn cargo build --example")?;
    if example_status.success() {
        return Ok(local_target_artifact_path(
            &target_base,
            name,
            true,
            target.as_deref(),
        ));
    }

    let mut bin_args = vec!["build", "--release", "--bin", name];
    if let Some(t) = target.as_deref() {
        bin_args.extend(["--target", t]);
    }
    let bin_status = std::process::Command::new("cargo")
        .args(&bin_args)
        .env("CARGO_TARGET_DIR", &target_base)
        .current_dir(source_dir)
        .status()
        .context("spawn cargo build --bin")?;
    if !bin_status.success() {
        bail!(
            "failed to build '{}' as example or bin in {}",
            name,
            source_dir.display()
        );
    }
    Ok(local_target_artifact_path(
        &target_base,
        name,
        false,
        target.as_deref(),
    ))
}

async fn build_from_git(
    repo: &str,
    commit: &str,
    example: Option<&str>,
    bin: Option<&str>,
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
    let example_owned = example.map(|s| s.to_owned());
    let bin_owned = bin.map(|s| s.to_owned());

    tokio::task::spawn_blocking(move || {
        git_clone_or_update(&repo, &commit, &src)?;

        let rust_target = std::env::var("RUST_TARGET").ok().filter(|s| !s.is_empty());
        let mut args = vec!["build", "--release"];
        if let Some(t) = rust_target.as_deref() {
            args.extend(["--target", t]);
        }
        if let Some(ex) = example_owned.as_deref() {
            args.extend(["--example", ex]);
        }
        if let Some(b) = bin_owned.as_deref() {
            args.extend(["--bin", b]);
        }
        let status = std::process::Command::new("cargo")
            .args(&args)
            .current_dir(&src)
            .status()
            .context("spawn cargo build")?;
        if !status.success() {
            bail!("cargo build failed");
        }

        // Find the produced binary.
        let meta = std::process::Command::new("cargo")
            .args(["metadata", "--format-version", "1", "--no-deps"])
            .current_dir(&src)
            .output()
            .context("cargo metadata")?;
        let json: serde_json::Value =
            serde_json::from_slice(&meta.stdout).context("parse cargo metadata")?;
        let target_dir = json["target_directory"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing target_directory"))?;

        if let Some(ex) = example_owned.as_deref() {
            Ok(artifact_path_from_target_dir(
                target_dir,
                ex,
                true,
                rust_target.as_deref(),
            ))
        } else if let Some(b) = bin_owned.as_deref() {
            Ok(artifact_path_from_target_dir(
                target_dir,
                b,
                false,
                rust_target.as_deref(),
            ))
        } else {
            bail!("binary spec must specify example or bin for git source");
        }
    })
    .await
    .context("join build task")?
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

fn set_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)
            .context("stat binary")?
            .permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(path, perms).context("chmod binary")?;
    }
    Ok(())
}

fn artifact_path_from_target_dir(
    target_dir: &str,
    binary_name: &str,
    is_example: bool,
    rust_target: Option<&str>,
) -> PathBuf {
    let mut path = PathBuf::from(target_dir);
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
