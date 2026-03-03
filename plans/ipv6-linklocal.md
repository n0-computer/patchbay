# IPv6 Link-Local Parity Plan

## TODO

- [x] Write plan
- [ ] Phase 0: Define target behavior and compatibility boundaries
- [x] Phase 1: Kernel behavior parity for link-local addresses and routes
- [ ] Phase 2: Router Advertisement and Router Solicitation behavior
- [x] Phase 3: Public API support for link-local and scope handling
- [ ] Phase 4: Real-world presets for consumer and production-like IPv6
- [ ] Phase 5: Tests and validation matrix
- [ ] Final review

## Goal

Make patchbay's IPv6 link-local behavior match production and consumer deployments as closely as practical:

- Every IPv6-capable interface has a usable link-local address.
- Default router behavior uses link-local next hops from RA/ND semantics.
- Scope-aware APIs and routing behavior work for `fe80::/10` correctly.
- Consumer CPE behavior and host behavior follow modern RFC expectations.

## Real-World Deployment Baselines

This section defines the target behavior we want to emulate first. These are the reference deployment classes for parity work.

### 1. Mobile carrier access (4G/5G, handset as host)

- The host receives RAs from a carrier router. Default router is link-local.
- The host has at least one LLA and often one or more temporary/stable global addresses.
- IPv4 fallback is typically NAT64/464XLAT. This does not change the need for correct LLA default-router behavior.
- A single interface commonly owns the active default route, with rapid route refresh during mobility events.

Patchbay parity target:

- RS on interface up, RA-driven default route via `fe80::/10` next hop.
- Route replacement behavior that handles carrier-style churn.
- Optional NAT64 remains independent from LLA mechanics.

### 2. Home router with IPv6 support (consumer CPE)

- CPE advertises one or more /64 LAN prefixes via RA.
- Router source address for RA is link-local on that LAN interface.
- Hosts choose default route from RA and maintain Neighbor Cache entries to the router LLA.
- Stateful firewall on CPE controls inbound behavior. This is separate from ND/RA correctness.

Patchbay parity target:

- Router LAN interfaces send RAs with configurable lifetime and preference.
- Hosts install and remove default routes based on RA timers.
- Prefix and default-router behavior follows RFC 4861/4862/5942 semantics.

### 3. Linux laptop host behavior

- Uses kernel SLAAC and RFC 4861 ND behavior.
- Link-local sockets and routes require interface scope correctness.
- DAD is normally enabled; addresses can remain tentative briefly after link up.

Patchbay parity target:

- Default mode keeps DAD enabled.
- Tests can assert tentative -> preferred transition when needed.
- Scope-aware APIs prevent accidental use of ambiguous LLAs.

### 4. macOS laptop host behavior

- Uses RA-driven default route and SLAAC with temporary address rotation.
- Strongly depends on scoped address handling for user-space link-local sockets.
- Route selection prefers valid default routers and can switch after lifetime expiry.

Patchbay parity target:

- Route lifetime and preference updates are modeled.
- Link-local route installation and socket examples work with explicit scope.
- The simulator models Linux-kernel-observable behavior and policy, not a byte-for-byte macOS network stack implementation.

### 5. Windows laptop host behavior

- Uses RA and SLAAC by default, including temporary addresses and stable behavior per interface.
- Scope zone index is required for link-local destinations in many user-space APIs.
- Multiple interfaces can produce multiple candidate default routes.

Patchbay parity target:

- Zone-aware examples and tests are part of docs and helper utilities.
- Multi-interface device tests validate deterministic `default_via` behavior when multiple LLAs exist.
- The simulator models Linux-kernel-observable behavior and policy, not a byte-for-byte Windows network stack implementation.

### 6. Cross-platform baseline rules we should match

- Every IPv6-enabled interface has an LLA.
- RA source is link-local.
- Default routers are represented as scoped LLA next hops.
- Route and socket operations requiring scope fail fast when scope is missing.
- DAD is on by default, off only in explicit deterministic test mode.

## Research Basis

Primary references used for this plan:

- RFC 4861 (Neighbor Discovery): RA source must be link-local; default routers are learned from RA.
  - https://www.rfc-editor.org/rfc/rfc4861
- RFC 4862 (SLAAC): link-local creation lifecycle and DAD requirements.
  - https://www.rfc-editor.org/rfc/rfc4862
