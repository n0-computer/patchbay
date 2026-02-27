# Plan: Real-World Network Conditions

Goal: Make netsim-core simulate the NAT, routing, and impairment conditions that
real applications encounter - while keeping the builder API concise for common
cases.

---

## Part 1: NAT Realism

### Current state

| Mode | nftables rules | RFC 4787 mapping | RFC 4787 filtering |
|------|---------------|-----------------|-------------------|
| `None` | (none) | N/A | N/A |
| `Cgnat` | `masquerade` on IX iface | EIM (port-preserving) | APDF (conntrack) |
| `DestinationIndependent` | fullcone map + snat | EIM | EIF (full cone) |
| `DestinationDependent` | `masquerade random` | APDM | APDF |

Problems:
1. **Missing the most common home-router behavior** - EIM + APDF (port-restricted
   cone). Linux `snat` without `random` does this. Most consumer routers
   (FritzBox, Unifi, TP-Link, ASUS) produce this. Nothing in between our
   FullCone and Symmetric.
2. **No filtering dimension** - Mapping and filtering conflated (RFC 4787 S4 vs S5).
3. **No hairpinning control** - Fullcone implicitly hairpins, others don't.
4. **CGNAT is just masquerade** - Real CGNAT (RFC 6888) is EIM + EIF with
   port-block allocation.
5. **No conntrack timeout control** - Linux default UDP 30s/120s vs RFC 5min.
6. **Variant names don't match RFC 4787** - `DestinationIndependent` should be
   `EndpointIndependent`.

### New NAT model

#### Building blocks (advanced API)

```rust
/// NAT mapping and filtering behavior per RFC 4787.
///
/// Abbreviations used in variant docs:
/// - EIM: Endpoint-Independent Mapping (RFC 4787 S4.1)
/// - EDM: Endpoint-Dependent Mapping (symmetric)
/// - EIF: Endpoint-Independent Filtering (RFC 4787 S5, full cone)
/// - APDF: Address-and-Port-Dependent Filtering (port-restricted cone)

/// NAT mapping behavior per RFC 4787 S4.1.
pub enum NatMapping {
    /// Same external port for all destinations (EIM).
    /// Port-preserving. nftables: `snat to <ip>`.
    EndpointIndependent,
    /// Different external port per destination IP+port (symmetric).
    /// Port randomized. nftables: `masquerade random,fully-random`.
    EndpointDependent,
}

/// NAT filtering behavior per RFC 4787 S5.
pub enum NatFiltering {
    /// Any external host can send to the mapped port (full cone).
    /// nftables: fullcone DNAT map in prerouting.
    EndpointIndependent,
    /// Only the exact (IP, port) the internal endpoint contacted.
    /// nftables: conntrack-only (no prerouting DNAT).
    AddressAndPortDependent,
}
```

#### Preset profiles (primary API)

