//! User-namespace bootstrap.
//!
//! [`init_userns`] moves the process into a new user namespace in which it
//! keeps its own uid and gid. If the host has a subordinate id range for the
//! user (`/etc/subuid`, `/etc/subgid`, and the `newuidmap`/`newgidmap`
//! helpers), the namespace also maps uid and gid 0 to the first subordinate
//! id, so commands can run as root on request (see [`Device::root`]), and the
//! rest of the range to itself.
//!
//! [`Device::root`]: crate::Device::root

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
/// `/etc/subgid` have an entry for the user, uid and gid 0 map to the first
/// subordinate id and the rest of the range maps to itself. Commands can then
/// run as root with [`Device::root`](crate::Device::root) and
/// [`Router::root`](crate::Router::root). If the helpers are missing or fail,
/// only the user's own ids are mapped and root is not available.
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
        std::fs::read_to_string(path).is_ok_and(|map| {
            map.lines()
                .any(|line| line.split_whitespace().next() == Some("0"))
        })
    };
    has_zero("/proc/self/uid_map") && has_zero("/proc/self/gid_map")
}

#[cfg(target_os = "linux")]
fn do_bootstrap() -> anyhow::Result<()> {
    use anyhow::Context;
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    match RangeParams::detect(uid, gid) {
        Some(range) => range_bootstrap(uid, gid, &range).context("range userns bootstrap")?,
        None => single_id_bootstrap(uid, gid).context("single-id userns bootstrap")?,
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
    sub_uid_start: u32,
    sub_uid_count: u32,
    sub_gid_start: u32,
    sub_gid_count: u32,
}

#[cfg(target_os = "linux")]
impl RangeParams {
    /// Detects whether a range map is possible for the current user.
    ///
    /// Requires `newuidmap` and `newgidmap` on `PATH` and an `/etc/subuid` and
    /// `/etc/subgid` entry for the user. Returns `None` (use the single-id
    /// map) if anything is missing.
    fn detect(uid: u32, gid: u32) -> Option<Self> {
        let newuidmap = find_in_path("newuidmap")?;
        let newgidmap = find_in_path("newgidmap")?;
        let user = user_name(uid)?;
        let (sub_uid_start, sub_uid_count) = parse_subid("/etc/subuid", &user, uid)?;
        let (sub_gid_start, sub_gid_count) = parse_subid("/etc/subgid", &user, gid)?;
        Some(Self {
            newuidmap,
            newgidmap,
            sub_uid_start,
            sub_uid_count,
            sub_gid_start,
            sub_gid_count,
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
/// writes the range maps against the parent's pid. If the helpers fail, for
/// example because they lack their setuid bit, this falls back to the
/// single-id map, so a broken helper costs the range but not the namespace.
#[cfg(target_os = "linux")]
fn range_bootstrap(uid: u32, gid: u32, range: &RangeParams) -> anyhow::Result<()> {
    use anyhow::Context;
    use rustix::{
        io::{read, write},
        pipe::pipe,
        process::{getpid, waitpid, Pid, WaitOptions},
        thread::UnshareFlags,
    };

    let uid_lines = id_map_lines(uid, range.sub_uid_start, range.sub_uid_count);
    let gid_lines = id_map_lines(gid, range.sub_gid_start, range.sub_gid_count);

    // p2c signals the child once the parent has unshared (and carries the
    // parent pid). c2p carries the child's success byte back.
    let (p2c_r, p2c_w) = pipe().context("pipe")?;
    let (c2p_r, c2p_w) = pipe().context("pipe")?;

    // SAFETY: called single-threaded during bootstrap (before Tokio starts).
    // The child only reads a pid, runs the setuid helpers, and `_exit`s.
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(std::io::Error::last_os_error()).context("fork");
    }
    if child == 0 {
        drop(p2c_w);
        drop(c2p_r);
        let mut pid_buf = [0u8; 4];
        let ok = read(&p2c_r, &mut pid_buf).is_ok_and(|n| n == 4) && {
            let target = u32::from_ne_bytes(pid_buf).to_string();
            run_idmap(&range.newuidmap, &target, &uid_lines)
                && run_idmap(&range.newgidmap, &target, &gid_lines)
        };
        let _ = write(&c2p_w, &[u8::from(ok)]);
        // SAFETY: exit without running atexit handlers in the forked child.
        unsafe { libc::_exit(0) };
    }

    drop(p2c_r);
    drop(c2p_w);
    let mapped = (|| -> anyhow::Result<bool> {
        crate::netns::unshare_namespaces(UnshareFlags::NEWUSER)
            .context("unshare(CLONE_NEWUSER) failed")?;
        let pid = getpid().as_raw_nonzero().get() as u32;
        write(&p2c_w, &pid.to_ne_bytes()).context("signal child")?;
        let mut status = [0u8; 1];
        let got = read(&c2p_r, &mut status).context("await child")?;
        Ok(got == 1 && status[0] == 1)
    })();
    // Closing the pipe unblocks a child that still waits for the pid.
    drop(p2c_w);
    let _ = waitpid(Pid::from_raw(child), WaitOptions::empty());
    if !mapped? {
        tracing::warn!("newuidmap/newgidmap failed, falling back to a single-id user namespace");
        write_single_id_maps(uid, gid).context("fall back to single-id maps")?;
    }
    Ok(())
}

/// Returns the `inner outer count` triples for one id and its subordinate
/// range: the id to itself, 0 to the first subordinate id, and the rest of the
/// range to itself.
#[cfg(target_os = "linux")]
fn id_map_lines(id: u32, sub_start: u32, sub_count: u32) -> Vec<[u32; 3]> {
    let mut lines = vec![[id, id, 1], [0, sub_start, 1]];
    if sub_count > 1 {
        lines.push([sub_start + 1, sub_start + 1, sub_count - 1]);
    }
    lines
}

/// Runs `helper <pid> <inner> <outer> <count>...` with the given map lines.
#[cfg(target_os = "linux")]
fn run_idmap(helper: &std::path::Path, pid: &str, lines: &[[u32; 3]]) -> bool {
    std::process::Command::new(helper)
        .arg(pid)
        .args(lines.iter().flatten().map(u32::to_string))
        .status()
        .is_ok_and(|status| status.success())
}

/// Finds an executable by name on `PATH`.
#[cfg(target_os = "linux")]
fn find_in_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Parses an `/etc/subuid`-style file for the `login:start:count` entry that
/// matches the user by name or numeric id.
#[cfg(target_os = "linux")]
fn parse_subid(path: &str, name: &str, id: u32) -> Option<(u32, u32)> {
    let contents = std::fs::read_to_string(path).ok()?;
    let id_str = id.to_string();
    for line in contents.lines() {
        let mut fields = line.split(':');
        let login = fields.next()?;
        if login != name && login != id_str {
            continue;
        }
        let start = fields.next()?.parse().ok()?;
        let count = fields.next()?.parse().ok()?;
        return Some((start, count));
    }
    None
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
/// Returned by [`Device::root`](crate::Device::root) and
/// [`Router::root`](crate::Router::root). The patchbay process keeps the
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
pub struct AsRoot<'a> {
    lab: &'a crate::lab::LabInner,
    ns: &'a str,
}

impl<'a> AsRoot<'a> {
    pub(crate) fn new(lab: &'a crate::lab::LabInner, ns: &'a str) -> Self {
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
        let rt = self.lab.rt_handle_for(self.ns)?;
        self.lab.netns.run_closure_in(self.ns, move || {
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
        self.lab.netns.run_closure_in(self.ns, move || {
            cmd.spawn().context("spawn command as root in namespace")
        })
    }
}

/// Fails with an explanation if uid or gid 0 is not mapped.
fn ensure_root_mapped() -> anyhow::Result<()> {
    if root_is_mapped() {
        return Ok(());
    }
    anyhow::bail!(
        "root is not mapped in patchbay's user namespace. Running commands as root \
         needs newuidmap/newgidmap on PATH and an /etc/subuid and /etc/subgid entry \
         for the user"
    )
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::id_map_lines;

    #[test]
    fn map_lines_keep_the_id_and_put_root_on_the_range() {
        assert_eq!(
            id_map_lines(1000, 100000, 65536),
            [[1000, 1000, 1], [0, 100000, 1], [100001, 100001, 65535]]
        );
        assert_eq!(
            id_map_lines(1000, 100000, 1),
            [[1000, 1000, 1], [0, 100000, 1]]
        );
    }
}
