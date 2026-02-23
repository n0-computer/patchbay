# IPv6 Support Plan

**Status:** ❌ not implemented
**Effort:** ~8–10 dev-days for full dual-stack + all NAT combos + test matrix

---

## Goals

- Add IPv6 (and dual-stack) support at every layer: IX bridge, router WAN/LAN, device interfaces.
- Extend NAT to support IPv6 modes (NPTv6, masquerade) in addition to existing IPv4 modes.
- Cover all meaningful (IP version) × (NAT mode) combos in the test matrix.
- Keep the change backwards-compatible: existing IPv4-only topologies continue to work unchanged.

---

## Address Scheme

| Role | IPv4 (existing) | IPv6 (new) |
|---|---|---|
| IX public pool | `203.0.113.0/24` | `2001:db8::/32` (documentation) |
| IX gateway | `203.0.113.1` | `2001:db8::1` |
| Private LAN pool | `10.0.0.0/16` | `fd10::/48` (ULA) |
| Per-router /24 | `10.0.X.0/24` | `fd10:0:X::/64` |
| Loopback | n/a | `::1` (kernel default) |

The IX uses the IANA documentation prefix `2001:db8::/32` (analogous to `203.0.113.0/24`).
Private LANs use a ULA prefix `fd10::/48` (analogous to `10.0.0.0/16`).

---

## IP Version Model

Each router and device interface can be in one of three modes:

```
IpVersion { V4Only, V6Only, DualStack }
```

This is set per-router and propagates to both its WAN link (IX-facing) and LAN switch.
Devices inherit the version of the switch they attach to; if attached to multiple switches with
different versions the device gets the union.

---

## Affected Structs (`core.rs`)

### `CoreConfig`

```rust
// Add alongside existing v4 fields:
pub ix_gw_v6: Ipv6Addr,          // 2001:db8::1
pub ix_cidr_v6: Ipv6Net,         // 2001:db8::/32
pub private_cidr_v6: Ipv6Net,    // fd10::/48
```

### `Switch`

```rust
pub cidr_v6: Option<Ipv6Net>,    // e.g. fd10:0:3::/64
pub gw_v6: Option<Ipv6Addr>,     // fd10:0:3::1
pub next_host_v6: u64,           // monotone allocator (start at 2)
```

### `Router`

```rust
pub upstream_ip_v6: Option<Ipv6Addr>,   // IX-facing address
pub downstream_gw_v6: Option<Ipv6Addr>, // LAN gateway
pub nat_v6: NatV6Mode,
```

### `DeviceIface`

```rust
pub ip_v6: Option<Ipv6Addr>,
```

---

## New Types

### `NatV6Mode`

```rust
#[derive(Default)]
pub enum NatV6Mode {
    #[default]
    None,       // No translation; devices use global unicast directly
    Nptv6,      // RFC 6296 network-prefix translation (stateless, 1:1 prefix mapping)
    Masquerade, // Stateful masquerade (non-standard but useful for testing symmetric behaviour)
}
```

`Nptv6` is the realistic "home with native IPv6" scenario: ULA addresses on LAN are mapped
to the provider-assigned prefix on the WAN side in a stateless bijection.
`Masquerade` is useful when you want to test reflexive address behaviour on IPv6 (analogous to
`DestinationDependent` for IPv4).

### `RouterConfig` TOML extension

```toml
[[router]]
name = "isp"
nat  = "cgnat"      # existing v4 nat field
nat_v6 = "none"     # new: "none" | "nptv6" | "masquerade" (default "none")
ip_version = "dual" # new: "v4" | "v6" | "dual" (default "v4")
```

---

## Allocators (`core.rs`)

Four new functions, mirroring existing IPv4 ones:

```rust
fn alloc_ix_ip_v6_low(&mut self)  -> Ipv6Addr  // 2001:db8::10, ::11, …
fn alloc_ix_ip_v6_high(&mut self) -> Ipv6Addr  // 2001:db8::fa, ::f9, …
fn alloc_private_cidr_v6(&mut self) -> Result<Ipv6Net>  // fd10:0:1::/64, fd10:0:2::/64, …
fn alloc_from_switch_v6(&mut self, sw: SwitchId) -> Result<Ipv6Addr>
    // Increments next_host_v6; calls add_host_v6(cidr, host)
```

`alloc_private_cidr_v6` increments a `/16` slot counter within `fd10::/48`, matching
the existing IPv4 pattern of `/24` slots within `10.0.0.0/16`.

---

## Netlink Additions (`netlink.rs`)

