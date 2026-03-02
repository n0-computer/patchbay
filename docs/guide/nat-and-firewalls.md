# NAT and Firewalls

patchbay implements NAT and firewalls using nftables rules injected into
router namespaces. This gives you kernel-level packet processing that
behaves identically to real hardware.

## IPv4 NAT modes

Set the NAT mode on a router with `.nat()`:

```rust
use patchbay::Nat;

let home = lab.add_router("home").nat(Nat::Home).build().await?;
```

Available modes:

| Mode | Mapping | Filtering | Typical use |
|------|---------|-----------|-------------|
| `None` | (no NAT) | (no NAT) | Datacenter, public IPs |
| `Home` | Endpoint-independent | Endpoint-dependent | Home WiFi router |
| `Corporate` | Endpoint-independent | Endpoint-dependent | Enterprise gateway |
| `FullCone` | Endpoint-independent | Endpoint-independent | Gaming, fullcone VPN |
| `CloudNat` | Endpoint-dependent | Endpoint-dependent | AWS/GCP cloud NAT |
| `Cgnat` | Endpoint-dependent | Endpoint-dependent | Carrier-grade NAT (ISP) |

**Mapping** controls how external ports are assigned. Endpoint-independent
mapping reuses the same external port for all destinations, which is what
makes UDP hole-punching work.

**Filtering** controls which inbound packets get forwarded.
Endpoint-independent filtering (fullcone) lets any host send to a mapped
port. Endpoint-dependent filtering only forwards replies from hosts you
already contacted.

For a deep dive into the nftables implementation and hole-punching
mechanics, see [NAT Hole-Punching](../reference/holepunching.md).

### Custom NAT

For fine-grained control, build a `NatConfig` directly:

```rust
use patchbay::nat::{NatConfig, NatMapping, NatFiltering};

let custom = Nat::Custom(NatConfig {
    mapping: NatMapping::EndpointIndependent,
    filtering: NatFiltering::EndpointIndependent,
    ..Default::default()
});

let router = lab.add_router("custom")
    .nat(custom)
    .build().await?;
```

### Changing NAT at runtime

Switch NAT mode and flush connection tracking state:

```rust
router.set_nat_mode(Nat::Corporate).await?;
router.flush_nat_state().await?;
```

## IPv6 NAT modes

IPv6 NAT is configured separately from IPv4 NAT using `.nat_v6()`:

```rust
use patchbay::NatV6Mode;

let router = lab.add_router("r")
    .ip_support(IpSupport::DualStack)
    .nat_v6(NatV6Mode::Nptv6)
    .build().await?;
```

| Mode | What it does |
|------|-------------|
| `None` | No IPv6 NAT (default). Devices get globally routable addresses. |
| `Nptv6` | NPTv6 prefix translation (RFC 6296). 1:1 stateless mapping. |
| `Masquerade` | IPv6 masquerade (like IPv4 NAPT). Rare in practice but useful for testing. |
| `Nat64` | SIIT translation (RFC 6145). IPv6-only devices reach IPv4 hosts via `64:ff9b::/96`. |

### NAT64

NAT64 lets IPv6-only devices communicate with IPv4 servers. The router
runs a userspace SIIT translator that converts between IPv6 and IPv4
headers. Packets addressed to `64:ff9b::<ipv4-addr>` are translated and
forwarded to the embedded IPv4 address.

```rust
use patchbay::{IpSupport, NatV6Mode, Nat};

let carrier = lab
    .add_router("carrier")
    .ip_support(IpSupport::DualStack)
    .nat(Nat::Home)
    .nat_v6(NatV6Mode::Nat64)
    .build()
    .await?;

// Or use the MobileV6 preset which sets this up automatically:
let carrier = lab
    .add_router("carrier")
    .preset(RouterPreset::MobileV6)
    .build()
    .await?;
```

To reach a v4 server from an IPv6-only device, embed the server's IPv4
address in the NAT64 prefix:

```rust
use patchbay::nat64::embed_v4_in_nat64;

let server_v4: Ipv4Addr = "203.0.113.10".parse()?;
let nat64_addr = embed_v4_in_nat64(server_v4);
// nat64_addr = 64:ff9b::cb00:710a
// Connect to this address and the NAT64 router translates it to 203.0.113.10
```

See [IPv6 Deployments](../reference/ipv6.md) for how real carriers
deploy NAT64 and how to simulate each scenario.

## Firewalls

Firewall presets control both inbound and outbound traffic on a router:

```rust
use patchbay::Firewall;

let corp = lab.add_router("corp")
    .firewall(Firewall::Corporate)
    .build().await?;
```

| Preset | Inbound | Outbound |
|--------|---------|----------|
| `None` | All allowed | All allowed |
| `BlockInbound` | Block unsolicited (RFC 6092 CE) | All allowed |
| `Corporate` | Block unsolicited | TCP 80, 443 + UDP 53 only |
| `CaptivePortal` | Block unsolicited | TCP 80, 443 + UDP 53 only (all other UDP blocked) |

### Custom firewalls

Build a `FirewallConfig` for precise control:

```rust
use patchbay::firewall::FirewallConfig;

let config = FirewallConfig::builder()
    .block_inbound(true)
    .allow_tcp_ports(&[80, 443, 8080])
    .allow_udp_ports(&[53, 443])
    .build();

let router = lab.add_router("strict")
    .firewall(Firewall::Custom(config))
    .build().await?;
```

## Combining NAT and firewalls

NAT and firewalls are independent. A router can have any combination:

```rust
// NAT + firewall (typical home router)
let home = lab.add_router("home")
    .nat(Nat::Home)
    .firewall(Firewall::BlockInbound)
    .build().await?;

// Firewall only, no NAT (datacenter with strict rules)
let dc = lab.add_router("dc")
    .firewall(Firewall::Corporate)
    .build().await?;

// Double NAT (ISP CGNAT + home NAT)
let isp = lab.add_router("isp").nat(Nat::Cgnat).build().await?;
let home = lab.add_router("home")
    .upstream(isp.id())
    .nat(Nat::Home)
    .build().await?;
```

Router presets set both NAT and firewall to sensible defaults for each
scenario. Call individual methods after `.preset()` to override specific
settings.
