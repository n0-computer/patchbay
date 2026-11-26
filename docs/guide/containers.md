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
`default_via`, `mtu`, `ipv6_provisioning_mode`) and adds container
configuration:

| Method | Purpose |
|--------|---------|
| `env(k, v)` | Set an environment variable in the container. |
| `entrypoint([..])` | Replace the image's entrypoint. An empty list clears it. |
| `arg(a)` / `args([..])` | Append to the container's command. |
| `volume(host, container)` | Bind-mount a host path read-write. |
| `volume_ro(host, container)` | Bind-mount a host path read-only. |
| `tmpfs(container)` | Mount a fresh tmpfs at a path. |
| `run_arg(a)` | Pass an extra flag to `podman run` (escape hatch). |
| `pull(policy)` | When `build` pulls the image: `Missing` (default), `Always` or `Never`. |
| `ready_tcp(port)` | Block `build` until `port` accepts a connection. |
| `ready_timeout(timeout)` | How long `build` waits for readiness (default 30 s). |

## Containers in an existing device

`Device::add_container` starts a container in a device that exists already,
instead of in a new device. The container shares the device's network
namespace with the device's own code and with the other containers in the
device. They reach each other on `127.0.0.1`, and they share the device's lab
addresses:

```rust,ignore
let node = lab.add_device("node").uplink(net.id()).build().await?;

// Redis and the code under test run side by side in one device.
let redis = node
    .add_container("redis", "docker.io/library/redis:7-alpine")
    .ready_tcp(6379)
    .build()
    .await?;
node.spawn(|_| async move {
    let client = connect("redis://127.0.0.1:6379").await?;
    // ...
    anyhow::Ok(())
})?;
```

