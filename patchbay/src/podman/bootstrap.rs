//! Shared bootstrap for main functions and pre-runtime ELF constructors.

use std::{
    ffi::CStr,
    io::IoSliceMut,
    mem::MaybeUninit,
    os::fd::{AsFd, AsRawFd, IntoRawFd, OwnedFd},
    sync::atomic::{AtomicI32, Ordering},
};

use rustix::{
    event::{poll, PollFd, PollFlags, Timespec},
    io::Errno,
    net::{
        recvmsg, shutdown, socketpair, AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage,
        RecvFlags, ReturnFlags, Shutdown, SocketFlags, SocketType,
    },
    process::{waitpid, Pid, WaitOptions, WaitStatus},
    thread::{move_into_link_name_space, LinkNameSpaceType},
};

pub(super) const HELPER_BINARY: &CStr = c"patchbay-podman-helper";
// A fixed frame lets an ELF constructor validate it without deserialization.
pub(super) const HELLO: [u8; 4] = 3u32.to_le_bytes();

const NOT_RUN: i32 = -1;
const TAKEN: i32 = -2;
const ROOT: i32 = -3;
const SOCKET: i32 = -4;
const SPAWN: i32 = -5;
const HANDSHAKE: i32 = -6;
const JOIN: i32 = -7;

// A nonnegative state owns the descriptor. The PID or errno is published
// before the state and can be read after taking the descriptor.
static CTOR_STATE: AtomicI32 = AtomicI32::new(NOT_RUN);
static CTOR_DETAIL: AtomicI32 = AtomicI32::new(0);

pub(super) struct Failure {
    stage: i32,
    errno: Option<Errno>,
}

impl Failure {
    pub(super) fn into_error(self) -> anyhow::Error {
        let message = match self.stage {
            ROOT => "call the Podman bootstrap as a normal user before init_userns",
            SOCKET => "create the Podman control socket",
            SPAWN => "run podman unshare to start patchbay-podman-helper",
            HANDSHAKE => "receive the Patchbay user namespace from the Podman helper (check that the matching helper is on PATH)",
            JOIN => "join the Patchbay user namespace (call the bootstrap before starting threads or entering a user namespace)",
            _ => "Podman constructor bootstrap failed",
        };
        match self.errno {
            Some(errno) => anyhow::Error::new(errno).context(message),
            None => anyhow::anyhow!(message),
        }
    }
}

pub(super) struct Connection {
    pub(super) socket: OwnedFd,
    pub(super) pid: Pid,
}

pub(super) fn start(helper: &CStr) -> Result<Connection, Failure> {
    if rustix::process::geteuid().is_root() {
        return Err(Failure {
            stage: ROOT,
            errno: None,
        });
    }
    let (socket, peer) = socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|errno| Failure {
        stage: SOCKET,
        errno: Some(errno),
    })?;
    let pid = spawn(helper, &peer).map_err(|errno| Failure {
        stage: SPAWN,
        errno: Some(errno),
    })?;
    drop(peer);

    let result = receive_namespace(&socket)
        .map_err(|errno| Failure {
            stage: HANDSHAKE,
            errno: Some(errno),
        })
        .and_then(|namespace| {
            move_into_link_name_space(namespace.as_fd(), Some(LinkNameSpaceType::User)).map_err(
                |errno| Failure {
                    stage: JOIN,
                    errno: Some(errno),
                },
            )
        });
    if let Err(error) = result {
        let _ = shutdown(&socket, Shutdown::Both);
        // The helper exits when its peer closes. Reap it if it has already
        // exited without blocking ELF startup on a broken helper.
        let _ = waitpid(Some(pid), WaitOptions::NOHANG);
        return Err(error);
    }
    Ok(Connection { socket, pid })
}

pub(super) fn init_for_ctor() {
    if CTOR_STATE.load(Ordering::Relaxed) != NOT_RUN {
        return;
    }
    match start(HELPER_BINARY) {
        Ok(Connection { socket, pid }) => {
            CTOR_DETAIL.store(pid.as_raw_pid(), Ordering::Relaxed);
            CTOR_STATE.store(socket.into_raw_fd(), Ordering::Release);
        }
        Err(error) => {
            CTOR_DETAIL.store(
                error.errno.map_or(0, Errno::raw_os_error),
                Ordering::Relaxed,
            );
            CTOR_STATE.store(error.stage, Ordering::Release);
        }
    }
}

