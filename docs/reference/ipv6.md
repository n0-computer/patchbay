# Real-World IPv6 Deployments

How IPv6 works in practice and how to simulate each scenario in patchbay.

---

## How ISPs Actually Deploy IPv6

### Residential (FTTH, Cable, DSL)

ISPs assign a **globally routable prefix** (typically /56 or /60) via
DHCPv6-PD (Prefix Delegation). The CE (Customer Edge) router carves /64s
from this prefix for each LAN segment. Devices get **public GUA addresses**
— no NAT involved. The security boundary is a **stateful firewall** on the
CE router that blocks unsolicited inbound connections (RFC 6092).

IPv4 access is provided in parallel via dual-stack (separate IPv4 address
with NAT44) or via DS-Lite / MAP-E / MAP-T (IPv4-in-IPv6 tunneling to the
ISP's AFTR).

**Key properties:**
- Devices have globally routable IPv6 addresses
- No IPv6 NAT — the prefix is public
- Stateful firewall blocks inbound, allows outbound + established
- SLAAC for address assignment (not DHCPv6 address assignment)
- Privacy extensions (RFC 4941) rotate source addresses

**Carriers:** Deutsche Telekom, Comcast, AT&T, Orange, BT, NTT.

### Mobile (4G/5G)

Each device typically gets a **single /64** via RA (Router Advertisement).
The device is the only host on its /64. There is no home router — the
carrier's gateway acts as the first hop.

For IPv4 access, carriers use either:
- **464XLAT** (RFC 6877): CLAT on device + NAT64 on carrier gateway
- **NAT64 + DNS64**: carrier synthesizes AAAA records from A records

Some carriers (T-Mobile US, Jio) are IPv6-only with NAT64. Others
(Verizon, NTT Docomo) do dual-stack.

**Key properties:**
- One /64 per device (not shared)
- NAT64/DNS64 for IPv4 access (no real IPv4 address)
- No firewall — carrier relies on per-device /64 isolation
- 3GPP CGNAT for remaining IPv4 users

### Enterprise / Corporate

Enterprises typically run dual-stack internally with PA (Provider
Aggregatable) or PI (Provider Independent) space. Strict firewalls allow
only TCP 80/443 and UDP 53 outbound. All other ports are blocked —
STUN/TURN on non-standard ports fails, must use TURN-over-TLS on 443.

Some enterprises use ULA (fd00::/8) internally with NAT66 at the border,
though this is discouraged by RFC 4864 and IETF best practices.

### Hotel / Airport / Guest WiFi

After captive portal authentication, these networks typically allow:
- TCP 80, 443 (HTTP/HTTPS)
- TCP/UDP 53 (DNS)
- All other UDP blocked (kills QUIC, STUN, direct P2P)
- TCP to other ports sometimes allowed (unlike corporate)

Many guest networks are still IPv4-only. Those with IPv6 usually provide
GUA addresses with a restrictive firewall.

---

## ULA + NAT66: Mostly a Myth

RFC 4193 ULA (fd00::/8) was designed for stable internal addressing, not
as an IPv6 equivalent of RFC 1918. In practice:

- **No major ISP deploys NAT66** — it defeats the end-to-end principle
- Android **does not support NAT66** (no DHCPv6 client, only SLAAC)
- ULA is used alongside GUA for stable internal addressing, never alone
- RFC 6296 NPTv6 (prefix translation) exists but is niche — mostly
  for multihoming, not general NAT

If you need to simulate "NATted IPv6", use NPTv6 (`NatV6Mode::Nptv6`)
which does stateless 1:1 prefix translation. But understand this is rare
in the real world.

---

## Simulating Real-World Scenarios in Patchbay

### Using Router Presets

[`RouterPreset`] configures NAT, firewall, IP support, and address pool in
one call. Individual methods override preset values when called after
`preset()`.

```rust
// One-liner for each common case:
let home = lab.add_router("home").preset(RouterPreset::Home).build().await?;
let dc   = lab.add_router("dc").preset(RouterPreset::Datacenter).build().await?;
let corp = lab.add_router("corp").preset(RouterPreset::Corporate).build().await?;

// Override one knob:
let home = lab.add_router("home")
    .preset(RouterPreset::Home)
    .nat(Nat::FullCone)   // swap NAT type, keep everything else
    .build().await?;
```

| Preset | NAT | NAT v6 | Firewall | IP | Pool |
|--------|-----|--------|----------|----|------|
| `Home` | Home (EIM+APDF) | None | BlockInbound | DualStack | Private |
| `Datacenter` | None | None | None | DualStack | Public |
| `IspV4` | None | None | None | V4Only | Public |
| `Mobile` | Cgnat | None | BlockInbound | DualStack | Public |
| `MobileV6` | None | **Nat64** | BlockInbound | V6Only | Public |
| `Corporate` | Corporate (sym) | None | Corporate | DualStack | Public |
| `Hotel` | Corporate (sym) | None | CaptivePortal | V4Only | Private |
| `Cloud` | CloudNat | None | None | DualStack | Public |

### Scenario 1: Residential Dual-Stack (Most Common)

A home router with NATted IPv4 and public IPv6. The CE router firewall
blocks unsolicited inbound on both families.

```rust
let home = lab.add_router("home").preset(RouterPreset::Home).build().await?;
let laptop = lab.add_device("laptop").uplink(home.id()).build().await?;
// laptop.ip()  → 10.0.x.x (private IPv4, NATted)
// laptop.ip6() → fd10:0:x::2 (ULA v6, firewalled)
```

### Scenario 2: IPv6-Only Mobile with NAT64

A carrier network where devices only have IPv6. IPv4 destinations are
reached via NAT64 — a userspace SIIT translator on the router converts
between IPv6 and IPv4 headers using the well-known prefix `64:ff9b::/96`.

```rust
let carrier = lab.add_router("carrier")
    .preset(RouterPreset::MobileV6)
    .build().await?;
let phone = lab.add_device("phone").uplink(carrier.id()).build().await?;
// phone.ip6() → 2001:db8:1:x::2 (public GUA)
// phone.ip()  → None (no IPv4 on the device)

// Reach an IPv4 server via NAT64:
use patchbay::nat64::embed_v4_in_nat64;
let nat64_addr = embed_v4_in_nat64(server_v4_ip);
// Connect to [64:ff9b::<server_v4>]:port — translated to IPv4 by the router
```

The `MobileV6` preset configures: `IpSupport::V6Only` + `NatV6Mode::Nat64`
\+ `Firewall::BlockInbound` + public GUA pool. You can also configure NAT64
manually on any router:

```rust
let carrier = lab.add_router("carrier")
    .ip_support(IpSupport::DualStack)  // or V6Only
    .nat_v6(NatV6Mode::Nat64)
    .build().await?;
```

### Scenario 3: Corporate Firewall (Restrictive)

Enterprise network that blocks everything except web traffic. STUN/ICE
fails — P2P apps must fall back to TURN-over-TLS on port 443.

```rust
let corp = lab.add_router("corp").preset(RouterPreset::Corporate).build().await?;
let workstation = lab.add_device("ws").uplink(corp.id()).build().await?;
```

### Scenario 4: Hotel / Captive Portal

Guest WiFi that allows web traffic but blocks most UDP.

```rust
let hotel = lab.add_router("hotel").preset(RouterPreset::Hotel).build().await?;
let guest = lab.add_device("guest").uplink(hotel.id()).build().await?;
```

### Scenario 5: Mobile Carrier (CGNAT + Dual-Stack)

Multiple subscribers sharing a single public IPv4 address. Common on
mobile and some fixed-line ISPs.

```rust
let carrier = lab.add_router("carrier").preset(RouterPreset::Mobile).build().await?;
let phone = lab.add_device("phone").uplink(carrier.id()).build().await?;
```

### Scenario 6: Peer-to-Peer Connectivity Test Matrix

Test how two peers connect across different network types:

```rust
// Home user: easy NAT, firewalled
let home = lab.add_router("home")
    .preset(RouterPreset::Home)
    .nat(Nat::FullCone)
    .build().await?;
let alice = lab.add_device("alice").uplink(home.id()).build().await?;

// Mobile user: CGNAT
let mobile = lab.add_router("mobile").preset(RouterPreset::Mobile).build().await?;
let bob = lab.add_device("bob").uplink(mobile.id()).build().await?;

// Corporate user: strict firewall
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

Patchbay assigns and exposes link-local IPv6 addresses on IPv6-capable
interfaces.

- Device side: `DeviceIface::ll6()`
- Router side: `RouterIface::ll6()`
- Router snapshots: `Router::iface(name)` and `Router::interfaces()`

Use `ip6()` when you need a global/ULA source or destination. Use `ll6()`
for neighbor/router-local checks and link-local route assertions.

### Provisioning mode and DAD mode

Configure IPv6 behavior at lab creation with `LabOpts`:

```rust
let lab = Lab::with_opts(
    LabOpts::default()
        .ipv6_provisioning_mode(Ipv6ProvisioningMode::Static)
        .ipv6_dad_mode(Ipv6DadMode::Enabled),
).await?;
```

- `Ipv6ProvisioningMode::Static`: patchbay installs routes during wiring.
- `Ipv6ProvisioningMode::RaDriven`: enables RA-driven provisioning path.
- `Ipv6DadMode::Disabled`: deterministic mode, current default.
- `Ipv6DadMode::Enabled`: kernel DAD behavior in namespaces.

### Scoped default route behavior

When an IPv6 default gateway is link-local (`fe80::/10`), route installation
must include interface scope. Patchbay uses scoped route installation for this
path, so default routing remains valid after interface changes.

---

## Common Pitfalls

### NPTv6 and NDP

NPTv6 `dnat prefix to` rules must include address match clauses (e.g.,
`ip6 daddr <wan_prefix>`) to avoid translating NDP packets. Without this,
neighbor discovery breaks and the router becomes unreachable.

### IPv6 Firewall Is Not Optional

On IPv4, NAT implicitly blocks inbound connections (no port mapping = no
access). On IPv6 with public GUA addresses, there is **no NAT** — devices
are directly addressable. Without `Firewall::BlockInbound`, any host on
the IX can connect to your devices. This matches reality: every CE router
ships with an IPv6 stateful firewall enabled by default.

### On-Link Prefix Confusion

When IX-level routers share a /64 IX prefix, their WAN addresses are
on-link with each other. Routing prefixes carved from the IX range can
cause "on-link" confusion where packets are sent directly (ARP/NDP) rather
than via the gateway. Use distinct prefixes for IX (/64) and downstream
pools (/48 from a different range).
