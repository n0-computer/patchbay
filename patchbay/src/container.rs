//! Run OCI containers as lab devices.
//!
//! A [`Container`] is a [`Device`] whose workload is a container instead of a
//! Rust closure. It is wired into the lab exactly like a device (its own
//! network namespace, a veth uplink to a router, a lab IP) and the container
//! runs joined to that namespace, so it is reachable from other lab devices at
//! its lab IP. This lets an integration test stand up the auxiliary services it
//! needs, such as an ACME test server, a database, or a DNS server, next to the
//! code under test.
//!
//! # Runtime
//!
//! Containers run with `podman`, which must be on `PATH`. Podman is daemonless:
//! the `podman run` process is forked from inside the device's network
//! namespace, so `--network=host` makes the container join that namespace
//! directly. Docker does not work here because its daemon runs in a different
//! namespace, so a docker container would not land in the device's namespace.
//!
//! # Requirements
//!
//! Running a container inside patchbay's rootless user namespace needs a full
//! subuid/subgid range, so the host must have `newuidmap`/`newgidmap` installed
//! and an `/etc/subuid` and `/etc/subgid` entry for the user (the standard
//! rootless-podman setup). Without them patchbay falls back to a single-uid
//! namespace, in which image layers cannot be unpacked; [`build`] then fails
//! with a storage error. Device and router networking work either way.
//!
//! [`build`]: ContainerBuilder::build
//!
//! # Example
//!
//! ```no_run
//! # use patchbay::Lab;
//! # async fn f(lab: Lab, net_id: patchbay::NodeId) -> anyhow::Result<()> {
//! let pebble = lab
//!     .add_container("pebble", "ghcr.io/letsencrypt/pebble:latest")
//!     .uplink(net_id)
//!     .env("PEBBLE_VA_NOSLEEP", "1")
//!     .ready_tcp(14000)
//!     .build()
//!     .await?;
//!
//! // Reachable from any lab device on `pebble.ip()`.
//! let directory = format!(
//!     "https://{}:14000/dir",
//!     pebble.ip().expect("pebble has an ip")
//! );
//! # let _ = directory;
//! # Ok(())
//! # }
//! ```

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    ops::Deref,
    path::PathBuf,
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use tracing::{debug, warn};

use crate::{device::DeviceBuilder, Device};

/// Default container runtime binary.
const DEFAULT_RUNTIME: &str = "podman";

/// How long [`ContainerBuilder::ready_tcp`] waits for the service to accept a
/// connection before giving up.
const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Readiness gate applied after the container starts.
#[derive(Clone, Debug)]
enum Ready {
    /// No readiness check; `build` returns as soon as the container starts.
    None,
    /// Wait until a TCP connection to this port on the container's IP succeeds.
    Tcp { port: u16, timeout: Duration },
}

/// A mount to add to the container.
#[derive(Clone, Debug)]
enum Mount {
    /// Bind-mount a host path at `container` path, read-only when `read_only`.
    Bind {
        host: PathBuf,
        container: String,
        read_only: bool,
    },
    /// Mount a fresh tmpfs at `container` path.
    Tmpfs { container: String },
}

impl Mount {
    /// Renders this mount as `podman run` arguments.
    fn to_args(&self) -> Vec<String> {
        match self {
            Mount::Bind {
                host,
                container,
                read_only,
            } => {
                let mut spec = format!("{}:{container}", host.display());
                if *read_only {
                    spec.push_str(":ro");
                }
                vec!["--volume".to_string(), spec]
            }
            Mount::Tmpfs { container } => vec!["--tmpfs".to_string(), container.clone()],
        }
    }
}

/// Builder for a [`Container`]; returned by [`Lab::add_container`](crate::Lab::add_container).
///
/// The uplink and interface methods mirror [`Lab::add_device`](crate::Lab::add_device):
/// a container is a device, so it is wired into the topology the same way.
pub struct ContainerBuilder {
    device: DeviceBuilder,
    image: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    mounts: Vec<Mount>,
    run_args: Vec<String>,
    runtime: String,
    ready: Ready,
}

impl ContainerBuilder {
    /// Wraps a device builder with container configuration.
    pub(crate) fn new(device: DeviceBuilder, image: impl Into<String>) -> Self {
        Self {
            device,
            image: image.into(),
            args: Vec::new(),
            env: Vec::new(),
            mounts: Vec::new(),
            run_args: Vec::new(),
            runtime: DEFAULT_RUNTIME.to_string(),
            ready: Ready::None,
        }
    }

