# Real-World Network Patterns

Patterns for testing P2P applications against common real-world network
conditions. Each section describes what happens from the application's
perspective and how to simulate it.

---

## VPN Connect / Disconnect

### What happens when a VPN connects

A VPN client performs three operations:

1. **IP change** - A new tunnel interface (wg0, tun0) gets a VPN-assigned
   address. The device now has two IPs: physical and tunnel.

2. **Route change** - For full-tunnel VPNs, a new default route via the tunnel
   is installed. All traffic exits through the VPN server. For split-tunnel,
   only specific CIDRs (corporate ranges) route through the tunnel.

3. **DNS change** - VPN pushes its own DNS servers. Private hostnames become
   resolvable.

**Impact on existing connections**: Existing TCP connections do not automatically
die but break in practice. The source IP that the remote knows is the old
physical IP. After routing changes, outgoing packets exit via the tunnel with a
different source IP. The remote sends responses to the old IP. Connections stall
and eventually time out. QUIC connections can migrate if both sides support it.

### Full-tunnel VPN

All traffic exits through the VPN server. STUN reports the VPN server's public
IP as the reflexive address. Direct connections between two VPN peers go through
two VPN hops.

```rust
// VPN exit node (NATs all clients behind server IP)
let vpn_exit = lab.add_router("vpn-exit")
    .nat(Nat::Moderate)
    .mtu(1420)          // WireGuard overhead
    .build().await?;

// Before VPN: device on home network
let home = lab.add_router("home").nat(Nat::Moderate).build().await?;
let device = lab.add_device("client").uplink(home.id()).build().await?;

// Connect VPN: device moves to VPN router, gets new IP
device.iface("eth0").unwrap().replug(vpn_exit.id()).await?;

// Disconnect VPN: device returns to home router
device.iface("eth0").unwrap().replug(home.id()).await?;
```

### Split-tunnel VPN

Some traffic goes through VPN, rest uses physical interface. Model with two
interfaces on different routers:

```rust
let device = lab.add_device("client")
    .iface("eth0", home.id())      // physical: internet traffic
    .iface("wg0", vpn_exit.id())   // tunnel: corporate traffic
    .default_via("eth0")                  // default route on physical
    .build().await?;

// Corporate server only reachable via VPN
let corp_server = lab.add_device("server").uplink(vpn_exit.id()).build().await?;
// Internet server reachable via physical
let public_server = lab.add_device("relay").uplink(dc.id()).build().await?;

// Switch from split to full tunnel
device.set_default_route("wg0").await?;
// Switch back
device.set_default_route("eth0").await?;
```

### VPN kill switch

A kill switch drops all non-tunnel traffic immediately:

```rust
device.iface("eth0").unwrap().link_down().await?;           // kill switch fires
device.iface("eth0").unwrap().replug(vpn_exit.id()).await?;  // tunnel established
device.iface("eth0").unwrap().link_up().await?;
```

### VPN MTU impact

VPN encapsulation reduces effective MTU. Common values:

| Protocol | Overhead | Inner MTU |
|----------|----------|-----------|
| WireGuard | 60B (v4) / 80B (v6) | 1420 / 1400 |
| OpenVPN UDP | ~50-60B | ~1400 |
| IPsec ESP (NAT-T) | 52-72B | ~1400 |

If ICMP "fragmentation needed" is blocked (common in corporate/cloud), PMTUD
fails silently. Small requests work, large transfers hang.

```rust
// Simulate VPN MTU + PMTUD blackhole
let vpn = lab.add_router("vpn")
    .mtu(1420)
    .block_icmp_frag_needed()  // PMTU blackhole
    .build().await?;
```

---

## NAT Traversal

See [NAT Hole-Punching](holepunching.md) for the full NAT implementation
reference (nftables fullcone map, conntrack behavior, and debugging notes).

### Hole punching (STUN + simultaneous open)

Both peers discover their reflexive address via STUN, exchange it through a
signaling channel, then send UDP probes simultaneously. Each probe creates a
NAT mapping that the peer's probe can traverse.

```rust
// Both behind cone NATs: hole punching works
let nat_a = lab.add_router("nat-a").nat(Nat::Moderate).build().await?;
let nat_b = lab.add_router("nat-b").nat(Nat::Moderate).build().await?;
// Assert: direct connection established

// One side symmetric: hole punching fails, relay needed
let nat_a = lab.add_router("nat-a").nat(Nat::Moderate).build().await?;
let nat_b = lab.add_router("nat-b").nat(Nat::Strict).build().await?;
// Assert: falls back to relay (TURN/DERP)
```

