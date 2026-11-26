//! Run OCI containers as lab devices, see [`Container`].
//!
//! podman runs in a user namespace nested in patchbay's, see [`PodmanUserns`].

use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    os::{
        fd::{AsRawFd, BorrowedFd, OwnedFd},
        unix::process::CommandExt,
    },
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use rustix::{
    io::{read, write},
    pipe::{pipe_with, PipeFlags},
    process::{getegid, geteuid, waitpid, Pid, WaitOptions},
    thread::{move_into_link_name_space, LinkNameSpaceType, UnshareFlags},
};
use tokio::io::{AsyncRead, AsyncReadExt};
use tracing::{debug, warn};

use crate::{
    device::DeviceBuilder,
    userns::{read_id_map, IdMapLine},
    Device,
};

/// The container runtime binary.
pub(crate) const PODMAN: &str = "podman";

/// How long [`ContainerBuilder::build`] waits for readiness by default.
const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Container label that records the patchbay process that started it, see [`WATCHER_SCRIPT`].
pub(crate) const OWNER_LABEL: &str = "patchbay.owner";

/// When [`ContainerBuilder::build`] pulls the image.
///
/// A pull needs the host network, so it fails from inside a lab namespace,
/// for example from a device task. Pull such images first with
/// [`Lab::pull_image`](crate::Lab::pull_image).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum PullPolicy {
    /// Pulls the image only if it is missing. This is the default.
    #[default]
    Missing,
    /// Always pulls, to get the newest image for a tag such as `latest`.
    Always,
    /// Never pulls. `build` fails if the image is missing.
    Never,
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
    fn to_args(&self) -> [String; 2] {
        match self {
            Mount::Bind {
                host,
                container,
                read_only,
            } => {
                // `--mount` reads its value as one CSV record, so quoted
                // fields may hold `,` and `:`. `--volume` has no quoting.
                let quote = |field: String| format!("\"{}\"", field.replace('"', "\"\""));
                let mut spec = format!(
                    "type=bind,{},{}",
                    quote(format!("src={}", host.display())),
                    quote(format!("dst={container}"))
                );
                if *read_only {
                    spec.push_str(",ro");
                }
                ["--mount".to_string(), spec]
            }
            Mount::Tmpfs { container } => ["--tmpfs".to_string(), container.clone()],
        }
    }
}

/// Builder for a [`Container`].
///
/// From [`Lab::add_container`](crate::Lab::add_container), it wires a new
/// device like a [`DeviceBuilder`]. From [`Device::add_container`], it joins a
/// wired device, and a wiring method makes [`build`](Self::build) fail.
#[must_use = "a ContainerBuilder does nothing until you call build"]
pub struct ContainerBuilder {
    target: Target,
    /// Name of the container in the lab.
    name: String,
    /// The first error of a builder method, returned by `build`.
    result: Result<()>,
    spec: RunSpec,
    pull: PullPolicy,
    ready_tcp: Option<u16>,
    ready_timeout: Duration,
    /// Set for a new device.
    guard: Option<DeviceGuard>,
}

/// The device whose network namespace a container joins.
enum Target {
    /// A new device for the container, wired by the builder.
    NewDevice(DeviceBuilder),
    /// A device that exists already.
    Existing(Device),
}

/// What `podman run` starts: the image and its settings.
#[derive(Clone, Debug, Default)]
struct RunSpec {
    image: String,
    /// Replaces the image's entrypoint if set. An empty list clears it.
    entrypoint: Option<Vec<String>>,
    args: Vec<String>,
    env: Vec<(String, String)>,
    mounts: Vec<Mount>,
    run_args: Vec<String>,
}

impl ContainerBuilder {
    /// Returns a builder for a container in a new device named `name`.
    pub(crate) fn new(name: &str, device: DeviceBuilder, image: impl Into<String>) -> Self {
        let guard = DeviceGuard {
            lab: Arc::clone(&device.inner),
            id: Some(device.id),
            wired: false,
        };
        Self {
            guard: Some(guard),
            ..Self::with_target(name, Target::NewDevice(device), image)
        }
    }

    /// Returns a builder for a container that joins an existing device.
    pub(crate) fn attach(name: &str, device: Device, image: impl Into<String>) -> Self {
        Self::with_target(name, Target::Existing(device), image)
    }

    fn with_target(name: &str, target: Target, image: impl Into<String>) -> Self {
        Self {
            target,
            name: name.to_string(),
            result: Ok(()),
            spec: RunSpec {
                image: image.into(),
                ..RunSpec::default()
            },
            pull: PullPolicy::default(),
            ready_tcp: None,
            ready_timeout: DEFAULT_READY_TIMEOUT,
            guard: None,
        }
    }

    /// Adds an interface, like [`DeviceBuilder::uplink`].
    pub fn uplink(self, config: impl Into<crate::IfaceConfig>) -> Self {
        self.wire("uplink", |device| device.uplink(config))
    }

    /// Adds a named interface, like [`DeviceBuilder::iface`].
    pub fn iface(self, ifname: &str, config: impl Into<crate::IfaceConfig>) -> Self {
        self.wire("iface", |device| device.iface(ifname, config))
    }

    /// Sets the default route interface, like [`DeviceBuilder::default_via`].
    pub fn default_via(self, ifname: &str) -> Self {
        self.wire("default_via", |device| device.default_via(ifname))
    }

    /// Sets the MTU of all interfaces, like [`DeviceBuilder::mtu`].
    pub fn mtu(self, mtu: u32) -> Self {
        self.wire("mtu", |device| device.mtu(mtu))
    }

    /// Sets the IPv6 provisioning mode, like [`DeviceBuilder::ipv6_provisioning_mode`].
    pub fn ipv6_provisioning_mode(self, mode: crate::Ipv6ProvisioningMode) -> Self {
        self.wire("ipv6_provisioning_mode", |device| {
            device.ipv6_provisioning_mode(mode)
        })
    }

    /// Applies a wiring method, or keeps an error for an existing device.
    fn wire(mut self, method: &str, wire: impl FnOnce(DeviceBuilder) -> DeviceBuilder) -> Self {
        match self.target {
            Target::NewDevice(device) => self.target = Target::NewDevice(wire(device)),
            Target::Existing(ref device) => {
                let (name, device) = (&self.name, device.name());
                let err = anyhow!(
                    "`{method}` wires a new device, but container '{name}' joins the \
                     existing device '{device}'"
                );
                return self.fail(err);
            }
        }
        self
    }

    /// Keeps the first error of a builder method for `build`.
    fn fail(mut self, err: anyhow::Error) -> Self {
        if self.result.is_ok() {
            self.result = Err(err);
        }
        self
    }

