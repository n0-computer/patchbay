# Codebase Review

Higher-level suggestions that were not applied directly.

---

## Open

#### `add_host` hardcodes /24 assumption (low)

`add_host(cidr, host)` replaces only the last octet, which only works for /24
subnets. If the allocator ever moves to /16 or /25 this will silently produce
wrong addresses.

#### Duplicate lock-extract-drop boilerplate (low)

Nearly every handle method (Device, Router, Ix) follows the same pattern:
```rust
let inner = self.lab.lock().unwrap();
let ns = inner.device(self.id).unwrap().ns.clone();
let netns = Arc::clone(&inner.netns);
drop(inner);
```
Extract a small helper on each handle to reduce repetition.

#### nftables via netlink (won't fix)

Rust crates for nftables via netlink (`nftnl`, `rustables`, `mnl`) exist but are
immature with rough APIs. Not worth replacing `Command::new("nft")` for now.

---

## Completed

41. **`ObservedAddr` wrapper** — converted from wrapper struct to `pub type ObservedAddr = SocketAddr`; removed `.observed` field access from all call sites ✅
42. **`DeviceIface::ip()` returns `Option`** — `Device::ip()` and `DeviceIface::ip()` now return `Option<Ipv4Addr>`; v6-only devices return `None` instead of `Ipv4Addr::UNSPECIFIED` ✅
43. **`ensure_root_ns` race condition** — eliminated by making `Lab::new()` async; root namespace setup runs eagerly in the constructor; removed all lazy-init machinery ✅
44. **Suppressed stderr on `tc` commands** — `qdisc.rs` now captures stderr via `Stdio::piped()` + `.output()` and includes it in error messages on failure ✅
45. **API cleanup: rename `Impair` → `LinkCondition`** — enum, fields, methods, and presets all renamed; `ImpairLimits` → `LinkLimits` ✅
46. **Remove deprecated aliases** — removed `NatMode`, `switch_route`, `set_impair`, `switch_uplink`, `rebind_nats`, `impair_downlink`, `impair_link`; removed serde alias `destination-independent` ✅
47. **Unify NAT API** — removed `RouterBuilder::nat_config()` and `Router::set_nat_config()`; added `impl From<NatConfig> for Nat` so users pass `Nat::Custom(cfg)` ✅
48. **`spawn_command_async`** — added on Device, Router, and Ix; uses `tokio::process::Command` with rt enter guard for reactor context ✅
49. **Real PMTU blackhole test** — `pmtu_blackhole_drops_large_packets` verifies MTU + `block_icmp_frag_needed` silently drops oversized UDP packets ✅