```rust
/// NAT behavior preset for common real-world equipment.
///
/// Abbreviations used in variant docs:
/// - EIM: Endpoint-Independent Mapping (same external port for all destinations)
/// - EDM: Endpoint-Dependent Mapping (different port per destination, "symmetric")
/// - EIF: Endpoint-Independent Filtering (any host can reach the mapped port)
/// - APDF: Address-and-Port-Dependent Filtering (only contacted host:port can reply)
pub enum Nat {
    /// No NAT - addresses are publicly routable.
    ///
    /// Use for datacenter racks, cloud VMs with elastic IPs,
    /// or any host that needs a stable public address.
    None,

    /// Home router - the most common consumer NAT.
    ///
    /// EIM + APDF. Port-preserving. No hairpin. UDP timeout 300s.
    /// This is what Linux `snat to <ip>` produces.
    ///
    /// Observed on: FritzBox, Unifi (default), TP-Link Archer, ASUS RT-AX,
    /// OpenWRT default masquerade.
    ///
    /// Hole-punching works with simultaneous open (both sides must send).
    /// This is the RFC 4787 REQ-1 compliant "port-restricted cone" NAT.
    Home,

    /// Corporate firewall - symmetric NAT.
    ///
    /// EDM + APDF. Random ports. No hairpin. UDP timeout 120s.
    /// Produces a different external port per (dst_ip, dst_port) 4-tuple.
    ///
    /// Observed on: Cisco ASA (PAT), Palo Alto NGFW (DIPP), Fortinet
    /// FortiGate, Juniper SRX. AWS/Azure/GCP NAT gateways behave identically.
    ///
    /// Hole-punching is impossible without relay (TURN/DERP).
    Corporate,

    /// Carrier-grade NAT per RFC 6888.
    ///
    /// EIM + EIF. Port-preserving. No hairpin. UDP timeout 300s.
    /// Applied on the ISP/IX-facing interface (stacks with home NAT).
    ///
    /// Observed on: A10 Thunder CGN, Cisco ASR CGNAT, Juniper MX MS-MPC.
    /// Mobile carriers (T-Mobile, Vodafone, O2) use this for LTE/5G subscribers.
    /// RFC 6888 mandates EIM to preserve P2P traversal at the ISP layer.
    Cgnat,

    /// Cloud NAT gateway - symmetric NAT with randomized ports.
    ///
    /// EDM + APDF. Random ports. No hairpin. UDP timeout 350s.
    ///
    /// Observed on: AWS NAT Gateway, Azure NAT Gateway, GCP Cloud NAT
    /// (default dynamic port allocation mode).
    ///
    /// Functionally identical to Corporate but with longer timeouts
    /// matching documented cloud provider behavior.
    CloudNat,

    /// Full cone - most permissive NAT for testing.
    ///
    /// EIM + EIF. Port-preserving. Hairpin on. UDP timeout 300s.
    /// Any external host can send to the mapped port after first outbound packet.
    ///
    /// Observed on: older FritzBox firmware, some CGNAT with full-cone policy.
    /// Hole-punching always succeeds.
    FullCone,
}
```

#### Custom NAT builder

```rust
lab.add_router("fw")
    .nat_custom(|n| n
        .mapping(NatMapping::EndpointDependent)
        .filtering(NatFiltering::AddressAndPortDependent)
        .hairpin(false)
        .udp_timeout(Duration::from_secs(120))
    )
    .build().await?;
```

#### Builder examples

```rust
// Home router - replaces DestinationIndependent in most test sites
let home = lab.add_router("home").nat(Nat::Home).build().await?;

// Mobile user behind CGNAT + home router (double NAT)
let isp = lab.add_router("isp").nat(Nat::Cgnat).build().await?;
let home = lab.add_router("home").upstream(isp.id()).nat(Nat::Home).build().await?;

// Corporate firewall
let corp = lab.add_router("corp").nat(Nat::Corporate).build().await?;

// DC with public IPs - same as today
let dc = lab.add_router("dc").build().await?;
```

#### Migration from current `NatMode`

| Old | New | Notes |
|-----|-----|-------|
| `NatMode::None` | `Nat::None` | Unchanged |
| `NatMode::Cgnat` | `Nat::Cgnat` | Now EIM+EIF per RFC 6888 |
| `NatMode::DestinationIndependent` | `Nat::Home` (most sites) / `Nat::FullCone` (if test needs EIF) | See migration note below |
| `NatMode::DestinationDependent` | `Nat::Corporate` | Renamed |

**Migration**: Move most `DestinationIndependent` call sites to `Nat::Home`
(EIM+APDF). Tests that rely on unsolicited inbound UDP (full-cone behavior)
move to `Nat::FullCone`. Most hole-punching tests should pass with `Home`
because they use simultaneous open.

#### nftables rules per profile

**EIM + APDF (Home):**
```nftables
table ip nat {
    chain postrouting {
        type nat hook postrouting priority srcnat; policy accept;
        oif "<wan>" snat to <wan_ip>
    }
}
```

**EIM + EIF (FullCone, Cgnat):**
Current fullcone map approach. For CGNAT, apply on IX iface.

**EDM + APDF (Corporate, CloudNat):**
```nftables
table ip nat {
    chain postrouting {
        type nat hook postrouting priority srcnat; policy accept;
        oif "<wan>" masquerade random,fully-random
    }
}
```

**Conntrack timeouts** (per profile, via sysctl in router ns):
| Profile | `udp_timeout` | `udp_timeout_stream` | `tcp_timeout_established` |
|---------|--------------|---------------------|--------------------------|
| Home | 30 | 300 | 7200 |
| Corporate | 30 | 120 | 3600 |
| Cgnat | 30 | 300 | 7200 |
| CloudNat | 30 | 350 | 3600 |
| FullCone | 30 | 300 | 7200 |