    /// Sets an environment variable inside the container.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.spec.env.push((key.into(), value.into()));
        self
    }

    /// Appends an argument to the container's command (after the image name).
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.spec.args.push(arg.into());
        self
    }

    /// Appends several arguments to the container's command.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.spec.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Replaces the image's entrypoint.
    ///
    /// The container runs the entrypoint followed by the [`args`](Self::args).
    /// An empty list clears the entrypoint, so the args alone are the command.
    pub fn entrypoint<I, S>(mut self, entrypoint: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.spec.entrypoint = Some(entrypoint.into_iter().map(Into::into).collect());
        self
    }

    /// Bind-mounts a host path into the container, read-write.
    ///
    /// A relative host path is relative to the current directory.
    pub fn volume(self, host: impl Into<PathBuf>, container: impl Into<String>) -> Self {
        self.bind(host.into(), container.into(), false)
    }

    /// Bind-mounts a host path read-only, like [`volume`](Self::volume).
    pub fn volume_ro(self, host: impl Into<PathBuf>, container: impl Into<String>) -> Self {
        self.bind(host.into(), container.into(), true)
    }

    /// Adds a bind mount with an absolute host path.
    ///
    /// podman reads a relative source without a leading `.` as a volume name.
    fn bind(mut self, host: PathBuf, container: String, read_only: bool) -> Self {
        match std::path::absolute(&host) {
            Ok(host) => self.spec.mounts.push(Mount::Bind {
                host,
                container,
                read_only,
            }),
            Err(err) => {
                return self
                    .fail(anyhow!(err).context(format!("resolve volume '{}'", host.display())));
            }
        }
        self
    }

    /// Mounts a fresh tmpfs at a path in the container.
    pub fn tmpfs(mut self, container: impl Into<String>) -> Self {
        self.spec.mounts.push(Mount::Tmpfs {
            container: container.into(),
        });
        self
    }

    /// Passes an extra flag to `podman run`, before the image name.
    ///
    /// For flags the builder does not model, such as
    /// `--security-opt=label=disable` for bind mounts on an SELinux host.
    /// With `--privileged`, container root can write the lab's DNS files.
    pub fn run_arg(mut self, arg: impl Into<String>) -> Self {
        self.spec.run_args.push(arg.into());
        self
    }

    /// Sets when [`build`](Self::build) pulls the image, see [`PullPolicy`].
    pub fn pull(mut self, policy: PullPolicy) -> Self {
        self.pull = policy;
        self
    }

    /// Makes [`build`](Self::build) wait until `port` accepts a TCP connection.
    ///
    /// The check connects to the container's lab IP, IPv4 if it has one, from
    /// inside its namespace. It does not test routes to the container. It
    /// fails early if the container exits, and at once if the port accepts
    /// connections before the start.
    pub fn ready_tcp(mut self, port: u16) -> Self {
        self.ready_tcp = Some(port);
        self
    }

    /// Sets how long [`build`](Self::build) waits for readiness.
    ///
    /// The default is 30 seconds.
    pub fn ready_timeout(mut self, timeout: Duration) -> Self {
        self.ready_timeout = timeout;
        self
    }

    /// Applies a `[device.<name>.container]` table.
    pub(crate) fn config(mut self, config: &crate::config::ContainerConfig) -> Self {
        self = self.args(config.args.iter().cloned()).pull(config.pull);
        if let Some(entrypoint) = &config.entrypoint {
            self = self.entrypoint(entrypoint.iter().cloned());
        }
        for (key, value) in &config.env {
            self = self.env(key, value);
        }
        for spec in &config.volumes {
            self = match parse_volume(spec) {
                Ok((host, container, false)) => self.volume(host, container),
                Ok((host, container, true)) => self.volume_ro(host, container),
                Err(err) => self.fail(err),
            };
        }
        for path in &config.tmpfs {
            self = self.tmpfs(path);
        }
        for arg in &config.run_args {
            self = self.run_arg(arg);
        }
        if let Some(port) = config.ready_tcp {
            self = self.ready_tcp(port);
        }
        if let Some(secs) = config.ready_timeout_secs {
            self = self.ready_timeout(Duration::from_secs(secs));
        }
        self
    }

    /// Starts the container and waits for readiness.
    ///
    /// Wires a new device, then pulls the image as set by [`pull`](Self::pull).
    /// A failed or cancelled build removes the new device.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// - the host lacks container support, see [`Container`].
    /// - the lab has a container of this name.
    /// - a wiring method was called for an existing device.
    /// - the wiring, the pull, or `podman run` fails.
    /// - the [`ready_tcp`](Self::ready_tcp) gate times out, or the container
    ///   exits first.
    /// - the device or the lab goes away during the start.
    pub async fn build(self) -> Result<Container> {
        let Self {
            target,
            name,
            result,
            spec,
            pull,
            ready_tcp,
            ready_timeout,
            mut guard,
        } = self;
        result?;
        ensure_watcher()?;
        let lab = match &target {
            Target::NewDevice(builder) => &builder.inner,
            Target::Existing(device) => device.lab(),
        };
        lab.ensure_container_name_free(&name)?;
        let device = match target {
            Target::Existing(device) => device,
            Target::NewDevice(builder) => builder.build().await?,
        };
        if let Some(guard) = &mut guard {
            guard.wired = true;
        }
        ensure_image(&spec.image, pull).await?;
        let owns_device = guard.is_some();
        let container = run(&device, &name, &spec, ready_tcp, ready_timeout, owns_device).await?;
        if let Some(guard) = &mut guard {
            guard.id = None;
        }
        Ok(container)
    }
}

/// Removes the new device of a container build that fails, is cancelled, or never runs.
struct DeviceGuard {
    lab: Arc<crate::lab::LabInner>,
    /// `None` once the build succeeded.
    id: Option<crate::NodeId>,
    /// Whether [`DeviceBuilder::build`] succeeded.
    wired: bool,
}

impl Drop for DeviceGuard {
    fn drop(&mut self) {
        match self.id {
            Some(id) if self.wired => {
                if let Err(err) = self.lab.remove_device(id) {
                    warn!(err = %format_args!("{err:#}"), "could not remove the device of a failed container");
                }
            }
            Some(id) => self.lab.forget_device(id),
            None => {}
        }
    }
}

/// Parses a `host:container[:ro|:rw]` volume spec.
pub(crate) fn parse_volume(spec: &str) -> Result<(&str, &str, bool)> {
    let parts: Vec<&str> = spec.split(':').collect();
    match parts[..] {
        [host, container] if !host.is_empty() && !container.is_empty() => {
            Ok((host, container, false))
        }
        [host, container, mode @ ("ro" | "rw")] if !host.is_empty() && !container.is_empty() => {
            Ok((host, container, mode == "ro"))
        }
        _ => bail!("invalid volume '{spec}', expected 'host:container[:ro|:rw]'"),
    }
}

/// Starts the container in `device`, waits for readiness, and registers it.
///
/// On error, the container is removed.
async fn run(
    device: &Device,
    lab_name: &str,
    spec: &RunSpec,
    ready_tcp: Option<u16>,
    ready_timeout: Duration,
    owns_device: bool,
) -> Result<Container> {
    // Container root has no capabilities in the device's network namespace.
    // Let it bind low ports and ping anyway.
    let (low, high) = ping_group_range(&read_id_map("/proc/self/gid_map")?)?;
    device.run_sync(move || {
        crate::wiring::set_sysctl_root("net/ipv4/ip_unprivileged_port_start", "0")?;
        crate::wiring::set_sysctl_root("net/ipv4/ping_group_range", &format!("{low} {high}"))
    })?;

    if let Some(port) = ready_tcp {
        let target = ready_target(device, port);
        if accepts(device, target).await? {
            bail!(
                "{target} accepts connections before the container starts, so `ready_tcp` \
                 cannot tell when the container is ready"
            );
        }
    }

    let name = container_name(device, lab_name);
    let run_args = build_run_args(&name, spec, &device_dns_files(device, spec));
    debug!(container = %name, image = %spec.image, "starting container");
    let (inner, output) = start(device, name, owns_device, run_args)?
        .await
        .context("`podman run` thread failed")?;
    // From here on, dropping `container` removes it, also on error.
    let container = Container {
        device: device.clone(),
        inner,
    };
    output.with_context(|| format!("start container '{}'", container.inner.name))?;
    container.wait_ready(ready_tcp, ready_timeout).await?;
    // Fails for a parallel build of the same name, or if the device or the
    // lab went away. Dropping `container` then removes it.
    device
        .lab()
        .register_container(lab_name, device.id(), &container.inner)?;
    Ok(container)
}

/// Builds a host-unique podman container name.
///
/// The owner tag makes the name unique on the host, and the counter in the
/// process. So a new container can reuse a lab name while podman removes the
/// old one. The namespace and the lab name show in `podman ps`.
fn container_name(device: &Device, name: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    let raw = format!("patchbay-{}-{}-c{seq}-{name}", owner_tag(), device.ns());
    sanitize_name(&raw)
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

/// Returns read-only mounts of the device's `/etc/hosts` and `/etc/resolv.conf`.
///
/// podman's mount namespace has no DNS overlay, see [`PodmanUserns`]. Paths
/// that `spec` mounts itself are skipped.
fn device_dns_files(device: &Device, spec: &RunSpec) -> Vec<Mount> {
    let core = device.lab().core.lock().expect("poisoned");
    [
        (core.dns.hosts_path_for(device.id()), "/etc/hosts"),
        (core.dns.resolv_path(), "/etc/resolv.conf"),
    ]
    .into_iter()
    .filter(|(_, target)| {
        !spec.mounts.iter().any(|mount| match mount {
            Mount::Bind { container, .. } | Mount::Tmpfs { container } => container == target,
        })
    })
    .map(|(host, target)| Mount::Bind {
        host,
        container: target.to_string(),
        read_only: true,
    })
    .collect()
}

/// Builds the argument vector for `podman run`.
///
/// `extra_mounts` come before the mounts of `spec`.
fn build_run_args(name: &str, spec: &RunSpec, extra_mounts: &[Mount]) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--network=host".to_string(),
        // `prepare` pulled the image on the host already.
        "--pull=never".to_string(),
        "--name".to_string(),
        name.to_string(),
        // Lets the watcher find the container if this process dies.
        "--label".to_string(),
        format!("{OWNER_LABEL}={}", owner_tag()),
    ];
    for (k, v) in &spec.env {
        args.push("--env".to_string());
        args.push(format!("{k}={v}"));
    }
    for mount in extra_mounts.iter().chain(&spec.mounts) {
        args.extend(mount.to_args());
    }
    if let Some(entrypoint) = &spec.entrypoint {
        // podman reads a JSON array as the full entrypoint command.
        let json = serde_json::to_string(entrypoint).expect("strings serialize");
        args.push(format!("--entrypoint={json}"));
    }
    args.extend(spec.run_args.iter().cloned());
    args.push(spec.image.clone());
    args.extend(spec.args.iter().cloned());
    args
}

