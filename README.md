# netsim-rs

Rust network namespace simulator for NAT/routing/link-impairment labs, plus an iroh simulation runner.

## What You Get

- Linux netns topology builder (`router`, `device`, NAT modes, impairments)
- Simulation runner for TOML scenarios (`run`, `spawn`, `wait-for`, `switch-route`, etc.)
- Built-in `kind = "iroh-transfer"` workflow with result reports:
  - `results.json`
  - `results.md`
  - `combined-results.json` / `combined-results.md` (across runs in one work root)
  - per-step logs in `logs/`

## Prerequisites

- Linux host (or Linux VM) with:
  - `ip`, `tc`, `nft`
  - capabilities: `CAP_NET_ADMIN`, `CAP_SYS_ADMIN`, `CAP_NET_RAW`
- Standalone binary setup:

```bash
netsim setup-caps
```

- Repo dev/test setup after every rebuild:

```bash
./setcap.sh
```

## Local Dev Commands

Build/check:

```bash
cargo check
cargo fmt
```

Cleanup leaked lab resources and stop VM:

```bash
netsim cleanup
```

Run tests locally (if your host policy allows it):

```bash
./setcap.sh
cargo test
```

## VM Workflow (Recommended)

The QEMU VM flow is built into `netsim run-vm`.
From the repo, use `cargo run -- run-vm ...` (or `cargo make run-vm ...`).

### VM mounts

- `/app` -> repo workspace (read-only)
- `/target` -> host target dir (read-only)
- `/work` -> host `.netsim-work` (read-write)

Simulation outputs are written per run under `/work`, with:

- run directory: `/work/<sim-name>-YYMMDD-HHMMSS/`
- symlink: `/work/latest` -> most recent run

On host this is:

- `.netsim-work/latest/results.json`
- `.netsim-work/latest/results.md`
- `.netsim-work/combined-results.json`
- `.netsim-work/combined-results.md`
- `.netsim-work/latest/logs/*`

For `kind = "iroh-transfer"` with `id = "xfer"`, logs are grouped as:

- `.netsim-work/latest/logs/xfer/provider/` (`--logs-path` dir)
- `.netsim-work/latest/logs/xfer/fetcher-0/` (or `fetcher-1`, …)
- `.netsim-work/latest/logs/xfer/provider.log`
- `.netsim-work/latest/logs/xfer/fetcher-0.log`

### Start and run

```bash
cargo run -- run-vm ./iroh-integration/sims/iroh-1to1-public.toml

# run a whole directory and produce combined results
cargo run -- run-vm ./iroh-integration/sims
```

### Run and tee output

```bash
cargo run -- run-vm ./iroh-integration/sims/iroh-1to1-public.toml |& tee run-1to1
```

If mount paths or runner-binary location changed since the last VM boot, recreate once:

```bash
cargo run -- run-vm --recreate ./iroh-integration/sims/iroh-1to1-public.toml
```

### Run tests in VM

```bash
cargo make test-vm
```

### Shut down VM

```bash
cargo make vm-down
```

## Iroh Simulations

Included sims:

- `./iroh-integration/sims/iroh-1to1-public.toml`
- `./iroh-integration/sims/iroh-1to1-nat.toml`
- `./iroh-integration/sims/iroh-switch-direct.toml`

Shared binary defaults are in:

- `iroh-integration/iroh-defaults.toml`

## Binary Overrides

You can override binaries at runtime with repeatable `--binary`:

```text
--binary "<name>:<mode>:<value>"
```

Modes:

- `build`: build from local checkout directory
- `fetch`: download from URL
- `path`: copy local file into workdir bins and use copied file

Examples:

```bash
# Build transfer from checkout path
cargo run -- run-vm ./iroh-integration/sims/iroh-1to1-public.toml \
  --binary "transfer:build:../iroh"

# Force relay URL
cargo run -- run-vm ./iroh-integration/sims/iroh-1to1-nat.toml \
  --binary "relay:fetch:https://github.com/n0-computer/iroh/releases/download/v0.96.0/iroh-relay-v0.96.0-x86_64-unknown-linux-musl.tar.gz"

# Use prebuilt transfer binary from host path
cargo run -- run-vm ./iroh-integration/sims/iroh-1to1-public.toml \
  --binary "transfer:path:./target/x86_64-unknown-linux-musl/release/examples/transfer"
```

## Testing Uncommitted Iroh Changes

If you have an iroh checkout with local uncommitted changes, run the sim against that source directly:

```bash
cargo run -- run-vm ./iroh-integration/sims/iroh-1to1-public.toml \
  --binary "transfer:build:../iroh"
```

Notes:

- Build artifacts are written under `/work/latest/build-target` (host `.netsim-work/latest/build-target`).
- `run-vm` always executes a Linux x86_64 guest binary:
  - on Linux host, it stages the current `netsim` binary into `/work/.netsim-bin/netsim`
  - on macOS host, it downloads `netsim-x86_64-unknown-linux-musl.tar.gz` from the latest GitHub release and stages that into `/work/.netsim-bin/netsim`.