---

## Part 2: Region Routing

### Current state

Regions are latency labels applied via tc netem on egress. Every namespace can
reach every other with zero latency by default. `set_region_latency` is one-way
and manual. No concept of regional links or transit.

### New model: Region objects with links

Regions become first-class objects with explicit links between them. Traffic
between regions traverses real per-region transit router namespaces with
configurable latency, jitter, loss, and bandwidth on each link. If a link goes
down, traffic must route through alternate paths (if any exist).

```rust
// Define regions
let us = lab.add_region("us");
let eu = lab.add_region("eu");
let asia = lab.add_region("asia");

// Define inter-region links with presets
lab.link_regions(&us, &eu, RegionLink::good(45));   // 45ms one-way, clean
lab.link_regions(&us, &asia, RegionLink::good(80));  // 80ms one-way, clean
lab.link_regions(&eu, &asia, RegionLink::bad(140));   // 140ms, some jitter+loss

// Or fully custom
lab.link_regions(&us, &eu, RegionLink {
    latency_ms: 45,
    jitter_ms: 5,
    loss_pct: 0.0,
    rate_mbit: 0,     // 0 = unlimited
});

// Attach routers to regions
let dc_us = lab.add_router("dc-us").region(&us).build().await?;
let home_eu = lab.add_router("home-eu").region(&eu).nat(Nat::Home).build().await?;
let isp_asia = lab.add_router("isp-asia").region(&asia).nat(Nat::Cgnat).build().await?;
```

#### RegionLink presets

```rust
pub struct RegionLink {
    pub latency_ms: u32,
    pub jitter_ms: u32,
    pub loss_pct: f32,
    pub rate_mbit: u32,  // 0 = unlimited
}

impl RegionLink {
    /// Clean link with given one-way latency. No jitter, no loss, unlimited.
    pub fn good(latency_ms: u32) -> Self {
        Self { latency_ms, jitter_ms: 0, loss_pct: 0.0, rate_mbit: 0 }
    }

    /// Degraded link. Adds 10% jitter and 0.1% loss relative to latency.
    pub fn bad(latency_ms: u32) -> Self {
        Self {
            latency_ms,
            jitter_ms: latency_ms / 10,
            loss_pct: 0.1,
            rate_mbit: 0,
        }
    }
}
```

#### Quick preset: common region topologies

```rust
// Preset with real-world latencies between US/EU/Asia
let regions = lab.add_default_regions();  // returns { us, eu, asia }
// Pre-configured links:
//   us <> eu:   45ms one-way (transatlantic fiber, ~90ms RTT)
//   us <> asia: 80ms one-way (transpacific fiber, ~160ms RTT)
//   eu <> asia: 140ms one-way (overland/subsea, ~280ms RTT)

let dc = lab.add_router("dc").region(&regions.us).build().await?;
let home = lab.add_router("home").region(&regions.eu).nat(Nat::Home).build().await?;
```

#### Implementation plan

**Architecture**: Each region gets a transit router namespace connected to the
IX bridge. Inter-region links are veth pairs between transit namespaces with
tc netem impairment. Routers in a region route through their region's transit.

**Step-by-step**:

1. **`add_region("name")` creates a Region handle** and allocates a transit
   namespace (like a regular router namespace). The transit gets an IX IP and
   connects to the IX bridge. No routing setup yet.

2. **`link_regions(&a, &b, link)` creates a veth pair** between transit-a and
   transit-b namespaces. Applies tc netem (latency, jitter, loss, rate) on both
   ends of the veth. Adds routes in each transit for the other region's CIDRs
   via the veth peer.

3. **`RouterBuilder::region(&r)` tags the router** with a region. During
   `build()`, the router's IX interface connects to the IX bridge as usual.
   The key change: instead of a direct default route to the IX gateway, the
   router gets a default route via its region's transit namespace.

   Implementation: the router's "ix" veth connects not to the IX bridge
   directly but to a per-region bridge inside the transit namespace. The
   transit namespace then has the IX bridge connection and inter-region veths.

4. **Per-region CIDR tracking**: When a router is assigned to a region, its
   downstream CIDRs are registered with that region. Transit routers
   automatically get routes for all remote-region CIDRs pointing to the
   appropriate inter-transit veths.

