# NAT Hole Punching in netsim

## Background: NAT type taxonomy (RFC 4787)

Two independent axes define NAT behavior for UDP:

**Mapping** — how the NAT assigns external ip:port for outbound flows:

| Mapping type          | Behaviour                                        |
|-----------------------|--------------------------------------------------|
| Endpoint-Independent  | Same external port for all destinations           |
| Endpoint-Dependent    | Different external port per destination (symmetric) |

**Filtering** — which inbound packets the NAT accepts on a mapped port:

| Filtering type        | Accepts inbound from                             |
|-----------------------|--------------------------------------------------|
| Endpoint-Independent  | Any host (full cone)                             |
| Address-Dependent     | Any port on hosts the client has sent to          |
| Addr+Port-Dependent   | Only the exact host:port the client sent to       |

Combined, these give the classic four NAT types:

| NAT type             | Mapping | Filtering       | Hole-punch friendly |
|----------------------|---------|-----------------|---------------------|
| Full Cone            | EIM     | EIF             | Yes — trivial       |
| Restricted Cone      | EIM     | Addr-Dependent  | Yes — send first    |
| Port-Restricted Cone | EIM     | Addr+Port-Dep   | Yes — simultaneous  |
| Symmetric            | EDM     | Addr+Port-Dep   | No (port prediction needed) |

---

## Implemented NAT modes

### `NatMode::None`

No NAT.  Downstream addresses are publicly routable (DC / server
behavior).  No nftables rules.

- **Mapping**: N/A
- **Filtering**: N/A
- **Hole punching**: Not needed — direct connectivity.

### `NatMode::Cgnat`

ISP-level carrier-grade NAT.  Applied on the IX-facing interface of ISP
routers.

```nft
table ip nat {
    chain postrouting {
        type nat hook postrouting priority 100;
        oif "ix" masquerade
    }
}
```

- **Mapping**: Endpoint-Dependent (`masquerade` = SNAT with port randomization
  per flow, and the external IP follows the outgoing interface).
- **Filtering**: Addr+Port-Dependent (Linux conntrack default).
- **Hole punching**: Difficult.  Different port per destination means the
  peer cannot predict the mapped port from a STUN probe.  Simultaneous
  open can work if both sides guess the same port, but this is unreliable.
  Real ISP CGNATs vary widely; some are more permissive.

### `NatMode::DestinationIndependent`

