# AGENTS.md

Project: **patchbay** - Linux network-namespace lab for NAT, routing, and link-condition experiments.

This file is the single entry point for agents. Read it fully before working. Follow all rules below; they are mandatory, not suggestions.

## Key Resources (read as needed)

| File | Purpose |
|------|---------|
| [`AGENTS.md`](AGENTS.md) | **You are here.** Architecture, conventions, mandatory workflow. |
| [`plans/PLAN.md`](plans/PLAN.md) | Plan index with in-progress, open, partial, and completed plans. |
| [`REVIEW.md`](REVIEW.md) | Open and completed review items. |
| [`docs/reference/holepunching.md`](docs/reference/holepunching.md) | NAT implementation: fullcone maps, APDF filtering, nftables lessons. |
| [`HISTORY.md`](HISTORY.md) | Chronological changelog (moved from old AGENTS.md). |
| [`docs/`](docs/) | IPv6 deployments, network patterns, holepunching, TOML reference. |

---

## Wording Rules

All prose in this project (markdown files, doc comments, UI strings) follows these rules. They are MUSTs unless a specific context dictates otherwise.

- **No em dashes, no unicode symbols in regular prose.** Prefer commas or semicolons for inline asides. A single `-` is fine occasionally but don't lean on it; restructure the sentence instead. Use `->` for arrows, plain ASCII quotes. Unicode box-drawing characters are fine when you are actually drawing diagrams.
- **Write full sentences.** Not `the, words, fullstop.` style fragments. Concise is good and headings can differ, but body text uses complete sentences.
- **Sound like a professional, enthusiastic engineer telling nerdy friends about things.** You are not an LLM, a documentation machine, a startup marketer, or a corporate enterprise seller.
- **Accessible to newcomers, precise for professionals.** Explain concepts clearly enough that someone new can follow, but use correct technical terms so experts get exactly what they need.
- **Doc comments follow Rust conventions.** First line is a single-sentence summary. Use `# Examples`, `# Errors`, `# Panics` sections where appropriate. See the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/documentation.html).

---

## Architecture

### Crate: `patchbay`

The library crate. All network simulation logic lives here.

- **`src/core.rs`** - `NetworkCore` holds topology state: router/device/switch records, NAT rule generation, and nftables helpers.
- **`src/lab.rs`** - Public API surface: `Lab`, `Device`, `Router`, `Ix` handles; builders (`RouterBuilder`, `DeviceBuilder`); types (`Nat`, `NatConfig`, `LinkCondition`, `LinkLimits`, `IpSupport`, etc).
- **`src/handles.rs`** - `Device`, `Router`, and `Ix` handle implementations with cached fields and `with()`/`with_mut()` helpers.
- **`src/netns.rs`** - `NetnsManager` runs two workers per namespace (async tokio + sync thread). All `setns(2)` calls happen here.
- **`src/qdisc.rs`** - All `tc` command invocation: netem (latency/jitter/loss/reorder/duplicate/corrupt), TBF (rate), HTB (region latency).
- **`src/netlink.rs`** - `Netlink` struct wrapping `rtnetlink::Handle` for link/addr/route operations.
- **`src/nat.rs`** - NAT rule generation from `NatConfig` (mapping + filtering + timeouts).
- **`src/firewall.rs`** - Firewall rule generation from `FirewallConfig`.
- **`src/test_utils.rs`** - UDP reflector/probe helpers for integration tests.
- **`src/tests.rs`** - Integration test suite (~108 tests).
- **`src/config.rs`** - TOML config structures for `Lab::load`.
- **`src/userns.rs`** - ELF constructor bootstrap into an unprivileged user namespace.
- **`src/lib.rs`** - Re-exports and `check_caps()`.

### Key Design Rules

1. **Never block a tokio thread with TCP/UDP I/O.** Use `spawn_task_in_netns` + `tokio::net` + `tokio::time::timeout`.
2. **Sync `run_closure_in` is for fast non-I/O work only** (sysctl, `Command::spawn`).
3. **NAT rules are generated from `NatConfig`**, not from `Nat` variants directly. `Nat::to_config()` expands presets into a mapping + filtering + timeouts struct.
4. **Tests use `#[tokio::test(flavor = "current_thread")]`** because `setns` is thread-local.

### Workspace

```
patchbay/         - Library crate (main development target)
patchbay-utils/   - CLI utilities
patchbay-runner/  - Binary crate (sim runner, inspect)
patchbay-vm/      - VM orchestration
ui/               - Vite + React browser UI
```

