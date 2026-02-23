# Ergonomics Plan

## TODO

- [x] Write plan
- [x] Remove obsolete VM code from `netsim` crate (`src/vm.rs`, `run-vm` command)
- [x] Add shared asset handling (`src/assets.rs`) via `netsim` → `netsim-vm` dependency
- [x] Add shared embedded UI server + `serve` command in both CLIs
- [x] Add `--open` run-time serving with keep-open behavior
- [x] Add `progress.json` + incremental `manifest.json` updates in runner
- [x] Simplify CLI summary table to status + up/down numbers
- [x] Migrate transfer binary path to `target:examples/transfer` shortcut
- [ ] Final review

## Goals
- Remove obsolete VM orchestration from `netsim` crate and keep VM ownership in `netsim-vm`.
- Share asset/path handling logic via `netsim` library dependency from `netsim-vm`.
- Add a shared embedded-UI HTTP server and expose `serve` command in both CLIs.
- Add run-time UI serving for `run` flows with `--open`, including long-lived serving until Ctrl-C.
- Improve run visibility with `progress.json` + incremental `manifest.json` updates.
- Simplify CLI post-run output to one concise per-sim summary table (`status`, `down`, `up`).
- Replace VM-only hardcoded transfer path with portable `target:examples/transfer` shortcut resolution.

## Scope
- Rust crates: `netsim` (`src/main.rs`, `src/lib.rs`, `src/sim/*`, new shared modules), `crates/netsim-vm` (`src/main.rs`, `src/vm.rs`, `src/util.rs`, `Cargo.toml`).
- UI: `ui/src/*` polling and progress presentation.
- Config/docs/plans: `iroh-integration/iroh-defaults.toml`, `plans/PLAN.md`, `AGENTS.md`.

## Design

### 1. Shared library modules in `netsim`
1. Add `assets` module for:
   - `--binary` override parsing (shared syntax and validation).
   - `target:<kind>/<name>` expansion (`examples|bin`, release-only).
   - target dir resolution precedence:
     1. `NETSIM_TARGET_DIR`
     2. `cargo metadata --format-version 1 --no-deps` `target_directory`
     3. hard error
   - VM-aware lookup preference: when flagged as VM mode, prefer musl target path first if present.
2. Add `serve` module for:
   - embedded `ui/dist/index.html` serving.
   - static file serving from work root.
   - `GET /__netsim/runs` run listing endpoint.
   - browser open helper and server lifecycle handle.

### 2. CLI command refactor
1. `netsim`:
   - remove `run-vm` command and obsolete `src/vm.rs` usage.
   - add `serve` command.
   - extend `run` with `--open` to start server and open UI before/while run.
   - after run completion with `--open`, keep process serving until Ctrl-C.
2. `netsim-vm`:
   - add dependency on `netsim`.
   - add `serve` command using shared serve module.
   - extend `run` with `--open` and same keep-open behavior.

### 3. Sim runner reporting/progress
1. Introduce `progress.json` at run root with incremental updates:
   - run status (`running|done`), counts, current sim, per-sim statuses.
2. Update run `manifest.json` incrementally as each sim finishes.
3. Refresh combined artifacts during run so UI can show partial results.
4. Replace verbose combined terminal tables with concise single-run summary table:
   - `sim`, `status`, `down_mbps`, `up_mbps`.

### 4. Portable target shortcut migration
1. Switch `iroh-integration/iroh-defaults.toml`:
   - `/target/.../examples/transfer` -> `target:examples/transfer`.
2. Ensure path expansion works in:
   - local `netsim run`.
   - guest runs via `netsim-vm run` (VM mode musl preference).

### 5. UI behavior updates
1. Poll `progress.json` while run status is `running`.
2. Re-fetch manifest/results/combined during active runs.
3. Show run progress/state prominently (current sim, completed/total, per-sim state).

## Validation
1. `cargo check`
2. `cargo clippy --tests --examples --fix`
3. `cargo fmt`
4. `cd ui && npm run build`

## Risks
- Embedded UI path compile-time coupling (`ui/dist/index.html`) can fail if stale/missing.
- Keeping server open after run changes command completion behavior; should be opt-in via `--open`.
- Incremental manifest/progress writes must stay atomic enough for UI polling.