5. **Link failure**: `lab.break_region_link(&us, &asia)` brings down the veth
   pair between those transits. If `eu <> asia` and `us <> eu` still exist,
   traffic from US to Asia can route through EU (higher latency:
   45 + 140 = 185ms instead of 80ms). This requires transit routers to have
   routes to all other regions' CIDRs.

**Routing approach**:

The simplest correct approach: each transit namespace has explicit routes to
every other region's CIDRs. For N regions this is O(N^2) routes total, which
is fine for the expected scale (3-10 regions).

- Transit-US routes: `[eu_cidrs] via transit-eu veth`, `[asia_cidrs] via transit-asia veth`
- Transit-EU routes: `[us_cidrs] via transit-us veth`, `[asia_cidrs] via transit-asia veth`

For link failure fallback routing, when a direct link is broken, routes to that
region's CIDRs are updated to point through an intermediate transit. This is a
simple `ip route replace` operation.

**Symmetric routing**: Traffic from US to EU goes through transit-us -> transit-eu.
Return traffic goes transit-eu -> transit-us. Both directions have netem applied
on the respective veth ends, so latency is symmetric by default.

**Complexity assessment**: Medium. The transit namespace creation reuses existing
router namespace code. The main new work is:
- Per-region bridge management inside transit namespaces
- Route table maintenance when regions/links change
- Fallback route computation on link failure (simple for small N)

No source-based routing or policy routing needed. Standard destination-based
routing works because each region's routers connect through their region's
transit, not directly to the IX bridge.

#### Region link reference values

| Route | One-way (ms) | Source |
|-------|-------------|--------|
| US East <> US West | 28 | Azure/AWS measurements |
| US East <> EU West | 45 | Transatlantic fiber (~90ms RTT measured) |
| US West <> Asia Pacific | 80 | Transpacific fiber (~160ms RTT) |
| EU West <> Asia Pacific | 140 | Overland + subsea (~280ms RTT) |
| US East <> South America | 57 | US-Brazil fiber (~114ms RTT) |
| Intra-region (same metro) | 1 | Datacenter measurements |

---

## Part 3: Enhanced Impairment

### Current state

`Impair` has `Wifi` (20ms delay), `Mobile` (50ms, 1% loss), and `Manual`.
No jitter, no reorder, no duplication, no corruption.

### Add jitter + new parameters to ImpairLimits

```rust
/// Parameters for tc netem impairment.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ImpairLimits {
    /// Rate limit in kbit/s (0 = unlimited).
    pub rate_kbit: u32,
    /// Packet loss percentage (0.0 - 100.0).
    pub loss_pct: f32,
    /// One-way latency in milliseconds.
    pub latency_ms: u32,
    /// Jitter in milliseconds (uniform +/-jitter around latency).
    pub jitter_ms: u32,
    /// Packet reordering percentage (0.0 - 100.0).
    pub reorder_pct: f32,
    /// Packet duplication percentage (0.0 - 100.0).
    pub duplicate_pct: f32,
    /// Bit corruption percentage (0.0 - 100.0).
    pub corrupt_pct: f32,
}
```

tc command: `netem delay Xms Yms loss Z% reorder R% duplicate D% corrupt C%`.

### Revised presets

Renamed from `Impair` to `LinkCondition` (see Part 5).

```rust
pub enum LinkCondition {
    /// Wired LAN (1G Ethernet). Negligible latency, no impairment.
    /// Measured: <0.5ms RTT, zero loss on modern switches.
    /// Use for datacenter-local, same-rack communication.
    Lan,

    /// Good WiFi - 5GHz band, close to AP, low contention.
    /// 5ms one-way delay, 2ms jitter, 0.1% loss.
    /// Measured: typical home/office 5GHz under light load.
    Wifi,

    /// Congested WiFi - 2.4GHz, far from AP, interference.
    /// 40ms one-way delay, 15ms jitter, 2% loss, 20 Mbit.
    /// Measured: crowded coffee shop, conference hall, hotel lobby.
    WifiBad,

    /// 4G/LTE good signal.
    /// 25ms one-way delay, 8ms jitter, 0.5% loss.
    /// Measured: urban LTE with good signal (-80 to -90 dBm RSRP).
    Mobile4G,

    /// 3G or degraded 4G.
    /// 100ms one-way delay, 30ms jitter, 2% loss, 2 Mbit.
    /// Measured: HSPA+, or LTE with poor signal in rural/moving.
    Mobile3G,

    /// LEO satellite (Starlink-class).
    /// 40ms one-way delay, 7ms jitter, 1% loss.
    /// Measured: Starlink 2024 median ~80ms RTT, improving.
    Satellite,

    /// GEO satellite (HughesNet/Viasat).
    /// 300ms one-way delay, 20ms jitter, 0.5% loss, 25 Mbit.
    /// Inherent to geostationary orbit (36,000 km round-trip).
    SatelliteGeo,

    /// Fully custom impairment.
    Manual(ImpairLimits),
}
```