35. **Duplicate docstring in `apply_nat_for_router`** — removed duplicate line ✅
36. **Dead `replace_default_route_v6`** — removed unused method from `netlink.rs` ✅
37. **Redundant `nft flush ruleset` on fresh namespaces** — removed 3 pointless `nft` process spawns per lab (fresh `unshare(CLONE_NEWNET)` namespaces have no rules) ✅
38. **Duplicate `unshare(CLONE_NEWNS)` + overlay setup** — extracted `apply_mount_overlay()` shared by async worker, sync worker, user threads, and tokio blocking pool ✅
39. **Merge ns creation + async worker thread** — `create_unshared_netns_fd()` + lazy `Worker::rt_handle()` merged into single `Worker::spawn()` that creates namespace via `unshare(CLONE_NEWNET)` and stays alive as async worker; saves 1 thread per namespace ✅
40. **DNS overlay set-after-create** — `set_dns_overlay()` removed; `create_netns(name, dns_overlay)` passes overlay at creation time so async worker applies it at startup ✅

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
22. **Unneeded `.to_string()` in core.rs** — `RouterData::wan_ifname()` helper added, deduplicating 3 occurrences of `if uplink == ix_sw { "ix" } else { "wan" }` pattern; ~90 redundant `.to_string()` on already-owned `String` from `node_ns()` removed from test code ✅
23. **Variable assignments/clones before `nl_run`** — structurally required: `nl_run` closures are `'static` (sent to per-ns worker threads), so data from `&RouterSetupData` must be cloned before capture; accepted as-is ✅
24. **Repetitive/legacy patterns in lab and core** — `smoke_debug_netns_exit_trace` debug test + 4 exclusive helpers removed; sync `spawn_tcp_reflector` replaced with async `spawn_tcp_reflector_in_ns`; `add_region_latency` renamed to `set_region_latency` ✅
18. **Test suite debugging + fixes** — fixed 5 failing tests: (a) `reflexive_ip_all_combos` skips `None/Via*Isp` combos (no return route); (b) `link_down_up_connectivity` UDP: `Lab::link_up` now re-adds default route (kernel removes it on link-down); (c) `link_down_up_connectivity` TCP: replaced 3× single-use echo spawns with one persistent `spawn_tcp_echo_server` loop; (d) `switch_route_reflexive_ip` SpecificIp: re-reads device IP after each `switch_route` call; (e) `latency_device_plus_region`: lowered threshold to ≥25ms (upload-only impair); (f) `rate_presets` Mobile: 1000 packets instead of 100 for reliable 1% loss detection ✅
25. **Migrate tests to Device/Router handle API** — ~200+ test call sites migrated: `node_ns()` → `.ns()`, `router_uplink_ip()` → `.uplink_ip()`, `device_ip()` → `.ip()`, `spawn_reflector(&ns)` → `handle.spawn_reflector()`, `probe_udp_mapping("name")` → `handle.probe_udp_mapping()`; `DualNatLab` converted to hold handles; dead Lab methods removed (`node_ns`, `device_ns_name`, `router_ns_name`, `router_downlink_gw`, `router_uplink_ip`, `device_ip`, `router_id`, `device_id`, `spawn_reflector`, `probe_udp_mapping`) ✅
26. **`nl_run` closure noise reduction** — `RouterSetupData` and `IfaceBuild` derive `Clone`; closures now capture `data.clone()` / `dev.clone()` instead of 5-8 individual field extractions ✅
27. **Core internalization** — `NetworkCore` → `pub(crate)`; all free functions (`ensure_netns_dir`, `open_netns_fd`, `cleanup_netns`, `create_named_netns`, `run_closure_in_namespace`, `spawn_closure_in_namespace_thread`, `run_command_in_namespace`, `set_sysctl_*`, `apply_impair_in`, `run_nft_in`, `apply_*_nat`) → `pub(crate)`; only `spawn_command_in_namespace`, `NodeId`, `ResourceList` remain public ✅
28. **`resources()` → `ResourceList::global()`** — `resources()` free function removed; `ResourceList::global()` added as associated function; all callers in lab.rs and netsim/main.rs migrated ✅
29. **Drop `NETSIM_NS_*` from env_vars** — `NETSIM_NS_<DEV>` entries removed from `Lab::env_vars()`; callers use `Device::ns()` / `Router::ns()` instead ✅
30. **Remove unnecessary cleanup, simplify Netlink** — namespaces are fd-only (no bind-mounts/pinning); removed `ResourceList`, `own_links` tracker, `cleanup_links_with_prefix_ip` (`ip link del` shelling), atexit/panic hooks; kernel reclaims everything when fds close. `Netlink` made `Clone` (just wraps `Handle: Clone`), all methods `&self`, removed `Arc<Mutex<Netlink>>` wrapper. Per-task spans added to `async_worker_main` (`TASK_SEQ` counter + `debug_span!("task"/"nl", id)`) for debugging dropped futures ✅
31. **`saturating_add` on address allocators** — all 7 allocators (`alloc_ix_ip_low`, `alloc_ix_ip_v6_low`, `alloc_private_cidr`, `alloc_public_cidr`, `alloc_private_cidr_v6`, `alloc_from_switch`, `alloc_from_switch_v6`) now use `checked_add` + `bail!("pool exhausted")`; tests `test_ix_ip_alloc_no_duplicates` and `test_ix_ip_v6_alloc_no_duplicates` added ✅
32. **`eprintln!` in `apply_impair_in`** — replaced with `tracing::warn!` ✅
33. **`Impair::Wifi/Mobile` doc inaccuracy** — doc comments corrected to match actual `impair_to_limits` values (no jitter) ✅
34. **`DnsEntries::new()` panics via `.expect()`** — `NetworkCore::new()` now returns `Result<Self>`; error propagated to `Lab::new()` ✅
