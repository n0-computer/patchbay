# AGENTS.md

Project: `netsim-rs`

This file captures key context, conventions, and workflows learned while working on this repo. It is intended to help other agents onboard quickly and avoid repeated mistakes.

## Overview
- Rust library + binary for building Linux network-namespace labs (routers, switches, devices).
- Uses `rtnetlink` for link setup and `tc`/`nft` for impairment + NAT.
- Core types: `LabCore` in `src/core.rs`, high-level API in `src/lib.rs`.
- Additional module: `src/qdisc.rs` encapsulates all `tc qdisc` usage.

## Key Concepts / Architecture
- **LabCore**: low-level topology and build logic (routers/switches/devices).
- **Lab**: convenience wrapper with dc/home/isp shorthands; maps to `LabCore`.
- **Lab root namespace**: IX and transit links are built in a dedicated lab namespace (`<prefix>-root`), not host root.
- **Netns backend**: `src/netns.rs` selects backend (`fd` default, `named`, `auto`) and provides create/open/cleanup.
- **Namespaces**: `fd` backend uses in-memory namespace FD registry; `named` backend uses `ip netns add`.
- **Netlink**: `Netlink` struct in `src/core.rs` wraps `rtnetlink::Handle` and provides helper methods.
- **Qdisc**: all `tc`/`qdisc` command invocation is in `src/qdisc.rs`.
- **Resource cleanup**: `ResourceList` tracks bridges + netns and tries to clean on exit/panic.

## Permissions / Running Without Root
Root is no longer strictly required. Use capabilities:
- Required caps: `CAP_NET_ADMIN`, `CAP_SYS_ADMIN`, `CAP_NET_RAW`.
- Use `./setcap.sh` to grant caps to:
  - `ip`, `tc`, `nft` binaries (if present).
  - built `netsim` binaries and test binaries.
- Rebuilds drop caps; re-run `./setcap.sh` after rebuild.
- New `check_caps()` in `src/lib.rs` is used instead of `require_root()` in tests and `main`.

## Local Tasks (cargo-make)
`Makefile.toml` provides tasks:
- `run-local`: runs `./setcap.sh` then `cargo run -- ${ARGS}`.
- `test-local`: runs `./setcap.sh` then `cargo test -- ${ARGS}`.
- `target-dir`: prints effective target dir.
  - Uses `RUST_TARGET` if set, otherwise `rustc -vV` host.
  - Base uses `${CARGO_MAKE_TARGET_DIR}` or `<workspace>/target`.

## VM (Lima) Tasks
Lima config: `lima.yaml` (Debian Trixie).
Provisioning installs `iproute2`, `nftables`, etc.

VM tasks in `Makefile.toml`:
- `setup`: start or create `netsim-vm`.
- `build-vm`: `cargo build --release --target x86_64-unknown-linux-musl`.
- `build-test-vm`: `cargo test --no-run --target x86_64-unknown-linux-musl`.
- `run-vm`: build, bind-mount target dir into VM as `/target`, then execute binary.
- `test-vm`: build tests, bind-mount target dir, run test binaries in VM.
- `shutdown`: stop VM.

**Mounting:** The target dir is bind-mounted at runtime using:
- `TARGET_DIR=$(RUST_TARGET=... cargo make --quiet target-dir)`
- `realpath --relative-to="$PWD" "$TARGET_DIR"` ensures target is under `/app`.
- Mounted in VM: `/app/<rel>` -> `/target`.

## Qdisc / Impairments
All tc usage is in `src/qdisc.rs`.
- `apply_impair(ns, ifname, limits)` builds netem + optional tbf.
- `apply_region_latency` builds HTB root + netem classes + filters.
- HTB uses `r2q 1000` to suppress quantum warnings.
- `Impair::Wifi` and `Impair::Mobile` map to `ImpairLimits` before invoking qdisc.

## Naming / Prefixes
Lab uses a process-unique prefix like `lab-p####`.
Bridges use `br-p####-N` (shorter names).
Namespaces use `lab-p####-N`.

## Common Pitfalls
- **Netns creation**: prefer `ip netns add` for stable `/var/run/netns/*` entries.
- **Host root leakage**: never run lab dataplane operations in host root netns; keep all IX/transit operations inside the dedicated lab root namespace.
- **Capabilities**: running tests without caps will fail; use `check_caps()`.
- **`no_new_privs`**: if launcher/container sets `no_new_privs=1`, file capabilities from `setcap` will not be granted at exec time.
- **`ip netns add` limits**: named netns creation depends on mount operations (`--make-shared` + bind mounts under `/var/run/netns`) and can fail on mount-restricted hosts; keep FD-backend fallback available.
- **TC warnings**: use `r2q 1000` in HTB root to avoid large quantum warnings.
- **Makefile target dir**: do not assume `./target`, always use `cargo make target-dir`.

