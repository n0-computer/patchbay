# Building Topologies

A patchbay topology is built from three kinds of objects: **routers** that
provide network connectivity, **devices** that run your code, and
**regions** that introduce latency between groups of routers. This chapter
explains how to compose them into realistic network layouts.

## Routers

Every router connects to the lab's internet exchange (IX) bridge and
receives a public IP address on that link. Downstream devices connect to
the router through veth pairs and receive addresses from the router's
address pool.

```rust
let dc = lab.add_router("dc").build().await?;
```

A router with no additional configuration acts like a datacenter switch:
devices behind it get public IPs, there is no NAT, and there is no
firewall. To model different real-world environments, you configure NAT,
firewalls, IP support, and address pools on the router builder. The
[NAT and Firewalls](nat-and-firewalls.md) chapter covers those options
in detail.

### Chaining routers

Routers can be chained behind other routers using the `.upstream()` method.
Instead of connecting directly to the IX, the downstream router receives
its address from the parent router's pool. This is how you build
multi-layer topologies like ISP + home or corporate gateway + branch
office:

```rust
let isp = lab.add_router("isp").nat(Nat::Cgnat).build().await?;
let home = lab
    .add_router("home")
    .upstream(isp.id())
    .nat(Nat::Home)
    .build()
    .await?;
```

In this example, the home router sits behind the ISP. Devices behind
`home` are double-NATed: their traffic passes through home NAT first, then
through carrier-grade NAT at the ISP. This is a common topology for
testing P2P connectivity where both peers sit behind multiple layers of
NAT.

### Router presets

For common deployment patterns, `RouterPreset` configures NAT, firewall,
IP support, and address pool in a single call. This avoids repeating the
same combinations across tests:

```rust
use patchbay::RouterPreset;

let home = lab.add_router("home").preset(RouterPreset::Home).build().await?;
let dc   = lab.add_router("dc").preset(RouterPreset::Datacenter).build().await?;
let corp = lab.add_router("corp").preset(RouterPreset::Corporate).build().await?;
```

The following table lists all available presets. Each row shows the NAT
mode, firewall policy, IP address family, and downstream address pool that
the preset configures:

| Preset | NAT | Firewall | IP support | Pool |
|--------|-----|----------|------------|------|
| `Home` | Home | BlockInbound | DualStack | Private |
| `Datacenter` | None | None | DualStack | Public |
| `IspV4` | Cgnat | None | V4Only | CgnatShared |
| `Mobile` | Home | BlockInbound | DualStack | Private |
| `MobileV6` | None (v4) / Nat64 (v6) | BlockInbound | V6Only | Public |
| `Corporate` | Corporate | Corporate | DualStack | Private |
| `Hotel` | Home | CaptivePortal | DualStack | Private |
| `Cloud` | None | None | DualStack | Public |

Methods called after `.preset()` override the preset's defaults, so you
can use a preset as a starting point and customize individual settings.
For example, `RouterPreset::Home` with `.nat(Nat::FullCone)` gives you a
home-style topology with fullcone NAT instead of the default
endpoint-dependent filtering.

### Address families

By default, routers run dual-stack (both IPv4 and IPv6). You can restrict
a router to a single address family with `.ip_support()`:

```rust
use patchbay::IpSupport;

let v6_only = lab.add_router("carrier")
    .ip_support(IpSupport::V6Only)
    .build().await?;
```

The three options are `V4Only`, `V6Only`, and `DualStack`. Devices behind
a V6Only router will only receive IPv6 addresses. If the router also has
NAT64 enabled, those devices can still reach IPv4 destinations through the
NAT64 prefix; see the [NAT and Firewalls](nat-and-firewalls.md) chapter
for details.

## Devices

Devices are the endpoints where your code runs. Each device gets its own
network namespace with one or more interfaces, each connected to a
router. IP addresses are assigned automatically from the router's pool.

```rust
let server = lab
    .add_device("server")
    .iface("eth0", dc.id(), None)
    .build()
    .await?;
```

You can read a device's assigned addresses through the handle:

