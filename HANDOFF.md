# HANDOFF.md

## User goal (high-level)
Build a Linux network simulation (no VMs) using **network namespaces** and **routing + NAT** to emulate:
- a “map” of **two ISPs**, a **DC**, and **home routers** with **devices behind them**
- configurable NAT behaviors at the home router and optionally CGNAT at the ISP
- ability to run commands inside device namespaces (`run_in(device, command)`)

Implementation must be in **Rust**, as a wrapper around **rtnetlink**, with tests that verify NAT behavior using **STUN-like** commands that check public IP/port visibility.

User explicitly allowed **nftables** for NAT implementation.

## Required API (high-level)
Create a high-level `Lab` API (builder style), roughly:

- `Lab::new()`
- `add_isp(name, mode) -> IspId`
  - `mode`: `NoCgnat` or `Cgnat { ... }`
- `add_dc(name) -> DcId`
- `add_home(name, isp, nat_mode) -> HomeId`
  - `nat_mode`: destination-independent vs destination-dependent NAT
- `add_device(name, home) -> DeviceId`
- `build()`: creates netns, links, addresses, routes, forwarding, NAT rules
- `run_in(device, command...)`: execute in device netns

Additionally for testing convenience:
- helper to spawn STUN-like UDP reflectors in specific namespaces
- helper `probe_udp_mapping(device, reflector_addr)` returning observed external `ip:port`
- helper `isp_public_ip(isp)` for CGNAT test assertions

## NAT feature requirements
### Home NAT options
- **Destination-independent NAT** (EIM-ish):
  - mapping should be stable across different destinations
  - test should show the same observed external port when contacting two different reflectors
  - nftables approach: `snat ... persistent` is preferred to force stable mapping

- **Destination-dependent NAT** (EDM / symmetric-ish):
  - mapping should differ per destination
  - test should show different observed external ports when contacting different reflectors
  - nftables approach: `masquerade random` (or `snat ... random`) to encourage changing mappings

### ISP CGNAT option
- If CGNAT enabled, ISP should NAT subscriber WAN traffic to ISP “public” address on IX-facing interface.
- Test should show device’s observed external IP equals ISP public IP (not the home WAN IP).

## Testing requirements
- Include tests that verify NAT behavior with **STUN-like** probes:
  - Build lab
  - Start 2 reflectors in different locations (e.g., DC namespace and IX/root)
  - From a device, probe both reflectors and compare observed address
    - destination-independent: same external port across both
    - destination-dependent: different external ports across both
  - CGNAT: observed external IP equals ISP public

- Tests must run without VMs, purely netns.
- Tests require root / CAP_NET_ADMIN and nft installed.
- Because `setns` is thread-local, tests should use a single-thread tokio runtime (`current_thread`) OR carefully manage thread pinning.

## Implementation constraints from user
- Put the whole implementation in **two files only**:
  - `Cargo.toml`
  - `src/lib.rs` with **inline modules and a test module**
- nftables is acceptable for NAT.

## Topology assumptions / approach (from assistant’s design)
- Create a root “IX” bridge (e.g., `br-ix`) with IP like `203.0.113.1/24`
- Connect each ISP and DC edge namespace to the IX bridge via veth.
- Home routers connect to their ISP via a veth point-to-point link (/30).
- Devices live behind home routers on a “LAN” subnet (e.g., 192.168.X.0/24).
- Enable IPv4 forwarding in ISP/home/DC namespaces.
- Apply nft rules inside router namespaces.

## Key implementation learnings / gotchas
- `rtnetlink` handles links/addrs/routes, but:
  - creating named netns is typically done by `unshare(CLONE_NEWNET)` + bind-mounting `/proc/self/ns/net` to `/var/run/netns/<name>`
- `setns()` is **thread-local**:
  - if you setns in async code on a multi-thread runtime, you can corrupt behavior
  - either: run with `tokio::test(flavor="current_thread")` or isolate setns operations into forked processes.
- For applying nft rules reliably:
  - simplest: execute `nft -f -` inside the target namespace (fork + setns + spawn nft and pipe rules to stdin)
- For STUN-like verification:
  - implement a small UDP reflector that replies with the sender’s `ip:port` as observed at the reflector
  - tests probe reflectors to derive observed NAT mapping

## Deliverable state (what exists vs what needs work)
- Assistant produced a draft single-file implementation in `src/lib.rs` plus `Cargo.toml`:
  - includes Lab API, netns creation, some rtnetlink helpers, nft application, reflector/probe, and tests.
- However, the draft has **known issues to audit/fix**:
  - Some routing/return-route logic was sketched and may be incorrect/incomplete.
  - Device and home LAN connectivity used dummy interfaces (not real veth/bridge), which may not actually forward packets between device<->home. A correct model usually needs veth pairs for device-home LAN links or a bridge in the home namespace.
  - DC “LAN” also used a dummy interface; reflectors bound to DC IX IP should still work, but DC-internal server simulation is minimal.
  - A route-add helper attempted to build a CIDR string awkwardly (likely buggy).
  - ISP CGNAT rules are applied per-home and may be duplicated; should be idempotent or built once per ISP.

The next agent should treat the current code as a **starting scaffold**, then:
1. Replace dummy LAN links with real veth connections between home and each device (or a home bridge).
2. Ensure proper routing between:
   - device -> home -> isp -> IX -> dc (reflector)
   - return path must be correct without relying on unreachable private subnets (NAT should handle it)
3. Make nft rulesets consistent and not flushed unexpectedly if multiple homes exist.
4. Ensure cleanup is robust (namespaces, bridge, veth).
5. Keep everything within the 2-file constraint.

## How NAT verification should work (test logic recap)
- Start two UDP reflectors on different destination IPs (e.g., one in DC ns on `dc_ix_ip`, one on IX bridge IP `203.0.113.1`).
- From device namespace, send UDP probe to each reflector.
- Reflector replies “OBSERVED x.x.x.x:pppp”.
- Compare observed endpoints:
  - DestinationIndependent: observed ports should match across reflectors.
  - DestinationDependent: observed ports should differ across reflectors.
- For CGNAT:
  - observed IP should be ISP public IP.

## Non-goals / explicit exclusions
- No VMs.
- No external STUN servers; use local “STUN-like” reflector(s).
- Avoid complex multi-file structure; only Cargo.toml + lib.rs.
