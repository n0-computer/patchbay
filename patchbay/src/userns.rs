//! User-namespace bootstrap.
//!
//! [`init_userns`] moves the process into a new user namespace in which it
//! keeps its own uid and gid. If the host has a subordinate id range for the
//! user (`/etc/subuid`, `/etc/subgid`, and the `newuidmap`/`newgidmap`
//! helpers), the namespace also maps the ids around the user's onto that
//! range, so uid 0 exists and commands can run as root on request (see
//! [`Device::as_root`]).
//!
//! [`Device::as_root`]: crate::Device::as_root

use std::sync::OnceLock;

/// Capabilities that child processes keep.
///
/// A process that is not uid 0 keeps only its ambient capabilities across
/// `execve`. These keep `nft`, `tc`, `conntrack` and `ping` working.
#[cfg(target_os = "linux")]
const AMBIENT_CAPS: [u32; 3] = [
    10, // CAP_NET_BIND_SERVICE
    12, // CAP_NET_ADMIN
    13, // CAP_NET_RAW
];

/// Why root is not mapped, set by the bootstrap when it maps only the user.
static NO_ROOT_REASON: OnceLock<String> = OnceLock::new();

/// Idempotent user-namespace bootstrap.
///
/// Call at the start of `main()`, or from a `#[ctor::ctor]` function, before
/// any threads exist. Uses an internal `OnceLock`, so it is safe to call more
/// than once. Later calls return the first result.
///
/// The process keeps its uid and gid inside the namespace, so services that
/// check the caller's uid, such as the session D-Bus or PipeWire portals, keep
/// working. The process itself has all capabilities in the namespace. Child
/// processes keep `CAP_NET_ADMIN`, `CAP_NET_RAW` and `CAP_NET_BIND_SERVICE`,
/// which covers `nft`, `tc`, `ping` and low ports.
///
/// If `newuidmap` and `newgidmap` are on `PATH` and `/etc/subuid` and
/// `/etc/subgid` have an entry for the user, the ids from 0 up to the size of
/// the range, except the user's own, map onto that range. Uid 0, `nobody`
/// and the system ids then exist, and commands can run as root with
/// [`Device::as_root`](crate::Device::as_root) and
/// [`Router::as_root`](crate::Router::as_root). Without the range, or if the
/// helpers fail, only the user's own ids are mapped and root is not available.
/// The error of a root spawn then says why.
pub fn init_userns() -> anyhow::Result<()> {
    static RESULT: OnceLock<Result<(), String>> = OnceLock::new();
    RESULT
        .get_or_init(|| do_bootstrap().map_err(|e| format!("{e:#}")))
        .as_ref()
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Returns whether uid and gid 0 are mapped in the current user namespace,
/// so that a child process can switch to root.
pub(crate) fn root_is_mapped() -> bool {
    let has_zero = |path: &str| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|map| parse_id_map(&map))
            .is_some_and(|lines| {
                lines
                    .iter()
                    .any(|&[inner, _, count]| inner == 0 && count > 0)
            })
    };
    has_zero("/proc/self/uid_map") && has_zero("/proc/self/gid_map")
}

/// Parses `/proc/<pid>/{uid,gid}_map` text into `[inner, outer, count]` lines.
fn parse_id_map(text: &str) -> Option<Vec<[u32; 3]>> {
    text.lines()
        .map(|line| {
            let mut fields = line.split_whitespace().map(|field| field.parse().ok());
            let line = [fields.next()??, fields.next()??, fields.next()??];
            fields.next().is_none().then_some(line)
        })
        .collect()
}

/// Whether the process runs in the initial user namespace, whose map covers
/// every id.
#[cfg(target_os = "linux")]
fn in_initial_userns() -> bool {
    std::fs::read_to_string("/proc/self/uid_map")
        .ok()
        .and_then(|map| parse_id_map(&map))
        .is_some_and(|lines| lines == [[0, 0, u32::MAX]])
}

