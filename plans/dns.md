# DNS & Name Resolution Plan

**Status:** phase 1 complete (rewrite)

---

## Research findings

### Linux /etc/hosts

- glibc hardcodes `/etc/hosts`. No include directive, no `hosts.d/`. One file.
- glibc re-reads `/etc/hosts` on each `getaddrinfo()` call (checks mtime).
  Changes are picked up immediately. No nscd in user namespaces.
- Bind-mounting a custom file over `/etc/hosts` is the only supported approach.

### Mount namespaces are per-thread

- `unshare(CLONE_NEWNS)` from a thread only affects **that thread's** mount
  namespace. Other threads in the same process keep the original mounts.
  Verified experimentally with C test program.
- This means: each worker thread can have its own `/etc/hosts` without
  affecting the test runner or other workers.

### tokio blocking pool threads

- `tokio::runtime::Builder::on_thread_start(f)` registers a callback that
  runs on **every thread** the runtime creates, including blocking pool threads
  (verified in tokio source: `blocking/pool.rs:229` passes `after_start` to
  pool, `pool.rs:504` calls it on each blocking thread start).
- `tokio::net::lookup_host` calls `getaddrinfo` via `spawn_blocking`, which
  runs on blocking pool threads. These threads WILL get the `on_thread_start`
  callback.
- Solution: pass the hosts file path into the runtime builder's
  `on_thread_start`, which does `unshare(CLONE_NEWNS)` + bind-mount.

### /etc/resolv.conf

- Same approach works: bind-mount a generated resolv.conf over
  `/etc/resolv.conf` in every worker thread (sync, async, blocking pool).
- glibc reads `/etc/resolv.conf` lazily and caches until mtime changes.
- hickory-resolver with `system-config` reads it at construction time.

---

## Phase 1 — `/etc/hosts` and `/etc/resolv.conf` overlay

### Design: mount-once, write-through

Each device gets a **persistent bind-mount** of a generated hosts file over
`/etc/hosts` and optionally a resolv.conf over `/etc/resolv.conf`. The mount
happens once at worker thread startup. Subsequent `dns_entry()` calls just
rewrite the underlying file — glibc picks up changes on the next
`getaddrinfo()` via mtime check.

**Three thread types need the overlay:**

1. **Async worker thread** — does `unshare(CLONE_NEWNS)` + mount after
   `setns(CLONE_NEWNET)`, before building the tokio runtime.
2. **Sync worker thread** — does `unshare(CLONE_NEWNS)` + mount after
   `setns(CLONE_NEWNET)`, before entering the message loop.
3. **Tokio blocking pool threads** — get the overlay via the runtime's
   `on_thread_start` callback, which does `unshare(CLONE_NEWNS)` + mount.

