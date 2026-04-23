# Worklog: port mapping server on patchbay routers

Started: 2026-04-23T12:34:28Z
Mode: overnight
Plan: plans/portmapping.md
Branch: upnp

## Progress

### 2026-04-23T12:34:00Z - organize phase

Read `.agents/{workflow,writing,big-jobs,lang/rust,lib/patchbay}.md`.
Confirmed overnight mode rules: no pushing, no destructive ops, full
cycle per commit, no shortcuts.

Read the portmapper client across `portmapper/src/{lib.rs, nat_pmp.rs,
nat_pmp/protocol/*, pcp.rs, pcp/protocol/*, upnp.rs, mapping.rs}` and
the `igd-next-0.17.0` SSDP + SOAP paths in
`~/.cargo/registry/src/index.crates.io-*/igd-next-0.17.0/src/common/*`.

Inventoried patchbay's router surface: `router.rs` builder, `core.rs`
`RouterData`/`RouterConfig`, `nft.rs` rule generation, `wiring.rs`
namespace setup, `dns_server.rs` as a reference for an in-process
server attached to the router lifecycle.

### 2026-04-23T12:45:00Z - plan written

Wrote `plans/portmapping.md` with goal, context, eight-step approach,
risk register, and commit strategy. Plan lives as a checklist; will
tick off steps as each lands.

## Staff reviews

(none yet)

## Blockers

(none)

## Decisions made

- Portmap server lives in-process inside each router's namespace
  (same pattern as `DnsServer`), not as a spawned `miniupnpd` process.
  Rationale: no system packages required, deterministic lifecycle,
  easy to inspect state from tests, matches project style.
- NAT-PMP and PCP share a single UDP 5351 socket, dispatched on the
  version byte. Rationale: that is how real gateways behave per RFC
  6887 section 19, and the tests must see a single port.
- DNAT rules go into a dedicated chain inside the existing `ip nat`
  table, not a separate table. Rationale: priority ordering with the
  fullcone DNAT rule matters; easier to reason about one table.
- All three protocols are served out of a single `PortmapServer`
  struct that owns the registry. Each protocol handler is a spawned
  task on the router ns runtime, owning its share of the socket
  setup. Rationale: one registry, one set of rules, consistent
  locking.

### 2026-04-23T13:05:00Z - adversarial plan review

Spawned an adversarial reviewer against `plans/portmapping.md`. Three
criticals surfaced. All three led to real plan changes:

- `set_nat_mode` flushes the `ip nat` table wholesale, so portmap
  rules must live in a separate table. Plan now creates `ip portmap`.
- PCP ANNOUNCE on port 5350 is out of scope for the portmapper
  client. Plan scopes PCP to unicast `Map` + `Announce` on 5351.
- Portmapper's client-side `ip_and_gateway()` may fall back to
  loopback when `get_local_ipaddr()` returns none. Worked around by
  ensuring test devices have a proper default route; tracked as a
  likely `../net-tools` patch in Step 8.

Substantive items folded in: UPnP description HTTP server binds
port 0 (no 49152 collision), `Home` preset stays default-off, DNAT
rule rejects internal IPs outside the downstream CIDR, UPnP lease=0
means "permanent" per IGD1 not "delete", error code mapping table
required per-protocol.

### 2026-04-23T13:25:00Z - Step 1 landed

`patchbay/src/portmap/{mod,config,registry,wire}.rs` live. 11 unit
tests green, covering alloc, preferred-port, dedup, conflict,
protocol independence, remove by either index, and expiry. Public
API surface is `PortmapConfig` + `PortmapMode` only; internals are
`pub(crate)`.

The module compiles with a temporary `#[allow(dead_code)]` on
`registry` and `wire` because no consumer exists yet. Step 2 will
consume the registry via the nftables helpers and remove the allow.

Workspace lib tests: 205 passed, 0 failed. No clippy regressions
against `patchbay`. Pre-existing clippy warnings in `patchbay-runner`
are unchanged.

### 2026-04-23T13:55:00Z - Step 2 landed

Dedicated `ip portmap` nftables table at priority `-110`, separate
from `ip nat`. `set_nat_mode` flushing `ip nat` no longer clears
portmap rules. Four unit tests cover empty, UDP, TCP, and multi-rule
render. Apply uses `add table + flush table` for idempotent atomic
swaps.

