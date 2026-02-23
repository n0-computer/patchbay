# Rootless netsim-rs via Unprivileged User Namespaces

## TODO

- [x] Write plan
- [x] Add libc-only `#[ctor]` bootstrap (`src/userns.rs`) for `cargo test`
- [x] Add public `bootstrap_userns()` in `src/lib.rs` with anyhow error handling
- [x] Split `main()` — sync outer calling `bootstrap_userns()` before Tokio starts
- [x] Remove named/auto netns backend and `NETSIM_NETNS_BACKEND` env var; fd-only remains
- [x] Remove `setup-caps` command and `src/caps.rs`
- [x] Add `-U` to `nsenter` in `run-in` command
- [ ] Final review

Goal: `cargo run -- run sim.toml` and `cargo test` work with zero capabilities, zero
`setcap`, zero `sudo`, as long as the kernel allows unprivileged user namespaces.

---

## 1. Kernel prerequisite

```
sysctl kernel.unprivileged_userns_clone   # must be 1 (or absent — always 1 on >= 6.x)
sysctl user.max_user_namespaces           # must be > 0  (default: 63530+)
```

On Arch Linux with kernel 6.18.1 (this machine) both are satisfied unconditionally.
The netsim binary needs no `setcap`, no suid bit, no sudo.

---

## 2. What we gain inside a user namespace

After `unshare(CLONE_NEWUSER)` + uid/gid map write, inside the user namespace:

| Operation | Works? | Notes |
|-----------|--------|-------|
| `unshare(CLONE_NEWNET)` | ✓ | have CAP_SYS_ADMIN in userns |
| `setns()` into owned netns | ✓ | same userns owns both |
| Create veth pairs / bridges (rtnetlink) | ✓ | CAP_NET_ADMIN in userns |
| Move veth to another netns | ✓ | both netns owned by same userns |
| Write `/proc/sys/net/…` sysctl | ✓ | scoped per-netns by kernel |
| `nft` NAT / masquerade rules | ✓ | UID=0, CAP_NET_ADMIN in userns |
| `tc` qdiscs / netem | ✓ | CAP_NET_ADMIN in userns |
| Write to host `/proc/sys/net/…` | ✗ | host netns not owned by us |
| Load kernel modules | ✗ | requires real CAP_SYS_MODULE |
| Touch host network interfaces | ✗ | outside our userns scope |

---

## 3. Named backend: removed

The named backend (`ip netns add` / `/var/run/netns/`) requires host root for all
operations and is incompatible with rootless mode. **It is removed entirely.** Only the
`fd` backend remains. The `NETSIM_NETNS_BACKEND` env var and `auto`/`named` parsing are
deleted. All namespace management is done via `unshare(CLONE_NEWNET)` + `/proc/self/ns/net`
FDs stored in the in-process `FdRegistry`.

Cleanup code that currently calls `ip netns del` (which silently no-ops in fd mode)
is also removed.

---

## 4. Current privilege model: deprecated

`setup-caps` (`netsim setup-caps`, `src/caps.rs`) applies
`cap_net_admin,cap_sys_admin,cap_net_raw+ep` to the netsim binary and to
`ip`, `tc`, `nft`, `ping`, `ping6`.

**`setup-caps` is deprecated.** It is kept in the binary for the initial rootless
implementation to avoid breaking existing workflows, but it will be removed once rootless
mode is validated. A deprecation notice is added to its output:

```
WARNING: setup-caps is deprecated. netsim now runs rootless via user namespaces.
         Run 'netsim bootstrap-check' to verify your system is ready.
```

`check_caps()` in `src/lib.rs` already accepts UID=0 as the fast path. Inside a user
namespace with `uid_map = "0 <real_uid> 1"`, `nix::unistd::Uid::effective()` returns 0,
so `is_root()` is `true` and the function returns `Ok(())`. **No change needed.**

---

## 5. The critical timing constraint