- RFC 4007 (Scoped addressing): link-local requires zone index / scope handling.
  - https://www.rfc-editor.org/rfc/rfc4007
- RFC 5942 (Subnet model): hosts should only treat explicit on-link prefixes as on-link.
  - https://www.rfc-editor.org/rfc/rfc5942
- RFC 4191 (Router preferences and RIO): default-router preference and route information semantics.
  - https://www.rfc-editor.org/rfc/rfc4191
- RFC 7084 (IPv6 CE requirements) plus updates RFC 9096 and RFC 9818: consumer router requirements.
  - https://www.rfc-editor.org/rfc/rfc7084
  - https://www.rfc-editor.org/rfc/rfc9096
  - https://www.rfc-editor.org/rfc/rfc9818
- Linux man pages: `ipv6(7)`, `ip-route(8)`, `ip-address(8)` for scope-id, route installation, and DAD state.
  - https://man7.org/linux/man-pages/man7/ipv6.7.html
  - https://man7.org/linux/man-pages/man8/ip-route.8.html
  - https://man7.org/linux/man-pages/man8/ip-address.8.html

## Current Gaps (as of today)

Observed in current codebase:

- IPv6 addresses are assigned explicitly from patchbay pools, not from link-local lifecycle.
- IPv6 default route helper uses only gateway address and does not explicitly model interface scope for link-local next hops.
- DAD is globally disabled in namespaces, which diverges from production defaults where DAD is usually enabled.
- Public handles expose global/ULA-like `ip6()` but do not expose link-local per interface.
- No explicit RA/RS simulation path for host default-router learning from link-local router addresses.
- Devtools and event payloads do not surface LLA or default-router source details.
- Presets do not distinguish static provisioning from RA-driven provisioning.
- The plan text previously implied OS-specific emulation that patchbay cannot provide inside Linux network namespaces.

## Phase 0: Behavior Contract

Define the exact behavior profile to avoid ambiguous implementation:

1. Add an internal design note under `docs/reference/ipv6.md` describing two modes:
   - `production_like` (default target): DAD enabled, LLAs present, RA/RS path active where configured.
   - `deterministic_test` (compat mode): existing deterministic static assignment semantics where needed for old tests.
2. Define a policy profile matrix (`consumer_home`, `mobile_carrier`, `enterprise_strict`, `lab_deterministic`) that maps to expected observable behavior for tests and docs.
3. Decide where strict realism is mandatory versus opt-in.
4. List non-goals for first iteration, for example full DHCPv6-PD server stack in core.
5. Define migration rules for existing tests that currently assume immediate non-tentative IPv6 addresses.
6. Explicitly state that patchbay does not emulate non-Linux host stacks. It emulates deployment behavior and routing/address policy visible at the wire and netlink levels.

Acceptance:

- The project has one written contract for link-local behavior and migration strategy.
- The contract explicitly maps behavior expectations to at least home CPE, mobile carrier, and laptop hosts.

## Phase 1: Kernel Parity for LLA + Routes

### 1.1 Interface link-local visibility

1. Extend interface state model to carry an optional link-local IPv6 address (`ll6`) independently from global/ULA `ip6`.
2. Add explicit `AddrState` metadata for IPv6 addresses where available (`tentative`, `preferred`, `deprecated`, `dadfailed`).
3. Add helper methods on handles, for example `DeviceIface::ll6()` and router-side iface accessors.
4. Ensure `ll6` can be discovered from netlink after interface bring-up, and cache refresh hooks exist after replug/reconfigure.

### 1.2 Route installation with scoped next hops

1. Add netlink methods for IPv6 default routes that can bind both next hop and output interface (scope-safe for link-local).
2. For link-local next hop default routes, always install route with explicit device context.
3. Keep existing global next hop path for non-link-local gateways.
4. Add idempotent `replace_default_route_v6_scoped` helper for multi-uplink devices.
5. Ensure route query helpers return interface index/name together with gateway to avoid scope loss.

### 1.3 DAD behavior control

1. Stop globally forcing `accept_dad=0` by default for production-like mode.
2. Add explicit option to disable DAD only for deterministic test mode.
3. Surface address state transitions (`tentative`, `dadfailed`) where useful for debugging.
4. Add bounded waiting helper for tests that need a preferred IPv6 address before connect.

Acceptance:

