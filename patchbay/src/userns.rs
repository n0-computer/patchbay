//! User-namespace bootstrap helpers.
//!
//! The ctor entrypoint is intentionally libc-only so it can run from ELF
//! `.init_array` before Rust stdlib/TLS initialization.

use std::sync::OnceLock;

/// Idempotent user-namespace bootstrap.
///
/// Call at the start of `main()` (before Tokio creates threads) when running as
/// a non-root user.  Uses an internal `OnceLock` so it is safe to call multiple
/// times; subsequent calls are instant no-ops.
pub fn init_userns() -> anyhow::Result<()> {
    static RESULT: OnceLock<Result<(), String>> = OnceLock::new();
    RESULT
        .get_or_init(|| do_bootstrap().map_err(|e| e.to_string()))
        .as_ref()
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Raw libc-only variant safe for ELF `.init_array` / pre-TLS contexts.
///
/// # Safety
///
/// Must only be called from a single-threaded ELF init context (e.g. a
/// `#[ctor::ctor]` function) before the Rust standard library has been
/// initialized.  After that point use [`init_userns`] instead.
pub unsafe fn init_userns_for_ctor() {
    #[cfg(target_os = "linux")]
    unsafe {
        userns_bootstrap_libc();
    }
}

#[cfg(target_os = "linux")]
fn do_bootstrap() -> anyhow::Result<()> {
    use anyhow::Context;
    let uid = nix::unistd::Uid::current().as_raw();
    let gid = nix::unistd::Gid::current().as_raw();

    // Prefer a full subuid/subgid range map when the host is set up for it
    // (newuidmap/newgidmap on PATH plus /etc/subuid,/etc/subgid entries). The
    // extra ids let nested rootless container runtimes chown image files and
    // mount overlay; a single-uid map cannot. Fall back to the single-uid map,
    // which needs no host configuration, when the range is unavailable.
    match RangeParams::detect(uid, gid) {
        Some(range) => range_bootstrap(uid, gid, &range).context("range userns bootstrap")?,
        None => single_uid_bootstrap(uid, gid).context("single-uid userns bootstrap")?,
    }

    if nix::unistd::Uid::effective().is_root() {
        Ok(())
    } else {
        anyhow::bail!("userns bootstrap finished without UID 0 mapping")
    }
}

/// Single-entry map (inner 0 to the current uid/gid). Needs no host setup.
#[cfg(target_os = "linux")]
fn single_uid_bootstrap(uid: u32, gid: u32) -> anyhow::Result<()> {
    use anyhow::Context;
    use nix::sched::{unshare, CloneFlags};
    unshare(CloneFlags::CLONE_NEWUSER).context("unshare(CLONE_NEWUSER) failed")?;
    std::fs::write("/proc/self/setgroups", "deny\n").context("write setgroups")?;
    std::fs::write("/proc/self/uid_map", format!("0 {uid} 1\n")).context("write uid_map")?;
    std::fs::write("/proc/self/gid_map", format!("0 {gid} 1\n")).context("write gid_map")?;
    Ok(())
}

/// Sub-id ranges and helper paths for a full range map.
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
    /// `/etc/subgid` entry for the user. Returns `None` (use the single-uid
    /// fallback) if anything is missing.
    fn detect(uid: u32, gid: u32) -> Option<Self> {
        let newuidmap = find_in_path("newuidmap")?;
        let newgidmap = find_in_path("newgidmap")?;
        let user = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)).ok()??;
        let (sub_uid_start, sub_uid_count) = parse_subid("/etc/subuid", &user.name, uid)?;
        let (sub_gid_start, sub_gid_count) = parse_subid("/etc/subgid", &user.name, gid)?;
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