`unshare(CLONE_NEWUSER)` requires the calling process to be **single-threaded.**
The kernel returns `EINVAL` if any other threads exist (`/proc/<pid>/task/` count > 1).

The current `main()` is:

```rust
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> { … }
```

`current_thread` Tokio can start background I/O/timer threads before the async body
begins. The bootstrap **must happen before any runtime starts**, i.e., before
`#[tokio::main]` expands.

Two call sites need the bootstrap:

1. **The `netsim` binary** — solvable by splitting `main()` (sync outer, async inner).
2. **`cargo test`** — the test harness owns `main()`; we cannot intercept it the same way.

---

## 6. Bootstrap via `ctor` for `cargo test`

### How `ctor` works

The [`ctor` crate](https://crates.io/crates/ctor) marks functions with
`#[ctor]` which places them in the ELF `.init_array` section.
The dynamic linker (`ld.so`) executes all `.init_array` functions after loading/relocation
and **before transferring control to the program entry point** (`_start` → `main()`).

Guarantees relevant to our use:
- **Single-threaded**: the process has exactly one thread at `.init_array` time. No user
  code has run; no `std::thread::spawn()` has been called. The kernel will accept
  `unshare(CLONE_NEWUSER)` here.
- **Before test harness**: the Rust test harness synthesises a `main()` that spawns test
  threads. All `.init_array` functions run before that `main()` begins, so before any
  test thread exists.
- **Covers `cargo test`**: the test binary is a normal ELF; `.init_array` runs the same
  way as in a release binary.

### The Rust 1.81.0 TLS regression

Rust 1.81.0 introduced a regression: calling any Rust stdlib I/O from `.init_array` (e.g.,
`println!`, `env_logger::init()`) triggered `thread::set_current()` TLS initialisation,
which then conflicted with the test harness's own thread init, panicking with
`"thread::set_current should only be called once per thread"`.

**The fix**: never call Rust stdlib functions from the `#[ctor]` body. Use only raw `libc`
syscalls. This sidesteps TLS entirely and is the correct pattern regardless of Rust version.

The `ctor` docs warn it is "incompatible with Tokio." That warning means: do not call
async code or Tokio APIs from within the ctor body. Calling `libc::unshare()` and
`libc::write()` — pure syscalls — is unambiguously safe.

### Implementation

The `#[ctor]` function is a thin, `libc`-only wrapper:

```rust
// src/userns.rs  (new file, compiled only on Linux)

/// Called from ELF .init_array before main() — MUST use only libc, no Rust stdlib.
#[cfg(target_os = "linux")]
#[ctor::ctor]
fn userns_bootstrap_ctor() {
    // Safety: single-threaded at .init_array time; libc calls only.
    unsafe { userns_bootstrap_libc() }
}

/// libc-only bootstrap — safe to call from .init_array.
/// No Rust stdlib, no allocator, no TLS, no panic infrastructure.
unsafe fn userns_bootstrap_libc() {
    // Already real root → nothing to do.
    if libc::getuid() == 0 {
        return;
    }

    // Already mapped UID 0 inside a user namespace → nothing to do.
    // (getuid() already returned 0 in that case, so this branch is redundant
    // but left for clarity — see above.)

    // unshare(CLONE_NEWUSER). Fails with EINVAL if not single-threaded
    // or if nested user namespaces are not permitted.
    if libc::unshare(libc::CLONE_NEWUSER) != 0 {
        // Cannot bootstrap; check_caps() will produce a clear error later.
        return;
    }

    let uid = libc::getuid();
    let gid = libc::getgid();

    // Write "deny" to /proc/self/setgroups (required before gid_map on Linux >= 3.19).
    proc_write(b"/proc/self/setgroups\0", b"deny");

    // uid_map: "0 <uid> 1\n"
    let mut uid_buf = [0u8; 32];
    let uid_line = fmt_map_line(&mut uid_buf, uid);
    proc_write(b"/proc/self/uid_map\0", uid_line);

    // gid_map: "0 <gid> 1\n"
    let mut gid_buf = [0u8; 32];
    let gid_line = fmt_map_line(&mut gid_buf, gid);
    proc_write(b"/proc/self/gid_map\0", gid_line);
}

/// Format "0 <id> 1\n" into `buf` on the stack. Returns the filled slice.
/// No allocation; digits written right-to-left then shifted.
unsafe fn fmt_map_line(buf: &mut [u8; 32], id: u32) -> &[u8] {
    // "0 " prefix
    buf[0] = b'0';
    buf[1] = b' ';
    // id as decimal
    let mut pos = 2usize;
    let mut n = id;
    let id_start = pos;
    if n == 0 {
        buf[pos] = b'0';
        pos += 1;
    } else {
        // write digits to scratch area at end of buf, reversed
        let mut tmp = [0u8; 12];
        let mut t = 11usize;
        while n > 0 {
            tmp[t] = b'0' + (n % 10) as u8;
            n /= 10;
            t -= 1;
        }
        // copy forward
        let digits = &tmp[t + 1..12];
        for &d in digits {
            buf[pos] = d;
            pos += 1;
        }
    }
    buf[pos] = b' '; pos += 1;
    buf[pos] = b'1'; pos += 1;
    buf[pos] = b'\n'; pos += 1;
    &buf[..pos]
}

/// Open a /proc/self/ file and write `data` using raw libc.
unsafe fn proc_write(path: &[u8], data: &[u8]) {
    let fd = libc::open(path.as_ptr() as *const libc::c_char, libc::O_WRONLY);
    if fd < 0 {
        return;
    }
    libc::write(fd, data.as_ptr() as *const libc::c_void, data.len());
    libc::close(fd);
}
```

A separate public function `bootstrap_userns()` in `src/lib.rs` uses full Rust error
handling (anyhow) for the binary `main()` call site, where stdlib is fully initialised:

```rust
/// Bootstrap a user namespace. Call once, before any threads are spawned, before Tokio.
/// Idempotent: no-op if already real root or already UID 0 inside a user namespace.
pub fn bootstrap_userns() -> anyhow::Result<()> {
    use nix::sched::{unshare, CloneFlags};

    if nix::unistd::Uid::current().is_root() {
        return Ok(());   // real root
    }
    // Already bootstrapped: uid_map starts with "0 " meaning we are UID 0 in a userns.
    let uid_map = std::fs::read_to_string("/proc/self/uid_map").unwrap_or_default();
    if uid_map.trim_start().starts_with("0 ") {
        return Ok(());
    }

    let uid = nix::unistd::Uid::current().as_raw();
    let gid = nix::unistd::Gid::current().as_raw();

    unshare(CloneFlags::CLONE_NEWUSER)
        .context("unshare(CLONE_NEWUSER) — ensure no threads are running yet")?;

    std::fs::write("/proc/self/setgroups", "deny\n")
        .context("write /proc/self/setgroups")?;
    std::fs::write("/proc/self/uid_map", format!("0 {uid} 1\n"))
        .context("write /proc/self/uid_map")?;
    std::fs::write("/proc/self/gid_map", format!("0 {gid} 1\n"))
        .context("write /proc/self/gid_map")?;

    anyhow::ensure!(
        nix::unistd::Uid::effective().is_root(),
        "expected UID 0 after userns bootstrap"
    );
    Ok(())
}
```

### `ctor` as a dev-dependency only?

`ctor` could be a regular dependency (applies to binary and tests) or dev-dependency (tests only, with `#[cfg(test)]`). Since the binary's `main()` already calls `bootstrap_userns()` before Tokio, the `#[ctor]` is only strictly needed for tests. Two options:

**Option A** — `ctor` as a regular dependency, `#[ctor]` always active:
- Pro: works for any test harness or future binary without extra plumbing
- Pro: simpler — one mechanism, always present
- Con: `ctor` runs the bootstrap even in the binary, where `main()` also calls it — the
  second call (full Rust path) is idempotent (`uid_map` check) and harmless

**Option B** — `ctor` as dev-dependency, `#[cfg(test)]` gated:
- Pro: zero overhead in release binary
- Con: any future test binary or integration test needs the `#[ctor]` crate at runtime

**Recommendation: Option A** (regular dependency). The overhead is negligible, and it
ensures every binary that links `netsim` bootstraps correctly regardless of entry point.

Cargo.toml addition:

```toml
[dependencies]
ctor = "0.2"
libc = "0.2"    # already transitively present via nix; make it explicit
```

---

## 7. Binary `main()` change

Split the entry point so `bootstrap_userns()` runs before Tokio starts:

```rust
// src/main.rs

fn main() -> anyhow::Result<()> {
    // Must be single-threaded; called before #[tokio::main] starts any threads.
    // The #[ctor] in src/userns.rs already ran this for cargo test, but the
    // full-Rust version here gives proper error messages for the binary path.
    netsim::bootstrap_userns()?;
    tokio_main()
}

#[tokio::main(flavor = "current_thread")]
async fn tokio_main() -> anyhow::Result<()> {
    netsim::Lab::init_tracing();
    let cli = Cli::parse();
    // … existing match on cli.command …
}
```

---

## 8. `run-in` and `nsenter` across sessions

### The problem

`inspect` creates keeper processes inside network namespaces owned by the inspect session's
user namespace. When `run-in` is called later as a fresh process, it starts in the host
user namespace. The target netns belongs to the inspect's user namespace. A fresh
`nsenter -t <pid> -n` fails: the caller has no capabilities in the owning user namespace.

### The fix

Add `-U` to the `nsenter` invocation:

```rust
// src/main.rs  run_in_command()
proc.arg("-U")          // enter the keeper's user namespace first
    .arg("-t").arg(pid.to_string())
    .arg("-n")          // then enter its network namespace
    .arg("--")
    .arg(&cmd[0]);
```

`nsenter -U` joins the user namespace of the target PID. An unprivileged user can join
a child user namespace (one descended from their own) via `setns()` — no capabilities
needed on the host side. Once inside the user namespace, the caller has UID 0 and
CAP_SYS_ADMIN scoped there, which is sufficient to enter the network namespace.

`-U` is present in `util-linux` >= 2.23 (available on any distro since 2014).

The `bootstrap_userns()` call in `run-in`'s `main()` is still correct: it detects UID 0
(real root) or an already-mapped userns and returns immediately, so the `-U` path always
starts from the host user namespace and transitions via nsenter.

---

## 9. sysctl writes

`set_sysctl_in()` (`src/core.rs:1332`) switches to the target netns then calls
`std::fs::write("/proc/sys/net/ipv4/ip_forward", "1")`. The Linux kernel virtualises
`/proc/sys/net/` per network namespace: the write only affects the netns the writing
thread is currently in. No host sysctl is modified. Works as-is. ✓

---

## 10. External tools (`nft`, `tc`, `ping`)

All child processes spawned from within the user namespace inherit UID 0 and the full
userns-scoped capability set. No `setcap` is needed on these binaries.

- **`nft`**: checks `CAP_NET_ADMIN` via `capget()`. Inside our userns, `capget()` returns
  the userns-scoped caps. Modern `nft` (>= 0.9.1) works correctly. ✓
- **`tc`**: uses rtnetlink with `CAP_NET_ADMIN`. ✓
- **`ping`/`ping6`**: need `CAP_NET_RAW` for ICMP raw sockets. Inside userns, we have it. ✓

### nftables connection tracking

`apply_isp_cgnat()` and `apply_home_nat()` use masquerade rules that require `nf_conntrack`
and `nf_nat` kernel modules. These are not restricted by user namespaces once loaded, but
**autoloading** requires `CAP_SYS_MODULE` in the initial user namespace (which we don't
have).

On virtually all modern systems these modules are pre-loaded at boot (Docker, podman,
firewalld, iptables all load them). If they are absent, `nft` returns an error on first
masquerade use.

**Mitigation**: add a pre-flight check that probes for nftables availability in a fresh
netns before running the full build. This is a follow-up item.

---

## 11. Gaps and required changes (summary)

### Required

| # | File | Change |
|---|------|--------|
| 1 | `Cargo.toml` | Add `ctor = "0.2"`, make `libc = "0.2"` explicit |
| 2 | `src/userns.rs` (new) | `#[ctor]` bootstrap function, libc-only |
| 3 | `src/lib.rs` | `pub fn bootstrap_userns()` with anyhow errors; remove named backend |
| 4 | `src/main.rs` | Split `main()` / `tokio_main()`; call `bootstrap_userns()` in sync `main()` |
| 5 | `src/main.rs` | Add `-U` to `nsenter` in `run_in_command()` |
| 6 | `src/netns.rs` | Remove `Named` backend, `probe_named_backend_support()`, `NETSIM_NETNS_BACKEND` env, cleanup `ip netns del` call |

### Deprecated (keep, warn, remove later)

| # | File | Change |
|---|------|--------|
| 7 | `src/caps.rs` | Add deprecation warning printed before running setcap |
| 8 | `src/main.rs` | `SetupCaps` command: print deprecation notice; keep functional |

### Follow-up (not in initial impl)

| # | Item |
|---|------|
| 9 | Pre-flight nftables probe (check `nf_conntrack` loaded) |
| 10 | `netsim bootstrap-check` subcommand (verify userns prereqs) |
| 11 | Remove `setup-caps` entirely |

---

## 12. Compatibility after changes

| Scenario | Works? | Notes |
|----------|--------|-------|
| `cargo run -- run` (unprivileged) | ✓ | bootstrap_userns() in sync main() |
| `cargo test` | ✓ | #[ctor] bootstraps before harness threads |
| `netsim inspect` + `netsim run-in` | ✓ | nsenter -U added |
| `netsim cleanup` | ✓ | fd registry cleanup; ip netns del removed |
| `netsim setup-caps` | ⚠ deprecated | prints warning; still works for old setups |
| `NETSIM_NETNS_BACKEND=named` | ✗ removed | fd is the only backend |
| nftables NAT (nf_conntrack loaded) | ✓ | typical on any modern distro |
| nftables NAT (nf_conntrack absent) | ✗ | modprobe nf_conntrack needed; pre-flight TBD |
| Real root (no user namespace) | ✓ | bootstrap_userns() no-ops on UID 0 |

---

## 13. What does NOT change

- The `fd` backend and `FdRegistry` / `Worker` / `worker_main` architecture.
- The `rtnetlink` crate usage for veth, bridges, routes.
- The sysctl write mechanism.
- `nft` / `tc` invocation (no args change; they just run as UID 0 in userns).
- TOML config format.
- The Tokio runtime, Lab API, LabCore, async task dispatch.
- `check_caps()` logic (UID 0 fast-path already handles userns).

---

## 14. Manual verification before implementing

```bash
# Confirm userns is enabled
sysctl kernel.unprivileged_userns_clone   # expect: 1 (or not present on >= 6.x)

# Full smoke test — replicate what bootstrap_userns() does
unshare -U --map-root-user bash
id                                        # expect: uid=0(root)
ip link add br0 type bridge               # expect: success
ip link set br0 up
ip netns add test1 2>/dev/null || echo "named netns expected to fail (fd mode ok)"
nft add table inet smoke                  # expect: success (nf_conntrack loaded)
nft delete table inet smoke
sysctl -w net.ipv4.ip_forward=1          # expect: success (netns-scoped)
exit

# Confirm nf_conntrack is loaded
lsmod | grep nf_conntrack
```

All of the above should pass on this machine (Arch, kernel 6.18.1) without any
configuration changes.
