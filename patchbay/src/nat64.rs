//! Userspace SIIT (Stateless IP/ICMP Translation) for NAT64.
//!
//! Creates a TUN device and translates IPv6 ↔ IPv4 headers per RFC 6145.
//! Combined with nftables masquerade on the v4 side, this implements
//! stateful NAT64 (RFC 6146).
//!
//! Packet flow (IPv6 client → IPv4 server):
//! ```text
//! Device (fd10::2, dst = 64:ff9b::203.0.113.1)
//!   → route 64:ff9b::/96 dev tun-nat64
//!   → SIIT strips IPv6 header, creates IPv4 (src=pool, dst=203.0.113.1)
//!   → IPv4 routing + nftables masquerade → IX → destination
//! ```

use std::{
    net::{Ipv4Addr, Ipv6Addr},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

use anyhow::{Context, Result};
use tracing::{debug, trace, warn};

/// Well-known NAT64 prefix (RFC 6052).
pub(crate) const NAT64_PREFIX: [u16; 6] = [0x0064, 0xff9b, 0, 0, 0, 0];

/// IPv4 pool address used as source for translated packets.
/// The router's WAN IP handles actual NAPT via nftables masquerade.
pub(crate) const NAT64_V4_POOL: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 64);

// ── TUN device creation ─────────────────────────────────────────────────

// Linux TUN/TAP ioctl constants.
const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
const IFF_TUN: libc::c_short = 0x0001;
const IFF_NO_PI: libc::c_short = 0x1000;

#[repr(C)]
struct IfReq {
    ifr_name: [u8; libc::IFNAMSIZ],
    ifr_flags: libc::c_short,
    _pad: [u8; 22],
}

nix::ioctl_write_ptr_bad!(tunsetiff, TUNSETIFF, IfReq);

