//! Local rootless Podman integration for devices.
//!
//! Use [`crate::init_userns_with_podman_for_ctor`] with libtest or call
//! [`crate::init_userns_with_podman`] at the start of your own main function.
//! Pass the bootstrap token to [`Lab::create_podman`].

use std::{
    collections::HashMap,
    env,
    ffi::CString,
    fs::File,
    io::{IoSlice, IoSliceMut},
    mem::MaybeUninit,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock, Weak,
    },
};

use anyhow::{anyhow, bail, ensure, Context, Result};
use rustix::net::{
    recvmsg, sendmsg, shutdown, socketpair, AddressFamily, RecvAncillaryBuffer,
    RecvAncillaryMessage, RecvFlags, ReturnFlags, SendAncillaryBuffer, SendAncillaryMessage,
    SendFlags, Shutdown, SocketFlags, SocketType,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{Device, Lab};

mod bootstrap;

use bootstrap::{Connection, HELLO, HELPER_BINARY};
const MAX_MESSAGE: usize = 64 * 1024;

static PROCESS_HELPER: OnceLock<std::result::Result<Arc<Helper>, String>> = OnceLock::new();

/// A token for starting a lab session on the process-wide Podman helper.
///
/// Move this token into [`Lab::create_podman`]. Each lab can obtain its own
/// token with [`podman_session_token`].
pub struct PodmanBootstrap(Arc<Helper>);

/// Local Podman session for one lab.
///
/// Holds the lab open until the session is dropped. Dropping the session
/// removes its containers. Other sessions keep using the same helper.
/// Cleanup waits for any pending Podman command and the session's removals.
pub struct Podman {
    session: Arc<Session>,
}

struct Session {
    helper: Arc<Helper>,
    lab: Lab,
    id: SessionId,
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Err(error) = self.helper.exchange(self.id, Operation::Close, None) {
            eprintln!("could not clean up Podman session {}: {error:#}", self.id);
        }
    }
}

impl Lab {
    /// Bind the bootstrapped Podman helper to this lab.
    pub fn create_podman(&self, bootstrap: PodmanBootstrap) -> Result<Podman> {
        ensure!(!self.inner.cancel.is_cancelled(), "the lab has shut down");
        let helper = bootstrap.0;
        let id = helper.next_session.fetch_add(1, Ordering::Relaxed);
        Ok(Podman {
            session: Arc::new(Session {
                helper,
                lab: self.clone(),
                id,
            }),
        })
    }
}

impl Podman {
    /// Select an image already present in the local Podman store.
    pub fn container(&self, image: impl Into<String>) -> ContainerBuilder<'_> {
        ContainerBuilder {
            session: &self.session,
            spec: ContainerSpec {
                image: image.into(),
                entrypoint: None,
                args: Vec::new(),
            },
        }
    }
}

/// Builds the image and command to run on a device.
pub struct ContainerBuilder<'a> {
    session: &'a Arc<Session>,
    spec: ContainerSpec,
}