### 2026-04-23T14:15:00Z - Step 3 landed

NAT-PMP server on UDP 5351 inside the router namespace.
`RouterBuilder::portmap(PortmapMode)` wires it up at build time.
Source IP validation against downstream CIDR; lifetime 0 deletes per
RFC 6886 section 3.3. First surprise: `Nat::Home`'s APDF filter in
`ip filter forward` drops DNAT'd inbound packets with `NEW` conntrack
state. Fixed by adding `iif "wan" ct status dnat accept` to
`generate_nat_rules` before the blanket drop. Integration test with
portmapper as client confirms inbound UDP reaches the device.

183 workspace tests pass (up from 171), 0 failures.

### 2026-04-23T14:40:00Z - Step 4 landed

PCP dispatch added to the same UDP 5351 socket. Scope limited to
Announce and Map opcodes, unicast, client-initiated. PCP ANNOUNCE
on UDP 5350 is intentionally out of scope because portmapper does
not consume it.

Hit one real bug: initial `MAP_DATA_SIZE = 60` was wrong; correct
value is 36 per RFC 6887 section 11.1 (12 nonce + 1 proto + 3
reserved + 2 local + 2 external + 16 v6 = 36). portmapper's client
sends 60-byte packets (24 header + 36 body) which my decoder
rejected as truncated. Fixed.

Nonce authentication on Map: repeat requests for an existing
`(client_ip, local_port, proto)` must carry the same 12-byte nonce
or the server returns NotAuthorized.

Integration test confirms TCP roundtrip through a PCP-granted
mapping.

### 2026-04-23T15:20:00Z - Step 5 landed

UPnP IGD:1 server. Two tasks: SSDP responder on multicast
239.255.255.250:1900 joined on the downstream bridge, and a
hand-rolled HTTP/1.1 server on an ephemeral TCP port on
`downstream_gw`. Three routes: `GET /rootDesc.xml`, `GET /WANIPCn.xml`,
`POST /ctl/IPConn`. SOAP actions: GetExternalIPAddress, AddPortMapping,
AddAnyPortMapping, DeletePortMapping.

Hand-rolled HTTP chosen over pulling axum/hyper into the core crate
for a three-route fixed surface.

Integration tests: UPnP-only probe + map + UDP roundtrip, and
a `probe_all_protocols` test that validates all three protocols
coexist on a single router when `PortmapMode::All` is selected.

### 2026-04-23T15:45:00Z - Steps 6 and 7 landed

`Router::set_portmap(mode)` dynamic op added, matching the existing
`set_nat_mode` / `set_firewall` pattern. Drops the server (which
aborts its tasks via `AbortOnDropHandle`), flushes the portmap table,
and optionally starts a fresh server on the new config.

Two regression tests landed:
- `set_nat_mode_preserves_active_mapping`: validates the adversarial
  review's C1 finding. Creates a NAT-PMP mapping, switches from
  `Nat::Home` to `Nat::Cgnat`, then sends a UDP packet to the
  external address. The DNAT rule in the separate `ip portmap` table
  survives the `ip nat` flush, so the traffic still reaches the
  device.
- `set_portmap_disables_server_at_runtime`: probes NAT-PMP + PCP,
  disables via `set_portmap(None)`, reprobes, and asserts all three
  protocols are absent.

Step 8 (portmapper bug fixes in `../net-tools`) turned out not to be
needed. The predicted `Ipv4Addr::LOCALHOST` fallback bug did not
materialize because `netdev::get_local_ipaddr()` correctly returns
the device's LAN IP inside a patchbay namespace.

199 workspace lib tests pass (up from 183), 0 failures. clippy
clean. fmt clean.

### 2026-04-23T16:10:00Z - staff review round

Spawned four adversarial reviewers in parallel against the full diff:
Rust expert, distributed systems, safety/security, docs/QA. Each
saw only the code, not any of the intermediate reasoning. Findings:

Consolidated critical and substantive items after opposing-stance
review:
- UPnP caller ownership: LAN host could DNAT inbound traffic to a
  peer's internal client, or delete another tenant's mapping. Fixed
  in Phase B: AddPortMapping and DeletePortMapping now require
  peer.ip() to match the mapping's internal client.