/// A user namespace nested in patchbay's, laid out like podman's own rootless one.
///
/// Inner 0 is the host user, and inner 1.. are the subordinate ids in host
/// order, see [`nested_id_map`]. podman runs as its root with the environment
/// of its own rootless re-exec, so it uses the user's normal storage. podman
/// cannot create this namespace itself: its re-exec calls `newuidmap`, whose
/// setuid bit has no effect in patchbay's namespace (containers/podman#7774).
///
/// Container root has no capabilities in the lab's network namespaces, which
/// patchbay's namespace owns, so it cannot change routes or firewall rules.
///
/// All podman processes share one mount namespace, as with podman's pause
/// process: `podman exec` reads the root file system that `podman run`
/// mounted. The namespace is a private copy made at creation, so later host
/// mounts do not reach podman.
///
/// The mount namespace drops the bind mounts of device workers. Their
/// `/proc/net` mount would be locked here, and the kernel refuses the
/// container's `proc` mount under a locked mount. A new podman process sees
/// its own network namespace in `/proc/net` without the mount. The DNS overlay
/// goes too, so [`device_dns_files`] mounts the device's files.
struct PodmanUserns {
    user_ns: OwnedFd,
    /// Owned by `user_ns`, so that podman can make its mounts private.
    mount_ns: OwnedFd,
    /// Host uid that inner 0 maps to.
    host_uid: u32,
    /// Host gid that inner 0 maps to.
    host_gid: u32,
}

impl PodmanUserns {
    /// Returns the namespace, creating it on first use.
    ///
    /// A failed creation is not cached, so that a transient error such as
    /// `EMFILE` can recover.
    fn get() -> Result<&'static Self> {
        static USERNS: OnceLock<PodmanUserns> = OnceLock::new();
        // The forked child keeps every fd open at the fork. Two parallel
        // creations would hold each other's p2c write end, so neither child
        // would see EOF.
        static CREATE: Mutex<()> = Mutex::new(());
        if let Some(userns) = USERNS.get() {
            return Ok(userns);
        }
        let _create = CREATE.lock().expect("poisoned");
        if let Some(userns) = USERNS.get() {
            return Ok(userns);
        }
        let userns = Self::create()?;
        Ok(USERNS.get_or_init(|| userns))
    }

    /// Creates the namespaces in a forked child and opens them.
    ///
    /// Only one creation may run at a time, see [`get`](Self::get).
    fn create() -> Result<Self> {
        let uid_map = read_id_map("/proc/self/uid_map")?;
        let gid_map = read_id_map("/proc/self/gid_map")?;
        let (uid_lines, host_uid) = nested_id_map(&uid_map, geteuid().as_raw())?;
        let (gid_lines, host_gid) = nested_id_map(&gid_map, getegid().as_raw())?;
        let proc_net = std::ffi::CString::new(format!("/proc/{}/net", std::process::id()))
            .expect("path has no NUL byte");

        // c2p reports the unshare, and p2c releases the child once the parent
        // opened the namespaces. CLOEXEC keeps the write ends out of processes
        // that other threads spawn.
        let (c2p_r, c2p_w) = pipe_with(PipeFlags::CLOEXEC).context("pipe")?;
        let (p2c_r, p2c_w) = pipe_with(PipeFlags::CLOEXEC).context("pipe")?;

        // SAFETY: the child only makes system calls (unshare, umount2, mount,
        // write, read, _exit) and does not allocate, so forking a threaded
        // process is sound.
        let child = unsafe { libc::fork() };
        if child < 0 {
            return Err(std::io::Error::last_os_error()).context("fork");
        }
        if child == 0 {
            drop(c2p_r);
            drop(p2c_w);
            // Detach a device worker's bind mounts in a copy that patchbay's
            // namespace owns, before the nested one locks them.
            let ok = crate::netns::unshare_namespaces(UnshareFlags::NEWNS).is_ok()
                && {
                    for path in [proc_net.as_c_str(), c"/etc/hosts", c"/etc/resolv.conf"] {
                        detach_mounts(path);
                    }
                    crate::netns::unshare_namespaces(UnshareFlags::NEWUSER | UnshareFlags::NEWNS)
                        .is_ok()
                }
                && {
                    // The copy is private from a device worker and a slave of the
                    // host otherwise. Make it private in both cases.
                    // SAFETY: all pointers are valid C strings or null.
                    unsafe {
                        libc::mount(
                            std::ptr::null(),
                            c"/".as_ptr(),
                            std::ptr::null(),
                            libc::MS_REC | libc::MS_PRIVATE,
                            std::ptr::null(),
                        ) == 0
                    }
                };
            let _ = write(&c2p_w, &[u8::from(ok)]);
            let _ = read(&p2c_r, &mut [0u8; 1]);
            // SAFETY: exit without running atexit handlers in the forked child.
            unsafe { libc::_exit(0) };
        }

        drop(c2p_w);
        drop(p2c_r);
        let fds = (|| -> Result<(OwnedFd, OwnedFd)> {
            let mut status = [0u8; 1];
            let got = read(&c2p_r, &mut status).context("await child")?;
            if got != 1 || status[0] != 1 {
                bail!("creating the podman namespaces failed in the forked child");
            }
            std::fs::write(format!("/proc/{child}/uid_map"), uid_lines)
                .context("write nested uid_map")?;
            std::fs::write(format!("/proc/{child}/gid_map"), gid_lines)
                .context("write nested gid_map")?;
            let user_ns = std::fs::File::open(format!("/proc/{child}/ns/user"))
                .context("open nested user namespace")?;
            let mount_ns = std::fs::File::open(format!("/proc/{child}/ns/mnt"))
                .context("open nested mount namespace")?;
            Ok((user_ns.into(), mount_ns.into()))
        })();
        drop(p2c_w);
        let _ = waitpid(Pid::from_raw(child), WaitOptions::empty());
        let (user_ns, mount_ns) = fds.context("create the user namespace for podman")?;
        debug!(host_uid, host_gid, "created podman user namespace");
        Ok(Self {
            user_ns,
            mount_ns,
            host_uid,
            host_gid,
        })
    }
}

/// Builds the map for [`PodmanUserns`] from patchbay's own map.
///
/// Maps inner 0 to `own`, patchbay's id, and inner 1.. to the other mapped ids
/// in host order. With patchbay's layout, inner N is then subordinate id N - 1,
/// as in plain rootless podman. Returns the map and the host id of `own`.
fn nested_id_map(map: &[IdMapLine], own: u32) -> Result<(String, u32)> {
    let own_line = map
        .iter()
        .find(|line| line.contains_inner(own))
        .with_context(|| format!("id {own} is not mapped"))?;
    let host = own_line.outer + (own - own_line.inner);
    let mut others: Vec<&IdMapLine> = map.iter().filter(|line| *line != own_line).collect();
    if others.is_empty() {
        bail!(
            "patchbay's user namespace has no subordinate id range. Containers need \
             newuidmap/newgidmap on PATH and an /etc/subuid and /etc/subgid entry for \
             the user"
        );
    }
    others.sort_by_key(|line| line.outer);
    let mut lines = format!("0 {own} 1\n");
    let mut next = 1;
    for line in others {
        lines.push_str(&format!("{next} {} {}\n", line.inner, line.count));
        next += line.count;
    }
    Ok((lines, host))
}

/// Returns the `net.ipv4.ping_group_range` that allows every mapped gid.
///
/// The kernel turns both ends into host gids and allows the host gids between
/// them. The range therefore runs from the inner gid with the lowest host gid
/// to the inner gid with the highest.
fn ping_group_range(map: &[IdMapLine]) -> Result<(u32, u32)> {
    let low = map
        .iter()
        .min_by_key(|line| line.outer)
        .context("empty gid map")?;
    let high = map
        .iter()
        .max_by_key(|line| line.outer + (line.count - 1))
        .context("empty gid map")?;
    let high_end = high.inner + (high.count - 1);
    if high_end < low.inner {
        // Inner and host order disagree, so no single range covers both.
        // Allow the lowest line, which holds the user's own gid, the group of
        // container root.
        return Ok((low.inner, low.inner + (low.count - 1)));
    }
    Ok((low.inner, high_end))
}

/// Returns a podman command that runs as root of [`PodmanUserns`].
///
/// It has the global `--remote=false` flag, so add the subcommand after it.
/// `CONTAINER_HOST` would send the command to a podman service elsewhere.
fn runtime_command() -> Result<Command> {
    let mut cmd = in_podman_namespaces(PODMAN)?;
    cmd.arg("--remote=false");
    Ok(cmd)
}

