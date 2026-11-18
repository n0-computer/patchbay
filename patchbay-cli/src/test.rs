//! Test command implementation.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{bail, Context, Result};
use patchbay_utils::manifest::{self, RunKind, RunManifest, TestStatus};

/// Shared test arguments used by both `patchbay test` and `patchbay compare test`.
#[derive(Debug, Clone, clap::Args)]
pub struct TestArgs {
    /// Include ignored tests (like `cargo test -- --include-ignored`).
    #[arg(long)]
    pub include_ignored: bool,

    /// Run only ignored tests (like `cargo test -- --ignored`).
    #[arg(long)]
    pub ignored: bool,

    /// Package to test.
    #[arg(short = 'p', long = "package")]
    pub packages: Vec<String>,

    /// Test target name.
    #[arg(long = "test")]
    pub tests: Vec<String>,

    /// Number of build jobs.
    #[arg(short = 'j', long)]
    pub jobs: Option<u32>,

    /// Features to enable.
    #[arg(short = 'F', long)]
    pub features: Vec<String>,

    /// Build in release mode.
    #[arg(long)]
    pub release: bool,

    /// Test only library.
    #[arg(long)]
    pub lib: bool,

    /// Don't stop on first failure.
    #[arg(long)]
    pub no_fail_fast: bool,

    /// Extra args passed after `--` to cargo/test binaries (filter, etc).
    #[arg(last = true)]
    pub extra_args: Vec<String>,
}

impl TestArgs {
    /// Apply shared cargo flags to a command (packages, tests, jobs, features, etc).
    fn apply_cargo_flags(&self, cmd: &mut Command) {
        cmd.env("RUSTFLAGS", crate::util::patchbay_rustflags());
        for p in &self.packages {
            cmd.arg("-p").arg(p);
        }
        for t in &self.tests {
            cmd.arg("--test").arg(t);
        }
        if let Some(j) = self.jobs {
            cmd.arg("-j").arg(j.to_string());
        }
        for f in &self.features {
            cmd.arg("-F").arg(f);
        }
        if self.release {
            cmd.arg("--release");
        }
        if self.lib {
            cmd.arg("--lib");
        }
        if self.no_fail_fast {
            cmd.arg("--no-fail-fast");
        }
    }

    /// Build a `cargo nextest run` command with JSON output.
    pub fn nextest_cmd(&self, dir: Option<&Path>) -> Command {
        let mut cmd = Command::new("cargo");
        cmd.arg("nextest").arg("run");
        cmd.env("NEXTEST_EXPERIMENTAL_LIBTEST_JSON", "1");
        cmd.arg("--message-format").arg("libtest-json");
        self.apply_cargo_flags(&mut cmd);
        if let Some(d) = dir {
            cmd.current_dir(d);
        }
        if self.include_ignored {
            cmd.arg("--run-ignored").arg("all");
        } else if self.ignored {
            cmd.arg("--run-ignored").arg("ignored-only");
        }
        for a in &self.extra_args {
            cmd.arg(a);
        }
        cmd
    }

    /// Build a `cargo test` command (fallback when nextest is unavailable).
    pub fn cargo_test_cmd_in(&self, dir: Option<&Path>) -> Command {
        let mut cmd = Command::new("cargo");
        cmd.arg("test");
        self.apply_cargo_flags(&mut cmd);
        if let Some(d) = dir {
            cmd.current_dir(d);
        }
        if self.include_ignored || self.ignored || !self.extra_args.is_empty() {
            cmd.arg("--");
            if self.ignored {
                cmd.arg("--ignored");
            } else if self.include_ignored {
                cmd.arg("--include-ignored");
            }
            for a in &self.extra_args {
                cmd.arg(a);
            }
        }
        cmd
    }

    /// Convert to patchbay-vm TestVmArgs.
    #[cfg(feature = "vm")]
    pub fn into_vm_args(self, target: String, recreate: bool) -> patchbay_vm::TestVmArgs {
        let mut cargo_args = Vec::new();
        if let Some(j) = self.jobs {
            cargo_args.extend(["--jobs".into(), j.to_string()]);
        }
        for f in &self.features {
            cargo_args.extend(["--features".into(), f.clone()]);
        }
        if self.release {
            cargo_args.push("--release".into());
        }
        if self.lib {
            cargo_args.push("--lib".into());
        }
        if self.no_fail_fast {
            cargo_args.push("--no-fail-fast".into());
        }
        cargo_args.extend(self.extra_args);
        patchbay_vm::TestVmArgs {
            filter: None,
            target,
            packages: self.packages,
            tests: self.tests,
            recreate,
            cargo_args,
        }
    }
}

/// Check if cargo-nextest is available.
pub fn has_nextest() -> bool {
    Command::new("cargo-nextest")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Parse nextest JSON (libtest format) lines into TestResults.
/// Re-exports from patchbay_utils for use by compare.rs.
pub use manifest::parse_nextest_json;

/// Resolve `target_directory` from cargo metadata.
fn cargo_target_dir() -> Option<PathBuf> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version=1", "--no-deps"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let meta: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    meta["target_directory"].as_str().map(PathBuf::from)
}

