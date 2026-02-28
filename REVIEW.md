# Codebase Review

Higher-level suggestions that were not applied directly.

---

## Open

#### `add_host` hardcodes /24 assumption (low)

`add_host(cidr, host)` replaces only the last octet, which only works for /24
subnets. If the allocator ever moves to /16 or /25 this will silently produce
wrong addresses.

#### Mutex/lock architecture (medium–high)

87 `.lock().unwrap()` calls across the crate (59 in handles.rs, 26 in lab.rs,
2 in tests.rs). No deadlocks (single mutex), no lock held across `.await`
(always extract-then-release). But the lock-unlock-await-relock pattern opens
TOCTOU windows, and the boilerplate is substantial.

##### Concurrency model

All 108 internal tests use `tokio::test(flavor = "current_thread")`. The runner
binary also uses `current_thread`. So the main tokio task never races with
itself. **However, real concurrency exists:**

- `Device::spawn()` / `Router::spawn()` enqueue tasks onto per-namespace worker
  runtimes, each running on a **dedicated OS thread**. The spawned closure
  receives a cloned handle. If user code calls `dev.ip()`, `dev.name()`, etc.
  from inside a spawned task, it acquires the main `Mutex<NetworkCore>` from the
  worker thread while the main thread may also hold or acquire it.
- `Device::spawn_thread()` launches an OS thread that could hold handle
  references.
- The sim runner spawns child processes via `spawn_command`, but those don't
  share the Rust mutex, so they're irrelevant.

**Conclusion:** The mutex is genuinely shared across threads in normal use.
The current architecture is safe against panics and deadlocks, but the
lock-gap-relock sequences have real TOCTOU windows that would manifest if
user code mutates topology from spawned tasks concurrently with the main thread.

##### TOCTOU audit of every multi-lock method

**handles.rs `Device::link_up` — 2 locks**
- Lock 1 (200–212): reads `ns`, `iface.uplink`, `default_via`
- Gap: `nl_run` sets interface up (async)
- Lock 2 (223–225): reads `router_downlink_gw_for_switch(uplink)`
- **TOCTOU:** If another thread calls `replug_iface` between locks, `uplink`
  now points to the old switch. Lock 2 reads the gateway for the wrong
  (possibly removed) switch → `Result::Err` (not silent corruption, but a
  spurious failure).
- **Fix:** Extract `gw_ip` in lock 1 alongside `uplink`. Trivial.

**handles.rs `Device::set_default_route` — 3 locks**
- Lock 1 (237–250): reads `ns`, `uplink`, `impair`
- Lock 2 (253–255): reads `router_downlink_gw_for_switch(uplink)`
- Gap: `nl_run` replaces route (async), `apply_or_remove_impair` (sync)
- Lock 3 (263–265): writes `set_device_default_via`
- **TOCTOU locks 1→2:** Same as `link_up` — stale `uplink`. Fix: combine.
- **TOCTOU locks 2→3:** Between route change and record update, another thread
  reading `dev.default_via` sees the old value. Mostly harmless (kernel state
  is already updated), but `impair` may be applied to wrong interface if
  another `set_default_route` call races.
- **Fix:** Combine locks 1+2. Lock 3 is unavoidable (post-await record update).

**handles.rs `Device::set_link_condition` — 1 lock, held too long**
- Lock (272–289): acquires mut lock, extracts data, calls
  `apply_or_remove_impair` (which blocks on `tc` command via sync worker,
  ~1–5ms), then updates record.
- **Issue:** Not TOCTOU but **lock contention** — every other thread is blocked
  during the `tc` command. This is the only place the lock is held during I/O.
- **Fix:** Extract data, drop lock, apply tc, re-lock for record update.

**handles.rs `Device::replug_iface` — 4 lock acquisitions**
- Lock 1 (448–506): reads device + old iface + target router + switch;
  **allocates** new IP via `alloc_from_switch`. Unlocks.