For `spawn_command` children, the `pre_exec` hook continues to apply the
overlay per-child (children don't inherit the worker's mount namespace since
they're forked, not spawned as threads).

### Rust API

```rust
impl Lab {
    /// Adds a hosts entry to all devices.
    pub fn dns_entry(&self, name: &str, ip: IpAddr) -> Result<()>;

    /// Sets the nameserver for all devices (writes resolv.conf overlay).
    pub fn set_nameserver(&self, server: IpAddr) -> Result<()>;

    /// Resolves a name from lab-wide entries (in-memory, no syscall).
    pub fn resolve(&self, name: &str) -> Option<IpAddr>;
}

impl Device {
    /// Adds a hosts entry to this device only.
    pub fn dns_entry(&self, name: &str, ip: IpAddr) -> Result<()>;

    /// Resolves using device + lab-wide entries (in-memory).
    pub fn resolve(&self, name: &str) -> Option<IpAddr>;
}
```

### Storage (`core.rs`)

```rust
pub(crate) struct DnsEntries {
    global: Vec<(String, IpAddr)>,
    per_device: HashMap<NodeId, Vec<(String, IpAddr)>>,
    nameserver: Option<IpAddr>,
    hosts_dir: PathBuf,
}
```

Single hosts file per device at `<hosts_dir>/<node_id>.hosts`. One shared
`resolv.conf` at `<hosts_dir>/resolv.conf`. The `hosts_dir` is
`$TMPDIR/netsim-<prefix>-hosts/`.

### Worker thread mount setup (`netns.rs`)

Workers store the hosts file path and resolv.conf path. At thread startup:

```
setns(CLONE_NEWNET)
unshare(CLONE_NEWNS)
if hosts_path exists: mount --bind hosts_path /etc/hosts
if resolv_path exists: mount --bind resolv_path /etc/resolv.conf
```

For the async worker, the tokio runtime is built with:

```rust
tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .on_thread_start(move || {
        unshare(CLONE_NEWNS);
        if let Some(p) = &hosts { mount_bind(p, "/etc/hosts"); }
        if let Some(p) = &resolv { mount_bind(p, "/etc/resolv.conf"); }
    })
    .build()
```

This covers blocking pool threads too.

### `dns_entry()` flow

1. Append to in-memory map.
2. Rewrite the hosts file (global + per-device entries merged).
3. `touch` the file (update mtime) so glibc notices on next `getaddrinfo`.
4. If workers haven't started yet, the file will be mounted at startup.
   If workers are already running, the bind-mount already points to this
   file — just rewriting it is enough (glibc checks mtime).

No need to re-mount. No need for `apply_hosts_overlay`. Just write the file.

### `set_nameserver()` flow

1. Write `nameserver <ip>` to `<hosts_dir>/resolv.conf`.
2. Same logic: workers already have the bind-mount, glibc picks up changes.

### `spawn_command` `pre_exec`

Still needed: child processes are forked and inherit the parent's mount
namespace at fork time. If the sync worker has the overlay, the child inherits
it — **no separate `pre_exec` needed**. Actually: `fork()` preserves the
parent's mount namespace view. Since the sync worker already has `/etc/hosts`
bind-mounted, the child process will see it too. Remove the `pre_exec` hook.

Wait — verify this. The `cmd.spawn()` calls `fork()+exec()`. The forked child
inherits the parent thread's mount namespace. Since the sync worker thread did
`unshare(CLONE_NEWNS)` and then bind-mounted, the child should inherit those
mounts. Yes — verified: `fork()` inherits mount namespace.

So `inject_hosts_pre_exec` is unnecessary and can be removed.

### Implementation steps

| # | Task |
|---|------|
| 1 | Create hosts file + resolv.conf at `DnsEntries::new()` (empty initially) |
| 2 | Mount both files in worker thread startup (sync, async, on_thread_start) |
| 3 | `Lab::dns_entry` / `Device::dns_entry` — append + rewrite file |
| 4 | `Lab::set_nameserver` — write resolv.conf |
| 5 | Remove `inject_hosts_pre_exec` and `pre_exec` from `spawn_command` |
| 6 | `Lab::resolve` / `Device::resolve` — in-memory lookup |
| 7 | Tests |

### Tests

```
dns_std_to_socket_addrs         — run_sync: std ToSocketAddrs resolves custom name
dns_tokio_lookup                — spawn: tokio::net::lookup_host resolves custom name
dns_hickory_system_resolver     — spawn: hickory TokioResolver::from_system_conf resolves
dns_entry_visible_in_spawned_cmd — spawn_command: getent hosts resolves custom name
dns_entry_lab_wide              — lab.dns_entry visible from two devices
dns_entry_device_specific       — dev.dns_entry visible only to that device
dns_resolve_in_process          — lab.resolve / dev.resolve returns correct IPs
dns_entry_after_build           — entry added post-build, getent picks it up
dns_hosts_file_content          — cat /etc/hosts shows correct content
dns_set_nameserver              — set_nameserver; cat /etc/resolv.conf shows nameserver line
```

---

## Future work

### In-lab authoritative DNS server (hickory-server)

Run a real DNS server inside the lab root namespace so devices can resolve
names via standard DNS queries. Combine with `set_nameserver` to point
devices at it.

### In-lab ACME CA (pebble) + cert trust injection

Run pebble as a local ACME CA for TLS certificate provisioning in tests.
