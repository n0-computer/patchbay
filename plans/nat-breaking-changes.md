# NAT refactor: breaking changes

Tracks every breaking change for the final PR description.

## `Nat` enum

Replaced deployment-flavored variants with a three-tier behavior
gradient:

| Old | New |
|-----|-----|
| `Nat::Home` | `Nat::Easy` |
| `Nat::Cgnat` | `Nat::Easiest` |
| `Nat::Corporate`, `Nat::CloudNat` | `Nat::Hard` |
| `Nat::FullCone` | `Nat::Easiest` plus `hairpin(true)` if needed |
| `Nat::None`, `Nat::Custom` | unchanged |

The new variants: `None`, `Easiest` (EIM+EIF), `Easy` (EIM+APDF), `Hard`
(EDM+APDF with random ports), and `Custom(NatConfig)`.

## `NatMapping`

`EndpointIndependent` and `EndpointDependent`, both unit variants. An
earlier draft of this PR carried a `PortPreservation` payload on
`EndpointDependent` to distinguish port-preserving symmetric NAT from
random symmetric NAT. That distinction was dropped because Linux
`nftables` cannot produce port-preserving symmetric NAT distinguishably
from EIM-under-light-load. See `docs/reference/nat-limitations.md`.

## `NatFiltering`

Gained `AddressDependent` variant for RFC 4787 ADF (RFC 3489 "Restricted
Cone"). Only supported with `NatMapping::EndpointIndependent`; the
builder rejects `EDM + AddressDependent`.

## `NatConfigBuilder::build()`

Now returns `Result<NatConfig, NatConfigError>`. Rejects:

- `EndpointDependent` mapping with `AddressDependent` filtering
- `EndpointDependent` mapping with `hairpin = true`

Previously both combinations compiled and produced subtly wrong
nftables rules.

## `NatConfig` and `ConntrackTimeouts`

Both are `#[non_exhaustive]`. Construction is through
`NatConfig::builder()` only. `NatConfig` has public fields for ergonomic
read access and pattern matching; mutating fields post-build bypasses
builder validation and is documented as caller responsibility.

## `RouterPreset`

- `IspCgnat` changed from EIM+EIF to EIM+APDF (most RFC 6888 compliant
  CGNATs use APDF, not EIF), firewall `None` to `BlockInbound`.
- `IspCgnatHard` renamed to `IspCgnatSymmetric`, RFC 7753 citation
  dropped (that RFC is a PCP extension, not a PBA RFC; the preset does
  not model Port Block Allocation). Firewall `None` to `BlockInbound`.
- `MobileCarrier` (new): EDM+APDF, 60-second UDP stream timeout,
  `BlockInbound` firewall.
- `IspCgnatSymmetric` and `MobileCarrier` both map to `Nat::Hard`
  (symmetric NAT with random ports). An earlier draft simulated these
  as port-preserving symmetric NAT; patchbay does not do that
  distinctly.

## `Router` API

- `Router::nat_mode` renamed to `Router::nat_config`, returns
  `Option<NatConfig>` (flattened). Matches peer accessors.
- `Router::set_nat_mode` renamed to `Router::set_nat`. Matches
  `Router::set_firewall`. Accepts `impl Into<Option<NatConfig>>`.

## `RouterBuilder::nat`

Accepts `impl Into<Option<NatConfig>>`. `Nat::None` and `None` both
disable NAT.

## Wire format

`RouterState.nat` and `LabEventKind::NatChanged.nat` serialize as
`Option<NatConfig>`:

```
// before
"nat": "home"
// after
"nat": {
  "mapping": "endpoint_dependent",
  "filtering": "address_and_port_dependent",
  "timeouts": { "udp": 30, "udp_stream": 300, "tcp_established": 7200 },
  "hairpin": false
}
// or when NAT is disabled
"nat": null
```

## TOML

`[[router]] nat = "..."` accepts `none`, `easiest`, `easy`, `hard`,
`custom`. Old strings (`home`, `corporate`, `cgnat`, `cloud-nat`,
`full-cone`, `hardest`) fail to parse.

## TypeScript devtools bindings

`ui/src/devtools-types.ts`: `Nat = NatConfig | null`; `NatMapping` is a
union of unit string literals; `NatFiltering` gained
`"address_dependent"`.

## Test coverage added

- `adf_allows_different_port_from_contacted_host` (positive) and
  `adf_drops_from_uncontacted_host` (negative): validate ADF
  semantics.
- `preset_nat_snapshots`: pins mapping, filtering, UDP stream timeout,
  firewall, IP support, v6 NAT mode, and `hairpin = false` for every
  preset.
- `builder_rejects_edm_with_adf`, `builder_rejects_edm_with_hairpin`,
  plus matching positive cases.
- `nat_easiest_is_eim_eif`, `nat_easy_is_eim_apdf`,
  `nat_hard_is_edm_apdf`: pin the `Nat::*` conversions.

## Documentation

- New `docs/reference/nat-limitations.md` explaining what classes of
  NAT patchbay does not simulate faithfully (SYMPP, sequential port
  allocation, vendor quirks, behavior under load, IPv6 corner cases)
  and how a future backend could close each gap.
- Preset tables updated in `docs/guide/topology.md`, `lib.rs`,
  `docs/guide/nat-and-firewalls.md`, `docs/reference/holepunching.md`,
  and `docs/reference/toml-reference.md`.
