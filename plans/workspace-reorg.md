# Workspace Reorganisation Plan

## TODO

- [x] Write plan
- [x] Step 1: Scaffold crate directories (`netsim-core/`, `netsim-utils/`, `netsim/`, `netsim-vm/`)
- [x] Step 2: `cargo check -p netsim-core` clean; `RouterCfg`/`LabCore` renamed
- [x] Step 3: `cargo check -p netsim-utils` clean; `build_ui` fn present
- [x] Step 4: `cargo check -p netsim` clean; `RunOpts::from_env` + typed overrides
- [x] Step 5: `cargo check -p netsim-vm` clean; no netsim-core dep
- [x] Step 6: Root `Cargo.toml` replaced; old `src/` deleted
- [x] Step 7: `cargo build --workspace && cargo test --workspace` green
- [x] Step 8: `ctor`/`init_userns` refactor — `init_userns_for_ctor` in `netsim-core`, called from `netsim/src/init.rs`
- [ ] Step 9: Report/`SimOutcome` refactor — `TransferResult` still present in `report.rs`; separate PR
- [ ] Final review

## Goal

Split the monolithic root crate into four focused crates with clean dependency boundaries,
clean up the public APIs, and (as a separate final step) refactor the reporting layer.

---

## Target Layout

```
repo/
├── Cargo.toml          workspace manifest only (no [package])
├── ui/                 Vite/TS frontend source (unchanged by reorg; see Step 9)
├── netsim-core/        pure network management lib (no workspace deps)
├── netsim-utils/       shared sim utilities lib (no workspace deps)
├── netsim/             sim CLI + runner (lib + bin; deps: core + utils)
└── netsim-vm/          VM orchestrator bin (dep: utils only)
```

## Dependency Graph

```
netsim-core  ──► rtnetlink, nix (full), ipnet, tokio, libc, futures, …
netsim-utils ──► anyhow, serde, reqwest (blocking+rustls), sha2, flate2, tar,
                 tracing, webbrowser, glob  + whatever serve.rs HTTP stack uses
netsim       ──► netsim-core, netsim-utils
             ──► clap, serde_json, regex, toml, rcgen, comfy-table, ctrlc,
                 chrono, tracing-subscriber, ctor
netsim-vm    ──► netsim-utils   ← the only workspace dep; no kernel deps
             ──► clap, serde, serde_json, toml, nix (signal/process), dirs,
                 flate2, tar
```

Key win: `netsim-vm` no longer drags in rtnetlink / nix-full / ipnet.

---

## File Movement Map

```
src/core.rs           → netsim-core/src/core.rs
src/netlink.rs        → netsim-core/src/netlink.rs
src/netns.rs          → netsim-core/src/netns.rs
src/qdisc.rs          → netsim-core/src/qdisc.rs
src/userns.rs         → netsim-core/src/userns.rs
src/util.rs           → netsim-core/src/util.rs   (used by core for naming)
src/lib.rs            → netsim-core/src/lib.rs     (trimmed, renamed items — see below)

src/assets.rs         → netsim-utils/src/assets.rs
src/binary_cache.rs   → netsim-utils/src/binary_cache.rs
src/serve.rs          → netsim-utils/src/serve.rs
build.rs              → netsim-utils/build.rs      (adjust ui_dir path)

src/sim/              → netsim/src/sim/            (whole directory, cp -r)
src/main.rs           → netsim/src/main.rs
NEW                   → netsim/src/lib.rs           (public sim runner API)

crates/netsim-vm/     → netsim-vm/                 (mv whole dir)
```

---

## Naming Changes (applied during the move)

| Old name | New name | Location | Reason |
|----------|----------|----------|--------|
| `RouterCfg` | `RouterConfig` | netsim-core `config` | consistency with other `*Config` |
| `LabCore` | `NetworkCore` | netsim-core `core` | descriptive; avoids "LabCore" leaking as a public name. `NetworkManager` clashes with the Linux system daemon name. |
| `bootstrap_userns` | `init_userns` | netsim-core `lib` | imperative verb; idiomatic for an init fn |

---

## `netsim-core` Public API

