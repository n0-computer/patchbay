# netsim-vm Standalone VM Runner Plan

## TODO

- [x] Write plan
- [x] Add standalone `crates/netsim-vm` bin crate and workspace wiring
- [x] Migrate VM lifecycle/mount logic from `src/vm.rs` into `netsim-vm`
- [x] Add `netsim-vm run` with `--netsim-version` (latest, release tag, `git:<ref>`, `path:`)
- [x] Add `netsim-vm test` VM test execution (host build/discover, guest execute, summary)
- [x] Replace Makefile VM tasks with `cargo run -p netsim-vm -- ...`
- [ ] Remove `qemu-vm.sh` file from repository
- [ ] Remove `netsim run-vm` path from `netsim` binary (deferred by request)
- [ ] Final review

## Goal
Replace shell-based VM orchestration (`qemu-vm.sh`) with a standalone workspace binary crate `netsim-vm` that supports both VM sim runs and VM test execution (`cargo make test-vm` parity), with no Rust crate dependency between `netsim` and `netsim-vm`.

## Hard Requirements
1. `netsim-vm` is a binary crate in this workspace.
2. Preferred: no Rust dependency edges between `netsim` and `netsim-vm`.
   - Transitional allowance: `netsim-vm -> netsim` dependency is acceptable now to share build/staging logic; can be feature-gated or removed later.
3. `netsim-vm` can:
   - run sims in VM using a selected `netsim` source (`latest` release, release tag, `git:<ref>`, or `path:<local-binary>`)
   - run tests in VM (current `test-vm` workflow parity)
4. `qemu-vm.sh` is removed from active workflow (and then removed from repo after parity validation).

## CLI Surface (Proposed)

Binary: `netsim-vm`

1. `netsim-vm run [OPTIONS] <SIM_OR_DIR>...`
   - Purpose: VM equivalent of current `netsim run-vm`.
   - Args:
     - `--work-dir <PATH>` default `.netsim-work`
     - `--binary <name:mode:value>` repeatable
     - `--recreate` recreate VM if runtime mount metadata mismatches
     - `--netsim-version <SOURCE>` default `latest`
       - accepted forms:
         - `latest`
         - release version tag (e.g. `0.10.0`, normalized to `v0.10.0` for GH release lookup)
         - git ref `git:<branch-or-ref>` (e.g. `git:feat/foo`)
2. `netsim-vm test [OPTIONS] [-- <CARGO_TEST_ARGS...>]`
   - Purpose: replaces current `cargo make test-vm`.
   - Args:
     - `--target <TRIPLE>` default `x86_64-unknown-linux-musl`
     - `--package <PKG>` repeatable (optional filters)
     - `--test <NAME>` repeatable (optional filters)
     - `--recreate`
     - passthrough args after `--` to host-side `cargo test --no-run`
3. `netsim-vm up [OPTIONS]`
   - Boot or reuse VM and ensure mounts.
4. `netsim-vm down`
   - Stop VM and helper processes.
5. `netsim-vm status`
   - Show running state, PID, mount/runtime metadata checks.
6. `netsim-vm ssh -- <CMD...>`
   - Execute command over guest SSH with configured key/port.
7. `netsim-vm cleanup`
   - Best-effort cleanup of VM helper artifacts/processes (seed server, virtiofsd sockets/pids, stale runtime files).

## Internal Structure (crate `crates/netsim-vm`)

Keep it intentionally simple:

1. `src/main.rs`
   - clap command parsing and dispatch.
2. `src/vm.rs`
   - all VM behavior in one module, like current `netsim/src/vm.rs`:
     - config/env resolution (`QEMU_VM_*`)
     - lifecycle (`up/down/status/cleanup`)
     - cloud-init + seed handling
     - mounts (`/app`, `/target`, `/work`)
     - guest exec (`run`, `test`, `ssh`)
     - host-side test build/discovery for `test` command
3. `src/util.rs` (optional)
   - small helpers for GH download/extract + generic command helpers.

No additional submodules unless strictly needed.

## Behavioral Parity Matrix

1. Preserve existing VM runtime/state paths:
   - `.qemu-vm/<name>/...`
2. Preserve shared base image cache:
   - `dirs::data_dir()/netsim-rs/qemu-images`
3. Preserve mount model:
   - guest `/app` readonly, `/target` readonly, `/work` readwrite
4. Preserve QEMU defaults:
   - accel auto detect (`kvm`/`hvf`/`tcg`), seed HTTP fallback, ssh forward, qemu-img backing disk flow.
5. Preserve cleanup of helper processes and sockets.

## Artifact Strategy Options (host -> guest)

Context:
- VM run typically needs:
  1. a `netsim` executable
  2. optional sim binaries (from `--binary ...`)
- VM test needs test executables compiled for linux-musl and visible in guest.

### Option A (Recommended): Explicit source + staged binaries dir
- Behavior:
  - Resolve `netsim` by `--netsim-version`:
    - `latest` / release tag -> download GH release artifact
    - `git:<ref>` -> host-build `netsim` for musl from workspace
  - Resolve extra binaries:
    - `path:` -> copy file into `<work_dir>/binaries`
    - `build:` -> build musl target from workspace, then copy artifact into `<work_dir>/binaries`
    - `fetch:` -> download to `<work_dir>/binaries`
  - Always run guest from staged `<work_dir>/binaries/*`.
- Tradeoffs:
  - Pros: deterministic guest inputs, easiest debugging, unified run/test staging model.
  - Cons: extra copy step and disk usage.