    /// Adds an auto-named interface (eth0, eth1, ...) with the given config.
    ///
    /// Accepts anything that converts to [`IfaceConfig`](crate::IfaceConfig),
    /// including a bare [`NodeId`](crate::NodeId) for a simple routed uplink.
    pub fn uplink(mut self, config: impl Into<crate::IfaceConfig>) -> Self {
        self.device = self.device.uplink(config);
        self
    }

    /// Adds a named interface with the given configuration.
    pub fn iface(mut self, ifname: &str, config: impl Into<crate::IfaceConfig>) -> Self {
        self.device = self.device.iface(ifname, config);
        self
    }

    /// Overrides which interface carries the default route.
    pub fn default_via(mut self, ifname: &str) -> Self {
        self.device = self.device.default_via(ifname);
        self
    }

    /// Sets the MTU on all interfaces of the container device.
    pub fn mtu(mut self, mtu: u32) -> Self {
        self.device = self.device.mtu(mtu);
        self
    }

    /// Sets an environment variable inside the container.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Appends an argument to the container's command (after the image name).
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Appends several arguments to the container's command.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Bind-mounts a host path into the container, read-write.
    ///
    /// Use this to hand the service its data or config, for example a Pebble
    /// config file or a seeded database directory. The host path should be
    /// absolute. For read-only, use [`volume_ro`](Self::volume_ro).
    pub fn volume(mut self, host: impl Into<PathBuf>, container: impl Into<String>) -> Self {
        self.mounts.push(Mount::Bind {
            host: host.into(),
            container: container.into(),
            read_only: false,
        });
        self
    }

    /// Bind-mounts a host path into the container, read-only.
    pub fn volume_ro(mut self, host: impl Into<PathBuf>, container: impl Into<String>) -> Self {
        self.mounts.push(Mount::Bind {
            host: host.into(),
            container: container.into(),
            read_only: true,
        });
        self
    }

    /// Mounts a fresh tmpfs at the given path inside the container.
    ///
    /// Useful for a writable scratch directory on an otherwise read-only image.
    pub fn tmpfs(mut self, container: impl Into<String>) -> Self {
        self.mounts.push(Mount::Tmpfs {
            container: container.into(),
        });
        self
    }

    /// Passes an extra flag to `podman run` (before the image name).
    ///
    /// An escape hatch for flags the builder does not model, such as
    /// `--cgroup-manager=cgroupfs` or a `:z` SELinux volume label.
    pub fn run_arg(mut self, arg: impl Into<String>) -> Self {
        self.run_args.push(arg.into());
        self
    }

    /// Overrides the container runtime binary (default `podman`).
    ///
    /// Only daemonless runtimes that inherit the caller's network namespace
    /// work; see the [module docs](self).
    pub fn runtime(mut self, runtime: impl Into<String>) -> Self {
        self.runtime = runtime.into();
        self
    }

    /// Waits, after start, until a TCP connection to `port` succeeds.
    ///
    /// The check connects to the container's own lab IP from inside its
    /// namespace, so it reflects what other lab devices will see. Uses a
    /// 30-second timeout; see [`ready_tcp_timeout`](Self::ready_tcp_timeout) to
    /// change it.
    pub fn ready_tcp(mut self, port: u16) -> Self {
        self.ready = Ready::Tcp {
            port,
            timeout: DEFAULT_READY_TIMEOUT,
        };
        self
    }

    /// Like [`ready_tcp`](Self::ready_tcp) with an explicit timeout.
    pub fn ready_tcp_timeout(mut self, port: u16, timeout: Duration) -> Self {
        self.ready = Ready::Tcp { port, timeout };
        self
    }