```rust
// ── Top-level lib re-exports ────────────────────────────────────────────────

pub use crate::core::NodeId;

pub enum NatMode { … }
pub enum Impair { … }
pub struct ObservedAddr { pub addr: SocketAddr, pub port: u16 }

pub struct Lab { … }          // the primary consumer entry point
pub struct DeviceBuilder<'lab> { … }

// Idiomatic init, OnceLock-guarded (see Step 8):
pub fn init_userns() -> Result<()>;
// Raw libc version for pre-TLS ctor (unsafe; see Step 8):
pub unsafe fn init_userns_for_ctor();

pub fn check_caps() -> Result<()>;

// ── pub mod config ───────────────────────────────────────────────────────────

pub mod config {
    /// Deserializable topology description (used with Lab::from_config).
    pub struct LabConfig {
        pub router: Vec<RouterConfig>,
        pub device: HashMap<String, toml::Value>,
        pub region: Option<HashMap<String, RegionConfig>>,
    }
    pub struct RouterConfig { pub name, pub region, pub upstream, pub nat }  // renamed from RouterCfg
    pub struct RegionConfig { … }
}

// ── pub mod core ─────────────────────────────────────────────────────────────
// Exposed for power users and integration tests; not the primary API.

pub mod core {
    pub struct NodeId(u64);
    pub struct NetworkCore { … }   // renamed from LabCore
    pub struct ResourceList { … }
    pub struct Device { … }
    pub struct Router { … }
    pub struct Switch { … }
    pub enum DownstreamPool { … }
    pub fn resources() -> &'static ResourceList;
    // … (rest stays as-is)
}

// ── pub mod test_utils ───────────────────────────────────────────────────────
// Probe / reflector helpers; useful in downstream integration tests.

pub mod test_utils {
    /// Spawn a UDP reflector in a named namespace.
    pub fn spawn_reflector(ns_name: &str, bind: SocketAddr) -> Result<TaskHandle>;
    /// Spawn a UDP reflector in the lab root (IX bridge) namespace.
    pub fn spawn_reflector_on_ix(lab: &Lab, bind: SocketAddr) -> Result<TaskHandle>;
    /// Observe the external UDP address seen by a reflector.
    pub fn probe_in_ns(
        ns: &str, reflector: SocketAddr, timeout: Duration, port: Option<u16>,
    ) -> Result<ObservedAddr>;
    /// Convenience: one round-trip probe, returns ObservedAddr.
    pub fn udp_roundtrip_in_ns(ns: &str, reflector: SocketAddr) -> Result<ObservedAddr>;
    /// Convenience: measure one-way UDP RTT.
    pub fn udp_rtt_in_ns(ns: &str, reflector: SocketAddr) -> Result<Duration>;
}
```

`netsim-core` carries **no** `ctor` dep in `[dependencies]`.
`ctor = "0.2"` goes in `[dev-dependencies]` only (used for the test-module ctor; see Step 8).

---

## `netsim-utils` Public API

All functionality is grouped under named modules; nothing is re-exported at crate root.

```rust
// ── pub mod assets ───────────────────────────────────────────────────────────

pub mod assets {
    pub struct BinarySpec { pub name, pub mode, pub path, pub url, pub repo,
                            pub commit, pub example, pub bin, pub features,
                            pub all_features }
    pub enum BinaryOverride { Build(PathBuf), Fetch(String), Path(PathBuf) }
    pub enum PathResolveMode { Local, Vm }

    pub fn parse_binary_overrides(raw: &[String]) -> Result<HashMap<String, BinaryOverride>>;
    pub fn infer_binary_mode(spec: &BinarySpec) -> Result<&str>;
    pub fn resolve_binary_source_path(path: &Path, mode: PathResolveMode) -> Result<PathBuf>;
    pub fn resolve_target_dir() -> Result<PathBuf>;
    pub fn resolve_target_artifact(kind: &str, name: &str, mode: PathResolveMode) -> Result<PathBuf>;
}

// ── pub mod binary_cache ─────────────────────────────────────────────────────

pub mod binary_cache {
    pub fn cached_binary_for_url(url: &str, cache_dir: &Path) -> Result<PathBuf>;
    pub fn url_cache_key(url: &str) -> String;
    pub fn set_executable(path: &Path) -> Result<()>;
}

// ── pub mod ui ───────────────────────────────────────────────────────────────

pub mod ui {
    pub use crate::serve::{start_ui_server, DEFAULT_UI_BIND};

    /// Rebuild the embedded UI from source (runs `npm install && npm run build`
    /// in the `ui/` directory adjacent to the workspace root).
    /// Normally invoked by the build script; exposed here for tooling / dev workflows.
    pub fn build_ui(ui_dir: &Path) -> Result<()>;
}
```

