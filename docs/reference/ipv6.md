# Real-World IPv6 Deployments

IPv6 deployment varies widely across ISPs, carriers, and enterprises.
The differences matter for testing: a P2P application that works over a
residential dual-stack connection may fail on a corporate network that
blocks non-web UDP, or on a mobile carrier that assigns only IPv6
addresses and translates IPv4 traffic through NAT64. This page explains
how each environment works and how to reproduce it in patchbay.

---

## IPv6 Terms Used Here

A few IPv6 terms appear throughout this page:

- *GUA* (Global Unicast Address) — a publicly routable address, the IPv6
  equivalent of a public IPv4 address. Devices with GUAs are reachable
  from anywhere on the internet unless a firewall intervenes.
- *ULA* (Unique Local Address) — an address in `fd00::/8`, routable only
  within a site. Analogous to RFC 1918 private IPv4 space, but rarely
  used as the sole address family.
- *Link-local address* — an address in `fe80::/10`, valid only on the
  directly connected link. Every IPv6 interface has one. Used for
  neighbor discovery, router solicitation, and as next-hop addresses in
  routing tables.
- *SLAAC* (Stateless Address Autoconfiguration) — the mechanism by which
  a host picks its own address from a prefix advertised by a router. No
  DHCP server involved.
- *RA* (Router Advertisement) — a message a router sends to announce its
  presence, the prefix it serves, and default-route information.
- *RS* (Router Solicitation) — a message a host sends to ask nearby
  routers to send an RA immediately instead of waiting for the next
  periodic one.
- *DAD* (Duplicate Address Detection) — a probe the kernel sends before
  using an address, to verify no other host on the link already claims
  it.

---

## How ISPs Actually Deploy IPv6

### Residential (FTTH, Cable, DSL)

The ISP assigns the home router a globally routable prefix — typically a
/56 or /60 — via DHCPv6 Prefix Delegation (DHCPv6-PD). The home router
carves /64 subnets from this prefix, one per LAN segment, and announces
them via Router Advertisements. Devices on the LAN run SLAAC to pick
their own addresses within the /64. The result is that every device gets
a public, globally routable IPv6 address with no NAT involved.

The security boundary is a stateful firewall on the home router (the CE
router in RFC 6092 terms). It blocks unsolicited inbound connections
while allowing outbound traffic and replies to established sessions.
This firewall is what prevents the outside world from reaching devices
directly despite their public addresses. Privacy extensions (RFC 4941)
rotate the source address periodically so that outbound connections do
not reveal a stable device identifier.

IPv4 access runs in parallel, either via a separate IPv4 address with
traditional NAT44, or via transition mechanisms like DS-Lite, MAP-E, or
MAP-T that tunnel IPv4 inside IPv6 to the ISP's gateway.

Carriers that deploy this model include Deutsche Telekom, Comcast, AT&T,
Orange, BT, and NTT.

### Mobile (4G/5G)

Mobile carriers assign each device a single /64 prefix via Router
Advertisement. The device is the only host on its /64 — there is no
home router between the device and the carrier gateway. This means the
carrier gateway is the first IP hop, and it controls all routing and
policy.

For IPv4 connectivity, carriers take one of two approaches. Some run
pure IPv6 with NAT64: the device has no IPv4 address at all, and the
carrier gateway translates IPv4-bound traffic using the well-known
prefix `64:ff9b::/96`. DNS64 synthesizes AAAA records so applications
connect to IPv6 addresses that the gateway maps back to IPv4. T-Mobile
US and Jio operate this way. Other carriers like Verizon and NTT Docomo
run dual-stack, giving devices both IPv4 (often behind CGNAT) and IPv6
addresses.

Mobile networks typically do not run per-device firewalls. Instead, they
rely on the fact that each device has its own /64 prefix, which provides
natural isolation — no other subscriber shares the prefix.

### Enterprise / Corporate

Enterprises typically run dual-stack internally using provider-allocated
(PA) or provider-independent (PI) address space. The defining
characteristic is a strict outbound firewall: only TCP 80/443 and UDP 53
are allowed. All other ports are blocked, which means STUN and TURN on
non-standard ports fail. Applications that need relay connectivity must
use TURN-over-TLS on port 443.