/// Returns a command that runs `program` like podman, as root of [`PodmanUserns`].
///
/// The command joins the shared mount namespace. It keeps the caller's network
/// namespace and working directory.
fn in_podman_namespaces(program: &str) -> Result<Command> {
    let userns = PodmanUserns::get()?;
    let user_fd = userns.user_ns.as_raw_fd();
    let mount_fd = userns.mount_ns.as_raw_fd();
    // Joining a mount namespace moves to its root directory.
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|dir| std::ffi::CString::new(dir.into_os_string().into_encoded_bytes()).ok());
    let mut cmd = Command::new(program);
    // What podman's own rootless re-exec sets.
    cmd.env("_CONTAINERS_USERNS_CONFIGURED", "done")
        .env("_CONTAINERS_ROOTLESS_UID", userns.host_uid.to_string())
        .env("_CONTAINERS_ROOTLESS_GID", userns.host_gid.to_string());
    // SAFETY: the closure only makes system calls and does not allocate. The
    // fds stay open for the life of the process.
    unsafe {
        cmd.pre_exec(move || {
            let fd = BorrowedFd::borrow_raw(user_fd);
            move_into_link_name_space(fd, Some(LinkNameSpaceType::User))?;
            let fd = BorrowedFd::borrow_raw(mount_fd);
            move_into_link_name_space(fd, Some(LinkNameSpaceType::Mount))?;
            if let Some(cwd) = &cwd {
                // A directory that only exists in the caller's mount
                // namespace leaves the command in `/`.
                libc::chdir(cwd.as_ptr());
            }
            Ok(())
        });
    }
    Ok(cmd)
}

/// Returns whether podman is on `PATH` and podman's user namespace can be created.
///
/// Tests use it to skip. Call it after [`init_userns`](crate::init_userns).
/// It does not run podman: a plain podman run in patchbay's namespace removes
/// the host's `pause.pid`, which leaks the host's pause process.
#[doc(hidden)]
pub fn containers_supported() -> bool {
    crate::userns::find_in_path(PODMAN).is_some() && PodmanUserns::get().is_ok()
}

/// Detaches all mounts stacked on `path`.
fn detach_mounts(path: &std::ffi::CStr) {
    // Fails with EINVAL once no mount is left, or on a locked mount. The bound
    // ends the loop on any other error.
    for _ in 0..16 {
        // SAFETY: `path` is a valid C string.
        if unsafe { libc::umount2(path.as_ptr(), libc::MNT_DETACH) } != 0 {
            break;
        }
    }
}

/// Removes this process's containers when this process dies.
///
/// A shell in podman's namespaces waits until its stdin closes, which happens
/// when this process exits for any reason, also on `SIGKILL`. It then removes
/// every container with this process's [`OWNER_LABEL`]. The shell has a
/// process group of its own and ignores the signals of a Ctrl-C or a closed
/// terminal.
const WATCHER_SCRIPT: &str = r#"trap '' INT TERM HUP
read -r _
ids=$(podman --remote=false ps -aq --filter "label=$1")
[ -z "$ids" ] || podman --remote=false rm -f -t 0 $ids"#;

/// Starts the [`WATCHER_SCRIPT`] shell once per process.
fn ensure_watcher() -> Result<()> {
    /// The write end of the shell's stdin, open for the life of this process.
    static WATCHER: Mutex<Option<std::process::ChildStdin>> = Mutex::new(None);
    let mut watcher = WATCHER.lock().expect("poisoned");
    if watcher.is_none() {
        let label = format!("{OWNER_LABEL}={}", owner_tag());
        let mut cmd = in_podman_namespaces("sh")?;
        cmd.args(["-c", WATCHER_SCRIPT, "sh", &label])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        let mut shell = cmd.spawn().context("start the container watcher")?;
        *watcher = shell.stdin.take();
    }
    Ok(())
}

