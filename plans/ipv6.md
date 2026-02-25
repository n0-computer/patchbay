# IPv6 Support Plan (New API)

## TODO

- [x] Write plan
- [ ] Phase 1: Data model + netlink + allocators
- [ ] Phase 2: Builder + async setup + handle accessors
- [ ] Phase 3: NAT v6 + qdisc dual-protocol + config/TOML
- [ ] Phase 4: Tests

**Status:** in progress

---

## Goals

- Dual-stack and v6-only at every layer: IX bridge, router WAN/LAN, device interfaces.
- IPv6 NAT modes (NPTv6, masquerade) alongside existing IPv4 modes.
- Backwards-compatible: existing v4-only topologies unchanged.
- Handle-based API (`Device`, `Router`, `DeviceIface`) throughout; no namespace names in tests.
- Test suite designed with parameterized helpers to avoid code duplication across v4/v6.

---

## Address Scheme

| Role | IPv4 (existing) | IPv6 (new) |
|---|---|---|
| IX public pool | `203.0.113.0/24` | `2001:db8::/32` (documentation prefix) |
| IX gateway | `203.0.113.1` | `2001:db8::1` |
| Private LAN pool | `10.0.0.0/16` | `fd10::/48` (ULA) |
| Per-router /24 | `10.0.X.0/24` | `fd10:0:X::/64` |

---

## New Public Types

### `IpSupport`

```rust
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Deserialize)]
pub enum IpSupport {
    #[default]
    V4Only,
    V6Only,
    DualStack,
}
```

Set per-router via `RouterBuilder::ip_support()`. Propagates to WAN link, LAN switch, and
device interfaces (devices inherit from the switch they attach to).

### `NatV6Mode`

```rust
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Deserialize)]
pub enum NatV6Mode {
    #[default]
    None,       // No translation; global unicast directly
    Nptv6,      // RFC 6296 stateless prefix translation
    Masquerade, // Stateful masquerade (useful for testing symmetric v6 behaviour)
}
```

---

## Phase 1: Data Model + Netlink + Allocators

**Files:** `core.rs`, `netlink.rs`

### Struct additions

**`CoreConfig`** — add alongside existing v4 fields, initialize in `NetworkCore::new()`:
```rust
pub ix_gw_v6: Ipv6Addr,       // 2001:db8::1
pub ix_cidr_v6: Ipv6Net,      // 2001:db8::/32
pub private_cidr_v6: Ipv6Net, // fd10::/48
```

**`Switch`** — add v6 pool alongside v4:
```rust
pub cidr_v6: Option<Ipv6Net>,     // e.g. fd10:0:3::/64
pub gw_v6: Option<Ipv6Addr>,      // fd10:0:3::1
next_host_v6: u8,                  // monotone allocator (start at 2)
```

**`RouterData`** — add v6 fields:
```rust
pub upstream_ip_v6: Option<Ipv6Addr>,
pub downstream_gw_v6: Option<Ipv6Addr>,
pub nat_v6: NatV6Mode,
pub ip_support: IpSupport,
```

**`DeviceIfaceData`** — add:
```rust
pub ip_v6: Option<Ipv6Addr>,
```

**`RouterConfig`** (internal, not config.rs) — add:
```rust
pub nat_v6: NatV6Mode,
pub ip_support: IpSupport,
```

**`IfaceBuild`** — add optional v6 fields:
```rust
pub(crate) gw_ip_v6: Option<Ipv6Addr>,
pub(crate) dev_ip_v6: Option<Ipv6Addr>,
pub(crate) prefix_len_v6: u8,        // typically 64
```

**`RouterSetupData`** — add v6 mirror of every v4 field:
```rust
pub ip_support: IpSupport,
pub nat_v6: NatV6Mode,
pub ix_gw_v6: Option<Ipv6Addr>,
pub ix_cidr_v6_prefix: Option<u8>,
pub upstream_gw_v6: Option<Ipv6Addr>,
pub upstream_cidr_prefix_v6: Option<u8>,
pub return_route_v6: Option<(Ipv6Addr, u8, Ipv6Addr)>,
pub downlink_bridge_v6: Option<(Ipv6Addr, u8)>,
pub downstream_cidr_v6: Option<Ipv6Net>,  // for NPTv6 LAN prefix
```

### Netlink v6 methods

Add to `Netlink` in `netlink.rs` — direct analogues of v4 using `RouteMessageBuilder::<Ipv6Addr>`:

