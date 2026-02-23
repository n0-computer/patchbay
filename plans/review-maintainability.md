# Maintainability Review — netsim-rs

Expert-level audit of every Rust source file. Findings are ordered by value.
Monoliths are fine; splitting is only proposed where concerns are genuinely
orthogonal and the boundary is clean.

---

## Status

- 1.1 Typed node IDs: ❌ (explicitly declined; keep `NodeId` aliases).
- 1.2 NAT mode cleanup: ✅ (single `NatMode` in `RouterConfig`, unified `apply_nat`).
- 1.3 Step enum: ✅ (tagged enum + shared fields via `StepShared`).
- 2.1 Netlink module split: ✅ (`src/netlink.rs`).
- 2.2 Runner split: ✅ (`src/sim/steps.rs`, `src/sim/progress.rs`).
- 3.1 Typed CIDR threading: ✅ (no string round-trips).
- 3.2 Remove `date` subprocess: ✅ (chrono UTC formatting).
- 3.3 Unique dir helper: ✅ (`create_unique_dir`).
- 3.4 Lookup helpers: ✅ (`resolve_device` / `resolve_router`).
- 3.5 Cleanup naming: ✅ (`cleanup_registered`, `cleanup_by_prefix`).
- 3.6 Filename/env sanitizers: ✅ (`src/util.rs`).
- 3.7 Dead `provider_args`: ✅ (removed).
- 4.1 `#![allow(dead_code)]`: ✅ (removed).

## 1. Type Safety

### 1.1 `NodeId` type aliases provide no safety

```rust
pub type DeviceId = NodeId;
pub type RouterId = NodeId;
pub type SwitchId = NodeId;
```

Transparent aliases — a `DeviceId` silently passes where a `RouterId` is
expected. `LabCore` has separate maps for each kind, so a mix-up is a
real bug.

