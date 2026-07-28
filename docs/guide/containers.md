# Running Containers

Some integration tests need auxiliary services that are easier to run as a
container than to embed: an ACME test server like [Pebble], a database, a
message broker, a real DNS server. patchbay can run these as ordinary lab
nodes.

A **container is a device**. It gets its own network namespace, a veth uplink
to a router, and a lab IP, exactly like a device created with
[`add_device`](https://docs.rs/patchbay/latest/patchbay/struct.Lab.html#method.add_device).
The difference is only what runs inside it: an OCI image instead of a Rust
closure. Other lab devices reach the service at the container's lab IP.

## Example

```rust,ignore
let net = lab.add_router("net").build().await?;

// Start Pebble as a lab device on `net`, and wait until it is listening.
let pebble = lab
    .add_container("pebble", "ghcr.io/letsencrypt/pebble:latest")
    .uplink(net.id())
    .env("PEBBLE_VA_NOSLEEP", "1")
    .ready_tcp(14000)
    .build()
    .await?;

// The directory URL other lab devices use.
let directory = format!("https://{}:14000/dir", pebble.ip().unwrap());

// A client device on the same network can now reach it.
let client = lab.add_device("client").uplink(net.id()).build().await?;
```

The builder mirrors the device builder for wiring (`uplink`, `iface`,
`default_via`, `mtu`) and adds container configuration:

| Method | Purpose |
|--------|---------|
| `env(k, v)` | Set an environment variable in the container. |
| `arg(a)` / `args([..])` | Append to the container's command. |
| `run_arg(a)` | Pass an extra flag to `podman run` (escape hatch). |
| `ready_tcp(port)` | Block `build` until `port` accepts a connection. |
| `runtime(name)` | Override the runtime binary (default `podman`). |

The returned [`Container`] dereferences to [`Device`], so `ip()`, `run_sync()`,
and the other device accessors work directly. It also has `exec()`, `logs()`,
and `stop()`. The container is removed when the handle is dropped.

## Runtime: podman, not docker

Containers run with **podman**, which must be on `PATH`. This is not
incidental. patchbay is rootless: the whole process lives in an unprivileged
user namespace, and each node's network namespace is entered by a worker thread,
not persisted to a path. podman is daemonless, so the `podman run` process,
forked from inside the device's worker thread, is already in the device's
network namespace; `--network=host` then means "use the namespace I am already
in", and the container lands on the device's lab IP with no extra plumbing.

Docker cannot do this rootlessly. Its client talks to `dockerd`, which runs in
its own namespace, so a docker container joins the daemon's namespace, not the
device's. Making docker work would require manipulating the container's
namespace from outside its owning user namespace, which rootless patchbay
cannot do.

## Requirements

Running a container needs a full subuid/subgid range inside patchbay's user
namespace, which is the standard rootless-podman setup:

- `podman`, `newuidmap`, and `newgidmap` on `PATH`.
- An `/etc/subuid` and `/etc/subgid` entry for your user (for example
  `you:100000:65536`).

patchbay maps that range with `newuidmap`/`newgidmap` when it bootstraps its
user namespace. Without it patchbay falls back to a single-uid namespace, which
cannot unpack image layers; `build()` then fails with a storage error. Router
and device networking work regardless.

The image is pulled on the host before the container starts, because the device
namespace it runs in has no route to a registry. If a specific image needs a
runtime tweak, pass it through with `run_arg`, for example
`run_arg("--cgroup-manager=cgroupfs")`.

[Pebble]: https://github.com/letsencrypt/pebble
[`Container`]: https://docs.rs/patchbay/latest/patchbay/struct.Container.html
[`Device`]: https://docs.rs/patchbay/latest/patchbay/struct.Device.html