Some enterprises use ULA (`fd00::/8`) internally with NAT66 at the
border, though this is discouraged by RFC 4864 and IETF best practices.
See the section on ULA + NAT66 below.

### Hotel / Airport / Guest WiFi

After captive portal authentication, guest networks allow web traffic
(TCP 80 and 443) and DNS (TCP/UDP 53) but block most other UDP. This
kills QUIC, STUN, and direct P2P connectivity. Unlike corporate
networks, some guest networks allow TCP on non-standard ports, but this
varies. Many guest networks are still IPv4-only. Those that offer IPv6
assign GUA addresses behind a restrictive firewall.

---

## ULA + NAT66: Mostly a Myth

RFC 4193 ULA (`fd00::/8`) was designed for stable internal addressing,
not as an IPv6 equivalent of RFC 1918 private space. No major ISP
deploys NAT66 — it defeats the end-to-end principle that IPv6 was
designed to restore. Android does not support NAT66 at all because it
lacks a DHCPv6 client and relies entirely on SLAAC. Where ULA appears
in practice, it is used alongside GUA for stable internal service
addresses, never as the sole address family.

RFC 6296 NPTv6 (Network Prefix Translation) does exist for stateless
1:1 prefix mapping at site borders, primarily for multihoming. If you
need to simulate "NATted IPv6" in patchbay, use `NatV6Mode::Nptv6`,
but understand that this configuration is rare in production.

---

## Simulating Real-World Scenarios in Patchbay

Each ISP deployment model described above maps to a patchbay router
configuration. `RouterPreset` captures the most common combinations in
a single call, and individual builder methods let you override any
default when your test scenario diverges from the preset.

```rust
// One-liner for each common case:
let home = lab.add_router("home").preset(RouterPreset::Home).build().await?;
let dc   = lab.add_router("dc").preset(RouterPreset::Public).build().await?;
let corp = lab.add_router("corp").preset(RouterPreset::Corporate).build().await?;

// Override one knob:
let home = lab.add_router("home")
    .preset(RouterPreset::Home)
    .nat(Nat::Easiest)   // swap NAT type, keep everything else
    .build().await?;
```

The full preset table:

| Preset | NAT | NAT v6 | Firewall | IP | Pool |
|--------|-----|--------|----------|----|------|
| `Home` | Home (EIM+APDF) | None | BlockInbound | DualStack | Private |
| `Public` | None | None | None | DualStack | Public |
| `PublicV4` | None | None | None | V4Only | Public |
| `IspCgnat` | Cgnat (EIM+EIF) | None | None | DualStack | Private |
| `IspV6` | None | **Nat64** | BlockInbound | V6Only | Public |
| `Corporate` | Corporate (sym) | None | Corporate | DualStack | Private |
| `Hotel` | Corporate (sym) | None | CaptivePortal | V4Only | Private |
| `Cloud` | CloudNat (sym) | None | None | DualStack | Private |

### Scenario 1: Residential Dual-Stack (Most Common)

Most residential connections today are dual-stack: IPv4 behind NAT, IPv6
with public addresses behind a stateful firewall. This is the baseline
for testing home-user connectivity. Applications using Happy Eyeballs
(RFC 8305) will prefer IPv6 when both families are available.

```rust
let home = lab.add_router("home").preset(RouterPreset::Home).build().await?;
let laptop = lab.add_device("laptop").uplink(home.id()).build().await?;
// laptop.ip()  -> 10.0.x.x (private IPv4, NATted)
// laptop.ip6() -> fd10:0:x::2 (ULA v6, firewalled)
```

### Scenario 2: IPv6-Only Mobile with NAT64

T-Mobile US, Jio, and other large carriers run IPv6-only networks. Your
application receives no IPv4 address. To reach an IPv4 server, the
carrier gateway translates between IPv6 and IPv4 using the well-known
prefix `64:ff9b::/96`: the device connects to an IPv6 address that
embeds the IPv4 destination, and the gateway rewrites the headers.

This is one of the most important scenarios to test against, because it
breaks applications that hardcode IPv4 addresses or assume a dual-stack
environment.

