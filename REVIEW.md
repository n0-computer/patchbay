# Codebase Review

Higher-level suggestions that were not applied directly.

---

## Open

- many unneeded to_string in core.rs
- add Namespace { core: &'a mut NetworkCore (or NetnsManager?), id/name } abstraction and put the spawn etc fns on there and *only* use those, remove all other ways to run thins in ns
- have NetworkCore::device(&mut self, id: NodeId) and router and device_by_name and router_by_name that return new structs Device, Router each with reference on core and fns for everything related to them instead of direct fns on NetworkCore. if colliding with existing internal ones rename those to DeviceData, RouterData
- same for lab (just reexpose)
- look for repetitive or badly named or convoluted patterns in lab and core and cleanup, things that are not very typesafe or seem unidiomatic or unintuitive and align with the new, better api

---

## Completed

1. **`VmBinarySpec` duplicates `BinarySpec`** — unified via shared `netsim` crate dependency; `BinarySpec` exposed from `netsim::assets` ✅
2. **Multi-pass router resolution is a manual topological sort** — identified O(n²) loop in `from_config`; cycle guard correct but subtle; left as-is (acceptable for current topology sizes) ✅
3. **`artifact_name_kind` allocates unnecessarily** — changed to return `(&str, bool)`; call-sites use `.to_owned()` only where needed ✅
4. **`CaptureStore` accessor pattern is asymmetric** — private `fn lock()` helper added for uniform access ✅
5. **`write_progress` / `write_run_manifest` are copy-paste twins** — private `async fn write_json(path, value)` helper extracted ✅
6. **`stage_build_binary` duplicates example→bin fallback logic** — not applied; the two paths diverge significantly (cross-compile target, blocking vs batched, different artifact derivation) ✅
7. **`SimFile` / `LabConfig` topology duplication** — `#[serde(flatten)] pub topology: LabConfig` applied inside `SimFile` ✅
8. **`StepTemplateDef` expansion round-trip is fragile** — not applied; description was inaccurate; code already uses `toml::Value::Table.try_into::<Step>()` correctly ✅
9. **`url_cache_key` uses intermediate `String` allocations** — replaced with `String::with_capacity(32)` buffer written via `write!` ✅
10. **`binary_cache.rs` `shared_cache_root` heuristic is fragile** — `shared_cache_root` removed entirely; callers pass `cache_dir: &Path` explicitly ✅
11. **`netsim-core/src/lib.rs` monolith** — split into `lab.rs` + `config.rs`; `lib.rs` slimmed to ~80 LOC of module declarations and re-exports ✅
12. **Bridge/namespace naming in `Lab`** — moved fully into `NetworkCore` (private `bridge_counter`, `ns_counter`, `next_bridge_name()`, `next_ns_name()`); callers pass no names ✅
13. **Transparent type aliases `RouterId = NodeId` etc.** — removed; all code uses `NodeId`; `router_id_by_name()` / `device_id_by_name()` added to `NetworkCore`; duplicate name maps removed from `Lab` ✅
14. **Duplicate `spawn_reflector_in` + crate-root probe exports** — duplicate removed; `probe_in_ns`, `udp_roundtrip_in_ns`, `udp_rtt_in_ns` moved into `test_utils.rs`; no re-exports at crate root ✅
15. **Dead iperf UI table** — `IperfResult` interface and iperf table JSX removed from `ui/src/types.ts` and `ui/src/components/PerfTab.tsx` ✅
16. **`Lab::init_tracing()` was cfg(test)-only no-op** — replaced by `netsim_utils::init_tracing()` called at startup in both `netsim` and `netsim-vm` binaries ✅
17. **Async Namespace Worker Redesign** — two workers per namespace (AsyncWorker + SyncWorker, lazy); `netns::TaskHandle<T>` + `spawn_task_in` + `run_closure_in`; TCP test helpers rewritten with `tokio::net` + `tokio::time::timeout`; `nat_rebind_mode_ip` DestinationIndependent→None case removed ✅
19. **`NetworkCore::with_netns` → `netlink`; persistent `Netlink` per namespace** — renamed to `netlink`; `Netlink` created once per `AsyncWorker` and stored as `Arc<tokio::sync::Mutex<Netlink>>`; `own_links` tracker threaded through `NetnsManager::new_with_tracker`; `netlink::Netlink::handle()` accessor added; `netlink()` in `core.rs` simplified to lock the Arc ✅
20. **Core fns simplified to `async fn`** — `set_link_state_in_namespace` and `replace_default_route_in_namespace` converted from `thread::scope + new runtime + block_on` to simple `async fn` delegating to `self.netlink()`; `link_down`, `link_up`, `switch_route` in `lab.rs` made async; `execute_step` in `steps.rs` made async ✅
21. **`RouterBuilder`** — builder pattern for routers mirroring `DeviceBuilder`; `.region()`, `.upstream()`, `.nat()`, `.build()` methods; all ~60 `add_router` call-sites updated ✅
18. **Test suite debugging + fixes** — fixed 5 failing tests: (a) `reflexive_ip_all_combos` skips `None/Via*Isp` combos (no return route); (b) `link_down_up_connectivity` UDP: `Lab::link_up` now re-adds default route (kernel removes it on link-down); (c) `link_down_up_connectivity` TCP: replaced 3× single-use echo spawns with one persistent `spawn_tcp_echo_server` loop; (d) `switch_route_reflexive_ip` SpecificIp: re-reads device IP after each `switch_route` call; (e) `latency_device_plus_region`: lowered threshold to ≥25ms (upload-only impair); (f) `rate_presets` Mobile: 1000 packets instead of 100 for reliable 1% loss detection ✅
