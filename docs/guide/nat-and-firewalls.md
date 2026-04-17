# NAT and Firewalls

patchbay implements NAT and firewalls using nftables rules injected into
router namespaces. Because these are real kernel-level packet processing
rules, they behave identically to their counterparts on physical hardware.
This chapter covers all available NAT modes, firewall presets, custom
configurations, and runtime mutation.

## IPv4 NAT

NAT controls how a router translates addresses for traffic flowing
between its downstream (private) and upstream (public) interfaces. Two
independent properties define how a NAT behaves, both specified by
RFC 4787.

*Mapping* determines how the router assigns external ports. With
endpoint-independent mapping, the router reuses the same external port
for all destinations. A device that binds port 40000 and sends to a
STUN server gets mapped to, say, external port 40000. When it then
sends to a different host, the mapping stays the same. This is what
makes UDP hole-punching possible: a peer learns the mapped address via
STUN, shares it with another peer, and the mapping holds regardless of
who sends to it. With endpoint-dependent mapping, each new destination
gets a different external port, so the address learned from STUN is
useless for other peers.

*Filtering* determines which inbound packets the router forwards.
Endpoint-independent filtering (full cone) accepts packets from any
external host, as long as a mapping exists. Address-dependent filtering
accepts packets from any port on an address the internal device has
already contacted. Address-and-port-dependent filtering only forwards
packets from the exact address and port the internal device has
already contacted; unsolicited packets are dropped even if the port is
mapped.

You configure NAT on the router builder with `.nat()`:

```rust
use patchbay::Nat;

let home = lab.add_router("home").nat(Nat::Easy).build().await?;
```

The `Nat` enum forms a gradient of hole-punching difficulty. Each variant
combines a mapping, filtering, and port-preservation choice to match a
real-world device class:

| Mode | Mapping | Filtering | Port allocation | Real-world model |
|------|---------|-----------|-----------------|------------------|
| `None` | n/a | n/a | n/a | Datacenter, public IPs |
| `Easiest` | Endpoint-independent | Endpoint-independent | Preserve | Full-cone router with UPnP or static forwarding |
| `Easy` | Endpoint-independent | Address-and-port-dependent | Preserve | Standard home WiFi router |
| `Hard` | Endpoint-dependent | Address-and-port-dependent | Preserve | Mobile carrier, symmetric CGNAT, port-predictable symmetric NAT |
| `Hardest` | Endpoint-dependent | Address-and-port-dependent | Random | Enterprise gateway, AWS/Azure/GCP NAT Gateway |

For a deep dive into how these modes are implemented in nftables and how
hole-punching works across them, see the
[NAT Hole-Punching](../reference/holepunching.md) reference.

### Custom NAT configurations

When the presets do not match your scenario, you can build a `NatConfig`
directly and choose the mapping, filtering, and timeout behavior
independently:

```rust
use patchbay::nat::{NatConfig, NatMapping, NatFiltering};

let custom = NatConfig::builder()
    .mapping(NatMapping::EndpointIndependent)
    .filtering(NatFiltering::EndpointIndependent)
    .hairpin(true)
    .build();

let router = lab.add_router("custom").nat(custom).build().await?;
```

### Changing NAT at runtime

You can switch a router's NAT mode after the topology is built. This is
useful for testing how your application reacts when the NAT environment
changes mid-session, for example simulating a network migration. Call
`flush_nat_state()` afterward to clear stale conntrack entries so that
new connections use the updated rules:

```rust
router.set_nat_mode(Nat::Hardest).await?;
router.flush_nat_state().await?;
```

## IPv6 NAT

IPv6 NAT is configured separately from IPv4 using `.nat_v6()`. In most
real-world deployments, IPv6 does not use NAT at all: devices receive
globally routable addresses and a stateful firewall handles inbound
filtering. patchbay defaults to this behavior. For the scenarios where
IPv6 NAT does exist in practice, four modes are available:

```rust
use patchbay::NatV6Mode;

let router = lab.add_router("r")
    .ip_support(IpSupport::DualStack)
    .nat_v6(NatV6Mode::Nptv6)
    .build().await?;
```

| Mode | Description |
|------|-------------|
| `None` | No IPv6 NAT. Devices get globally routable addresses. This is the default and the most common real-world configuration. |
| `Nat64` | Stateless IP/ICMP Translation (RFC 6145). Allows IPv6-only devices to reach IPv4 hosts through the well-known prefix `64:ff9b::/96`. The most important v6 NAT mode in practice; used by major mobile carriers. |
| `Nptv6` | Network Prefix Translation (RFC 6296). Performs stateless 1:1 prefix mapping at the border, preserving end-to-end connectivity while hiding internal prefixes. |
| `Masquerade` | IPv6 masquerade, analogous to IPv4 NAPT. Rare in production but useful for testing applications that must handle v6 address rewriting. |