/// Run a test command, capturing output. Returns (exit success, stdout, stderr).
pub fn run_piped(cmd: &mut Command, verbose: bool) -> Result<(bool, String, String)> {
    use std::io::BufRead;

    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().context("failed to spawn test command")?;
    let stdout_pipe = child.stdout.take().unwrap();
    let stderr_pipe = child.stderr.take().unwrap();

    let v = verbose;
    let out_t = std::thread::spawn(move || {
        let mut buf = String::new();
        for line in std::io::BufReader::new(stdout_pipe)
            .lines()
            .map_while(Result::ok)
        {
            if v {
                println!("{line}");
            }
            buf.push_str(&line);
            buf.push('\n');
        }
        buf
    });
    let err_t = std::thread::spawn(move || {
        let mut buf = String::new();
        for line in std::io::BufReader::new(stderr_pipe)
            .lines()
            .map_while(Result::ok)
        {
            if verbose {
                eprintln!("{line}");
            }
            buf.push_str(&line);
            buf.push('\n');
        }
        buf
    });

    let status = child.wait().context("failed to wait for test command")?;
    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();
    Ok((status.success(), stdout, stderr))
}

/// Run tests natively via nextest (preferred) or cargo test (fallback).
pub fn run_native(args: TestArgs, verbose: bool, persist: bool) -> Result<()> {
    let use_nextest = has_nextest();
    if !use_nextest {
        eprintln!("patchbay: warning: cargo-nextest not found, falling back to cargo test");
        eprintln!("patchbay: install with: cargo install cargo-nextest");
    }

    let mut cmd = if use_nextest {
        args.nextest_cmd(None)
    } else {
        args.cargo_test_cmd_in(None)
    };

    if let Some(target_dir) = cargo_target_dir() {
        cmd.env("PATCHBAY_OUTDIR", target_dir.join("testdir-current"));
    }

    let started_at = time::OffsetDateTime::now_utc();
    let (success, stdout, stderr) = run_piped(&mut cmd, verbose)?;
    let ended_at = time::OffsetDateTime::now_utc();

    // Parse results: structured JSON from nextest, text fallback for cargo test.
    let results = if use_nextest {
        parse_nextest_json(&stdout)
    } else {
        let combined = format!("{stdout}\n{stderr}");
        manifest::parse_test_output(&combined)
    };

    let pass = results
        .iter()
        .filter(|r| r.status == TestStatus::Pass)
        .count() as u32;
    let fail = results
        .iter()
        .filter(|r| r.status == TestStatus::Fail)
        .count() as u32;
    let total = results.len() as u32;
    let git = manifest::git_context();

    let manifest = RunManifest {
        kind: RunKind::Test,
        project: None,
        commit: git.commit,
        branch: git.branch,
        dirty: git.dirty,
        pr: None,
        pr_url: None,
        title: None,
        started_at: Some(started_at),
        ended_at: Some(ended_at),
        runtime: (ended_at - started_at).try_into().ok(),
        outcome: Some(if success { "pass" } else { "fail" }.to_string()),
        pass: Some(pass),
        fail: Some(fail),
        total: Some(total),
        tests: results,
        os: Some(std::env::consts::OS.to_string()),
        arch: Some(std::env::consts::ARCH.to_string()),
        patchbay_version: option_env!("CARGO_PKG_VERSION").map(|v| v.to_string()),
    };

    if let Some(target_dir) = cargo_target_dir() {
        let testdir = target_dir.join("testdir-current");
        if let Err(e) = std::fs::create_dir_all(&testdir) {
            eprintln!("patchbay: warning: could not create testdir: {e}");
        }
        let run_json = testdir.join("run.json");
        match serde_json::to_string_pretty(&manifest) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&run_json, json) {
                    eprintln!("patchbay: warning: could not write run.json: {e}");
                }
            }
            Err(e) => eprintln!("patchbay: warning: could not serialize run.json: {e}"),
        }
    }

    if persist {
        persist_run()?;
    }

    if !success {
        bail!("tests failed");
    }
    Ok(())
}

/// Copy testdir-current/ into `.patchbay/work/run-{timestamp}/`.
fn persist_run() -> Result<()> {
    let target_dir = cargo_target_dir().context("could not determine cargo target dir")?;
    let testdir = target_dir.join("testdir-current");
    if !testdir.exists() {
        return Ok(());
    }
    let ts = patchbay_utils::manifest::run_dir_stamp(time::OffsetDateTime::now_utc());
    let dest = PathBuf::from(format!(".patchbay/work/run-{ts}"));
    std::fs::create_dir_all(dest.parent().unwrap())?;
    // -rL: dereference symlinks (testdir-current is a symlink to testdir-N)
    let status = Command::new("cp")
        .args(["-rL"])
        .arg(&testdir)
        .arg(&dest)
        .status()
        .context("cp testdir")?;
    if !status.success() {
        bail!("failed to copy testdir to {}", dest.display());
    }
    println!("patchbay: persisted run to {}", dest.display());
    Ok(())
}

/// Run tests in a VM via patchbay-vm.
#[cfg(feature = "vm")]
pub fn run_vm(args: TestArgs, backend: patchbay_vm::Backend) -> anyhow::Result<()> {
    use patchbay_vm::VmOps;
    let backend = backend.resolve();
    let target = patchbay_vm::default_test_target();
    backend.run_tests(args.into_vm_args(target, false))
}