### Double NAT (CGNAT + home router)

The device is behind two NAT layers. STUN returns the outermost public IP.
Port forwarding (UPnP) only works on the home router, not the CGNAT. Hole
punching is more timing-sensitive.

```rust
let cgnat = lab.add_router("cgnat").nat(Nat::Open).build().await?;
let home = lab.add_router("home")
    .upstream(cgnat.id())
    .nat(Nat::Moderate)
    .build().await?;
let device = lab.add_device("client").uplink(home.id()).build().await?;
```

### NAT mapping timeout

After a period of inactivity, NAT mappings expire. The application must send
keepalives to prevent this. Default UDP timeouts vary by NAT type (120-350s).
Test by waiting beyond the timeout period then verifying connectivity.

```rust
// Custom short timeout for fast testing
let nat = lab.add_router("nat")
    .nat(
        NatConfig::builder()
            .mapping(NatMapping::EndpointIndependent)
            .filtering(NatFiltering::AddressAndPortDependent)
            .udp_timeout(5) // seconds, short for testing
            .build()?,
    )
    .build().await?;

// Wait for timeout, verify mapping expired
tokio::time::sleep(Duration::from_secs(6)).await;
router.flush_nat_state().await?;
// Assert: reflexive address changed (new mapping)
```

---

## WiFi to Cellular Handoff

When a device switches from WiFi to cellular, it loses its WiFi IP
address and receives a new one from the cellular carrier. Existing TCP
connections break because the remote peer is sending replies to the old
address. QUIC connections can survive if both sides support connection
migration. In practice there is a 0.5–5 second gap with no connectivity
during the transition while the cellular radio attaches and the new
address is assigned.

```rust
let wifi_router = lab.add_router("wifi").nat(Nat::Moderate).build().await?;
let cell_router = lab
    .add_router("cell")
    .preset(RouterPreset::IspCgnatSymmetric)
    .build().await?;

let device = lab.add_device("phone")
    .iface("eth0", wifi_router.id())
    .build().await?;
device.iface("eth0").unwrap().set_condition(LinkCondition::wifi(), LinkDirection::Both).await?;

// Simulate handoff with connectivity gap
device.iface("eth0").unwrap().link_down().await?;
tokio::time::sleep(Duration::from_millis(500)).await;
device.iface("eth0").unwrap().replug(cell_router.id()).await?;
device.iface("eth0").unwrap().set_condition(LinkCondition::mobile_4g(), LinkDirection::Both).await?;
device.iface("eth0").unwrap().link_up().await?;

// Assert: application reconnects within X seconds
```

---

## Corporate Firewall Blocking UDP

UDP packets are silently dropped. STUN requests time out. ICE falls back
through: UDP direct -> UDP relay (TURN) -> TCP relay -> TLS/TCP relay on 443.

```rust
let corp = lab.add_router("corp")
    .nat(Nat::Strict)
    .firewall(Firewall::Corporate)  // TCP 80,443 + UDP 53 only
    .build().await?;

let workstation = lab.add_device("ws").uplink(corp.id()).build().await?;
// Assert: connection type is Relay, not Direct
// Assert: relay uses TCP/TLS on port 443
```

---

## Asymmetric Bandwidth

Most consumer connections have significantly less upload bandwidth than
download. Residential cable runs around 100/10 Mbps, cellular around
50/10 Mbps, satellite around 100/10 Mbps. The asymmetry matters for P2P
applications because the bottleneck is always the uploader's upload
speed, not their download speed.

The bottleneck for P2P transfers is the uploader's upload speed. For video
calls, each direction is limited by the sender's upload.

```rust
// 20 Mbps down, 2 Mbps up (10:1 ratio)
let router = lab.add_router("isp")
    .nat(Nat::Moderate)
    .downlink_condition(LinkCondition::new().rate_mbit(20))
    .build().await?;

let device = lab.add_device("client").uplink(router.id()).build().await?;
device.iface("eth0").unwrap().set_condition(
    LinkCondition::new().rate_mbit(2),
    LinkDirection::Both,
).await?;
```

---

## IPv6 Transition

See [IPv6 Deployments](ipv6.md) for the full IPv6 deployment reference and
router preset table.

### Dual-stack

Device has both v4 and v6 addresses. Applications using Happy Eyeballs
(RFC 8305) try v6 first. ICE collects both v4 and v6 candidates. Direct v6
connections skip NAT traversal entirely if both peers have public v6 addresses.

```rust
let router = lab.add_router("dual")
    .ip_support(IpSupport::DualStack)
    .nat(Nat::Moderate)
    .build().await?;
```