impl ContainerBuilder<'_> {
    /// Override the image's entrypoint with one executable.
    pub fn entrypoint(mut self, executable: impl Into<String>) -> Self {
        self.spec.entrypoint = Some(executable.into());
        self
    }

    /// Append a command argument.
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.spec.args.push(arg.into());
        self
    }

    /// Append command arguments.
    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.spec.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Start the container in the device's existing network namespace.
    ///
    /// Wait for application readiness before sending requests. Dropping this
    /// future can leave an in-flight container running until session cleanup.
    pub async fn spawn_in(self, device: &Device) -> Result<Container> {
        let session = self.session;
        let namespace = device.podman_namespace(&session.lab)?;
        let id = request(
            Arc::clone(session),
            Operation::Start(self.spec),
            Some(namespace.into()),
        )
        .await?;
        Ok(Container {
            session: Arc::downgrade(session),
            id,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct ContainerSpec {
    image: String,
    entrypoint: Option<String>,
    args: Vec<String>,
}

/// A container managed by a lab's Podman session.
#[derive(Debug)]
pub struct Container {
    session: Weak<Session>,
    id: String,
}

impl Container {
    /// The full Podman container ID.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Stop and remove the container before the session ends.
    pub async fn remove(&self) -> Result<()> {
        if let Some(session) = self.session.upgrade() {
            request(session, Operation::Remove(self.id.clone()), None).await?;
        }
        Ok(())
    }
}

struct Helper {
    control: Mutex<OwnedFd>,
    next_session: AtomicU64,
}

impl Helper {
    fn from_connection(connection: Connection) -> Result<Arc<Self>> {
        let Connection { socket, pid } = connection;
        let reaper = std::thread::Builder::new()
            .name("patchbay-podman-wait".into())
            .spawn(move || match bootstrap::wait(pid) {
                Ok(status) if status.exit_status() == Some(0) => {}
                Ok(status) => eprintln!("Podman helper exited with {status:?}"),
                Err(error) => eprintln!("could not wait for Podman helper: {error}"),
            });
        if let Err(error) = reaper {
            drop(socket);
            let _ = bootstrap::wait(pid);
            return Err(error).context("start Podman helper wait thread");
        }
        Ok(Arc::new(Self {
            control: Mutex::new(socket),
            next_session: AtomicU64::new(1),
        }))
    }

    fn exchange(
        &self,
        session: SessionId,
        operation: Operation,
        namespace: Option<BorrowedFd<'_>>,
    ) -> Result<String> {
        let socket = self
            .control
            .lock()
            .map_err(|_| anyhow!("Podman control socket lock poisoned"))?;
        send(&socket, &Request { session, operation }, namespace)?;
        let (reply, fd) = receive::<Reply>(&socket)?.context("Podman helper disconnected")?;
        ensure!(fd.is_none(), "unexpected descriptor in Podman reply");
        reply.map_err(anyhow::Error::msg)
    }
}

fn process_token(result: &std::result::Result<Arc<Helper>, String>) -> Result<PodmanBootstrap> {
    result
        .as_ref()
        .map(|helper| PodmanBootstrap(Arc::clone(helper)))
        .map_err(|error| anyhow::anyhow!("{error}"))
}

/// Enter a Patchbay user namespace with a local Podman helper in its parent.
///
/// Call this once at the start of your main function, before starting any
/// threads. Requires local rootless Podman and `patchbay-podman-helper` on
/// `PATH`. With libtest use [`init_userns_with_podman_for_ctor`] instead.
pub fn init_userns_with_podman() -> Result<PodmanBootstrap> {
    if bootstrap::ctor_initialized() {
        return podman_session_token();
    }
    process_token(PROCESS_HELPER.get_or_init(|| {
        let start = || -> Result<Arc<Helper>> {
            crate::userns::ensure_podman_bootstrap_available()?;
            let helper_path = find_helper()?;
            let helper_path = CString::new(helper_path.as_os_str().as_bytes())?;
            let connection =
                bootstrap::start(&helper_path).map_err(bootstrap::Failure::into_error)?;
            crate::userns::record_podman_bootstrap();
            Helper::from_connection(connection)
        };
        start().map_err(|error| format!("{error:#}"))
    }))
}

/// Set up a Patchbay user namespace and start the Podman helper before libtest.
///
/// Register this in place of the normal Patchbay namespace constructor.
/// After startup, call [`podman_session_token`] to obtain the token or
/// retrieve the startup error. Both Podman and its Patchbay helper must
/// be on `PATH`.
///
/// # Safety
///
/// Call this from a single-threaded ELF constructor, before any other
/// namespace bootstrap.
pub unsafe fn init_userns_with_podman_for_ctor() {
    bootstrap::init_for_ctor();
}

/// Get a fresh token for a lab's Podman session.
///
/// The process must have bootstrapped Podman in its constructor or at the
/// start of `main`. Multiple threads can get tokens for independent labs.
pub fn podman_session_token() -> Result<PodmanBootstrap> {
    if let Some(result) = PROCESS_HELPER.get() {
        return process_token(result);
    }
    ensure!(
        bootstrap::ctor_initialized(),
        "set up Podman with init_userns_with_podman or init_userns_with_podman_for_ctor first"
    );
    process_token(PROCESS_HELPER.get_or_init(|| {
        let connection = bootstrap::take().map_err(|error| format!("{error:#}"))?;
        crate::userns::record_podman_bootstrap();
        Helper::from_connection(connection).map_err(|error| format!("{error:#}"))
    }))
}

fn find_helper() -> Result<PathBuf> {
    use rustix::fs::{accessat, Access, AtFlags, CWD};

    let binary = HELPER_BINARY.to_str().expect("ASCII helper name");
    let path = env::var_os("PATH").context("PATH is unset; cannot find patchbay-podman-helper")?;
    let helper = env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|file| {
            file.is_file() && accessat(CWD, file, Access::EXEC_OK, AtFlags::EACCESS).is_ok()
        })
        .with_context(|| {
            format!("{binary} not found on PATH (install it with the podman feature)")
        })?;
    std::path::absolute(helper).context("resolve Podman helper path")
}

type SessionId = u64;

#[derive(Deserialize, Serialize)]
struct Request {
    session: SessionId,
    operation: Operation,
}

#[derive(Deserialize, Serialize)]
enum Operation {
    Start(ContainerSpec),
    Remove(String),
    Close,
}

type Reply = std::result::Result<String, String>;

async fn request(
    session: Arc<Session>,
    operation: Operation,
    namespace: Option<OwnedFd>,
) -> Result<String> {
    // Keep the session alive until the reply, even if the async caller is cancelled.
    tokio::task::spawn_blocking(move || {
        session
            .helper
            .exchange(session.id, operation, namespace.as_ref().map(AsFd::as_fd))
    })
    .await
    .context("Podman request task failed")?
}

fn send(socket: &OwnedFd, message: &impl Serialize, fd: Option<BorrowedFd<'_>>) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    ensure!(body.len() <= MAX_MESSAGE, "Podman message too large");
    send_packet(socket, &body, fd)
}

fn send_packet(socket: &OwnedFd, body: &[u8], fd: Option<BorrowedFd<'_>>) -> Result<()> {
    let mut storage = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut ancillary = SendAncillaryBuffer::new(&mut storage);
    let fds: Vec<_> = fd.into_iter().collect();
    if !fds.is_empty() {
        ensure!(
            ancillary.push(SendAncillaryMessage::ScmRights(&fds)),
            "could not encode namespace descriptor"
        );
    }
    let sent = sendmsg(
        socket,
        &[IoSlice::new(body)],
        &mut ancillary,
        SendFlags::NOSIGNAL,
    )
    .context("send Podman message")?;
    ensure!(sent == body.len(), "short Podman message");
    Ok(())
}

fn receive<T: DeserializeOwned>(socket: &OwnedFd) -> Result<Option<(T, Option<OwnedFd>)>> {
    let mut body = vec![0; MAX_MESSAGE];
    let mut storage = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut ancillary = RecvAncillaryBuffer::new(&mut storage);
    let received = recvmsg(
        socket,
        &mut [IoSliceMut::new(&mut body)],
        &mut ancillary,
        RecvFlags::CMSG_CLOEXEC,
    )
    .context("receive Podman message")?;
    ensure!(
        !received
            .flags
            .intersects(ReturnFlags::CTRUNC | ReturnFlags::TRUNC),
        "truncated Podman message"
    );
    let mut fds = ancillary
        .drain()
        .flat_map(|message| match message {
            RecvAncillaryMessage::ScmRights(fds) => Some(fds),
            _ => None,
        })
        .flatten();
    let fd = fds.next();
    ensure!(fds.next().is_none(), "expected at most one descriptor");
    if received.bytes == 0 {
        return Ok(None);
    }
    Ok(Some((serde_json::from_slice(&body[..received.bytes])?, fd)))
}

/// Internal entry point for `patchbay-podman-helper`.
#[doc(hidden)]
pub fn run_helper(create_namespace: bool) -> Result<()> {
    let uid_map = std::fs::read_to_string("/proc/self/uid_map")?;
    let mut fields = uid_map.split_whitespace();
    ensure!(
        rustix::process::geteuid().is_root()
            && fields.next() == Some("0")
            && fields.next().is_some_and(|uid| uid != "0"),
        "the helper must run under rootless podman unshare"
    );

    // SAFETY: fcntl returns a fresh descriptor or fails for an invalid stdin.
    let fd = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("open Podman control socket");
    }
    // SAFETY: F_DUPFD_CLOEXEC returned a fresh descriptor owned by this call.
    let control = unsafe { OwnedFd::from_raw_fd(fd) };
    ensure!(
        rustix::net::sockopt::socket_type(&control)? == SocketType::SEQPACKET,
        "invalid Podman control socket"
    );
    // SAFETY: Mark the inherited descriptor close-on-exec for Podman children.
    if unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error()).context("protect Podman control socket");
    }
    if create_namespace {
        crate::init_userns().context("create the Patchbay user namespace inside Podman's")?;
        let userns =
            File::open("/proc/self/ns/user").context("open the Patchbay user namespace")?;
        return send_packet(&control, &HELLO, Some(userns.as_fd()));
    }

    let (ours, peer) = socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )?;
    let mut worker = Command::new(env::current_exe()?)
        .arg("--create-userns")
        .stdin(Stdio::from(peer))
        .spawn()
        .context("start the Patchbay namespace setup process")?;
    let namespace = bootstrap::receive_namespace(&ours);
    // Closing the socket lets a late worker exit without sending its descriptor.
    drop(ours);
    if namespace.is_err() {
        let _ = worker.kill();
    }
    let status = worker
        .wait()
        .context("wait for the Patchbay namespace setup process")?;
    let namespace = namespace.context("receive the mapped Patchbay user namespace")?;
    ensure!(
        status.success(),
        "Patchbay namespace setup process exited with {status}"
    );
    send_packet(&control, &HELLO, Some(namespace.as_fd()))?;
    drop(namespace);

    let mut containers = HashMap::new();
    let served = serve(&control, &mut containers);
    let _ = shutdown(&control, Shutdown::Both);
    let cleanup = cleanup(&mut containers, None);
    served?;
    cleanup
}

