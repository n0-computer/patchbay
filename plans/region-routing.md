# Region Routing

## Context

Replace the string-based `set_region_latency` API with first-class Region objects backed by real router namespaces, inter-region veths with tc netem, and break/restore for fallback routing.

### Design decisions from conversation

1. **Per-region router namespaces**: Each region is a router namespace with a bridge. User routers in a region connect to the region's bridge as sub-routers. Inter-region links are veths between region routers with tc netem for latency/jitter/loss.

2. **Region router = Router internally**: A region router is just a `RouterData` in `NetworkCore::routers` with `Nat::None`, `DownstreamPool::Public`, and a `region_` name prefix. Reuses all existing router setup code. `Region` public handle wraps a `NodeId` (the region router's ID) + lab Arc.

3. **198.18.0.0/15 address space, /20 per region**: Each region gets a contiguous /20 (16 × /24). First /24 is the region bridge subnet, remaining 15 are public downstream pool. One aggregate /20 route per peer — no per-router route propagation.

4. **203.0.113.0/24 for inter-region point-to-point veths**: /30 per link, up to 85 links. Leaves 172.16.0.0/12 free for future use.

5. **Zero cost without regions**: If `add_region` is never called, regionless routers sit directly on the IX bridge at 198.18.0.0/24. No extra namespaces, threads, or routes.

6. **Router names starting with `region_` are reserved**: `add_router("region_...")` returns an error. Region routers use names like `region_us`, `region_eu`.

7. **Break/restore = route replacement**: `break_region_link(eu, asia)` replaces the direct route on each side with a route through an intermediate region. `restore` puts back the direct route. Simple `ip route replace` commands.

---

## Address Space

```
198.18.0.0/15 total (512 × /24), /20 per region, max 16+1 regions

Region 0 "global" (regionless):  198.18.0.0/20
  198.18.0.0/24    IX bridge, regionless routers (.10, .11, ...)
  198.18.1.0/24  }
  ...            } 15 × /24 public downstream pool
  198.18.15.0/24 }

Region 1 "us":  198.18.16.0/20
  198.18.16.0/24   region-us bridge, us routers (.10, .11, ...)
  198.18.17.0/24 }
  ...            } 15 × /24 public downstream
  198.18.31.0/24 }

Region 2 "eu":  198.18.32.0/20   (198.18.32–47)
Region 3 "asia": 198.18.48.0/20  (198.18.48–63)
...
Region 16:       198.19.0.0/20   (198.19.0–15)

Inter-region p2p veths: 203.0.113.0/24 (/30 each, up to 85 links)
Private downstream:     10.0.0.0/16 (unchanged)
```

## Capacity

| Resource | Limit | Bottleneck |
|----------|-------|------------|
| Regions | 16 (+ 1 global) | /20 blocks in 198.18.0.0/15 |
| Routers per region | ~245 | IPs .10–.254 in region's first /24 |
| Nat::None routers per region | 15 | /24 downstream blocks per /20 |
| Devices per Nat::None router | 253 | .2–.254 in the /24 downstream |
| Devices per NATted router | 253 | .2–.254 in the 10.0.x.0/24 |
| Private subnets (all regions) | ~65k | 10.0.0.0/16 pool |
| Inter-region links | 85 | /30 blocks in 203.0.113.0/24 |
| Regionless routers | ~245 | IPs .10–.254 in 198.18.0.0/24 |
| Regionless Nat::None downstream | 15 | /24 blocks in global /20 |

---

## Topology

```
Root NS
├── IX bridge (198.18.0.0/24, gw .1)
│   │
│   ├── dc (198.18.0.10) ← regionless, directly on IX
│   │   └── br-lan 198.18.1.0/24
│   │       └── server (198.18.1.2, public)
│   │
│   ├── region_us router (198.18.0.20 on IX, 198.18.16.1 on br-us)
│   │   ├── br-us (198.18.16.0/24)
│   │   │   ├── relay-us (198.18.16.10, Nat::None)
│   │   │   │   └── br-lan 198.18.17.0/24
│   │   │   │       └── relay-srv (198.18.17.2, public)
│   │   │   └── home-us (198.18.16.11, Nat::Home)
│   │   │       └── br-lan 10.0.1.0/24
│   │   │           └── laptop (10.0.1.2, private)
│   │   │
│   │   ├── veth ←→ region_eu   203.0.113.0/30  netem 45ms
│   │   └── veth ←→ region_asia 203.0.113.4/30  netem 80ms
│   │
│   ├── region_eu router (198.18.0.21 on IX, 198.18.32.1 on br-eu)
│   │   ├── br-eu (198.18.32.0/24)
│   │   │   └── home-eu (198.18.32.10, Nat::Home)
│   │   │       └── br-lan 10.0.2.0/24
│   │   │           └── phone (10.0.2.2, private)
│   │   │
│   │   ├── veth ←→ region_us   203.0.113.0/30  netem 45ms
│   │   └── veth ←→ region_asia 203.0.113.8/30  netem 140ms
│   │
│   └── region_asia router (198.18.0.22 on IX, 198.18.48.1 on br-asia)
│       ├── br-asia (198.18.48.0/24)
│       │   └── isp-asia (198.18.48.10, Nat::Cgnat)
│       │       └── br-lan 10.0.3.0/24
│       │           └── phone (10.0.3.2, private)
│       │
│       ├── veth ←→ region_us   203.0.113.4/30  netem 80ms
│       └── veth ←→ region_eu   203.0.113.8/30  netem 140ms
```

---

## Routing tables

**Region_us router** (198.18.0.20 on IX, 198.18.16.1 on br-us):
```
198.18.16.0/24  dev br-us          # region bridge subnet (on-link)
198.18.17.0/24  via 198.18.16.10   # return route: relay-us public downstream
198.18.32.0/20  via 203.0.113.2    # eu (via us↔eu veth)
198.18.48.0/20  via 203.0.113.6    # asia (via us↔asia veth)
default         via 198.18.0.1     # IX gateway (regionless + fallback)
```

Region router bridge is /24 only, NOT /20 on-link. Public downstream return routes
(e.g. `198.18.17.0/24 via 198.18.16.10`) are added in the region router's NS when a
Nat::None sub-router is built — same `return_route` mechanism already used for root-NS
return routes today, just targeting the region router NS instead. NATted routers don't
need return routes (private 10.0.x.0/24 downstream).

**Why not put relay-us directly at 198.18.17.1?** Routers need two interfaces (WAN on
region bridge + LAN on downstream bridge). A single-subnet approach would eliminate the
WAN connection to the region bridge, breaking forwarding. The return route is one extra
`ip route add` per Nat::None sub-router — zero new mechanism, reuses existing code path.

**Root NS** (IX gateway 198.18.0.1):
```
198.18.0.0/24   dev ix-br          # IX bridge (on-link)
198.18.16.0/20  via 198.18.0.20    # us (aggregate, added when region created)
198.18.32.0/20  via 198.18.0.21    # eu
198.18.48.0/20  via 198.18.0.22    # asia
```

Root NS uses /20 aggregates — one route per region, never updated when routers are added/removed.

---

## Break / Restore

**`break_region_link(eu, asia)`** — reroute through us:
```
# On region_eu: replace direct asia route with via-us
ip route replace 198.18.48.0/20 via 203.0.113.1   # asia via us (us↔eu veth, us side)

# On region_asia: replace direct eu route with via-us
ip route replace 198.18.32.0/20 via 203.0.113.5   # eu via us (us↔asia veth, us side)
```

Traffic eu→asia now goes: region_eu → region_us → region_asia.
Latency: netem on us↔eu veth (45ms) + netem on us↔asia veth (80ms) = 125ms one-way.

**`restore_region_link(eu, asia)`** — put back direct routes:
```
ip route replace 198.18.48.0/20 via 203.0.113.10  # direct eu↔asia veth
ip route replace 198.18.32.0/20 via 203.0.113.9   # direct asia↔eu veth
```

Implementation: `run_closure_in` on the region router ns with `Command::new("ip")`. ~10 lines.

---

## Internal: Region router is a Router

A region router is created via the existing `add_router` + `setup_router_async` code path:

```rust
// Pseudocode for add_region("us"):
let region_router_id = inner.add_router(
    "region_us",
    Nat::None,
    DownstreamPool::Public,  // region's /20 downstream pool
    None,                     // no region tag (it IS the region)
    IpSupport::V4Only,       // v6 future work
    NatV6Mode::None,
);
// setup_router_async creates ns, bridge, IX veth — all reused
```

The `Region` public handle:
```rust
pub struct Region {
    name: String,
    idx: u8,                           // region index (1–16)
    router_id: NodeId,                 // the region router's ID
    lab: Arc<Mutex<NetworkCore>>,
}
```

User routers in a region use `.upstream(region.router_id)` — they become sub-routers of the region router, connecting to the region's bridge. This is the existing sub-router code path.

Router name validation: `add_router` rejects names starting with `region_`.

---

## Public API

```rust
// Create regions
let us = lab.add_region("us").await?;
let eu = lab.add_region("eu").await?;
let asia = lab.add_region("asia").await?;

// Link them
lab.link_regions(&us, &eu, RegionLink::good(45))?;
lab.link_regions(&us, &asia, RegionLink::good(80))?;
lab.link_regions(&eu, &asia, RegionLink::good(140))?;

// Or use preset
let regions = lab.add_default_regions().await?;  // { us, eu, asia } pre-linked

// Routers in regions
let dc = lab.add_router("dc-us").region(&us).build().await?;
let home = lab.add_router("home-eu").region(&eu).nat(Nat::Home).build().await?;

// Break/restore
lab.break_region_link(&eu, &asia)?;    // reroutes through us
lab.restore_region_link(&eu, &asia)?;  // restores direct path

// Regionless (no change from today)
let dc = lab.add_router("dc").build().await?;  // directly on IX
```

---

## Implementation Steps

### Step 1: Address space migration
- [ ] Change `CoreConfig` in core.rs: `ix_cidr` → `198.18.0.0/24`, `public_cidr` → `198.18.1.0/24`, `ix_gw` → `198.18.0.1`
- [ ] Update `alloc_ix_ip_low` to start from .10 in 198.18.0.x (already does, just different base)
- [ ] Update `alloc_public_cidr` to use 198.18.1.0/24 base (subnet 1, 2, ... up to 15 for global region)
- [ ] Add `next_region_idx: u8` counter to NetworkCore (starts at 1)
- [ ] Add `regions: HashMap<String, RegionInfo>` to NetworkCore where `RegionInfo { idx: u8, router_id: NodeId }`
- [ ] Verify all existing tests still pass with new IP range

### Step 2: Region router creation
- [ ] Add `Lab::add_region(&self, name: &str) -> Result<Region>` (async, calls build internally)
- [ ] Validate region name (not empty, not duplicate, idx ≤ 16)
- [ ] Add router name validation: `add_router` rejects names starting with `region_`
- [ ] Region router uses `add_router("region_{name}", Nat::None, DownstreamPool::Public, ...)`
- [ ] Override the region router's downstream allocation to use the region's /20 pool:
  - IX subnet: `198.18.{idx*16}.0/24`
  - Public downstream base: `198.18.{idx*16 + 1}.0/24` (15 /24s available)
- [ ] Region router goes through `setup_router_async` like any IX router
- [ ] Add /20 route in root NS: `198.18.{idx*16}.0/20 via <region_router_ix_ip>`
- [ ] `Region` handle: `{ name, idx, router_id, lab }` with `AsRef<str>` impl (returns name)

### Step 3: Router region assignment
- [ ] `RouterBuilder::region(impl AsRef<str>)` — looks up region by name, sets router's uplink to region router's downstream switch (sub-router path)
- [ ] Per-region IP allocation: router gets IP from region's first /24
- [ ] Per-region public downstream: `Nat::None` routers get /24 from region's downstream pool
- [ ] Return route in region router's NS: `198.18.{n}.0/24 via <router_upstream_ip>` (same pattern as root-NS return routes, placed in region router NS instead)
- [ ] Private downstream unchanged (10.0.0.0/16)

### Step 4: Inter-region links
- [ ] Add `RegionLink { latency_ms, jitter_ms, loss_pct, rate_mbit }` with `good(ms)` and `bad(ms)` constructors
- [ ] Add `Lab::link_regions(&self, a: &Region, b: &Region, link: RegionLink) -> Result<()>`
- [ ] Allocate /30 from 203.0.113.0/24 (`next_interregion_subnet` counter on NetworkCore)
- [ ] Create veth pair between region router namespaces (reuse existing veth creation in root ns + move to ns pattern)
- [ ] Assign /30 IPs to each end
- [ ] Apply tc netem on both veth ends (using existing `qdisc::apply_impair` or new helper)
- [ ] Add routes on each region router: `198.18.{peer_base}.0/20 via <peer_veth_ip>`
- [ ] Store link data: `region_links: HashMap<(String, String), RegionLinkInfo>` with veth names, /30 IPs, RegionLink params, broken flag

### Step 5: Break / Restore
- [ ] `Lab::break_region_link(&self, a: &Region, b: &Region) -> Result<()>`
  - Find intermediate region `m` with non-broken links to both `a` and `b`
  - On region_a router: `ip route replace <b_cidr>/20 via <m_veth_ip_on_a_side>`
  - On region_b router: `ip route replace <a_cidr>/20 via <m_veth_ip_on_b_side>`
  - Mark link as broken
- [ ] `Lab::restore_region_link(&self, a: &Region, b: &Region) -> Result<()>`
  - Restore direct routes using the a↔b veth IPs
  - Mark link as not broken

### Step 6: Default regions preset
- [ ] `Lab::add_default_regions(&self) -> Result<DefaultRegions>` (async)
- [ ] Creates us, eu, asia regions
- [ ] Links: us↔eu 45ms, us↔asia 80ms, eu↔asia 140ms
- [ ] `DefaultRegions { us, eu, asia }`

### Step 7: Deprecate old API
- [ ] Remove old `set_region_latency` and `region_latencies: Vec<(String, String, u32)>` from NetworkCore
- [ ] Remove old `apply_region_latencies` method
- [ ] Migrate existing tests to new API

### Step 8: Extend qdisc for LinkLimits in region filters
- [ ] `apply_region_latency_dual` signature: `&[(IpNet, u32)]` → `&[(IpNet, LinkLimits)]`
- [ ] `add_netem_class` accepts `LinkLimits` (jitter, loss, etc.)
- [ ] `add_htb_class` accepts optional rate
- [ ] Note: this may no longer be needed if region latency is purely on inter-region veths. Evaluate during implementation — the old per-destination-CIDR tc filter approach may be fully replaced by per-veth netem.

### Step 9: Re-exports and public API
- [ ] Add to `lib.rs`: `Region`, `RegionLink`, `DefaultRegions`
- [ ] Update README.md API examples

### Step 10: Tests
- [ ] `region_basic_latency` — two regions linked at 50ms, verify RTT ≥ 90ms
- [ ] `region_default_regions` — add_default_regions(), verify us↔eu RTT ≥ 80ms
- [ ] `region_break_restore` — 3 regions, break eu↔asia, verify RTT changes to ~250ms (via us), restore, verify RTT back to ~280ms
- [ ] `region_no_cost_without_regions` — lab without regions = same as today
- [ ] `region_regionless_to_region_connectivity` — regionless router can reach devices in a region
- [ ] `region_mixed_nat` — region with Nat::Home router, verify NAT + region latency work together
- [ ] Migrate existing latency tests (`latency_directional_between_regions`, `latency_inter_region_dc_to_dc`, etc.)

---

## Files

| File | Changes |
|------|---------|
| `netsim-core/src/lab.rs` | Region/RegionLink/DefaultRegions types, add_region, link_regions, break/restore, RouterBuilder::region, router name validation |
| `netsim-core/src/core.rs` | Address space (198.18.0.0/15), RegionInfo, per-region allocators, region_links, remove old region_latencies |
| `netsim-core/src/qdisc.rs` | Maybe extend apply_region_latency_dual for LinkLimits (or remove if fully replaced by veth netem) |
| `netsim-core/src/lib.rs` | Re-exports |
| `netsim-core/src/tests.rs` | New + migrated tests |

## Verification

```bash
cargo check -p netsim-core --tests && cargo check --workspace
cargo nextest run -p netsim-core  # all tests, verify no regressions
cargo nextest run -p netsim-core -E 'test(region)'  # new region tests
```
