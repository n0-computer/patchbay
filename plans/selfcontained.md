# Plan: Self-Contained `netsim` Binary

## TODO

- [x] Write plan
- [x] Reshape CLI into explicit `run`/`run-vm`/`setup-caps` subcommands
- [x] Embed VM orchestration in Rust — ported to `crates/netsim-vm` (see `netsim-vm-split.md`)
- [x] Built-in self-capability setup (`netsim setup-caps`) — superseded/removed in rootless migration
- [x] Keep `setcap.sh` for test binaries — superseded/removed in rootless migration
- [x] Wire `run-vm` to execute `netsim` in guest
- [x] Update automation and docs (`Makefile.toml`, `AGENTS.md`)
- [ ] Validate external-checkout flow end-to-end
- [ ] Final review

## Goal
Ship a single `netsim` binary that can be dropped into another checkout (for example `iroh`) and used directly for:
- local runs: `netsim run ./sims/1to1.toml`
- VM runs: `netsim run-vm ./sims/1to1.toml`

The binary should embed current `qemu-vm.sh` behavior and provide a built-in capability setup flow for the binary itself (`sudo setcap ... <self>`), while keeping `./setcap.sh` for project test binaries.

## Scope / Non-Goals
- In scope:
  - Replace shell-driven VM orchestration (`qemu-vm.sh`) with Rust code inside `netsim`.
  - Add a first-class self-capability command in the CLI.
  - Preserve existing sim runner behavior (`run_sim`) for local execution.
  - Keep and narrow `setcap.sh` to the test-binary/tooling workflow.
- Out of scope:
  - Rewriting VM behavior semantics (cloud-init shape, mounts, SSH contract) beyond parity.
  - Removing cargo-make tasks in this phase (they should delegate to `netsim`).

## Target UX
1. `netsim run ./sims/1to1.toml --work-dir .netsim-work`
2. `netsim run-vm ./sims/1to1.toml --work-dir .netsim-work`
3. `netsim setup-caps`
4. Optional bootstrap in another checkout:
   - copy/symlink `netsim` into `$PATH`
   - run `netsim setup-caps`
   - run `netsim run` or `netsim run-vm`

## Implementation Plan

### 1. Reshape CLI into explicit subcommands
Add subcommands to `src/main.rs`:
- `run` (local sim execution)
- `run-vm` (VM lifecycle + remote execution)
- `setup-caps` (self capability setup)
- (optional) keep current positional form as backward-compatible alias to `run`.

Implementation sketch:
```rust
#[derive(clap::Subcommand)]
enum Command {
    Run(RunArgs),
    RunVm(RunVmArgs),
    SetupCaps(SetupCapsArgs),
}
```

Notes:
- Keep `--binary` override semantics identical for `run` and `run-vm`.
- Parse once, then dispatch to `sim::run_sim(...)`, `vm::run_sim_in_vm(...)`, or `caps::setup_self_caps(...)`.

### 2. Embed `qemu-vm.sh` behavior in a single `src/vm.rs`
Use one module file (`src/vm.rs`) with no submodules for the first pass.
Port the script nearly literally by invoking external commands, with short helper
functions to keep LOC and ordering close to the bash version.

Suggested helper shape:
- `run_checked(cmd, args)`
- `run_capture(cmd, args)`
- `need_cmd(name)`
- `is_running(pid_file)`
- `abspath(path)`
- `log(msg)` / `err(msg)`

Parity requirements from `qemu-vm.sh`:
- command set: `up`, `down`, `status`, `ssh`
- runtime state under `.qemu-vm/<name>/`
- mount path mismatch checks + `--recreate` behavior
- seed ISO/HTTP fallback behavior
- auto accel selection (`kvm`/`hvf`/`tcg`)

Implementation sketch:
```rust
pub(crate) async fn run_vm_sim(args: RunVmArgs) -> Result<()> {
    let vm = VmConfig::from_args_env(&args)?;
    vm_up(&vm)?;
    vm_prepare_guest(&vm)?;
    vm_exec_netsim_run(&vm, args.sim, args.work_dir, args.binary_overrides)?;
    Ok(())
}
```