```rust
let carrier = lab.add_router("carrier")
    .preset(RouterPreset::IspV6)
    .build().await?;
let phone = lab.add_device("phone").uplink(carrier.id()).build().await?;
// phone.ip6() -> 2001:db8:1:x::2 (public GUA)
// phone.ip()  -> None (no IPv4 on the device)

// Reach an IPv4 server via NAT64:
use patchbay::nat64::embed_v4_in_nat64;
let nat64_addr = embed_v4_in_nat64(server_v4_ip);
// Connect to [64:ff9b::<server_v4>]:port, translated to IPv4 by the router
```

The `IspV6` preset configures `IpSupport::V6Only`,
`NatV6Mode::Nat64`, `Firewall::BlockInbound`, and a public GUA pool.
You can also configure NAT64 manually on any router when you need a
different combination:

```rust
let carrier = lab.add_router("carrier")
    .ip_support(IpSupport::DualStack)  // or V6Only
    .nat_v6(NatV6Mode::Nat64)
    .build().await?;
```

### Scenario 3: Corporate Firewall (Restrictive)

Enterprise networks block everything except web traffic. STUN binding
requests on non-standard ports are silently dropped, so ICE candidates
never resolve. P2P applications must detect this and fall back to
TURN-over-TLS on port 443 — the only UDP port that survives the
firewall is DNS on 53.

```rust
let corp = lab.add_router("corp").preset(RouterPreset::Corporate).build().await?;
let workstation = lab.add_device("ws").uplink(corp.id()).build().await?;
```

### Scenario 4: Hotel / Captive Portal

Guest WiFi networks allow web browsing but block most UDP, which kills
QUIC and prevents direct P2P connections. The difference from corporate
is that some hotel networks allow TCP on non-standard ports, so
TURN-over-TCP (not just TLS on 443) may work.

```rust
let hotel = lab.add_router("hotel").preset(RouterPreset::Hotel).build().await?;
let guest = lab.add_device("guest").uplink(hotel.id()).build().await?;
```

### Scenario 5: Mobile Carrier (CGNAT + Dual-Stack)

Carriers that still offer IPv4 typically share a single public IPv4
address across many subscribers via CGNAT. The device has both IPv4 and
IPv6, but the IPv4 address is behind carrier-grade NAT — an extra layer
on top of any home NAT.

```rust
let carrier = lab.add_router("carrier").preset(RouterPreset::IspCgnat).build().await?;
let phone = lab.add_device("phone").uplink(carrier.id()).build().await?;
```

### Scenario 6: Peer-to-Peer Connectivity Test Matrix

The real value of these presets is composing them to test how two peers
connect across different network types. A home user behind cone NAT can
hole-punch with another home user, but a corporate user behind a strict
firewall forces a relay fallback. Testing the full matrix catches
connectivity regressions that single-topology tests miss.

```rust
let home = lab.add_router("home")
    .preset(RouterPreset::Home)
    .nat(Nat::Easiest)
    .build().await?;
let alice = lab.add_device("alice").uplink(home.id()).build().await?;

let mobile = lab.add_router("mobile").preset(RouterPreset::IspCgnat).build().await?;
let bob = lab.add_device("bob").uplink(mobile.id()).build().await?;

let corp = lab.add_router("corp").preset(RouterPreset::Corporate).build().await?;
let charlie = lab.add_device("charlie").uplink(corp.id()).build().await?;

// Test: can alice reach bob? bob reach charlie? etc.
```

---

## IPv6 Feature Reference

| Feature | API | Notes |
|---------|-----|-------|
| Dual-stack | `IpSupport::DualStack` | Both v4 and v6 |
| IPv6-only | `IpSupport::V6Only` | No v4 routes |
| IPv4-only | `IpSupport::V4Only` | No v6 routes (default) |
| NPTv6 | `NatV6Mode::Nptv6` | Stateless 1:1 prefix translation |
| NAT66 (masquerade) | `NatV6Mode::Masquerade` | Like NAT44 but for v6 |
| Block inbound | `Firewall::BlockInbound` | RFC 6092 CE router |
| Corporate FW | `Firewall::Corporate` | Block inbound + TCP 80,443 + UDP 53 |
| Captive portal FW | `Firewall::CaptivePortal` | Block inbound + block non-web UDP |
| Custom FW | `Firewall::Custom(cfg)` | Full control via `FirewallConfig` |
| NAT64 | `NatV6Mode::Nat64` | Userspace SIIT + nftables masquerade |
| DHCPv6-PD | *not planned* | Use static /64 allocation |

