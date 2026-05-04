# Running in a VM

patchbay requires Linux network namespaces, which means it cannot run
natively on macOS or Windows. The `patchbay-vm` crate solves this by
wrapping your simulations and tests in a Linux VM, giving you the same
experience on any development machine.

Two VM backends are available:

| Backend | Platform | Boot time | How it works |
|---------|----------|-----------|--------------|
| **QEMU** | Linux, macOS (Intel and Apple Silicon) | 30-60s | Full Debian cloud image with SSH access |
| **Apple container** | macOS 26+ Apple Silicon only | Sub-second | Lightweight VM via Apple's [Containerization](https://github.com/apple/containerization) framework |

By default, `patchbay-vm` auto-detects the best backend. On macOS 26
with Apple Silicon and the `container` CLI installed it picks the
container backend; everywhere else it falls back to QEMU. You can force
a backend with `--backend`:

```bash
patchbay-vm --backend container run sim.toml
patchbay-vm --backend qemu run sim.toml
```

## Installing patchbay-vm

```bash
cargo install --git https://github.com/n0-computer/patchbay patchbay-vm
```

## Running simulations

The `run` command boots a VM (or reuses a running one), stages the
simulation files and binaries, and executes them inside the guest:

```bash
patchbay-vm run ./sims/iperf-baseline.toml
```

Results and logs are written to the work directory (`.patchbay-work/` by
default). You can pass multiple simulation files, and they run
sequentially in the same VM.

### Controlling the patchbay version

By default, `patchbay-vm` downloads the latest release of the patchbay
runner binary. You can pin a version, build from a Git ref, or point to a
local binary:

```bash
patchbay-vm run sim.toml --patchbay-version v0.10.0
patchbay-vm run sim.toml --patchbay-version git:main
patchbay-vm run sim.toml --patchbay-version path:/usr/local/bin/patchbay
```

### Binary overrides

If your simulation references custom binaries (test servers, protocol
implementations), you can stage them into the VM:

```bash
patchbay-vm run sim.toml --binary myserver:path:./target/release/myserver
```

The binary is copied into the guest's work directory and made available
at the path the simulation expects.

## Running tests

The `test` command cross-compiles your Rust tests for musl, stages the
test binaries in the VM, and runs them:

```bash
patchbay-vm test
patchbay-vm test --package patchbay
patchbay-vm test -- --test-threads=4
```

This is the recommended way to run patchbay integration tests on macOS.
The VM has all required tools pre-installed (nftables, iproute2, iperf3)
and unprivileged user namespaces enabled.

## VM lifecycle

The VM boots on first use and stays running between commands. Subsequent
`run` or `test` calls reuse the existing VM, which avoids the 30-60
second boot time on repeated invocations.

```bash
patchbay-vm up        # Boot the VM (or verify it is running)
patchbay-vm status    # Show VM state, SSH port, mount paths
patchbay-vm down      # Shut down the VM
patchbay-vm cleanup   # Remove stale sockets and PID files
```

You can also SSH into the guest directly for debugging:

```bash
patchbay-vm ssh -- ip netns list
patchbay-vm ssh -- nft list ruleset
```

## How it works

Both backends share the same three mount points inside the guest:

| Guest path | Host path | Access | Purpose |
|------------|-----------|--------|---------|
| `/app` | Workspace root | Read-only | Source code and simulation files |
| `/target` | Cargo target dir | Read-only | Build artifacts |
| `/work` | Work directory | Read-write | Simulation output and logs |

### QEMU backend

`patchbay-vm` downloads a Debian cloud image (cached in
`~/.local/share/patchbay/qemu-images/`), creates a COW disk backed by
it, and boots QEMU with cloud-init for initial provisioning. The guest
gets SSH access via a host-forwarded port (default 2222).

File sharing uses virtiofs when available (faster, requires virtiofsd on
the host) and falls back to 9p. Hardware acceleration is auto-detected:
KVM on Linux, HVF on macOS, TCG emulation as a last resort.

### Apple container backend