The old `Wifi` and `Mobile` presets change values (breaking for tests that assert
exact latency). Tests needing precise control should use `Manual`.

### Router downlink impairment

`Router::impair_downlink` already exists and applies impairment on the router's
downstream bridge, affecting download traffic to all devices. This is the right
place for modeling "bad ISP" or "congested link" conditions affecting all
devices behind a router. Adding `LinkCondition` support to the router builder
makes this even easier:

```rust
// Apply at build time
let home = lab.add_router("home")
    .nat(Nat::Home)
    .downlink_condition(LinkCondition::WifiBad)  // all downstream devices affected
    .build().await?;

// Or change at runtime
home.set_downlink_condition(Some(LinkCondition::Mobile3G))?;
```

`set_link_condition` applies impairment symmetrically (both directions on the
link). This matches reality: bad WiFi affects both upload and download similarly.
Asymmetric rate limiting (different up/down speeds) is deferred to future work.

---

## Part 4: Firewall Presets

### Current state

No packet filter rules beyond NAT. All namespaces accept all traffic.

### Proposed presets

```rust
pub enum Firewall {
    /// No filtering - all traffic passes (default).
    None,

    /// Corporate/enterprise firewall.
    /// Allows: TCP 80, 443. UDP 53 (DNS only). Blocks all other UDP.
    /// Observed on: Cisco ASA, Palo Alto, Fortinet in enterprise deployments.
    /// Impact: STUN/ICE fails, must use TURN-over-TLS on port 443.
    Corporate,

    /// Hotel/airport captive-portal style.
    /// Allows: TCP 80, 443, 53. UDP 53. Throttles everything else.
    /// Observed on: hotel/airport guest WiFi after captive portal auth.
    /// Impact: STUN unreliable, TURN over 443 works.
    CaptivePortal,
}
```

Custom rules via builder:
```rust
lab.add_router("strict")
    .firewall_custom(|fw| fw
        .allow_tcp_out(&[80, 443])
        .allow_udp_out(&[53])
        .block_all_udp_out()  // except DNS
    )
    .build().await?;
```

Implementation: nftables filter chain in the forward hook.

---

## Part 5: API Naming Cleanup

Review of current names against standard networking terminology:

| Current | Issue | New name |
|---------|-------|----------|
| `switch_route(to)` | "switch" is ambiguous (also L2 device) | `set_default_route(ifname)` |
| `switch_uplink(ifname, router)` | see below | `replug_iface(ifname, to_router)` |
| `rebind_nats()` | non-standard term | `flush_nat_state()` |
| `Impair` (type) | verb not noun | `LinkCondition` |
| `impair_link(a, b, impair)` | verb matches if type renamed | `set_link_condition(a, b, cond)` |
| `set_impair(ifname, impair)` | same | `set_link_condition(ifname, cond)` |
| `impair_downlink(impair)` | same | `set_downlink_condition(cond)` |
| `spawn_reflector(bind)` | informal; it's a STUN-like echo server | keep (test infrastructure, not user-facing networking term) |
| `probe_udp_mapping(reflector)` | reasonable STUN terminology | keep |
| `NatMode::DestinationIndependent` | wrong RFC term | `Nat::Home` / `Nat::FullCone` |
| `NatMode::DestinationDependent` | wrong RFC term | `Nat::Corporate` |

**`switch_uplink` rename**: `replug_iface` - describes the physical action
(unplugging a cable from one router and plugging it into another). Accurate
for the implementation (tears down old veth, creates new one to target router).

---

## Part 6: MTU Control

### Proposed API

```rust
lab.add_router("vpn-gw")
    .mtu(1420)         // set MTU on WAN + LAN interfaces
    .build().await?;

// Or per-device
lab.add_device("laptop")
    .uplink(home.id())
    .mtu(1420)
    .build().await?;
```

