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
        .get_or_init(|| {
            #[cfg(target_os = "linux")]
            if nix::unistd::Uid::effective().is_root() {
                return Ok(());
            }
            do_bootstrap().map_err(|e| e.to_string())
        })
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
    use nix::sched::{unshare, CloneFlags};
    if nix::unistd::Uid::effective().is_root() {
        return Ok(());
    }
    let uid = nix::unistd::Uid::current().as_raw();
    let gid = nix::unistd::Gid::current().as_raw();
    unshare(CloneFlags::CLONE_NEWUSER).context("unshare(CLONE_NEWUSER) failed")?;
    std::fs::write("/proc/self/setgroups", "deny\n").context("write setgroups")?;
    std::fs::write("/proc/self/uid_map", format!("0 {uid} 1\n")).context("write uid_map")?;
    std::fs::write("/proc/self/gid_map", format!("0 {gid} 1\n")).context("write gid_map")?;
    if nix::unistd::Uid::effective().is_root() {
        Ok(())
    } else {
        anyhow::bail!("userns bootstrap finished without UID 0 mapping")
    }
}

#[cfg(not(target_os = "linux"))]
fn do_bootstrap() -> anyhow::Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
unsafe fn userns_bootstrap_libc() {
    let uid = unsafe { libc::getuid() };
    if uid == 0 {
        return;
    }
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