- In a dual-stack lab, each IPv6-capable interface reports `ll6`.
- Default route via link-local next hop works and resolves only with explicit interface scope.
- DAD behavior is configurable and defaults to production-like mode.
- Replug and route-switch operations preserve valid scoped default route behavior.

## Phase 2: RA/RS and Default Router Learning

### 2.1 Router behavior

1. Add RA emission capability for router interfaces that should advertise prefixes.
2. Ensure RA source address is the router interface link-local address.
3. Support Router Lifetime and RFC 4191 preference fields.
4. Optionally support Route Information Option for local-only communication patterns.
5. Support configurable RA intervals and immediate unsolicited RA on topology changes.
6. Implement RA emission as long-lived per-interface tasks owned by the namespace async worker, not on Tokio runtime threads outside netns.
7. Define deterministic shutdown and restart semantics for those tasks during router removal, replug, or topology reset.

### 2.2 Host behavior

1. Add RS sending on host iface bring-up when RA mode is enabled.
2. Populate default router from received RA using router link-local source.
3. Maintain prefix list and on-link behavior consistent with RFC 5942 rules.
4. Handle RA lifetime expiration and default-router withdrawal.
5. Respect RIO when no default route is present (local communications-only scenarios).
6. Implement host RS/default-router learning in netns worker tasks to preserve setns correctness and avoid cross-thread scope bugs.

### 2.3 Compatibility mode

1. Keep static route assignment path for tests that depend on fixed setup.
2. Allow per-lab toggle: static provisioning versus RA-driven provisioning.
3. Add per-device override for targeted migration of complex tests.

Acceptance:

- Host default route can be learned from RA and points to a link-local router address on the correct interface.
- RA disable/enable and Router Lifetime changes produce expected default-router list behavior.
- Static mode remains deterministic and preserves legacy test stability.
- RA/RS tasks are started and stopped cleanly with namespace lifecycle events.

## Phase 3: Public API and CLI Ergonomics

1. Add explicit getters:
   - `DeviceIface::ll6()`
   - `RouterIface::ll6()`
2. Add getters for default-router metadata per interface, including scoped next hop.
3. Add utility constructors for scoped socket addresses in examples and helpers.
4. Add convenience method for creating scoped textual addresses for diagnostics.
5. Update event/devtools payloads to include link-local and scope metadata where relevant.
6. Document route and socket caveats for link-local usage in tests (`sin6_scope_id` / iface binding requirements).
7. Add migration notes for downstream users currently calling `ip6()` and assuming it is sufficient for routing.

Acceptance:

- Users can retrieve and use link-local addresses safely from the public API.
- Devtools can display link-local addresses distinctly from global/ULA addresses.
- Downstream tests can choose between global `ip6()` and scoped `ll6()` explicitly.

## Planned Rust API Changes

This section summarizes the expected public API surface changes, with compatibility notes.

### New getters and metadata

1. `DeviceIface::ll6() -> Option<Ipv6Addr>`
   - Returns the interface link-local IPv6 address when IPv6 is enabled.
2. `Router::iface(name: &str) -> Option<RouterIface>`
   - Returns an owned snapshot handle for a router interface.
3. `Router::interfaces() -> Vec<RouterIface>`
   - Returns all router interfaces as owned snapshots.
4. `RouterIface::ll6() -> Option<Ipv6Addr>`
   - Exposes router-side LLAs for diagnostics and RA/default-router assertions.
5. `DeviceIface::default_router_v6() -> Option<ScopedIpv6NextHop>`
   - Returns current default-router next hop including interface scope metadata.
6. Optional address state accessors, for example:
   - `DeviceIface::ip6_state() -> Option<AddrState>`
   - `DeviceIface::ll6_state() -> Option<AddrState>`
   - `RouterIface::ll6_state() -> Option<AddrState>`

### New supporting types

1. `ScopedIpv6NextHop`
   - Proposed fields: `{ addr: Ipv6Addr, ifname: Arc<str>, ifindex: u32 }`
   - Purpose: represent link-local next hops safely without losing scope.
2. `AddrState`
   - Proposed variants: `Tentative`, `Preferred`, `Deprecated`, `DadFailed`, `Unknown`
   - Purpose: expose kernel IPv6 address lifecycle where available.
3. Optional provisioning mode enum, for example `Ipv6ProvisioningMode`
   - `Static`, `RaDriven`
   - Purpose: make behavior explicit at lab or profile level.