struct ManagedContainer {
    session: SessionId,
    // Preserve the /proc descriptor path used by Podman through removal.
    _namespace: OwnedFd,
}

fn cleanup(
    containers: &mut HashMap<String, ManagedContainer>,
    session: Option<SessionId>,
) -> Result<()> {
    let mut error = None;
    containers.retain(|id, container| {
        if session.is_some_and(|session| session != container.session) {
            return true;
        }
        if let Err(cause) = remove(id) {
            eprintln!("could not remove Podman container {id}: {cause:#}");
            error = Some(cause);
            // Keep the descriptor and retry outstanding removals at helper exit.
            true
        } else {
            false
        }
    });
    error.map_or(Ok(()), Err)
}

fn serve(control: &OwnedFd, containers: &mut HashMap<String, ManagedContainer>) -> Result<()> {
    while let Some((request, namespace)) = receive::<Request>(control)? {
        let session = request.session;
        let result = match (request.operation, namespace) {
            (Operation::Start(spec), Some(namespace)) => {
                start(session, spec, namespace, containers)
            }
            (Operation::Remove(id), None) => {
                if containers
                    .get(&id)
                    .is_some_and(|container| container.session == session)
                {
                    remove(&id).map(|()| {
                        containers.remove(&id);
                        String::new()
                    })
                } else {
                    Ok(String::new())
                }
            }
            (Operation::Close, None) => cleanup(containers, Some(session)).map(|()| String::new()),
            _ => Err(anyhow!("invalid Podman request")),
        };
        let reply: Reply =
            result.map_err(|error| format!("{error:#}").chars().take(8_000).collect());
        if let Err(error) = send(control, &reply, None) {
            // The test can exit while a container is starting.
            if error.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::PIPE) {
                break;
            }
            return Err(error);
        }
    }
    Ok(())
}

