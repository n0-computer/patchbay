# NAT simulation limits

patchbay implements NAT through Linux nftables rules injected into router
namespaces. The nftables rule machinery is real kernel code, so simulated
NATs behave like their real-world counterparts for most properties:
mapping type, filtering rules, conntrack timeouts, hairpin, and the
rewrite pipeline. This document lists the classes of NAT where the
simulation diverges from published taxonomies, why, and what would be
required to close each gap.

## What is and is not modeled

The [`Nat`](../rust-doc) enum forms a deliberately coarse three-step
gradient:

| Preset | RFC 3489 | RFC 4787 | Backend |
|--------|----------|----------|---------|
| `Open` | Full Cone | EIM + EIF | `@fullcone` map, no filter |
| `Moderate` | Port Restricted Cone | EIM + APDF | `@fullcone` map, APDF filter |
| `Strict` | Symmetric | EDM + APDF (random ports) | `masquerade random` |

RFC 4787's third mapping class (`Address-Dependent Mapping`) is not
modeled: no real-world deployment we are aware of uses it, and dropping
it keeps the gradient honest. RFC 4787's third filtering class
(`Address-Dependent Filtering`) is modeled through
`NatFiltering::AddressDependent`, reachable via [`NatConfig::builder`].

## Gap 1: port-preserving symmetric NAT (SYMPP)

### What it is

Some real-world hardware sits between `Moderate` and `Strict`: endpoint-
dependent mapping (fresh mapping per destination, so STUN's reflexive
address does not generalize) combined with port preservation (the
external port matches the internal source port when free). The hole-
punching literature calls this class SYMPP.

Examples in the wild:

- Some mobile carriers that allocate a deterministic external port range
  per subscriber and allocate sequentially within the range.
- Port Block Allocated CGNAT deployments (A+P / RFC 6346 family) where a
  subscriber's external port range is fixed and allocation within the
  range is deterministic.
- Older enterprise firewalls before vendors moved to random port
  allocation (circa 2013 for Cisco ASA PAT).

Hole-punching against SYMPP can succeed through port prediction: the
peer observes the reflexive port for one flow and predicts the port for
the next.

### Why patchbay does not simulate SYMPP distinctly

Linux `nftables` does not offer a NAT statement that produces SYMPP
behavior:

- `masquerade` without flags tries to preserve the source port. For a
  single internal source sending to several destinations, the kernel
  checks 4-tuple uniqueness `(ext_ip, ext_port, dst_ip, dst_port)`.
  Different destinations occupy different tuples even with the same
  `ext_port`, so preservation succeeds and every flow lands on the same
  external port. The observable behavior is EIM, not EDM.
- `masquerade random` allocates a random port per flow. True EDM, no
  preservation.
- `masquerade fully-random` is the same class as `random` for our
  purposes.

An earlier iteration of this library tried to simulate SYMPP with
`masquerade` (no flag) and presented it as a separate `Nat::Strict` tier
distinct from random symmetric NAT. The empirical test confirmed that
in single-flow tests the two are indistinguishable. Shipping a preset
that promises a distinction the backend cannot produce would mislead
users writing hole-punching test suites; the library now models SYMPP
as `Strict` (random) to keep simulation pessimistic and honest.

### Impact

`RouterPreset::IspCgnatSymmetric` (which also represents the cellular
CGNAT case) resolves to `Nat::Strict` (random symmetric NAT).
Hole-punching tests against this preset will fail, as they would
against any symmetric NAT without port prediction. Applications that
rely on port-prediction-based traversal to reach peers behind real
SYMPP hardware will NOT see that path exercised in patchbay tests.
The pessimistic model is the right default for "does my app work?"
testing; it is wrong for "does my app exploit SYMPP optimistically?"
testing.

### How a future backend could add it

Three plausible approaches:

1. **Custom kernel module.** Fork or extend `netfilter-full-cone-nat` to
   expose an EDM variant that allocates external ports deterministically
   per destination while preserving the source port when possible. This
   matches how some hardware NATs are actually built, but requires an
   out-of-tree kernel module and platform-specific packaging.