## Link-Local Addressing and Scope

Every IPv6 interface has a link-local address in `fe80::/10`. Unlike
global or ULA addresses, link-local addresses are valid only on the
directly connected link — they cannot be routed across hops. The kernel
uses them for neighbor discovery (finding other hosts on the link) and
as next-hop addresses in routing tables. They are always present, even
when no global prefix has been assigned.

In patchbay, you can inspect link-local addresses through interface
snapshots:

- Device side: `Iface::ll6()`
- Router side: `RouterIface::ll6()`
- Router snapshots: `Router::iface(name)` and `Router::interfaces()`

Use `ip6()` when you need a global/ULA source or destination. Use
`ll6()` for neighbor/router-local checks and link-local route
assertions.

### Provisioning mode and DAD mode

patchbay supports two IPv6 provisioning modes, configured at lab
creation. The choice controls how IPv6 routes and addresses are set up
in each namespace.

`Ipv6ProvisioningMode::Static` installs routes during topology wiring.
This is the simpler model: routes are deterministic, and there is no
timing dependency on router advertisements. Use this when your test
cares about connectivity and routing outcomes, not about the
provisioning process itself.

`Ipv6ProvisioningMode::RaDriven` models the RA/RS-driven provisioning
path. patchbay emits structured RA and RS events and installs link-local
scoped default routes for default interfaces. This models real host
routing behavior while keeping tests deterministic and introspectable.
Use this when your application depends on RA timing, default-route
installation order, or link-local gateway behavior.

DAD (Duplicate Address Detection) is disabled by default to keep test
setup deterministic — the kernel DAD probe adds a delay before an
address becomes usable, which introduces timing variance. Enable it with
`Ipv6DadMode::Enabled` when you specifically need to test DAD-related
behavior.

```rust
let lab = Lab::with_opts(
    LabOpts::default()
        .ipv6_provisioning_mode(Ipv6ProvisioningMode::Static)
        .ipv6_dad_mode(Ipv6DadMode::Enabled),
).await?;
```

### Fidelity boundaries

patchbay models RA and RS behavior at the control-plane level: it
updates routes and emits structured events in tracing logs, but it does
not emit raw ICMPv6 RA or RS packets on virtual links. Application-level
route and connectivity behavior is covered, but packet-capture workflows
that expect real RA/RS frames are not.

Specific areas outside the model:

- Full SLAAC state-machine behavior across all timers and transitions.
- Neighbor Discovery timing details, including exact probe/retransmit
  timing.
- Host temporary address rotation and privacy-address lifecycles.

For the complete list, see [Limitations](../limitations.md).

### Scoped default route behavior

When an IPv6 default gateway is link-local (`fe80::/10`), the route
must include the outgoing interface as scope — without it, the kernel
does not know which link the gateway lives on. patchbay handles this
automatically during route installation, so default routing remains
valid after interface changes.

---

## Common Pitfalls

### NPTv6 and NDP

NPTv6 `dnat prefix to` rules must include address match clauses (e.g.,
`ip6 daddr <wan_prefix>`) to avoid translating NDP packets. Without
this, neighbor discovery breaks and the router becomes unreachable.

### IPv6 Firewall Is Not Optional

On IPv4, NAT implicitly blocks inbound connections — no port mapping
means no access. On IPv6 with public GUA addresses, there is **no NAT**
and devices are directly addressable from the internet. Without
`Firewall::BlockInbound`, any host on the IX can connect to your
devices. This matches reality: every residential CE router ships with an
IPv6 stateful firewall enabled by default.

### On-Link Prefix Confusion

When IX-level routers share a /64 IX prefix, their WAN addresses are
on-link with each other. If downstream routing prefixes are carved from
the same range, the kernel may treat them as on-link too, sending
packets directly via NDP rather than through the gateway. patchbay
avoids this by using distinct prefix ranges for the IX (/64) and
downstream pools (/48 from a different range).
