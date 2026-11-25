/// Normalizes strings for filenames and environment variable suffixes.
fn sanitize_with(name: &str, allow_dash: bool) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || (allow_dash && c == '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Produces a safe path component (alphanumeric, `_`, and `-`).
pub fn sanitize_for_path_component(name: &str) -> String {
    sanitize_with(name, true)
}

/// Produces a safe environment variable suffix (alphanumeric + `_`).
pub fn sanitize_for_env_key(name: &str) -> String {
    sanitize_with(name, false)
}

/// Calls `bind` again while it fails with [`ErrorKind::AddrInUse`], for up to
/// one second, then returns its last result.
///
/// Use it when a test closes a socket and binds the same fixed port again.
/// When any thread spawns a child process, the child holds a copy of every
/// open socket until it starts its program. A socket closed in that moment
/// keeps its port bound for a few milliseconds, so the second bind can fail
/// with `EADDRINUSE`. This holds for TCP and UDP alike, and `SO_REUSEADDR`
/// does not help while a copied TCP listener is open.
///
/// Call it in the device's context, for example in `device.run_sync` or in a
/// task from `device.spawn`. The sleep between tries blocks the thread, but
/// it only happens on a retry.
///
/// [`ErrorKind::AddrInUse`]: std::io::ErrorKind::AddrInUse
///
/// # Examples
///
/// ```no_run
/// # fn main() -> std::io::Result<()> {
/// let addr: std::net::SocketAddr = "0.0.0.0:4433".parse().unwrap();
/// let socket = patchbay::util::retry_addr_in_use(|| std::net::UdpSocket::bind(addr))?;
/// # drop(socket);
/// # Ok(())
/// # }
/// ```
pub fn retry_addr_in_use<T>(mut bind: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut backoff = Duration::from_micros(50);
    loop {
        match bind() {
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && Instant::now() < deadline => {
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(10));
            }
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Error, ErrorKind},
        net::UdpSocket,
        time::{Duration, Instant},
    };

    use super::retry_addr_in_use;

    #[test]
    fn retry_addr_in_use_binds_once_the_port_is_free() {
        let held = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = held.local_addr().unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(held);
        });
        let socket = retry_addr_in_use(|| UdpSocket::bind(addr)).unwrap();
        assert_eq!(socket.local_addr().unwrap(), addr);
        release.join().unwrap();
    }

    #[test]
    fn retry_addr_in_use_returns_other_errors_at_once() {
        let mut calls = 0;
        let result: std::io::Result<()> = retry_addr_in_use(|| {
            calls += 1;
            Err(Error::from(ErrorKind::PermissionDenied))
        });
        assert_eq!(result.unwrap_err().kind(), ErrorKind::PermissionDenied);
        assert_eq!(calls, 1);
    }

    #[test]
    fn retry_addr_in_use_gives_up_after_the_deadline() {
        let held = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = held.local_addr().unwrap();
        let start = Instant::now();
        let err = retry_addr_in_use(|| UdpSocket::bind(addr)).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AddrInUse);
        assert!(start.elapsed() >= Duration::from_secs(1));
    }
}