2. **Dynamic sets plus nftables numgen.** Use `numgen inc mod N` or a
   destination-IP hash to produce a per-destination external port
   offset, combined with a static SNAT port range. Produces a SYMPP-like
   port allocation that is deterministic but distinct per destination.
   Does not preserve the source port exactly, so it is closer to
   "predictable symmetric" than to SYMPP.

3. **Userspace NAT.** Run a userspace process (TUN or eBPF) that
   implements custom NAT semantics. Maximum flexibility, worst
   performance, significant engineering investment.

The path we would pursue first is (2): it is the smallest change with
the largest coverage improvement and fits inside the existing nftables
pipeline.

## Gap 2: sequential and deterministic port allocation

Related to Gap 1. Port-Block-Allocated CGNAT allocates a contiguous
port range to each subscriber and assigns ports within the range
sequentially. This matters for:

- Per-subscriber port predictability (peer can guess a subscriber's next
  external port from a prior observation).
- Logging simplicity (one log line per subscriber rather than per flow).

Neither property is modeled. The `@fullcone` map and `masquerade random`
allocate from the full ephemeral range without per-subscriber carving.
Addressing this requires the same numgen-based nftables work from Gap 1.

## Gap 3: TCP NAT behavior

patchbay's NAT is specified in UDP-centric terms (hole-punching,
conntrack UDP timeouts, fullcone UDP map). TCP goes through conntrack
and SNAT normally, so outbound TCP works, but:

- There is no TCP equivalent of the `@fullcone` map, so TCP "fullcone"
  semantics are not distinctly modeled. TCP through `Nat::Open`
  behaves as whatever conntrack does for TCP, which in practice is
  closer to APDF.
- TCP-specific NAT behaviors such as RST-on-expired-flow or SYN-cookies
  are not modeled.

Adding TCP fullcone would extend `@fullcone` to `inet_service` keys for
both protocols. Low-effort; we have not prioritized it.

## Gap 4: vendor quirks

Real NAT hardware has vendor-specific idiosyncrasies that patchbay does
not reproduce:

- Cisco ASA default TCP idle timeout is 3600 seconds; Palo Alto is 3600
  but with different post-close handling; Fortinet is 180s for UDP by
  default; Juniper SRX has per-service timeouts.
- Some vendors perform application-layer gateway (ALG) fixups for
  protocols like SIP, FTP, and H.323. patchbay does not.
- Some hardware drops packets during the first ~50ms after a conntrack
  entry is created (ASIC warm-up).
- Cisco ASA post-8.4 randomizes PAT ports by default; pre-8.4 did not.

The `RouterPreset` timeouts are rough approximations. Override them
explicitly with `NatConfig::builder` when your test needs a specific
vendor shape.

## Gap 5: NAT behavior under load

Real NATs behave differently when many concurrent flows exist from the
same internal source:

- Port pool exhaustion forces fallback allocation strategies that vary
  by vendor.
- Some hardware changes mapping behavior above a per-subscriber flow
  threshold (for example switching from EIM to effectively EDM under
  load to prevent port exhaustion).
- Conntrack tables in patchbay simulate a single subscriber, so
  multi-subscriber contention on a CGN is not modeled.

This is fundamentally a simulation-scale question rather than a missing
feature. Addressing it requires running many device namespaces behind
one CGN router, which patchbay supports topologically, but the CGN
hardware behaviors above require vendor-specific rule sets that we do
not ship.

## Gap 6: IPv6 NAT edge cases

patchbay models NPTv6, masquerade, and NAT64 on the IPv6 side. Not
modeled:

- SIIT-DC and stateful NAT64 edge behaviors (DNS64 timing, prefix
  selection from multiple `64:ff9b::` variants).
- NPTv6 checksum-neutral translation corner cases for ICMPv6 error
  messages.
- MAP-T and MAP-E (RFC 7597 / 7599) used by some ISPs for deterministic
  v4-over-v6 translation.

## Summary

patchbay's NAT model is deliberately coarser than the full RFC 4787
cross product. The biggest missing class is port-preserving symmetric
NAT (SYMPP); the others are either niche, protocol-specific, or require
multi-subscriber simulation scale. The coarse model is honest about
what it simulates and pessimistic where it deliberately omits detail.
For tests whose correctness depends on the gaps above, the fix is to
run against the real hardware; patchbay will not pretend to reproduce
behavior it cannot.
