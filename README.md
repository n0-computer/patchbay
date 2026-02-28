# netsim

netsim lets you build realistic network topologies out of Linux network
namespaces and run real code against them. You define routers, devices, NAT
policies, and link conditions through a Rust builder API, and the library
wires everything up with veth pairs, nftables rules, and tc qdisc
scheduling. Each node gets its own namespace with a private network stack,
so processes running inside see what they would see on a separate machine.
The whole thing runs unprivileged and cleans up when the `Lab` is dropped.

Each router and device runs in its own Linux network namespace with private
interfaces, routes, and firewall rules. The library handles veth wiring,
address allocation, NAT configuration, and cleanup automatically.

## Quick example

See the [`simple.rs`](netsim/examples/simple.rs) example for the runnable version.

```rust
// We need to enter a user namespace before any threads are spawned.
netsim_core::init_userns().expect("failed to enter user namespace");

// Create a lab (async — sets up the root namespace and IX bridge).
let lab = Lab::new().await;

// A "datacenter" router: downstream devices get "public" IPs.
let dc = lab.add_router("dc").region("eu").build().await?;

// A "home" router with a NAT: downstream devices get private IPs.
let home = lab
    .add_router("home")
    .nat(Nat::Home)
    .build()
    .await?;

// A device behind the home router, with a lossy WiFi link.
let dev = lab
    .add_device("laptop")
    .iface("eth0", home.id(), Some(LinkCondition::Wifi))
    .build()
    .await?;

// A server in the datacenter.
let server = lab
    .add_device("server")
    .iface("eth0", dc.id(), None)
    .build()
    .await?;

// Run a command inside a device's network namespace.
let mut child = dev.spawn_command({
    let mut cmd = std::process::Command::new("ping");
    cmd.args(["-c1", &server.ip().unwrap().to_string()]);
    cmd
})?;

// Run async tasks inside a device's network namespace.
// Runs on a tokio runtime, so you can use all tokio networking primitives
// and everything that builds on top of it.
let client_task = dev.spawn(async move |_| {
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    println!("local addr: {}", stream.local_addr()?);
    stream.write_all(b"hello server").await?;
    anyhow::Ok(())
});
```


## Requirements

- Linux (bare-metal, VM, or CI container)
- `tc` and `nft` in PATH (for link conditions and NAT rules)
- Unprivileged user namespaces enabled (default on most distros):

  ```bash
  sysctl kernel.unprivileged_userns_clone   # check
  sudo sysctl -w kernel.unprivileged_userns_clone=1  # enable
  ```

  On Ubuntu 24.04+ with AppArmor:

  ```bash
  sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
  ```

No `sudo` is needed at runtime. The library bootstraps into an unprivileged
user namespace where it has full networking capabilities.

## Architecture

```
                        IX bridge (203.0.113.0/24)
                     ┌──────────┼──────────┐
                   dc(r2)    home(r4)    isp(r6)
                     │         │
                   br-lan    br-lan
                     │         │
                  server    laptop
```

**Namespaces.** Each node (router or device) gets a dedicated network
namespace. A lab-scoped root namespace hosts the IX bridge that interconnects
all top-level routers. Veth pairs connect namespaces across the topology.

**Worker threads.** Each namespace has a lazy async worker (single-threaded
tokio runtime) and a lazy sync worker. `device.spawn(...)` runs async tasks
on the namespace's tokio runtime; `device.run_sync(...)` dispatches closures
to the sync worker. This means callers never need to worry about `setns` —
the workers handle namespace entry.

**NAT.** Routers support four IPv4 NAT modes (`None`, `Cgnat`,
`Home`, `Symmetric`) and three IPv6 modes
(`None`, `Nptv6`, `Masquerade`), configured via nftables rules.

**Link conditions.** `tc netem` and `tc tbf` provide packet loss, latency,
and rate limiting. Apply presets (`LinkCondition::Wifi`, `LinkCondition::Mobile4G`) or
custom values at build time or dynamically.

**Cleanup.** Namespace file descriptors are held in-process. When the `Lab`
is dropped, workers are shut down and namespaces disappear automatically.

