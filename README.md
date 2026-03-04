# patchbay

patchbay lets you build realistic network topologies out of Linux network
namespaces and run real code against them. You define routers, devices, NAT
policies, and link conditions through a Rust builder API, and the library
wires everything up with veth pairs, nftables rules, and tc qdisc
scheduling. Each node gets its own namespace with a private network stack,
so processes running inside see what they would see on a separate machine.
Everything runs unprivileged and cleans up when the `Lab` is dropped.

## Quick example

See the [`simple.rs`](patchbay-runner/examples/simple.rs) example for the runnable version.

```rust
// Enter a user namespace before any threads are spawned.
patchbay::init_userns().expect("failed to enter user namespace");

// Create a lab (async - sets up the root namespace and IX bridge).
let lab = Lab::new().await?;

// A "datacenter" router: downstream devices get public IPs.
let dc = lab.add_router("dc").build().await?;

// A "home" router with NAT: downstream devices get private IPs.
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
let mut child = dev.spawn_command_sync({
    let mut cmd = std::process::Command::new("ping");
    cmd.args(["-c1", &server.ip().unwrap().to_string()]);
    cmd
})?;

// Run an async task inside a device's network namespace.
// This runs on a per-namespace tokio runtime, so you can use all
// tokio networking primitives and everything built on top of them.
let client_task = dev.spawn(|_dev| async move {
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    println!("local addr: {}", stream.local_addr()?);
    stream.write_all(b"hello server").await?;
    anyhow::Ok(())
})?;
```


## Requirements

- Linux (bare-metal, VM, or CI container).
- `tc` and `nft` in PATH (for link conditions and NAT rules).
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

Every node (router or device) gets its own network namespace. A lab-scoped
root namespace hosts the IX bridge that interconnects all top-level routers.
Veth pairs connect namespaces across the topology.

Each namespace has a lazy async worker (single-threaded tokio runtime) and a
lazy sync worker. `device.spawn(...)` runs async tasks on the namespace's
tokio runtime; `device.run_sync(...)` dispatches closures to the sync
worker. Callers never need to worry about `setns`; the workers handle
namespace entry.

### Multi-region routing

Routers can be assigned to regions, and regions can be linked with simulated
latency. When two routers live in different regions, traffic between them
flows through per-region router namespaces with configurable impairment,
giving you realistic cross-continent delays on top of the per-link
conditions.

```rust
let eu = lab.add_region("eu").await?;
let us = lab.add_region("us").await?;
lab.link_regions(&eu, &us, RegionLink::good(80)).await?;

let dc_eu = lab.add_router("dc-eu").region(&eu).build().await?;
let dc_us = lab.add_router("dc-us").region(&us).build().await?;
// Traffic between dc-eu and dc-us now carries 80ms of added latency.
```

You can also tear down and restore region links at runtime with
`lab.break_region_link()` and `lab.restore_region_link()` for fault
injection scenarios.

### Router presets

`RouterPreset` configures NAT, firewall, IP support, and address pool in
one call to match real-world deployment patterns:

```rust
let home = lab.add_router("home").preset(RouterPreset::Home).build().await?;
let dc   = lab.add_router("dc").preset(RouterPreset::Datacenter).build().await?;
let corp = lab.add_router("corp").preset(RouterPreset::Corporate).build().await?;
```

Available presets: `Home`, `Datacenter`, `IspV4`, `Mobile`, `MobileV6`,
`Corporate`, `Hotel`, `Cloud`. Individual methods called after `preset()`
override preset values. See [docs/reference/ipv6.md](docs/reference/ipv6.md) for the full
reference table.

### NAT

Routers support six IPv4 NAT presets (`None`, `Home`, `Corporate`,
`CloudNat`, `FullCone`, `Cgnat`) and four IPv6 modes (`None`, `Nptv6`,
`Masquerade`, `Nat64`), all configured via nftables rules. You can also
build custom NAT configs from mapping + filtering + timeout parameters.