`netsim-utils/build.rs` calls `ui::build_ui(ui_dir)` so the logic is not duplicated
between the build script and the library function.

**Path adjustment for `build.rs`**: `CARGO_MANIFEST_DIR` will be `netsim-utils/`,
so `ui/` is one level up:
```rust
let ui_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent().unwrap().join("ui");
build_ui(&ui_dir)?;
```

---

## `netsim` Public API (lib + bin)

```rust
// Re-export the entire netsim-core as `core`, plus the most common top-level
// items so callers can write `use netsim::{Lab, init_userns}` without reaching
// into sub-modules.
pub use netsim_core as core;

// Most-used items surfaced at crate root for ergonomics:
pub use netsim_core::{
    Lab, DeviceBuilder, NodeId, NatMode, Impair, ObservedAddr,
    init_userns, check_caps,
    config::{LabConfig, RouterConfig, RegionConfig},
};
pub use netsim_utils::assets::BinaryOverride;

mod sim;  // private; all orchestration detail

// ── Sim runner entry points ───────────────────────────────────────────────────

/// Run one or more simulations.
pub async fn run_sims(opts: RunOpts) -> Result<RunSummary>;

/// Build / fetch binaries declared in sim files without executing steps.
pub async fn prepare_sims(opts: PrepareOpts) -> Result<()>;

// ── Options ───────────────────────────────────────────────────────────────────

pub struct RunOpts {
    /// Paths to `.toml` sim files (directories are globbed for `*.toml`).
    pub sim_paths: Vec<PathBuf>,
    /// Root for run output directories.
    pub work_dir: PathBuf,
    /// Project root for `cargo build` invocations (defaults to cwd).
    pub build_root: PathBuf,
    /// Already-parsed binary overrides (use `parse_binary_overrides` to build
    /// these from CLI `--binary name:mode:value` strings).
    pub binary_overrides: Vec<(String, BinaryOverride)>,
    /// Skip all builds; expect artifacts already present.
    pub no_build: bool,
    /// Mirror spawned-process stdout/stderr to the terminal.
    pub verbose: bool,
}

impl RunOpts {
    /// Build `RunOpts` from environment defaults.
    /// `work_dir` defaults to `$NETSIM_WORK_DIR` or `.netsim-work`;
    /// `build_root` defaults to the current directory.
    pub fn from_env(sim_paths: Vec<PathBuf>) -> Self;
}

pub struct PrepareOpts {
    pub sim_paths: Vec<PathBuf>,
    pub work_dir: PathBuf,
    pub build_root: PathBuf,
    pub binary_overrides: Vec<(String, BinaryOverride)>,
    pub no_build: bool,
}

// ── Summary / result types ────────────────────────────────────────────────────

pub struct RunSummary {
    pub run_root: PathBuf,
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub sims: Vec<SimOutcome>,
}

pub struct SimOutcome {
    pub name: String,
    pub sim_dir: PathBuf,
    pub status: SimStatus,
    pub runtime_ms: u128,
    /// Per-step measurement results collected via `[step.results]` mappings.
    pub step_results: Vec<StepResult>,
}

pub enum SimStatus {
    Passed,
    Failed { error: String },
}

/// Generic per-step measurement record.
/// Fields are optional because any individual result capture may be absent.
pub struct StepResult {
    pub id: String,
    pub elapsed_s: Option<f64>,
    pub size_bytes: Option<u64>,   // total bytes (up+down combined, or down)
    pub up_bytes: Option<u64>,
    pub down_bytes: Option<u64>,
    pub up_mbps: Option<f64>,
    pub down_mbps: Option<f64>,
}
```

`sim/` internals (`SimFile`, `Step`, `StepEntry`, etc.) remain `pub(crate)`.

**`main.rs` update**: the binary builds `RunOpts` from clap args (already-parsed
`BinaryOverride` values via `parse_binary_overrides`, then collects into the vec).
The `from_env` constructor is used when `--work-dir` and `--build-root` are absent.

---

## Implementation Steps

### Step 0 — Prep

Nothing pending (plans dir already created, MAINTENANCE.md already removed).

---

### Step 1 — Scaffold crate directories

