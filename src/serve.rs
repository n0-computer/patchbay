//! Embedded UI server for netsim run artifacts.

use anyhow::{Context, Result};
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

const UI_INDEX: &str = include_str!("../ui/dist/index.html");

/// Running embedded UI server handle.
pub struct UiServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl UiServer {
    /// Base HTTP URL.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Open the base URL in a browser.
    pub fn open_browser(&self) -> Result<()> {
        open_browser(&self.url())
    }
}

impl Drop for UiServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Start serving embedded UI + work-root files.
pub fn start_ui_server(work_root: PathBuf, bind: &str) -> Result<UiServer> {
    fs::create_dir_all(&work_root)
        .with_context(|| format!("create work root {}", work_root.display()))?;
    let listener = TcpListener::bind(bind).with_context(|| format!("bind UI server on {bind}"))?;
    listener
        .set_nonblocking(true)
        .context("set UI listener nonblocking")?;
    let addr = listener.local_addr().context("get UI listener address")?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let join = thread::spawn(move || {
        while !stop_thread.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _peer)) => {
                    let _ = handle_client(stream, &work_root);
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(_) => {
                    thread::sleep(Duration::from_millis(50));
                }
            }
        }
    });
    Ok(UiServer {
        addr,
        stop,
        join: Some(join),
    })
}

/// Open URL in default browser.
pub fn open_browser(url: &str) -> Result<()> {
    webbrowser::open(url).context("open browser")?;
    Ok(())
}

fn handle_client(mut stream: TcpStream, work_root: &Path) -> Result<()> {
    let mut buf = [0u8; 16 * 1024];
    let read = stream.read(&mut buf).context("read HTTP request")?;
    if read == 0 {
        return Ok(());
    }
    let req = String::from_utf8_lossy(&buf[..read]);
    let mut lines = req.lines();
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("");
    let raw_path = parts.next().unwrap_or("/");
    if method != "GET" && method != "HEAD" {
        write_response(
            &mut stream,
            405,
            "text/plain; charset=utf-8",
            b"method not allowed",
            method == "HEAD",
        )?;
        return Ok(());
    }

    let path = raw_path.split('?').next().unwrap_or("/");
    if path == "/" || path == "/index.html" {
        write_response(
            &mut stream,
            200,
            "text/html; charset=utf-8",
            UI_INDEX.as_bytes(),
            method == "HEAD",
        )?;
        return Ok(());
    }
    if path == "/__netsim/runs" {
        let body = runs_json(work_root).context("build runs endpoint body")?;
        write_response(
            &mut stream,
            200,
            "application/json; charset=utf-8",
            body.as_bytes(),
            method == "HEAD",
        )?;
        return Ok(());
    }

    if path.contains("..") {
        write_response(
            &mut stream,
            403,
            "text/plain; charset=utf-8",
            b"forbidden",
            method == "HEAD",
        )?;
        return Ok(());
    }
    let rel = path.trim_start_matches('/');
    let full = work_root.join(rel);
    if !full.exists() || !full.is_file() {
        write_response(
            &mut stream,
            404,
            "text/plain; charset=utf-8",
            b"not found",
            method == "HEAD",
        )?;
        return Ok(());
    }
    let bytes = fs::read(&full).with_context(|| format!("read {}", full.display()))?;
    write_response(
        &mut stream,
        200,
        guess_mime(&full),
        &bytes,
        method == "HEAD",
    )?;
    Ok(())
}

fn runs_json(work_root: &Path) -> Result<String> {
    let mut runs = Vec::new();
    for entry in fs::read_dir(work_root).with_context(|| format!("read {}", work_root.display()))? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !path.is_dir() || name.starts_with('.') || name == "latest" {
            continue;
        }
        runs.push(name.to_string());
    }
    runs.sort();
    runs.reverse();
    Ok(serde_json::json!({
        "workRoot": work_root.display().to_string(),
        "runs": runs,
    })
    .to_string())
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    head_only: bool,
) -> Result<()> {
    let status_text = match status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let headers = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
        status,
        status_text,
        content_type,
        body.len()
    );
    stream
        .write_all(headers.as_bytes())
        .context("write response headers")?;
    if !head_only {
        stream.write_all(body).context("write response body")?;
    }
    Ok(())
}

fn guess_mime(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
    {
        "html" => "text/html; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "md" | "log" | "txt" => "text/plain; charset=utf-8",
        "qlog" => "application/json; charset=utf-8",
        _ => "application/octet-stream",
    }
}