### Builder and configuration changes

1. Add RA/provisioning knobs on router and possibly lab builders, for example:
   - `RouterBuilder::ra_enabled(bool)`
   - `RouterBuilder::ra_lifetime(Duration)`
   - `RouterBuilder::ra_preference(RouterPreference)`
2. Add compatibility and determinism controls:
   - `LabOpts::ipv6_dad_mode(...)` or equivalent
   - `LabOpts::ipv6_provisioning_mode(...)` or equivalent
3. Keep existing APIs functional:
   - `Device::ip6()` remains available for global/ULA address use cases.
   - Existing static provisioning paths remain valid in deterministic mode.
4. Keep `LabOpts` as the entry point for global defaults. This matches existing builder style and remains feasible with additive fields.

### Netlink/internal API additions

1. Scoped default-route helpers for link-local gateways.
2. Route query helpers that return interface metadata with gateway.
3. Interface refresh helpers to update cached `ll6` and address states after replug/reconfigure.
4. RA/RS task registration APIs on netns workers for lifecycle-safe startup and teardown.

### Events and devtools schema impact

1. Extend interface-related event payloads with optional link-local fields.
2. Include default-router source/scope info where relevant.
3. Keep fields additive to preserve backward compatibility for consumers that ignore new keys.

### Compatibility policy

1. Existing callers using `ip6()` continue to work.
2. New APIs are additive in first rollout.
3. Behavior changes that can affect timing or routing default to explicit opt-in until migration is complete.

## Phase 4: Real-world Presets and Defaults

Align presets with consumer behavior expectations from RFC 7084 family:

1. Home/consumer preset:
   - RA enabled on LAN
   - link-local router identity stable
   - realistic default firewall posture remains intact
   - default-route learning is RA-driven for hosts
2. Datacenter/internal preset:
   - support optional link-local-only infrastructure links where appropriate
   - keep loopback/global addresses for management scenarios
3. Mobile-like preset:
   - preserve existing NAT64 and v6-only semantics while ensuring link-local correctness on access links
4. Policy profile toggles:
   - `consumer_home`, `mobile_carrier`, `enterprise_strict`, `lab_deterministic` knobs for timing and address-selection policy where practical.

Acceptance:

- Presets express explicit link-local policy and behavior.
- Example topologies in docs cover at least one mobile, one home, and one laptop profile scenario.
- Docs explicitly state profiles are deployment-policy emulation, not OS-kernel emulation.

## Phase 5: Test Matrix

Add focused tests, separate from existing IPv6 tests.

Test module location:

- New module file: `patchbay/src/tests/ipv6_ll.rs`
- Register in `patchbay/src/tests/mod.rs` as `mod ipv6_ll;`
- Keep existing `ipv6.rs` intact for broader IPv6 behavior, and use `ipv6_ll.rs` for link-local and RA/RS semantics.

Core tests:

1. `link_local_presence_on_all_ipv6_ifaces`
   - Verifies every IPv6-capable interface gets a non-empty `fe80::/10` address and that API getters return it.
2. `default_route_via_link_local_requires_dev_scope`
   - Verifies a default route using link-local next hop is only installed/usable with explicit interface scope.
3. `ra_source_is_link_local`
   - Verifies outbound RA packets use the router interface LLA as source, never a global/ULA source.
4. `host_learns_default_router_from_ra_link_local`
   - Verifies host installs default route from received RA and next hop is scoped LLA of advertising router.
5. `dad_enabled_production_mode`
   - Verifies production-like mode enables DAD and observed address state transitions from tentative to preferred.
6. `dad_disabled_deterministic_mode`
   - Verifies deterministic mode disables DAD and addresses become usable immediately for stable tests.
7. `link_local_socket_scope_required` (expected failure without scope, success with scope)
   - Verifies application-level connect/send to LLA fails without scope id and succeeds with correct scope.
8. `router_lifetime_zero_withdraws_default_router`
   - Verifies RA with lifetime 0 removes default-router entry and default route from host routing table.
9. `rio_local_routes_without_default_router`
   - Verifies RIO routes can exist and be used when no default route is advertised.
10. `multi_uplink_prefers_router_preference_and_metric`
   - Verifies host/router selection across multiple candidate defaults follows preference/metric policy.