### NAT64

NAT64 is the mechanism that lets IPv6-only mobile networks (T-Mobile US,
Jio, NTT Docomo) provide IPv4 connectivity. The router runs a userspace
SIIT translator that rewrites packet headers between IPv6 and IPv4.
When an IPv6-only device sends a packet to an address in the `64:ff9b::/96`
prefix, the translator extracts the embedded IPv4 address, rewrites the
headers, and forwards the packet as IPv4. Return traffic is translated
back to IPv6.

You can configure NAT64 explicitly or use the `IspV6` preset, which
sets up a V6Only router with NAT64 and an inbound firewall, matching the
configuration of a typical mobile carrier gateway:

```rust
use patchbay::{IpSupport, NatV6Mode, Nat, RouterPreset};

// Explicit configuration:
let carrier = lab
    .add_router("carrier")
    .ip_support(IpSupport::V6Only)
    .nat_v6(NatV6Mode::Nat64)
    .firewall(Firewall::BlockInbound)
    .build()
    .await?;

// Or equivalently, using the preset:
let carrier = lab
    .add_router("carrier")
    .preset(RouterPreset::IspV6)
    .build()
    .await?;
```

To reach an IPv4 server from an IPv6-only device, embed the server's IPv4
address in the NAT64 prefix using the `embed_v4_in_nat64` helper:

```rust
use patchbay::nat64::embed_v4_in_nat64;

let server_v4: Ipv4Addr = dc.uplink_ip().unwrap();
let nat64_addr = embed_v4_in_nat64(server_v4);
// nat64_addr is 64:ff9b::<v4 octets>, e.g. 64:ff9b::cb00:710a

let target = SocketAddr::new(IpAddr::V6(nat64_addr), 8080);
// Connecting to this address goes through the NAT64 translator.
```

The [IPv6 Deployments](../reference/ipv6.md) reference covers how real
carriers deploy NAT64 and how to simulate each scenario in patchbay.

## Firewalls

Firewall presets control which traffic a router allows in each direction.
They are independent of NAT: a router can have a firewall without NAT
(common for datacenter servers behind a stateful firewall), NAT without a
firewall, or both.

```rust
use patchbay::Firewall;

let corp = lab.add_router("corp")
    .firewall(Firewall::Corporate)
    .build().await?;
```

The following presets are available:

| Preset | Inbound policy | Outbound policy |
|--------|----------------|-----------------|
| `None` | All traffic allowed | All traffic allowed |
| `BlockInbound` | Block unsolicited connections (RFC 6092 CE router behavior) | All traffic allowed |
| `Corporate` | Block unsolicited connections | Allow only TCP 80, 443 and UDP 53 |
| `CaptivePortal` | Block unsolicited connections | Allow only TCP 80, 443 and UDP 53; block all other UDP |

The `Corporate` and `CaptivePortal` presets are particularly useful for
testing P2P applications: corporate firewalls block STUN and direct UDP,
forcing applications to fall back to TURN relaying over TLS on port 443.
Captive portal firewalls additionally kill QUIC by blocking all
non-DNS UDP.

### Custom firewall rules

When the presets do not match your test scenario, build a `FirewallConfig`
directly:

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

## Composing NAT and firewalls

NAT and firewalls are orthogonal. A router can have any combination of the
two, and they operate at different points in the nftables pipeline. Some
typical compositions:

```rust
// Home router: NAT + inbound firewall. The most common residential setup.
let home = lab.add_router("home")
    .nat(Nat::Easy)
    .firewall(Firewall::BlockInbound)
    .build().await?;

// Datacenter with strict outbound rules but no NAT.
let dc = lab.add_router("dc")
    .firewall(Firewall::Corporate)
    .build().await?;

// Double NAT: ISP carrier-grade NAT in front of a home router. The
// `IspCgnat` preset gives realistic defaults (EIM + APDF, v6 inbound
// blocked); use `.nat(Nat::Easiest)` if you specifically want to model
// a full-cone CGNAT.
let isp = lab.add_router("isp").preset(RouterPreset::IspCgnat).build().await?;
let home = lab.add_router("home")
    .preset(RouterPreset::Home)
    .upstream(isp.id())
    .build().await?;
```

Router presets set both NAT and firewall to sensible defaults for each
deployment pattern. Calling individual methods after `.preset()` overrides
the preset's defaults, so you can start from a known configuration and
adjust only what your test needs.
