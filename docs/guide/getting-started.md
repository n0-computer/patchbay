# Getting Started

This chapter walks through building your first patchbay lab: a home
router with NAT, a datacenter router, and two devices that communicate
across them. By the end you will have a working topology with a ping
traversing a NAT and an async TCP exchange between two isolated network
stacks.

## System requirements

patchbay needs a Linux environment. A bare-metal machine, a VM, or a CI
container all work. You need two userspace tools in your PATH:

- **`tc`** from the `iproute2` package, used for link condition shaping.
- **`nft`** from the `nftables` package, used for NAT and firewall rules.

You also need unprivileged user namespaces, which are enabled by default
on most distributions. You can verify this with:

```bash
sysctl kernel.unprivileged_userns_clone
```

If the value is 0, enable it with `sudo sysctl -w kernel.unprivileged_userns_clone=1`.
On Ubuntu 24.04 and later, AppArmor restricts unprivileged user namespaces
separately:

```bash
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
```

No root access is needed at runtime. patchbay enters an unprivileged user
namespace at startup that grants it the capabilities needed to create
network namespaces, veth pairs, and nftables rules.

## Adding patchbay to your project

Add patchbay and its runtime dependencies to your `Cargo.toml`. You need
tokio with at least the `rt` and `macros` features, since patchbay is async
internally:

```toml
[dependencies]
patchbay = "0.1"
tokio = { version = "1", features = ["rt", "macros"] }
anyhow = "1"
```

## Entering the user namespace

Before any threads are spawned, your program must call `init_userns()` to
enter the unprivileged user namespace. This has to happen before tokio
starts its thread pool, because `unshare(2)` only works in a
single-threaded process. The standard pattern splits `main` into a sync
entry point and an async body:

```rust
fn main() -> anyhow::Result<()> {
    patchbay::init_userns().expect("failed to enter user namespace");
    async_main()
}

#[tokio::main]
async fn async_main() -> anyhow::Result<()> {
    // All lab code goes here.
    Ok(())
}
```

If you skip this call, `Lab::new()` will fail because the process lacks
the network namespace capabilities it needs.

In integration tests, you can avoid the `main` / `async_main` split by
using a `#[ctor]` initializer that runs before any test thread is spawned:

```rust
#[cfg(test)]
#[ctor::ctor]
fn init() {
    patchbay::init_userns().expect("failed to enter user namespace");
}

#[tokio::test]
async fn my_test() -> anyhow::Result<()> {
    let lab = patchbay::Lab::new().await?;
    // ...
    Ok(())
}
```

The `ctor` crate runs the function at load time, before `main` or the
test harness starts. This keeps your test functions clean and avoids
repeating the namespace setup in every binary.

## Creating a lab

A `Lab` is the top-level container for a topology. When you create one, it
sets up a root network namespace with an internet exchange (IX) bridge.
Every top-level router connects to this bridge, which provides the
backbone for inter-router connectivity.

```rust
let lab = patchbay::Lab::new().await?;
```

## Adding routers and devices

Routers connect to the IX bridge and provide network access to downstream
devices. A router without any NAT configuration gives its devices public
IP addresses, like a datacenter. Adding `.nat(Nat::Home)` places a NAT in
front of the router's downstream, assigning devices private addresses and
masquerading their traffic, like a typical home WiFi router.

```rust
use patchbay::{Nat, LinkCondition};

// A datacenter router whose devices get public IPs.
let dc = lab.add_router("dc").build().await?;

// A home router whose devices sit behind NAT.
let home = lab.add_router("home").nat(Nat::Home).build().await?;
```

Devices attach to routers through named network interfaces. Each interface
is a veth pair connecting the device's namespace to the router's namespace.
You can optionally apply a link condition to the interface to simulate
real-world impairment like packet loss, latency, and jitter.