/// Runs a podman subcommand from the blocking pool and returns its output.
///
/// The command runs in the caller's network namespace: the host's outside
/// the lab, a device's in a device task. Fails with podman's stderr if podman
/// fails.
pub(crate) async fn podman(
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> Result<std::process::Output> {
    check(podman_output(args).await?)
}

/// Like [`podman`], but returns the output also if podman fails.
async fn podman_output(
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> Result<std::process::Output> {
    let args: Vec<OsString> = args.into_iter().map(|arg| arg.as_ref().into()).collect();
    tokio::task::spawn_blocking(move || run_podman(args))
        .await
        .context("podman task failed")?
}

/// Blocking form of [`podman`].
fn podman_blocking(
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> Result<std::process::Output> {
    check(run_podman(args)?)
}

/// Runs podman, see [`runtime_command`], and returns its output.
fn run_podman(args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Result<std::process::Output> {
    let output = runtime_command()?.args(args).output();
    output.context("run `podman` (is it installed?)")
}

/// Returns podman's output, or an error with its stderr if it failed.
fn check(output: std::process::Output) -> Result<std::process::Output> {
    if !output.status.success() {
        bail!(
            "podman failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

/// Removes a container with `podman rm`.
fn remove_blocking(name: &str) -> Result<()> {
    podman_blocking(["rm", "-f", "-t", "1", name])
        .with_context(|| format!("remove container '{name}'"))
        .map(drop)
}

/// Starts `podman run` in the device's network namespace, which the container joins.
///
/// A thread of its own waits for podman, so neither a cancelled build nor the
/// end of the device's runtime ends the wait. The returned [`ContainerInner`]
/// exists only after podman exits, because a removal before that would come
/// before the container exists.
fn start(
    device: &Device,
    name: String,
    owns_device: bool,
    args: Vec<String>,
) -> Result<tokio::sync::oneshot::Receiver<(Arc<ContainerInner>, Result<std::process::Output>)>> {
    let mut cmd = runtime_command()?;
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let podman = device
        .spawn_command_sync(cmd)
        .context("run `podman` (is it installed?)")?;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("patchbay-podman-run".into())
        .spawn(move || {
            let output = podman.wait_with_output().context("wait for podman");
            let inner = Arc::new(ContainerInner {
                name,
                removed: AtomicBool::new(false),
                owns_device,
            });
            // If the build is gone, dropping `inner` removes the container.
            let _ = sender.send((inner, output.and_then(check)));
        })
        .context("spawn the `podman run` thread")?;
    Ok(receiver)
}

/// Returns `<pid>:<start time>` of this process, for [`OWNER_LABEL`] and names.
///
/// The start time tells a reused pid apart.
fn owner_tag() -> &'static str {
    static TAG: OnceLock<String> = OnceLock::new();
    TAG.get_or_init(|| {
        let pid = std::process::id();
        // Field 22 is the start time.
        let start = proc_stat_field("self", 22).unwrap_or_else(|| "unknown".into());
        format!("{pid}:{start}")
    })
}

/// Returns field `index` (1-based, as in `proc(5)`) of `/proc/<pid>/stat`.
pub(crate) fn proc_stat_field(pid: impl std::fmt::Display, index: usize) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 2, the command name in parentheses, may contain spaces and `)`.
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace()
        .nth(index.checked_sub(3)?)
        .map(str::to_owned)
}

/// Ensures that the image is present locally, as set by `policy`.
pub(crate) async fn ensure_image(image: &str, policy: PullPolicy) -> Result<()> {
    if policy != PullPolicy::Always && image_present(image).await? {
        return Ok(());
    }
    if policy == PullPolicy::Never {
        bail!("image '{image}' is not present, and the pull policy is `Never`");
    }
    // podman runs in the caller's network namespace, which needs the host's.
    if !in_process_netns() {
        bail!(
            "cannot pull image '{image}' from inside a lab namespace (for example \
             from a device task). Pull it first with `Lab::pull_image` from outside \
             the lab"
        );
    }
    podman(["pull", "--", image])
        .await
        .with_context(|| format!("pull image '{image}'"))
        .map(drop)
}

/// Returns whether the calling thread is in the process's own network namespace.
fn in_process_netns() -> bool {
    let thread = std::fs::read_link("/proc/thread-self/ns/net");
    let process = std::fs::read_link("/proc/self/ns/net");
    matches!((thread, process), (Ok(thread), Ok(process)) if thread == process)
}

/// Returns whether the image already exists in local storage.
async fn image_present(image: &str) -> Result<bool> {
    let output = podman_output(["image", "exists", "--", image]).await?;
    match output.status.code() {
        Some(1) => Ok(false),
        _ => check(output).map(|_| true),
    }
}

/// Shared interior of a [`Container`].
///
/// The lab or `stop` removes the container. The drop removes one that never
/// made it into the lab's registry.
pub(crate) struct ContainerInner {
    name: String,
    /// Set once a removal started, so that the container is removed once.
    removed: AtomicBool,
    /// Whether the build created the device, which `stop` then removes.
    owns_device: bool,
}

impl ContainerInner {
    /// Removes the container, unless a removal already started.
    ///
    /// Inside a Tokio runtime, the removal runs on a thread of its own, not in
    /// `spawn_blocking`: a runtime that shuts down drops such tasks unstarted.
    pub(crate) fn remove(&self) {
        if self.removed.swap(true, Ordering::AcqRel) {
            return;
        }
        let name = self.name.clone();
        let remove = move || {
            if let Err(err) = remove_blocking(&name) {
                warn!(container = %name, err = %format_args!("{err:#}"), "container removal failed");
            }
        };
        if tokio::runtime::Handle::try_current().is_err() {
            return remove();
        }
        let spawned = std::thread::Builder::new()
            .name("patchbay-container-rm".into())
            .spawn(remove);
        if let Err(err) = spawned {
            warn!(container = %self.name, %err, "could not start the container removal");
        }
    }
}

impl Drop for ContainerInner {
    fn drop(&mut self) {
        self.remove();
    }
}

/// A container in the lab's registry.
pub(crate) struct ContainerEntry {
    /// The device whose network namespace the container joined.
    pub(crate) device: crate::NodeId,
    pub(crate) container: Arc<ContainerInner>,
}

impl ContainerEntry {
    /// Returns the container unless its removal started.
    pub(crate) fn live(&self) -> Option<Arc<ContainerInner>> {
        self.is_alive().then(|| Arc::clone(&self.container))
    }

    /// Returns whether the container's removal has not started.
    pub(crate) fn is_alive(&self) -> bool {
        !self.container.removed.load(Ordering::Acquire)
    }

    /// Removes the container.
    pub(crate) fn remove(self) {
        self.container.remove();
    }
}

/// A container running as a lab device.
///
/// A container runs an OCI image in a device's network namespace, so other
/// lab devices reach it at the device's lab IP. [`Lab::add_container`] creates
/// a device for it. [`Device::add_container`] joins an existing device, whose
/// code and other containers then reach it on `127.0.0.1`.
///
/// The lab owns the container, like a device: dropping handles does not
/// remove it. It runs until [`stop`](Self::stop), until
/// [`Lab::remove_device`] removes its device, or until the last [`Lab`] handle
/// drops. If the process dies without cleanup, also on `SIGKILL` or Ctrl-C, a
/// watcher process removes its containers.
///
/// Containers run with podman, which must be on `PATH`. `podman run` starts in
/// the device's network namespace, and `--network=host` makes the container
/// join it. Docker does not work, because its containers join the namespace
/// of its daemon. The host also needs `newuidmap`, `newgidmap`, and an
/// `/etc/subuid` and `/etc/subgid` entry for the user.
///
/// A container differs from a device in these ways:
///
/// - [`Device`] methods such as [`spawn`](Device::spawn) run code on the host
///   file system, so `Container` does not dereference to `Device`. Use
///   [`command`](Self::command) to run a command in the container.
/// - The container sees the device's `/etc/hosts` and `/etc/resolv.conf`,
///   read-only, also entries that [`Device::set_host`] adds after the start.
/// - Container root cannot change the lab's routes or firewall rules. It can
///   bind low ports and ping.
/// - podman in your shell lists patchbay's containers, but cannot `exec` into
///   them, copy files, or stop one that runs as non-root.
///
/// # Examples
///
/// ```no_run
/// # async fn f(lab: patchbay::Lab, net: patchbay::NodeId) -> anyhow::Result<()> {
/// let pebble = lab
///     .add_container("pebble", "ghcr.io/letsencrypt/pebble:latest")
///     .uplink(net)
///     .ready_tcp(14000)
///     .build()
///     .await?;
/// // Other lab devices reach it at this address.
/// let ip = pebble.ip().expect("pebble has an IPv4 address");
/// let directory = format!("https://{ip}:14000/dir");
/// # let _ = directory;
/// # Ok(())
/// # }
/// ```
///
/// [`Lab`]: crate::Lab
/// [`Lab::add_container`]: crate::Lab::add_container
/// [`Lab::remove_device`]: crate::Lab::remove_device
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

impl Container {
    /// Joins a device and a container interior into a handle.
    pub(crate) fn from_parts(device: Device, inner: Arc<ContainerInner>) -> Self {
        Self { device, inner }
    }

    /// Returns the lab device the container runs in.
    ///
    /// Code that the device runs, for example with [`Device::spawn`], runs on
    /// the host file system, not in the container.
    pub fn device(&self) -> &Device {
        &self.device
    }

    /// Returns the node id of the container's device.
    pub fn id(&self) -> crate::NodeId {
        self.device.id()
    }

    /// Returns the container's IPv4 lab address, if it has one.
    pub fn ip(&self) -> Option<Ipv4Addr> {
        self.device.ip()
    }

    /// Returns the container's IPv6 lab address, if it has one.
    pub fn ip6(&self) -> Option<std::net::Ipv6Addr> {
        self.device.ip6()
    }

    /// Returns the podman container name, which is unique on the host.
    pub fn podman_name(&self) -> &str {
        &self.inner.name
    }

    /// Runs a command in the container and returns its output.
    ///
    /// The first item is the program. A short form of
    /// [`command`](Self::command) with [`output`](ContainerCommand::output).
    ///
    /// # Errors
    ///
    /// Returns an error if `cmd` is empty or podman does not start. A non-zero
    /// exit of the command is in the returned output, not an error.
    pub async fn exec<I, S>(&self, cmd: I) -> Result<std::process::Output>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut parts = cmd.into_iter().map(Into::into);
        let program = parts.next().context("exec needs a program")?;
        self.command(program).args(parts).output().await
    }

    /// Returns a command that runs `program` in the container.
    pub fn command(&self, program: impl Into<String>) -> ContainerCommand {
        ContainerCommand {
            container: self.clone(),
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            workdir: None,
            user: None,
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }

    /// Returns the container's logs (stdout and stderr, combined).
    pub async fn logs(&self) -> Result<String> {
        let output = podman(["logs", &self.inner.name]).await?;
        let mut logs = String::from_utf8_lossy(&output.stdout).into_owned();
        logs.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok(logs)
    }

    /// Stops and removes the container, and frees its name in the lab.
    ///
    /// Also removes the device if [`Lab::add_container`](crate::Lab::add_container)
    /// created it. Returns `Ok` at once if a removal already started.
    pub async fn stop(&self) -> Result<()> {
        if self.inner.removed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let name = self.inner.name.clone();
        let result = tokio::task::spawn_blocking(move || remove_blocking(&name))
            .await
            .context("container runtime task failed")
            .and_then(|result| result);
        if result.is_err() {
            // Let a later stop or the drop try again.
            self.inner.removed.store(false, Ordering::Release);
        } else if self.inner.owns_device {
            // Fails only if the device is gone already.
            let _ = self.device.lab().remove_device(self.device.id());
        }
        result
    }

    /// Copies a host file or directory into the container.
    ///
    /// The destination's parent directory must exist in the container.
    pub async fn copy_to(&self, host_src: impl AsRef<Path>, container_dest: &str) -> Result<()> {
        let dest = self.in_container(container_dest);
        let args = ["cp".into(), "--".into(), host_path(host_src)?, dest];
        podman(args).await.map(drop)
    }

    /// Copies a file or directory out of the container to a host path.
    pub async fn copy_from(&self, container_src: &str, host_dest: impl AsRef<Path>) -> Result<()> {
        let src = self.in_container(container_src);
        let args = ["cp".into(), "--".into(), src, host_path(host_dest)?];
        podman(args).await.map(drop)
    }

    /// Returns `podman cp`'s `<container>:<path>` form of a container path.
    fn in_container(&self, path: &str) -> OsString {
        format!("{}:{path}", self.inner.name).into()
    }

    /// Writes a file in the container, creating or replacing it.
    ///
    /// The parent directory must exist in the container.
    pub async fn write_file(&self, container_path: &str, contents: impl AsRef<[u8]>) -> Result<()> {
        let dir = tempfile::tempdir().context("create staging directory")?;
        let staged = dir.path().join("file");
        tokio::fs::write(&staged, contents.as_ref())
            .await
            .context("write staging file")?;
        self.copy_to(&staged, container_path).await
    }

    /// Reads a file in the container.
    pub async fn read_file(&self, container_path: &str) -> Result<Vec<u8>> {
        let dir = tempfile::tempdir().context("create staging directory")?;
        let staged = dir.path().join("file");
        self.copy_from(container_path, &staged).await?;
        tokio::fs::read(&staged)
            .await
            .with_context(|| format!("read '{container_path}', which must be a file"))
    }

    /// Returns whether the container's processes still run.
    async fn is_running(&self) -> Result<bool> {
        let args = [
            "inspect",
            "--format",
            "{{.State.Running}}",
            &self.inner.name,
        ];
        let output = podman(args).await?;
        Ok(String::from_utf8_lossy(&output.stdout).trim() == "true")
    }

    /// Polls the readiness gate until it passes or times out.
    async fn wait_ready(&self, ready_tcp: Option<u16>, timeout: Duration) -> Result<()> {
        let Some(port) = ready_tcp else {
            return Ok(());
        };
        let target = ready_target(&self.device, port);
        // A timeout too large for an `Instant` never ends.
        let deadline = Instant::now().checked_add(timeout);
        let mut next_state_check = Instant::now() + Duration::from_secs(1);
        loop {
            if accepts(&self.device, target).await? {
                debug!(container = %self.inner.name, %target, "container ready");
                return Ok(());
            }
            let exited = if Instant::now() >= next_state_check {
                next_state_check = Instant::now() + Duration::from_secs(1);
                !self.is_running().await?
            } else {
                false
            };
            if exited || deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                let logs = self.logs().await.unwrap_or_default();
                let why = if exited {
                    "exited before it accepted a connection".to_string()
                } else {
                    format!("did not accept a connection within {timeout:?}")
                };
                bail!(
                    "container '{}' on {target} {why}\n--- logs ---\n{}",
                    self.inner.name,
                    logs.trim()
                );
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

/// Returns the address that the `ready_tcp` gate connects to: the device's lab IP.
fn ready_target(device: &Device, port: u16) -> SocketAddr {
    let ip = device
        .ip()
        .map(IpAddr::V4)
        .or_else(|| device.ip6().map(IpAddr::V6))
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
    SocketAddr::new(ip, port)
}

/// Returns whether `target` accepts a TCP connection from inside the device.
async fn accepts(device: &Device, target: SocketAddr) -> Result<bool> {
    let connect = device.spawn(move |_| async move {
        let connect = tokio::net::TcpStream::connect(target);
        let connected = tokio::time::timeout(Duration::from_millis(500), connect).await;
        matches!(connected, Ok(Ok(_)))
    })?;
    connect.await.context("connect task failed")
}

/// Returns a host path for `podman cp`, which reads a relative path with `:` as `container:path`.
fn host_path(path: impl AsRef<Path>) -> Result<OsString> {
    let path = path.as_ref();
    let absolute = std::path::absolute(path);
    let absolute = absolute.with_context(|| format!("resolve '{}'", path.display()))?;
    Ok(absolute.into_os_string())
}

/// Name of the environment variable that marks the processes of one [`ContainerCommand`].
const EXEC_MARKER_VAR: &str = "PATCHBAY_EXEC_ID";

/// How often a kill checks the processes of a [`ContainerCommand`] again.
const KILL_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long a kill waits for the podman client to exit before it kills the client.
const KILL_CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

/// A command that runs in a [`Container`] with `podman exec`.
///
/// It runs in the container's file system and network namespace, as the
/// container's user unless [`user`](Self::user) sets one. The builder mirrors
/// [`std::process::Command`]: stdin is null by default, and
/// [`spawn`](Self::spawn) and [`spawn_sync`](Self::spawn_sync) inherit stdout
/// and stderr.
///
/// `podman exec` does not pass `SIGKILL` on to the process in the container,
/// so a kill sends it to the command's processes itself. It finds them below
/// the podman client, and by a unique `PATCHBAY_EXEC_ID` environment variable
/// that each command gets. A child process that outlives its parent and clears
/// its environment escapes both. Dropping a child whose process runs kills the
/// same way, from a background thread.
///
/// # Examples
///
/// ```no_run
/// # async fn f(container: patchbay::Container) -> anyhow::Result<()> {
/// let output = container
///     .command("sh")
///     .args(["-c", "echo $GREETING from $(pwd)"])
///     .env("GREETING", "hello")
///     .workdir("/tmp")
///     .output()
///     .await?;
/// assert_eq!(output.stdout, b"hello from /tmp\n");
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
#[must_use = "a ContainerCommand does nothing until it is spawned"]
pub struct ContainerCommand {
    container: Container,
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    workdir: Option<String>,
    user: Option<String>,
    stdin: Option<std::process::Stdio>,
    stdout: Option<std::process::Stdio>,
    stderr: Option<std::process::Stdio>,
}

impl ContainerCommand {
    /// Appends an argument.
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Appends arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Sets an environment variable for the command (`podman exec --env`).
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Sets several environment variables for the command.
    pub fn envs<I, K, V>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let vars = vars
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()));
        self.env.extend(vars);
        self
    }

    /// Sets the working directory in the container (`podman exec --workdir`).
    pub fn workdir(mut self, dir: impl Into<String>) -> Self {
        self.workdir = Some(dir.into());
        self
    }

    /// Sets the user the command runs as (`podman exec --user`).
    ///
    /// Takes a user name from the image or a uid, optionally with `:group`.
    /// `"0"` is root in the container.
    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// Sets stdin. Any value passes `--interactive` to `podman exec`.
    pub fn stdin(mut self, stdin: impl Into<std::process::Stdio>) -> Self {
        self.stdin = Some(stdin.into());
        self
    }

    /// Sets stdout.
    pub fn stdout(mut self, stdout: impl Into<std::process::Stdio>) -> Self {
        self.stdout = Some(stdout.into());
        self
    }

    /// Sets stderr.
    pub fn stderr(mut self, stderr: impl Into<std::process::Stdio>) -> Self {
        self.stderr = Some(stderr.into());
        self
    }

    /// Spawns the command and returns a handle for a Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if podman does not start. A command that podman cannot
    /// run, for example in a removed container, exits with an error status.
    pub fn spawn(self) -> Result<ContainerChild> {
        self.spawn_with(std::process::Stdio::inherit)
    }

    /// Spawns the command and returns a handle for blocking code.
    ///
    /// # Errors
    ///
    /// The same as [`spawn`](Self::spawn).
    pub fn spawn_sync(self) -> Result<ContainerChildSync> {
        let (device, cmd, marker) = self.podman_command(std::process::Stdio::inherit)?;
        let mut client = device
            .spawn_command_sync(cmd)
            .context("spawn `podman exec` (is podman installed?)")?;
        Ok(ContainerChildSync {
            stdin: client.stdin.take(),
            stdout: client.stdout.take(),
            stderr: client.stderr.take(),
            exec: Exec::new(client.id(), client, marker),
        })
    }

    /// Runs the command to completion and returns its output.
    ///
    /// Captures stdout and stderr, unless they are set.
    pub async fn output(self) -> Result<std::process::Output> {
        self.spawn_with(std::process::Stdio::piped)?
            .wait_with_output()
            .await
    }

    /// Spawns the command, with `default_output` for unset stdout and stderr.
    fn spawn_with(self, default_output: fn() -> std::process::Stdio) -> Result<ContainerChild> {
        let (device, cmd, marker) = self.podman_command(default_output)?;
        let mut client = device
            .spawn_command(tokio::process::Command::from(cmd))
            .context("spawn `podman exec` (is podman installed?)")?;
        let pid = client.id().expect("a child has a pid until it is reaped");
        Ok(ContainerChild {
            stdin: client.stdin.take(),
            stdout: client.stdout.take(),
            stderr: client.stderr.take(),
            exec: Exec::new(pid, client, marker),
        })
    }

    /// Builds the `podman exec` command, with `default_output` for unset outputs.
    fn podman_command(
        self,
        default_output: fn() -> std::process::Stdio,
    ) -> Result<(Device, Command, String)> {
        let marker = {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let seq = NEXT.fetch_add(1, Ordering::Relaxed);
            format!("{EXEC_MARKER_VAR}={}/{seq}", owner_tag())
        };
        let mut args = vec!["exec".to_string()];
        if self.stdin.is_some() {
            args.push("--interactive".into());
        }
        for (key, value) in &self.env {
            args.push("--env".into());
            args.push(format!("{key}={value}"));
        }
        args.push("--env".into());
        args.push(marker.clone());
        if let Some(dir) = self.workdir {
            args.push("--workdir".into());
            args.push(dir);
        }
        if let Some(user) = self.user {
            args.push("--user".into());
            args.push(user);
        }
        args.push(self.container.inner.name.clone());
        args.push(self.program);
        args.extend(self.args);

        let mut cmd = runtime_command()?;
        // As a subreaper, the client stays the parent of the command's conmon,
        // whose child is the command. A kill walks this tree.
        // SAFETY: the closure only makes a system call.
        unsafe {
            cmd.pre_exec(|| {
                rustix::process::set_child_subreaper(Some(rustix::process::getpid()))?;
                Ok(())
            });
        }
        cmd.args(args)
            .stdin(self.stdin.unwrap_or_else(std::process::Stdio::null))
            .stdout(self.stdout.unwrap_or_else(default_output))
            .stderr(self.stderr.unwrap_or_else(default_output));
        Ok((self.container.device, cmd, marker))
    }
}

/// The processes of one [`ContainerCommand`], which a kill finds in `/proc`.
///
/// patchbay has all capabilities in the nested user namespace, so it can read
/// `/proc` of any container user and kill the process.
#[derive(Debug, Clone)]
struct ExecProcesses {
    /// Pid of the podman client.
    client: u32,
    /// The `PATCHBAY_EXEC_ID=<owner tag>/<counter>` entry of the command's environment.
    marker: String,
}

impl ExecProcesses {
    /// Sends `SIGKILL` to the command's processes.
    ///
    /// These are the processes below the client outside podman's mount
    /// namespace, which leaves out conmon, and those with the marker. Only the
    /// marker finds a process once `client_runs` is false: a reaped client's
    /// pid may belong to another process.
    fn kill(&self, client_runs: bool) {
        let tree = client_runs.then(|| self.tree()).flatten();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return;
        };
        for pid in entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        {
            // The pidfd keeps a reused pid from getting the signal, so the
            // checks below describe the process that gets it.
            let pidfd = Pid::from_raw(pid as i32).and_then(|pid| {
                rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()).ok()
            });
            let Some(pidfd) = pidfd else {
                continue;
            };
            let in_tree = tree.as_ref().is_some_and(|(tree, podman_mounts)| {
                tree.contains(&pid)
                    && parent(pid).is_some_and(|parent| tree.contains(&parent))
                    && mount_ns(pid).is_some_and(|mounts| mounts != *podman_mounts)
            });
            if in_tree || self.marks(pid) {
                // The process may have exited since the checks.
                let _ = rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::KILL);
            }
        }
    }

    /// Returns the client with the processes below it, and its mount namespace.
    fn tree(&self) -> Option<(HashSet<u32>, PathBuf)> {
        let podman_mounts = mount_ns(self.client)?;
        let parents: Vec<(u32, u32)> = std::fs::read_dir("/proc")
            .ok()?
            .flatten()
            .filter_map(|entry| {
                let pid = entry.file_name().to_str()?.parse().ok()?;
                Some((pid, parent(pid)?))
            })
            .collect();
        let mut tree = HashSet::from([self.client]);
        loop {
            let below: Vec<u32> = parents
                .iter()
                .filter(|(pid, parent)| tree.contains(parent) && !tree.contains(pid))
                .map(|(pid, _)| *pid)
                .collect();
            if below.is_empty() {
                return Some((tree, podman_mounts));
            }
            tree.extend(below);
        }
    }

    /// Returns whether the environment of process `pid` holds the marker.
    fn marks(&self, pid: u32) -> bool {
        std::fs::read(format!("/proc/{pid}/environ")).is_ok_and(|environ| {
            environ
                .split(|byte| *byte == 0)
                .any(|var| var == self.marker.as_bytes())
        })
    }
}

/// Returns the parent of a process.
fn parent(pid: u32) -> Option<u32> {
    proc_stat_field(pid, 4)?.parse().ok()
}

/// Returns the mount namespace of a process, as `mnt:[<inode>]`.
fn mount_ns(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/ns/mnt")).ok()
}

/// The podman client process of a [`ContainerChild`] or [`ContainerChildSync`].
trait PodmanClient: Send + 'static {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>>;
    fn kill(&mut self) -> std::io::Result<()>;
}

impl PodmanClient for std::process::Child {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        std::process::Child::try_wait(self)
    }

    fn kill(&mut self) -> std::io::Result<()> {
        std::process::Child::kill(self)
    }
}

