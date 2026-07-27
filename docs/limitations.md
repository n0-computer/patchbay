# Limitations

patchbay models real Linux networking with high fidelity, but it has
boundaries. Understanding them helps you decide when patchbay is a good
fit and where to expect differences from production systems.

## IPv6 limitations

### RA and RS are modeled, not packet-emulated

In `Ipv6ProvisioningMode::RaDriven`, patchbay models Router
Advertisement (RA) and Router Solicitation (RS) behavior through route
updates and structured tracing events. It does not send raw ICMPv6 RA
or RS packets on virtual links. Application-level routing behavior is
close to production, but packet-capture workflows that expect real RA/RS
frames will not see them.

### SLAAC behavior is partial

patchbay models default-route and address behavior needed for routing
tests, but it does not implement a full Stateless Address
Autoconfiguration (SLAAC) state machine with all timing transitions.
Connectivity and route-selection tests work well. Detailed host
autoconfiguration timing studies are out of scope.

### Neighbor Discovery timing is not fully emulated

Neighbor Discovery (ND) address and router behavior is represented in
route and interface state, but exact kernel-level timing of ND probes,
retries, and expiration is not emulated. Most application tests are
unaffected. Low-level protocol timing analysis should use a dedicated
packet-level setup.

### DHCPv6 prefix delegation is not implemented

patchbay does not implement a DHCPv6 Prefix Delegation server or client
flow. Use static /64 allocation in topologies instead. Prefix-based
routing and NAT64 scenarios work with static setup, but
residential-prefix churn workflows are not represented.

## General platform and model limitations

### Linux-only execution model

patchbay uses Linux network namespaces, nftables, and tc — it requires
a Linux kernel. macOS and Windows host stacks are not emulated. For
non-Linux development machines, [patchbay-vm](guide/vm.md) wraps
simulations in a QEMU Linux VM.

### Requires kernel features and host tooling

patchbay depends on unprivileged user namespaces and the `nft` and `tc`
userspace tools. If these capabilities are unavailable or restricted —
as in some CI containers or hardened environments — labs cannot run. See
[Getting Started](guide/getting-started.md) for the kernel sysctl
settings that may need adjustment.

### No wireless or cellular radio-layer simulation

patchbay models link effects with `tc` parameters: latency, jitter,
loss, and rate limits. It does not model WiFi or cellular PHY/MAC
behavior such as radio scheduling, channel contention, or handover
signaling. The link condition presets (`wifi()`, `mobile_4g()`, etc.) apply
realistic impairment at the IP layer, which is sufficient for transport
and application resilience testing but not for radio-layer research.

### Dynamic routing protocols are not built in

patchbay focuses on static topology wiring, NAT, firewalling, and route
management through its API. It does not include built-in BGP, OSPF, or
RIP control-plane implementations. You can run routing daemons inside
namespaces yourself — the namespaces are real Linux network stacks — but
protocol orchestration is user-managed, not first-class.

### Time and clock behavior are not virtualized

patchbay uses the host kernel clock and scheduler. It does not
virtualize per-node clocks or provide deterministic virtual time. Most
integration tests work as expected, but time-sensitive
distributed-system tests that depend on precise clock relationships
between nodes may need additional controls.
