# NAT Hole-Punching

How patchbay implements NAT traversal using nftables, and what we learned
getting UDP hole-punching to work across different NAT types in Linux
network namespaces.

---

## Background: RFC 4787 mapping x filtering

Two independent axes define NAT behavior for UDP:

| Axis | Endpoint-Independent (EI) | Endpoint-Dependent (ED) |
|---|---|---|
| **Mapping** — how external ports are assigned | Same ext port for all destinations | Different ext port per destination |
| **Filtering** — which inbound packets are forwarded | Any external host can send to mapped port | Only the contacted host:port can reply |

Combined, these give the real-world profiles we simulate:

| Preset | Mapping | Filtering | Hole-punch? | Real-world examples |
|---|---|---|---|---|
| `Nat::Home` | EIM | APDF | Yes, simultaneous open | FritzBox, Unifi, TP-Link, ASUS RT, OpenWRT |
| `Nat::FullCone` | EIM | EIF | Always | Old FritzBox firmware, some CGNAT |
| `Nat::Corporate` | EDM | APDF | Never (need relay) | Cisco ASA, Palo Alto, Fortinet, Juniper SRX |
| `Nat::CloudNat` | EDM | APDF | Never (need relay) | AWS/Azure/GCP NAT Gateway |
| `Nat::Cgnat` | — | — | Varies | ISP-level, stacks with home NAT |

---

## The fullcone dynamic map

The only reliable way to get endpoint-independent mapping (EIM) in nftables
is to explicitly track port mappings in a dynamic `@fullcone` map:

```nft
table ip nat {
    map fullcone {
        type inet_service : ipv4_addr . inet_service
        flags dynamic,timeout
        timeout 300s
        size 65536
    }
    chain prerouting {
        type nat hook prerouting priority dstnat; policy accept;
        iif "ix" meta l4proto udp dnat to udp dport map @fullcone
    }
    chain postrouting {
        type nat hook postrouting priority srcnat; policy accept;
        oif "ix" meta l4proto udp update @fullcone { udp sport timeout 300s : ip saddr . udp sport }
        oif "ix" snat to <wan_ip>
    }
}
```

How it works:

1. **Postrouting** records the pre-SNAT source port in `@fullcone` before the
   `snat` rule executes. The map key is the UDP source port, the value is
   `internal_ip . internal_port`. Even if `snat` later remaps the port, the
   map holds the correct mapping keyed by the *original* port.

2. **Prerouting** looks up inbound UDP by destination port in `@fullcone` and
   DNATs to the internal host. This bypasses conntrack reverse-NAT entirely.

3. **Timeout** is 300s. Outbound traffic refreshes the entry.

**Why `update` must come before `snat`**: nftables NAT statements record the
transformation but the conntrack entry's reply tuple is not yet available
during the same chain evaluation. By recording `udp sport` / `ip saddr`
*before* SNAT, we capture the original tuple.

---

## Filtering modes

### EIF (Endpoint-Independent Filtering) — `Nat::FullCone`

No filter chain needed. Prerouting DNAT fires for any inbound packet whose
destination port is in the map, regardless of source address. Any external
host can reach the internal endpoint once it has sent one outbound packet.

### APDF (Address-and-Port-Dependent Filtering) — `Nat::Home`

Same fullcone map for EIM, plus a forward filter:

```nft
table ip filter {
    chain forward {
        type filter hook forward priority 0; policy accept;
        iif "ix" ct state established,related accept
        iif "ix" drop
    }
}
```

Why this works for hole-punching:

1. Device sends to peer -> postrouting SNAT creates conntrack entry + fullcone
   map records port.
2. Peer sends to device -> prerouting DNAT via fullcone map changes dst from
   router WAN IP to device internal IP.
3. After DNAT, the packet's 5-tuple (`peer:port -> device_internal:port`)
   matches the **reply direction** of the outbound conntrack entry from step 1.
4. Conntrack marks the packet `ct state established`.
5. Forward filter allows it.

An unsolicited packet from an unknown host also gets DNATed (step 2), but
no outbound conntrack entry exists for that source, so it's `ct state new`
and dropped by the filter.

### EDM (Endpoint-Dependent Mapping) — `Nat::Corporate` / `Nat::CloudNat`

```nft
table ip nat {
    chain postrouting {
        type nat hook postrouting priority 100;
        oif "ix" masquerade random
    }
}
```

`masquerade random` randomizes the source port per conntrack entry. No
fullcone map, no prerouting chain. Hole-punching is impossible because the
peer can't predict the mapped port from a STUN probe.

---

## nftables pitfalls

### `snat to <ip>` does NOT preserve ports reliably

The single biggest surprise. Conventional wisdom says `snat to <ip>` without
a port range is "port-preserving". In practice, **Linux conntrack assigns
different external ports for different conntrack entries from the same source
socket**, even when there is no port conflict.