**NAT64** provides IPv4 access for IPv6-only devices via the well-known
prefix `64:ff9b::/96`. A userspace SIIT translator on the router converts
between IPv6 and IPv4 headers; nftables masquerade handles port mapping.
Use `RouterPreset::MobileV6` or `.nat_v6(NatV6Mode::Nat64)` directly.
See [docs/reference/ipv6.md](docs/reference/ipv6.md) for details.

### IPv6 link-local and provisioning modes

Every IPv6-capable device/router interface now exposes a link-local address
through the handle snapshots:

- `Device::default_iface().and_then(|i| i.ll6())`
- `Device::interfaces().iter().filter_map(|i| i.ll6())`
- `router.iface("ix").or_else(|| router.iface("wan")).and_then(|i| i.ll6())`
- `router.interfaces().iter().filter_map(|i| i.ll6())`

Patchbay also supports explicit IPv6 provisioning and DAD modes via `LabOpts`:

```rust
let lab = Lab::with_opts(
    LabOpts::default()
        .ipv6_provisioning_mode(Ipv6ProvisioningMode::Static)
        .ipv6_dad_mode(Ipv6DadMode::Enabled),
).await?;
```

`Ipv6ProvisioningMode::Static` keeps route wiring deterministic.  
`Ipv6ProvisioningMode::RaDriven` enables patchbay's RA/RS-driven path.  
`Ipv6DadMode::Disabled` is the current default for deterministic test setup.

### Firewalls

Firewall presets control both inbound and outbound traffic:
`BlockInbound` (RFC 6092 CE router), `Corporate` (TCP 80,443 + UDP 53),
`CaptivePortal` (block non-web UDP). All presets expand to a
`FirewallConfig` which can also be built from scratch via the builder API.

### Link conditions

`tc netem` and `tc tbf` provide packet loss, latency, jitter, and rate
limiting. Apply presets (`LinkCondition::Wifi`, `LinkCondition::Mobile4G`)
or custom values at build time or dynamically.

### Cleanup

Namespace file descriptors are held in-process. When the `Lab` is dropped,
workers are shut down and namespaces disappear automatically.

## API overview

### Building a topology

```rust
let lab = Lab::new().await?;

// Regions (optional)
let eu = lab.add_region("eu").await?;
let us = lab.add_region("us").await?;
lab.link_regions(&eu, &us, RegionLink::good(80)).await?;

// Routers
let dc = lab.add_router("dc")
    .region(&eu)
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
```

### Running code in namespaces

```rust
// Async task on the device's tokio runtime
let jh = dev.spawn(|_dev| async move {
    let stream = tokio::net::TcpStream::connect("203.0.113.10:80").await?;
    Ok::<_, anyhow::Error>(())
})?;

// Blocking closure on the sync worker
let local_addr = dev.run_sync(|| {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
    Ok(sock.local_addr()?)
})?;

// Spawn an OS command (sync, returns std::process::Child)
let child = dev.spawn_command({
    let mut cmd = tokio::process::Command::new("curl");
    cmd.arg("http://203.0.113.10");
    cmd
})?;

// Spawn an OS command (sync, returns std::process::Child)
let child = dev.spawn_command_sync({
    let mut cmd = std::process::Command::new("curl");
    cmd.arg("http://203.0.113.10");
    cmd
})?;

// Dedicated OS thread in the namespace
let handle = dev.spawn_thread(|| {
    // long-running work
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
dev.set_link_condition("wlan0", Some(LinkCondition::Manual(LinkLimits {
    rate_kbit: 1000,
    loss_pct: 5.0,
    latency_ms: 100,
    ..Default::default()
}))).await?;

// Change NAT mode at runtime
router.set_nat_mode(Nat::Corporate).await?;
router.flush_nat_state().await?;
```

### Handles

`Device`, `Router`, and `Ix` are lightweight, cloneable handles. All three
provide `spawn`, `run_sync`, `spawn_thread`, `spawn_command`,
`spawn_command_sync`, and `spawn_reflector` for running code in their
namespace. Handle methods return `Result` or `Option` when the underlying
node has been removed from the lab.

For IPv6 diagnostics, use per-interface snapshots instead of only `ip6()`:

- `DeviceIface::ip6()` for global/ULA address.
- `DeviceIface::ll6()` for `fe80::/10` link-local address.
- `RouterIface::ip6()` and `RouterIface::ll6()` for router-side interface state.