Implementation constraints:
- Preserve command sequence and side effects from `qemu-vm.sh` first; refactor later.
- Keep function names close to script sections (`ensure_image`, `ensure_key`,
  `render_cloud_init`, `create_seed`, `start_qemu`, `wait_for_ssh`, etc.).
- Use `std::process::Command` (plus `bash -lc` only where shell features are required).

### 3. Implement built-in self-capability setup (`setup-caps`)
Add `src/caps.rs` for capability setup flow for both the current binary and required system tools:
- detect Linux + `setcap` + `sudo`
- resolve `std::env::current_exe()`
- print exact command and why it is needed
- run `sudo setcap cap_net_admin,cap_sys_admin,cap_net_raw+ep <self>`
- run `sudo setcap cap_net_admin,cap_sys_admin,cap_net_raw+ep` for required tools (`ip`, `tc`, `nft`, `ping`, `ping6`) when present
- verify with `getcap <self>` and print result

Implementation sketch:
```rust
pub fn setup_self_caps() -> Result<()> {
    let exe = std::env::current_exe()?;
    eprintln!("Will run: sudo setcap ... {}", exe.display());
    run("sudo", ["setcap", CAPS, exe])?;
    verify_caps(&exe)?;
    Ok(())
}
```

Documentation constraints:
- explicitly explain rebuild invalidates caps on rebuilt binaries
- explain that `sudo` is used only to set file capabilities
- explain failure mode when `no_new_privs=1`
- explain why helper tools are explicitly capped (capabilities are not reliably retained across `execve` without ambient-capability setup)

### 4. Keep `setcap.sh` for tests and align responsibility
Retain `./setcap.sh` for project-local build/test artifacts and tool capabilities.
Adjust messaging so responsibilities are unambiguous:
- `netsim setup-caps` = self-contained external binary setup
- `./setcap.sh` = repo test/dev capability bootstrap (`cargo test`, tool bins, test bins)

If needed, simplify `setcap.sh` output to point users to `netsim setup-caps` for non-repo usage.

### 5. Wire `run-vm` to execute `netsim` inside guest
`run-vm` should:
- ensure local VM is up
- mount target/work paths as today
- run `/target/<target-triple>/release/netsim run <sim> --work-dir /work ...` over SSH

Key requirement:
- VM path contract remains stable so existing `iroh-integration` artifacts keep working.
- Must work on macOS hosts with `brew install qemu` as the only explicit extra requirement.

### 6. Update automation and docs
- Update `Makefile.toml` tasks to call the binary subcommands instead of wrapper scripts where possible.
- Update `README.md` and `AGENTS.md` command examples (`run`/`run-vm`/`setup-caps`).
- Keep `qemu-vm.sh` during migration behind a feature flag or temporary compatibility path, then remove once parity checks pass.
- Ensure `run`/`run-vm` print a combined summary at the end as a terminal table.

### 7. Validation and rollout
Validation checklist:
- local: `netsim setup-caps`, `netsim run ...`
- VM: `netsim run-vm ...`
- repo dev path still works: `./setcap.sh`, `cargo make test-local`, `cargo make run-vm`
- verify another checkout can call `netsim` without repo scripts

Rollout approach:
1. Land CLI + `setup-caps`.
2. Land single-file `src/vm.rs` literal port with parity tests.
3. Switch tasks/docs to new commands.
4. Remove or deprecate `qemu-vm.sh` after stable soak.

## Risks / Open Items
1. VM orchestration parity risk: script-to-Rust migration may miss edge-case behavior in cloud-init and filesystem sharing.
2. Capability setup risk: applying capabilities to system tools may fail on hosts with restrictive filesystem policy; surface failures clearly and keep `setcap.sh` as repo-local fallback.
3. Portability risk: `run-vm` remains QEMU-host dependent; keep defaults compatible with Linux + macOS (HVF/TCG on macOS) and fail fast with clear diagnostics when host tools are missing.
