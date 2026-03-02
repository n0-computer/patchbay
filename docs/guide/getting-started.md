# Getting Started

This guide walks you through your first patchbay lab: two routers, two
devices, and a ping across a NAT.

## Requirements

You need a Linux machine (bare-metal, VM, or CI container) with:

- **`tc`** and **`nft`** in your PATH (from `iproute2` and `nftables` packages)
- **Unprivileged user namespaces** enabled (default on most distros)

Check with:

```bash
sysctl kernel.unprivileged_userns_clone   # should be 1
```

On Ubuntu 24.04+ with AppArmor:

```bash
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
```

No `sudo` is needed at runtime. patchbay bootstraps into an unprivileged
user namespace where it has full networking capabilities.

## Add patchbay to your project

```bash
cargo add patchbay
```

Your `Cargo.toml` should include tokio with the `rt` and `macros` features
(at minimum) since patchbay uses async internally:

```toml
[dependencies]
patchbay = "0.1"
tokio = { version = "1", features = ["rt", "macros"] }
anyhow = "1"
```

## Enter the user namespace

Before any threads are spawned, call `init_userns()`. This enters an
unprivileged user namespace that gives patchbay the permissions it needs
to create network namespaces, veth pairs, and nftables rules.

```rust
fn main() -> anyhow::Result<()> {
    patchbay::init_userns().expect("failed to enter user namespace");
    async_main()
}

#[tokio::main]
async fn async_main() -> anyhow::Result<()> {
    // lab code goes here
    Ok(())
}
```

The split between `main()` and `async_main()` is necessary because
`init_userns()` must run before tokio starts its thread pool.

## Create a lab

A `Lab` is the top-level container. It creates a root network namespace
with an "internet exchange" (IX) bridge that interconnects all routers.

```rust
let lab = patchbay::Lab::new().await?;
```

## Add routers and devices

Routers connect to the IX and provide connectivity to downstream devices.
A plain router gives devices public IPs; adding `.nat(Nat::Home)` puts a
NAT in front of them, just like a home WiFi router.

```rust
use patchbay::{Nat, LinkCondition};

// Datacenter router: devices get public IPs.
let dc = lab.add_router("dc").build().await?;

// Home router: devices get private IPs behind NAT.
let home = lab.add_router("home").nat(Nat::Home).build().await?;
```

Devices attach to routers through named interfaces. You can optionally
apply link conditions (packet loss, latency, jitter) to simulate
real-world links.

```rust
let server = lab
    .add_device("server")
    .iface("eth0", dc.id(), None)
    .build()
    .await?;

let laptop = lab
    .add_device("laptop")
    .iface("eth0", home.id(), Some(LinkCondition::Wifi))
    .build()
    .await?;
```

## Ping across the NAT

Every device handle can spawn OS commands inside its network namespace.
Let's ping the server from the laptop:

```rust
let mut child = laptop.spawn_command({
    let mut cmd = std::process::Command::new("ping");
    cmd.args(["-c1", &server.ip().unwrap().to_string()]);
    cmd
})?;

let status = tokio::task::spawn_blocking(move || child.wait()).await??;
assert!(status.success());
```

The ping packet travels through the laptop's namespace, gets NATed at
the home router, crosses the IX bridge, and arrives at the server. The
reply follows the same path in reverse. All of this happens in real
kernel network stacks, isolated from your host.

## Run async code in a namespace

For anything more interesting than `ping`, you probably want async
networking. `device.spawn()` runs an async closure on a per-namespace
tokio runtime:

```rust
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

let addr = SocketAddr::from((server.ip().unwrap(), 8080));

// Start a TCP listener on the server.
let server_task = server.spawn(async move |_dev| {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let (mut stream, _peer) = listener.accept().await?;
    let mut buf = vec![0u8; 64];
    let n = stream.read(&mut buf).await?;
    assert_eq!(&buf[..n], b"hello");
    anyhow::Ok(())
})?;

// Connect from the laptop.
let client_task = laptop.spawn(async move |_dev| {
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    stream.write_all(b"hello").await?;
    anyhow::Ok(())
})?;

client_task.await??;
server_task.await??;
```

Both tasks run in separate network namespaces with fully isolated
stacks. The tokio primitives work exactly as they would in a normal
application. For more on running code in namespaces, see
[Running Code in Namespaces](running-code.md).

## Cleanup

When the `Lab` goes out of scope (or is dropped), all namespaces,
workers, and kernel resources are cleaned up automatically. No leftover
interfaces or namespaces pollute your host.

## What's next

- [Topology](topology.md) covers routers chains, multi-interface
  devices, regions, link conditions, and presets.
- [NAT and Firewalls](nat-and-firewalls.md) dives into NAT modes,
  IPv6 NAT, firewalls, and custom configurations.
- [Running Code](running-code.md) explains all the ways to execute
  code inside namespaces and dynamic operations like replug and link
  control.