impl PodmanClient for tokio::process::Child {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        tokio::process::Child::try_wait(self)
    }

    fn kill(&mut self) -> std::io::Result<()> {
        self.start_kill()
    }
}

/// Ends a round of a kill, after [`ExecProcesses::kill`]. Returns whether the kill is done.
///
/// `status` is the client's status from before the round. The client exits
/// when the command's process is gone, and a process that started after a
/// round gets the next one. After [`KILL_CLIENT_TIMEOUT`], the round kills
/// the client, and the kill then fails.
fn kill_done(
    status: Option<std::process::ExitStatus>,
    client: &mut impl PodmanClient,
    started: Instant,
) -> Result<bool> {
    use std::os::unix::process::ExitStatusExt;
    match status {
        // `podman exec` exits with a code. A signal ends it only from outside.
        Some(status) if status.signal() == Some(libc::SIGKILL) => {
            bail!("podman was killed, so the command's processes may still run in the container")
        }
        Some(_) => Ok(true),
        None => {
            if started.elapsed() > KILL_CLIENT_TIMEOUT {
                client.kill().context("kill podman client")?;
            }
            Ok(false)
        }
    }
}

/// Kills the command's processes until the client exits, blocking the thread.
fn kill_blocking(processes: &ExecProcesses, client: &mut impl PodmanClient) -> Result<()> {
    let started = Instant::now();
    loop {
        let status = client.try_wait().context("check podman client")?;
        processes.kill(status.is_none());
        if kill_done(status, client, started)? {
            return Ok(());
        }
        std::thread::sleep(KILL_POLL_INTERVAL);
    }
}