/// Maps inner 0 to the current uid plus the sub-id range to inner 1.., using the
/// setuid `newuidmap`/`newgidmap` helpers.
///
/// The current process enters the new user namespace, then a short-lived child
/// (still in the parent namespace, where the setuid helpers have authority)
/// writes the range maps against the parent's pid.
#[cfg(target_os = "linux")]
fn range_bootstrap(uid: u32, gid: u32, range: &RangeParams) -> anyhow::Result<()> {
    use std::os::fd::AsFd;

    use anyhow::{bail, Context};
    use nix::{
        sched::{unshare, CloneFlags},
        sys::wait::waitpid,
        unistd::{fork, getpid, pipe, read, write, ForkResult},
    };

    // p2c signals the child once the parent has unshared (and carries the
    // parent pid); c2p carries the child's success byte back.
    let (p2c_r, p2c_w) = pipe().context("pipe")?;
    let (c2p_r, c2p_w) = pipe().context("pipe")?;

    // SAFETY: called single-threaded during bootstrap (before Tokio starts).
    // The child only reads a pid, execs the setuid helpers, and `_exit`s.
    match unsafe { fork() }.context("fork")? {
        ForkResult::Child => {
            drop(p2c_w);
            drop(c2p_r);
            let mut pid_buf = [0u8; 4];
            let ok = read(p2c_r.as_fd(), &mut pid_buf).is_ok_and(|n| n == 4) && {
                let target = u32::from_ne_bytes(pid_buf).to_string();
                run_idmap(
                    &range.newuidmap,
                    &target,
                    uid,
                    range.sub_uid_start,
                    range.sub_uid_count,
                ) && run_idmap(
                    &range.newgidmap,
                    &target,
                    gid,
                    range.sub_gid_start,
                    range.sub_gid_count,
                )
            };
            let _ = write(c2p_w.as_fd(), &[u8::from(ok)]);
            // SAFETY: exit without running atexit handlers in the forked child.
            unsafe { libc::_exit(0) };
        }
        ForkResult::Parent { child } => {
            drop(p2c_r);
            drop(c2p_w);
            unshare(CloneFlags::CLONE_NEWUSER).context("unshare(CLONE_NEWUSER) failed")?;
            let pid = getpid().as_raw() as u32;
            write(p2c_w.as_fd(), &pid.to_ne_bytes()).context("signal child")?;
            let mut status = [0u8; 1];
            let got = read(c2p_r.as_fd(), &mut status).context("await child")?;
            let _ = waitpid(child, None);
            drop(p2c_w);
            if got != 1 || status[0] != 1 {
                bail!("newuidmap/newgidmap failed to write the range map");
            }
            Ok(())
        }
    }
}

/// Runs `helper <pid> 0 <id> 1 1 <sub_start> <sub_count>`, mapping inner 0 to
/// `id` and the sub-id range to inner 1.
#[cfg(target_os = "linux")]
fn run_idmap(helper: &std::path::Path, pid: &str, id: u32, sub_start: u32, sub_count: u32) -> bool {
    std::process::Command::new(helper)
        .args([
            pid,
            "0",
            &id.to_string(),
            "1",
            "1",
            &sub_start.to_string(),
            &sub_count.to_string(),
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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

#[cfg(not(target_os = "linux"))]
fn do_bootstrap() -> anyhow::Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
unsafe fn userns_bootstrap_libc() {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
        return;
    }

    unsafe { proc_write(c"/proc/self/setgroups".as_ptr(), b"deny\n") };

    let mut uid_buf = [0u8; 32];
    let uid_line = format_map_line(&mut uid_buf, uid);
    unsafe { proc_write(c"/proc/self/uid_map".as_ptr(), uid_line) };

    let mut gid_buf = [0u8; 32];
    let gid_line = format_map_line(&mut gid_buf, gid);
    unsafe { proc_write(c"/proc/self/gid_map".as_ptr(), gid_line) };
}

#[cfg(target_os = "linux")]
unsafe fn proc_write(path: *const libc::c_char, data: &[u8]) {
    let fd = unsafe { libc::open(path, libc::O_WRONLY) };
    if fd < 0 {
        return;
    }
    let _ = unsafe { libc::write(fd, data.as_ptr().cast::<libc::c_void>(), data.len()) };
    let _ = unsafe { libc::close(fd) };
}

#[cfg(target_os = "linux")]
fn format_map_line(buf: &mut [u8; 32], id: u32) -> &[u8] {
    buf[0] = b'0';
    buf[1] = b' ';
    let mut pos = 2usize;

    if id == 0 {
        buf[pos] = b'0';
        pos += 1;
    } else {
        let mut n = id;
        let mut rev = [0u8; 12];
        let mut len = 0usize;
        while n > 0 {
            rev[len] = b'0' + (n % 10) as u8;
            n /= 10;
            len += 1;
        }
        while len > 0 {
            len -= 1;
            buf[pos] = rev[len];
            pos += 1;
        }
    }

    buf[pos] = b' ';
    pos += 1;
    buf[pos] = b'1';
    pos += 1;
    buf[pos] = b'\n';
    pos += 1;
    &buf[..pos]
}