    /// Wires the container device, starts the container, and waits for
    /// readiness.
    ///
    /// # Errors
    ///
    /// Returns an error if the device fails to wire, if the runtime binary is
    /// missing or `run` fails (the error includes the runtime's stderr), or if
    /// a [`ready_tcp`](Self::ready_tcp) gate times out.
    pub async fn build(self) -> Result<Container> {
        let name = container_name(&self.device);

        // Pull on the host before wiring the (registry-unreachable) device
        // namespace.
        ensure_image(&self.runtime, &self.image).await?;
        let device = self.device.build().await?;

        let run_args = build_run_args(
            &name,
            &self.image,
            &self.env,
            &self.mounts,
            &self.args,
            &self.run_args,
        );
        debug!(container = %name, runtime = %self.runtime, image = %self.image, "starting container");

        // Fork the runtime from inside the device namespace so the container,
        // run with `--network=host`, joins it.
        let runtime = self.runtime.clone();
        let spawn_args = run_args.clone();
        let output = device
            .run_sync(move || {
                runtime_command(&runtime)
                    .args(&spawn_args)
                    .output()
                    .with_context(|| format!("spawn `{runtime} run` (is it installed?)"))
            })
            .with_context(|| format!("start container '{name}'"))?;
        if !output.status.success() {
            bail!(
                "`{} run` for container '{}' failed: {}",
                self.runtime,
                name,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let container = Container {
            device,
            inner: Arc::new(ContainerInner {
                name,
                runtime: self.runtime,
            }),
        };

        container.wait_ready(&self.ready).await?;
        Ok(container)
    }
}

/// Builds a lab-unique podman container name from the device's namespace.
///
/// The namespace name (e.g. `lab3-d7`) is unique within a process, and the lab
/// prefix carries a per-process tag, so the resulting name does not collide
/// across parallel test processes on the same host.
fn container_name(device: &DeviceBuilder) -> String {
    let inner = device.inner.core.lock().expect("poisoned");
    let prefix = inner.cfg.prefix.clone();
    let dev = inner
        .device(device.id)
        .map(|d| d.ns.to_string())
        .unwrap_or_else(|| format!("dev{}", device.id));
    sanitize_name(&format!("{prefix}-{dev}"))
}

/// Replaces characters podman does not accept in a container name with `-`.
fn sanitize_name(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Builds the argument vector for `<runtime> run`.
///
/// Detached (`-d`), joined to the caller's network namespace (`--network=host`),
/// and replacing any leftover container of the same name (`--replace`) so a
/// re-run after a crash does not collide.
fn build_run_args(
    name: &str,
    image: &str,
    env: &[(String, String)],
    mounts: &[Mount],
    cmd_args: &[String],
    extra_run_args: &[String],
) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--replace".to_string(),
        "--network=host".to_string(),
        // The image is pre-pulled on the host; the device namespace has no
        // route to a registry.
        "--pull=never".to_string(),
        "--name".to_string(),
        name.to_string(),
    ];
    for (k, v) in env {
        args.push("--env".to_string());
        args.push(format!("{k}={v}"));
    }
    for mount in mounts {
        args.extend(mount.to_args());
    }
    args.extend(extra_run_args.iter().cloned());
    args.push(image.to_string());
    args.extend(cmd_args.iter().cloned());
    args
}

/// Builds a runtime command carrying the rootless environment podman needs
/// inside patchbay's user namespace.
///
/// patchbay maps the invoking user to uid 0, so podman would otherwise treat
/// itself as rootful and use unwritable system paths. These variables tell it
/// it is the already-configured rootless namespace, so it uses the user's
/// rootless storage and reuses already-pulled images.
///
/// These variables are the only way to get this behavior: podman keys "am I
/// rootless" off euid, there is no `containers.conf` override, and
/// `--userns=ns:<path>` is broken for rootless. Making it a supported flag is
/// an open request (containers/podman#7774). They are internal, but stable and
/// exactly what podman sets on its own rootless re-exec.
fn runtime_command(runtime: &str) -> Command {
    let (uid, gid) = host_ids();
    let mut cmd = Command::new(runtime);
    cmd.env("_CONTAINERS_USERNS_CONFIGURED", "done")
        .env("_CONTAINERS_ROOTLESS_UID", uid.to_string())
        .env("_CONTAINERS_ROOTLESS_GID", gid.to_string());
    cmd
}

/// Returns the outer (host) uid and gid that the current user namespace maps
/// inner id 0 to. These key the user's rootless container storage.
fn host_ids() -> (u32, u32) {
    (
        outer_id("/proc/self/uid_map"),
        outer_id("/proc/self/gid_map"),
    )
}

/// Parses the outer id that a `/proc/self/{uid,gid}_map` maps inner id 0 to.
fn outer_id(path: &str) -> u32 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| {
            contents.lines().find_map(|line| {
                let mut fields = line.split_whitespace();
                if fields.next()? != "0" {
                    return None;
                }
                fields.next()?.parse::<u32>().ok()
            })
        })
        .unwrap_or(0)
}