```bash
mkdir -p netsim-core/src netsim-utils/src netsim/src/sim netsim-vm/src
```

Create stub `Cargo.toml` + `src/lib.rs` for each new crate so workspace compiles
throughout. Fill real deps incrementally as files move in.

---

### Step 2 — Populate `netsim-core`

**File copies** (keep originals until `cargo check -p netsim-core` is clean):

```bash
cp src/core.rs      netsim-core/src/core.rs
cp src/netlink.rs   netsim-core/src/netlink.rs
cp src/netns.rs     netsim-core/src/netns.rs
cp src/qdisc.rs     netsim-core/src/qdisc.rs
cp src/userns.rs    netsim-core/src/userns.rs
cp src/util.rs      netsim-core/src/util.rs
```

**`netsim-core/src/lib.rs`**: start from current `src/lib.rs` and:
- Remove `pub mod assets`, `pub mod binary_cache`, `pub mod serve` and their imports.
- Remove `bootstrap_userns` (replaced by `init_userns` with OnceLock — see Step 8).
- Add `pub mod test_utils;` referencing the moved probe/reflector helpers.
- In `pub mod config`, rename `RouterCfg` → `RouterConfig` throughout.
- In `core.rs`, rename `LabCore` → `NetworkCore` throughout (use sed or replace_all
  edit; it appears in ~30 places).