```rust
pub(crate) async fn add_addr6(&mut self, ifname: &str, ip: Ipv6Addr, prefix: u8) -> Result<()>
// Pattern: identical to add_addr4 — `self.handle.address().add(idx, ip.into(), prefix)`

pub(crate) async fn add_default_route_v6(&mut self, via: Ipv6Addr) -> Result<()>
// Pattern: `RouteMessageBuilder::<Ipv6Addr>::new().gateway(via).build()`

pub(crate) async fn replace_default_route_v6(&mut self, ifname: &str, via: Ipv6Addr) -> Result<()>
// Pattern: iterate v6 routes, delete those with prefix_len==0, add new bound to ifindex

pub(crate) async fn add_route_v6(&mut self, dst: Ipv6Addr, prefix: u8, via: Ipv6Addr) -> Result<()>
// Pattern: `RouteMessageBuilder::<Ipv6Addr>::new().destination_prefix(dst, prefix).gateway(via).build()`
```

### Allocators

New methods on `NetworkCore`:

```rust
// Parallel to alloc_ix_ip_low(). State: next_ix_low_v6: u16 starting at 0x10.
// Returns 2001:db8::10, ::11, … by patching the last segment of ix_gw_v6.
pub(crate) fn alloc_ix_ip_v6_low(&mut self) -> Ipv6Addr

// Parallel to alloc_private_cidr(). State: next_private_slot_v6: u16 starting at 1.
// Returns fd10:0:1::/64, fd10:0:2::/64, … by incrementing segment 3 of private_cidr_v6.
pub(crate) fn alloc_private_cidr_v6(&mut self) -> Ipv6Net

// Parallel to alloc_from_switch(). Uses switch.next_host_v6.
// Patches the last segment of switch.cidr_v6 with the host counter.
pub(crate) fn alloc_from_switch_v6(&mut self, sw: NodeId) -> Result<Ipv6Addr>
```

Update existing `add_device_iface` — after v4 allocation, if switch has `cidr_v6`, also call
`alloc_from_switch_v6()` and store into `DeviceIfaceData.ip_v6`.

**Verification:** `cargo check -p netsim-core --tests`

---

## Phase 2: Builder + Async Setup + Handle Accessors

**Files:** `lab.rs`, `core.rs`

### RouterBuilder additions

```rust
pub struct RouterBuilder {
    // existing: inner, name, region, upstream, nat, result
    ip_support: IpSupport,   // new, default V4Only
    nat_v6: NatV6Mode,       // new, default None
}

impl RouterBuilder {
    pub fn ip_support(mut self, support: IpSupport) -> Self
    pub fn nat_v6(mut self, mode: NatV6Mode) -> Self
}
```

### RouterBuilder::build() phase 1 changes

In the lock section, when `ip_support != V4Only`:
1. `alloc_ix_ip_v6_low()` → store in `RouterData.upstream_ip_v6`
2. `alloc_private_cidr_v6()` → pass to `add_switch()` as v6 CIDR
3. `connect_router_uplink()` — also store v6 IP
4. Populate all `RouterSetupData` v6 fields from cfg + router snapshot

For sub-routers: read parent switch's `gw_v6` and `cidr_v6.prefix_len()` into
`upstream_gw_v6` / `upstream_cidr_prefix_v6`.

When `ip_support == V6Only`: skip v4 allocations entirely. Set `upstream_ip = None`,
`downstream_cidr = None`, etc. V6-only routers still need a bridge (L2), but no v4 addresses.

### setup_router_async changes

**IX-level router** — after existing v4 block (`add_addr4` + `add_default_route_v4`):
```rust
if let Some(ip6) = router.upstream_ip_v6 {
    h.add_addr6("ix", ip6, data.ix_cidr_v6_prefix.unwrap()).await?;
    h.add_default_route_v6(data.ix_gw_v6.unwrap()).await?;
}
```

After `set_sysctl_in(…, "net/ipv4/ip_forward", "1")`:
```rust
if data.ip_support != IpSupport::V4Only {
    set_sysctl_in(&router.ns, "net/ipv6/conf/all/forwarding", "1")?;
    set_sysctl_in(&router.ns, "net/ipv6/conf/all/accept_dad", "0")?;  // disable DAD
}
```

**Sub-router** — same pattern on the `wan` interface.

**Downlink bridge** — after `add_addr4(&br, lan_ip, lan_prefix)`:
```rust
if let Some((gw_v6, prefix)) = &data.downlink_bridge_v6 {
    h.add_addr6(&br, *gw_v6, *prefix).await?;
}
```

**Return route** — after v4 return route:
```rust
if let Some((net6, prefix6, via6)) = data.return_route_v6 {
    nl_run(netns, &data.root_ns, async move |h| {
        h.add_route_v6(net6, prefix6, via6).await.ok();
        Ok(())
    }).await.ok();
}
```

**NAT v6** — after `apply_nat(…)`:
```rust
if data.nat_v6 != NatV6Mode::None {
    apply_nat_v6(&router.ns, data.nat_v6, wan_if, lan_prefix_v6, wan_prefix_v6).await?;
}
```