The container backend uses Apple's
[Containerization](https://github.com/apple/containerization) framework,
which runs each container inside its own lightweight Linux VM powered by
the Virtualization.framework hypervisor. Apple's default kernel ships
with everything patchbay needs built-in: network namespaces, nftables,
netem/HTB/TBF qdiscs, veth pairs, and bridges.

Instead of SSH, commands execute through `container exec`. Directories
are shared via native VirtioFS mounts (no separate virtiofsd process).
On first boot the guest installs required userspace tools (iproute2,
nftables, etc.) from the Debian repositories; subsequent runs skip this
step.

## Setting up the QEMU backend on macOS

1. Install QEMU:

```bash
brew install qemu
```

2. For faster file sharing, install virtiofsd (optional but recommended):

```bash
brew install virtiofsd
```

3. Build the musl runner binary. On Apple Silicon:

```bash
rustup target add aarch64-unknown-linux-musl
brew install messense/macos-cross-toolchains/aarch64-unknown-linux-musl
```

Add to `.cargo/config.toml`:

```toml
[target.aarch64-unknown-linux-musl]
linker = "aarch64-linux-musl-gcc"
```

Then build:

```bash
cargo build --release --target aarch64-unknown-linux-musl -p patchbay-runner --bin patchbay
```

On Intel Macs, replace `aarch64` with `x86_64` throughout.

4. Run:

```bash
patchbay-vm --backend qemu run \
  --patchbay-version "path:target/aarch64-unknown-linux-musl/release/patchbay" \
  ./path/to/sim.toml
```

The first boot downloads a Debian cloud image and provisions the VM,
which takes 1-2 minutes. Subsequent runs reuse the running VM.

## Setting up the Apple container backend

### Requirements

- Mac with Apple Silicon (M1 or later)
- macOS 26 (Tahoe) or later
- [container CLI](https://github.com/apple/container) installed

### Installation

Install via Homebrew (recommended):

```bash
brew install container
```

Alternatively, download the latest signed installer package from the
[container releases page](https://github.com/apple/container/releases)
and double-click to install.

3. Start the system service:

```bash
container system start
```

4. Verify it works:

```bash
container run --rm debian:trixie-slim echo "hello from container"
```

### Building the musl target

Simulations run inside an ARM64 Linux VM, so the patchbay runner binary
must be cross-compiled for `aarch64-unknown-linux-musl`.

1. Install the Rust target and a musl cross-compiler:

```bash
rustup target add aarch64-unknown-linux-musl
brew install messense/macos-cross-toolchains/aarch64-unknown-linux-musl
```

2. Tell Cargo which linker to use. Add to `.cargo/config.toml` (create
   it if it does not exist):

```toml
[target.aarch64-unknown-linux-musl]
linker = "aarch64-linux-musl-gcc"
```

3. Build the runner binary:

```bash
cargo build --release --target aarch64-unknown-linux-musl -p patchbay-runner --bin patchbay
```

### Running a simulation

```bash
patchbay-vm --backend container run \
  --patchbay-version "path:target/aarch64-unknown-linux-musl/release/patchbay" ./path/to/sim.toml
```

On the first run the container backend pulls the Debian base image and
installs packages (takes about 15 seconds). Subsequent runs reuse the
existing container and skip provisioning entirely.

## Configuration

All settings have sensible defaults. Override them through environment
variables when needed.

### QEMU backend

| Variable | Default | Description |
|----------|---------|-------------|
| `QEMU_VM_MEM_MB` | 8192 | Guest RAM in megabytes |
| `QEMU_VM_CPUS` | all | Guest CPU count (defaults to all host CPUs) |
| `QEMU_VM_SSH_PORT` | 2222 | Host port forwarded to guest SSH |
| `QEMU_VM_NAME` | patchbay-vm | VM instance name |
| `QEMU_VM_DISK_GB` | 40 | Disk size in gigabytes |

VM state lives in `.qemu-vm/<name>/` in your project directory. The disk
image uses COW backing, so it only consumes space for blocks that differ
from the base image.

### Apple container backend

| Variable | Default | Description |
|----------|---------|-------------|
| `CONTAINER_VM_MEM_MB` | 8192 | Guest RAM in megabytes |
| `CONTAINER_VM_CPUS` | all | Guest CPU count (defaults to all host CPUs) |
| `CONTAINER_VM_IMAGE` | debian:trixie-slim | OCI image to use |
| `CONTAINER_VM_NAME` | patchbay | Container instance name |

Container state lives in `.container-vm/<name>/` in your project
directory.
