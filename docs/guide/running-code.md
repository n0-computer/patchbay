# Running Code in Namespaces

Every node in a patchbay topology, whether it is a device, a router, or
the IX itself, has its own Linux network namespace. Each namespace comes
with two workers: an async worker backed by a single-threaded tokio
runtime, and a sync worker backed by a dedicated OS thread. You never
interact with `setns` directly; the workers enter the correct namespace
before executing your code.

This chapter describes all the execution methods available on node handles,
when to use each one, and how to modify the topology at runtime.

## Async tasks

The `spawn` method is the primary way to run networking code inside a
namespace. It takes an async closure, dispatches it to the namespace's
tokio runtime, and returns a join handle that resolves when the task
completes:

```rust
let handle = dev.spawn(async move |_dev| {
    let stream = tokio::net::TcpStream::connect("203.0.113.10:80").await?;
    let mut buf = vec![0u8; 1024];
    let n = stream.read(&mut buf).await?;
    anyhow::Ok(n)
})?;

let bytes_read = handle.await??;
```

The closure receives a clone of the device handle, which you can use to
query addresses or spawn further tasks. All tokio networking primitives
work inside `spawn`: `TcpStream`, `TcpListener`, `UdpSocket`, timeouts,
intervals, and anything built on top of them. Because the runtime is
single-threaded and pinned to the namespace, all socket operations happen
against the namespace's isolated network stack.

You should use `spawn` for any work that involves network I/O. The
alternative, blocking I/O in a sync context, will stall the worker thread
and can cause kernel-level timeouts for TCP (SYN retransmit takes roughly
127 seconds to exhaust). Always prefer async networking via `spawn`.

## Sync closures

The `run_sync` method dispatches a closure to the namespace's sync worker
thread and blocks until it returns. It is intended for quick, non-I/O
operations: reading a sysctl value, creating a socket to inspect its local
address, or spawning an OS process.

```rust
let local_addr = dev.run_sync(|| {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
    Ok(sock.local_addr()?)
})?;
```

Because `run_sync` blocks both the calling thread and the sync worker,
avoid doing anything slow inside it. TCP connects, HTTP requests, and
other blocking network I/O belong in `spawn`, not in `run_sync`.

## OS commands

`spawn_command` runs an OS process inside the namespace and registers
the child with the namespace's tokio reactor, so `.wait()` and
`.wait_with_output()` work as non-blocking futures. It takes a
`tokio::process::Command` and returns a `tokio::process::Child`:

```rust
let mut child = dev.spawn_command({
    let mut cmd = tokio::process::Command::new("curl");
    cmd.arg("http://203.0.113.10");
    cmd
})?;

let status = child.wait().await?;
assert!(status.success());
```

When you need a synchronous `std::process::Child` instead (for example
to pass to `spawn_blocking` or manage outside of an async context), use
`spawn_command_sync`:

```rust
let mut child = dev.spawn_command_sync({
    let mut cmd = std::process::Command::new("curl");
    cmd.arg("http://203.0.113.10");
    cmd
})?;

let output = tokio::task::spawn_blocking(move || {
    child.wait_with_output()
}).await??;
assert!(output.status.success());
```

## Dedicated threads

When you have long-running blocking work that would starve the sync
worker, `spawn_thread` creates a dedicated OS thread inside the
namespace. Unlike `run_sync`, this thread does not compete with other
sync operations on the same namespace:

```rust
let handle = dev.spawn_thread(|| {
    // This thread runs in the device's namespace.
    // It can do blocking work for an extended period
    // without affecting run_sync calls.
    Ok(())
})?;
```

## UDP reflectors

`spawn_reflector` starts a UDP echo server in the namespace. It is a
convenience method for connectivity tests: send a datagram to the
reflector and measure the round-trip time to verify that the path works.

```rust
let bind_addr = SocketAddr::new(IpAddr::V4(server_ip), 9000);
server.spawn_reflector(bind_addr)?;
```

The reflector runs on the namespace's async worker and echoes every
received datagram back to its sender.

## Dynamic topology operations

A patchbay topology is not static. After building the initial layout, you
can modify interfaces, routes, link conditions, and NAT configuration at
runtime. These operations are useful for simulating network events during
a test: a WiFi handoff, a link failure, or a NAT policy change.

### Replugging interfaces

Move a device's interface from one router to another. The interface
receives a new IP address from the new router's pool, and routes are
updated automatically:

```rust
dev.replug_iface("wlan0", other_router.id()).await?;
```

This models scenarios like roaming between WiFi access points or
switching between ISPs.

### Switching the default route

For multi-homed devices, change which interface carries the default route.
This simulates a WiFi-to-cellular handoff or a VPN tunnel activation:

```rust
dev.set_default_route("cell0").await?;
```

### Bringing interfaces down and up

Simulate link failures by administratively disabling an interface. While
the interface is down, packets sent to or from it are dropped:

```rust
dev.link_down("wlan0").await?;
// All traffic over wlan0 is now dropped.

dev.link_up("wlan0").await?;
// The interface is back and traffic flows again.
```

### Changing link conditions at runtime

Modify link impairment on the fly to simulate degrading or improving
network quality:

```rust
use patchbay::{LinkCondition, LinkLimits};

// Switch to a 3G-like link.
dev.set_link_condition("wlan0", Some(LinkCondition::Mobile3G)).await?;

// Apply custom impairment.
dev.set_link_condition("wlan0", Some(LinkCondition::Manual(LinkLimits {
    rate_kbit: 500,
    loss_pct: 15.0,
    latency_ms: 200,
    ..Default::default()
}))).await?;

// Remove all impairment and return to a clean link.
dev.set_link_condition("wlan0", None).await?;
```

### Changing NAT at runtime

Switch a router's NAT mode and flush stale connection tracking state.
This is covered in more detail in the
[NAT and Firewalls](nat-and-firewalls.md) chapter:

```rust
router.set_nat_mode(Nat::Corporate).await?;
router.flush_nat_state().await?;
```

## Handles

`Device`, `Router`, and `Ix` are lightweight, cloneable handles. All three
types support the same set of execution methods described above: `spawn`,
`run_sync`, `spawn_thread`, `spawn_command`, `spawn_command_sync`, and
`spawn_reflector`. Cloning a handle is cheap; it does not duplicate the
underlying namespace or its workers.

Handle methods return `Result` or `Option` when the underlying node has
been removed from the lab. If you hold a handle to a device that no longer
exists, calls will return an error rather than panicking.

When debugging IPv6 behavior, inspect interface snapshots instead of only
top-level `ip6()` accessors:

- `device.default_iface().and_then(|i| i.ip6())` for global/ULA IPv6.
- `device.default_iface().and_then(|i| i.ll6())` for link-local `fe80::/10`.
- `router.interfaces()` for `RouterIface` snapshots on `ix`/`wan` and bridge.

## Cleanup

When the `Lab` is dropped, it shuts down all async and sync workers, then
closes the namespace file descriptors. The kernel removes veth pairs,
routes, and nftables rules when the last reference to a namespace
disappears. No explicit cleanup is needed, and no state leaks onto the
host between test runs.
