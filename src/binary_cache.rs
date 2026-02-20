//! Shared binary caching helpers for URL assets.

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use tar::Archive;

/// Resolve (and cache) a URL-backed binary under a shared work-root cache.
pub fn cached_binary_for_url(url: &str, work_dir: &Path) -> Result<PathBuf> {
    let cache_root = shared_cache_root(work_dir).join(".binary-cache");
    let key = url_cache_key(url);
    let entry_dir = cache_root.join(key);
    std::fs::create_dir_all(&entry_dir)
        .with_context(|| format!("create cache dir {}", entry_dir.display()))?;

    let marker = entry_dir.join("resolved.path");
    if marker.exists() {
        let marked = std::fs::read_to_string(&marker)
            .with_context(|| format!("read cache marker {}", marker.display()))?;
        let marked = PathBuf::from(marked.trim());
        if marked.exists() {
            return Ok(marked);
        }
    }

    let filename = url
        .rsplit('/')
        .next()
        .unwrap_or("binary")
        .split('?')
        .next()
        .unwrap_or("binary");
    let archive_path = entry_dir.join(filename);
    if !archive_path.exists() {
        tracing::info!(
            url,
            dest = %archive_path.display(),
            "downloading binary asset"
        );
        let response = reqwest::blocking::get(url).context("GET binary url")?;
        if !response.status().is_success() {
            bail!("download failed: {} {}", url, response.status());
        }
        let bytes = response.bytes().context("read binary response")?;
        std::fs::write(&archive_path, &bytes)
            .with_context(|| format!("write {}", archive_path.display()))?;
    }

    let resolved = if filename.ends_with(".tar.gz") || filename.ends_with(".tgz") {
        extract_first_binary(&archive_path, &entry_dir)?
    } else {
        set_executable(&archive_path)?;
        archive_path
    };

    let mut marker_file = std::fs::File::create(&marker)
        .with_context(|| format!("create cache marker {}", marker.display()))?;
    writeln!(marker_file, "{}", resolved.display())
        .with_context(|| format!("write cache marker {}", marker.display()))?;
    Ok(resolved)
}

/// Compute stable cache key from URL.
pub fn url_cache_key(url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for b in digest.iter().take(16) {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

fn shared_cache_root(work_dir: &Path) -> PathBuf {
    if let Some(parent) = work_dir.parent() {
        if parent
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.starts_with("sim-"))
            .unwrap_or(false)
        {
            if let Some(root) = parent.parent() {
                return root.to_path_buf();
            }
        }
    }
    work_dir.to_path_buf()
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

fn set_executable(path: &Path) -> Result<()> {
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
