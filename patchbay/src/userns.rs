//! User-namespace bootstrap helpers.
//!
//! The ctor entrypoints are intentionally libc-only so they can run from ELF
//! `.init_array` before Rust stdlib/TLS initialization.

use std::sync::OnceLock;

/// How the calling user is mapped into the new user namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mapping {
    /// Map the calling user and group to 0 (root) inside the namespace.
    Root,
    /// Map the calling user and group to their own ids inside the namespace.
    KeepUid,
}

/// Capabilities that child processes keep in [`Mapping::KeepUid`] mode.
///
/// A process that is not uid 0 keeps only its ambient capabilities across
/// `execve`. These keep `nft`, `tc`, `conntrack` and `ping` working.
#[cfg(target_os = "linux")]
const AMBIENT_CAPS: [u32; 3] = [
    10, // CAP_NET_BIND_SERVICE
    12, // CAP_NET_ADMIN
    13, // CAP_NET_RAW
];

/// The mapping and result of the first bootstrap in this process.
static STATE: OnceLock<(Mapping, Result<(), String>)> = OnceLock::new();

/// Idempotent user-namespace bootstrap.
///
/// Call at the start of `main()` (before Tokio creates threads) when running as
/// a non-root user.  Uses an internal `OnceLock` so it is safe to call multiple
/// times; subsequent calls are instant no-ops.
///
/// Maps the calling user to root (uid 0) inside the namespace. See
/// [`init_userns_keep_uid`] to keep the user's own uid.
pub fn init_userns() -> anyhow::Result<()> {
    init(Mapping::Root)
}

/// Idempotent user-namespace bootstrap that keeps the caller's uid and gid.
///
/// Use this when the process also talks to a service that checks its uid,
/// such as the session D-Bus: under [`init_userns`] a client like `zbus`
/// claims uid 0, and the bus rejects it.
///
/// The process itself keeps all capabilities in the namespace. Child
/// processes are not root and keep only `CAP_NET_ADMIN`, `CAP_NET_RAW` and
/// `CAP_NET_BIND_SERVICE`. That covers `nft`, `tc`, `ping` and low ports,
/// but not commands such as `mount` or ones that check for uid 0.
///
/// Fails if [`init_userns`] already ran in this process.
pub fn init_userns_keep_uid() -> anyhow::Result<()> {
    init(Mapping::KeepUid)
}

fn init(mapping: Mapping) -> anyhow::Result<()> {
    let (done, result) =
        STATE.get_or_init(|| (mapping, do_bootstrap(mapping).map_err(|e| e.to_string())));
    if *done != mapping {
        anyhow::bail!("user namespace already initialized with mapping {done:?}, not {mapping:?}");
    }
    result
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
        userns_bootstrap_libc(Mapping::Root);
    }
}

/// Libc-only variant of [`init_userns_keep_uid`] for ELF `.init_array` contexts.
///
/// # Safety
///
/// Must only be called from a single-threaded ELF init context (e.g. a
/// `#[ctor::ctor]` function) before the Rust standard library has been
/// initialized.  After that point use [`init_userns_keep_uid`] instead.
pub unsafe fn init_userns_keep_uid_for_ctor() {
    #[cfg(target_os = "linux")]
    unsafe {
        userns_bootstrap_libc(Mapping::KeepUid);
    }
}