fn start(
    session: SessionId,
    spec: ContainerSpec,
    namespace: OwnedFd,
    containers: &mut HashMap<String, ManagedContainer>,
) -> Result<String> {
    ensure!(!spec.image.is_empty(), "container image is empty");
    let path = format!("/proc/{}/fd/{}", std::process::id(), namespace.as_raw_fd());
    let mut command = podman();
    command.args([
        "create",
        "--pull=never",
        "--cap-drop=ALL",
        "--security-opt=no-new-privileges",
        &format!("--network=ns:{path}"),
    ]);
    if let Some(entrypoint) = spec.entrypoint {
        command.arg(format!("--entrypoint={entrypoint}"));
    }
    let output = checked(command.arg("--").arg(spec.image).args(spec.args), "create")?;
    let id = output.trim().to_owned();
    ensure!(
        id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Podman create did not return a container ID"
    );
    containers.insert(
        id.clone(),
        ManagedContainer {
            session,
            _namespace: namespace,
        },
    );
    if let Err(error) = checked(podman().args(["start", &id]), "start") {
        if remove(&id).is_ok() {
            containers.remove(&id);
        }
        return Err(error);
    }
    Ok(id)
}

fn remove(id: &str) -> Result<()> {
    checked(podman().args(["rm", "--force", "--ignore", id]), "remove")?;
    Ok(())
}

fn podman() -> Command {
    let mut command = Command::new("podman");
    command.arg("--remote=false");
    command
}

fn checked(command: &mut Command, operation: &str) -> Result<String> {
    let output = command
        .output()
        .with_context(|| format!("run Podman {operation}"))?;
    if !output.status.success() {
        bail!(
            "Podman {operation} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("invalid Podman output")
}
