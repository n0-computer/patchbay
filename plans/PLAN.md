# Plan Overview

Status key: ✅ implemented, ⚠️ partially implemented, ❌ not implemented.

| Plan | Step | Status | Evidence |
| --- | --- | --- | --- |
| `initial.md` | 1. Fix compile errors (§1) | ✅ | Builds with current deps/features; `nix` has `user`; no legacy import issues (`Cargo.toml`, `src/lib.rs`). |
| `initial.md` | 2. Add serde+toml deps and `Lab::load` (§4) | ✅ | `serde`/`toml` present and `Lab::load` implemented (`Cargo.toml`, `src/lib.rs`). |
| `initial.md` | 3. Add name maps + `run_on` / `spawn_on` (§2) | ✅ | Name maps and APIs implemented (`src/lib.rs`). |
| `initial.md` | 4. Add `Gateway` enum + DC/ISP device build paths (§3, §5) | ❌ | Superseded by unified router model + multi-interface `DeviceBuilder`; no `Gateway` API (`src/lib.rs`, `src/core.rs`). |
| `initial.md` | 5. Impair via `tc netem` incl. region latency (§5c, §5d) | ✅ | Implemented in `qdisc` and applied from build (`src/qdisc.rs`, `src/core.rs`). |
| `no-sudo.md` | Phase 0: capability/policy diagnostics + `netsim doctor` | ⚠️ | `check_caps()` exists, but no `NoNewPrivs` check or `doctor` command (`src/lib.rs`, `src/main.rs`). |
| `no-sudo.md` | Phase 1: namespace backend abstraction (`auto|named|fd`) | ⚠️ | Backend selection and modes implemented, but not as trait with full planned surface (`src/netns.rs`). |
| `no-sudo.md` | Phase 2: netns executor refactor | ✅ | `NetnsManager` worker-thread executor implemented (`src/netns.rs`, `src/core.rs`). |
| `no-sudo.md` | Phase 3: lab-root isolation hardening + guard tests | ⚠️ | Lab-root namespace model is implemented, but guard tests for host-route leakage are not present (`src/core.rs`, tests). |
| `no-sudo.md` | Phase 4: cleanup guarantees + leak regression tests | ⚠️ | Cleanup/resource tracking exists, but full planned leak-regression suite is incomplete (`src/core.rs`, `src/netns.rs`, `src/lib.rs` tests). |
| `no-sudo.md` | Phase 5: test matrix + docs | ❌ | No explicit matrix/docs artifact matching this phase in `plans/` or repo docs. |
| `iroh-netsim.md` | 1. `core.rs` multi-iface `Device` types | ✅ | `DeviceIface`/multi-interface device model present (`src/core.rs`). |
| `iroh-netsim.md` | 2. `lib.rs` `DeviceBuilder`, remove old router APIs | ✅ | `DeviceBuilder` + `add_router`; old `add_isp/add_dc/add_home` removed (`src/lib.rs`). |
| `iroh-netsim.md` | 3. Build path `IfaceBuild` + `wire_iface` | ✅ | `IfaceBuild` and `wire_iface` implemented (`src/core.rs`). |
| `iroh-netsim.md` | 4. TOML parse for `[[router]]` + `[device.*.*]` | ✅ | `Lab::from_config` parses this format (`src/lib.rs`). |
| `iroh-netsim.md` | 5. `qdisc::remove_qdisc_r` | ✅ | Implemented (`src/qdisc.rs`). |
| `iroh-netsim.md` | 6. Dynamic ops: `set_impair/link_down/link_up/switch_route` | ✅ | Implemented (`src/lib.rs`). |
| `iroh-netsim.md` | 7. Tests for dynamic ops | ✅ | Dynamic op tests present (`src/lib.rs`). |
| `iroh-netsim.md` | 8. `sim/topology.rs` with reused topology parsing | ⚠️ | Functionality exists via `load_topology` in runner, but dedicated module is absent (`src/sim/runner.rs`). |
| `iroh-netsim.md` | 9. `sim/env.rs` env-map + interpolation | ✅ | Implemented (`src/sim/env.rs`). |
| `iroh-netsim.md` | 10. `sim/build.rs` binary build/fetch (`git/url/path`) | ✅ | Implemented (`src/sim/build.rs`). |
| `iroh-netsim.md` | 11. `sim/transfer.rs` transfer orchestration/log reading | ✅ | Implemented for single fetcher flow (`src/sim/transfer.rs`). |
| `iroh-netsim.md` | 12. `sim/report.rs` parse logs + emit reports | ✅ | `results.json`/`results.md` writing implemented (`src/sim/report.rs`). |
| `iroh-netsim.md` | 13. `sim/runner.rs` step executor (`wait-for` default 300s, actions) | ✅ | Implemented (`src/sim/runner.rs`). |
| `iroh-netsim.md` | 14. CLI wiring in `src/main.rs` | ✅ | Implemented (`src/main.rs`). |
| `iroh-netsim.md` | 15. Write `iroh-integration/topos/*.toml` and `sims/*.toml` | ✅ | Files exist (`iroh-integration/topos/`, `iroh-integration/sims/`). |
| `iroh-netsim.md` | 16. End-to-end `cargo make run-vm` proof | ❌ | No confirmed/recorded completion evidence in repo artifacts. |
| `iroh-netsim.md` | 17. Phase 4 `count` device expansion | ✅ | `Lab::from_config` expands device templates with `count` into suffixed devices (`src/lib.rs`). |
| `iroh-netsim.md` | 18. Runner support for `fetchers = [...]` in transfer | ✅ | `iroh-transfer` supports `fetchers` and emits per-fetcher results (`src/sim/transfer.rs`, `src/sim/runner.rs`). |
| `iroh-netsim.md` | 19. 1→N sim files | ✅ | `iroh-1to10-public.toml` present (`iroh-integration/sims/`). |
| `iroh-netsim.md` | 20. Shared binary file support (`[sim] binaries = ...`) | ✅ | Shared binaries manifest loading and merge with inline specs implemented (`src/sim/mod.rs`, `src/sim/runner.rs`). |
| `iroh-netsim.md` | 21. Generic CLI `--binary name:mode:value` overrides | ✅ | Repeatable `--binary` override parsing with `build|fetch|path` modes implemented (`src/main.rs`, `src/sim/runner.rs`). |
| `iroh-netsim.md` | 22. `path` override copy-to-workdir semantics | ✅ | Path overrides are staged into `<work_dir>/bins` and chmodded executable (`src/sim/runner.rs`). |
| `iroh-netsim.md` | 23. Override validation + resolved-source startup reporting | ⚠️ | Validation implemented; startup logs resolved binary path per name, but no dedicated summary table yet (`src/sim/runner.rs`). |
| `iroh-netsim.md` | 24. Tests/examples for shared binaries + overrides | ⚠️ | Added override parser tests + shared defaults file + sims switched to shared binaries; merge-path tests are still limited (`src/sim/runner.rs`, `iroh-integration/iroh-defaults.toml`, `iroh-integration/sims/`). |
| `selfcontained.md` | 1. Reshape CLI into explicit `run`/`run-vm`/`setup-caps` subcommands | ⚠️ | Implemented historically, but now partially superseded by `netsim-vm-split.md` (planned ownership move of VM commands out of `netsim`). |
| `selfcontained.md` | 2. Embed `qemu-vm.sh` behavior in Rust VM module | ⚠️ | Implemented historically in `src/vm.rs`, but planned to move into standalone `crates/netsim-vm` and retire `qemu-vm.sh` (`netsim-vm-split.md`). |
| `selfcontained.md` | 3. Implement built-in self capability setup (`netsim setup-caps`) | ✅ | Implemented with `sudo setcap` + verification for self and required tools (`src/caps.rs`, `src/main.rs`). |
| `selfcontained.md` | 4. Keep `setcap.sh` for test binaries and clarify role split | ✅ | `setcap.sh` now explicitly documents repo test/dev scope and points standalone users to `netsim setup-caps` (`setcap.sh`). |
| `selfcontained.md` | 5. Wire `run-vm` to execute `netsim run` in guest | ⚠️ | Implemented in current `netsim` binary, but planned to be superseded by standalone `netsim-vm run` flow (`netsim-vm-split.md`). |
| `selfcontained.md` | 6. Update automation/docs to binary-first workflows | ⚠️ | Implemented for old ownership model; docs/tasks will be revised for `netsim-vm` command ownership (`netsim-vm-split.md`). |
| `selfcontained.md` | 7. Validate local + VM + external-checkout flow | ⚠️ | Local+VM run/test paths validated with `cargo make run-vm` and `cargo make test-vm`; external-checkout validation still pending. |
| `netsim-vm-split.md` | 1. Add standalone `netsim-vm` bin crate and workspace wiring | ✅ | Added `crates/netsim-vm` and workspace members in root manifest (`crates/netsim-vm/*`, `Cargo.toml`). |
| `netsim-vm-split.md` | 2. Migrate VM lifecycle/mount logic from `src/vm.rs` into `netsim-vm` | ✅ | Ported VM lifecycle + guest orchestration into `crates/netsim-vm/src/vm.rs` with CLI dispatch in `crates/netsim-vm/src/main.rs`. |
| `netsim-vm-split.md` | 3. Add `netsim-vm run` using GH-downloaded `netsim` guest binary | ✅ | Implemented `netsim-vm run` with `--netsim-version` (`latest`, release tag, `git:<ref>`, `path:<local-binary>`) and guest staging under `/work/.netsim-bin` (`crates/netsim-vm/src/vm.rs`). |
| `netsim-vm-split.md` | 4. Add `netsim-vm test` for VM test execution parity (`test-vm`) | ✅ | Implemented `netsim-vm test`: host `cargo test --no-run --message-format json`, staging to `/work/binaries/tests`, guest execution + summary (`crates/netsim-vm/src/vm.rs`). |
| `netsim-vm-split.md` | 5. Replace Makefile VM tasks and retire `qemu-vm.sh` | ⚠️ | Makefile VM tasks now call `cargo run -p netsim-vm -- ...`; `qemu-vm.sh` still present in repo pending explicit removal (`Makefile.toml`). |
| `netsim-vm-split.md` | 6. Remove `netsim run-vm` path and finalize docs split | ⚠️ | Deferred by request to avoid altering `netsim` crate behavior; docs/plans updated but `netsim run-vm` remains available. |
| `ui.md` | 1. Scaffold Vite + React + TS project at `ui/` | ✅ | `ui/package.json`, `vite.config.ts`, `tsconfig.json`; builds to single `dist/index.html` via `vite-plugin-singlefile` (`ui/`). |
| `ui.md` | 2. Dev server: serve `.netsim-work` + run listing endpoint | ✅ | Vite plugin serves work root files; `GET /__netsim/runs` returns dir listing; default path `<repo>/.netsim-work`; `NETSIMS=` override (`ui/vite.config.ts`). |
| `ui.md` | 3. Perf tab: sortable tables + two-run compare | ✅ | Transfers + iperf tables, all-runs overview, compare diff with Δmbps/Δ% colour coding (`ui/src/components/PerfTab.tsx`). |
| `ui.md` | 4. Logs tab: ANSI tracing + iroh NDJSON rendering + filters | ✅ | Tracing text formatted as `TIME LEVL target: message`; iroh events with badges; level/regex/iroh-only filters; sidebar file tree from manifest (`ui/src/components/LogsTab.tsx`). |
| `ui.md` | 5. Timeline tab: SVG swimlane, Y=time, X=node lanes | ✅ | iroh NDJSON events + tracing WARN/ERROR/INFO + iroh::_events spans; scroll/zoom; tooltips; kind filter toggles (`ui/src/components/TimelineTab.tsx`). |
| `ui.md` | 6. Qlog tab: JSON-seq event table | ✅ | Parses JSON-seq qlog; virtualised table; filter; expand-on-click; category colouring (`ui/src/components/QlogTab.tsx`). |
| `ui.md` | 7. Rust: write `manifest.json` per run dir | ⚠️ | Run-level `manifest.json` is now written and updated incrementally, but per-sim log manifest is still inferred in UI (`src/sim/runner.rs`, `ui/src/App.tsx`). |
| `ui.md` | 8. Rust: embed `dist/index.html` + write to work root | ⚠️ | Embedded serving is implemented via `netsim::serve` and `serve` commands, but UI file is served from binary (not written into work root) (`src/serve.rs`, `src/main.rs`, `crates/netsim-vm/src/main.rs`). |
| `ui.md` | 9. Qlog auto-discovery (index.json per qlog dir) | ❌ | Pending Rust support; qlog tab requires manual paste of path for now. |
| `ergonomics.md` | 1. Remove obsolete VM code from `netsim` crate | ✅ | Removed `run-vm` command and deleted `src/vm.rs`; `netsim` now owns local run/serve/cleanup only (`src/main.rs`, `src/vm.rs`). |
| `ergonomics.md` | 2. Add shared asset handling via `netsim` -> `netsim-vm` dependency | ✅ | Added `netsim` dependency in `netsim-vm` and shared override/target shortcut logic via `netsim::assets` (`crates/netsim-vm/Cargo.toml`, `src/assets.rs`, `crates/netsim-vm/src/util.rs`, `src/sim/runner.rs`). |
| `ergonomics.md` | 3. Add shared embedded UI server + `serve` command in both CLIs | ✅ | Added shared embedded server in `netsim::serve` and `serve` subcommands in both binaries (`src/serve.rs`, `src/main.rs`, `crates/netsim-vm/src/main.rs`). |
| `ergonomics.md` | 4. Add run-time `--open` serving with keep-open behavior | ✅ | Added `--open` to local and VM run commands; server stays up post-run until Ctrl-C (`src/main.rs`, `crates/netsim-vm/src/main.rs`). |
| `ergonomics.md` | 5. Add `progress.json` + incremental `manifest.json` updates | ✅ | Runner now writes/updates `progress.json` and run `manifest.json` during execution (`src/sim/runner.rs`). |
| `ergonomics.md` | 6. Simplify CLI summary table to status + up/down numbers | ✅ | Replaced verbose terminal output with concise run summary (`sim`, `status`, `down_mbps`, `up_mbps`) (`src/sim/report.rs`). |
| `ergonomics.md` | 7. Migrate transfer binary path to `target:examples/transfer` | ✅ | Added `target:` shortcut expansion and migrated iroh default transfer path (`src/assets.rs`, `src/sim/build.rs`, `iroh-integration/iroh-defaults.toml`, `crates/netsim-vm/src/vm.rs`). |