/// Runs a podman subcommand and returns its output.
///
/// Executes on a dedicated thread with its own mount namespace, in the host
/// network namespace. podman's storage setup makes its mounts private, which
/// needs a mount namespace owned by patchbay's user namespace; the plain
/// blocking-pool threads share the host mount namespace and cannot. The
/// container `run` does not use this path: it is forked from the device's
/// namespace worker, which already has a private mount namespace (and the
/// device network namespace).
async fn podman(runtime: &str, args: Vec<String>) -> Result<std::process::Output> {
    let runtime = runtime.to_string();
    tokio::task::spawn_blocking(move || podman_blocking(&runtime, &args))
        .await
        .context("container runtime task panicked")?
}

/// Blocking core of [`podman`], usable from `Drop`.
fn podman_blocking(runtime: &str, args: &[String]) -> Result<std::process::Output> {
    let runtime = runtime.to_string();
    let args = args.to_vec();
    std::thread::spawn(move || -> std::io::Result<std::process::Output> {
        // Private mount namespace so podman can make its storage mounts private.
        let _ = nix::sched::unshare(nix::sched::CloneFlags::CLONE_NEWNS);
        runtime_command(&runtime).args(&args).output()
    })
    .join()
    .map_err(|_| anyhow::anyhow!("container runtime thread panicked"))?
    .context("spawn container runtime")
}

