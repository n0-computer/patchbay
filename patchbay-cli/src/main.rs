//! Unified CLI entrypoint for patchbay simulations (native and VM).

mod compare;
#[cfg(target_os = "linux")]
mod native;
mod test;
#[cfg(feature = "upload")]
mod upload;
mod util;
#[cfg(feature = "vm")]
mod vm;

#[cfg(feature = "serve")]
use std::process::Command as ProcessCommand;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
#[cfg(feature = "serve")]
use patchbay_server::DEFAULT_UI_BIND;
use serde::Deserialize;
#[cfg(not(feature = "serve"))]
const DEFAULT_UI_BIND: &str = "127.0.0.1:7421";

#[derive(Parser)]
#[command(name = "patchbay", about = "Run a patchbay simulation")]
struct Cli {
    /// Verbose output (stream subcommand output live).
    #[arg(short = 'v', long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run one or more sims (native on Linux, VM elsewhere).
    Run {
        #[command(flatten)]
        args: RunArgs,
    },
    /// Resolve sims and build all required assets without running.
    Prepare {
        /// One or more sim TOML files or directories containing `*.toml`.
        #[arg()]
        sims: Vec<PathBuf>,
        /// Work directory for caches and prepared outputs.
        #[arg(long, default_value = ".patchbay/work")]
        work_dir: PathBuf,
        /// Binary override in `<name>:<mode>:<value>` form.
        #[arg(long = "binary")]
        binary_overrides: Vec<String>,
        /// Do not build binaries; resolve expected artifacts from target dirs.
        #[arg(long, default_value_t = false)]
        no_build: bool,

        /// Project root directory for resolving binaries and cargo builds.
        /// Defaults to the current working directory.
        #[arg(long)]
        project_root: Option<PathBuf>,
    },
    /// Serve embedded devtools UI over HTTP for a lab output directory.
    #[cfg(feature = "serve")]
    Serve {
        /// Output directory containing lab run subdirectories.
        ///
        /// Ignored when `--testdir` is set.
        #[arg(default_value = ".patchbay/work")]
        outdir: PathBuf,
        /// Serve `<cargo-target-dir>/testdir-current` instead of a path.
        ///
        /// Uses `cargo metadata` to locate the target directory.
        #[arg(long, default_value_t = false)]
        testdir: bool,
        /// Bind address for HTTP server.
        #[arg(long, default_value = DEFAULT_UI_BIND)]
        bind: String,
        /// Open browser after server start.
        #[arg(long, default_value_t = false)]
        open: bool,
    },
    /// Build topology from sim/topology config for interactive namespace debugging.
    #[cfg(target_os = "linux")]
    Inspect {
        /// Sim TOML or topology TOML file path.
        input: PathBuf,
        /// Work directory for inspect session metadata.
        #[arg(long, default_value = ".patchbay/work")]
        work_dir: PathBuf,
    },
    /// Run a command inside a node namespace from an inspect session.
    #[cfg(target_os = "linux")]
    RunIn {
        /// Device or router name from the inspected topology.
        node: String,
        /// Inspect session id (defaults to `$NETSIM_INSPECT`).
        #[arg(long)]
        inspect: Option<String>,
        /// Work directory containing inspect session metadata.
        #[arg(long, default_value = ".patchbay/work")]
        work_dir: PathBuf,
        /// Command and args to execute in the node namespace.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
    /// Run tests (native on Linux, VM elsewhere; --vm forces VM backend).
    Test {
        #[command(flatten)]
        args: test::TestArgs,

        /// Persist run output to `.patchbay/work/run-{timestamp}/`.
        #[arg(long)]
        persist: bool,

        /// Force VM backend.
        #[arg(long, num_args = 0..=1, default_missing_value = "auto")]
        vm: Option<String>,
    },
    /// Compare test or sim results across git refs.
    Compare {
        #[command(subcommand)]
        command: CompareCommand,
    },
    /// Upload a run/compare directory to a patchbay-server instance.
    Upload {
        /// Directory to upload (e.g. .patchbay/work/compare-20260325_120000).
        dir: PathBuf,
        /// Project name for scoping on the server.
        #[arg(long, env = "PATCHBAY_PROJECT")]
        project: String,
        /// Server URL (e.g. https://patchbay.example.com).
        #[arg(long, env = "PATCHBAY_URL")]
        url: String,
        /// API key for authentication.
        #[arg(long, env = "PATCHBAY_API_KEY")]
        api_key: String,
    },
    /// VM management and simulation execution.
    #[cfg(feature = "vm")]
    Vm {
        #[command(subcommand)]
        command: vm::VmCommand,
        /// Which VM backend to use.
        #[arg(long, default_value = "auto", global = true)]
        backend: patchbay_vm::Backend,
    },
}

#[derive(Subcommand)]
enum CompareCommand {
    /// Compare test results between git refs.
    ///
    /// Usage: patchbay compare test <ref> [ref2] [-- test-filter-and-args]
    Test {
        /// Git ref to compare (left side).
        left_ref: String,

        /// Second git ref (right side). If omitted, compares against current worktree.
        right_ref: Option<String>,

        /// Force rebuild even if a cached run exists for the commit.
        #[arg(long)]
        force_build: bool,

        /// Fail instead of building if no cached run exists for a ref.
        #[arg(long)]
        no_ref_build: bool,

        #[command(flatten)]
        args: test::TestArgs,
    },
    /// Compare sim results between git refs.
    Run {
        /// Git ref to compare (left side).
        left_ref: String,

        /// Second git ref (right side).
        right_ref: Option<String>,

        /// Sim TOML files or directories.
        #[arg(long = "sim", required = true)]
        sims: Vec<PathBuf>,
    },
}

fn resolve_project_root(opt: Option<PathBuf>) -> Result<PathBuf> {
    match opt {
        Some(p) => Ok(p),
        None => std::env::current_dir().context("resolve current directory"),
    }
}

fn main() -> Result<()> {
    #[cfg(target_os = "linux")]
    native::init()?;
    tokio_main()
}

#[tokio::main(flavor = "current_thread")]
async fn tokio_main() -> Result<()> {
    patchbay_utils::init_tracing();
    let cli = Cli::parse();
    match cli.command {
        Command::Run { args } => dispatch_run(args).await,
        Command::Prepare {
            sims,
            work_dir,
            binary_overrides,
            no_build,
            project_root,
        } => dispatch_prepare(sims, work_dir, binary_overrides, no_build, project_root).await,
        #[cfg(feature = "serve")]
        Command::Serve {
            outdir,
            testdir,
            bind,
            open,
        } => {
            let dir = if testdir {
                resolve_testdir_native()?
            } else {
                outdir
            };
            println!("patchbay: serving {} at http://{bind}/", dir.display());
            if open {
                let url = format!("http://{bind}/");
                let _ = ProcessCommand::new("xdg-open").arg(&url).spawn();
            }
            patchbay_server::serve(dir, &bind).await
        }
        #[cfg(target_os = "linux")]
        Command::Inspect { input, work_dir } => native::inspect_command(input, work_dir).await,
        #[cfg(target_os = "linux")]
        Command::RunIn {
            node,
            inspect,
            work_dir,
            cmd,
        } => native::run_in_command(node, inspect, work_dir, cmd),
        Command::Test { args, persist, vm } => dispatch_test(args, persist, vm, cli.verbose),
        Command::Compare { command } => dispatch_compare(command, cli.verbose),
        Command::Upload {
            dir,
            project,
            url,
            api_key,
        } => {
            if !dir.exists() {
                bail!("directory does not exist: {}", dir.display());
            }
            #[cfg(feature = "upload")]
            {
                upload::upload(&dir, &project, &url, &api_key)
            }
            #[cfg(not(feature = "upload"))]
            {
                let _ = (&dir, &project, &url, &api_key);
                bail!("upload support not compiled in (enable the `upload` feature)")
            }
        }
        #[cfg(feature = "vm")]
        Command::Vm { command, backend } => vm::dispatch_vm(command, backend).await,
    }
}

// ── Run dispatch ────────────────────────────────────────────────────────

#[derive(clap::Args)]
struct RunArgs {
    /// One or more sim TOML files or directories containing `*.toml`.
    #[arg()]
    sims: Vec<PathBuf>,
    /// Work directory for logs, binaries, and results.
    #[arg(long, default_value = ".patchbay/work")]
    work_dir: PathBuf,
    /// Binary override in `<name>:<mode>:<value>` form.
    #[arg(long = "binary")]
    binary_overrides: Vec<String>,
    /// Do not build binaries; resolve expected artifacts from target dirs.
    #[arg(long, default_value_t = false)]
    no_build: bool,
    /// Stream live stdout/stderr lines with node prefixes.
    #[arg(short = 'v', long, default_value_t = false)]
    verbose: bool,
    /// Start embedded UI server and open browser.
    #[arg(long, default_value_t = false)]
    open: bool,
    /// Bind address for embedded UI server.
    #[arg(long, default_value = DEFAULT_UI_BIND)]
    bind: String,
    /// Project root for resolving binaries. Defaults to cwd.
    #[arg(long)]
    project_root: Option<PathBuf>,
    /// Per-sim timeout (e.g. "120s", "5m").
    #[arg(long)]
    timeout: Option<String>,
}

#[allow(clippy::needless_return)]
async fn dispatch_run(r: RunArgs) -> Result<()> {
    // On Linux: run natively.
    #[cfg(target_os = "linux")]
    {
        let sim_timeout = r
            .timeout
            .map(|s| native::parse_duration(&s))
            .transpose()
            .context("invalid --timeout value")?;
        if r.open {
            #[cfg(feature = "serve")]
            {
                let bind_addr = r.bind.clone();
                let work = r.work_dir.clone();
                tokio::spawn(async move {
                    if let Err(e) = patchbay_server::serve(work, &bind_addr).await {
                        tracing::error!("server error: {e}");
                    }
                });
                println!("patchbay: http://{}/", r.bind);
                let url = format!("http://{}/", r.bind);
                let _ = ProcessCommand::new("xdg-open").arg(&url).spawn();
            }
            #[cfg(not(feature = "serve"))]
            bail!("--open requires the `serve` feature");
        }
        let project_root = resolve_project_root(r.project_root)?;
        let sims = resolve_sim_args(r.sims, &project_root)?;
        let res = native::run_sims(
            sims,
            r.work_dir,
            r.binary_overrides,
            r.verbose,
            Some(project_root),
            r.no_build,
            sim_timeout,
        )
        .await;
        if r.open && res.is_ok() {
            println!("run finished; server still running (Ctrl-C to exit)");
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        }
        return res;
    }

    // On non-Linux with VM feature: delegate to VM backend.
    #[cfg(all(not(target_os = "linux"), feature = "vm"))]
    {
        let vm_args = vm::VmRunArgs {
            sims: r.sims,
            work_dir: r.work_dir,
            binary_overrides: r.binary_overrides,
            verbose: r.verbose,
            open: r.open,
            bind: r.bind,
        };
        return vm::run_sims_vm(vm_args, patchbay_vm::Backend::Auto);
    }

    #[cfg(all(not(target_os = "linux"), not(feature = "vm")))]
    {
        let _ = r;
        bail!("run requires Linux or the `vm` feature");
    }
}

// ── Prepare dispatch ────────────────────────────────────────────────────

#[allow(clippy::needless_return)]
async fn dispatch_prepare(
    sims: Vec<PathBuf>,
    work_dir: PathBuf,
    binary_overrides: Vec<String>,
    no_build: bool,
    project_root: Option<PathBuf>,
) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let project_root = resolve_project_root(project_root)?;
        let sims = resolve_sim_args(sims, &project_root)?;
        return native::prepare_sims(
            sims,
            work_dir,
            binary_overrides,
            Some(project_root),
            no_build,
        )
        .await;
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (
            &sims,
            &work_dir,
            &binary_overrides,
            &no_build,
            &project_root,
        );
        bail!("prepare requires Linux (use `patchbay vm run` for non-Linux)");
    }
}

