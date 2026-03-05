# Codebase Review

Higher-level suggestions that were not applied directly.

---

## Open

* server crate: scan dir recursively. if events.jsonl found in current dir then serve only that, otherwise scan up to 3 layers deep and use all dirs. wanna run this in testdir (see testdir crate, 

* workdir vs outdir, clarify relation. runner-sim e2e test sets both, this should not be needed

* add RegionLink::is_empty and same for link conditions to remove manual > 0 checks at callsites. break region link should be async. never use run_closure_in with Command if you can easily make the call site async, use async command then to not block calling thread on sync cmds

#### IPv6 link-local / RA-driven provisioning branch (dev)

**`Ipv6Profile` variants are currently redundant** — `ProductionLike`,
`ConsumerHome`, `MobileCarrier`, `Enterprise` all map to `(Enabled, RaDriven)`.
Either add a comment noting future divergence intent or collapse them.

**`DeviceSetupData` unnecessary clone** — `dev: dev.clone()` in
`DeviceBuilder::build()` when only a few fields are needed later for event
emission.

#### `add_host` hardcodes /24 assumption (low)

`add_host(cidr, host)` replaces only the last octet, which only works for /24
subnets. If the allocator ever moves to /16 or /25, this will silently produce
wrong addresses.

#### nftables via netlink (won't fix)

Rust crates for nftables via netlink (`nftnl`, `rustables`, `mnl`) exist but
have immature APIs. Not worth replacing `Command::new("nft")` for now.

#### Dead `apply_region_latency_dual` + `Qdisc` methods (low)

`qdisc.rs` contains `apply_region_latency_dual()` and the full `Qdisc` builder
(htb root, classes, netem, filters). These were written for per-destination
latency shaping inside region routers but were never wired up; the current
approach uses simple netem on inter-region veths. Delete or wire up when
revisiting virtual-time or advanced region latency.

#### TOML config ignores regions (medium)

`from_config()` parses `regions` from TOML but does not call `add_region()`
or `link_regions()`. Region topologies can only be built programmatically.

#### Link condition loss is egress-only (medium)

`tc netem loss` on a device interface only affects outbound packets. A "50%
loss" link actually delivers ~50% in one direction and 100% in the other,
which does not match how real lossy links (e.g. WiFi) behave. We should
either apply netem on both ends of the veth pair, or use a single ingress +
egress qdisc setup (e.g. ifb mirroring) so that `LinkCondition::Wifi` gives
symmetric loss. The test bounds in `loss_udp_moderate` are currently widened
to paper over this. Needs a design decision on where to apply qdiscs.

#### `spawn_reflector` is fire-and-forget (medium)

`spawn_reflector` enqueues a task on the namespace worker and returns
immediately. There is no guarantee the reflector socket is bound by the time
callers start sending probes, which causes intermittent test failures. The
API should be async (return once the socket is bound) and return a drop
guard that stops the reflector when dropped. This would eliminate the manual
`sleep(300ms)` after every `spawn_reflector` call in tests.

#### `ip route replace` shelling in break/restore (low)

`break_region_link()` and `restore_region_link()` use `Command::new("ip")`
to replace routes. These could use the netlink API (`nl_run` + `Netlink`)
for consistency, but the sync `run_closure_in` path avoids async overhead
for these rare operations.

---

## Completed

59. **Arc\<str\> migration + NetworkCore method extraction** - All internal `String` fields migrated to `Arc<str>` (clones become refcount bumps). Lock-body logic extracted into `NetworkCore` methods (`prepare_link_regions`, `prepare_add_iface`, `prepare_replug_iface`, `remove_device`, `remove_router`, `resolve_link_target`, `remove_device_iface`, `renew_device_ip`, `add_dns_entry`, `router_nat_v6_params`, etc.) with purpose-built setup structs. Eliminates `let x; { lock; x = ...; }` pattern throughout `lab.rs` and `handles.rs`.
57. **Mutex/lock architecture overhaul** - `LabInner` struct with `netns` + `cancel` outside the mutex; `with()`/`with_mut()` helpers on handles; cached `name`/`ns` on Device/Router/Ix; per-node `tokio::sync::Mutex<()>` for operation serialization; `parking_lot::Mutex` for topology lock; all handle mutation methods made async; pre-await reads combined into single lock; `set_nat_v6_mode` write order fixed
58. **No-panics refactor** - Device/Router handles return `Result` or `Option` instead of panicking on removed nodes; `spawn()` returns `Result<JoinHandle>`; `with_device`/`with_router` return `Option<R>`; all test call sites updated
56. **Region index overflow** - `region_base(idx)` uses `checked_mul(16).expect()` instead of unchecked arithmetic
55. **Fix doc typo** - removed `(aka LinkCondition)` redundancy from lab.rs module doc
54. **Consolidate test helpers** - removed `probe_udp_from`, `spawn_tcp_echo_in`, sync `udp_send_recv_count`; all callers migrated to `test_utils` equivalents
53. **Remove dead region code** - deleted unused allocators and fields
52. **Combine consecutive `nl_run` blocks** - merged v4 + v6 return-route calls in `setup_router_async`
51. **Replace `parse().unwrap()` with direct construction** - `net4()`, `net6()`, `region_base()` helpers
50. **`RouterBuilder::error` helper** - deduplicated 15-field struct literal in error paths
49. **Real PMTU blackhole test** - verifies MTU + `block_icmp_frag_needed` drops oversized packets
48. **`spawn_command_async`** - added on Device, Router, and Ix
47. **Unify NAT API** - removed `RouterBuilder::nat_config()`; users pass `Nat::Custom(cfg)`
46. **Remove deprecated aliases** - removed `NatMode`, `switch_route`, `set_impair`, etc.
45. **API rename `Impair` -> `LinkCondition`** - enum, fields, methods, and presets all renamed
44. **Suppress stderr on `tc` commands** - stderr captured via `Stdio::piped()` + `.output()`
43. **`ensure_root_ns` race condition** - eliminated by making `Lab::new()` async
42. **`DeviceIface::ip()` returns `Option`** - v6-only devices return `None`
41. **`ObservedAddr` wrapper** - converted to `pub type ObservedAddr = SocketAddr`
40. **DNS overlay at creation time** - `create_netns(name, dns_overlay)` instead of set-after-create
39. **Merge ns creation + async worker** - single `Worker::spawn()` saves 1 thread per namespace
38. **Extract `apply_mount_overlay()`** - shared by async/sync workers, user threads, blocking pool
37. **Remove redundant `nft flush ruleset`** - fresh namespaces have no rules
36. **Dead `replace_default_route_v6`** - removed unused method
35. **Duplicate docstring in `apply_nat_for_router`** - removed
34. **`DnsEntries::new()` panics** - `NetworkCore::new()` now returns `Result`
33. **`Impair::Wifi/Mobile` doc inaccuracy** - corrected to match actual values
32. **`eprintln!` in `apply_impair_in`** - replaced with `tracing::warn!`
31. **`saturating_add` on allocators** - all 7 allocators use `checked_add` + `bail!`
30. **Remove unnecessary cleanup, simplify Netlink** - fd-only namespaces, removed ResourceList and hooks
29. **Drop `NETSIM_NS_*` from env_vars** - callers use handle methods instead
28. **`resources()` -> `ResourceList::global()`** - associated function instead of free fn
27. **Core internalization** - `NetworkCore` and helpers made `pub(crate)`
26. **`nl_run` closure noise** - `RouterSetupData`/`IfaceBuild` derive `Clone`
25. **Migrate tests to handle API** - ~200+ call sites migrated, dead Lab methods removed
24. **Remove repetitive/legacy patterns** - debug test removed, helpers consolidated
23. **Variable clones before `nl_run`** - structurally required, accepted as-is
22. **`RouterData::wan_ifname()` helper** - deduplicates wan interface name logic
21. **`RouterBuilder`** - builder pattern matching `DeviceBuilder`
20. **Core fns simplified to `async fn`** - `set_link_state`/`replace_default_route` use netlink directly
19. **Persistent `Netlink` per namespace** - created once per AsyncWorker
18. **Test suite debugging** - fixed 5+ failing tests across nat/link/region tests
17. **Async namespace worker redesign** - two workers per namespace with `TaskHandle<T>`
16. **`Lab::init_tracing()` cleanup** - replaced with `patchbay_utils::init_tracing()`
15. **Dead iperf UI table** - removed from UI
14. **Duplicate `spawn_reflector_in`** - consolidated into `test_utils.rs`
13. **Remove `RouterId` type aliases** - all code uses `NodeId`
12. **Bridge/namespace naming** - moved into `NetworkCore`
11. **Split `lib.rs` monolith** - into `lab.rs` + `config.rs`
10. **`shared_cache_root` removal** - callers pass `cache_dir` explicitly
9. **`url_cache_key` allocations** - replaced with `write!` buffer
8. **`StepTemplateDef` expansion** - not applied, already correct
7. **`SimFile`/`LabConfig` duplication** - `#[serde(flatten)]` applied
6. **`stage_build_binary` dedup** - not applied, paths diverge
5. **`write_progress`/`write_run_manifest` twins** - extracted `write_json` helper
4. **`CaptureStore` accessor pattern** - private `fn lock()` helper added
3. **`artifact_name_kind` allocations** - returns `(&str, bool)` now
2. **Multi-pass router resolution** - accepted as-is for current topology sizes
1. **`VmBinarySpec` duplication** - unified via shared crate dependency
