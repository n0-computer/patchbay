//! Formats JSON tracing log files as human-readable ANSI output.
//!
//! Reads JSON lines produced by `tracing_subscriber`'s JSON format layer and
//! re-renders them in the same style as the default ANSI fmt subscriber:
//!
//! ```text
//! 2026-03-03T14:30:00.123456Z  INFO setup_router{name="home"}: patchbay::core: applying NAT rules nat=Home
//! ```

use std::{
    io::{self, BufRead, BufReader, IsTerminal, Write},
    path::Path,
};

use anyhow::{Context, Result};

/// Returns true if ANSI color codes should be used.
fn use_color() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

/// ANSI color codes for log levels.
struct LevelColors;

impl LevelColors {
    fn for_level(level: &str, color: bool) -> (&'static str, &'static str) {
        if !color {
            return ("", "");
        }
        let code = match level.to_uppercase().as_str() {
            "ERROR" => "\x1b[31m", // red
            "WARN" => "\x1b[33m",  // yellow
            "INFO" => "\x1b[32m",  // green
            "DEBUG" => "\x1b[34m", // blue
            "TRACE" => "\x1b[35m", // purple
            _ => "\x1b[0m",
        };
        (code, "\x1b[0m")
    }

    fn bold(color: bool) -> (&'static str, &'static str) {
        if color {
            ("\x1b[1m", "\x1b[0m")
        } else {
            ("", "")
        }
    }

    fn dim(color: bool) -> (&'static str, &'static str) {
        if color {
            ("\x1b[2m", "\x1b[0m")
        } else {
            ("", "")
        }
    }
}

/// Format a single JSON log line to the output.
fn format_line(line: &str, out: &mut impl Write, color: bool) -> Result<()> {
    let v: serde_json::Value = serde_json::from_str(line).context("parse JSON log line")?;

    let timestamp = v["timestamp"].as_str().unwrap_or("");
    let level = v["level"].as_str().unwrap_or("INFO");
    let target = v["target"].as_str().unwrap_or("");
    let message = v["fields"]["message"].as_str().unwrap_or("");

    // Format spans: chain of span_name{field=value} separated by ":"
    let mut span_parts = Vec::new();
    if let Some(spans) = v["spans"].as_array() {
        for span in spans {
            let span_name = span["name"].as_str().unwrap_or("?");
            let mut fields = Vec::new();
            if let Some(obj) = span.as_object() {
                for (k, v) in obj {
                    if k == "name" {
                        continue;
                    }
                    fields.push(format!("{}={}", k, format_value(v)));
                }
            }
            if fields.is_empty() {
                span_parts.push(span_name.to_string());
            } else {
                span_parts.push(format!("{}{{{}}}", span_name, fields.join(",")));
            }
        }
    }

    // Format extra fields (fields other than "message")
    let mut extra_fields = Vec::new();
    if let Some(obj) = v["fields"].as_object() {
        for (k, v) in obj {
            if k == "message" {
                continue;
            }
            extra_fields.push(format!("{}={}", k, format_value(v)));
        }
    }

    let (level_start, level_end) = LevelColors::for_level(level, color);
    let (bold_start, bold_end) = LevelColors::bold(color);
    let (dim_start, dim_end) = LevelColors::dim(color);

    // timestamp
    write!(out, "{dim_start}{timestamp}{dim_end}")?;

    // level (right-padded to 5 chars)
    write!(out, " {level_start}{level:>5}{level_end}")?;

    // spans
    if !span_parts.is_empty() {
        write!(out, " {bold_start}{}{bold_end}", span_parts.join(":"))?;
    }

    // target
    if !target.is_empty() {
        write!(out, "{dim_start}: {target}{dim_end}")?;
    } else if !span_parts.is_empty() {
        write!(out, ":")?;
    }

    // message
    if !message.is_empty() {
        write!(out, " {message}")?;
    }

    // extra fields
    if !extra_fields.is_empty() {
        let fields_str = extra_fields.join(" ");
        write!(out, " {dim_start}{fields_str}{dim_end}")?;
    }

    writeln!(out)?;
    Ok(())
}

fn format_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

/// Formats JSON log lines to stdout.
///
/// If `path` is `Some`, reads from the file. If `None`, reads from stdin.
/// `follow` only applies to file mode — it is ignored when reading from stdin.
pub fn run(path: Option<&Path>, follow: bool) -> Result<()> {
    let color = use_color();
    let mut stdout = io::stdout().lock();

    let reader: Box<dyn BufRead> = if let Some(path) = path {
        let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
        Box::new(BufReader::new(file))
    } else {
        Box::new(BufReader::new(io::stdin()))
    };

    for line in reader.lines() {
        let line = line.context("read line")?;
        if line.trim().is_empty() {
            continue;
        }
        if let Err(e) = format_line(&line, &mut stdout, color) {
            // If it's not valid JSON, print the raw line.
            eprintln!("fmt-log: {e}");
            writeln!(stdout, "{line}")?;
        }
    }

    // Follow only makes sense for files.
    if let Some(path) = path.filter(|_| follow) {
        let mut last_pos = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let current_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            if current_len > last_pos {
                let file = std::fs::File::open(path)?;
                let mut reader = BufReader::new(file);
                io::Seek::seek(&mut reader, io::SeekFrom::Start(last_pos))?;
                for line in reader.lines() {
                    let line = line.context("read line")?;
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Err(e) = format_line(&line, &mut stdout, color) {
                        eprintln!("fmt-log: {e}");
                        writeln!(stdout, "{line}")?;
                    }
                }
                last_pos = current_len;
            }
        }
    }

    Ok(())
}