// ── Test dispatch ───────────────────────────────────────────────────────

#[allow(clippy::needless_return)]
fn dispatch_test(
    args: test::TestArgs,
    persist: bool,
    vm: Option<String>,
    verbose: bool,
) -> Result<()> {
    // Explicit --vm: force VM backend.
    if let Some(ref vm_backend) = vm {
        #[cfg(feature = "vm")]
        {
            let backend = match vm_backend.as_str() {
                "auto" => patchbay_vm::Backend::Auto.resolve(),
                "qemu" => patchbay_vm::Backend::Qemu,
                "container" => patchbay_vm::Backend::Container,
                other => bail!("unknown VM backend: {other}"),
            };
            return test::run_vm(args, backend);
        }
        #[cfg(not(feature = "vm"))]
        {
            let _ = vm_backend;
            bail!("VM support not compiled (enable the `vm` feature)");
        }
    }

    // No --vm flag: auto-detect based on platform.
    #[cfg(target_os = "linux")]
    {
        return test::run_native(args, verbose, persist);
    }

    #[cfg(all(not(target_os = "linux"), feature = "vm"))]
    {
        let _ = (verbose, persist);
        let backend = patchbay_vm::Backend::Auto.resolve();
        return test::run_vm(args, backend);
    }

    #[cfg(all(not(target_os = "linux"), not(feature = "vm")))]
    {
        let _ = (args, verbose, persist);
        bail!("test requires Linux or the `vm` feature");
    }
}