/// Creates a TUN device with the given name. Returns the file descriptor.
///
/// The TUN device is created with IFF_NO_PI (no packet info header),
/// so reads/writes are raw IP packets.
pub(crate) fn create_tun(name: &str) -> Result<OwnedFd> {
    let fd = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/net/tun")
        .context("open /dev/net/tun")?;

    let raw_fd = fd.as_raw_fd();

    let mut ifr = IfReq {
        ifr_name: [0u8; libc::IFNAMSIZ],
        ifr_flags: IFF_TUN | IFF_NO_PI,
        _pad: [0u8; 22],
    };
    let name_bytes = name.as_bytes();
    let copy_len = name_bytes.len().min(libc::IFNAMSIZ - 1);
    ifr.ifr_name[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

    // SAFETY: raw_fd is a valid open fd to /dev/net/tun, ifr is properly initialized.
    unsafe { tunsetiff(raw_fd, &ifr) }.context("TUNSETIFF ioctl")?;

    // Set non-blocking mode (required for tokio AsyncFd).
    let flags = unsafe { libc::fcntl(raw_fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error()).context("fcntl F_GETFL");
    }
    let ret = unsafe { libc::fcntl(raw_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error()).context("fcntl F_SETFL O_NONBLOCK");
    }

    // Consume the File, take ownership of the raw fd.
    let owned = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    // Prevent the File from closing the fd.
    std::mem::forget(fd);

    Ok(owned)
}

// ── SIIT header translation ─────────────────────────────────────────────

/// Extracts the embedded IPv4 address from the last 32 bits of an IPv6 address
/// that uses the well-known NAT64 prefix `64:ff9b::/96`.
pub(crate) fn extract_v4_from_nat64(addr: Ipv6Addr) -> Option<Ipv4Addr> {
    let segs = addr.segments();
    if segs[0..6] != NAT64_PREFIX {
        return None;
    }
    let octets = addr.octets();
    Some(Ipv4Addr::new(
        octets[12], octets[13], octets[14], octets[15],
    ))
}

/// Embeds an IPv4 address into the NAT64 well-known prefix.
pub(crate) fn embed_v4_in_nat64(v4: Ipv4Addr) -> Ipv6Addr {
    let o = v4.octets();
    Ipv6Addr::new(
        NAT64_PREFIX[0],
        NAT64_PREFIX[1],
        NAT64_PREFIX[2],
        NAT64_PREFIX[3],
        NAT64_PREFIX[4],
        NAT64_PREFIX[5],
        u16::from_be_bytes([o[0], o[1]]),
        u16::from_be_bytes([o[2], o[3]]),
    )
}

// IP protocol numbers.
const IPPROTO_TCP: u8 = 6;
const IPPROTO_UDP: u8 = 17;
const IPPROTO_ICMP: u8 = 1;
const IPPROTO_ICMPV6: u8 = 58;

/// Translates an IPv6 packet to IPv4. Returns None if not translatable.
///
/// Handles TCP, UDP, and ICMPv6 echo → ICMP echo.
pub(crate) fn translate_v6_to_v4(pkt: &[u8], v4_src: Ipv4Addr) -> Option<Vec<u8>> {
    if pkt.len() < 40 {
        return None; // Too short for IPv6 header.
    }
    let version = pkt[0] >> 4;
    if version != 6 {
        return None;
    }

    let payload_len = u16::from_be_bytes([pkt[4], pkt[5]]) as usize;
    let next_header = pkt[6];
    let hop_limit = pkt[7];

    // Source and destination IPv6 addresses.
    let mut src6_bytes = [0u8; 16];
    let mut dst6_bytes = [0u8; 16];
    src6_bytes.copy_from_slice(&pkt[8..24]);
    dst6_bytes.copy_from_slice(&pkt[24..40]);
    let dst6 = Ipv6Addr::from(dst6_bytes);

    // Extract embedded IPv4 destination from NAT64 prefix.
    let v4_dst = extract_v4_from_nat64(dst6)?;

    // Map next header to IPv4 protocol.
    let (v4_proto, payload) = match next_header {
        IPPROTO_TCP | IPPROTO_UDP => (next_header, &pkt[40..]),
        IPPROTO_ICMPV6 => (IPPROTO_ICMP, &pkt[40..]),
        _ => return None, // Skip extension headers / unsupported protocols.
    };

    if payload.len() < payload_len {
        return None; // Truncated.
    }
    let payload = &payload[..payload_len];

    // Build IPv4 packet.
    let total_len = 20 + payload.len();
    if total_len > 65535 {
        return None;
    }

    let mut out = Vec::with_capacity(total_len);

    // IPv4 header (20 bytes, no options).
    out.push(0x45); // version=4, IHL=5
    out.push(0x00); // DSCP/ECN
    out.extend_from_slice(&(total_len as u16).to_be_bytes()); // total length
    out.extend_from_slice(&[0x00, 0x00]); // identification
    out.extend_from_slice(&[0x40, 0x00]); // flags=DF, fragment offset=0
    out.push(hop_limit.saturating_sub(1).max(1)); // TTL
    out.push(v4_proto); // protocol
    out.extend_from_slice(&[0x00, 0x00]); // header checksum (filled below)
    out.extend_from_slice(&v4_src.octets()); // src
    out.extend_from_slice(&v4_dst.octets()); // dst

    // Compute IPv4 header checksum.
    let hdr_cksum = ipv4_header_checksum(&out[..20]);
    out[10] = (hdr_cksum >> 8) as u8;
    out[11] = (hdr_cksum & 0xff) as u8;

    // Append payload with adjusted checksums.
    match v4_proto {
        IPPROTO_TCP => {
            let mut tcp_payload = payload.to_vec();
            // Recompute TCP checksum with IPv4 pseudo-header.
            if tcp_payload.len() >= 20 {
                // Zero out checksum field.
                tcp_payload[16] = 0;
                tcp_payload[17] = 0;
                let cksum = tcp_udp_checksum_v4(v4_src, v4_dst, IPPROTO_TCP, &tcp_payload);
                tcp_payload[16] = (cksum >> 8) as u8;
                tcp_payload[17] = (cksum & 0xff) as u8;
            }
            out.extend_from_slice(&tcp_payload);
        }
        IPPROTO_UDP => {
            let mut udp_payload = payload.to_vec();
            // Recompute UDP checksum with IPv4 pseudo-header.
            if udp_payload.len() >= 8 {
                udp_payload[6] = 0;
                udp_payload[7] = 0;
                let cksum = tcp_udp_checksum_v4(v4_src, v4_dst, IPPROTO_UDP, &udp_payload);
                // UDP checksum 0 means "no checksum" in IPv4.
                let cksum = if cksum == 0 { 0xffff } else { cksum };
                udp_payload[6] = (cksum >> 8) as u8;
                udp_payload[7] = (cksum & 0xff) as u8;
            }
            out.extend_from_slice(&udp_payload);
        }
        IPPROTO_ICMP => {
            // ICMPv6 echo → ICMPv4 echo. Type 128→8, 129→0.
            let mut icmp = payload.to_vec();
            if icmp.len() >= 4 {
                match icmp[0] {
                    128 => icmp[0] = 8, // Echo Request
                    129 => icmp[0] = 0, // Echo Reply
                    _ => return None,   // Only translate echo.
                }
                // Recompute ICMP checksum (no pseudo-header in ICMPv4).
                icmp[2] = 0;
                icmp[3] = 0;
                let cksum = internet_checksum(&icmp);
                icmp[2] = (cksum >> 8) as u8;
                icmp[3] = (cksum & 0xff) as u8;
            }
            out.extend_from_slice(&icmp);
        }
        _ => {
            out.extend_from_slice(payload);
        }
    }

    Some(out)
}

/// Translates an IPv4 packet to IPv6. Returns None if not translatable.
///
/// The source IPv4 is embedded into the NAT64 prefix. The destination
/// must be the NAT64 pool address (or we could accept any v4 dst and
/// map it to the original v6 src, but nftables conntrack handles that).
pub(crate) fn translate_v4_to_v6(pkt: &[u8], v6_dst: Ipv6Addr) -> Option<Vec<u8>> {
    if pkt.len() < 20 {
        return None;
    }
    let version = pkt[0] >> 4;
    if version != 4 {
        return None;
    }
    let ihl = (pkt[0] & 0x0f) as usize * 4;
    if pkt.len() < ihl {
        return None;
    }

    let total_len = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
    if pkt.len() < total_len {
        return None;
    }

    let protocol = pkt[9];
    let ttl = pkt[8];
    let mut src4_bytes = [0u8; 4];
    src4_bytes.copy_from_slice(&pkt[12..16]);
    let src4 = Ipv4Addr::from(src4_bytes);

    let v6_src = embed_v4_in_nat64(src4);
    let payload = &pkt[ihl..total_len];

    // Map protocol.
    let (v6_next_header, translated_payload) = match protocol {
        IPPROTO_TCP => {
            let mut tcp = payload.to_vec();
            if tcp.len() >= 20 {
                tcp[16] = 0;
                tcp[17] = 0;
                let cksum = tcp_udp_checksum_v6(v6_src, v6_dst, IPPROTO_TCP, &tcp);
                tcp[16] = (cksum >> 8) as u8;
                tcp[17] = (cksum & 0xff) as u8;
            }
            (IPPROTO_TCP, tcp)
        }
        IPPROTO_UDP => {
            let mut udp = payload.to_vec();
            if udp.len() >= 8 {
                udp[6] = 0;
                udp[7] = 0;
                let cksum = tcp_udp_checksum_v6(v6_src, v6_dst, IPPROTO_UDP, &udp);
                let cksum = if cksum == 0 { 0xffff } else { cksum };
                udp[6] = (cksum >> 8) as u8;
                udp[7] = (cksum & 0xff) as u8;
            }
            (IPPROTO_UDP, udp)
        }
        IPPROTO_ICMP => {
            let mut icmp = payload.to_vec();
            if icmp.len() >= 4 {
                match icmp[0] {
                    8 => icmp[0] = 128, // Echo Request → ICMPv6
                    0 => icmp[0] = 129, // Echo Reply → ICMPv6
                    _ => return None,
                }
                // ICMPv6 uses a pseudo-header checksum.
                icmp[2] = 0;
                icmp[3] = 0;
                let cksum = tcp_udp_checksum_v6(v6_src, v6_dst, IPPROTO_ICMPV6, &icmp);
                let cksum = if cksum == 0 { 0xffff } else { cksum };
                icmp[2] = (cksum >> 8) as u8;
                icmp[3] = (cksum & 0xff) as u8;
            }
            (IPPROTO_ICMPV6, icmp)
        }
        _ => return None,
    };

    let payload_len = translated_payload.len();
    let mut out = Vec::with_capacity(40 + payload_len);

    // IPv6 header (40 bytes).
    out.push(0x60); // version=6
    out.extend_from_slice(&[0x00, 0x00, 0x00]); // traffic class + flow label
    out.extend_from_slice(&(payload_len as u16).to_be_bytes()); // payload length
    out.push(v6_next_header); // next header
    out.push(ttl.saturating_sub(1).max(1)); // hop limit
    out.extend_from_slice(&v6_src.octets()); // src
    out.extend_from_slice(&v6_dst.octets()); // dst

    out.extend_from_slice(&translated_payload);

    Some(out)
}

// ── Checksum helpers ────────────────────────────────────────────────────

/// Standard internet checksum (RFC 1071).
fn internet_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn ipv4_header_checksum(hdr: &[u8]) -> u16 {
    internet_checksum(hdr)
}

/// TCP/UDP checksum with IPv4 pseudo-header.
fn tcp_udp_checksum_v4(src: Ipv4Addr, dst: Ipv4Addr, proto: u8, payload: &[u8]) -> u16 {
    let mut pseudo = Vec::with_capacity(12 + payload.len());
    pseudo.extend_from_slice(&src.octets());
    pseudo.extend_from_slice(&dst.octets());
    pseudo.push(0);
    pseudo.push(proto);
    pseudo.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    pseudo.extend_from_slice(payload);
    internet_checksum(&pseudo)
}

/// TCP/UDP/ICMPv6 checksum with IPv6 pseudo-header.
fn tcp_udp_checksum_v6(src: Ipv6Addr, dst: Ipv6Addr, next_header: u8, payload: &[u8]) -> u16 {
    let mut pseudo = Vec::with_capacity(40 + payload.len());
    pseudo.extend_from_slice(&src.octets());
    pseudo.extend_from_slice(&dst.octets());
    pseudo.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    pseudo.extend_from_slice(&[0, 0, 0, next_header]);
    pseudo.extend_from_slice(payload);
    internet_checksum(&pseudo)
}

// ── Translation loop ────────────────────────────────────────────────────

/// Runs the SIIT translation loop on the given TUN fd.
///
/// Reads packets from the TUN device, translates them, and writes
/// the translated packet back to the same TUN device. The kernel
/// routing table determines which direction the packet goes:
/// - IPv6 packets matching `64:ff9b::/96` arrive here and leave as IPv4
/// - IPv4 packets destined for the NAT64 pool arrive here and leave as IPv6
///
/// The `v4_src` is the pool address used as IPv4 source for v6→v4 translation.
/// The `v6_pool_lookup` maps IPv4 destinations back to their original IPv6
/// source (for the return path). In practice, nftables conntrack handles this
/// transparently — the return v4 packet's dst will be `v4_src` and we translate
/// it to `v6_dst` which is the device that sent the original v6 packet.
///
/// Since we don't have conntrack state in userspace, we use a simple mapping:
/// for v4→v6, the dst IPv4 must be our pool address, and we need to know the
/// original v6 destination. We handle this by keeping a small LRU map of
/// recent translations.
pub(crate) async fn run_nat64_loop(
    tun_fd: OwnedFd,
    v4_src: Ipv4Addr,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    let tun = tokio::io::unix::AsyncFd::new(tun_fd).context("AsyncFd on TUN")?;
    let translations: Arc<Mutex<HashMap<(Ipv4Addr, u16), Ipv6Addr>>> =
        Arc::new(Mutex::new(HashMap::new()));

    debug!("nat64: translation loop starting");

    let mut buf = vec![0u8; 65536];

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                debug!("nat64: cancelled");
                return Ok(());
            }
            readable = tun.readable() => {
                let mut guard = readable.context("TUN readable")?;
                match guard.try_io(|inner| {
                    let fd = inner.as_raw_fd();
                    // SAFETY: fd is valid, buf is properly sized.
                    let n = unsafe {
                        libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                    };
                    if n < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                }) {
                    Ok(Ok(0)) => continue,
                    Ok(Ok(n)) => {
                        let pkt = &buf[..n];
                        let version = if n > 0 { pkt[0] >> 4 } else { 0 };

                        match version {
                            6 => {
                                // IPv6 → IPv4 translation.
                                if let Some(v4_pkt) = translate_v6_to_v4(pkt, v4_src) {
                                    // Remember the mapping for return traffic.
                                    // Extract v6 src and (proto, src_port) for the return lookup.
                                    if pkt.len() >= 40 {
                                        let mut src6 = [0u8; 16];
                                        src6.copy_from_slice(&pkt[8..24]);
                                        let v6_src_addr = Ipv6Addr::from(src6);

                                        // Extract dst IPv4 for the mapping key.
                                        let mut dst4 = [0u8; 4];
                                        dst4.copy_from_slice(&pkt[36..40]);
                                        // Use src port as part of the key.
                                        let src_port = if pkt.len() >= 42 {
                                            u16::from_be_bytes([pkt[40], pkt[41]])
                                        } else {
                                            0
                                        };

                                        let dst4_addr = Ipv4Addr::from(dst4);
                                        translations.lock().unwrap()
                                            .insert((dst4_addr, src_port), v6_src_addr);
                                    }

                                    // Write translated packet back to TUN.
                                    write_to_tun(&tun, &v4_pkt).await;
                                    trace!("nat64: v6→v4 translated {} bytes → {} bytes", n, v4_pkt.len());
                                }
                            }
                            4 => {
                                // IPv4 → IPv6 translation (return traffic).
                                if pkt.len() >= 20 {
                                    let ihl = (pkt[0] & 0x0f) as usize * 4;
                                    // The IPv4 dst should be our pool address.
                                    let mut dst4 = [0u8; 4];
                                    dst4.copy_from_slice(&pkt[16..20]);
                                    let dst4_addr = Ipv4Addr::from(dst4);

                                    if dst4_addr == v4_src {
                                        // Look up the original v6 destination.
                                        // Use dst port (which was the original src port).
                                        let dst_port = if pkt.len() >= ihl + 4 {
                                            u16::from_be_bytes([pkt[ihl + 2], pkt[ihl + 3]])
                                        } else {
                                            0
                                        };

                                        // src IPv4 of the return packet.
                                        let mut src4 = [0u8; 4];
                                        src4.copy_from_slice(&pkt[12..16]);
                                        let src4_addr = Ipv4Addr::from(src4);

                                        let v6_dst = translations.lock().unwrap()
                                            .get(&(src4_addr, dst_port))
                                            .copied();

                                        if let Some(v6_dst) = v6_dst {
                                            if let Some(v6_pkt) = translate_v4_to_v6(pkt, v6_dst) {
                                                write_to_tun(&tun, &v6_pkt).await;
                                                trace!("nat64: v4→v6 translated {} bytes → {} bytes", n, v6_pkt.len());
                                            }
                                        } else {
                                            trace!("nat64: no mapping for return pkt from {src4_addr}:{dst_port}");
                                        }
                                    }
                                }
                            }
                            _ => {
                                trace!("nat64: ignoring non-IP packet (version={})", version);
                            }
                        }
                    }
                    Ok(Err(e)) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Ok(Err(e)) => {
                        warn!("nat64: TUN read error: {e}");
                    }
                    Err(_would_block) => continue,
                }
            }
        }
    }
}