## API overview

### Building a topology

```rust
let lab = Lab::new().await;

// Routers
let dc = lab.add_router("dc")
    .region("eu")
    .build().await?;

let home = lab.add_router("home")
    .upstream(dc.id())           // chain behind dc
    .nat(Nat::Home)
    .ip_support(IpSupport::DualStack)
    .nat_v6(NatV6Mode::Nptv6)
    .build().await?;

// Devices
let dev = lab.add_device("phone")
    .iface("wlan0", home.id(), Some(LinkCondition::Wifi))
    .iface("eth0", dc.id(), None)
    .default_via("wlan0")
    .build().await?;

// Inter-region latency
lab.set_region_latency("eu", "us", 80);
```

### Running code in namespaces

```rust
// Async task on the device's tokio runtime (closure receives a Device handle)
dev.spawn(|_dev| async {
    let stream = tokio::net::TcpStream::connect("203.0.113.10:80").await?;
    Ok::<_, anyhow::Error>(())
});

// Blocking closure on the sync worker
let sock = dev.run_sync(|| {
    Ok(std::net::UdpSocket::bind("0.0.0.0:0")?)
})?;

// Spawn an OS command (sync)
let child = dev.spawn_command({
    let mut cmd = std::process::Command::new("curl");
    cmd.arg("http://203.0.113.10");
    cmd
})?;

// Spawn an OS command (async — returns tokio::process::Child)
let child = dev.spawn_command_async({
    let mut cmd = tokio::process::Command::new("curl");
    cmd.arg("http://203.0.113.10");
    cmd
})?;

// Dedicated OS thread
let handle = dev.spawn_thread(|| {
    // long-running work in the namespace
    Ok(())
})?;
```

### Dynamic operations

```rust
// Switch a device's uplink to a different router at runtime
dev.replug_iface("wlan0", other_router.id()).await?;

// Switch default route between interfaces
dev.set_default_route("eth0").await?;

// Link down / up
dev.link_down("wlan0").await?;
dev.link_up("wlan0").await?;

// Change link condition dynamically
dev.set_link_condition("wlan0", Some(LinkCondition::Manual {
    rate: 1000,      // kbit/s
    loss: 5.0,       // percent
    latency: 100,    // ms one-way
}))?;

// Change NAT mode at runtime
router.set_nat_mode(Nat::Symmetric)?;
router.flush_nat_state()?;  // flush conntrack
```

### Handles

`Device`, `Router`, and `Ix` are lightweight cloneable handles. All three
provide `spawn`, `run_sync`, `spawn_thread`, `spawn_command`,
`spawn_command_async`, and `spawn_reflector` for running code in their namespace.

## TOML configuration

Labs can also be loaded from TOML files via `Lab::load("lab.toml")`:

```toml
[[router]]
name = "dc"
region = "eu"

[[router]]
name = "home"
nat = "home"

[device.laptop.eth0]
gateway = "home"

[region.eu]
latencies = { us = 80 }
```

## Workspace crates

| Crate | Description |
|-------|-------------|
| `netsim-core` | Core library: topology builder, namespace management, NAT, link conditions |
| `netsim` | CLI runner for TOML-defined simulations with step sequencing |
| `netsim-vm` | QEMU VM wrapper for running simulations on macOS |
| `netsim-utils` | Shared utilities |

## TOML simulation runner

The `netsim` binary runs simulations defined in TOML files with a step-based
execution model: spawn processes, apply link conditions, wait for captures, and
assert on outputs.

```bash
cargo install --git https://github.com/n0-computer/netsim-rs

# Run a simulation
netsim run ./sims/iperf-baseline.toml

# Run all sims discovered from netsim.toml
netsim run
```

See [docs/reference.md](docs/reference.md) for the full simulation file
syntax.

## VM mode (macOS)

The `netsim-vm` crate wraps simulations in a QEMU Linux VM, allowing
development on macOS:

```bash
cargo install --git https://github.com/n0-computer/netsim-rs netsim-vm
netsim-vm run ./sims/my-sim.toml
netsim-vm down
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
