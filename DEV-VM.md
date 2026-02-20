# DEV-VM

This project now has a QEMU-based VM flow that can replace Lima:

- wrapper: `qemu-vm.sh`
- cargo-make file: `Makefile2.toml`

It uses a Debian cloud image and cloud-init.  
If ISO seed tools are missing, the wrapper automatically falls back to an internal HTTP seed server (`python3 -m http.server`) so you do not need `cloud-localds` just to boot.
To keep startup fast, `up` does not install packages. Required tools are installed lazily in `prepare-vm` (idempotent).

## Quick Start

1. Build and start VM:
```bash
cargo make --makefile Makefile2.toml setup-vm
```

2. Run binary in VM:
```bash
cargo make --makefile Makefile2.toml run-vm -- --help
```

3. Run tests in VM:
```bash
cargo make --makefile Makefile2.toml test-vm
```

4. Stop VM:
```bash
cargo make --makefile Makefile2.toml shutdown
```

## What Gets Mounted

- Host workspace -> `/app` in VM
- Host Cargo target dir -> `/target` in VM

The wrapper injects these as 9p mounts.
It prefers `virtiofs` when host `virtiofsd` is available, and falls back to `9p`.

## Required Host Tools

Minimum:
- `qemu-system-x86_64`
- `qemu-img`
- `ssh`
- `ssh-keygen`
- `curl`
- `python3` (for fallback cloud-init seed mode)

Optional (preferred cloud-init seed path):
- `cloud-localds` (from `cloud-image-utils`)
  - If missing, wrapper uses HTTP seed fallback automatically.

## Debian/Ubuntu Host

```bash
sudo apt update
sudo apt install -y \
  qemu-system-x86 qemu-utils openssh-client curl python3 \
  cloud-image-utils
```

## Arch Linux Host

```bash
sudo pacman -Syu --needed \
  qemu-base qemu-desktop qemu-img openssh curl python \
  cloud-image-utils
```

Notes:
- `cloud-image-utils` is optional with this wrapper, but recommended.
- If your package set differs, ensure `qemu-system-x86_64` and `qemu-img` are available in `PATH`.

## macOS Host (Homebrew)

```bash
brew install qemu openssh curl python cloud-utils
```

Notes:
- On Apple Silicon, this wrapper currently targets `x86_64` guests (`qemu-system-x86_64`).
- That works via emulation/HVF but is slower than native `aarch64` guest images.

## Wrapper Commands

```bash
./qemu-vm.sh up
./qemu-vm.sh up --recreate
./qemu-vm.sh status
./qemu-vm.sh ssh
./qemu-vm.sh ssh -- uname -a
./qemu-vm.sh down
```

## Useful Environment Overrides

- `QEMU_VM_NAME` (default `netsim-vm`)
- `QEMU_VM_STATE_DIR` (default `./.qemu-vm`)
- `QEMU_VM_IMAGE_URL`
- `QEMU_VM_MEM_MB` (default `4096`)
- `QEMU_VM_CPUS` (default `4`)
- `QEMU_VM_DISK_GB` (default `40`)
- `QEMU_VM_SSH_PORT` (default `2222`)
- `QEMU_VM_SEED_PORT` (default `8555`)

Example:
```bash
QEMU_VM_MEM_MB=8192 QEMU_VM_CPUS=8 ./qemu-vm.sh up
```

## Troubleshooting

### Error: `need cloud-localds, genisoimage, mkisofs, or xorriso`

This was the old behavior. The wrapper now falls back to HTTP seed mode automatically if none of these tools exist.

If you still see cloud-init failures:

1. Verify `python3` exists:
```bash
python3 --version
```

2. Check QEMU serial output:
```bash
tail -n 200 .qemu-vm/netsim-vm/serial.log
```

3. Check seed server log (fallback mode):
```bash
tail -n 200 .qemu-vm/netsim-vm/seed-http.log
```

4. If port conflict suspected, change seed port:
```bash
QEMU_VM_SEED_PORT=8855 ./qemu-vm.sh up
```

### SSH never comes up

1. Confirm VM process:
```bash
./qemu-vm.sh status
```

2. Confirm port binding:
```bash
ss -ltn | grep 2222
```

3. Retry clean start:
```bash
./qemu-vm.sh down
./qemu-vm.sh up
```

### `/app` or `/target` is empty

The wrapper now mounts both paths after SSH is ready and validates `/app/Cargo.toml`.

Manual checks:

```bash
./qemu-vm.sh ssh -- mount | grep -E ' /app | /target '
./qemu-vm.sh ssh -- ls -la /app | head
./qemu-vm.sh ssh -- ls -la /target | head
```

If mounts are missing, restart once:

```bash
./qemu-vm.sh down
./qemu-vm.sh up
```

If a VM is already running with old mount paths, force recreation:

```bash
./qemu-vm.sh up --recreate
```

If it still fails, inspect:

```bash
tail -n 200 .qemu-vm/netsim-vm/serial.log
```