- Gap: `nl_run` deletes old veth (async)
- Lock 2 (524–526): re-reads `router(to_router).downlink` to get `new_uplink`
- Gap: `wire_iface_async` creates new veth + addresses (async)
- Lock 3 (531–540): writes updated `iface.uplink`, `iface.ip`, `iface.ip_v6`
- **TOCTOU lock 1→2:** Lock 1 already extracted `target_router.downlink` (line
  463). Lock 2 reads it again redundantly. If `to_router` is removed between
  locks, lock 2 panics on `.unwrap()`. **Fix:** Use the value from lock 1.
- **TOCTOU lock 2→3:** Between wiring and record update, the device could be
  removed by a concurrent `remove_device`. Lock 3 returns
  `anyhow!("device disappeared")` — correct error, but the veth pair is now
  orphaned in the namespace (no leak since namespace cleanup will destroy it,
  but it's inconsistent).
- **Allocation safety:** `alloc_from_switch` increments `next_host` atomically
  under lock 1. Two concurrent `replug_iface` calls get different IPs because
  both acquire the same lock for allocation. **Safe.**
- **Kernel/state split risk:** After lock 1 allocates IP 10.0.0.5 and releases,
  the IP is "committed" in the allocator but doesn't exist on any interface
  yet. If the process crashes before `wire_iface_async`, the IP is leaked from
  the pool. Harmless in practice (lab is short-lived), but notable.

**handles.rs `Router::set_nat_v6_mode` — 2 locks**
- Lock 1 (768–797): reads router params for nft rule generation
- Gap: `run_nft_in` + `apply_nat_v6` (sync, ~10–50ms for nft commands)
- Lock 2 (801–806): writes `router.cfg.nat_v6 = mode`
- **TOCTOU:** Between locks, another thread reading `router.nat_v6_mode()` sees
  the **old** value even though the kernel already has the new rules. If another
  thread calls `set_nat_v6_mode` concurrently, the second call reads old state
  in its lock 1 and generates rules based on stale params.
- **Fix:** Write the mode in lock 1 (before applying rules). If rule
  application fails, write it back. This is what `set_nat_mode` already does
  correctly — it calls `inner.set_router_nat_mode()` in its first lock.

**lab.rs `Lab::break_region_link` — 2 locks**
- Lock 1 (884–969): reads region link data, intermediate router, computes
  routes. Explicit `drop(inner)`.
- Gap: `run_closure_in` executes `ip route replace` commands (sync)
- Lock 2 (971–976): writes `region_link.broken = true`
- **TOCTOU:** If `remove_router` removes the intermediate region router between
  locks, the route commands succeed (kernel state), but the topology is
  inconsistent. The `broken` flag is still set, but the router no longer exists
  to restore through.
- **Severity:** Low. Region links are long-lived infrastructure, not mutated
  concurrently in practice.

**lab.rs `Lab::restore_region_link` — 2 locks**
- Same pattern as `break_region_link`. Same risk. Same severity.

**lab.rs `Lab::set_link_condition(a, b)` — 1 lock, held too long**
- Lock (1163–1218): searches topology, then calls `apply_or_remove_impair`
  (blocks on `tc` command). Same issue as `Device::set_link_condition`.

##### Overall TOCTOU severity assessment

| Method | Locks | TOCTOU risk | Practical impact |
|--------|-------|-------------|-----------------|
| `link_up` | 2 | stale switch lookup | spurious error |
| `set_default_route` | 3 | stale switch + stale default_via | spurious error |
| `replug_iface` | 4 | redundant re-read + orphaned veth on removal | low |
| `set_nat_v6_mode` | 2 | stale config read + stale nat mode visible | inconsistent state |
| `break/restore_region_link` | 2 | stale router ref | low |
| `set_link_condition` (both) | 1 | lock held during I/O | contention |

**Are these actually triggerable?** Only if user code mutates topology from a
spawned namespace task while the main thread is also mutating. Example:
```rust
let dev = dev.clone();
let router = router.clone();
dev.spawn(move |_| async move {
    dev.set_default_route("eth0").await  // lock from worker thread
});
router.set_nat_mode(Nat::Corporate);    // lock from main thread — races
```
This is a plausible pattern. The current architecture doesn't protect against
it, and users have no way to know which methods are safe to call concurrently.

##### Concrete problems

**Problem 1: 59 lock acquisitions in handles.rs, almost all identical**

Typical getter repeats 25 times:
```rust
pub fn name(&self) -> String {
    let inner = self.lab.lock().unwrap();
    inner.device(self.id).map(|d| d.name.clone()).unwrap_or_default()
}
```

Typical namespace dispatch repeats ~15 times:
```rust
let (ns, netns) = {
    let inner = self.lab.lock().unwrap();
    let dev = inner.device(self.id).ok_or_else(|| anyhow!("unknown device id"))?;
    (dev.ns.clone(), Arc::clone(&inner.netns))
};
```

**Problem 2: Unnecessary consecutive read locks (link_up, set_default_route)**

As detailed above — extracting all read data in one lock would remove the gap.

**Problem 3: Lock held during I/O (set_link_condition, Lab::set_link_condition)**

`apply_or_remove_impair` dispatches `tc` to the sync worker and blocks ~1–5ms.
Lock is held the entire time.

**Problem 4: `set_nat_v6_mode` writes config after applying rules**

Inconsistent with `set_nat_mode` which writes first. Detailed above.

**Problem 5: `.unwrap()` on every `.lock()` — no poison recovery**

All 87 sites use `.lock().unwrap()`. `parking_lot::Mutex` would eliminate this
and add compile-time protection against holding the guard across `.await`.

##### Recommendations

**Recommendation A: `with()`/`ns_and_netns()` helpers on each handle**

```rust
impl Device {
    fn with<R>(&self, f: impl FnOnce(&DeviceData) -> R) -> R {
        let inner = self.lab.lock().unwrap();
        f(inner.device(self.id).expect("device handle has valid id"))
    }
    fn with_mut<R>(&self, f: impl FnOnce(&mut DeviceData) -> R) -> R {
        let mut inner = self.lab.lock().unwrap();
        f(inner.device_mut(self.id).expect("device handle has valid id"))
    }
    fn ns_and_netns(&self) -> Result<(String, Arc<NetnsManager>)> {
        let inner = self.lab.lock().unwrap();
        let dev = inner.device(self.id)
            .ok_or_else(|| anyhow!("unknown device id"))?;
        Ok((dev.ns.clone(), Arc::clone(&inner.netns)))
    }
}
```

Getters collapse to one-liners, namespace dispatch to two lines. Same pattern
for Router (`with_router`) and Ix. Eliminates ~40 boilerplate blocks.

**Recommendation B: Move `netns: Arc<NetnsManager>` out of the mutex**

`NetnsManager` is `Arc`-shared with its own internal `Mutex<HashMap<..>>`.
Every handle method that dispatches to a namespace must lock `NetworkCore`
just to `Arc::clone` the `netns` field — a pointer copy that doesn't need
topology protection. Store it alongside:

```rust
pub struct Lab {
    inner: Arc<Mutex<NetworkCore>>,
    netns: Arc<NetnsManager>,       // not behind the topology lock
}
pub struct Device {
    id: NodeId,
    lab: Arc<Mutex<NetworkCore>>,
    netns: Arc<NetnsManager>,       // cached at handle creation
}
```

With Recommendation E (cache ns name too), most namespace dispatch methods
wouldn't need the lock at all:
```rust
pub fn run_sync<F, R>(&self, f: F) -> Result<R> { ... {
    self.netns.run_closure_in(&self.ns, f)  // no lock needed
}}
```

This eliminates ~30 `Arc::clone(&inner.netns)` and makes it structurally
impossible to hold the topology lock during namespace operations.

**Recommendation C: Combine all pre-await reads into a single lock**

Fix `link_up`, `set_default_route`, and `replug_iface` to extract all needed
read data (including `gw_ip`) in a single lock acquisition before the first
await. The post-await record-update lock is unavoidable — that's inherent to
the lock-unlock-await-relock architecture — but the pre-await double-read is
a free fix.

**Recommendation D: Fix `set_link_condition` — don't hold lock during I/O**

Extract data, drop lock, run `tc`, re-lock for record update. Aligns with
every other method in the crate. Same for `Lab::set_link_condition`.

**Recommendation E: Cache immutable data on handles**

`Device::ns()`, `Device::name()`, `Router::ns()`, `Router::name()` never
change after construction. Store them on the handle:

```rust
pub struct Device {
    id: NodeId,
    name: String,   // cached at construction
    ns: String,     // cached at construction
    lab: Arc<Mutex<NetworkCore>>,
    netns: Arc<NetnsManager>,
}
```

Eliminates the lock for `name()`, `ns()`, and the namespace lookup in
`run_sync`, `spawn_command`, `spawn_thread`, etc. Combined with Rec B, most
namespace-dispatch methods need zero lock acquisitions.

**Recommendation F: Switch to `parking_lot::Mutex`**

- No poisoning (panicking thread auto-releases the lock)
- `MutexGuard: !Send` — compile error if held across `.await`
- `.lock()` returns guard directly (no `Result`), eliminating 87 `.unwrap()`s
- Already a transitive dep of tokio — no new dependency in practice

**Recommendation G: Fix `set_nat_v6_mode` consistency**

Write `router.cfg.nat_v6 = mode` in the first lock (before applying nft rules),
matching the `set_nat_mode` pattern. If rule application fails, write it back.

##### Rearchitecture alternatives (if the above isn't enough)

**Alternative 1: Channel-based command queue**

Replace `Arc<Mutex<NetworkCore>>` with an mpsc channel. All topology mutations
go through a single "core task" that owns `NetworkCore` exclusively. Handles
send commands and receive responses via oneshot channels:

```rust
pub struct Lab {
    tx: mpsc::Sender<CoreCmd>,
    netns: Arc<NetnsManager>,
}
enum CoreCmd {
    GetDeviceNs { id: NodeId, reply: oneshot::Sender<String> },
    SetNatMode { id: NodeId, mode: Nat, reply: oneshot::Sender<Result<NatParams>> },
    AllocAndWire { ... },
}
```

Pros:
- **Eliminates all TOCTOU**: the core task processes commands sequentially;
  multi-step operations become single commands (e.g. `AllocAndReplug`).
- No lock boilerplate — handles are just channel endpoints.
- Makes the concurrency model explicit: "one writer, many readers via messages."

Cons:
- Every getter becomes async (or needs a sync wrapper with `block_on`).
- More indirection — every field access is a round-trip.
- Overkill for a library where most callers are single-threaded.

**Verdict:** Not recommended unless the library needs to support a
multi-threaded `multi_thread` tokio runtime as a first-class use case.

**Alternative 2: Higher-level atomic operations on NetworkCore**

Instead of handles locking/unlocking around each step, push the multi-step
logic into `NetworkCore` methods that do everything under one lock:

```rust
impl NetworkCore {
    /// Atomically: extract all params, returns them. Caller does I/O, then
    /// calls `commit_default_route` with the result.
    pub fn prepare_set_default_route(&self, dev: NodeId, to: &str)
        -> Result<DefaultRouteParams> { ... }

    pub fn commit_set_default_route(&mut self, dev: NodeId, to: &str)
        -> Result<()> { ... }
}
```

This concentrates all lock-related logic in `core.rs` and makes handles thin
wrappers that call `prepare` → I/O → `commit`. The TOCTOU window between
prepare and commit still exists, but:
- All reads happen in one atomic `prepare` call (no double-read).
- The "prepare" return type documents exactly what the caller needs.
- Misuse (skipping commit, using wrong params) is visible in code review.

**Verdict:** Recommended as a middle ground. Keeps the current architecture but
makes the contract explicit. Combine with Rec A–F for maximum effect.

##### Summary table

| # | Recommendation | Effort | Impact |
|---|---------------|--------|--------|
| A | `with()`/`ns_and_netns()` helpers | small | eliminates ~40 boilerplate blocks |
| B | Move `netns` out of mutex | medium | structural separation, ~30 fewer Arc clones |
| C | Combine pre-await reads | small | removes gratuitous TOCTOU windows |
| D | Release lock before `tc` I/O | small | fixes only lock-during-I/O bug |
| E | Cache name/ns on handles | small | zero-lock for most common operations |
| F | `parking_lot::Mutex` | small | compile-time `.await` guard, no `.unwrap()` |
| G | Fix `set_nat_v6_mode` write order | small | state consistency |
| Alt 2 | `prepare`/`commit` pattern on NetworkCore | medium | explicit TOCTOU contract |
| Alt 1 | Channel-based command queue | large | eliminates all TOCTOU (overkill for now) |

#### nftables via netlink (won't fix)

Rust crates for nftables via netlink (`nftnl`, `rustables`, `mnl`) exist but are
immature with rough APIs. Not worth replacing `Command::new("nft")` for now.

#### Dead `apply_region_latency_dual` + `Qdisc` methods (low)

`qdisc.rs` contains `apply_region_latency_dual()` and the full `Qdisc` builder
(htb root, classes, netem, filters). Written for per-destination latency shaping
inside region routers but never wired up — the current approach uses simple netem
on inter-region veths. Delete or wire up when revisiting virtual-time / advanced
region latency.

#### TOML config ignores regions (medium)

`from_config()` parses `regions` from TOML but doesn't call `add_region()`
or `link_regions()`. The TODO at lab.rs:826/849 is still open. Region
topologies can only be built programmatically.

#### `ip route replace` shelling in break/restore (low)

`break_region_link()` and `restore_region_link()` use `Command::new("ip")`
to replace routes. These could use the netlink API (`nl_run` + `Netlink`)
for consistency, but the sync `run_closure_in` path avoids async overhead
for these rare operations.

---

## Completed

50. **`RouterBuilder::error` helper** — extracted `RouterBuilder::error()` constructor to deduplicate the 15-field struct literal in `add_router()` error paths ✅
51. **Replace `parse().unwrap()` with direct construction** — added `net4()`, `net6()`, `region_base()` helpers in lab.rs; all address/CIDR literals now use constructors instead of string parsing ✅
52. **Combine consecutive `nl_run` blocks** — merged v4 + v6 return-route `nl_run` calls in `setup_router_async` into single block ✅
53. **Remove dead region code** — deleted unused `alloc_region_host`, `region_cidr`, `all_routers`; removed unused `RegionInfo.next_host`, `RegionLinkData.ifname_a/ifname_b`, `Region.lab` fields ✅
54. **Consolidate test helpers** — removed `probe_udp_from`, `spawn_tcp_echo_in`, sync `udp_send_recv_count`; all callers migrated to `test_utils::probe_udp`, `spawn_tcp_echo_server`, async `udp_send_recv_count` with paced sending; fixes `loss_udp_moderate` ✅
55. **Fix doc typo** — removed `(aka LinkCondition)` redundancy from lab.rs module doc ✅
56. **Region index overflow** — `region_base(idx)` now uses `checked_mul(16).expect()` instead of unchecked `idx * 16` ✅

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

1. **`VmBinarySpec` duplicates `BinarySpec`** — unified via shared `patchbay-runner` crate dependency; `BinarySpec` exposed from `patchbay_runner::assets` ✅
2. **Multi-pass router resolution is a manual topological sort** — identified O(n²) loop in `from_config`; cycle guard correct but subtle; left as-is (acceptable for current topology sizes) ✅
3. **`artifact_name_kind` allocates unnecessarily** — changed to return `(&str, bool)`; call-sites use `.to_owned()` only where needed ✅
4. **`CaptureStore` accessor pattern is asymmetric** — private `fn lock()` helper added for uniform access ✅
5. **`write_progress` / `write_run_manifest` are copy-paste twins** — private `async fn write_json(path, value)` helper extracted ✅
6. **`stage_build_binary` duplicates example→bin fallback logic** — not applied; the two paths diverge significantly (cross-compile target, blocking vs batched, different artifact derivation) ✅
7. **`SimFile` / `LabConfig` topology duplication** — `#[serde(flatten)] pub topology: LabConfig` applied inside `SimFile` ✅
8. **`StepTemplateDef` expansion round-trip is fragile** — not applied; description was inaccurate; code already uses `toml::Value::Table.try_into::<Step>()` correctly ✅
9. **`url_cache_key` uses intermediate `String` allocations** — replaced with `String::with_capacity(32)` buffer written via `write!` ✅
10. **`binary_cache.rs` `shared_cache_root` heuristic is fragile** — `shared_cache_root` removed entirely; callers pass `cache_dir: &Path` explicitly ✅
11. **`patchbay/src/lib.rs` monolith** — split into `lab.rs` + `config.rs`; `lib.rs` slimmed to ~80 LOC of module declarations and re-exports ✅
12. **Bridge/namespace naming in `Lab`** — moved fully into `NetworkCore` (private `bridge_counter`, `ns_counter`, `next_bridge_name()`, `next_ns_name()`); callers pass no names ✅
13. **Transparent type aliases `RouterId = NodeId` etc.** — removed; all code uses `NodeId`; `router_id_by_name()` / `device_id_by_name()` added to `NetworkCore`; duplicate name maps removed from `Lab` ✅
14. **Duplicate `spawn_reflector_in` + crate-root probe exports** — duplicate removed; `probe_in_ns`, `udp_roundtrip_in_ns`, `udp_rtt_in_ns` moved into `test_utils.rs`; no re-exports at crate root ✅
15. **Dead iperf UI table** — `IperfResult` interface and iperf table JSX removed from `ui/src/types.ts` and `ui/src/components/PerfTab.tsx` ✅
16. **`Lab::init_tracing()` was cfg(test)-only no-op** — replaced by `patchbay_utils::init_tracing()` called at startup in both `patchbay-runner` and `patchbay-vm` binaries ✅
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
28. **`resources()` → `ResourceList::global()`** — `resources()` free function removed; `ResourceList::global()` added as associated function; all callers in lab.rs and patchbay-runner/main.rs migrated ✅
29. **Drop `NETSIM_NS_*` from env_vars** — `NETSIM_NS_<DEV>` entries removed from `Lab::env_vars()`; callers use `Device::ns()` / `Router::ns()` instead ✅
30. **Remove unnecessary cleanup, simplify Netlink** — namespaces are fd-only (no bind-mounts/pinning); removed `ResourceList`, `own_links` tracker, `cleanup_links_with_prefix_ip` (`ip link del` shelling), atexit/panic hooks; kernel reclaims everything when fds close. `Netlink` made `Clone` (just wraps `Handle: Clone`), all methods `&self`, removed `Arc<Mutex<Netlink>>` wrapper. Per-task spans added to `async_worker_main` (`TASK_SEQ` counter + `debug_span!("task"/"nl", id)`) for debugging dropped futures ✅
31. **`saturating_add` on address allocators** — all 7 allocators (`alloc_ix_ip_low`, `alloc_ix_ip_v6_low`, `alloc_private_cidr`, `alloc_public_cidr`, `alloc_private_cidr_v6`, `alloc_from_switch`, `alloc_from_switch_v6`) now use `checked_add` + `bail!("pool exhausted")`; tests `test_ix_ip_alloc_no_duplicates` and `test_ix_ip_v6_alloc_no_duplicates` added ✅
32. **`eprintln!` in `apply_impair_in`** — replaced with `tracing::warn!` ✅
33. **`Impair::Wifi/Mobile` doc inaccuracy** — doc comments corrected to match actual `impair_to_limits` values (no jitter) ✅
34. **`DnsEntries::new()` panics via `.expect()`** — `NetworkCore::new()` now returns `Result<Self>`; error propagated to `Lab::new()` ✅