/// Ensures the image is present locally, pulling it on the host (which has
/// registry access) if needed.
///
/// The container itself runs in a device namespace with no route to a registry,
/// so the image must be present before that run.
async fn ensure_image(runtime: &str, image: &str) -> Result<()> {
    if image_present(runtime, image).await {
        return Ok(());
    }
    let output = podman(runtime, vec!["pull".into(), image.into()]).await?;
    if !output.status.success() {
        bail!(
            "pulling image '{image}' failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Returns whether the image already exists in local storage.
async fn image_present(runtime: &str, image: &str) -> bool {
    podman(runtime, vec!["image".into(), "exists".into(), image.into()])
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Shared interior of a [`Container`], removed when the last handle drops.
struct ContainerInner {
    name: String,
    runtime: String,
}

impl Drop for ContainerInner {
    fn drop(&mut self) {
        // Best-effort removal. Runs from within the patchbay process (and its
        // user namespace), which is the same podman user context that created
        // the container, so removal by name works regardless of the device
        // namespace still being alive.
        let args = ["rm", "-f", "-t", "1", &self.name].map(String::from);
        match podman_blocking(&self.runtime, &args) {
            Ok(out) if out.status.success() => {}
            Ok(out) => warn!(
                container = %self.name,
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "container removal reported an error"
            ),
            Err(err) => warn!(container = %self.name, %err, "could not run container removal"),
        }
    }
}

/// A container running as a lab device.
///
/// Cloneable handle. Dereferences to the underlying [`Device`], so device
/// accessors ([`ip`](Device::ip), [`spawn`](Device::spawn), ...) work directly.
/// The container is removed when the last handle is dropped, or explicitly via
/// [`stop`](Self::stop).
#[derive(Clone)]
pub struct Container {
    device: Device,
    inner: Arc<ContainerInner>,
}

impl std::fmt::Debug for Container {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Container")
            .field("name", &self.inner.name)
            .field("ip", &self.device.ip())
            .finish()
    }
}

impl Deref for Container {
    type Target = Device;
    fn deref(&self) -> &Device {
        &self.device
    }
}

impl Container {
    /// Returns the underlying [`Device`] handle.
    pub fn device(&self) -> &Device {
        &self.device
    }

    /// Returns the runtime container name (lab-unique).
    pub fn container_name(&self) -> &str {
        &self.inner.name
    }

    /// Runs a command inside the running container and returns its output.
    ///
    /// # Errors
    ///
    /// Returns an error if the runtime cannot be spawned. A non-zero exit from
    /// the command itself is reported in the returned [`std::process::Output`],
    /// not as an error.
    pub async fn exec<I, S>(&self, cmd: I) -> Result<std::process::Output>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut args = vec!["exec".to_string(), self.inner.name.clone()];
        args.extend(cmd.into_iter().map(Into::into));
        podman(&self.inner.runtime, args).await
    }

    /// Returns the container's logs (stdout and stderr, combined).
    pub async fn logs(&self) -> Result<String> {
        let output = podman(
            &self.inner.runtime,
            vec!["logs".into(), self.inner.name.clone()],
        )
        .await?;
        let mut logs = String::from_utf8_lossy(&output.stdout).into_owned();
        logs.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok(logs)
    }

    /// Stops and removes the container.
    ///
    /// Called automatically when the last handle drops; use this to tear it
    /// down early or to surface removal errors.
    pub async fn stop(&self) -> Result<()> {
        let args = vec![
            "rm".into(),
            "-f".into(),
            "-t".into(),
            "1".into(),
            self.inner.name.clone(),
        ];
        let output = podman(&self.inner.runtime, args).await?;
        if !output.status.success() {
            bail!(
                "removing container '{}' failed: {}",
                self.inner.name,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    /// Polls the readiness gate until it passes or times out.
    async fn wait_ready(&self, ready: &Ready) -> Result<()> {
        let Ready::Tcp { port, timeout } = ready else {
            return Ok(());
        };
        let ip: IpAddr = self
            .device
            .ip()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let target = SocketAddr::new(ip, *port);
        let deadline = Instant::now() + *timeout;
        loop {
            // Connect from inside the container's namespace, so this measures
            // what other lab devices will see.
            let connected = self.device.run_sync(move || {
                Ok(
                    std::net::TcpStream::connect_timeout(&target, Duration::from_millis(500))
                        .is_ok(),
                )
            })?;
            if connected {
                debug!(container = %self.inner.name, %target, "container ready");
                return Ok(());
            }
            if Instant::now() >= deadline {
                let logs = self.logs().await.unwrap_or_default();
                bail!(
                    "container '{}' did not accept a connection on {} within {:?}\n--- logs ---\n{}",
                    self.inner.name,
                    target,
                    timeout,
                    logs.trim()
                );
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_args_have_the_required_flags() {
        let args = build_run_args(
            "lab-p1-c0",
            "docker.io/library/alpine",
            &[("A".into(), "1".into()), ("B".into(), "2".into())],
            &[],
            &["sleep".into(), "infinity".into()],
            &["--cgroup-manager=cgroupfs".into()],
        );
        // Detached, name, host network, replace.
        assert_eq!(&args[0], "run");
        assert!(args.contains(&"-d".to_string()));
        assert!(args.contains(&"--network=host".to_string()));
        assert!(args.contains(&"--replace".to_string()));
        let name_idx = args.iter().position(|a| a == "--name").unwrap();
        assert_eq!(args[name_idx + 1], "lab-p1-c0");
        // Env pairs.
        assert!(args.windows(2).any(|w| w[0] == "--env" && w[1] == "A=1"));
        assert!(args.windows(2).any(|w| w[0] == "--env" && w[1] == "B=2"));
        // Extra run arg comes before the image.
        let image_idx = args
            .iter()
            .position(|a| a == "docker.io/library/alpine")
            .unwrap();
        let cgroup_idx = args
            .iter()
            .position(|a| a == "--cgroup-manager=cgroupfs")
            .unwrap();
        assert!(cgroup_idx < image_idx);
        // Command args come after the image, in order.
        assert_eq!(&args[image_idx + 1], "sleep");
        assert_eq!(&args[image_idx + 2], "infinity");
    }

    #[test]
    fn mounts_render_before_the_image() {
        let mounts = vec![
            Mount::Bind {
                host: "/srv/data".into(),
                container: "/data".into(),
                read_only: false,
            },
            Mount::Bind {
                host: "/etc/conf".into(),
                container: "/conf".into(),
                read_only: true,
            },
            Mount::Tmpfs {
                container: "/scratch".into(),
            },
        ];
        let args = build_run_args("c", "img", &[], &mounts, &[], &[]);
        let image_idx = args.iter().position(|a| a == "img").unwrap();
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--volume" && w[1] == "/srv/data:/data"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--volume" && w[1] == "/etc/conf:/conf:ro"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--tmpfs" && w[1] == "/scratch"));
        // All mount args precede the image.
        let last_mount = args
            .iter()
            .rposition(|a| a == "--volume" || a == "--tmpfs")
            .unwrap();
        assert!(last_mount < image_idx);
    }

    #[test]
    fn sanitize_name_replaces_invalid_chars() {
        assert_eq!(sanitize_name("lab-p1/c0"), "lab-p1-c0");
        assert_eq!(sanitize_name("ok_name.1-2"), "ok_name.1-2");
        assert_eq!(sanitize_name("a b:c"), "a-b-c");
    }
}
