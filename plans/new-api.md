# New netsim-core API

## Design Principles

1. **NodeId-centric** — all operations use `NodeId`, never string names or namespace names
2. **Instant construction** — `add_router`/`add_device` immediately create namespaces and links (no `build()`)
3. **Opaque namespaces** — ns names never exposed publicly
4. **Private fields, accessor methods** — `Router` and `Device` have no `pub` fields
5. **`Lab` is `Clone`** — wraps `Arc<Mutex<LabInner>>`; `Device`/`Router` are lightweight cloneable handles
6. **Dynamic ops on the node types** — `Device` and `Router` own their mutation methods, no `&mut` needed
7. **Spawn model** — `Device::try_spawn(async |dev: Device| { ... })` gives the closure a cloned handle

## Internal Architecture

```rust
pub struct Lab {
    inner: Arc<Mutex<LabInner>>,
}

impl Clone for Lab { /* Arc::clone */ }

struct LabInner {
    core: NetworkCore,
    region_latencies: Vec<(String, String, u32)>,
}

pub struct Device {
    id: NodeId,
    lab: Arc<Mutex<LabInner>>,
}

pub struct Router {
    id: NodeId,
    lab: Arc<Mutex<LabInner>>,
}
```

### `std::sync::Mutex` Safety Analysis

We use `std::sync::Mutex`, not `tokio::sync::Mutex`. This is safe and deadlock-free
because of the following invariants:

**Invariant 1: The lock is never held across `.await` points.**

Every async method follows a strict lock-compute-unlock-await-lock pattern:

```rust
async fn link_up(&self, ifname: &str) -> Result<()> {
    // Phase 1: lock → read → unlock
    let (ns, gw_ip, is_default) = {
        let inner = self.lab.lock().unwrap();
        // read topology data...
    }; // lock dropped

    // Phase 2: async work, no lock held
    // netlink ops go through NetnsManager channel — does not touch the mutex
    set_link_state(&ns, ifname, true).await?;
    if is_default {
        replace_default_route(&ns, ifname, gw_ip).await?;
    }

    // Phase 3: lock → write bookkeeping → unlock (only if needed)
    Ok(())
}
```

This means the mutex never blocks a tokio worker thread for more than microseconds.
An `async` mutex would add unnecessary overhead since we never need to hold the
lock across a suspension point.

**Invariant 2: No nested locking.**

No method ever calls another method that tries to lock the same mutex while already
holding it. All internal helpers receive pre-extracted data, not the lock guard.

**Invariant 3: No lock ordering between multiple mutexes.**

There is exactly one `Mutex<LabInner>` per lab instance. `NetnsManager` uses its own
internal synchronization (channels + per-ns worker threads), but that's opaque — we
never hold our mutex while waiting on NetnsManager.

**What happens with concurrent calls (same device, two clones)?**

Example: two tasks call `device.link_up("eth0")` concurrently.

```
Task A: lock → read (ns, gw, is_default) → unlock → set_link_state.await → ...
Task B: lock → read (ns, gw, is_default) → unlock → set_link_state.await → ...
```