### wire_iface_async changes

In the device-ns nl_run block, after `add_addr4` + `add_default_route_v4`:
```rust
if let Some(ip6) = dev_ip_v6 {
    h.add_addr6(&ifname, ip6, prefix_len_v6).await?;
    // Disable DAD on device interfaces too
    set_sysctl_in(&dev_ns, "net/ipv6/conf/all/accept_dad", "0").ok();
}
if is_default {
    if let Some(gw6) = gw_ip_v6 {
        h.add_default_route_v6(gw6).await?;
    }
}
```

### setup_root_ns_async changes

The IX bridge needs a v6 address and v6 forwarding in the root namespace:
```rust
// After existing v4 setup of IX bridge:
h.add_addr6(&cfg.ix_br, cfg.ix_gw_v6, cfg.ix_cidr_v6.prefix_len()).await?;
set_sysctl_in(&cfg.root_ns, "net/ipv6/conf/all/forwarding", "1")?;
set_sysctl_in(&cfg.root_ns, "net/ipv6/conf/all/accept_dad", "0")?;
```

### Handle accessors

**Router:**
```rust
pub fn ip_support(&self) -> IpSupport
pub fn uplink_ip_v6(&self) -> Option<Ipv6Addr>
pub fn downstream_cidr_v6(&self) -> Option<Ipv6Net>
pub fn downstream_gw_v6(&self) -> Option<Ipv6Addr>
pub fn nat_v6_mode(&self) -> NatV6Mode
```

**Device:**
```rust
pub fn ip6(&self) -> Option<Ipv6Addr>  // default iface ip_v6
```

**DeviceIface:**
```rust
pub fn ip6(&self) -> Option<Ipv6Addr>
```

**Lab:**
```rust
pub fn router_uplink_ip_v6(&self, id: NodeId) -> Result<Option<Ipv6Addr>>
```

**lib.rs exports:** `IpSupport`, `NatV6Mode`

**Verification:** `cargo check -p netsim-core --tests`, write `smoke_v6_dc_roundtrip` test

---

## Phase 3: NAT v6 + QDisc + Config/TOML

### apply_nat_v6

Mirror of `apply_nat` / `apply_home_nat` / `apply_isp_cgnat` using `table ip6 nat`:

```rust
pub(crate) async fn apply_nat_v6(
    ns: &str,
    mode: NatV6Mode,
    wan_if: &str,
    lan_prefix: Ipv6Net,
    wan_prefix: Ipv6Net,
) -> Result<()> {
    match mode {
        NatV6Mode::None => Ok(()),
        NatV6Mode::Nptv6 => {
            let rules = format!(r#"
table ip6 nat {{
    chain postrouting {{
        type nat hook postrouting priority 100; policy accept;
        oif "{wan}" snat prefix to {wan_pfx}
    }}
    chain prerouting {{
        type nat hook prerouting priority -100; policy accept;
        iif "{wan}" dnat prefix to {lan_pfx}
    }}
}}
"#, wan=wan_if, wan_pfx=wan_prefix, lan_pfx=lan_prefix);
            run_nft_in(ns, &rules).await
        }
        NatV6Mode::Masquerade => {
            let rules = format!(r#"
table ip6 nat {{
    chain postrouting {{
        type nat hook postrouting priority 100; policy accept;
        oif "{wan}" masquerade
    }}
    chain forward {{
        type filter hook forward priority 0; policy accept;
        meta l4proto ipv6-icmp accept
    }}
}}
"#, wan=wan_if);
            run_nft_in(ns, &rules).await
        }
    }
}
```

Also add `Router::set_nat_v6_mode` (flush `table ip6 nat` + re-apply).

### QDisc dual-protocol

Change `apply_region_latency` signature from `&[(Ipv4Net, u32)]` to `&[(IpNet, u32)]`.

The `Qdisc::add_filter` method currently hardcodes `"protocol", "ip"` and `"match", "ip"`.
Add `add_filter_v6` with `"protocol", "ipv6"` and `"match", "ip6"`, using `prio 2` so both
v4 and v6 filters coexist on the same HTB tree.

In the `apply_region_latency` loop, branch on `IpNet::V4` / `IpNet::V6` to call the right filter method.
Both share the same HTB class + netem qdisc.

Update `apply_region_latencies` in `lab.rs` — for dual-stack routers, emit both v4 CIDRs and
v6 CIDRs (IX IPs + downstream CIDRs) into the filter list.

### Config/TOML

`config.rs` — add to `RouterConfig`:
```rust
#[serde(default)]
pub ip_support: IpSupport,
#[serde(default)]
pub nat_v6: NatV6Mode,
```

`lab.rs` `from_config` — chain `.ip_support(rcfg.ip_support).nat_v6(rcfg.nat_v6)`.