Optional PMTU blackhole simulation:
```rust
lab.add_router("broken-middlebox")
    .block_icmp_frag_needed()  // drop ICMP type 3 code 4
    .build().await?;
```

Implementation:
- `ip link set dev <iface> mtu <N>` via netlink (already in `netlink.rs`)
- ICMP blocking: single nftables rule in forward chain

---

## Part 7: Node Removal

### Proposed API

```rust
lab.remove_device(dev.id())?;
lab.remove_router(router.id())?;
```

Implementation:
- Close the namespace fd in `NetnsManager` - kernel reclaims all veth pairs,
  routes, nftables rules automatically
- Remove from `NetworkCore` data structures (devices/routers/switches maps)
- Cancel any reflectors via `CancellationToken` (already shared)

This is simple because namespaces are fd-based. Closing the fd is the only
cleanup needed - the kernel destroys everything inside.

---

## Part 8: Network Transition Helpers

Review of what's already possible:

| Scenario | Already possible? | How |
|----------|------------------|-----|
| WiFi to cellular replug_iface | Yes | `dev.link_down("eth0")` + `dev.link_up("eth1")` + `dev.set_default_route("eth1")` |
| Handoff to different router | Yes | `dev.replug_iface("eth0", new_router.id())` (current `switch_uplink`) |
| NAT mapping flush | Yes | `router.flush_nat_state()` (current `rebind_nats`) |
| DHCP renewal (new IP, same link) | **No** | Need `dev.renew_ip("eth0")` |
| Add secondary IP to interface | **No** | Need `dev.add_ip("eth0", ip)` |
| VPN connect (full tunnel) | Yes | `dev.replug_iface("eth0", vpn_router.id())` |
| VPN disconnect | Yes | `dev.replug_iface("eth0", original_router.id())` |
| VPN split tunnel | Yes | Two interfaces on different routers + `set_default_route` |
| Captive portal (no internet) | Yes | Router with no upstream; `replug_iface` to real router after "auth" |

### New methods

**`renew_ip`**: Simulates DHCP renewal. Allocates a new IP from the same
router's pool, replaces the old address via netlink, updates the default route.

```rust
/// Simulates DHCP renewal: allocates a new IP from the current router's pool,
/// replaces the old address, and updates the default route.
pub async fn renew_ip(&self, ifname: &str) -> Result<Ipv4Addr>;
```

The new IP comes from the same router's downstream switch pool (same as
initial allocation). This means consecutive `renew_ip` calls return
incrementing IPs from the pool.

**`add_ip`**: Adds a secondary IP address to an interface. Useful for
multi-homing, anycast, or simulating hosts with multiple addresses.

```rust
/// Adds an additional IP address to an interface without removing existing ones.
pub async fn add_ip(&self, ifname: &str, ip: Ipv4Addr, prefix_len: u8) -> Result<()>;
```

Implementation: netlink `RTM_NEWADDR` (without removing old). Linux supports
multiple addresses per interface natively.

---

## Implementation Order

### Phase 1: NAT presets + API rename (high value)

- [ ] Add `Nat` enum with `None`, `Home`, `Corporate`, `Cgnat`, `CloudNat`, `FullCone`
- [ ] Add `NatMapping`, `NatFiltering` enums for custom builder
- [ ] Implement nftables rules per profile
- [ ] Add conntrack timeout sysctls per profile
- [ ] Migrate existing `DestinationIndependent` sites to `Home` (move to `FullCone` if tests fail)
- [ ] Rename `switch_route` to `set_default_route`
- [ ] Rename `switch_uplink` to `replug_iface`
- [ ] Rename `rebind_nats` to `flush_nat_state`
- [ ] Rename `Impair` to `LinkCondition`, update all method names
- [ ] Keep old names as `#[deprecated]` aliases for one cycle

### Phase 2: Enhanced impairment (high value)

- [ ] Add `jitter_ms`, `reorder_pct`, `duplicate_pct`, `corrupt_pct` to `ImpairLimits`
- [ ] Add `Lan`, `WifiBad`, `Mobile4G`, `Mobile3G`, `Satellite`, `SatelliteGeo` presets
- [ ] Update `Wifi` / `Mobile` presets (breaking - old `Mobile` removed)
- [ ] Add `Manual(ImpairLimits)` with `Default` on `ImpairLimits`
- [ ] Add `downlink_condition` to router builder

