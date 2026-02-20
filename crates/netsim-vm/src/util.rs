use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use netsim::assets::{
    parse_binary_overrides, resolve_binary_source_path, BinaryOverride, PathResolveMode,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use tar::Archive;

pub fn stage_binary_overrides(
    raw: &[String],
    work_dir: &Path,
    target_dir: &Path,
    target: &str,
) -> Result<Vec<String>> {
    let parsed = parse_binary_overrides(raw)?;
    let bins_dir = work_dir.join("binaries");
    std::fs::create_dir_all(&bins_dir).with_context(|| format!("create {}", bins_dir.display()))?;

    let mut rewritten = Vec::new();
    for (name, ov) in parsed {
        let staged = match ov {
            BinaryOverride::Path(src) => stage_path_binary(&name, &src, &bins_dir)?,
            BinaryOverride::Fetch(url) => stage_fetch_binary(&url, &bins_dir)?,
            BinaryOverride::Build(src) => {
                stage_build_binary(&name, &src, &bins_dir, target_dir, target)?
            }
        };
        let guest = format!(
            "/work/binaries/{}",
            staged.file_name().and_then(|s| s.to_str()).unwrap_or("bin")
        );
        rewritten.push(format!("{name}:path:{guest}"));
    }
    Ok(rewritten)
}

fn stage_path_binary(name: &str, src: &Path, bins_dir: &Path) -> Result<PathBuf> {
    let resolved = resolve_binary_source_path(src, PathResolveMode::Vm)?;
    if !resolved.exists() || resolved.is_dir() {
        bail!(
            "binary override path for '{}' is invalid: {}",
            name,
            resolved.display()
        );
    }
    let dest = bins_dir.join(format!("{}-override", name));
    std::fs::copy(&resolved, &dest)
        .with_context(|| format!("copy {} -> {}", resolved.display(), dest.display()))?;
    set_executable(&dest)?;
    Ok(dest)
}

fn stage_fetch_binary(url: &str, bins_dir: &Path) -> Result<PathBuf> {
    let filename = url
        .rsplit('/')
        .next()
        .unwrap_or("binary")
        .split('?')
        .next()
        .unwrap_or("binary");
    let archive = bins_dir.join(filename);
    if !archive.exists() {
        let status = Command::new("curl")
            .args(["-fL", url, "-o"])
            .arg(&archive)
            .status()
            .with_context(|| format!("download {}", url))?;
        if !status.success() {
            bail!("download failed: {}", url);
        }
    }

    if filename.ends_with(".tar.gz") || filename.ends_with(".tgz") {
        extract_first_binary(&archive, bins_dir)
    } else {
        set_executable(&archive)?;
        Ok(archive)
    }
}

fn stage_build_binary(
    name: &str,
    src: &Path,
    bins_dir: &Path,
    target_dir: &Path,
    target: &str,
) -> Result<PathBuf> {
    if !src.is_dir() {
        bail!(
            "build source for '{}' is not a directory: {}",
            name,
            src.display()
        );
    }

    let example = Command::new("cargo")
        .args(["build", "--release", "--target", target, "--example", name])
        .env("CARGO_TARGET_DIR", target_dir)
        .current_dir(src)
        .status()
        .context("spawn cargo build --example")?;

    let built = if example.success() {
        target_dir.join(target).join("release").join(name)
    } else {
        let bin = Command::new("cargo")
            .args(["build", "--release", "--target", target, "--bin", name])
            .env("CARGO_TARGET_DIR", target_dir)
            .current_dir(src)
            .status()
            .context("spawn cargo build --bin")?;
        if !bin.success() {
            bail!(
                "failed to build '{}' as example or bin in {}",
                name,
                src.display()
            );
        }
        target_dir.join(target).join("release").join(name)
    };

    if !built.exists() {
        bail!("expected built binary not found: {}", built.display());
    }
    let dest = bins_dir.join(format!("{}-build", name));
    std::fs::copy(&built, &dest)
        .with_context(|| format!("copy {} -> {}", built.display(), dest.display()))?;
    set_executable(&dest)?;
    Ok(dest)
}

fn extract_first_binary(archive: &Path, extract_dir: &Path) -> Result<PathBuf> {
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
            set_executable(&dest)?;
            return Ok(dest);
        }
    }
    bail!("no executable binary found in {}", archive.display())
}

pub fn set_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)
            .with_context(|| format!("stat {}", path.display()))?
            .permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("chmod {}", path.display()))?;
    }
    Ok(())
}