**`netsim-core/src/test_utils.rs`** (new small file): move `spawn_reflector_in`,
`probe_in_ns`, `udp_roundtrip_in_ns`, `udp_rtt_in_ns` out of `src/lib.rs` into this
file, wrapped in `pub mod test_utils`. The `spawn_reflector` / `spawn_reflector_on_ix`
helpers on `Lab` can be left on `Lab` (they're builder convenience methods) or
delegated here — keep them on `Lab` for now and have `test_utils` expose the
free-function versions.

**Rename note for `RouterCfg`**: it appears in `src/lib.rs` (config mod) and in
`src/sim/topology.rs`, `src/sim/runner.rs`, `src/sim/mod.rs`. Apply the rename when
copying to netsim-core; update sim/ references in Step 4.

**`netsim-core/Cargo.toml`**:
```toml
[dependencies]
anyhow = "1"
tokio = { version = "1", features = ["rt", "macros", "sync", "time"] }
rtnetlink = "0.20"
netlink-packet-route = "0.28"
futures = "0.3"
nix = { version = "0.30", features = ["sched","mount","fs","signal","process","user"] }
serde = { version = "1", features = ["derive"] }
toml = "0.8"
tracing = "0.1"
ipnet = "2.11"
libc = "0.2"
chrono = { version = "0.4", default-features = false, features = ["clock"] }

[dev-dependencies]
serial_test = "3"
n0-tracing-test = "0.3"
ctor = "0.2"    # test-only; see Step 8
```

---

### Step 3 — Populate `netsim-utils`

```bash
cp src/assets.rs       netsim-utils/src/assets.rs
cp src/binary_cache.rs netsim-utils/src/binary_cache.rs
cp src/serve.rs        netsim-utils/src/serve.rs
cp build.rs            netsim-utils/build.rs
```

**`netsim-utils/src/lib.rs`**:
```rust
pub mod assets;
pub mod binary_cache;
pub mod ui;
mod serve;   // private; re-exported via ui
```

**`netsim-utils/src/ui.rs`** (new small file):
```rust
use std::path::Path;
use anyhow::Result;
use std::process::{Command, Stdio};

pub use crate::serve::{start_ui_server, DEFAULT_UI_BIND};

/// Build the Vite UI from source.  Called by the build script and exposed
/// for dev tooling.  `ui_dir` is the directory containing `package.json`.
pub fn build_ui(ui_dir: &Path) -> Result<()> {
    run_npm(ui_dir, &["install"])?;
    run_npm(ui_dir, &["run", "build"])
}

fn run_npm(ui_dir: &Path, args: &[&str]) -> Result<()> { … }
```

**`netsim-utils/build.rs`** — delegate to lib:
```rust
fn main() {
    let ui_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().join("ui");
    // Emit rerun-if-changed directives (keep current list).
    netsim_utils::ui::build_ui(&ui_dir).unwrap();
}
```
Wait: `build.rs` cannot call the lib (the lib isn't built yet at build-script time).
Keep the npm invocation logic directly in `build.rs` as today; `ui::build_ui` in the
lib duplicates the same logic for programmatic use. They share an extracted private
`run_npm(dir, args)` helper; easiest approach is to put it in both since build.rs and
the lib are distinct compilation units.

**`netsim-utils/Cargo.toml`**:
```toml
[dependencies]
anyhow = "1"
serde = { version = "1", features = ["derive"] }
reqwest = { version = "0.12", default-features = false, features = ["blocking","rustls-tls","stream"] }
sha2 = "0.10"
flate2 = "1"
tar = "0.4"
tracing = "0.1"
webbrowser = "1"
glob = "0.3"
# add http server deps used by serve.rs (axum or similar)
```

No workspace dependencies.

---

### Step 4 — Populate `netsim`

```bash
cp -r src/sim   netsim/src/sim
cp src/main.rs  netsim/src/main.rs
```

**`netsim/src/lib.rs`** — write fresh:
```rust
pub use netsim_core as core;
pub use netsim_core::{Lab, DeviceBuilder, NodeId, NatMode, Impair, ObservedAddr,
                      init_userns, check_caps,
                      config::{LabConfig, RouterConfig, RegionConfig}};
pub use netsim_utils::assets::BinaryOverride;

mod sim;

pub async fn run_sims(opts: RunOpts) -> Result<RunSummary> {
    sim::runner::run_sims_impl(opts).await
}
pub async fn prepare_sims(opts: PrepareOpts) -> Result<()> {
    sim::runner::prepare_sims_impl(opts).await
}

pub struct RunOpts { … }
impl RunOpts { pub fn from_env(sim_paths: Vec<PathBuf>) -> Self { … } }
pub struct PrepareOpts { … }
pub struct RunSummary { … }
pub struct SimOutcome { … }
pub enum SimStatus { … }
pub struct StepResult { … }
```

**Import changes inside `netsim/src/sim/`** — all `use netsim::*` become:
```
netsim::assets::*      → netsim_utils::assets::*
netsim::binary_cache::*→ netsim_utils::binary_cache::*
netsim::serve::*       → netsim_utils::ui::*
netsim::Lab            → netsim_core::Lab
netsim::config::*      → netsim_core::config::*
netsim::NatMode        → netsim_core::NatMode
netsim::Impair         → netsim_core::Impair
RouterCfg              → RouterConfig (all occurrences in topology.rs, runner.rs, mod.rs)
```

**`netsim/src/main.rs`** changes:
- `use netsim::serve` → `use netsim_utils::ui`
- `--binary` arg parsing: parse strings into `Vec<(String, BinaryOverride)>` via
  `netsim_utils::assets::parse_binary_overrides`, store typed values in `RunOpts`.
- Use `RunOpts::from_env` for defaults when flags are absent.
- The explicit `bootstrap_userns()` call becomes `init_userns()` (or `crate::init_userns()`
  since lib re-exports it).

**`netsim/Cargo.toml`**:
```toml
[dependencies]
netsim-core  = { path = "../netsim-core" }
netsim-utils = { path = "../netsim-utils" }
anyhow = "1"
tokio  = { version = "1", features = ["rt-multi-thread", "macros", "sync", "time", "fs", "process"] }
clap   = { version = "4", features = ["derive"] }
serde  = { version = "1", features = ["derive"] }
serde_json = "1"
toml   = "0.8"
regex  = "1"
comfy-table = "7"
ctrlc  = "3"
rcgen  = "0.13"
sha2   = "0.10"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
chrono = { version = "0.4", default-features = false, features = ["clock"] }
glob   = "0.3"
ctor   = "0.2"    # for the binary ctor — see Step 8

[dev-dependencies]
serial_test = "3"
n0-tracing-test = "0.3"
```

---

### Step 5 — Move `netsim-vm`

```bash
mv crates/netsim-vm netsim-vm
rmdir crates   # if now empty
```

**`netsim-vm/Cargo.toml`**:
```toml
# remove:  netsim = { path = "../.." }
# add:
netsim-utils = { path = "../netsim-utils" }
```

**Import changes**:
```
use netsim::assets::*       → use netsim_utils::assets::*
use netsim::binary_cache::* → use netsim_utils::binary_cache::*
use netsim::serve::*        → use netsim_utils::ui::*
```
That is the complete change set for netsim-vm.

---

### Step 6 — Rewrite root `Cargo.toml`

```toml
[workspace]
members  = ["netsim-core", "netsim-utils", "netsim", "netsim-vm"]
resolver = "2"

[workspace.dependencies]
anyhow  = "1"
tokio   = { version = "1", features = ["rt-multi-thread","macros","sync","time","fs"] }
serde   = { version = "1", features = ["derive"] }
tracing = "0.1"
```

Then delete the old source tree **only after `cargo build --workspace` is green**:
```bash
rm -rf src build.rs
```

---

### Step 7 — Verify

```bash
cargo check  -p netsim-core   # must be clean first
cargo check  -p netsim-utils
cargo check  -p netsim
cargo check  -p netsim-vm
cargo build  --workspace
cargo test   --workspace      # network tests need caps; skip via env filter if needed
```

---

### Step 8 — `ctor` / `init_userns` refactor

#### Overview

| | Current | After |
|---|---|---|
| ELF init | `#[ctor]` in `userns.rs` (raw libc) | `#[ctor]` in `netsim/src/init.rs` (still raw libc) |
| Explicit call in `main()` | `bootstrap_userns()` | `init_userns()` (OnceLock-idempotent) |
| Tests | covered by lib ctor | `#[ctor]` in `netsim-core` test module (dev-dep only) |

#### Changes

**`netsim-core/src/userns.rs`**:
- Keep the private `userns_bootstrap_libc()` (raw, pre-TLS safe); rename to
  `pub unsafe fn init_userns_for_ctor()` and make it public.
- Add public `init_userns() -> Result<()>` with an internal `OnceLock` guard:
  ```rust
  pub fn init_userns() -> Result<()> {
      static R: OnceLock<std::result::Result<(), String>> = OnceLock::new();
      R.get_or_init(|| {
          // Short-circuit if the ELF ctor already ran successfully.
          #[cfg(target_os = "linux")]
          if nix::unistd::Uid::effective().is_root() { return Ok(()); }
          do_bootstrap().map_err(|e| e.to_string())
      })
      .as_ref().map(|_| ()).map_err(|e| anyhow::anyhow!("{e}"))
  }
  ```
  where `do_bootstrap()` is the current body of `bootstrap_userns()`.
- Remove `#[ctor::ctor] fn userns_bootstrap_ctor()` from this file.
- Re-export both fns from `netsim-core/src/lib.rs`.
- Remove `ctor` from `netsim-core` `[dependencies]`; it remains in `[dev-dependencies]`.

**`netsim/src/init.rs`** (new file):
```rust
/// ELF .init_array bootstrap — runs before main() and before tokio spawns threads.
/// Uses raw libc so it is safe to call before Rust TLS initialisation.
#[cfg(target_os = "linux")]
#[ctor::ctor]
fn userns_ctor() {
    // SAFETY: single-threaded .init_array context; raw libc only.
    unsafe { netsim_core::init_userns_for_ctor() }
}
```
Add `mod init;` to `netsim/src/lib.rs` (or `main.rs`).

**`netsim/src/main.rs`**: keep `init_userns()?` at the very top of `fn main()` —
the OnceLock makes it a no-op when the ctor already ran.

**Test bootstrap in `netsim-core`**: add to `netsim-core/src/lib.rs`:
```rust
#[cfg(test)]
mod test_init {
    #[ctor::ctor]
    fn init() { let _ = super::init_userns(); }
}
```
`ctor` is only in `[dev-dependencies]` of `netsim-core`, so it does not leak into
the library's dependency surface.

#### Alternative (no `ctor` at all)

Calling `init_userns()` at the top of `main()` before `#[tokio::main]` is sufficient
for the binary because Tokio creates its thread pool inside the macro expansion.
For tests, the `#[ctor]` approach is the most ergonomic (centralised, automatic).
Replacing it with a `setup()` call at the top of every `#[test]` function also works
and eliminates the `ctor` dev-dep entirely, at the cost of boilerplate.

---

### Step 9 — Report / `SimOutcome` refactor  *(separate step, after Step 7–8 are green)*

#### Assessment

`report.rs` has structural problems inherited from an earlier domain-specific use case:

1. **`TransferResult` has dead fields**: `provider`, `fetcher`, `final_conn_direct`,
   `conn_upgrade`, `conn_events` are never populated by the current code — they're all
   hardcoded to empty-string / `None` / `0`.  Yet they flow into the markdown tables and
   the UI's `types.ts`.

2. **Markdown tables are static**: columns are hardcoded regardless of whether a given
   run has any data in those columns.  A run with only `elapsed_s` and `down_mbps`
   produces a table with 10 columns, most of them blank.

3. **`write_results` and `write_combined_results_for_runs` are tightly coupled to the
   `TransferResult` shape**, preventing future result types from being added cleanly.

4. **`SimOutcome` (in the `netsim` lib API defined in Step 4) carries `step_results`**,
   but the current internal `SimRunOutcome` doesn't expose step data to the caller —
   the results are written to disk and that's the only path.

#### Target model

```rust
// netsim/src/sim/report.rs  (after refactor)

/// Generic per-step measurement.  Replaces TransferResult + StepResultRecord.
pub(crate) struct StepResultRecord {
    pub id: String,
    pub elapsed_s: Option<f64>,
    pub size_bytes: Option<u64>,
    pub up_bytes: Option<u64>,
    pub down_bytes: Option<u64>,
    pub up_mbps: Option<f64>,
    pub down_mbps: Option<f64>,
}
// (mirrors the public StepResult in netsim/src/lib.rs — convert on the way out)
```

Remove `TransferResult` struct.  Consolidate `StepResultRecord` + `TransferResult`
into the single struct above.

#### Markdown output (data-driven)

```rust
fn write_md_table(records: &[StepResultRecord]) -> String {
    // Determine which columns have at least one non-None value.
    // Emit only those columns.
    let has_elapsed   = records.iter().any(|r| r.elapsed_s.is_some());
    let has_size      = records.iter().any(|r| r.size_bytes.is_some());
    let has_up_mbps   = records.iter().any(|r| r.up_mbps.is_some());
    let has_down_mbps = records.iter().any(|r| r.down_mbps.is_some());
    // … build header + rows dynamically
}
```

This replaces the hardcoded 10-column table with one that only shows populated columns.

#### `results.json` shape

```json
{
  "sim": "sim-name",
  "steps": [
    { "id": "xfer", "elapsed_s": 1.23, "down_mbps": 42.5, "size_bytes": 65536000 }
  ]
}
```

Remove the `"transfers"` key (rename to `"steps"`) and the `"iperf"` key from the
per-sim results JSON (iperf data should be a separate file or a separate key only
written when iperf results exist).

#### `combined-results.json` shape

Keep the `"runs"` array; inside each run, rename `"transfers"` → `"steps"`.

#### `SimOutcome` integration

The internal `SimRunOutcome` gains a `step_results: Vec<StepResultRecord>` field.
`run_sims_impl()` maps this to the public `SimOutcome::step_results: Vec<StepResult>`
before returning `RunSummary`.

#### UI changes required

`ui/src/types.ts`:
```ts
// Replace TransferResult with:
export interface StepResult {
  id: string
  elapsed_s?: number
  size_bytes?: number
  up_bytes?: number
  down_bytes?: number
  up_mbps?: number
  down_mbps?: number
}

export interface SimResults {
  sim: string
  steps: StepResult[]   // renamed from 'transfers'
  iperf?: IperfResult[] // optional
}

// CombinedRunResult: rename transfers → steps too
```

`ui/src/components/PerfTab.tsx`:
- Remove per-node throughput table (requires `provider`/`fetcher` which no longer exist).
- The step details table (`up_mbps`, `down_mbps`, `elapsed_s`, `size_bytes`) is kept.
- Rename `results.transfers` → `results.steps` everywhere.

`ui/src/App.tsx`:
- Remove `transferNodeThroughput` function (uses `provider`/`fetcher`).
- Remove `throughputFromTransfersOrIperf` or simplify: aggregate `up_mbps`/`down_mbps`
  directly from `steps` without per-node breakdown.
- `nodeCount`: remove inference from `provider`/`fetcher`; use `setup.devices` from
  `SimSummary` as the node count (it's already there).
- Rename `row.transfers` → `row.steps` in `CombinedRunResult` handling.

#### Tests to update

The two existing tests in `report.rs` (`step_result_record_computes_mbps`,
`write_results_writes_json_and_markdown`) need:
- Field renames (`duration_raw` → duration computed differently, or keep raw string approach).
- JSON assertion keys updated (`"transfers"` → `"steps"`).
- Markdown assertion: verify only non-empty columns appear.