### Phase 3: MTU + node removal + IP management (medium value)

- [ ] Add `.mtu()` to router/device builders
- [ ] Add `.block_icmp_frag_needed()` to router builder
- [ ] Implement `lab.remove_device()` / `lab.remove_router()`
- [ ] Add `dev.renew_ip()` for DHCP renewal simulation
- [ ] Add `dev.add_ip()` for secondary addresses

### Phase 4: Hairpinning (medium value)

- [ ] Implement hairpin for EIM+APDF (Home) via LAN-side masquerade rule
- [ ] Add `.hairpin(bool)` to custom NAT builder
- [ ] Test: two devices behind same router reaching each other via external IP

### Phase 5: Region routing (medium value, higher risk)

- [ ] Implement region objects with links (`add_region`, `link_regions`)
- [ ] Add `add_default_regions()` preset
- [ ] Remove old `set_region_latency` API
- [ ] Add `break_region_link` / `restore_region_link`

### Phase 6: Firewall presets (low priority)

- [ ] Add `Firewall` enum with `Corporate`, `CaptivePortal` presets
- [ ] Add `.firewall()` / `.firewall_custom()` to router builder

---

## API sketch: complete example

```rust
let lab = Lab::new();

// Regions with real-world latencies
let regions = lab.add_default_regions(); // { us, eu, asia }

// CGNAT ISP in EU
let isp = lab.add_router("isp-eu")
    .region(&regions.eu)
    .nat(Nat::Cgnat)
    .build().await?;

// Home router behind CGNAT
let home = lab.add_router("home")
    .upstream(isp.id())
    .nat(Nat::Home)
    .build().await?;

// Laptop on home WiFi
let laptop = lab.add_device("laptop")
    .uplink(home.id())
    .link_condition(LinkCondition::Wifi)
    .build().await?;

// Corporate user in US
let corp = lab.add_router("corp-fw")
    .region(&regions.us)
    .nat(Nat::Corporate)
    .firewall(Firewall::Corporate)
    .build().await?;

let workstation = lab.add_device("workstation")
    .uplink(corp.id())
    .build().await?;

// Relay server in US (public IP, no NAT)
let relay_router = lab.add_router("relay-dc")
    .region(&regions.us)
    .build().await?;
let relay = lab.add_device("relay")
    .uplink(relay_router.id())
    .build().await?;

// Test: laptop (Home NAT) <> workstation (Corporate NAT)
// Expected: direct hole-punch fails (Corporate = symmetric NAT),
// must fall back to relay
```

---

## Future Work

Capabilities identified as valuable but deferred beyond the phases above.

### Packet Capture API (low effort when needed)

`dev.capture("eth0", "/tmp/cap.pcap")` spawns `tcpdump` in the namespace.
Already possible via `dev.spawn_command()`, a convenience wrapper is nice but
not blocking.

### Traffic Flow Counters

`dev.iface_stats("eth0") -> IfaceStats` via netlink `RTM_GETLINK` or
`/sys/class/net/*/statistics/`. Useful for assertions on bandwidth tests.

### Conntrack / NAT State Inspection

`router.conntrack_list()` parses `conntrack -L` output. Enables asserting on
mapping reuse (EIM) vs non-reuse (EDM) directly rather than via probes.

### DNS Manipulation

Per-device DNS overrides already work via hosts-file overlay. Missing: DNS
blocking (NXDOMAIN), split-horizon, DNS latency simulation. Could be done with
a tiny stub resolver or impairment on the nameserver path.

### Policy Routing / Source-Based Routing

`ip rule add from <ip> table <N>` for multi-homed MPTCP testing. Not needed
until MPTCP is a test target.

### WiFi Channel Modeling

Out of scope - we model transport-layer effects (latency, jitter, loss) via
tc netem, not the 802.11 physical layer.

### Dynamic Routing Protocols

FRRouting can be spawned inside router namespaces via `router.spawn_command()`
today. No simulator changes needed. Deferred until a test scenario requires it.

### Scale Beyond Single Machine

Hundreds of namespaces per machine is sufficient for P2P testing. Multi-machine
distribution deferred.

### Virtual Time

See [virtual-time.md](virtual-time.md).