TOML example:
```toml
[[router]]
name = "isp"
nat = "cgnat"
nat_v6 = "masquerade"
ip_support = "dual"
```

**Verification:** `cargo check --workspace --tests`, existing tests still pass

---

## Phase 4: Tests

### Test design: parameterized helpers to avoid duplication

Instead of duplicating each test for v4/v6, define reusable topology builders and assertion helpers:

```rust
/// Address family for test parameterization.
enum Af { V4, V6 }

/// Build a simple DC + device topology, optionally dual-stack.
async fn build_dc_dev(ip: IpSupport, nat: NatMode, nat_v6: NatV6Mode) -> Result<(Lab, Router, Device)>

/// Build an ISP + home-router + device topology.
async fn build_isp_home_dev(
    ip: IpSupport, isp_nat: NatMode, home_nat: NatMode, nat_v6: NatV6Mode
) -> Result<(Lab, Router, Router, Device)>

/// Spawn a reflector on the given address family in a router's namespace.
async fn spawn_reflector(lab: &Lab, router: &Router, af: Af, port: u16) -> Result<SocketAddr>

/// Probe a reflector from a device and return the observed external address.
fn probe_reflexive(lab: &Lab, dev: &Device, reflector: SocketAddr, af: Af) -> Result<ObservedAddr>

/// Assert UDP roundtrip succeeds within timeout.
fn assert_roundtrip(lab: &Lab, dev: &Device, reflector: SocketAddr, af: Af) -> Result<()>

/// Assert UDP roundtrip fails (timeout).
fn assert_no_roundtrip(lab: &Lab, dev: &Device, reflector: SocketAddr, af: Af) -> Result<()>

/// Measure UDP RTT.
fn measure_rtt(lab: &Lab, dev: &Device, reflector: SocketAddr, af: Af) -> Result<Duration>
```

This lets tests like `nat_matrix` iterate over `[Af::V4, Af::V6]` instead of duplicating entire test bodies.

### Test list

**Smoke tests (basic connectivity):**

| Test | Setup | Assertion |
|------|-------|-----------|
| `smoke_v6_dc_roundtrip` | DC (v6-only) + device | v6 UDP roundtrip succeeds |
| `smoke_dual_stack_roundtrip` | DC (dual) + device | both v4 and v6 roundtrips succeed |
| `smoke_v6_ping_gateway` | v6-only router + device | `ping6 <gateway>` succeeds |
| `v6_device_to_device_same_lan` | 2 devices on v6-only router | `ping6` between devices |
| `v6_only_no_v4` | v6-only router + device | v4 probe fails, v6 succeeds |
| `dual_stack_public_addrs` | DC (dual, no NAT) + device | v4 reflexive = public, v6 reflexive = global |

**NAT v6 tests (all dual-stack IX):**

| Test | v4 NAT | v6 NAT | Assertion |
|------|--------|--------|-----------|
| `nat_v6_none_global` | None | None | v6 reflexive = device global addr |
| `nat_v6_nptv6_prefix` | None | NPTv6 | v6 reflexive has router's WAN /64 prefix |
| `nat_v6_masquerade` | None | Masquerade | v6 reflexive = router WAN addr |
| `nat_v6_dual_ei_none` | EI | None | v4 = router WAN IP; v6 = global |
| `nat_v6_dual_dd_none` | DD | None | v4 changes per-dest; v6 stable |
| `nat_v6_dual_ei_nptv6` | EI | NPTv6 | both translated, both reachable |

**Latency/impairment over v6:**

| Test | Assertion |
|------|-----------|
| `latency_v6_region` | v6 inter-region RTT includes region latency |
| `impair_v6_device` | v6 device impair adds expected delay |
| `latency_dual_stack_region` | both v4 and v6 see region latency |

**Verification:** `cargo test -p netsim-core -- --test-threads=1` (all pass)

---

## Backwards Compatibility

- Default `IpSupport::V4Only` and `NatV6Mode::None` → existing topologies unchanged.
- All new fields are `Option<_>` or have v4-only defaults.
- `wire_iface_async` only calls v6 methods when `dev_ip_v6.is_some()`.
- `setup_router_async` only does v6 setup when `ip_support != V4Only`.
- IX bridge always gets v6 address (cheap, harmless, simplifies dual-stack routers).

---

## Risks

- **nftables `snat prefix to`**: requires nftables >= 0.9.6. Check with `nft --version`.
- **DAD delays**: disable via `accept_dad=0` sysctl in every namespace during setup.
- **ICMPv6 / NDP under masquerade**: explicit `ipv6-icmp accept` in forward chain.
- **V6Only mode**: must skip all v4 allocation paths cleanly; `ip: None` in `DeviceIfaceData`.
