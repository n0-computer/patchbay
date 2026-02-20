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
| `iroh-netsim.md` | 17. Phase 4 `count` device expansion | ❌ | `count` parsing/expansion not implemented in lab topology loading. |
| `iroh-netsim.md` | 18. Runner support for `fetchers = [...]` in transfer | ❌ | Runner/transfer currently require singular `fetcher` path (`src/sim/transfer.rs`, `src/sim/runner.rs`). |
| `iroh-netsim.md` | 19. 1→N sim files | ✅ | `iroh-1to10-public.toml` present (`iroh-integration/sims/`). |