Home router NAT with full-cone behavior.  This is the mode that matters
most for P2P / hole punching.

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
        oif "ix" meta l4proto udp update @fullcone {
            udp sport timeout 300s : ip saddr . udp sport
        }
        oif "ix" snat to <wan_ip>
    }
}
```

- **Mapping**: Endpoint-Independent (`snat to <ip>` with no port range;
  Linux preserves the original source port when free).
- **Filtering**: Endpoint-Independent (via `@fullcone` dynamic map — any
  external host can send to a mapped port).
- **Hole punching**: Works reliably.  One side probes a STUN server to learn
  its mapped address; the other side can send to that address immediately,
  even without sending first.  Timing does not matter.

#### How the fullcone map works

1. **Outbound UDP** hits postrouting.  Before `snat` rewrites the source,
   `update @fullcone` records `src_port → src_ip . src_port`.  Since
   `snat to <ip>` (no port range) preserves the source port when it is
   free, `src_port == mapped_port` in virtually all cases.

2. **Inbound UDP** hits prerouting.  The destination port is looked up in
   `@fullcone`.  If found, the packet is DNATted to the recorded internal
   endpoint.  The kernel then routes it to the LAN.

3. **Timeout** is 300 seconds.  Outbound traffic refreshes the entry.

#### Why `update` must come before `snat`

nftables NAT statements record the transformation but the conntrack entry's
reply tuple is not readable within the same chain evaluation.  After `snat`,
`ct reply proto-dst` (the mapped port) is not yet available.  By recording
the mapping *before* SNAT using `udp sport` / `ip saddr`, we capture the
original tuple.

### `NatMode::DestinationDependent`

Symmetric-ish NAT.  Different external port per destination.

```nft
table ip nat {
    chain postrouting {
        type nat hook postrouting priority 100;
        oif "ix" masquerade random
    }
}
```

- **Mapping**: Endpoint-Dependent (`masquerade random` assigns a different
  random source port per destination).
- **Filtering**: Addr+Port-Dependent (Linux conntrack default).
- **Hole punching**: Very difficult.  The mapped port seen via STUN is
  different from the port that would be assigned for traffic to the peer.
  Port prediction or relay (TURN) is required.

---

## Limitations of current implementation

- **UDP only fullcone**.  The `@fullcone` map only handles UDP.  TCP
  hole punching (simultaneous SYN) relies on plain conntrack, which
  requires both sides to send at roughly the same time.  This matches
  real-world behavior — TCP hole punching is unreliable everywhere.

- **Port preservation assumption**.  If `snat to <ip>` remaps the source
  port (due to a port conflict with another flow), the fullcone map key
  won't match the actual mapped port.  This is extremely unlikely in our
  simulations (few concurrent flows, 64k port space) but theoretically
  possible.

- **No hairpin NAT**.  Internal-to-internal traffic via the public IP is
  not supported.  Packets from a LAN host to its own public addr are
  dropped.

---

## Future work

### Missing models commonly deployed in the wild

**Address-Restricted Cone** (EIM + Address-Dependent Filtering).
Common on many consumer routers (Netgear, TP-Link default firmware).
The NAT accepts inbound from any port on a remote host, but only if the
internal endpoint has previously sent *something* to that host's IP.
Implementation: extend the fullcone map to also track a set of
contacted remote IPs per mapping, and gate the prerouting DNAT on
`ip saddr` being in that set.

**Port-Restricted Cone** (EIM + Addr+Port-Dependent Filtering).
This is what Linux conntrack does by default — `snat to <ip>` without the
fullcone map.  Already achievable by using `DestinationDependent` mode
without the `random` flag, but not exposed as a distinct `NatMode` variant
yet.  Easy to add: just `snat to <ip>` with no fullcone map.

**Symmetric NAT with port prediction window**.  Some carrier NATs assign
ports sequentially within a small window.  Real-world hole-punch libraries
(libnice, PJNATH) exploit this with "port prediction" — probing N+1 after
seeing port N from STUN.  Could be simulated with `snat to <ip>:<range>`
plus a small sequential allocation pool.

### Configurable filtering mode

Decouple mapping and filtering into separate knobs:

```rust
enum NatMapping {
    EndpointIndependent,   // snat to <ip>
    EndpointDependent,     // masquerade random
}

enum NatFiltering {
    EndpointIndependent,   // fullcone map (current DestinationIndependent)
    AddressDependent,      // fullcone map + contacted-hosts set
    AddressPortDependent,  // pure conntrack (no fullcone map)
}
```

This would allow any combination (e.g. EIM mapping + address-dependent
filtering = restricted cone).

### Hairpin NAT

Add a prerouting rule for packets from the LAN with `ip daddr == wan_ip`:
DNAT back to the internal endpoint via the fullcone map.  Requires careful
ordering to avoid loops.

### TCP fullcone

Extend the `@fullcone` map to TCP.  Less useful in practice since TCP hole
punching always requires simultaneous SYN, but it would complete the model.

### Port-conflict-safe fullcone

Handle the rare case where `snat to <ip>` remaps the source port:
use a two-table approach where a second postrouting chain at a later
priority reads `ct reply proto-dst` after conntrack has finalized the
mapping.  Or restrict the SNAT port range to avoid conflicts entirely.

### nf_nat_fullcone kernel module

The [nft-fullcone](https://github.com/fullcone-nat-nftables/nft-fullcone)
out-of-tree kernel module provides true kernel-level fullcone NAT with
zero map-maintenance overhead.  As of kernel 6.18 it remains out-of-tree
(primarily used in OpenWrt).  If mainlined, it would replace the dynamic
map approach.