```rust
pub async fn add_addr6(&mut self, ifname: &str, ip: Ipv6Addr, prefix: u8) -> Result<()>
pub async fn add_default_route_v6(&mut self, via: Ipv6Addr) -> Result<()>
pub async fn replace_default_route_v6(&mut self, ifname: &str, via: Ipv6Addr) -> Result<()>
pub async fn add_route_v6(&mut self, dst: Ipv6Net, via: Ipv6Addr) -> Result<()>
```

These are direct analogues of the existing v4 methods, using
`rtnetlink::RouteMessageBuilder::<Ipv6Addr>`.  The existing v4 methods are unchanged.

Also need:
```rust
pub async fn set_sysctl_ipv6_fwd(&self, ns: &str) -> Result<()>
    // writes "1" to /proc/sys/net/ipv6/conf/all/forwarding inside namespace
```

---

## Wire-Iface Changes (`core.rs: wire_iface`)

After existing IPv4 address assignment, add a conditional block:

```rust
if let Some(ip6) = iface.ip_v6 {
    h.add_addr6(&ifname, ip6, 64).await?;
}
if iface.is_default_via {
    if let Some(gw6) = switch.gw_v6 {
        h.add_default_route_v6(gw6).await?;
    }
}
```

Router namespaces additionally call `set_sysctl_ipv6_fwd`.

---

## NAT v6 Implementation (`core.rs`)

### `apply_nat_v6`

```rust
pub async fn apply_nat_v6(
    ns: &str,
    mode: NatV6Mode,
    lan_if: &str,
    wan_if: &str,
    lan_prefix: Ipv6Net,   // ULA /64 (for NPTv6 mapping)
    wan_prefix: Ipv6Net,   // Provider /64
) -> Result<()>
```

**None**: no-op.

**Nptv6** (stateless prefix translation, requires `nft` + kernel `nft_nat`):

```
nft add table ip6 nptv6_{ns}
nft add chain ip6 nptv6_{ns} postrouting  { type nat hook postrouting priority 100; }
nft add chain ip6 nptv6_{ns} prerouting   { type nat hook prerouting  priority -100; }
nft add rule  ip6 nptv6_{ns} postrouting oif "{wan_if}" snat prefix to {wan_prefix}
nft add rule  ip6 nptv6_{ns} prerouting  iif "{wan_if}" dnat prefix to {lan_prefix}
```

Kernel support: `nft_nat` + `ip6_tables` already available in ≥5.1 kernels.
The `snat prefix to` syntax requires nftables ≥ 0.9.6.

**Masquerade** (stateful):

```
nft add table ip6 nat6_{ns}
nft add chain ip6 nat6_{ns} postrouting { type nat hook postrouting priority 100; }
nft add rule  ip6 nat6_{ns} postrouting oif "{wan_if}" masquerade
```

Note: ICMPv6 (NDP, MLD) must not be masqueraded; the kernel handles this by default via
`ip6tables -I FORWARD -p icmpv6 -j ACCEPT` (already implied by `RELATED,ESTABLISHED`
tracking).  Add an explicit FORWARD accept rule for ICMPv6:

```
nft add chain ip6 nat6_{ns} forward { type filter hook forward priority 0; policy accept; }
nft add rule  ip6 nat6_{ns} forward meta l4proto ipv6-icmp accept
```

### Dispatch in `apply_nat` / `apply_nat_v6`

The existing `apply_nat` function for IPv4 is unchanged.  A new `apply_nat_v6` is called
independently after it, during `Router::build`.

---

## QDisc Changes (`qdisc.rs`)

The `add_filter` function currently hardcodes `protocol ip` and `match ip dst`.  Add an
`IpVersion` parameter:

```rust
pub async fn add_filter(
    ns: &str, ifname: &str,
    dst: IpNet,           // changed from Ipv4Net → IpNet (enum)
    // … rest unchanged
) -> Result<()>
```

Internally branch on `dst`:
- `IpNet::V4(_)` → `protocol ip … match ip dst …` (existing behaviour)
- `IpNet::V6(_)` → `protocol ipv6 … match ip6 dst …`

`IpNet` is already a dependency via the `ipnet` crate.

---

## Config Schema (TOML, `lib.rs`)

```rust
#[derive(Deserialize, Default)]
pub struct RouterConfig {
    // existing fields …
    #[serde(default)]
    pub ip_version: IpVersionConfig,  // "v4" | "v6" | "dual"
    #[serde(default)]
    pub nat_v6: NatV6Mode,
}

#[derive(Deserialize, Default)]
pub enum IpVersionConfig {
    #[default] V4,
    V6,
    Dual,
}
```

`Lab::from_config` maps `IpVersionConfig` to the allocator calls:
- `V4`: existing path (no change)
- `V6`: only allocate v6 addresses; skip v4 address assignment in `wire_iface`
- `Dual`: allocate both; both address assignments run in `wire_iface`

