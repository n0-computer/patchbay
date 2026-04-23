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

## Summary

(pending)