Example: a device binds port 40000, sends to STUN (port preserved to 40000),
then sends to a peer — conntrack assigns port 27028 instead of 40000.

Tested and confirmed that none of these fix it:

```nft
oif "ix" snat to 203.0.113.11              # port NOT preserved across entries
oif "ix" snat to 203.0.113.11 persistent   # still remaps
oif "ix" masquerade persistent              # still remaps
oif "ix" snat to 203.0.113.11:1024-65535 persistent  # syntax error without proto match
```

The `persistent` flag is documented to "give a client the same
source-ip,source-port" but the kernel's NAT tuple uniqueness check still
triggers port reallocation across independent conntrack entries.

### A prerouting nat chain is required even if empty

Without a `type nat hook prerouting` chain registered in the nat table, the
kernel does not perform conntrack reverse-NAT lookup on inbound packets. This
means packets destined for the router's WAN IP that should be reverse-DNATed
are delivered to the router's INPUT chain instead of being forwarded to the
internal device.

### Conntrack reverse-NAT depends on port consistency

Even with a prerouting chain, conntrack reverse-NAT only works when the
inbound packet's 5-tuple matches the reply tuple of an existing conntrack
entry. If SNAT changed the port (which it does — see above), the peer sends
to the wrong port and conntrack doesn't match.

---

## Test helper subtlety: `holepunch_send_recv`

Both sides call `holepunch_send_recv(socket, peer_public_addr)` which sends
UDP probes every 200ms and checks for a response.

**Critical detail**: when one side receives a probe first, it must send a few
more packets before returning. Otherwise:

1. Side A receives side B's probe, returns success, stops sending.
2. Side B's probes to side A may have arrived *before* side A created its
   outbound conntrack entry at side B's NAT.
3. Those early probes were dropped by APDF filtering at side B's NAT.
4. Side A stopped sending, so side B never gets a packet through.

Fix: after receiving, send 3 "ack" packets to ensure the peer's NAT has an
established conntrack entry.

---

## NatConfig architecture

The `Nat` enum provides named presets. Each expands via `Nat::to_config()`
to a `NatConfig` struct:

```rust
pub struct NatConfig {
    pub mapping: NatMapping,           // EIM or EDM
    pub filtering: NatFiltering,       // EIF or APDF
    pub timeouts: ConntrackTimeouts,   // udp, udp_stream, tcp_established
}
```

`generate_nat_rules()` in `core.rs` builds nftables rules from `NatConfig`
alone — no match on `Nat` variants, just the mapping/filtering enums. Users
can either use presets (`router.nat(Nat::Home)`) or build custom configs
(`router.nat_config(NatConfig::builder()...build())`).

### CGNAT

`Nat::Cgnat` is applied at the ISP router level via `apply_isp_cgnat()`,
not through `NatConfig`. It uses plain `masquerade` (not `random`) on the
IX-facing interface and stacks with the home router's NAT.

---

## NPTv6 (Network Prefix Translation for IPv6)

NPTv6 translates the source/destination prefix while preserving the host
part, using nftables `snat prefix to` / `dnat prefix to`. Several issues
were found and fixed during implementation:

1. **Prefix length mismatch breaks translation.** NPTv6 requires matching
   prefix lengths on LAN and WAN sides. Fix: `nptv6_wan_prefix()` derives a
   unique /64 from the router's IX IP.

2. **Unrestricted `dnat prefix` breaks NDP.** Without an address match
   clause, NDP and ICMPv6 packets get translated, making the router
   unreachable. Fix: restrict rules to `ip6 saddr/daddr` matching the
   WAN/LAN prefix.

3. **WAN prefix must be outside the IX on-link range.** The IX CIDR was
   changed from /32 to /64 so WAN prefixes are off-link and routed via the
   gateway.

4. **Return routes needed for private v6 downstreams.** Added v6 return
   routes for all IX-level routers regardless of downstream pool.

See [IPv6 Deployments](ipv6.md) for the full IPv6 deployment reference.

---

## Limitations

- **UDP only in fullcone map.** TCP hole-punching (simultaneous SYN) relies
  on plain conntrack. Matches real-world behavior — TCP hole-punching is
  unreliable everywhere.

- **Port preservation assumption in map.** If `snat to <ip>` remaps the
  source port, the fullcone map key (original port) differs from the actual
  mapped port. In practice this doesn't happen in our simulations (few
  concurrent flows, 64k port space).

---

## Future work

- **Address-Restricted Cone** (EIM + address-dependent filtering): extend
  the fullcone map to track contacted remote IPs.
- **Hairpin NAT**: prerouting rule for LAN packets to own WAN IP.
- **TCP fullcone**: extend `@fullcone` to TCP for complete model.
- **Port-conflict-safe fullcone**: two-stage postrouting to read
  `ct reply proto-dst` after conntrack finalizes.