The device is wired already, so the wiring methods make `build` fail. The
first argument is the container's name for `Lab::container_by_name`, unique
among the lab's containers. A container from `Lab::add_container` has its
device's name. The sysctls for low ports and ping, see
[Differences from a device](#differences-from-a-device), apply to the
device's own processes too.

`ready_tcp` connects to the device's lab IP, so a service that listens only
on `127.0.0.1` never passes it. The build fails if the port accepts
connections before the container starts, for example from another container
in the device.

## Volumes and config

Hand a service its config or data with a bind mount. A relative host path is
relative to the current directory:

```rust,ignore
let pebble = lab
    .add_container("pebble", "ghcr.io/letsencrypt/pebble:latest")
    .uplink(net.id())
    .volume_ro(config_dir.join("pebble.json"), "/test/config/pebble.json")
    .args(["-config", "/test/config/pebble.json"])
    .ready_tcp(14000)
    .build()
    .await?;
```

On SELinux hosts a bind mount may need relabeling or no label. The builder
cannot add a `:z` suffix to a volume, but you can turn labeling off with
`run_arg("--security-opt=label=disable")`.

The returned [`Container`] has `ip()`, `ip6()` and `id()` for its lab address
and node, and `device()` for the underlying [`Device`]. It also has:

- `command(program)` and `exec(cmd)` run a command inside the container, see
  [Commands in a container](#commands-in-a-container).
- `logs()` fetches the container's logs.
- `write_file(path, bytes)` and `read_file(path)` write or read a file inside
  the container without a bind mount.
- `copy_to(host, container)` and `copy_from(container, host)` copy files or
  directories in or out.
- `stop()` removes the container early, and the device that
  `Lab::add_container` created for it.

Otherwise the lab keeps the container, like a device, until
`Lab::remove_device` removes its device or the last `Lab` handle drops.
Dropping a `Container` handle does not stop it, and
`Lab::container_by_name` finds the container until then.

patchbay also removes the containers of a process that dies without cleanup,
for example on `SIGKILL` or Ctrl-C. The first container build starts a small
watcher process, which removes them at once. It has a process group of its
own, so a Ctrl-C or a kill of the process group does not end it. If the
watcher dies too, for example when systemd stops the unit, the containers
keep running. `podman ps --filter label=patchbay.owner` lists them; `kill`
their processes, then `podman rm` them.

The podman in your shell lists these containers and can remove stopped ones.
It cannot `exec` into them, copy files, or stop one that runs as a user other
than root, because they run in patchbay's user namespace.

## Commands in a container

`Container::command` returns a `ContainerCommand`, a builder that mirrors
`std::process::Command`. The command runs with `podman exec`: in the
container's file system, as the container's user, and in the device's network
namespace. So it can use tools that exist only in the image:

```rust,ignore
let output = pebble
    .command("sh")
    .args(["-c", "ls /test/config && echo $MODE"])
    .env("MODE", "check")
    .workdir("/test")
    .output()
    .await?;
assert!(output.status.success());
```

`env`, `workdir` and `user` map to the `--env`, `--workdir` and `--user` flags
of `podman exec`. `user` takes a user name from the image or a uid, optionally
with `:group`, and `"0"` is root in the container. Without it, the command runs
as the container's user. A `stdin` value passes `--interactive`. `exec(cmd)` is
a short form of `command` with `output`.

`spawn` returns a `ContainerChild` for async code, and `spawn_sync` returns a
`ContainerChildSync` for blocking code. Both have `stdin`, `stdout` and
`stderr` fields, and `wait`, `try_wait` and `kill` methods:

```rust,ignore
let mut server = container
    .command("iperf3")
    .arg("-s")
    .stdout(Stdio::piped())
    .spawn()?;
// ...
server.kill().await?;
```

`kill` also kills the command's child processes, and after the command exited,
the ones it left running in the background. Dropping a handle does the same in
the background. The kill finds such left processes by an environment variable,
so one that clears or overwrites its environment escapes it. It runs until
patchbay removes the container. If podman does not exit within 5 seconds of
the kill, `kill` returns an error.

## From a TOML lab or sim

A device table with a `container` sub-table starts the device as a container.
This works in a lab file for `Lab::load` and in a sim file:

```toml
[[router]]
name = "dc"

[device.web.container]
image     = "docker.io/library/nginx:alpine"
env       = { NGINX_ENTRYPOINT_QUIET_LOGS = "1" }
ready_tcp = 80

[device.web.eth0]
gateway = "dc"
```

`Lab::container_by_name("web")` returns a handle for `exec` and the other
container methods. The [TOML reference](../reference/toml-reference.md) lists
all keys.

In a sim, a `run` or `spawn` step with `container = true` runs its command
inside the container of the step's device, instead of on the host file
system. The step's `env` and the sim variables reach the command, and
captures, `ready_after` and the logs work as for other steps. With
`root = true`, the command runs as uid 0 in the container. A
`${binary.<name>}` value is a host path, which the container cannot see.

```toml
[[step]]
action = "run"
id = "version"
device = "web"
container = true
cmd = ["nginx", "-v"]
[step.captures.version]
pipe = "stderr"
regex = "nginx/([0-9.]+)"
```

## Differences from a device

- The container sees the device's `/etc/resolv.conf` and `/etc/hosts`,
  read-only, so the lab DNS server and entries added with `Device::set_host`
  reach it, also after it starts. A volume at one of these paths replaces the
  device's file. With `run_arg("--privileged")`, container root can remount
  them read-write and change DNS for the whole lab, as device code can.
- Root in the container has no capabilities in the lab's network
  namespaces, so a container cannot change routes or firewall rules. patchbay
  sets per-namespace sysctls so that it can still bind ports below 1024 and
  ping.
- `container.device().run_sync(..)`, `spawn` and the other device methods
  run code in the device's network namespace on the host file system, not in
  the container. That is why `Container` does not dereference to `Device`.
  Use `command` or `exec` to run a command in the container.

## Runtime: podman, not docker

Containers run with **podman**, which must be on `PATH`. podman has no
daemon. patchbay starts `podman run` inside the device's network namespace,
and `--network=host` makes the container join that namespace, so it gets the
device's lab IP.

Docker cannot do this rootlessly. Its client talks to `dockerd`, which runs in
its own namespaces, so a docker container joins the daemon's network
namespace, not the device's.

## Requirements

Running a container needs the standard rootless-podman setup:

- `podman`, `newuidmap`, and `newgidmap` on `PATH`.
- An `/etc/subuid` and `/etc/subgid` entry for your user (for example
  `you:100000:65536`).

Without the range, `build()` fails with an error that says so. Router and
device networking work regardless.

podman uses your normal rootless storage, so images that you pulled with
podman before are available.

patchbay's podman sees the host's mounts as they were at the first container
build in the process. A volume whose host path is under a later mount shows
the directory below the mount, usually an empty one.

If a specific image needs a runtime tweak, pass it through with `run_arg`, for
example `run_arg("--cgroup-manager=cgroupfs")`.

## Pulling images

`build()` pulls the image on the host before the container starts, because the
device namespace it runs in has no route to a registry. By default it pulls
only a missing image. Set `pull(PullPolicy::Always)` to get the newest image
for a tag such as `latest`, or `pull(PullPolicy::Never)` to fail fast on a
missing image.

A pull is not possible from inside a lab namespace, for example from a device
task. Also, a pull in the middle of a test adds its download time to the test.
Pull the images first with `Lab::pull_image`, which does nothing if the image
is present:

```rust,ignore
lab.pull_image("docker.io/library/postgres:16").await?;
```

[Pebble]: https://github.com/letsencrypt/pebble
[`Container`]: https://docs.rs/patchbay/latest/patchbay/struct.Container.html
[`Device`]: https://docs.rs/patchbay/latest/patchbay/struct.Device.html
