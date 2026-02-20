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
- **Namespaces**: created and managed via `ip netns add` (see `create_named_netns`).
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
- **TC warnings**: use `r2q 1000` in HTB root to avoid large quantum warnings.
- **Makefile target dir**: do not assume `./target`, always use `cargo make target-dir`.

## File Map
- `src/lib.rs`: public API, tests, `check_caps`.
- `src/core.rs`: core topology + build, netlink helpers.
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