/// Finishes a kill in a background thread, for a handle dropped while its client runs.
fn kill_in_background(processes: ExecProcesses, mut client: impl PodmanClient) {
    match client.try_wait() {
        Ok(None) => {}
        Ok(Some(_)) => return,
        Err(err) => {
            warn!(%err, "could not check a dropped podman client");
            return;
        }
    }
    let spawned = std::thread::Builder::new()
        .name("patchbay-exec-kill".into())
        .spawn(move || {
            if let Err(err) = kill_blocking(&processes, &mut client) {
                warn!(err = %format_args!("{err:#}"), "could not kill a dropped container command");
            }
        });
    if let Err(err) = spawned {
        warn!(%err, "could not start the kill of a dropped container command");
    }
}

/// The podman client of a [`ContainerChild`] or [`ContainerChildSync`].
///
/// The drop kills the command if the client still runs.
#[derive(Debug)]
struct Exec<C: PodmanClient> {
    /// Taken only on drop.
    client: Option<C>,
    processes: ExecProcesses,
}

impl<C: PodmanClient> Exec<C> {
    fn new(pid: u32, client: C, marker: String) -> Self {
        Self {
            client: Some(client),
            processes: ExecProcesses {
                client: pid,
                marker,
            },
        }
    }

    /// Returns the client.
    fn client(&mut self) -> &mut C {
        self.client
            .as_mut()
            .expect("the client is taken only on drop")
    }
}

impl<C: PodmanClient> Drop for Exec<C> {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            kill_in_background(self.processes.clone(), client);
        }
    }
}

/// A process spawned with [`ContainerCommand::spawn`], for a Tokio runtime.
///
/// Mirrors [`tokio::process::Child`]. Dropping the handle while the process
/// runs kills it, see [`ContainerCommand`].
#[derive(Debug)]
pub struct ContainerChild {
    /// The stdin pipe, if [`ContainerCommand::stdin`] set it to piped.
    pub stdin: Option<tokio::process::ChildStdin>,
    /// The stdout pipe, if it is piped.
    pub stdout: Option<tokio::process::ChildStdout>,
    /// The stderr pipe, if it is piped.
    pub stderr: Option<tokio::process::ChildStderr>,
    exec: Exec<tokio::process::Child>,
}

impl ContainerChild {
    /// Returns the exit status if the process exited.
    ///
    /// `podman exec` exits with the exit code of the process.
    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        self.exec.client().try_wait().context("check podman client")
    }

    /// Waits for the process to exit. Closes stdin first.
    pub async fn wait(&mut self) -> Result<std::process::ExitStatus> {
        drop(self.stdin.take());
        self.exec
            .client()
            .wait()
            .await
            .context("wait for podman client")
    }

    /// Waits for the process to exit and collects stdout and stderr.
    pub async fn wait_with_output(mut self) -> Result<std::process::Output> {
        async fn read_all(pipe: Option<impl AsyncRead + Unpin>) -> Result<Vec<u8>> {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                pipe.read_to_end(&mut buf).await.context("read output")?;
            }
            Ok(buf)
        }
        let stdout = self.stdout.take();
        let stderr = self.stderr.take();
        let (status, stdout, stderr) =
            tokio::try_join!(self.wait(), read_all(stdout), read_all(stderr))?;
        Ok(std::process::Output {
            status,
            stdout,
            stderr,
        })
    }

    /// Kills the process and its child processes in the container.
    ///
    /// Waits until the podman client exits. After the process exited, this
    /// still kills the child processes it left running, but a drop does not.
    ///
    /// # Errors
    ///
    /// Returns an error if the client does not exit within 5 seconds. The kill
    /// then kills the client, and processes may still run in the container.
    pub async fn kill(&mut self) -> Result<()> {
        let started = Instant::now();
        loop {
            let status = self
                .exec
                .client()
                .try_wait()
                .context("check podman client")?;
            let processes = self.exec.processes.clone();
            tokio::task::spawn_blocking(move || processes.kill(status.is_none()))
                .await
                .context("kill task failed")?;
            if kill_done(status, self.exec.client(), started)? {
                return Ok(());
            }
            tokio::time::sleep(KILL_POLL_INTERVAL).await;
        }
    }
}

/// A process spawned with [`ContainerCommand::spawn_sync`], for blocking code.
///
/// Mirrors [`std::process::Child`]. Dropping the handle while the process
/// runs kills it, see [`ContainerCommand`].
#[derive(Debug)]
pub struct ContainerChildSync {
    /// The stdin pipe, if [`ContainerCommand::stdin`] set it to piped.
    pub stdin: Option<std::process::ChildStdin>,
    /// The stdout pipe, if it is piped.
    pub stdout: Option<std::process::ChildStdout>,
    /// The stderr pipe, if it is piped.
    pub stderr: Option<std::process::ChildStderr>,
    exec: Exec<std::process::Child>,
}