Both read the same topology state (fine — it's consistent). Both send netlink
commands to the same namespace worker, which serializes them internally. Setting
a link up twice is idempotent. Both might re-add the default route — also
idempotent, also serialized on the worker. No deadlock, no corruption.

**What about `switch_route` + `link_down` racing?**

```
Task A (switch_route): lock → read → unlock → replace_default_route.await
                       → apply_impair (sync, blocks, no lock) → lock → update default_via → unlock
Task B (link_down):    lock → read ns → unlock → set_link_down.await
```

The netlink ops are serialized on the per-namespace worker. The bookkeeping update
in Task A's phase 3 happens under a brief lock that Task B never contends with
(Task B has no phase 3). Even if ordering varies, the end state is consistent:
the interface is down, and default_via points to the switched interface.

**Sync operations that block (tc, nft commands):**

`apply_impair_in`, `run_closure_in_namespace`, etc. are sync and may block the
calling thread for a few milliseconds. They run *outside* the mutex. This blocks
the tokio worker thread momentarily — same as current behavior. No deadlock risk
since they don't touch the mutex.

## Core Types

```rust
// ── Identifiers ──
pub struct NodeId(/* opaque */);  // Copy, Clone, Hash, Eq, Debug

// ── Lab ──
pub struct Lab { /* Arc<Mutex<LabInner>> */ }  // Clone

impl Lab {
    // Construction
    pub fn new() -> Self;
    pub async fn load(path: impl AsRef<Path>) -> Result<Self>;
    pub fn from_config(cfg: LabConfig) -> Result<Self>;

    // Topology construction (sync builders, async build)
    pub fn add_router(&self, name: &str) -> RouterBuilder;
    pub fn add_device(&self, name: &str) -> DeviceBuilder;

    // Region latency (can be set/updated at any time)
    pub fn set_region_latency(&self, from: &str, to: &str, ms: u32);

    // Lookup by name → Option (returns owned handles)
    pub fn router_by_name(&self, name: &str) -> Option<Router>;
    pub fn device_by_name(&self, name: &str) -> Option<Device>;

    // Lookup by id → Option
    pub fn router(&self, id: NodeId) -> Option<Router>;
    pub fn device(&self, id: NodeId) -> Option<Device>;

    // Collections
    pub fn routers(&self) -> Vec<Router>;
    pub fn devices(&self) -> Vec<Device>;

    // Global info
    pub fn ix_gw(&self) -> Ipv4Addr;
    pub fn prefix(&self) -> String;

    // Link impairment (bidirectional, between any two connected nodes)
    pub fn impair_link(&self, from: NodeId, to: NodeId, impair: Option<Impair>) -> Result<()>;

    // Env vars for subprocess injection (NETSIM_IP_<DEV>, etc.)
    pub fn env_vars(&self) -> HashMap<String, String>;

    // Cleanup (Drop handles normal case; these are safety nets)
    pub fn cleanup(&self);
    pub fn cleanup_everything();  // static
}

// ── Router ──
pub struct Router { /* id + Arc<Mutex<LabInner>> */ }  // Clone

impl Router {
    // Accessors
    pub fn id(&self) -> NodeId;
    pub fn name(&self) -> String;
    pub fn region(&self) -> Option<String>;
    pub fn nat_mode(&self) -> NatMode;
    pub fn uplink_ip(&self) -> Option<Ipv4Addr>;
    pub fn downstream_cidr(&self) -> Option<Ipv4Net>;
    pub fn downstream_gw(&self) -> Option<Ipv4Addr>;

    // Dynamic operations
    pub async fn set_nat_mode(&self, mode: NatMode) -> Result<()>;
    pub fn rebind_nats(&self) -> Result<()>;

    // Switch which upstream router this router is connected to
    // Tears down old WAN link, allocates new IP from new upstream's pool,
    // wires new link, re-applies NAT with new WAN IP
    pub async fn set_uplink(&self, upstream: NodeId) -> Result<()>;

    // Spawn
    pub fn spawn<F, T>(&self, f: F) -> tokio::task::JoinHandle<T>
    where
        F: AsyncFnOnce(Router) -> T + Send + 'static,
        T: Send + 'static;

    pub fn spawn_command(&self, cmd: Command) -> Result<std::process::Child>;
}

// ── Device ──
pub struct Device { /* id + Arc<Mutex<LabInner>> */ }  // Clone

impl Device {
    // Accessors
    pub fn id(&self) -> NodeId;
    pub fn name(&self) -> String;
    pub fn ip(&self) -> Ipv4Addr;                         // default iface IP
    pub fn iface(&self, name: &str) -> Option<DeviceIface>;
    pub fn default_iface(&self) -> DeviceIface;
    pub fn interfaces(&self) -> Vec<DeviceIface>;

    // Dynamic operations
    pub async fn link_down(&self, ifname: &str) -> Result<()>;
    pub async fn link_up(&self, ifname: &str) -> Result<()>;
    pub async fn switch_route(&self, to_iface: &str) -> Result<()>;
    pub fn set_impair(&self, ifname: &str, impair: Option<Impair>) -> Result<()>;

    // Switch which router an interface is connected to (e.g. switching WiFi networks)
    // Tears down old link, allocates new IP from target router's pool, wires new link
    pub async fn switch_uplink(&self, ifname: &str, to_router: NodeId) -> Result<()>;

    // Spawn async work — closure receives cloned Device handle
    pub fn spawn<F, T>(&self, f: F) -> tokio::task::JoinHandle<T>
    where
        F: AsyncFnOnce(Device) -> T + Send + 'static,
        T: Send + 'static;

    pub fn try_spawn<F, T, E>(&self, f: F) -> tokio::task::JoinHandle<Result<T, E>>
    where
        F: AsyncFnOnce(Device) -> Result<T, E> + Send + 'static,
        T: Send + 'static,
        E: Send + 'static;

    pub fn spawn_command(&self, cmd: Command) -> Result<std::process::Child>;
}

pub struct DeviceIface { /* owned snapshot, no Arc */ }

impl DeviceIface {
    pub fn name(&self) -> &str;
    pub fn ip(&self) -> Ipv4Addr;
    pub fn impair(&self) -> Option<Impair>;
}

// ── Builders ──
pub struct RouterBuilder { /* ... */ }
impl RouterBuilder {
    pub fn region(self, region: &str) -> Self;
    pub fn upstream(self, parent: NodeId) -> Self;
    pub fn nat(self, mode: NatMode) -> Self;
    pub async fn build(self) -> Result<Router>;
}

pub struct DeviceBuilder { /* ... */ }
impl DeviceBuilder {
    pub fn iface(self, ifname: &str, router: NodeId, impair: Option<Impair>) -> Self;
    pub fn uplink(self, router: NodeId) -> Self;  // shorthand: auto-names eth0, eth1, ...
    pub fn default_via(self, ifname: &str) -> Self;
    pub async fn build(self) -> Result<Device>;
}

// ── Enums (unchanged) ──
pub enum NatMode { None, Cgnat, DestinationIndependent, DestinationDependent }
pub enum Impair { Wifi, Mobile, Manual { rate: u32, loss: f32, latency: u32 } }

// ── Config types (unchanged — pub fields, Deserialize, Default) ──
pub struct LabConfig { /* pub fields as-is for serde flatten */ }
pub struct RouterConfig { /* pub fields as-is */ }
pub struct RegionConfig { /* pub fields as-is */ }

// ── Crate-root free functions ──
pub fn check_caps() -> Result<()>;
pub fn init_userns() -> Result<()>;
pub unsafe fn init_userns_for_ctor();

// ── Resource cleanup (global, works without a Lab instance) ──
pub struct ResourceList { /* ... */ }
impl ResourceList {
    pub fn global() -> &'static Self;
    pub fn cleanup_by_prefix(&self, prefix: &str);
    pub fn cleanup_registered_prefixes(&self);
    pub fn register_prefix(&self, prefix: &str);
    pub fn set_cleanup_enabled(&self, enabled: bool);
}

// ── Utility functions ──
pub mod util {
    pub fn sanitize_for_env_key(name: &str) -> String;
    pub fn sanitize_for_path_component(name: &str) -> String;
}
```

## Usage Examples

```rust
// ── Basic topology ──
let lab = Lab::new();
let dc = lab.add_router("dc").region("eu").build().await?;
let alice = lab.add_device("alice").uplink(dc.id()).build().await?;
let bob = lab.add_device("bob").uplink(dc.id()).build().await?;

// ── Multi-homed device behind NAT ──
let isp = lab.add_router("isp").region("us").nat(NatMode::Cgnat).build().await?;
let home = lab.add_router("home").upstream(isp.id())
    .nat(NatMode::DestinationIndependent).build().await?;
let dev = lab.add_device("dev")
    .iface("eth0", home.id(), None)
    .iface("eth1", dc.id(), Some(Impair::Mobile))
    .default_via("eth1")   // only needed when not eth0
    .build().await?;

// ── Dynamic ops ──
alice.link_down("eth0").await?;
alice.switch_route("eth1").await?;
home.set_nat_mode(NatMode::DestinationDependent).await?;
lab.impair_link(alice.id(), dc.id(), Some(Impair::Wifi))?;
alice.set_impair("eth0", Some(Impair::Mobile))?;  // device-side only

// ── Switch WiFi network (device moves to different router) ──
let wifi2 = lab.add_router("wifi2").upstream(isp.id())
    .nat(NatMode::DestinationIndependent).build().await?;
dev.switch_uplink("eth0", wifi2.id()).await?;

// ── Switch upstream ISP (router moves to different upstream) ──
let isp2 = lab.add_router("isp2").region("us").nat(NatMode::Cgnat).build().await?;
home.set_uplink(isp2.id()).await?;

// ── Spawn ──
let handle = alice.try_spawn(async |dev: Device| {
    dev.link_down("eth0").await?;
    // ... network code runs inside alice's namespace ...
    dev.link_up("eth0").await?;
    anyhow::Ok(())
});
handle.await??;

// ── Spawn a raw command (sim step executor) ──
let child = alice.spawn_command(cmd)?;
```

## `switch_uplink` / `set_uplink` Design

**`device.switch_uplink(ifname, to_router)`** moves a device interface to a different
router's downstream network, simulating a WiFi network switch:

1. Lock → read current interface state (old veth idx, old IP, old switch, old impair) → unlock
2. Delete the old veth pair (root ns side)
3. Lock → allocate new IP from target router's pool, get new switch/bridge info → unlock
4. Create new veth pair, wire to target router's downstream bridge, assign new IP
5. Re-add default route if this is the default_via interface, re-apply impairment
6. Lock → update internal `DeviceIface` record (new uplink, new IP) → unlock

The interface name stays the same. The IP changes (comes from new router's pool).

**`router.set_uplink(upstream)`** moves a router to a different upstream, simulating
an ISP switch:

1. Lock → read current WAN state (old veth idx, old upstream switch, old WAN IP) → unlock
2. Delete the old WAN veth pair
3. Lock → allocate new IP from new upstream's pool → unlock
4. Create new WAN veth, wire to new upstream's downstream bridge, assign new WAN IP
5. Re-apply NAT rules with new WAN IP, re-add default route via new upstream gateway
6. Lock → update internal router record (new uplink, new upstream_ip) → unlock

## `impair_link` Design

`impair_link(from, to, impair)` applies tc netem in both directions between two
connected nodes:

- **Device ↔ Router**: applies on the device's interface (upload from device), AND
  on the router's downstream bridge scoped to the device's IP via tc filters (download).
- **Router ↔ Router**: applies on the downstream router's WAN interface (upload) and
  the upstream router's downstream bridge filtered to the downstream router's IP.
- Returns `Err` if the two nodes are not directly connected.

Replaces `set_router_impair`. `Device::set_impair` is kept as a convenience for
device-side-only impairment (upload direction only, single interface).

## `test_utils` Module

All background helpers return `tokio::task::JoinHandle`. Callers use `.abort()` to
stop them — no custom `TaskHandle` type needed. The current `TaskHandle` (mpsc-based
stop channel for sync threads) is eliminated; reflectors and echo servers become async
tokio tasks spawned via `Device::spawn` / `Router::spawn`, cancellable at any `.await`.

```rust
pub mod test_utils {
    // UDP reflectors — return JoinHandle, abort() to stop
    pub fn spawn_reflector(node: &Device, bind: SocketAddr) -> JoinHandle<Result<()>>;
    pub fn spawn_reflector_on(node: &Router, bind: SocketAddr) -> JoinHandle<Result<()>>;
    pub fn spawn_reflector_on_ix(lab: &Lab, bind: SocketAddr) -> JoinHandle<Result<()>>;

    // UDP connectivity check (ephemeral port — just verifies reachability)
    pub fn udp_roundtrip(node: &Device, reflector: SocketAddr) -> Result<ObservedAddr>;
    // UDP round-trip time measurement
    pub fn udp_rtt(node: &Device, reflector: SocketAddr) -> Result<Duration>;
    // NAT mapping probe (deterministic port per device — for comparing mappings across calls)
    pub fn probe_nat_mapping(dev: &Device, reflector: SocketAddr) -> Result<ObservedAddr>;

    // TCP helpers (promoted from test-private)
    pub fn spawn_tcp_echo(node: &Device, bind: SocketAddr) -> JoinHandle<Result<()>>;
    pub fn tcp_roundtrip(node: &Device, addr: SocketAddr) -> Result<Duration>;
}
```

## What Gets Removed / Internalized

### From current `Lab` public API:
| Current | New API |
|---|---|
| `build()` | removed — instant in builders |
| `root_namespace_name()` | removed |
| `run_on(name, cmd)` | `Device::spawn` / `Device::spawn_command` |
| `run_in_namespace(ns, f)` | `pub(crate)` |
| `run_in_namespace_thread(ns, f)` | `pub(crate)` |
| `spawn_on(name, cmd)` | `Device::spawn` |
| `spawn_unmanaged_on(device, cmd)` | `Device::spawn_command(cmd)` |
| `device_ns_name(device)` | removed |
| `router_ns_name(router)` | removed |
| `node_ns(id)` | removed |
| `router_downlink_bridge(id)` | internal — `impair_link` handles it |
| `set_router_impair(router, iface, impair)` | `Lab::impair_link(from, to, impair)` |
| `router_downlink_gw(id)` | `Router::downstream_gw()` |
| `router_uplink_ip(id)` | `Router::uplink_ip()` |
| `device_ip(id)` | `Device::ip()` |
| `router_id(name)` | `router_by_name(name).map(\|r\| r.id())` |
| `device_id(name)` | `device_by_name(name).map(\|d\| d.id())` |
| `set_nat_mode(router, mode)` | `Router::set_nat_mode(mode)` |
| `rebind_nats(router)` | `Router::rebind_nats()` |
| `link_down(device, ifname)` | `Device::link_down(ifname)` |
| `link_up(device, ifname)` | `Device::link_up(ifname)` |
| `switch_route(device, to)` | `Device::switch_route(to)` |
| `set_impair(device, iface, impair)` | `Device::set_impair(ifname, impair)` |
| `add_region_latency(from, to, ms)` | `Lab::set_region_latency(from, to, ms)` |
| `spawn_reflector(ns, bind)` | `test_utils::spawn_reflector(&Device, bind)` |
| `spawn_reflector_on_ix(bind)` | `test_utils::spawn_reflector_on_ix(&Lab, bind)` |
| `probe_udp_mapping(device, refl)` | `test_utils::probe_nat_mapping(&Device, refl)` |
| `probe_in_ns_from(ns, ...)` | `test_utils` internal |
| `prefix()` | **keep** |
| `env_vars()` | **keep** (drops `NETSIM_NS_*`) |
| `cleanup()` | **keep** |
| `cleanup_everything()` | **keep** |

### Kept public:
- `check_caps()`, `init_userns()`, `init_userns_for_ctor()` at crate root
- `ResourceList::global()` + cleanup methods
- `util::sanitize_for_*` functions
- `config` module with pub fields

### Internalized:
- `NetworkCore` → `pub(crate)`
- All `core::` free functions (namespace, netlink, nft, sysctl, impair) → `pub(crate)`
- `resources()` free function → replaced by `ResourceList::global()`

## Addressing netsim Crate Needs

| netsim usage | Migration |
|---|---|
| `spawn_unmanaged_on(dev, cmd)` | `lab.device_by_name(dev)?.spawn_command(cmd)` |
| `set_impair(dev, iface, impair)` | `lab.device_by_name(dev)?.set_impair(ifname, impair)` |
| `link_down(dev, iface)` | `lab.device_by_name(dev)?.link_down(iface).await` |
| `link_up(dev, iface)` | `lab.device_by_name(dev)?.link_up(iface).await` |
| `switch_route(dev, to)` | `lab.device_by_name(dev)?.switch_route(to).await` |
| `router_ns_name` / `device_ns_name` | `Router::spawn_command` / `Device::spawn_command` |
| `lab.build()` | removed; `load()` calls builders internally |
| `core::resources()` | `ResourceList::global()` |
| `core::spawn_command_in_namespace` | `Device::spawn_command` / `Router::spawn_command` |
| `env_vars()` | keep, drop `NETSIM_NS_*` entries |
| `prefix()` | keep |

---

## Implementation Plan

### Phase 1: Rename internal types to avoid conflicts

The new public `Device` and `Router` handle types will conflict with the existing
`core::Device` and `core::Router` data structs. Rename the internal ones first.

- [x] Rename `core::Device` → `core::DeviceData` (or `DeviceRecord`)
- [x] Rename `core::Router` → `core::RouterData` (or `RouterRecord`)
- [x] Rename `core::DeviceIface` → `core::DeviceIfaceData`
- [x] Update all internal references in `core.rs`, `lab.rs`, `netns.rs`
- [x] Run tests to verify rename is complete

### Phase 2: `Arc<Mutex<LabInner>>` restructuring

- [x] Create `LabInner` struct, move all `Lab` fields into it
- [x] Wrap `Lab` around `Arc<Mutex<LabInner>>`, derive `Clone`
- [x] Change all `Lab` methods from `&mut self` to `&self`, use `self.inner.lock()` internally
- [x] Change all `Lab` methods that returned `&str`/`&T` to return owned types
- [x] Audit every async method: ensure lock is never held across `.await`
- [x] Run existing tests — everything should pass with no API changes yet

### Phase 3: `Device` and `Router` handles + lookup methods

- [x] Create public `Device` handle struct: `{ id: NodeId, lab: Arc<Mutex<LabInner>> }`, implement `Clone`
- [x] Create public `Router` handle struct: same pattern, implement `Clone`
- [x] Add accessor methods on `Device`: `id()`, `name()`, `ip()`, `iface()`, `default_iface()`, `interfaces()`
- [x] Add accessor methods on `Router`: `id()`, `name()`, `region()`, `nat_mode()`, `uplink_ip()`, `downstream_cidr()`, `downstream_gw()`
- [x] Create public `DeviceIface` as owned snapshot struct with `name()`, `ip()`, `impair()` accessors
- [x] Add lookup methods on `Lab`: `router(id)`, `device(id)`, `router_by_name(name)`, `device_by_name(name)` returning `Option<Router>` / `Option<Device>`
- [x] Add collection methods: `lab.routers()`, `lab.devices()` returning `Vec`
- [x] Keep old Lab methods temporarily for incremental migration

### Phase 4: Move dynamic ops to `Device` / `Router`

- [x] Implement `Device::link_down(&self, ifname: &str)` with lock/read/unlock/await pattern
- [x] Implement `Device::link_up(&self, ifname: &str)`
- [x] Implement `Device::switch_route(&self, to_iface: &str)`
- [x] Implement `Device::set_impair(&self, ifname: &str, impair: Option<Impair>)`
- [x] Implement `Router::set_nat_mode(&self, mode: NatMode)`
- [x] Implement `Router::rebind_nats(&self)`
- [x] Update all test call sites to use new methods
- [x] Remove old Lab methods (except `set_router_impair` and `router_downlink_bridge` — deferred to Phase 7/10)

### Phase 5: Builders return `Device` / `Router`

- [x] Change `RouterBuilder::build()` to return `Result<Router>` instead of `Result<NodeId>`
- [x] Change `DeviceBuilder::build()` to return `Result<Device>` instead of `Result<NodeId>`
- [x] Remove lifetime parameter from builders — they hold `Arc<Mutex<LabInner>>` (done in Phase 2)
- [x] Add `DeviceBuilder::uplink(router_id)` shorthand (auto-names eth0, eth1, ...)
- [x] Update all test call sites: `let dc = lab.add_router("dc").build()?;` then use `dc.id()` where NodeId is needed

### Phase 6: `spawn` / `try_spawn` / `spawn_command`

- [x] Implement `Device::spawn(f: FnOnce(Device) -> Fut)` — clones handle, enters ns, runs closure
- [ ] Implement `Device::try_spawn` — deferred (trivial wrapper, add when needed)
- [x] Implement `Device::spawn_command(cmd: Command) -> Result<Child>` — enters ns, spawns process
- [x] Implement same methods on `Router` (spawn, spawn_command)
- [ ] Migrate test helpers that use `run_closure_in_namespace` / `spawn_task_in_netns` to `Device::spawn` — deferred to Phase 9
- [x] Migrate netsim `steps.rs` from `lab.spawn_unmanaged_on` to `Device::spawn_command`
- [ ] Migrate netsim `main.rs` inspect keeper from `spawn_command_in_namespace` to `Router/Device::spawn_command` — deferred to Phase 10
- [x] Remove old Lab methods: `run_on`, `spawn_on`, `spawn_unmanaged_on`

### Phase 7: `impair_link`, `switch_uplink`, `set_uplink`

- [ ] Implement `Lab::impair_link(from, to, impair)` — resolve topology, apply tc netem bidirectionally
- [ ] Implement `Device::switch_uplink(ifname, to_router)` — delete old veth, alloc new IP, wire new link, re-apply impair
- [ ] Implement `Router::set_uplink(upstream)` — delete old WAN veth, alloc new IP, wire new link, re-apply NAT
- [ ] Update tests that used `set_router_impair` + `router_downlink_bridge` to use `impair_link`
- [ ] Write new tests for `switch_uplink` and `set_uplink`

### Phase 8: Remove `build()`, make construction instant

- [ ] Refactor `NetworkCore::build()` into per-node setup methods:
  - [ ] `setup_root_ns()` — IX bridge, root namespace (called once from `Lab::new()` or lazily)
  - [ ] `setup_router(id)` — create ns, veth, addressing, NAT, sysctl, downstream bridge
  - [ ] `setup_device(id)` — create ns, wire all interfaces
- [ ] `RouterBuilder::build()` calls `setup_root_ns()` (idempotent) then `setup_router()`
- [ ] `DeviceBuilder::build()` calls `setup_device()`
- [ ] Handle region latency: `set_region_latency` stores rule + immediately (re)applies tc filters on affected IX interfaces
- [ ] Refactor `Lab::load()` to use instant construction (sequential `add_router().build().await` in topological order, then `add_device().build().await`)
- [ ] Remove `Lab::build()` and `NetworkCore::build()`
- [ ] Run full test suite

### Phase 9: `test_utils` and `ResourceList` cleanup

- [ ] Rewrite `test_utils` reflectors/echo servers as async tasks using `Device::spawn`, returning `JoinHandle`
- [ ] Remove `TaskHandle` type entirely — callers use `JoinHandle::abort()`
- [ ] Rewrite probe helpers (`udp_roundtrip`, `udp_rtt`, `probe_nat_mapping`) to take `&Device`
- [ ] Promote `spawn_tcp_echo` and `tcp_roundtrip` from test-private into `test_utils`
- [ ] Rename `resources()` → `ResourceList::global()`
- [ ] Migrate all tests from raw ns names to `Device`/`Router` handles
- [ ] Remove `smoke_debug_netns_exit_trace` test (accesses `lab.core` directly)
- [ ] Migrate netsim crate from `core::resources()` to `ResourceList::global()`

### Phase 10: Internalize and final cleanup

- [ ] Make `NetworkCore` and all its methods `pub(crate)`
- [ ] Make all `core::` free functions `pub(crate)` (namespace, netlink, nft, sysctl, impair, NAT)
- [ ] Remove `Lab::root_namespace_name()`, `Lab::node_ns()`, `Lab::device_ns_name()`, `Lab::router_ns_name()`
- [ ] Remove `Lab::router_downlink_bridge()`
- [ ] Remove `Lab::run_on()`, `Lab::run_in_namespace()`, `Lab::run_in_namespace_thread()`
- [ ] Remove `Lab::spawn_on()`
- [ ] Drop `NETSIM_NS_*` from `Lab::env_vars()`
- [ ] Rename `Lab::add_region_latency` → `Lab::set_region_latency` (if not already done)
- [ ] Update netsim crate imports and call sites for all changes
- [ ] Final audit: `cargo doc` to verify only intended items are public
- [ ] Run full test suite + netsim integration tests