// ── Compare dispatch ────────────────────────────────────────────────────

fn dispatch_compare(command: CompareCommand, verbose: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("get cwd")?;
    let work_dir = cwd.join(".patchbay/work");
    match command {
        CompareCommand::Test {
            left_ref,
            right_ref,
            force_build,
            no_ref_build,
            args,
        } => {
            use patchbay_utils::manifest::{self as mf, RunKind};

            let right_label = right_ref.as_deref().unwrap_or("worktree");
            println!(
                "patchbay compare test: {} \u{2194} {}",
                left_ref, right_label
            );

            let resolve_ref_results = |git_ref: &str, label: &str| -> Result<Vec<mf::TestResult>> {
                let sha = mf::resolve_ref(git_ref)
                    .with_context(|| format!("could not resolve ref '{git_ref}'"))?;

                if !force_build {
                    if let Some((_dir, manifest)) =
                        mf::find_run_for_commit(&work_dir, &sha, RunKind::Test)
                    {
                        println!("Using cached run for {label} ({sha:.8})");
                        return Ok(manifest.tests);
                    }
                }

                if no_ref_build {
                    bail!(
                        "no cached run for {label} ({sha:.8}); \
                             run `patchbay test --persist` on that ref first, \
                             or remove --no-ref-build"
                    );
                }

                println!("Running tests in {label} ...");
                let tree_dir = compare::setup_worktree(git_ref, &cwd)?;
                let started_at = time::OffsetDateTime::now_utc();
                let (results, _output) = compare::run_tests_in_dir(&tree_dir, &args, verbose)?;
                let ended_at = time::OffsetDateTime::now_utc();
                let runtime = (ended_at - started_at).try_into().unwrap_or_default();

                compare::persist_worktree_run(&tree_dir, &results, started_at, ended_at, runtime)?;
                compare::cleanup_worktree(&tree_dir)?;
                Ok(results)
            };

            let left_results = resolve_ref_results(&left_ref, &left_ref)?;

            let right_results = if let Some(ref r) = right_ref {
                resolve_ref_results(r, r)?
            } else {
                println!("Running tests in worktree ...");
                let (results, _output) = compare::run_tests_in_dir(&cwd, &args, verbose)?;
                results
            };

            let result = compare::compare_results(&left_results, &right_results);
            compare::print_summary(
                &left_ref,
                right_label,
                &left_results,
                &right_results,
                &result,
            );

            if result.regressions > 0 {
                bail!("{} regressions detected", result.regressions);
            }
            Ok(())
        }
        CompareCommand::Run {
            sims: _,
            left_ref: _,
            right_ref: _,
        } => {
            bail!("compare run is not yet implemented");
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// When no sim paths are given on the CLI, look for `patchbay.toml` or
/// `.patchbay.toml` in the project root and use its `simulations` path.
fn resolve_sim_args(sims: Vec<PathBuf>, project_root: &Path) -> Result<Vec<PathBuf>> {
    if !sims.is_empty() {
        return Ok(sims);
    }
    let candidates = [
        project_root.join("patchbay.toml"),
        project_root.join(".patchbay.toml"),
    ];
    for path in &candidates {
        if path.is_file() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read {}", path.display()))?;
            let cfg: PatchbayConfig =
                toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
            let sims_dir = project_root.join(&cfg.simulations);
            if !sims_dir.exists() {
                bail!(
                    "{}: simulations path '{}' does not exist",
                    path.display(),
                    sims_dir.display()
                );
            }
            println!("patchbay: using simulations from {}", sims_dir.display());
            return Ok(vec![sims_dir]);
        }
    }
    bail!(
        "no sim files specified and no patchbay.toml found in {}",
        project_root.display()
    )
}

#[derive(Deserialize)]
struct PatchbayConfig {
    /// Path to sims directory (relative to project root).
    simulations: String,
}

/// Resolve `testdir-current` inside the cargo target directory.
#[cfg(feature = "serve")]
fn resolve_testdir_native() -> Result<PathBuf> {
    let output = ProcessCommand::new("cargo")
        .args(["metadata", "--format-version=1", "--no-deps"])
        .output()
        .context("failed to run `cargo metadata`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("cargo metadata failed: {stderr}");
    }
    let meta: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parse cargo metadata")?;
    let target_dir = meta["target_directory"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("cargo metadata missing target_directory"))?;
    Ok(PathBuf::from(target_dir).join("testdir-current"))
}