### v6-only with NAT64

Device has only an IPv6 address. IPv4 destinations are reached via NAT64:
the router translates packets between IPv6 and IPv4 using the well-known
prefix `64:ff9b::/96`. Applications connect to `[64:ff9b::<ipv4>]:port`
and the router handles the rest. ICE candidates are v6 only; TURN must
be dual-stack.

```rust
use patchbay::nat64::embed_v4_in_nat64;

// One-liner: IspV6 preset = V6Only + NAT64 + BlockInbound
let carrier = lab.add_router("carrier")
    .preset(RouterPreset::IspV6)
    .build().await?;
let phone = lab.add_device("phone").uplink(carrier.id()).build().await?;

// Reach an IPv4 server via NAT64:
let nat64_addr = embed_v4_in_nat64(server_v4_ip);
let target = SocketAddr::new(IpAddr::V6(nat64_addr), 443);
```

---

## Captive Portal

The device has L3 connectivity but no internet access. HTTP requests redirect
to the portal. HTTPS and UDP fail. All connection attempts time out.

```rust
// Isolated router with no upstream (simulates pre-auth portal)
let portal = lab.add_router("portal").build().await?;  // no upstream
let device = lab.add_device("victim").uplink(portal.id()).build().await?;

// Assert: all connections fail/timeout

// User "authenticates" - move to real router
device.iface("eth0").unwrap().replug(real_router.id()).await?;

// Assert: connections now succeed
```

---

## DHCP Renewal (IP Change on Same Network)

The device stays on the same network but its IP address changes. This happens
during DHCP lease renewal, cloud instance metadata refresh, or ISP-side
reassignment.

```rust
let old_ip = device.ip();
let new_ip = device.iface("eth0").unwrap().renew_ip().await?;
assert_ne!(old_ip, Some(new_ip));

// Assert: application detects IP change and re-establishes connections
```

---

## Degraded Network Conditions

### Progressive degradation

Network conditions worsen over time (moving away from WiFi AP, entering tunnel
on cellular, weather affecting satellite).

```rust
device.iface("eth0").unwrap().set_condition(LinkCondition::wifi(), LinkDirection::Both).await?;
tokio::time::sleep(Duration::from_secs(5)).await;
device.iface("eth0").unwrap().set_condition(LinkCondition::wifi_bad(), LinkDirection::Both).await?;
tokio::time::sleep(Duration::from_secs(5)).await;
device.iface("eth0").unwrap().clear_condition(LinkDirection::Both).await?;  // remove impairment
```

### Intermittent connectivity

Network flaps briefly, simulating tunnels, elevators, or brief signal loss.

```rust
for _ in 0..3 {
    device.iface("eth0").unwrap().link_down().await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    device.iface("eth0").unwrap().link_up().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
}
// Assert: application recovers after each flap
```

---

## Simulator Primitive Reference

| Real-World Event | Simulator Primitive |
|---|---|
| VPN connects (full tunnel) | `dev.iface("eth0").unwrap().replug(vpn_router.id())` |
| VPN disconnects | `dev.iface("eth0").unwrap().replug(original_router.id())` |
| VPN kill switch | `iface.link_down()` then `iface.replug()` |
| VPN split tunnel | Two interfaces on different routers + `set_default_route` |
| WiFi to cellular | `iface.replug()` + `iface.set_condition()` |
| Network goes down briefly | `iface.link_down()`, sleep, `iface.link_up()` |
| Cone NAT | `Nat::Moderate` |
| Symmetric NAT | `Nat::Strict` |
| Double NAT / CGNAT | Chain routers: `home.upstream(cgnat.id())` |
| Corporate UDP block | `Firewall::Corporate` on router |
| Captive portal | Router with no upstream |
| DHCP renewal | `dev.iface("eth0").unwrap().renew_ip()` |
| Asymmetric bandwidth | `downlink_condition` on router + `iface.set_condition()` on device |
| Degrading conditions | Sequential `iface.set_condition()` calls |
| MTU reduction (VPN) | `.mtu(1420)` on router or device builder |
| PMTU blackhole | `.block_icmp_frag_needed()` on router builder |
| IPv6 dual-stack | `.ip_support(IpSupport::DualStack)` |
| IPv6 only | `.ip_support(IpSupport::V6Only)` |
| IPv6-only + NAT64 | `.preset(RouterPreset::IspV6)` or `.nat_v6(NatV6Mode::Nat64)` |
| Mobile carrier (CGNAT) | `.preset(RouterPreset::IspCgnat)` |
| Mobile carrier (v6-only) | `.preset(RouterPreset::IspV6)` |