11. `replug_iface_preserves_or_relearns_scoped_default_route`
   - Verifies replugging an interface preserves valid scoped route or re-learns it cleanly through RA/RS.
12. `devtools_shows_ll6_and_router_scope`
   - Verifies event stream and devtools payloads include LLA and scope metadata and UI renders them.
13. `home_profile_ra_refresh_after_router_restart`
   - Verifies consumer-home profile recovers default-router and prefix state after router restart.
14. `mobile_profile_fast_default_router_reselection`
   - Verifies mobile-like profile handles default-router churn quickly and converges to a usable route.
15. `ra_task_lifecycle_matches_namespace_lifecycle`
   - Verifies RA worker tasks are created, restarted, and terminated correctly with namespace/router lifecycle transitions.
16. `router_iface_api_exposes_ll6_consistently`
   - Verifies new `RouterIface` snapshots and getters stay consistent with netlink-observed interface state.
    - Status: implemented.

Additional exhaustiveness tests:

17. `static_mode_does_not_run_ra_rs_tasks`
   - Verifies static provisioning mode does not start RA/RS workers and still provides deterministic routing.
18. `ra_disabled_router_emits_no_ra`
   - Verifies per-router RA disable truly suppresses advertisements even when global profile enables RA.
19. `multiple_prefixes_from_ra_install_expected_addresses`
   - Verifies host behavior when router advertises multiple prefixes, including address and route selection expectations.
20. `default_router_preference_changes_reorder_selection`
   - Verifies RFC 4191 preference updates change default-router choice without requiring interface bounce.
21. `iface_remove_cleans_scoped_default_route`
   - Verifies removing an interface removes stale scoped link-local default routes and cached router metadata.
22. `iface_add_relearns_ll6_and_default_router`
   - Verifies hot-added interfaces discover LLA and default router correctly via RS/RA path.
23. `rebooted_router_new_ll6_replaces_old_neighbor_and_route`
   - Verifies host recovers cleanly when router LLA changes across restart and old next hop becomes invalid.
24. `dual_uplink_failover_preserves_connectivity_with_ll_next_hop`
   - Verifies failover when primary uplink/router disappears and secondary scoped default route takes over.
25. `nonscoped_ll_connect_fails_with_clear_error`
   - Verifies downstream-facing helper APIs surface a clear error for link-local socket usage without scope.
26. `devtools_payload_backward_compatible_when_ll6_missing`
   - Verifies additive schema behavior when older runs or v4-only interfaces lack LLA fields.

Implemented in `patchbay/src/tests/ipv6_ll.rs` so far:

- `link_local_presence_on_all_ipv6_ifaces`
- `router_iface_api_exposes_ll6_consistently`
- `dad_disabled_deterministic_mode`
- `radriven_default_route_uses_scoped_ll_and_switches_iface`
- `radriven_link_up_restores_scoped_ll_default_route`

Validation commands before completion:

- `cargo make format`
- `cargo clippy -p patchbay --tests --fix --allow-dirty`
- `cargo check -p patchbay --tests`
- `cargo nextest run -p patchbay`
- `cd ui && npm run build` (if devtools payload/UI changes are included)

## Rollout Strategy

1. Land data model + route primitives first, behind compatibility guards.
2. Land RA/RS path as opt-in.
3. Land `RouterIface` and scoped-next-hop APIs as additive public changes.
4. Ship API/docs/devtools visibility updates so users can debug new behavior.
5. Switch presets to production-like defaults after test parity is proven.
6. Remove old behavior only after migration window and test updates are complete.

## Risks and Mitigations

- Test flakiness from DAD timing:
  - Mitigation: deterministic mode, bounded retries, explicit waiting for non-tentative state.
- Behavior drift across kernels:
  - Mitigation: netlink-level assertions in tests, avoid shell-only checks.
- Backward compatibility breaks in existing tests:
  - Mitigation: per-lab toggle and staged migration.
- Scope-handling regressions in downstream apps:
  - Mitigation: helper APIs, docs, and compile-time type hints where possible.
- RA timing sensitivity in CI:
  - Mitigation: controlled RA timers in tests and explicit timeouts.
- Background-task lifecycle bugs for RA/RS workers:
  - Mitigation: explicit ownership model in netns workers, teardown tests, and tracing instrumentation for task state.

## Deliverables