#[cfg(target_os = "linux")]
fn do_bootstrap() -> anyhow::Result<()> {
    use anyhow::Context;
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    match RangeParams::detect(uid, gid) {
        Ok(range) => range_bootstrap(uid, gid, &range).context("range userns bootstrap")?,
        Err(reason) => {
            let _ = NO_ROOT_REASON.set(reason);
            single_id_bootstrap(uid, gid).context("single-id userns bootstrap")?;
        }
    }
    if rustix::process::geteuid().as_raw() != uid {
        anyhow::bail!("userns bootstrap finished without UID {uid} mapping");
    }
    // SAFETY: only makes system calls with pointers to local buffers.
    if unsafe { raise_ambient_caps() } != 0 {
        return Err(std::io::Error::last_os_error()).context("raise ambient capabilities");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn do_bootstrap() -> anyhow::Result<()> {
    Ok(())
}

/// Maps only the current uid and gid, each to itself. Needs no host setup.
#[cfg(target_os = "linux")]
fn single_id_bootstrap(uid: u32, gid: u32) -> anyhow::Result<()> {
    use anyhow::Context;
    use rustix::thread::UnshareFlags;
    crate::netns::unshare_namespaces(UnshareFlags::NEWUSER)
        .context("unshare(CLONE_NEWUSER) failed")?;
    write_single_id_maps(uid, gid)
}

/// Writes single-entry uid and gid maps of the current user namespace.
///
/// Skips a map that is already written, so this also completes a range map
/// that the helpers wrote only in part.
#[cfg(target_os = "linux")]
fn write_single_id_maps(uid: u32, gid: u32) -> anyhow::Result<()> {
    use anyhow::Context;
    let is_empty = |path: &str| std::fs::read_to_string(path).is_ok_and(|map| map.is_empty());
    if is_empty("/proc/self/uid_map") {
        std::fs::write("/proc/self/uid_map", format!("{uid} {uid} 1\n"))
            .context("write uid_map")?;
    }
    if is_empty("/proc/self/gid_map") {
        std::fs::write("/proc/self/setgroups", "deny\n").context("write setgroups")?;
        std::fs::write("/proc/self/gid_map", format!("{gid} {gid} 1\n"))
            .context("write gid_map")?;
    }
    Ok(())
}

/// Sub-id ranges and helper paths for a range map.
#[cfg(target_os = "linux")]
struct RangeParams {
    newuidmap: std::path::PathBuf,
    newgidmap: std::path::PathBuf,
    uid_lines: Vec<[u32; 3]>,
    gid_lines: Vec<[u32; 3]>,
}

#[cfg(target_os = "linux")]
impl RangeParams {
    /// Detects whether a range map is possible for the current user.
    ///
    /// Requires the initial user namespace (the setuid helpers have no effect
    /// in a nested one), a user other than root, `newuidmap` and `newgidmap` on
    /// `PATH`, and an `/etc/subuid` and `/etc/subgid` entry for the user.
    /// Otherwise returns why not, and the caller uses the single-id map.
    fn detect(uid: u32, gid: u32) -> Result<Self, String> {
        if !in_initial_userns() {
            return Err(
                "the process already runs in a nested user namespace, where \
                        newuidmap and newgidmap have no effect"
                    .into(),
            );
        }
        if uid == 0 {
            return Err("the process runs as real root, which needs no subordinate ids".into());
        }
        let need = "newuidmap and newgidmap on PATH and an /etc/subuid and /etc/subgid \
                    entry for the user";
        let newuidmap = find_in_path("newuidmap").ok_or_else(|| format!("needs {need}"))?;
        let newgidmap = find_in_path("newgidmap").ok_or_else(|| format!("needs {need}"))?;
        let user = user_name(uid);
        // shadow keys numeric /etc/subgid owners by uid, not by gid.
        let (sub_uid_start, sub_uid_count) = parse_subid("/etc/subuid", user.as_deref(), uid)
            .ok_or_else(|| format!("needs {need}"))?;
        let (sub_gid_start, sub_gid_count) = parse_subid("/etc/subgid", user.as_deref(), uid)
            .ok_or_else(|| format!("needs {need}"))?;
        let uid_lines = id_map_lines(uid, sub_uid_start, sub_uid_count)
            .ok_or("the /etc/subuid entry is empty or overflows")?;
        let gid_lines = id_map_lines(gid, sub_gid_start, sub_gid_count)
            .ok_or("the /etc/subgid entry is empty or overflows")?;
        Ok(Self {
            newuidmap,
            newgidmap,
            uid_lines,
            gid_lines,
        })
    }
}

/// Returns the login name of `uid` from the passwd database.
#[cfg(target_os = "linux")]
fn user_name(uid: u32) -> Option<String> {
    let mut pwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut result = std::ptr::null_mut();
    // SAFETY: `pwd`, `buf` and `result` are valid for the call. On success
    // `result` points to `pwd`, whose string fields point into `buf`.
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            pwd.as_mut_ptr(),
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() {
        return None;
    }
    // SAFETY: `getpwuid_r` succeeded, so `pw_name` is a NUL-terminated string in `buf`.
    let name = unsafe { std::ffi::CStr::from_ptr((*result).pw_name) };
    name.to_str().ok().map(str::to_owned)
}

/// Maps the current uid and gid plus their subordinate ranges, using the
/// setuid `newuidmap`/`newgidmap` helpers.
///
/// The current process enters the new user namespace, then a short-lived child
/// (still in the parent namespace, where the setuid helpers have authority)
/// writes the range maps against the parent's pid. If anything fails after
/// the unshare, for example because a helper lacks its setuid bit, this falls
/// back to the single-id map, so a broken helper costs root but not the
/// namespace. The helpers' error output is kept for the root spawn error.
#[cfg(target_os = "linux")]
fn range_bootstrap(uid: u32, gid: u32, range: &RangeParams) -> anyhow::Result<()> {
    use anyhow::Context;
    use rustix::{
        io::{read, write},
        pipe::{pipe_with, PipeFlags},
        process::{getpid, waitpid, Pid, WaitOptions},
        thread::UnshareFlags,
    };

    // p2c carries the parent's pid to the child once the parent has unshared.
    // c2p carries the child's success byte and the helpers' error output back.
    // CLOEXEC keeps the pipe ends out of the helpers and any other process.
    let (p2c_r, p2c_w) = pipe_with(PipeFlags::CLOEXEC).context("pipe")?;
    let (c2p_r, c2p_w) = pipe_with(PipeFlags::CLOEXEC).context("pipe")?;

    // SAFETY: called single-threaded during bootstrap (before Tokio starts).
    // The child makes only system calls until it has the pid, and the process
    // has no other threads that could hold a lock it then needs.
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(std::io::Error::last_os_error()).context("fork");
    }
    if child == 0 {
        drop(p2c_w);
        drop(c2p_r);
        let mut pid_buf = [0u8; 4];
        let report = match read(&p2c_r, &mut pid_buf) {
            Ok(4) => {
                let target = u32::from_ne_bytes(pid_buf).to_string();
                run_idmap(&range.newuidmap, &target, &range.uid_lines)
                    .and_then(|()| run_idmap(&range.newgidmap, &target, &range.gid_lines))
            }
            _ => Err(String::new()),
        };
        let message = match &report {
            Ok(()) => vec![1],
            Err(text) => [&[0], text.as_bytes()].concat(),
        };
        let _ = write(&c2p_w, &message);
        // SAFETY: exit without running atexit handlers in the forked child.
        unsafe { libc::_exit(0) };
    }

    drop(p2c_r);
    drop(c2p_w);
    let reap = |p2c_w| {
        // Closing the pipe unblocks a child that still waits for the pid.
        drop(p2c_w);
        let _ = waitpid(Pid::from_raw(child), WaitOptions::empty());
    };
    if let Err(err) = crate::netns::unshare_namespaces(UnshareFlags::NEWUSER) {
        reap(p2c_w);
        return Err(err).context("unshare(CLONE_NEWUSER) failed");
    }
    // From here on the process is in the new namespace, so every failure
    // falls back to the single-id map instead of returning unmapped.
    let pid = getpid().as_raw_nonzero().get() as u32;
    let sent = write(&p2c_w, &pid.to_ne_bytes());
    let mut message = Vec::new();
    if sent.is_ok() {
        let mut buf = [0u8; 512];
        while let Ok(n @ 1..) = read(&c2p_r, &mut buf) {
            message.extend_from_slice(&buf[..n]);
        }
    }
    reap(p2c_w);
    if message.first() == Some(&1) {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(message.get(1..).unwrap_or_default());
    let reason = match detail.trim() {
        "" => "newuidmap/newgidmap failed".to_string(),
        detail => format!("newuidmap/newgidmap failed: {detail}"),
    };
    tracing::warn!("{reason}; falling back to a single-id user namespace");
    let _ = NO_ROOT_REASON.set(reason);
    write_single_id_maps(uid, gid).context("fall back to single-id maps")
}

/// Returns the `inner outer count` triples for one id and its subordinate
/// range of `count` ids from `start`.
///
/// The id maps to itself. The range covers inner ids 0, 1, ... in order,
/// skipping the id, so with uid 1000 and 65536 subordinate ids, inner
/// 0..=999 map to `start..start + 1000` and inner 1001..=65536 to the rest.
/// Returns `None` for an empty or overflowing range.
#[cfg(target_os = "linux")]
fn id_map_lines(id: u32, start: u32, count: u32) -> Option<Vec<[u32; 3]>> {
    if count == 0 {
        return None;
    }
    start.checked_add(count - 1)?;
    let mut lines = vec![[id, id, 1]];
    let below = count.min(id);
    if below > 0 {
        lines.push([0, start, below]);
    }
    if count > below {
        lines.push([id.checked_add(1)?, start + below, count - below]);
    }
    Some(lines)
}

/// Runs `helper <pid> <inner> <outer> <count>...` with the given map lines.
/// Returns the helper's error output on failure.
#[cfg(target_os = "linux")]
fn run_idmap(helper: &std::path::Path, pid: &str, lines: &[[u32; 3]]) -> Result<(), String> {
    let output = std::process::Command::new(helper)
        .arg(pid)
        .args(lines.iter().flatten().map(u32::to_string))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .output()
        .map_err(|err| format!("{}: {err}", helper.display()))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Finds an executable file by name on `PATH`.
#[cfg(target_os = "linux")]
fn find_in_path(name: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

/// Parses an `/etc/subuid`-style file for the `login:start:count` entry that
/// matches the user by name or numeric uid.
#[cfg(target_os = "linux")]
fn parse_subid(path: &str, name: Option<&str>, uid: u32) -> Option<(u32, u32)> {
    let contents = std::fs::read_to_string(path).ok()?;
    let uid = uid.to_string();
    contents.lines().find_map(|line| {
        let mut fields = line.split(':');
        let login = fields.next()?;
        if Some(login) != name && login != uid {
            return None;
        }
        Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
    })
}

/// Adds [`AMBIENT_CAPS`] to the inheritable and ambient sets.
///
/// Returns 0 on success and -1 if a system call failed, with `errno` set.
///
/// # Safety
///
/// Only makes system calls with pointers to local buffers.
#[cfg(target_os = "linux")]
unsafe fn raise_ambient_caps() -> libc::c_long {
    /// `_LINUX_CAPABILITY_VERSION_3`, which takes two data words.
    const VERSION_3: u32 = 0x2008_0522;

    #[repr(C)]
    struct Header {
        version: u32,
        pid: libc::c_int,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    let mut header = Header {
        version: VERSION_3,
        pid: 0,
    };
    let mut data = [Data {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    // SAFETY: for version 3 the kernel writes two `Data` words, which `data` holds.
    if unsafe { libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr()) } != 0 {
        return -1;
    }
    for cap in AMBIENT_CAPS {
        data[(cap / 32) as usize].inheritable |= 1 << (cap % 32);
    }
    // SAFETY: the kernel reads the header and two `Data` words.
    if unsafe { libc::syscall(libc::SYS_capset, &mut header, data.as_ptr()) } != 0 {
        return -1;
    }
    let raise = libc::PR_CAP_AMBIENT_RAISE as libc::c_ulong;
    for cap in AMBIENT_CAPS {
        // SAFETY: prctl with integer arguments touches no memory of this process.
        if unsafe { libc::prctl(libc::PR_CAP_AMBIENT, raise, libc::c_ulong::from(cap), 0, 0) } != 0
        {
            return -1;
        }
    }
    0
}

/// Spawns commands as root (uid 0) in one node's namespace.
///
/// Returned by [`Device::as_root`](crate::Device::as_root),
/// [`Router::as_root`](crate::Router::as_root) and
/// [`Ix::as_root`](crate::Ix::as_root). The patchbay process keeps the
/// user's uid, and its own code already has all capabilities in the
/// namespace. Child processes normally run as the user too, with only the
/// network capabilities. Use this for commands that need more, or that check
/// for uid 0, such as `mount`.
///
/// Root inside the namespace is the user's first subordinate id outside it.
/// Files that a root command creates on the host file system are therefore
/// owned by that id, and the user cannot remove them directly (use
/// `podman unshare rm`). Prefer running commands as the user where they work.
///
/// Spawning fails if the host has no subordinate id range for the user, see
/// [`init_userns`].
#[must_use = "AsRoot does nothing until you spawn a command"]
pub struct AsRoot<'a> {
    lab: &'a crate::lab::LabInner,
    ns: String,
}

impl std::fmt::Debug for AsRoot<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsRoot").field("ns", &self.ns).finish()
    }
}

impl<'a> AsRoot<'a> {
    pub(crate) fn new(lab: &'a crate::lab::LabInner, ns: String) -> Self {
        Self { lab, ns }
    }

    /// Spawns a [`tokio::process::Command`] as root in the node's namespace.
    ///
    /// Works like [`Device::spawn_command`](crate::Device::spawn_command)
    /// otherwise.
    pub fn spawn_command(
        &self,
        mut cmd: tokio::process::Command,
    ) -> anyhow::Result<tokio::process::Child> {
        use anyhow::Context;
        ensure_root_mapped()?;
        cmd.uid(0).gid(0);
        let rt = self.lab.rt_handle_for(&self.ns)?;
        self.lab.netns.run_closure_in(&self.ns, move || {
            let _guard = rt.enter();
            cmd.spawn()
                .context("spawn async command as root in namespace")
        })
    }

    /// Spawns a [`std::process::Command`] as root in the node's namespace.
    ///
    /// Works like [`Device::spawn_command_sync`](crate::Device::spawn_command_sync)
    /// otherwise.
    pub fn spawn_command_sync(
        &self,
        mut cmd: std::process::Command,
    ) -> anyhow::Result<std::process::Child> {
        use std::os::unix::process::CommandExt;

        use anyhow::Context;
        ensure_root_mapped()?;
        cmd.uid(0).gid(0);
        self.lab.netns.run_closure_in(&self.ns, move || {
            cmd.spawn().context("spawn command as root in namespace")
        })
    }
}

/// Fails with an explanation if uid or gid 0 is not mapped.
fn ensure_root_mapped() -> anyhow::Result<()> {
    if root_is_mapped() {
        return Ok(());
    }
    let reason = NO_ROOT_REASON.get().map_or(
        "the user namespace maps only the user's own ids",
        String::as_str,
    );
    anyhow::bail!("root is not mapped in patchbay's user namespace: {reason}")
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{id_map_lines, parse_id_map};

    #[test]
    fn map_lines_keep_the_id_and_fill_zero_up_from_the_range() {
        assert_eq!(
            id_map_lines(1000, 100000, 65536).unwrap(),
            [[1000, 1000, 1], [0, 100000, 1000], [1001, 101000, 64536]]
        );
        // A range smaller than the id covers inner 0.. only.
        assert_eq!(
            id_map_lines(1000, 100000, 10).unwrap(),
            [[1000, 1000, 1], [0, 100000, 10]]
        );
        // An id of 0 would collide with the range.
        assert_eq!(
            id_map_lines(0, 100000, 10).unwrap(),
            [[0, 0, 1], [1, 100000, 10]]
        );
        assert_eq!(id_map_lines(1000, 100000, 0), None);
        assert_eq!(id_map_lines(1000, u32::MAX, 2), None);
    }

    #[test]
    fn parses_padded_id_maps() {
        assert_eq!(
            parse_id_map("      1000       1000          1\n         0     100000       1000\n"),
            Some(vec![[1000, 1000, 1], [0, 100000, 1000]])
        );
        assert_eq!(parse_id_map("0 1000\n"), None);
        assert_eq!(parse_id_map(""), Some(vec![]));
    }
}