```rust
let v4: Option<Ipv4Addr> = server.ip();
let v6: Option<Ipv6Addr> = server.ip6();
let ll: Option<Ipv6Addr> = server.default_iface().and_then(|i| i.ll6());
```

For router-side address inspection, use `router.interfaces()` or
`router.iface("ix")` / `router.iface("wan")` and read `ip6()` plus `ll6()`
from `RouterIface`.

### Multi-homed devices

A device can have multiple interfaces, each connected to a different
router. This models machines with both WiFi and Ethernet, phones with
WiFi and cellular, or VPN scenarios where a tunnel interface coexists with
the physical link:

```rust
let phone = lab
    .add_device("phone")
    .iface("wlan0", home.id(), Some(LinkCondition::Wifi))
    .iface("cell0", carrier.id(), Some(LinkCondition::Mobile4G))
    .default_via("wlan0")
    .build()
    .await?;
```

The `.default_via("wlan0")` call sets which interface carries the default
route. At runtime, you can switch the default route to a different
interface to simulate a handoff:

```rust
phone.set_default_route("cell0").await?;
```

## Link conditions

Link conditions simulate real-world network impairment. Under the hood,
patchbay uses `tc netem` for loss, latency, and jitter, and `tc tbf` for
rate limiting. You can apply conditions at build time through interface
presets, through custom parameters, or dynamically at runtime.

### Presets

The built-in presets model common access technologies:

| Preset | Loss | Latency | Jitter | Rate |
|--------|------|---------|--------|------|
| `Wifi` | 2% | 5 ms | 1 ms | 54 Mbit/s |
| `Mobile4G` | 1% | 30 ms | 10 ms | 50 Mbit/s |
| `Mobile3G` | 3% | 100 ms | 30 ms | 2 Mbit/s |
| `Satellite` | 0.5% | 600 ms | 50 ms | 10 Mbit/s |

Apply a preset when building the interface:

```rust
let dev = lab.add_device("laptop")
    .iface("eth0", home.id(), Some(LinkCondition::Wifi))
    .build().await?;
```

### Custom parameters

When the presets do not match your scenario, build a `LinkLimits` struct
directly:

```rust
use patchbay::{LinkCondition, LinkLimits};

let degraded = LinkCondition::Manual(LinkLimits {
    rate_kbit: 1000,    // 1 Mbit/s
    loss_pct: 10.0,     // 10% packet loss
    latency_ms: 50,     // 50 ms one-way delay
    jitter_ms: 20,      // 20 ms jitter
    ..Default::default()
});

let dev = lab.add_device("laptop")
    .iface("eth0", home.id(), Some(degraded))
    .build().await?;
```

### Runtime changes

You can change or remove link conditions at any point after the topology
is built. This is useful for simulating network degradation during a test,
for example switching from WiFi to a congested 3G link and verifying that
your application adapts:

```rust
dev.set_link_condition("eth0", Some(LinkCondition::Mobile3G)).await?;

// Later, restore a clean link.
dev.set_link_condition("eth0", None).await?;
```

## Regions

Regions model geographic distance between groups of routers. When you
assign routers to different regions and link those regions, traffic between
them passes through per-region router namespaces that apply configurable
latency via `tc netem`. This gives you realistic cross-continent delays on
top of any per-link conditions.

```rust
let eu = lab.add_region("eu").await?;
let us = lab.add_region("us").await?;
lab.link_regions(&eu, &us, RegionLink::good(80)).await?;

let dc_eu = lab.add_router("dc-eu").region(&eu).build().await?;
let dc_us = lab.add_router("dc-us").region(&us).build().await?;
```

In this topology, traffic between `dc_eu` and `dc_us` carries 80 ms of
added round-trip latency. Routers within the same region communicate
without the region penalty.

### Fault injection with region links

You can break and restore region links at runtime to simulate network
partitions. This is valuable for testing how your application handles
split-brain scenarios, failover logic, and reconnection:

```rust
lab.break_region_link(&eu, &us).await?;
// All traffic between EU and US routers is now blackholed.

// ... run your partition test ...

lab.restore_region_link(&eu, &us).await?;
// Connectivity is restored.
```

The break is immediate: packets in flight are dropped, and no new packets
can cross the link until it is restored.