- HTTP resource bounds: unbounded per-connection tokio::spawn, no
  timeouts, no body cap. Fixed in Phase B: 128-connection semaphore,
  10s header and body read timeouts, 64 KiB body cap with HTTP 413.
- PCP client_addr header unvalidated: RFC 6887 section 8.1 spec
  violation. Fixed in Phase B with AddressMismatch response.
- nft apply race between concurrent allocate+apply sequences. Fixed
  in Phase C: `ServerContext::apply_after_mutation` holds the
  registry guard across the apply, serializing writers. Rollback on
  apply failure so clients never see an orphan registry entry.
- No expired-mapping reaper. Fixed in Phase D: 30s ticker sweeps
  deadline-elapsed mappings and re-renders the table.

Code quality refinements (Phase A):
- Removed `#[allow(dead_code)]` module-level blankets.
- Removed two `const _: fn() -> ...` shims that silenced unused
  imports. Real imports now.
- Moved per-request context out of nat_pmp.rs (where it had been
  named Context but consumed by three protocols) into server.rs as
  ServerContext.
- Replaced hand-rolled Opcode::from_byte matchers with
  strum::FromRepr.
- `impl From<PortmapMode> for PortmapConfig` per agents/lang/rust.md.
- Explicit LAN-trust threat-model note in the module docstring.

Discarded after opposing stance:
- "Actor-pattern refactor": overkill for a simulator.
- "NAT-PMP local_port=0 returns UnsupportedOpcode": changed to
  NotAuthorizedOrRefused (wrong-code finding was valid, but the
  larger "silently drop" recommendation conflates two cases).
- "M3: ct status dnat accept is too broad": accepted. No other DNAT
  service exists today; if NAT64 or similar lands it will need to
  coordinate with the portmap rules. Noted in the module doc but not
  tightened further.

### 2026-04-23T17:05:00Z - final verification

`cargo test --workspace --lib`: 235 passed, 0 failed, 1 ignored
across all workspace crates. patchbay lib count: 201 (up from 171
baseline; 30 added by this series).

`cargo clippy --workspace --all-targets`: clean in `patchbay`. Two
warnings in `patchbay-runner` are pre-existing and unrelated.

`cargo make format-check`: clean.

## Summary

Patchbay routers now model the three port-mapping protocols real
consumer routers advertise: NAT-PMP (RFC 6886), PCP (RFC 6887), and
UPnP IGD:1 (SSDP + SOAP). Enable them per-router via
`RouterBuilder::portmap(PortmapMode)` or at runtime via
`Router::set_portmap(PortmapMode)`. Off by default on every preset.

The series spans eleven commits. Each commit compiles and passes the
full test suite independently:

1. Foundation module (registry, mapping keys, dedup, lifetime).
2. Dedicated `ip portmap` nftables table at priority -110.
3. NAT-PMP server + RouterBuilder::portmap + integration test.
4. PCP server sharing UDP 5351 with NAT-PMP.
5. UPnP IGD server (SSDP + handwritten HTTP/1.1 + SOAP).
6. `Router::set_portmap` dynamic reconfiguration.
7. Refactor: consolidate `ServerContext`, trim module seams.
8. Security hardening: caller ownership on UPnP, HTTP bounds, PCP
   client_addr validation.
9. Correctness: serialize nft applies, rollback on failure, add
   expired-mapping reaper.
10. Additional delete-path tests.

Integration coverage includes end-to-end UDP/TCP roundtrips for all
three protocols, `Router::set_nat_mode` regression (validates the
adversarial review's C1 finding), and `Router::set_portmap` shutdown.

Step 8 from the plan ("fix portmapper bugs in ../net-tools") was not
needed: the predicted `Ipv4Addr::LOCALHOST` fallback never fired
because netdev correctly resolves the device's LAN IP inside each
patchbay namespace.

Next things a reviewer might want to tighten (deferred as
non-critical):
- Move the UPnP HTTP body size / timeouts behind a `PortmapConfig`
  field if a test needs to exercise the limits.
- Replace the minimal SOAP tag extractor with a streaming parser if
  `igd-next`'s payload shape drifts.
- Scope the APDF `ct status dnat accept` rule to a named set
  populated from the registry once NAT64 or other future DNAT
  services land.