## TOML configuration

You can also load labs from TOML files via `Lab::load("lab.toml")`:

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

## Devtools UI

patchbay includes a built-in web UI for inspecting lab runs. Set
`PATCHBAY_OUTDIR` to write structured output (topology events, per-namespace
tracing logs, extracted events) to disk, then serve it in the browser:

```bash
# From a cargo test
PATCHBAY_OUTDIR=/tmp/pb cargo test my_test
patchbay serve /tmp/pb --open

# From the TOML runner (auto-serves with --open)
patchbay run ./sims/my-sim.toml --open
```

The UI provides five tabs:

- **Topology** — interactive graph of routers, devices, and links with a
  detail sidebar showing NAT, firewall, IPs, and counters.
- **Events** — table of lab lifecycle events (router added, device added,
  NAT changed, etc.) with relative/absolute timestamps.
- **Logs** — per-namespace tracing log viewer with JSON parsing, level
  badges, and target filtering. Supports jump-to-log from the timeline.
- **Timeline** — grid of extracted `_events` per node over time, with
  detail pane and jump-to-log.
- **Perf** — throughput results table (only for TOML runner sims).

Each `Lab` instance writes to a timestamped subdirectory under the outdir.
Multiple runs accumulate in the same outdir and appear in the run selector.

### Output from Rust tests

To enable devtools output from `cargo test`, pass the outdir via the
`PATCHBAY_OUTDIR` environment variable:

```rust
let lab = Lab::with_opts(LabOpts::default().label("my-test")).await?;
// ... run your test ...
// Lab writes events.jsonl, state.json, *.tracing.jsonl, *.events.jsonl
// to $PATCHBAY_OUTDIR/{timestamp}-my-test/
```

You can also emit custom events to the timeline by using tracing targets
with the `_events::` convention:

```rust
tracing::info!(target: "myapp::_events::ConnectionEstablished", peer = %addr);
```

These appear as timeline events in the UI, extracted automatically by the
per-namespace tracing subscriber.

## Workspace crates

| Crate | Description |
|-------|-------------|
| `patchbay` | Core library: topology builder, namespace management, NAT, link conditions |
| `patchbay-runner` | CLI runner for TOML-defined simulations with step sequencing |
| `patchbay-server` | Embedded devtools HTTP server with run discovery and SSE |
| `patchbay-vm` | QEMU VM wrapper for running simulations on macOS |
| `patchbay-utils` | Shared utilities |

## TOML simulation runner

The `patchbay` binary runs simulations defined in TOML files with a step-based
execution model: spawn processes, apply link conditions, wait for captures, and
assert on outputs.

```bash
cargo install --git https://github.com/n0-computer/patchbay

# Run a simulation
patchbay run ./sims/iperf-baseline.toml

# Run all sims discovered from patchbay.toml
patchbay run

# Serve a completed run directory in the browser
patchbay serve /path/to/outdir --open
```

See [docs/reference/toml-reference.md](docs/reference/toml-reference.md) for the full simulation file syntax.

## VM mode (macOS)

The `patchbay-vm` crate wraps simulations in a QEMU Linux VM, allowing
development on macOS:

```bash
cargo install --git https://github.com/n0-computer/patchbay patchbay-vm
patchbay-vm run ./sims/my-sim.toml
patchbay-vm down
```

## Documentation

| Document | Description |
|----------|-------------|
| [docs/reference/ipv6.md](docs/reference/ipv6.md) | Real-world IPv6 deployments, NAT64, router presets reference table |
| [docs/reference/patterns.md](docs/reference/patterns.md) | Simulating VPNs, WiFi handoff, captive portals, and other network events |
| [docs/reference/holepunching.md](docs/reference/holepunching.md) | NAT implementation details, hole-punching mechanics, nftables fullcone map |
| [docs/reference/toml-reference.md](docs/reference/toml-reference.md) | TOML simulation file syntax |

## License

Copyright 2026 N0, INC.

This project is licensed under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or
   http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or
   http://opensource.org/licenses/MIT)

at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
