//! User-namespace bootstrap helpers.
//!
//! The ctor entrypoint is intentionally libc-only so it can run from ELF
//! `.init_array` before Rust stdlib/TLS initialization.

#[cfg(target_os = "linux")]
#[ctor::ctor]
fn userns_bootstrap_ctor() {
    // SAFETY: runs in single-threaded init context and performs only raw libc syscalls.
    unsafe { userns_bootstrap_libc() }
}

#[cfg(target_os = "linux")]
unsafe fn userns_bootstrap_libc() {
    let uid = libc::getuid();
    if uid == 0 {
        return;
    }
    let gid = libc::getgid();
    if libc::unshare(libc::CLONE_NEWUSER) != 0 {
        return;
    }

    proc_write(c"/proc/self/setgroups".as_ptr(), b"deny\n");

    let mut uid_buf = [0u8; 32];
    let uid_line = format_map_line(&mut uid_buf, uid);
    proc_write(c"/proc/self/uid_map".as_ptr(), uid_line);

    let mut gid_buf = [0u8; 32];
    let gid_line = format_map_line(&mut gid_buf, gid);
    proc_write(c"/proc/self/gid_map".as_ptr(), gid_line);
}

#[cfg(target_os = "linux")]
unsafe fn proc_write(path: *const libc::c_char, data: &[u8]) {
    let fd = libc::open(path, libc::O_WRONLY);
    if fd < 0 {
        return;
    }
    let _ = libc::write(fd, data.as_ptr().cast::<libc::c_void>(), data.len());
    let _ = libc::close(fd);
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