### Permissions

No root required. The process bootstraps into an unprivileged user namespace via an ELF constructor before Tokio starts. The effective UID becomes 0 inside the user namespace.

### Naming / Prefixes

- Namespaces: `lab<N>-r<id>` (router), `lab<N>-d<id>` (device), `lab<N>-root`
- Bridges: `br-p<pid><n>-<sw_id>`
- Veths: `lab-p<pid><n>e<id>` / `lab-p<pid><n>g<id>`

---

## Mandatory Workflow

### Before every commit

Run these in order. All must pass with zero warnings and zero errors:

Always add a timeout to test runs (e.g. 90s).

```bash
cargo make format                         # NOT cargo fmt; uses project-specific unstable rustfmt options
cargo clippy -p patchbay --tests --fix --allow-dirty
cargo check -p patchbay --tests
cargo nextest run -p patchbay             # use nextest, not cargo test; parallelism in .config/nextest.toml
```

When that is clean, run `cargo check` for the full workspace and test the other crates individually.

If the UI was modified, also run: `cd ui && npm run test:e2e`

### Commit conventions

- Format: `feat: short description`, `fix: ...`, `refactor: ...`, `test: ...`, `docs: ...`, `chore: ...`
- Include a meaningful body for non-trivial changes.
- Do not commit without being asked. Stage files, then ask.

### Code quality

- **Document all public items.** Follow official Rust doc conventions (see Wording Rules above).
- **No warnings.** Treat clippy and rustc warnings as errors.
- **Test coverage.** Add tests for new functionality, covering all presets and code paths.
- **No over-engineering.** Only add what is needed for the current task.

---

## Plans

Plans live in `plans/`. The index is [`plans/PLAN.md`](plans/PLAN.md) with these sections:

1. `# In progress` - currently being worked on.
2. `# Open` - ready to start, listed with priority (1-5, default 2).
3. `# Partial` - mostly done, with a one-line note on what remains.
4. `# Completed` - done, listed with priority.

Omit empty sections.

Each plan file **must start with a `## TODO` checklist**:
- First item: `- [x] Write plan` (always checked).
- Middle items: implementation steps (`[x]` done, `[ ]` pending).
- Last item: `- [ ] Final review`.

### Review commands

- **`review`** - find completed plans with an unchecked `Final review`, review the implementation, and check it off.
- **`review general`** - scan the codebase for quality issues and update `REVIEW.md`.

### REVIEW.md format

- `# Open` - unresolved issues with full details.
- `# Completed` - resolved issues, one-liner per item.

---

## NAT Implementation (Summary)

Full details in [`docs/reference/holepunching.md`](docs/reference/holepunching.md).

| Preset | Mapping | Filtering | nftables approach |
|--------|---------|-----------|-------------------|
| `Nat::Home` | EIM | APDF | fullcone map + `snat to <ip>` + forward filter |
| `Nat::FullCone` | EIM | EIF | fullcone map + `snat to <ip>` |
| `Nat::Corporate` | EDM | APDF | `masquerade random` |
| `Nat::CloudNat` | EDM | APDF | `masquerade random` (longer timeouts) |
| `Nat::Cgnat` | n/a | n/a | plain `masquerade` on IX iface |

Key finding: `snat to <ip>` does NOT preserve ports reliably. The fullcone dynamic map is required for EIM.

---

## Link Condition Presets (Summary)

| Preset | Latency | Jitter | Loss | Rate |
|--------|---------|--------|------|------|
| `Lan` | 0 | 0 | 0% | - |
| `Wifi` | 5ms | 2ms | 0.1% | - |
| `WifiBad` | 40ms | 15ms | 2% | 20 Mbit |
| `Mobile4G` | 25ms | 8ms | 0.5% | - |
| `Mobile3G` | 100ms | 30ms | 2% | 2 Mbit |
| `Satellite` | 40ms | 7ms | 1% | - |
| `SatelliteGeo` | 300ms | 20ms | 0.5% | 25 Mbit |

---

## Common Pitfalls

- **Host root leakage**: Never run lab operations in the host root netns.
- **TC warnings**: Use `r2q 1000` in HTB root to avoid quantum warnings.
- **Port base in tests**: Each test combo needs unique ports to avoid conntrack collisions.
- **Holepunch timing**: After receiving a probe, send extra "ack" packets before returning (APDF timing asymmetry).
