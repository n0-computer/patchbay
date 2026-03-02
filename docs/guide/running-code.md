# Running Code in Namespaces

Every node in patchbay (devices, routers, the IX) has its own network
namespace with an async worker and a sync worker. You never call `setns`
yourself; the workers handle namespace entry transparently.

## Async tasks with `spawn`

`spawn` runs an async closure on the node's single-threaded tokio
runtime. This is the primary way to run network code:

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
query addresses or spawn further tasks. The returned handle is a
`JoinHandle` that resolves when the task completes.

All tokio networking primitives work inside `spawn`: TCP, UDP,
`TcpListener`, `UdpSocket`, timeouts, intervals, and everything built
on top of them.

## Sync closures with `run_sync`

`run_sync` dispatches a closure to the namespace's sync worker thread
and blocks until it returns. Use this for quick non-I/O operations like
reading a sysctl value or spawning a process:

```rust
let local_addr = dev.run_sync(|| {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
    Ok(sock.local_addr()?)
})?;
```

Do not do blocking network I/O (TCP connect, HTTP requests) inside
`run_sync`. The sync worker is a single thread; blocking it stalls all
other `run_sync` calls for that namespace. Use `spawn` with tokio
networking instead.

## OS commands with `spawn_command`

`spawn_command` runs an OS process inside the namespace. It returns a
`std::process::Child` that you manage yourself:

```rust
let mut child = dev.spawn_command({
    let mut cmd = std::process::Command::new("curl");
    cmd.arg("http://203.0.113.10");
    cmd
})?;

let output = tokio::task::spawn_blocking(move || child.wait_with_output()).await??;
assert!(output.status.success());
```

For async command management, use `spawn_command_async` which takes a
`tokio::process::Command` and returns a `tokio::process::Child`:

```rust
let child = dev.spawn_command_async({
    let mut cmd = tokio::process::Command::new("iperf3");
    cmd.args(["-c", "203.0.113.10"]);
    cmd
})?;
```

## Dedicated threads with `spawn_thread`

For long-running blocking work that would starve the sync worker, use
`spawn_thread` to get a dedicated OS thread in the namespace:

```rust
let handle = dev.spawn_thread(|| {
    // Long-running blocking work here.
    // This thread is in the device's network namespace.
    Ok(())
})?;
```

## UDP reflectors

`spawn_reflector` is a convenience method that starts a UDP echo server
in the namespace. Useful for connectivity tests:

```rust
let bind_addr = SocketAddr::new(IpAddr::V4(server_ip), 9000);
server.spawn_reflector(bind_addr)?;
```

## Dynamic operations

After building a topology, you can modify it at runtime.

### Replug interfaces

Move a device's interface to a different router:

```rust
dev.replug_iface("wlan0", other_router.id()).await?;
```

The interface gets a new IP from the new router's pool, and routes are
updated automatically.

### Switch default route

When a device has multiple interfaces, switch which one carries the
default route:

```rust
dev.set_default_route("cell0").await?;
```

### Link down / up

Simulate a cable unplug or WiFi disconnect:

```rust
dev.link_down("wlan0").await?;
// Interface is down, packets are dropped.

dev.link_up("wlan0").await?;
// Back online.
```

### Change link conditions

Degrade or improve a link at runtime:

```rust
use patchbay::{LinkCondition, LinkLimits};

// Switch to 3G
dev.set_link_condition("wlan0", Some(LinkCondition::Mobile3G)).await?;

// Custom degradation
dev.set_link_condition("wlan0", Some(LinkCondition::Manual(LinkLimits {
    rate_kbit: 500,
    loss_pct: 15.0,
    latency_ms: 200,
    ..Default::default()
}))).await?;

// Remove all impairment
dev.set_link_condition("wlan0", None).await?;
```

### Change NAT mode

Switch a router's NAT mode and flush stale conntrack entries:

```rust
router.set_nat_mode(Nat::Corporate).await?;
router.flush_nat_state().await?;
```

## Handles

`Device`, `Router`, and `Ix` are lightweight, cloneable handles. All
three support the same execution methods: `spawn`, `run_sync`,
`spawn_thread`, `spawn_command`, `spawn_command_async`, and
`spawn_reflector`.

Handle methods return `Result` or `Option` when the underlying node has
been removed from the lab. Cloning a handle is cheap and does not
duplicate the namespace or workers.

## Cleanup

When the `Lab` is dropped, all workers are shut down and namespace file
descriptors are closed. The kernel automatically cleans up veth pairs,
routes, and nftables rules when the namespace disappears. No manual
cleanup is needed.