## File Map
- `src/lib.rs`: public API, tests, `check_caps`.
- `src/core.rs`: core topology + build, netlink helpers.
- `src/netns.rs`: namespace backend selection + lifecycle helpers.
- `src/qdisc.rs`: tc/qdisc abstraction, netem/tbf/htb.
- `src/main.rs`: demo CLI; calls `check_caps()`.
- `Makefile.toml`: local + VM tasks.
- `lima.yaml`: VM definition.
- `setcap.sh`: capability setup script.

## Notes on Tests
Tests use `#[tokio::test(flavor = "current_thread")]` due to `setns` thread-local behavior.
Many tests are serial (`serial_test`) because they manipulate global network state.

## Useful Commands
Local:
```
./setcap.sh
cargo test
```

VM:
```
cargo make run-vm
cargo make test-vm
```

## General instructions

all plans are in plans/. keep overview of plans in plans/PLAN.md. 
document important findings and changes in AGENTS.md
always document public items, strictly adhere to official rust doc conventions and naming conventions
run cargo check, cargo clippy --tests --examples --fix, cargo fmt before each commit (and require to be clean)
when a task is ready run the checks then ask to commit, don't commit without asking, but stage files already.
after confirmation commit with "feat: short description" etc and some details afterwards. elaborate open issues a little, explain decisions taken concisely

## Recent Changes
- Completed public API doc coverage audit: all public library items now have rustdoc comments (verified with `RUSTFLAGS='-W missing-docs' cargo check`).
- Updated stale runtime docs: `Lab::build`/`Lab::load` no longer claim a `current_thread` Tokio requirement, matching the worker-thread `NetnsManager` `setns` model.
- Namespace entry is centralized: `setns(2)` is now only invoked in `src/netns.rs` worker threads (`NetnsManager`), while `src/core.rs` uses backend helpers and does not call `setns` directly.
- Public naming cleanup in namespace/process helpers:
  - `src/core.rs` now exposes canonical `*_in_namespace` helper names with compatibility wrappers for existing `*_in_netns`/`with_netns_thread` call sites.
  - `src/lib.rs` now uses canonical `Lab::run_on`, `Lab::run_in_namespace`, `Lab::run_in_namespace_thread`, and `Lab::spawn_unmanaged_on`, with backward-compatible aliases retained.
- `setcap.sh` now resolves system tool paths with elevated lookup (`sudo env PATH=... which <tool>`) when running unprivileged, so the same resolver context is used for lookup and subsequent `setcap`.
- Fixed local capability lookup regression: `setcap.sh` now resolves system tools with `which` under an augmented search path (`/usr/sbin:/sbin:/usr/bin:/bin`) so `ip`/`tc`/`nft` are found even when user PATH omits `sbin`.
- Local capability setup now also applies caps to `ping`/`ping6` (plus `/bin/*` aliases for `ip`/`tc`/`nft`) and no longer ignores setcap failures for these tools; this fixes non-sudo local ping/route permission failures in tests.
- Relaxed `dynamic_set_impair_changes_rtt` assertion from `+80ms` to `+40ms` RTT delta to align with observed single-path netem behavior in VM/local runs.
- Simplified FD netns backend internals: removed keeper-thread state from `FdRegistry`; namespace lifetime is now tied directly to stored namespace FDs.
- Fixed VM test runner stale-binary issue: `build-test-vm` now deletes old executable `deps/${crate}-*` test binaries before `cargo test --no-run`, so `test-vm` no longer executes outdated artifacts.
- `Lab::new` now appends a process-local atomic counter suffix to prefixes/bridge tags (`lab-p<pid><n>`, `br-p<pid><n>-*`) so concurrent labs in one process do not collide on netns/link names.
- Updated the iroh netsim plan to use `--log-path`, generate `results.json` and `results.md`, and add named binaries with `endpoint_id_only` / `endpoint_id_with_direct_addrs` strategies; `IROH_DATA_DIR` stays unset.
- Drafted iroh integration workflow + example sims and topos under `iroh-integration/` with transfer duration set to 20s and topology files colocated in `iroh-integration/topos`.
- Refactored namespace execution to a dedicated `NetnsManager` (`src/netns.rs`) that keeps one long-lived worker thread + single-thread Tokio runtime per namespace and executes async closures there, with panic forwarding through task join errors.
- Updated `set_sysctl_in` / `spawn_in_netns_thread` (`src/core.rs`) to avoid restoring the original netns on helper-thread exit; helper threads now stay in target netns for their lifetime and then terminate.
- Added `smoke_debug_netns_exit_trace` test (`src/lib.rs`) to emit deep namespace diagnostics (inode, links, IPv4 addrs, routes) pre-build, on build error, and post-build.
- FD netns backend now keeps a per-namespace keeper thread alive (`src/netns.rs`) so unnamed namespaces stay anchored and distinct; cleanup sends stop and joins keeper threads.
- Fixed FD netns capture to open thread-local namespace FDs (`/proc/thread-self/ns/net` with `/proc/self/task/<tid>/ns/net` fallback) instead of `/proc/self/ns/net`, which can resolve to thread-group leader ns in worker threads.