- Updated IPv6/link-local core behavior with scope-safe routing.
- RA/RS-capable provisioning path.
- Router interface public API (`RouterIface`) with LLA observability.
- Public API and devtools support for link-local observability.
- New docs and a dedicated link-local test suite.
- Production-like example scenarios for home, mobile, Linux, macOS, and Windows host behavior.

## Implementation Notes for Patchbay Architecture

This section maps planned work to current patchbay modules so implementation can start directly.

1. `patchbay/src/core.rs`
   - Extend interface and router state with `ll6` and optional address-state fields.
   - Replace unconditional DAD disable with mode-driven behavior.
   - Integrate scoped default-route installation paths.
   - Add RA/RS task lifecycle hooks at router/device setup and teardown points.
2. `patchbay/src/netlink.rs`
   - Add scoped IPv6 route helpers that bind gateway and output interface.
   - Add query helpers that return default route plus interface metadata.
   - Add address query helpers to read interface LLAs and flags.
3. `patchbay/src/handles.rs`
   - Add `RouterIface` value type analogous to `DeviceIface`.
   - Add `Router::iface` and `Router::interfaces` methods.
   - Add LLA/state/default-router getters to device and router iface snapshots.
4. `patchbay/src/lab.rs`
   - Extend `LabOpts` with IPv6 provisioning and DAD mode defaults.
   - Add builder options for RA behavior and profile selection.
5. `patchbay/src/event.rs` and `patchbay-server`
   - Add optional LLA and scoped-router fields in serialized events/state for devtools.
6. `ui/`
   - Display per-interface LLAs and scoped default-router details in node views.

## Feasibility Notes from Additional Research

1. Scoped route installation is feasible with Linux route semantics and current rtnetlink primitives.
   - Linux route model supports explicit `via + dev` behavior for scoped next hops.
   - Current patchbay already has device-route helpers and can be extended with scoped default-route helpers.
2. RA source constraints are straightforward to enforce.
   - RFC 4861 requires router advertisements to use link-local source addresses.
   - This requirement fits patchbay's per-interface router model once LLAs are observable.
3. DAD realism is feasible but requires controlled timing in tests.
   - Linux default behavior includes tentative states; deterministic test mode must remain available.
4. Full non-Linux host emulation is not feasible in-scope.
   - Patchbay runs Linux netns, so policy-profile emulation is the practical and correct target.

## Execution Checklist (PR-by-PR)

This is the concrete implementation order to reduce risk and keep each change reviewable.

### PR 1: Data model and scoped route primitives

Scope:

- Add core/interface fields for link-local visibility and route scope metadata.
- Add netlink helpers needed for scoped IPv6 default routes.
- Keep behavior unchanged by default, no RA/RS yet.

Files:

1. `patchbay/src/core.rs`
   - Add `ll6` and optional address-state fields in interface structs.
   - Add default-router scoped metadata fields (internal only for now).
2. `patchbay/src/netlink.rs`
   - Add scoped IPv6 default route add/replace helpers.
   - Add route query helper that returns gateway + device info.
   - Add helper(s) to query interface link-local IPv6 addresses.
3. `patchbay/src/handles.rs`
   - Add `DeviceIface::ll6()` and placeholder state getters if data exists.

Checks:

- `cargo check -p patchbay --tests`
- Add/adjust unit tests for netlink helper behavior where feasible.

Exit criteria:

- Link-local can be discovered from netlink and exposed on device iface snapshots.
- Scoped default-route helper compiles and is callable from core.

### PR 2: RouterIface public API and snapshot plumbing

Scope:

- Introduce `RouterIface` as a value-type snapshot API.
- Wire `Router::iface` and `Router::interfaces`.

Files:

1. `patchbay/src/handles.rs`
   - Add `RouterIface` struct and getters (`name`, `ip`, `ip6`, `ll6`, optional state).
   - Add `Router::iface(name)` and `Router::interfaces()`.
2. `patchbay/src/core.rs`
   - Ensure router interface snapshots can include new fields.
3. `patchbay/src/lib.rs`
   - Re-export `RouterIface` if needed.

Checks:

- `cargo check -p patchbay --tests`
- Add API-level tests for snapshot consistency.

Exit criteria:

- Downstream code can fetch router-side LLAs through stable public APIs.

### PR 3: LabOpts and builder knobs for provisioning and DAD modes

Scope:

- Extend `LabOpts` and router builder knobs without changing defaults yet.
- Add explicit policy profile config structs/enums.

