# NAT refactor: breaking changes

This file tracks every breaking change introduced by the NAT taxonomy
redesign so the final PR description can cite them accurately.

## Public API

### `Nat` enum variants renamed

Replaced deployment-flavored variants with a behavior gradient:

| Old | New |
|-----|-----|
| `Nat::Home` | `Nat::Easy` |
| `Nat::Corporate` | `Nat::Hardest` |
| `Nat::Cgnat` | `Nat::Easiest` |
| `Nat::CloudNat` | `Nat::Hardest` |
| `Nat::FullCone` | `Nat::Easiest` plus `hairpin(true)` if hairpin is needed |
| `Nat::None`, `Nat::Custom` | unchanged |

### `Nat::to_config()` returns `Option<NatConfig>`

Restored as a thin alias for `impl From<Nat> for Option<NatConfig>`.
Timeouts come from `ConntrackTimeouts::default()`; per-deployment
timeouts come from the matching `RouterPreset`.

### `NatMapping` enum shape changed

`EndpointDependent` now carries a `PortPreservation` payload. The
previous unit variant no longer exists.

```rust
// before
NatMapping::EndpointDependent
// after
NatMapping::EndpointDependent(PortPreservation::Preserve)
NatMapping::EndpointDependent(PortPreservation::Random)
```

This makes EIM+Random structurally unrepresentable, replacing the old
silently-ignored combination.

### `NatFiltering` gained `AddressDependent`

Third variant for RFC 4787 Address-Dependent Filtering (RFC 3489
"Restricted Cone"). nftables backend uses a dynamic `@contacted` set of
external IPs the internal side has reached and a matching rule in the
forward chain.

### `PortPreservation` added

Two variants: `Preserve` and `Random`. Only reachable via
`NatMapping::EndpointDependent(...)`. EIM always preserves ports and
offers no configuration.

### `NatConfigBuilder::build()` returns `Result<NatConfig, NatConfigError>`

The builder now validates cross-field invariants and returns an error
for:

- `NatMapping::EndpointDependent(_)` with `NatFiltering::AddressDependent`.
  The ADF implementation requires the fullcone map to deliver inbound
  packets from any port on a contacted address.
- `NatMapping::EndpointDependent(_)` with `hairpin = true`. The hairpin
  DNAT rule relies on the fullcone map.

Previously both combinations compiled and produced subtly wrong
nftables rules.

### `NatConfig` is `#[non_exhaustive]`

Construction through `NatConfig::builder()` is the stable path. Direct
struct literals outside the crate no longer compile. `ConntrackTimeouts`
is also `#[non_exhaustive]`.

### `RouterPreset` changes

- Added `IspCgnatSymmetric`: EDM + APDF + Preserve, 180-second UDP
  stream timeout, `BlockInbound` firewall.
- Added `MobileCarrier`: EDM + APDF + Preserve, 60-second UDP stream
  timeout, `BlockInbound` firewall.
- `IspCgnat` changed from EIM + EIF to EIM + APDF with `BlockInbound`
  firewall. Published measurement data (ipSpace, IMC'16, and observed
  behavior on Swisscom, Deutsche Telekom, Starlink) supports EIM+APDF
  as the typical RFC 6888 compliant deployment; EIF is rare.
- `IspCgnat` and `IspCgnatSymmetric` firewalls changed from
  `Firewall::None` to `Firewall::BlockInbound` so IPv6 inbound is
  blocked by default, matching Swisscom, Deutsche Telekom, Starlink.
- No variant named `IspCgnatHard`; the earlier draft used that name and
  cited RFC 7753. The preset does not model Port Block Allocation, so
  the name and citation were dropped.

### `Router::nat_mode` renamed to `Router::nat_config`

Returns `Option<NatConfig>` (flat), matching the pattern of peer
accessors like `mtu()`, `uplink_ip()`, and `downstream_cidr()`. `None`
covers both "router removed" and "NAT disabled". Use `exists` /
`uplink_ip` checks if you need to distinguish.

### `Router::set_nat_mode` renamed to `Router::set_nat`

Matches the naming of `Router::set_firewall`. Accepts
`impl Into<Option<NatConfig>>`; `Nat`, `NatConfig`, `None`, and
`Some(config)` all compile.

### `RouterBuilder::nat` signature widened

Accepts `impl Into<Option<NatConfig>>`. `Nat::None` and `None` both
disable NAT.

### `PortPreservation` and `NatConfigError` re-exported from the crate
root

Previously unreachable for downstream users even though they appeared in
public function signatures.

## Internal changes

### `RouterConfig.nat` stores `Option<NatConfig>`

Was `Nat`. Call sites that set or read the field use the expanded
config directly. `RouterConfig::effective_nat_config()` was deleted;
call sites read `router_cfg.nat` directly.

### `set_nat` deletes and re-creates tables instead of flushing

Dynamic nftables sets (`@contacted` for ADF) survive `flush table`. The
runtime path now does `delete table` before reapplying, so mode
transitions start from a clean state.

## Wire format changes

### `RouterState.nat`

JSON shape changed from a kebab-case preset string to a config object
or `null`:

```
// before
"nat": "home"

// after
"nat": {
  "mapping": { "endpoint_dependent": "preserve" },
  "filtering": "address_and_port_dependent",
  "timeouts": { "udp": 30, "udp_stream": 300, "tcp_established": 7200 },
  "hairpin": false
}

// or when NAT is disabled
"nat": null
```

The `mapping` field is either the string `"endpoint_independent"` or an
object like `{ "endpoint_dependent": "preserve" }` because
`NatMapping::EndpointDependent` carries `PortPreservation`.

### `LabEventKind::NatChanged.nat`

Same change as `RouterState.nat`.

### TOML config

The `[[router]] nat = "..."` field still accepts a `Nat` enum but the
accepted strings are now `none`, `easiest`, `easy`, `hard`, `hardest`,
`custom`. Old strings (`home`, `corporate`, `cgnat`, `cloud-nat`,
`full-cone`) fail to parse.

## Test coverage added

- `port_mapping_edm_preserve_stable`: validates that `Nat::Hard`
  (`masquerade` without the `random` flag) preserves the internal source
  port across destinations. If this test ever fails on a new kernel,
  the `Nat::Hard`/`Nat::Hardest` distinction must be re-examined.
- `adf_allows_different_port_from_contacted_host`: positive path for
  `NatFiltering::AddressDependent`. The contacted peer sends from a
  different source port and the packet reaches the device.
- `adf_drops_from_uncontacted_host`: negative path for ADF. An
  uncontacted external host sends to the mapped address and the packet
  is dropped.
- `preset_nat_snapshots`: validates mapping, filtering, port
  preservation, and UDP stream timeout for every `RouterPreset`. Changes
  here must match the docs in `docs/guide/nat-and-firewalls.md` and the
  TOML reference.
- `builder_rejects_edm_with_adf`, `builder_rejects_edm_with_hairpin`,
  plus the matching positive cases: validate that the builder's
  cross-field invariants hold.
- `nat_easiest_is_eim_eif`, `nat_easy_is_eim_apdf`,
  `nat_hard_is_edm_preserve_apdf`, `nat_hardest_is_edm_random_apdf`:
  pins the `Nat::*` preset expansions.