pub(super) fn ctor_initialized() -> bool {
    CTOR_STATE.load(Ordering::Acquire) != NOT_RUN
}

pub(super) fn take() -> anyhow::Result<Connection> {
    use std::os::fd::FromRawFd;

    let state = CTOR_STATE.load(Ordering::Acquire);
    match state {
        NOT_RUN => {
            anyhow::bail!("register init_userns_with_podman_for_ctor before taking the token")
        }
        TAKEN => anyhow::bail!("the Podman bootstrap token has already been taken"),
        _ => {}
    }
    CTOR_STATE
        .compare_exchange(state, TAKEN, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| anyhow::anyhow!("the Podman bootstrap token has already been taken"))?;
    match state {
        state if state < 0 => {
            let detail = CTOR_DETAIL.load(Ordering::Relaxed);
            Err(Failure {
                stage: state,
                errno: (detail != 0).then(|| Errno::from_raw_os_error(detail)),
            }
            .into_error())
        }
        fd => {
            // SAFETY: The successful atomic claim transfers the descriptor once.
            let socket = unsafe { OwnedFd::from_raw_fd(fd) };
            let pid = Pid::from_raw(CTOR_DETAIL.load(Ordering::Relaxed))
                .expect("constructor stores a valid helper PID");
            Ok(Connection { socket, pid })
        }
    }
}

fn spawn(helper: &CStr, peer: &OwnedFd) -> rustix::io::Result<Pid> {
    unsafe extern "C" {
        static environ: *const *mut libc::c_char;
    }
    let mut actions = MaybeUninit::uninit();
    // SAFETY: All arguments and file actions remain valid until posix_spawnp
    // returns. The calling process has not started threads.
    unsafe {
        let result = libc::posix_spawn_file_actions_init(actions.as_mut_ptr());
        if result != 0 {
            return Err(Errno::from_raw_os_error(result));
        }
        let mut actions = actions.assume_init();
        let mut result = libc::posix_spawn_file_actions_adddup2(
            &mut actions,
            peer.as_raw_fd(),
            libc::STDIN_FILENO,
        );
        let mut pid = 0;
        if result == 0 {
            let mut argv = [
                c"podman".as_ptr().cast_mut(),
                c"--remote=false".as_ptr().cast_mut(),
                c"unshare".as_ptr().cast_mut(),
                helper.as_ptr().cast_mut(),
                std::ptr::null_mut(),
            ];
            result = libc::posix_spawnp(
                &mut pid,
                c"podman".as_ptr(),
                &actions,
                std::ptr::null(),
                argv.as_mut_ptr(),
                environ,
            );
        }
        libc::posix_spawn_file_actions_destroy(&mut actions);
        if result != 0 {
            return Err(Errno::from_raw_os_error(result));
        }
        Pid::from_raw(pid).ok_or(Errno::CHILD)
    }
}

pub(super) fn receive_namespace(socket: &OwnedFd) -> rustix::io::Result<OwnedFd> {
    let mut ready = [PollFd::new(socket, PollFlags::IN)];
    if poll(
        &mut ready,
        Some(&Timespec {
            tv_sec: 30,
            tv_nsec: 0,
        }),
    )? == 0
    {
        return Err(Errno::TIMEDOUT);
    }
    let mut body = [0; HELLO.len()];
    let mut storage = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = RecvAncillaryBuffer::new(&mut storage);
    let msg = recvmsg(
        socket,
        &mut [IoSliceMut::new(&mut body)],
        &mut control,
        RecvFlags::CMSG_CLOEXEC,
    )?;
    let mut fds = control
        .drain()
        .flat_map(|item| match item {
            RecvAncillaryMessage::ScmRights(fds) => Some(fds),
            _ => None,
        })
        .flatten();
    let fd = fds.next().ok_or(Errno::PROTO)?;
    if msg.bytes != HELLO.len()
        || msg
            .flags
            .intersects(ReturnFlags::CTRUNC | ReturnFlags::TRUNC)
        || body != HELLO
        || fds.next().is_some()
    {
        return Err(Errno::PROTO);
    }
    Ok(fd)
}

pub(super) fn wait(pid: Pid) -> rustix::io::Result<WaitStatus> {
    loop {
        match waitpid(Some(pid), WaitOptions::empty()) {
            Ok(Some((_, status))) => return Ok(status),
            Err(Errno::INTR) => continue,
            Err(error) => return Err(error),
            Ok(None) => return Err(Errno::CHILD),
        }
    }
}