---

## Test Matrix

Each test row is a `#[tokio::test]` in `lib.rs` (following existing NAT test patterns).

### Topology/Version combinations

| # | IX mode | Router WAN | Router LAN | Device | Expected |
|---|---------|------------|------------|--------|----------|
| 1 | v4 | v4-only | v4-only | v4 | Baseline (existing) |
| 2 | v6 | v6-only | v6-only | v6 | IPv6-only end-to-end |
| 3 | dual | dual | dual | dual | Both stacks reachable |
| 4 | dual | dual | v4-only | v4 | v6 termination at router |
| 5 | dual | dual | v6-only | v6 | v4 termination at router |

Tests 2–5 verify basic `udp_rtt_in_ns` reachability.

### NAT × IP-version combos (all use dual-stack IX)

| # | v4 nat | v6 nat | Test assertion |
|---|--------|--------|---------------|
| A | None | None | Device sees own public address on both stacks |
| B | EI | None | v4 reflexive = router WAN IP; v6 = device ULA mapped to… wait, none means global |
| C | DD | None | v4 changes per-destination; v6 stable |
| D | Cgnat | None | v4 double-NAT; v6 direct |
| E | EI | NPTv6 | v4 EI mapping; v6 NPTv6 prefix translated |
| F | DD | NPTv6 | v4 symmetric; v6 prefix translated |
| G | EI | Masquerade | v4 EI; v6 masquerade (source changes per-flow) |
| H | None | NPTv6 | v4 direct; v6 NPTv6 |
| I | None | Masquerade | v4 direct; v6 masquerade |

(Cgnat × v6-nat combos omitted — CGNAT is ISP-only, not home-router level; covered by D.)

### Test helpers to add

```rust
// Dual-stack reflexive address probe
async fn probe_v6_in_ns(ns: &str, dst: Ipv6Addr, port: u16) -> Result<SocketAddrV6>
async fn udp_rtt_v6_in_ns(ns: &str, dst: SocketAddrV6) -> Result<Duration>

// Dual-stack reflector (listens on ::0 and 0.0.0.0)
fn spawn_dual_reflector(ns: &str, port: u16) -> JoinHandle<()>
```

The existing `probe_in_ns` / `udp_rtt_in_ns` bind explicitly to `0.0.0.0`; the new v6
variants bind to `[::]:port`.  A dual-stack variant binding to `[::]:port` with
`IPV6_V6ONLY=0` can cover both if the test host kernel supports it (true on Linux).

---

## Backwards Compatibility

- Default `ip_version = "v4"` and `nat_v6 = "none"` for all routers → existing topologies
  and tests are unaffected.
- All new fields are `#[serde(default)]` with the IPv4-only default.
- `wire_iface` only calls IPv6 netlink methods when `ip_v6.is_some()`.

---

## Effort Breakdown

| Area | Estimate |
|------|----------|
| `CoreConfig` + struct fields (v6 address fields, allocators) | 0.5 d |
| `netlink.rs` v6 methods (`add_addr6`, routes, fwd sysctl) | 0.5 d |
| `wire_iface` dual-stack provisioning | 0.5 d |
| Config/TOML schema + `Lab::from_config` routing | 0.5 d |
| Allocators (`alloc_ix_v6_*`, `alloc_private_cidr_v6`, host alloc) | 0.5 d |
| `apply_nat_v6` (NPTv6 + masquerade nftables rules) | 1.5 d |
| `qdisc.rs` dual-protocol filter (`IpNet` branching) | 0.5 d |
| Test helpers (`probe_v6_in_ns`, dual reflector) | 0.5 d |
| Test matrix rows (combos 1–5 + A–I, ~14 tests) | 3.0 d |
| **Total** | **~8 d** |

The 3 days for tests is the dominant cost.  The implementation side is mostly additive
(new methods alongside existing ones); no IPv4 code paths need to change.

### What could blow up

- **nftables `snat prefix to` availability**: requires nftables ≥ 0.9.6 and kernel
  `nft_chain_nat` built with IPv6.  The VM kernel used by the test harness may need
  a config check.
- **NPTv6 checksum-neutral mapping**: the kernel's NPTv6 implementation (`nft`) handles
  this transparently via the checksum-neutral algorithm; no application changes needed.
- **ICMPv6 / NDP**: masquerade mode must not break Neighbor Discovery.  The `FORWARD`
  accept rule for `ipv6-icmp` covers this, but needs careful testing on multi-hop
  topologies.
- **Dual-stack reflector bind**: `[::]:port` with `IPV6_V6ONLY=0` covers both families on
  Linux; if any test runs in a netns where IPv4-mapped IPv6 is disabled (rare), separate
  v4 and v6 reflectors are needed.