**Proposal:** Phantom-typed newtypes:
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Id<T>(u64, PhantomData<T>);
pub type DeviceId = Id<Device>;
pub type RouterId = Id<Router>;
pub type SwitchId = Id<Switch>;
```
The compiler rejects cross-kind mistakes. The inner `u64` stays; all existing
`NodeId(x)` construction sites become `Id::new(x)` or similar thin wrapper.

### 1.2 `NatMode` has an `unreachable!()` in production

`apply_home_nat` panics on `NatMode::None` and `NatMode::Cgnat` via
`unreachable!()`. The caller guards against this with `if let Some(nat) =
router.cfg.nat`, but `RouterConfig.nat` is `Option<NatMode>` even though
`None` and `Cgnat` should never appear there. `RouterConfig.cgnat: bool` is a
separate field that encodes what's already in the `NatMode` variant.

**Proposal:** Collapse into a single `apply_nat` function that takes the full
`NatMode` enum and handles all variants. Remove `RouterConfig.cgnat` — the
`Cgnat` variant already carries that information. Internally:

```rust
fn apply_nat(ns: &str, wan_if: &str, wan_ip: Ipv4Addr, mode: NatMode) -> Result<()> {
    match mode {
        NatMode::None => Ok(()),
        NatMode::Cgnat => apply_cgnat(ns, wan_if).await,
        NatMode::DestinationIndependent => apply_snat(ns, wan_if, wan_ip).await,
        NatMode::DestinationDependent => apply_masquerade(ns, wan_if).await,
    }
}
```

The build path calls `apply_nat` unconditionally rather than branching on
`router.cfg.cgnat` then separately on `router.cfg.nat`. `RouterConfig`
simplifies to just `{ nat: NatMode, downlink_bridge: String, downstream_pool:
DownstreamPool }`.

### 1.3 `Step` struct is a flat optional-field grab-bag

`sim/mod.rs` — `Step` has ~20 fields, most only valid for one action. All
validation happens at runtime via `.context("run: missing device")?`.

**Proposal:** Typed `StepKind` enum — parse once, exhaustive match everywhere:
```rust
pub enum StepKind {
    Run    { device: String, cmd: Vec<String>, parser: Option<Parser> },
    Spawn  { id: String, device: String, cmd: Vec<String>,
             kind: SpawnKind, captures: HashMap<String, CaptureSpec>,
             env: HashMap<String, String>, ready_after: Option<Duration> },
    Wait   { duration: Duration },
    WaitFor { id: String, timeout: Duration },
    SetImpair   { device: String, ifname: Option<String>, impair: Option<Impair> },
    SwitchRoute { device: String, to: String },
    LinkDown    { device: String, interface: String },
    LinkUp      { device: String, interface: String },
    Assert      { check: String },
}
pub enum SpawnKind {
    Generic,
    IrohTransfer { provider: String, fetchers: Vec<String>,
                   relay_url: Option<String>, fetch_args: Vec<String>,
                   strategy: Option<String> },
}
pub struct Step { pub kind: StepKind }
```
The existing flat TOML schema is preserved via a custom `Deserialize` that reads
the `action` field first then populates the correct variant. The step executor
becomes a simple exhaustive `match step.kind { ... }` with no `bail!("unknown
step action")` and no runtime option-unwrapping for required fields.

---

## 2. Module Splits (selective)

### 2.1 Extract `Netlink` from `core.rs` → `src/netlink.rs`

The `Netlink` struct and its ~15 async methods (lines 1448–1646 of `core.rs`)
are fully self-contained — no back-references into `LabCore` or `ResourceList`.
Pulling them into `src/netlink.rs` cuts `core.rs` by ~200 lines and makes both
files easier to navigate.

`is_eexist` and `parse_cidr_v4` move with it (see §3.2).

### 2.2 Split `sim/runner.rs` — steps and progress into separate files

`runner.rs` at 1913 lines covers orchestration, step execution, relay cert
generation, progress/manifest JSON writing, binary resolution, and utilities.

**Proposal:**
- `sim/steps.rs` — `execute_step` and all its direct helpers (relay cert,
  capture reading, parser application, assert evaluation). ~700 lines.
- `sim/progress.rs` — `RunProgress`, `RunManifest`, `write_progress`,
  `write_run_manifest`, `build_run_manifest`. ~150 lines.
- `runner.rs` — orchestration only (`run_sims`, `run_single_sim`,
  `execute_single_sim`, binary resolution, utility fns). ~600 lines.

---

## 3. DRY and Code Cleanup

### 3.1 Remove `parse_cidr_v4` — thread typed values instead of strings

`core.rs:1658` manually splits a CIDR string and re-parses the parts.
All call sites format an `Ipv4Addr` and a prefix length to a string, then
immediately re-parse them. The crate already has `ipnet`.

**Proposal:** Change `Netlink::add_addr4` to accept `(Ipv4Addr, u8)` directly
(or `Ipv4Net`). Remove `parse_cidr_v4`. All callers already have the typed
values in hand.

### 3.2 `format_timestamp` and `now_stamp` both shell out to `date`

`runner.rs:808` spawns `date -u -d@<secs>` for RFC 3339 formatting.
`now_stamp` (line 928) spawns `date +%y%m%d-%H%M%S` for directory naming.
Both have fragile fallbacks for when `date` fails.

**Proposal:** Pure-Rust implementations — straightforward integer arithmetic,
no subprocess, no fallback needed.

```rust
fn format_rfc3339(ts: SystemTime) -> String { /* secs / 60 / 60 / ... */ }
fn now_stamp() -> String { /* same, different format */ }
```

### 3.3 `prepare_run_root` / `prepare_sim_dir` are near-identical

Both functions: create-dir, collision-resolve with an incrementing suffix,
return the `PathBuf`. The only difference is the base string.

**Proposal:** `fn create_unique_dir(parent: &Path, base: &str) -> Result<PathBuf>`,
called by both.

### 3.4 Repeated `"unknown device/router"` lookup pattern — 12+ sites

```rust
self.device_by_name.get(name).copied()
    .ok_or_else(|| anyhow!("unknown device '{}'", name))?
```

**Proposal:** `Lab::resolve_device(name) -> Result<DeviceId>` and
`Lab::resolve_router(name) -> Result<RouterId>`. Every method that looks up by
name calls these. Error messages become consistent; call sites lose 2–3 lines each.

### 3.5 `cleanup_all` / `cleanup_everything` naming confusion

`ResourceList` has `cleanup_all`, `cleanup_everything_with_prefix`,
`cleanup_everything` — the all/everything distinction is meaningless.
`main.rs::perform_cleanup` calls all three in a non-obvious order.

**Proposal:** Rename to two clear methods:
- `cleanup_registered()` — deletes explicitly registered links and namespaces (normal teardown)
- `cleanup_by_prefix(prefix: &str)` — scans `ip link show`, for crash recovery

### 3.6 `sanitize_for_filename` duplicated across files

`runner.rs` has `sanitize_for_filename`, `main.rs` has `env_key_suffix`,
and `ensure_relay_runtime_assets` has an inline version. All do the same
character-mapping transform.

**Proposal:** One function in `runner.rs` (or a small `util` inline module
in `lib.rs`); re-export where needed. Remove duplicates.

### 3.7 Dead `provider_args` Vec in `transfer.rs`

`transfer.rs` builds both a `provider_args: Vec<String>` and passes the same
arguments to `provider_cmd.args(...)` directly. Nothing consumes `provider_args`.
Leftover from an earlier refactor.

**Proposal:** Remove `provider_args` entirely.

---

## 4. Minor Idiom Fixes

### 4.1 Remove `#![allow(dead_code)]` from `lib.rs`

Silences all dead-code warnings library-wide, hiding real rot.

**Proposal:** Remove it. Fix or annotate individual items that genuinely need
suppression (e.g. public API items not yet exercised in tests).

### 4.2 Remove `Netlink::ops` / `bump()`

The `ops: u64` counter is incremented on every netlink call but never read or
surfaced. Debugging artifact.

**Proposal:** Delete `ops`, `bump()`, and all `self.bump()` calls.

### 4.3 `Switch.next_host` visibility

`next_host` is a private allocator counter that has leaked into the `Switch`
struct with no visibility annotation (defaults to `pub` within the module,
visible inside `core.rs`). It has no business being part of the observable
struct returned by `LabCore::switch()`.

**Proposal:** Make it `pub(super)` or restructure to keep it fully inside
the allocation logic. Long-term: separate `Switch` (observable state) from
an internal `SwitchAlloc` (counter) held in a parallel map in `LabCore`.

### 4.4 `is_some()` + `unwrap()` in `connect_router_downlink`

```rust
if sw_entry.cidr.is_some() {
    let cidr = sw_entry.cidr.unwrap();
```

**Proposal:** `if let Some(cidr) = sw_entry.cidr { ... }`.

### 4.5 `let _ =` discards on meaningful return values

`connect_router_downlink` and `connect_router_uplink` return `(Ipv4Net,
Ipv4Addr)` / `Ipv4Addr` but callers discard with `let _ = ...`. The values
are stored inside the function, so the discard is intentional, but `let _ =`
implies an ignored error.

**Proposal:** If the caller never needs the return value, change the return
type to `Result<()>` and store internally. Otherwise name the binding
`let _result = ...` to signal intentional discard.

---

## 5. Cargo / Workspace

### 5.1 `tokio = { features = ["full"] }` — over-broad

`full` pulls in `rt-multi-thread`, `io-std`, and other unused features.

**Proposal:** Use explicit features: `["rt", "net", "fs", "task", "time",
"io-util", "macros"]`. Reduces compile time and binary size.

### 5.2 `n0-tracing-test` in `[dependencies]` not `[dev-dependencies]`

Test helper linked into production binary.

**Proposal:** Move to `[dev-dependencies]`.

### 5.3 Audit unused deps

`dirs = "5"` is listed in the main crate but no `dirs::` usage is present.

**Proposal:** Run `cargo machete` / `cargo udeps` and drop unused deps.

---

## 6. Summary Priority Table

| # | Location | Issue | Effort | Impact |
|---|----------|-------|--------|--------|
| 1.1 | `core.rs` | Phantom-typed `Id<T>` newtypes | Low | High (safety) |
| 1.2 | `core.rs` | `apply_nat`, remove `cgnat` bool | Med | High (correctness) |
| 1.3 | `sim/mod.rs` | `StepKind` enum replacing flat `Step` | High | High (correctness, DX) |
| 2.1 | `core.rs` | Extract `Netlink` → `src/netlink.rs` | Low | Med (clarity) |
| 2.2 | `sim/runner.rs` | Split into `steps.rs` + `progress.rs` | Med | Med (navability) |
| 3.1 | `core.rs` | Remove `parse_cidr_v4`, thread typed values | Low | Med |
| 3.2 | `runner.rs` | Pure-Rust `format_timestamp` / `now_stamp` | Low | Low |
| 3.3 | `runner.rs` | Deduplicate `prepare_*_dir` | Low | Low |
| 3.4 | `lib.rs` | `resolve_device`/`resolve_router` helpers | Low | Med |
| 3.5 | `core.rs` | Rename cleanup API | Low | Med |
| 3.6 | multiple | Deduplicate `sanitize_*` functions | Low | Low |
| 3.7 | `transfer.rs` | Remove dead `provider_args` Vec | Low | Low |
| 4.1 | `lib.rs` | Remove `#![allow(dead_code)]` | Low | Med |
| 4.2 | `core.rs` | Remove `Netlink::ops`/`bump` | Low | Low |
| 4.3 | `core.rs` | `Switch.next_host` visibility | Low | Low |
| 4.4 | `core.rs` | `if let` instead of `is_some`+`unwrap` | Low | Low |
| 4.5 | `core.rs` | `let _ =` on meaningful returns | Low | Low |
| 5.1 | `Cargo.toml` | Replace `tokio full` with explicit features | Low | Low |
| 5.2 | `Cargo.toml` | `n0-tracing-test` → dev-deps | Low | Low |
| 5.3 | `Cargo.toml` | Remove unused deps (`dirs`) | Low | Low |

---

## 7. Suggested Implementation Order

1. **Cargo housekeeping** (§5) — zero-risk, zero-churn.
2. **Remove `#![allow(dead_code)]`** (§4.1) — surfaces anything truly dead before more changes.
3. **Phantom-typed `Id<T>`** (§1.1) — compiler-enforced correctness, low churn.
4. **`apply_nat` / remove `cgnat` bool** (§1.2) — clean up the NAT wiring path.
5. **`resolve_device`/`resolve_router` + cleanup rename** (§3.4, §3.5) — reduce boilerplate.
6. **Extract `Netlink` → `netlink.rs`; remove `parse_cidr_v4`** (§2.1, §3.1) — pairs naturally.
7. **Minor idiom fixes** (§4.2–4.5, §3.2–3.3, §3.6–3.7) — batch these.
8. **Split `runner.rs`** (§2.2) — structural, do on a clean branch.
9. **`StepKind` enum** (§1.3) — most invasive, do last with good test coverage.