impl ContainerChildSync {
    /// Returns the exit status if the process exited, like [`ContainerChild::try_wait`].
    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        self.exec.client().try_wait().context("check podman client")
    }

    /// Waits for the process to exit. Closes stdin first.
    pub fn wait(&mut self) -> Result<std::process::ExitStatus> {
        drop(self.stdin.take());
        self.exec.client().wait().context("wait for podman client")
    }

    /// Kills the process and its child processes, like [`ContainerChild::kill`].
    pub fn kill(&mut self) -> Result<()> {
        let processes = self.exec.processes.clone();
        kill_blocking(&processes, self.exec.client())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_args_have_the_required_flags() {
        let spec = RunSpec {
            image: "docker.io/library/alpine".into(),
            env: vec![("A".into(), "1".into()), ("B".into(), "2".into())],
            args: vec!["sleep".into(), "infinity".into()],
            run_args: vec!["--cgroup-manager=cgroupfs".into()],
            ..RunSpec::default()
        };
        let args = build_run_args("lab-p1-c0", &spec, &[]);
        // Detached, name, host network, and no --replace, which could remove
        // another process's container.
        assert_eq!(&args[0], "run");
        assert!(args.contains(&"-d".to_string()));
        assert!(args.contains(&"--network=host".to_string()));
        assert!(!args.contains(&"--replace".to_string()));
        let name_idx = args.iter().position(|a| a == "--name").unwrap();
        assert_eq!(args[name_idx + 1], "lab-p1-c0");
        let owner = format!("{OWNER_LABEL}={}", owner_tag());
        assert!(has_flag(&args, "--label", &owner));
        assert!(has_flag(&args, "--env", "A=1"));
        assert!(has_flag(&args, "--env", "B=2"));
        // An extra run arg comes before the image.
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
        let spec = RunSpec {
            image: "img".into(),
            mounts,
            ..RunSpec::default()
        };
        let args = build_run_args("c", &spec, &[]);
        let image_idx = args.iter().position(|a| a == "img").unwrap();
        assert!(has_flag(
            &args,
            "--mount",
            r#"type=bind,"src=/srv/data","dst=/data""#
        ));
        assert!(has_flag(
            &args,
            "--mount",
            r#"type=bind,"src=/etc/conf","dst=/conf",ro"#
        ));
        assert!(has_flag(&args, "--tmpfs", "/scratch"));
        // All mount args precede the image.
        let last_mount = args
            .iter()
            .rposition(|a| a == "--mount" || a == "--tmpfs")
            .unwrap();
        assert!(last_mount < image_idx);
    }

    #[test]
    fn entrypoint_renders_as_a_json_array_before_the_image() {
        let spec = RunSpec {
            image: "img".into(),
            entrypoint: Some(vec!["/bin/sh".into(), "-c".into()]),
            args: vec!["echo \"hi\"".into()],
            ..RunSpec::default()
        };
        let args = build_run_args("c", &spec, &[]);
        let image_idx = args.iter().position(|a| a == "img").unwrap();
        let entry_idx = args
            .iter()
            .position(|a| a == r#"--entrypoint=["/bin/sh","-c"]"#)
            .unwrap();
        assert!(entry_idx < image_idx);
        assert_eq!(args[image_idx + 1], "echo \"hi\"");

        // An empty entrypoint clears the image's entrypoint.
        let spec = RunSpec {
            image: "img".into(),
            entrypoint: Some(Vec::new()),
            ..RunSpec::default()
        };
        assert!(build_run_args("c", &spec, &[]).contains(&"--entrypoint=[]".to_string()));
        let spec = RunSpec {
            image: "img".into(),
            ..RunSpec::default()
        };
        assert!(!build_run_args("c", &spec, &[])
            .iter()
            .any(|a| a.starts_with("--entrypoint")));
    }

    /// Returns whether `args` holds `flag` followed by `value`.
    fn has_flag(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2)
            .any(|pair| pair[0] == flag && pair[1] == value)
    }

    fn line(inner: u32, outer: u32, count: u32) -> IdMapLine {
        IdMapLine {
            inner,
            outer,
            count,
        }
    }

    #[test]
    fn nested_map_matches_rootless_podman_layout() {
        // patchbay's map for uid 1000 with subordinate ids 100000..=165535.
        let map = [
            line(1000, 1000, 1),
            line(0, 100000, 1000),
            line(1001, 101000, 64536),
        ];
        let (lines, host) = nested_id_map(&map, 1000).unwrap();
        // Nested N maps to host 100000 + N - 1, as in plain rootless podman.
        assert_eq!(lines, "0 1000 1\n1 0 1000\n1001 1001 64536\n");
        assert_eq!(host, 1000);
    }

    #[test]
    fn nested_map_sorts_by_host_id() {
        let map = [line(5, 200, 10), line(1000, 1000, 1), line(0, 100, 5)];
        let (lines, _) = nested_id_map(&map, 1000).unwrap();
        assert_eq!(lines, "0 1000 1\n1 0 5\n6 5 10\n");
    }

    #[test]
    fn nested_map_needs_a_range() {
        let err = nested_id_map(&[line(1000, 1000, 1)], 1000).unwrap_err();
        assert!(err.to_string().contains("subordinate id range"), "{err}");
        assert!(nested_id_map(&[line(1000, 1000, 1)], 5).is_err());
    }

    #[test]
    fn ping_range_spans_all_mapped_host_gids() {
        let map = [
            line(1000, 1000, 1),
            line(0, 100000, 1000),
            line(1001, 101000, 64536),
        ];
        assert_eq!(ping_group_range(&map).unwrap(), (1000, 65536));
        let single = [line(1000, 1000, 1)];
        assert_eq!(ping_group_range(&single).unwrap(), (1000, 1000));
        // A range smaller than the gid: inner order disagrees with host order.
        let small = [line(1000, 1000, 1), line(0, 100000, 10)];
        assert_eq!(ping_group_range(&small).unwrap(), (1000, 1000));
    }

    #[test]
    fn parse_volume_accepts_podman_syntax() {
        assert_eq!(parse_volume("/a:/b").unwrap(), ("/a", "/b", false));
        assert_eq!(parse_volume("/a:/b:ro").unwrap(), ("/a", "/b", true));
        assert_eq!(parse_volume("/a:/b:rw").unwrap(), ("/a", "/b", false));
        assert!(parse_volume("/a").is_err());
        assert!(parse_volume("/a:/b:z").is_err());
        assert!(parse_volume(":/b").is_err());
    }

    /// Parallel first uses of podman's namespace all get the same one.
    #[test]
    fn podman_userns_created_in_parallel() {
        // Not `containers_supported`, which would create the namespace.
        if crate::userns::find_in_path(PODMAN).is_none() || !crate::userns::root_is_mapped() {
            eprintln!("skipping: podman or a subordinate id range is missing");
            return;
        }
        let threads = 16;
        let (sender, receiver) = std::sync::mpsc::sync_channel(threads);
        for _ in 0..threads {
            let sender = sender.clone();
            std::thread::spawn(move || sender.send(PodmanUserns::get()));
        }
        let mut first = None;
        for _ in 0..threads {
            let userns = receiver.recv_timeout(Duration::from_secs(20));
            let userns = userns.expect("a creation hangs").expect("create");
            assert!(std::ptr::eq(*first.get_or_insert(userns), userns));
        }
    }

    /// patchbay's podman stays local, in the device, despite `CONTAINER_HOST`.
    ///
    /// `CONTAINER_HOST` turns a podman client remote unless `--remote=false`
    /// overrides it.
    #[test]
    fn podman_ignores_a_remote_container_host() {
        if !containers_supported() {
            eprintln!("skipping: podman or a subordinate id range is missing");
            return;
        }
        let output = runtime_command()
            .unwrap()
            .env("CONTAINER_HOST", "unix:///nonexistent/podman.sock")
            .args(["ps", "-a", "--format", "{{.Names}}"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "podman went remote: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    /// A container that finishes its start after its device or lab is gone is not registered.
    ///
    /// The caller's guard then removes it, instead of the registry keeping it.
    #[tokio::test(flavor = "current_thread")]
    async fn late_registration_fails() -> Result<()> {
        crate::check_caps()?;
        let lab = crate::Lab::new().await?;
        let net = lab.add_router("net").build().await?;
        let removed = lab.add_device("removed").uplink(net.id()).build().await?;
        let kept = lab.add_device("kept").uplink(net.id()).build().await?;
        // Marked removed, so that its drop does not run podman.
        let container = Arc::new(ContainerInner {
            name: "unused".into(),
            removed: AtomicBool::new(true),
            owns_device: false,
        });

        lab.remove_device(removed.id())?;
        let registered = removed
            .lab()
            .register_container("c", removed.id(), &container);
        assert!(registered.is_err(), "registered on a removed device");

        let lab_inner = Arc::clone(kept.lab());
        drop(lab);
        let registered = lab_inner.register_container("c", kept.id(), &container);
        assert!(registered.is_err(), "registered after the lab dropped");
        Ok(())
    }

    #[test]
    fn sanitize_name_replaces_invalid_chars() {
        assert_eq!(sanitize_name("lab-p1/c0"), "lab-p1-c0");
        assert_eq!(sanitize_name("ok_name.1-2"), "ok_name.1-2");
        assert_eq!(sanitize_name("a b:c"), "a-b-c");
    }
}