async fn write_to_tun(tun: &tokio::io::unix::AsyncFd<OwnedFd>, data: &[u8]) {
    loop {
        match tun.writable().await {
            Ok(mut guard) => {
                match guard.try_io(|inner| {
                    let fd = inner.as_raw_fd();
                    // SAFETY: fd is valid, data is properly sized.
                    let n = unsafe {
                        libc::write(fd, data.as_ptr() as *const libc::c_void, data.len())
                    };
                    if n < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                }) {
                    Ok(Ok(_)) => return,
                    Ok(Err(e)) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Ok(Err(e)) => {
                        warn!("nat64: TUN write error: {e}");
                        return;
                    }
                    Err(_would_block) => continue,
                }
            }
            Err(e) => {
                warn!("nat64: TUN writable wait error: {e}");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_extract_roundtrip() {
        let v4 = Ipv4Addr::new(203, 0, 113, 1);
        let v6 = embed_v4_in_nat64(v4);
        assert_eq!(v6.to_string(), "64:ff9b::cb00:7101");
        assert_eq!(extract_v4_from_nat64(v6), Some(v4));
    }

    #[test]
    fn extract_non_nat64_returns_none() {
        let v6 = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
        assert_eq!(extract_v4_from_nat64(v6), None);
    }

    #[test]
    fn translate_udp_v6_to_v4() {
        // Build a minimal IPv6/UDP packet.
        let src6 = Ipv6Addr::new(0xfd10, 0, 0, 0, 0, 0, 0, 2);
        let dst6 = embed_v4_in_nat64(Ipv4Addr::new(10, 0, 0, 1));
        let v4_src = Ipv4Addr::new(192, 0, 2, 64);

        // UDP payload: src_port=1234, dst_port=5000, len=12, cksum=0, data="hi"
        let mut udp = vec![0u8; 10];
        udp[0..2].copy_from_slice(&1234u16.to_be_bytes()); // src port
        udp[2..4].copy_from_slice(&5000u16.to_be_bytes()); // dst port
        udp[4..6].copy_from_slice(&10u16.to_be_bytes()); // length
                                                         // checksum will be computed
        udp[8] = b'h';
        udp[9] = b'i';

        // Compute proper UDP checksum for the IPv6 packet.
        udp[6] = 0;
        udp[7] = 0;
        let cksum = tcp_udp_checksum_v6(src6, dst6, IPPROTO_UDP, &udp);
        let cksum = if cksum == 0 { 0xffff } else { cksum };
        udp[6] = (cksum >> 8) as u8;
        udp[7] = (cksum & 0xff) as u8;

        let mut pkt = Vec::with_capacity(40 + udp.len());
        // IPv6 header
        pkt.push(0x60);
        pkt.extend_from_slice(&[0, 0, 0]);
        pkt.extend_from_slice(&(udp.len() as u16).to_be_bytes());
        pkt.push(IPPROTO_UDP);
        pkt.push(64); // hop limit
        pkt.extend_from_slice(&src6.octets());
        pkt.extend_from_slice(&dst6.octets());
        pkt.extend_from_slice(&udp);

        let v4_pkt = translate_v6_to_v4(&pkt, v4_src).expect("translation should succeed");
        assert_eq!(v4_pkt[0] >> 4, 4, "should be IPv4");
        assert_eq!(v4_pkt[9], IPPROTO_UDP, "should be UDP");
        // Dst should be 10.0.0.1
        assert_eq!(&v4_pkt[16..20], &[10, 0, 0, 1]);
        // Src should be our pool address.
        assert_eq!(&v4_pkt[12..16], &v4_src.octets());
    }

    #[test]
    fn translate_udp_v4_to_v6() {
        let src4 = Ipv4Addr::new(10, 0, 0, 1);
        let v6_dst = Ipv6Addr::new(0xfd10, 0, 0, 0, 0, 0, 0, 2);

        // Build minimal IPv4/UDP packet.
        let mut udp = vec![0u8; 10];
        udp[0..2].copy_from_slice(&5000u16.to_be_bytes());
        udp[2..4].copy_from_slice(&1234u16.to_be_bytes());
        udp[4..6].copy_from_slice(&10u16.to_be_bytes());
        udp[8] = b'h';
        udp[9] = b'i';

        let dst4 = NAT64_V4_POOL;
        udp[6] = 0;
        udp[7] = 0;
        let cksum = tcp_udp_checksum_v4(src4, dst4, IPPROTO_UDP, &udp);
        let cksum = if cksum == 0 { 0xffff } else { cksum };
        udp[6] = (cksum >> 8) as u8;
        udp[7] = (cksum & 0xff) as u8;

        let total_len = 20 + udp.len();
        let mut pkt = vec![0u8; total_len];
        pkt[0] = 0x45;
        pkt[2..4].copy_from_slice(&(total_len as u16).to_be_bytes());
        pkt[6] = 0x40; // DF
        pkt[8] = 64; // TTL
        pkt[9] = IPPROTO_UDP;
        pkt[12..16].copy_from_slice(&src4.octets());
        pkt[16..20].copy_from_slice(&dst4.octets());
        let hdr_cksum = ipv4_header_checksum(&pkt[..20]);
        pkt[10] = (hdr_cksum >> 8) as u8;
        pkt[11] = (hdr_cksum & 0xff) as u8;
        pkt[20..].copy_from_slice(&udp);

        let v6_pkt = translate_v4_to_v6(&pkt, v6_dst).expect("translation should succeed");
        assert_eq!(v6_pkt[0] >> 4, 6, "should be IPv6");
        assert_eq!(v6_pkt[6], IPPROTO_UDP, "should be UDP");
        // Src should be 64:ff9b::10.0.0.1
        let expected_src = embed_v4_in_nat64(src4);
        assert_eq!(&v6_pkt[8..24], &expected_src.octets());
        // Dst should be our v6_dst
        assert_eq!(&v6_pkt[24..40], &v6_dst.octets());
    }
}