#[cfg(target_os = "linux")]
fn do_bootstrap(mapping: Mapping) -> anyhow::Result<()> {
    use anyhow::Context;
    use rustix::thread::UnshareFlags;
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    let (inner_uid, inner_gid) = match mapping {
        Mapping::Root => (0, 0),
        Mapping::KeepUid => (uid, gid),
    };
    crate::netns::unshare_namespaces(UnshareFlags::NEWUSER)
        .context("unshare(CLONE_NEWUSER) failed")?;
    std::fs::write("/proc/self/setgroups", "deny\n").context("write setgroups")?;
    std::fs::write("/proc/self/uid_map", format!("{inner_uid} {uid} 1\n"))
        .context("write uid_map")?;
    std::fs::write("/proc/self/gid_map", format!("{inner_gid} {gid} 1\n"))
        .context("write gid_map")?;
    match mapping {
        Mapping::Root if !rustix::process::geteuid().is_root() => {
            anyhow::bail!("userns bootstrap finished without UID 0 mapping")
        }
        Mapping::Root => Ok(()),
        Mapping::KeepUid => {
            if rustix::process::geteuid().as_raw() != uid {
                anyhow::bail!("userns bootstrap finished without UID {uid} mapping");
            }
            // SAFETY: only makes system calls with pointers to local buffers.
            if unsafe { raise_ambient_caps() } != 0 {
                return Err(std::io::Error::last_os_error()).context("raise ambient capabilities");
            }
            Ok(())
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn do_bootstrap(_mapping: Mapping) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
unsafe fn userns_bootstrap_libc(mapping: Mapping) {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let (inner_uid, inner_gid) = match mapping {
        Mapping::Root => (0, 0),
        Mapping::KeepUid => (uid, gid),
    };
    if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
        return;
    }

    unsafe { proc_write(c"/proc/self/setgroups".as_ptr(), b"deny\n") };

    let mut uid_buf = [0u8; 32];
    let uid_line = format_map_line(&mut uid_buf, inner_uid, uid);
    unsafe { proc_write(c"/proc/self/uid_map".as_ptr(), uid_line) };

    let mut gid_buf = [0u8; 32];
    let gid_line = format_map_line(&mut gid_buf, inner_gid, gid);
    unsafe { proc_write(c"/proc/self/gid_map".as_ptr(), gid_line) };

    if mapping == Mapping::KeepUid {
        let _ = unsafe { raise_ambient_caps() };
    }
}

/// Adds [`AMBIENT_CAPS`] to the inheritable and ambient sets.
///
/// Returns 0 on success and -1 if a system call failed, with `errno` set.
///
/// # Safety
///
/// libc-only; safe to call before the Rust standard library is initialized.
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

#[cfg(target_os = "linux")]
unsafe fn proc_write(path: *const libc::c_char, data: &[u8]) {
    let fd = unsafe { libc::open(path, libc::O_WRONLY) };
    if fd < 0 {
        return;
    }
    let _ = unsafe { libc::write(fd, data.as_ptr().cast::<libc::c_void>(), data.len()) };
    let _ = unsafe { libc::close(fd) };
}

/// Formats an id map line `"<inner> <outer> 1\n"` into `buf`.
#[cfg(target_os = "linux")]
fn format_map_line(buf: &mut [u8; 32], inner: u32, outer: u32) -> &[u8] {
    let mut pos = 0usize;
    pos = write_decimal(buf, pos, inner);
    buf[pos] = b' ';
    pos += 1;
    pos = write_decimal(buf, pos, outer);
    buf[pos..pos + 3].copy_from_slice(b" 1\n");
    pos += 3;
    &buf[..pos]
}

/// Writes `n` in decimal at `buf[pos..]` and returns the new position.
#[cfg(target_os = "linux")]
fn write_decimal(buf: &mut [u8; 32], mut pos: usize, n: u32) -> usize {
    if n == 0 {
        buf[pos] = b'0';
        return pos + 1;
    }
    let mut n = n;
    let mut rev = [0u8; 10];
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
    pos
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::format_map_line;

    #[test]
    fn map_line_formats_ids() {
        let mut buf = [0u8; 32];
        assert_eq!(format_map_line(&mut buf, 0, 1000), b"0 1000 1\n");
        assert_eq!(format_map_line(&mut buf, 1000, 1000), b"1000 1000 1\n");
        assert_eq!(
            format_map_line(&mut buf, u32::MAX, u32::MAX),
            b"4294967295 4294967295 1\n"
        );
    }
}
