# Topology: Routers, Devices, and Regions

patchbay topologies are built from three primitives: **routers** that
provide connectivity, **devices** that run your code, and **regions**
that add inter-region latency.

## Routers

Every router connects to the lab's IX (internet exchange) bridge and
gets a public IP on that link. Downstream devices connect to the router
through veth pairs.

```rust
let dc = lab.add_router("dc").build().await?;
```

### Router chains (upstream)

Routers can chain behind other routers using `.upstream()`. The
downstream router gets its public IP from the parent instead of the IX
directly. This is how you build multi-layer topologies:

```rust
let isp = lab.add_router("isp").build().await?;
let home = lab
    .add_router("home")
    .upstream(isp.id())
    .nat(Nat::Home)
    .build()
    .await?;
```

Here, `home` sits behind `isp`. Devices behind `home` are double-NATed
(carrier-grade NAT + home NAT) if you add NAT to both routers.

### Router presets

`RouterPreset` configures NAT, firewall, IP support, and address pool in
one call:

```rust
use patchbay::RouterPreset;

let home = lab.add_router("home").preset(RouterPreset::Home).build().await?;
let dc   = lab.add_router("dc").preset(RouterPreset::Datacenter).build().await?;
let corp = lab.add_router("corp").preset(RouterPreset::Corporate).build().await?;
```

Available presets:

| Preset | NAT | Firewall | IP Support | Pool |
|--------|-----|----------|------------|------|
| `Home` | Home | BlockInbound | DualStack | Private |
| `Datacenter` | None | None | DualStack | Public |
| `IspV4` | Cgnat | None | V4Only | CgnatShared |
| `Mobile` | Home | BlockInbound | DualStack | Private |
| `MobileV6` | None (v4) / Nat64 (v6) | BlockInbound | V6Only | Public |
| `Corporate` | Corporate | Corporate | DualStack | Private |
| `Hotel` | Home | CaptivePortal | DualStack | Private |
| `Cloud` | None | None | DualStack | Public |

Individual methods called after `.preset()` override preset values, so
you can use a preset as a starting point and tweak specific settings.

### IP support

Control which address families a router provides:

```rust
use patchbay::IpSupport;

let v4_only = lab.add_router("legacy")
    .ip_support(IpSupport::V4Only)
    .build().await?;

let v6_only = lab.add_router("modern")
    .ip_support(IpSupport::V6Only)
    .build().await?;

let dual = lab.add_router("both")
    .ip_support(IpSupport::DualStack)
    .build().await?;
```

## Devices

Devices are the endpoints where your code runs. Each device gets one or
more network interfaces, each connected to a router.

```rust
let phone = lab
    .add_device("phone")
    .iface("wlan0", home.id(), Some(LinkCondition::Wifi))
    .build()
    .await?;
```

### Multi-interface devices

A device can have multiple interfaces connected to different routers.
This models dual-homed machines, VPN scenarios, or WiFi + cellular
devices:

```rust
let phone = lab
    .add_device("phone")
    .iface("wlan0", home.id(), Some(LinkCondition::Wifi))
    .iface("cell0", carrier.id(), Some(LinkCondition::Mobile4G))
    .default_via("wlan0")
    .build()
    .await?;
```

`.default_via("wlan0")` sets which interface carries the default route.
You can switch it at runtime with `phone.set_default_route("cell0")`.

### Device IPs

Devices get IPs assigned automatically from their router's pool. Access
them through the handle:

```rust
let v4 = phone.ip();       // Option<Ipv4Addr>
let v6 = phone.ip6();      // Option<Ipv6Addr>
```

## Link conditions

Link conditions simulate real-world impairment using `tc netem` and
`tc tbf`. Apply them at build time or change them dynamically.

### Built-in presets

```rust
use patchbay::LinkCondition;

// At build time:
let dev = lab.add_device("laptop")
    .iface("eth0", home.id(), Some(LinkCondition::Wifi))
    .build().await?;
```

Available presets: `Wifi` (2% loss, 5ms latency, 1ms jitter, 54 Mbit),
`Mobile4G` (1% loss, 30ms latency, 10ms jitter, 50 Mbit),
`Mobile3G` (3% loss, 100ms latency, 30ms jitter, 2 Mbit),
`Satellite` (0.5% loss, 600ms latency, 50ms jitter, 10 Mbit).

### Custom link conditions

```rust
use patchbay::{LinkCondition, LinkLimits};

let terrible_wifi = LinkCondition::Manual(LinkLimits {
    rate_kbit: 1000,    // 1 Mbit/s
    loss_pct: 10.0,     // 10% packet loss
    latency_ms: 50,     // 50ms delay
    jitter_ms: 20,      // 20ms jitter
    ..Default::default()
});

let dev = lab.add_device("laptop")
    .iface("eth0", home.id(), Some(terrible_wifi))
    .build().await?;
```

### Dynamic link conditions

Change link conditions at runtime to simulate network degradation:

```rust
// Degrade the link
dev.set_link_condition("eth0", Some(LinkCondition::Mobile3G)).await?;

// Remove all impairment
dev.set_link_condition("eth0", None).await?;
```

## Regions

Regions add configurable latency between groups of routers, simulating
cross-continent or cross-datacenter delays.

```rust
let eu = lab.add_region("eu").await?;
let us = lab.add_region("us").await?;
lab.link_regions(&eu, &us, RegionLink::good(80)).await?;

let dc_eu = lab.add_router("dc-eu").region(&eu).build().await?;
let dc_us = lab.add_router("dc-us").region(&us).build().await?;
// Traffic between dc-eu and dc-us now carries 80ms added latency.
```

Under the hood, each region gets a router namespace. Traffic between
routers in different regions flows through the region routers, which
apply the configured delay via `tc netem`.

### Fault injection

Break and restore region links at runtime for partition testing:

```rust
lab.break_region_link(&eu, &us).await?;
// Traffic between EU and US is now blackholed.

lab.restore_region_link(&eu, &us).await?;
// Connectivity restored.
```

This is useful for testing how your application handles network
partitions, failover, and reconnection.