```rust
// A server in the datacenter, with a clean link.
let server = lab
    .add_device("server")
    .iface("eth0", dc.id(), None)
    .build()
    .await?;

// A laptop behind the home router, over a lossy WiFi link.
let laptop = lab
    .add_device("laptop")
    .iface("eth0", home.id(), Some(LinkCondition::Wifi))
    .build()
    .await?;
```

At this point you have four network namespaces (IX root, dc router, home
router with NAT, server, laptop) wired together with veth pairs. The
laptop has a private IP behind the home router's NAT, and the server has a
public IP on the datacenter router's subnet.

## Running a ping across the NAT

Every device handle can spawn OS commands inside its network namespace. To
verify connectivity, ping the server from the laptop:

```rust
let mut child = laptop.spawn_command_sync({
    let mut cmd = std::process::Command::new("ping");
    cmd.args(["-c1", &server.ip().unwrap().to_string()]);
    cmd
})?;

let status = tokio::task::spawn_blocking(move || child.wait()).await??;
assert!(status.success());
```

The ICMP echo request travels from the laptop's namespace through the home
router, where nftables masquerade translates the source address. The
packet then crosses the IX bridge, enters the datacenter router's
namespace, and arrives at the server. The reply follows the reverse path.
All of this happens in real kernel network stacks, fully isolated from
your host.

## Running async code in a namespace

For anything beyond shell commands, you will want to run async Rust code
inside a namespace. The `spawn` method runs an async closure on the
device's single-threaded tokio runtime, giving you access to the full
tokio networking stack (TCP, UDP, listeners, timeouts) within that
namespace's isolated network:

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

// Connect from the laptop. Traffic is NATed through the home router.
let client_task = laptop.spawn(async move |_dev| {
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    stream.write_all(b"hello").await?;
    anyhow::Ok(())
})?;

client_task.await??;
server_task.await??;
```

Both tasks run in separate network namespaces with completely isolated
stacks. The tokio primitives behave exactly as they would in a normal
application, but all traffic flows through the simulated topology. The
[Running Code in Namespaces](running-code.md) chapter covers all
execution methods in detail.

## Cleanup

When the `Lab` goes out of scope, it shuts down all namespace workers and
closes the namespace file descriptors. The kernel automatically removes
veth pairs, routes, and nftables rules when the last reference to a
namespace disappears. No cleanup code is needed and no leftover state
pollutes the host.

## Viewing results in the browser

patchbay can write structured output to disk, including topology events,
per-namespace tracing logs, and extracted custom events, and serve them
in an interactive web UI. Set the `PATCHBAY_OUTDIR` environment variable
to enable this:

```bash
PATCHBAY_OUTDIR=/tmp/pb cargo test my_test
```

Each `Lab` creates a timestamped subdirectory under the outdir. You can
optionally label it for easier identification:

```rust
let lab = Lab::with_opts(LabOpts::default().label("my-test")).await?;
```

After the test completes, serve the output directory:

```bash
patchbay serve /tmp/pb --open
```

This opens the devtools UI in your browser with tabs for topology, events,
logs, timeline, and performance results. Multiple runs accumulate in the
same outdir and appear in the run selector dropdown.

You can also emit custom events to the timeline using the `_events::`
tracing target convention:

```rust
tracing::info!(target: "myapp::_events::PeerConnected", addr = %peer_addr);
```

The per-namespace tracing subscriber extracts these into `.events.jsonl`
files, which the timeline tab renders automatically.

## What comes next

The following chapters cover patchbay's features in more depth:

- [Building Topologies](topology.md) explains router chains, multi-homed
  devices, regions with inter-region latency, link condition presets, and
  router presets.
- [NAT and Firewalls](nat-and-firewalls.md) covers all NAT modes
  (including NAT64 for IPv6-only networks), firewall presets, custom
  configurations, and runtime changes.
- [Running Code in Namespaces](running-code.md) describes the execution
  model, all the ways to run code inside a namespace, and dynamic topology
  operations like interface replug and link control.