Files:

1. `patchbay/src/lab.rs`
   - Extend `LabOpts` with IPv6 provisioning/DAD defaults.
   - Add profile and RA config options on builders.
2. `patchbay/src/config.rs`
   - Add TOML mapping for new knobs where relevant.
3. `patchbay/src/lib.rs`
   - Re-export new public enums/types.

Checks:

- `cargo check -p patchbay --tests`
- Add config parsing tests for new options.

Exit criteria:

- New knobs are accepted and serialized/loaded, with old behavior preserved by default.

### PR 4: DAD mode implementation and scoped default-route usage in core wiring

Scope:

- Replace unconditional DAD disable with mode-driven behavior.
- Switch IPv6 default-route install paths to scoped helpers for link-local gateways.

Files:

1. `patchbay/src/core.rs`
   - Gate DAD sysctl handling on mode.
   - Update iface/router setup paths to call scoped route helpers where needed.
2. `patchbay/src/netlink.rs`
   - Finalize any missing error handling and idempotency behavior.

Checks:

- `cargo check -p patchbay --tests`
- Add tests:
  - `dad_enabled_production_mode`
  - `dad_disabled_deterministic_mode`
  - `default_route_via_link_local_requires_dev_scope`

Exit criteria:

- DAD behavior is mode-dependent.
- Link-local default routes are scope-safe.

### PR 5: RA/RS engine on netns workers

Scope:

- Implement RA sender tasks on router interfaces and RS/default-router learning on hosts.
- Add lifecycle management hooks for start/stop/restart.

Files:

1. `patchbay/src/core.rs`
   - Add RA/RS task registration and teardown integration points.
2. `patchbay/src/netns.rs`
   - Add APIs needed to own long-lived namespace tasks safely.
3. `patchbay/src/lab.rs`
   - Trigger RA/RS initialization from topology/build lifecycle.

Checks:

- `cargo check -p patchbay --tests`
- Add initial RA/RS tests in `patchbay/src/tests/ipv6_ll.rs`:
  - `ra_source_is_link_local`
  - `host_learns_default_router_from_ra_link_local`
  - `router_lifetime_zero_withdraws_default_router`
  - `ra_task_lifecycle_matches_namespace_lifecycle`

Exit criteria:

- RA/RS works in opt-in mode and survives lifecycle churn.

### PR 6: Events/devtools observability and UI support

Scope:

- Surface `ll6` and scoped default-router metadata in events/state.
- Render metadata in devtools UI.
- Add schema/back-compat tests in `patchbay/src/tests/ipv6_ll.rs` and UI assertions as needed.

Files:

1. `patchbay/src/event.rs`
   - Add optional fields for LLA/scope metadata.
2. `patchbay-server/src/lib.rs`
   - Ensure serialization path includes new fields.
3. `ui/src/` (details + topology panes)
   - Display per-interface LLAs and scoped router info.

Checks:

- `cargo check -p patchbay-server`
- `cd ui && npm run build`
- Add/adjust UI tests if applicable.

Exit criteria:

- Devtools shows LLA and scope metadata without breaking existing views.
- Additive payload behavior is verified when LLA data is absent.

### PR 7: Presets, policy profiles, docs, and migration finalization

Scope:

- Wire policy profiles to presets.
- Update docs and examples.
- Decide default switch timing.

Files:

1. `patchbay/src/lab.rs` / preset definitions
   - Map presets to profile behavior.
2. `docs/reference/ipv6.md`
   - Add behavior contract and migration notes.
3. `docs/guide/*` as needed
   - Update examples to use `ll6`/scoped routes where relevant.
4. `plans/PLAN.md`
   - Move this plan through Partial -> Completed when done.

Checks:

- Full mandatory workflow from AGENTS.md:
  - `cargo make format`
  - `cargo clippy -p patchbay --tests --fix --allow-dirty`
  - `cargo check -p patchbay --tests`
  - `cargo nextest run -p patchbay`
  - `cargo check` (workspace)
  - `cd ui && npm run test:e2e` if UI changed
- All tests from Phase 5 matrix are implemented in `patchbay/src/tests/ipv6_ll.rs` and passing.

Exit criteria:

- Profiles and presets are documented, test-backed, and ready for default rollout decision.
- `ipv6_ll.rs` is the canonical link-local test module, and coverage is exhaustive for planned behavior.