### Option B: Bind-mount host target directly and execute in place
- Behavior:
  - Build/download on host and execute from mounted `/target` or workspace path.
- Tradeoffs:
  - Pros: less copying, faster iterative local dev.
  - Cons: fragile path coupling, stale-artifact confusion, weaker reproducibility.

### Option C: Manifest-driven artifact lock
- Behavior:
  - Build/download, then write a manifest lock (`binaries.lock.json`) and enforce only locked artifacts are run.
- Tradeoffs:
  - Pros: strongest reproducibility and provenance.
  - Cons: more implementation overhead and UX friction.

Decision for now:
- Implement **Option A** first.
- Keep code paths structured so Option C can be added later without changing CLI.

## Test Flow Design (`netsim-vm test`)

1. Host compile phase:
   - run `cargo test --no-run --target x86_64-unknown-linux-musl` (plus filters/passthrough).
   - remove stale matching test binaries before compile (existing behavior parity).
2. Artifact discovery:
   - collect test executables from musl target `deps/` with explicit filtering:
     - executable regular files
     - filenames matching selected crate/package test artifact prefixes
     - skip stale executables by deleting matching previous `deps/<crate>-*` before build
   - stage selected binaries under `<work_dir>/binaries/tests/`.
3. Guest prep:
   - ensure VM up and mounts.
   - ensure runtime deps installed in guest (same package set as current prepare-vm).
4. Guest execution:
   - execute each test binary in guest with controlled env.
   - stream output and preserve exit codes.
5. Summary:
   - final terminal summary table with pass/fail per test binary and overall status.

### `netsim-vm test` command contract
- Default behavior: workspace-wide test binary build (`cargo test --no-run --target <target>`).
- Filtering:
  - `--package` and `--test` map to cargo flags during build and to artifact filters during discovery.
- Extra args after `--`:
  - appended to host `cargo test --no-run ...` invocation.
- Target dir resolution:
  - resolve with `cargo metadata` (`target_directory`) first;
  - respect explicit `CARGO_TARGET_DIR` when set;
  - compose per-target path (`<target_dir>/<triple>/...`).

## Makefile Migration

1. Replace shell-wrapper VM tasks:
   - `setup-vm` -> `cargo run -p netsim-vm -- up`
   - `run-vm` -> `cargo run -p netsim-vm -- run ...`
   - `test-vm` -> `cargo run -p netsim-vm -- test ...`
   - `vm-status` -> `cargo run -p netsim-vm -- status`
   - `vm-down` -> `cargo run -p netsim-vm -- down`
2. Remove dependency on `qemu-vm.sh` in tasks.
3. After validation, remove `qemu-vm.sh` file.

## netsim CLI Changes

1. Remove `run-vm` command from `netsim` binary (strict ownership split).
2. `netsim cleanup` no longer stops VM; VM lifecycle is `netsim-vm down/cleanup`.

## Implementation Steps

1. Workspace setup
   - add `crates/netsim-vm` bin crate.
   - update root workspace members.
2. VM logic move
   - migrate `src/vm.rs` logic into `crates/netsim-vm/src/vm.rs` (single module style).
   - keep code close to existing literal command flow; avoid structural refactors.
3. GH `netsim` download runner path
   - keep robust staging and guest invocation (helper functions may live in `util.rs`).
4. Add `netsim-vm test` command
   - implement build/discover/execute/summarize test flow.
5. Rewire Makefile tasks
   - replace shell script calls with `netsim-vm`.
6. Remove old integration
   - remove `src/vm.rs` and `netsim` `run-vm`.
7. Docs + plan updates
   - `README.md`, `AGENTS.md`, `plans/selfcontained.md`, `plans/PLAN.md`.
8. Validation
   - `cargo check`
   - `cargo test`
   - smoke:
     - `cargo run -p netsim-vm -- up`
     - `cargo run -p netsim-vm -- run ...`
     - `cargo run -p netsim-vm -- test`
     - `cargo run -p netsim-vm -- down`

## Shared Code Note
- For initial delivery, allow `netsim-vm` to reuse build/staging helpers from `netsim` if it materially reduces duplicate logic.
- Add a follow-up item to feature-gate or split shared helpers into a neutral crate if coupling becomes a maintenance issue.

## Risks

1. Test artifact discovery can drift across cargo/rust versions.
2. GH release asset naming drift can break automatic download.
3. CLI split may break existing user habits (`netsim run-vm` removed).

## Acceptance Criteria

1. `cargo make run-vm` and `cargo make test-vm` work without `qemu-vm.sh`.
2. `netsim-vm run` executes sims in guest using downloaded release `netsim`.
3. `netsim-vm test` executes compiled tests in guest and returns correct aggregate status.
4. No Rust dependency between `netsim` and `netsim-vm`.
5. `qemu-vm.sh` is no longer required and removed after parity confirmation.

## Open Issues And Future Work

1. Release availability:
   - `--netsim-version latest` currently depends on published GH release assets; until published, use `path:` or `git:`.
2. Make ergonomics for source selection:
   - `run-vm` currently defaults to `path:${target_dir}/debug/netsim`; consider adding a dedicated `run-vm-release` task for GH-release mode.
3. Artifact provenance:
   - add optional lock manifest (`binaries.lock.json`) for stronger reproducibility and auditing.
4. Coupling cleanup:
   - if shared build/staging logic grows, extract into a neutral helper crate and remove transitional coupling allowances.
5. Ownership finalization:
   - decide whether to remove `netsim run-vm` and `qemu-vm.sh` entirely once downstream users migrate.
