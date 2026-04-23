//! PCP server (RFC 6887) sharing UDP 5351 with NAT-PMP.
//!
//! Scope: the `Announce` and `Map` opcodes, client-initiated, unicast.
//! The server-to-client ANNOUNCE multicast on UDP 5350 is not
//! implemented because the `portmapper` client does not consume it. Peer
//! mode is also out of scope.
//!
//! Map requests authenticate with a 12-byte nonce: once a mapping exists
//! for `(client_ip, local_port, proto)`, a subsequent request for the
//! same tuple must present the same nonce or the server replies
//! [`ResultCode::NotAuthorized`]. Lifetime=0 deletes the mapping per RFC
//! 6887 section 15, with the same nonce check.

use std::{
    net::{Ipv4Addr, Ipv6Addr},
    num::NonZeroU16,
    time::Duration,
};

use tracing::{debug, trace};

use super::{
    nft,
    registry::{AllocOutcome, MapProto},
    server::ServerContext,
};

/// PCP wire-format version byte.
pub(crate) const VERSION: u8 = 2;

/// Response indicator ORed into the opcode field.
const RESPONSE_INDICATOR: u8 = 1 << 7;

/// Request/response fixed header size per RFC 6887 section 7.
const HEADER_SIZE: usize = 24;

/// Map opcode data size per RFC 6887 section 11.1:
/// 12 (nonce) + 1 (protocol) + 3 (reserved) + 2 (local port) + 2 (external
/// port) + 16 (external IPv6) = 36 bytes.
const MAP_DATA_SIZE: usize = 36;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::FromRepr)]
enum Opcode {
    Announce = 0,
    Map = 1,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug)]
enum ResultCode {
    Success = 0,
    #[allow(dead_code)]
    UnsuppVersion = 1,
    NotAuthorized = 2,
    #[allow(dead_code)]
    MalformedRequest = 3,
    #[allow(dead_code)]
    UnsuppOpcode = 4,
    NoResources = 8,
    UnsuppProtocol = 9,
    CannotProvideExternal = 11,
    AddressMismatch = 12,
}

/// Protocol byte values from RFC 6887 section 11.1 (IANA protocol
/// numbers). 0 means "all protocols" but patchbay rejects that case.
const PROTO_TCP: u8 = 6;
const PROTO_UDP: u8 = 17;

#[derive(Debug)]
struct MapData {
    nonce: [u8; 12],
    protocol: MapProto,
    local_port: u16,
    external_port: u16,
    #[allow(dead_code)]
    external_address: Ipv6Addr,
}

#[derive(Debug)]
struct Decoded {
    /// Client IP as declared in the PCP header bytes 8-23, IPv4-mapped.
    client_addr: Ipv6Addr,
    request: Request,
}

#[derive(Debug)]
enum Request {
    Announce,
    Map {
        lifetime_seconds: u32,
        data: MapData,
    },
    /// Decoded but unsupported (e.g. protocol=0 for all protocols).
    UnsupportedProtocol {
        data: MapData,
    },
}

/// Decodes a PCP request. Returns `None` on malformed packets so the
/// server can silently drop them per RFC 6887 section 8.2.
fn decode(buf: &[u8]) -> Option<Decoded> {
    if buf.len() < HEADER_SIZE || buf[0] != VERSION {
        return None;
    }
    let opcode = Opcode::from_repr(buf[1])?;
    let lifetime_seconds = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let mut client_bytes = [0u8; 16];
    client_bytes.copy_from_slice(&buf[8..24]);
    let client_addr = Ipv6Addr::from(client_bytes);
    let request = match opcode {
        Opcode::Announce => Request::Announce,
        Opcode::Map => {
            if buf.len() < HEADER_SIZE + MAP_DATA_SIZE {
                return None;
            }
            let data = &buf[HEADER_SIZE..HEADER_SIZE + MAP_DATA_SIZE];
            let mut nonce = [0u8; 12];
            nonce.copy_from_slice(&data[..12]);
            let proto_byte = data[12];
            let local_port = u16::from_be_bytes([data[16], data[17]]);
            let external_port = u16::from_be_bytes([data[18], data[19]]);
            let mut addr_bytes = [0u8; 16];
            addr_bytes.copy_from_slice(&data[20..36]);
            let external_address = Ipv6Addr::from(addr_bytes);
            match proto_byte {
                PROTO_UDP => Request::Map {
                    lifetime_seconds,
                    data: MapData {
                        nonce,
                        protocol: MapProto::Udp,
                        local_port,
                        external_port,
                        external_address,
                    },
                },
                PROTO_TCP => Request::Map {
                    lifetime_seconds,
                    data: MapData {
                        nonce,
                        protocol: MapProto::Tcp,
                        local_port,
                        external_port,
                        external_address,
                    },
                },
                _ => Request::UnsupportedProtocol {
                    data: MapData {
                        nonce,
                        protocol: MapProto::Udp,
                        local_port,
                        external_port,
                        external_address,
                    },
                },
            }
        }
    };
    Some(Decoded {
        client_addr,
        request,
    })
}

/// Encodes a response header. The opcode-specific data follows the
/// fixed 24-byte header.
fn encode_header(
    opcode_byte: u8,
    result: ResultCode,
    lifetime_seconds: u32,
    epoch_time: u32,
) -> [u8; HEADER_SIZE] {
    let mut buf = [0u8; HEADER_SIZE];
    buf[0] = VERSION;
    buf[1] = RESPONSE_INDICATOR | opcode_byte;
    buf[2] = 0;
    buf[3] = result as u8;
    buf[4..8].copy_from_slice(&lifetime_seconds.to_be_bytes());
    buf[8..12].copy_from_slice(&epoch_time.to_be_bytes());
    // bytes 12-23 reserved, already zero.
    buf
}

fn encode_announce_response(result: ResultCode, epoch_time: u32) -> Vec<u8> {
    encode_header(Opcode::Announce as u8, result, 0, epoch_time).to_vec()
}

/// Shape of a `Map` response body. Split out from the encoder so the
/// lint against long argument lists stays happy.
struct MapResponse {
    result: ResultCode,
    lifetime_seconds: u32,
    epoch_time: u32,
    nonce: [u8; 12],
    protocol: MapProto,
    local_port: u16,
    external_port: u16,
    external_address: Ipv4Addr,
}

fn encode_map_response(r: MapResponse) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_SIZE + MAP_DATA_SIZE);
    buf.extend_from_slice(&encode_header(
        Opcode::Map as u8,
        r.result,
        r.lifetime_seconds,
        r.epoch_time,
    ));
    buf.extend_from_slice(&r.nonce);
    buf.push(match r.protocol {
        MapProto::Udp => PROTO_UDP,
        MapProto::Tcp => PROTO_TCP,
    });
    buf.extend_from_slice(&[0u8; 3]); // reserved
    buf.extend_from_slice(&r.local_port.to_be_bytes());
    buf.extend_from_slice(&r.external_port.to_be_bytes());
    buf.extend_from_slice(&r.external_address.to_ipv6_mapped().octets());
    buf
}

/// Handles a single PCP packet. Returns the response bytes or `None`
/// when the packet is malformed and should be silently dropped.
pub(crate) async fn handle_request(
    ctx: &ServerContext,
    client_ip: Ipv4Addr,
    packet: &[u8],
) -> Option<Vec<u8>> {
    let Decoded {
        client_addr,
        request,
    } = match decode(packet) {
        Some(r) => r,
        None => {
            trace!(?client_ip, len = packet.len(), "pcp: invalid request");
            return None;
        }
    };

    let epoch = ctx.epoch_time();

    // RFC 6887 section 8.1: the client address in the PCP header MUST
    // match the packet's source IP (or be its IPv4-mapped form). If it
    // does not, respond with AddressMismatch.
    if client_addr != client_ip.to_ipv6_mapped() {
        debug!(
            ?client_ip,
            ?client_addr,
            "pcp: client_addr header does not match source IP"
        );
        return Some(match request {
            Request::Announce => encode_announce_response(ResultCode::AddressMismatch, epoch),
            Request::Map { data, .. } | Request::UnsupportedProtocol { data, .. } => {
                encode_map_response(MapResponse {
                    result: ResultCode::AddressMismatch,
                    lifetime_seconds: 0,
                    epoch_time: epoch,
                    nonce: data.nonce,
                    protocol: data.protocol,
                    local_port: data.local_port,
                    external_port: 0,
                    external_address: Ipv4Addr::UNSPECIFIED,
                })
            }
        });
    }

    if !ctx.downstream_cidr.contains(&client_ip) {
        debug!(?client_ip, cidr = %ctx.downstream_cidr, "pcp: client not on downstream subnet");
        return Some(match request {
            Request::Announce => encode_announce_response(ResultCode::NotAuthorized, epoch),
            Request::Map { data, .. } | Request::UnsupportedProtocol { data, .. } => {
                encode_map_response(MapResponse {
                    result: ResultCode::NotAuthorized,
                    lifetime_seconds: 0,
                    epoch_time: epoch,
                    nonce: data.nonce,
                    protocol: data.protocol,
                    local_port: data.local_port,
                    external_port: 0,
                    external_address: Ipv4Addr::UNSPECIFIED,
                })
            }
        });
    }

    match request {
        Request::Announce => Some(encode_announce_response(ResultCode::Success, epoch)),
        Request::UnsupportedProtocol { data, .. } => Some(encode_map_response(MapResponse {
            result: ResultCode::UnsuppProtocol,
            lifetime_seconds: 0,
            epoch_time: epoch,
            nonce: data.nonce,
            protocol: data.protocol,
            local_port: data.local_port,
            external_port: 0,
            external_address: Ipv4Addr::UNSPECIFIED,
        })),
        Request::Map {
            lifetime_seconds,
            data,
        } => Some(handle_map(ctx, client_ip, lifetime_seconds, data).await),
    }
}

async fn handle_map(
    ctx: &ServerContext,
    client_ip: Ipv4Addr,
    lifetime_seconds: u32,
    data: MapData,
) -> Vec<u8> {
    let epoch = ctx.epoch.elapsed().as_secs() as u32;

    let local = match NonZeroU16::new(data.local_port) {
        Some(p) => p,
        None => {
            return encode_map_response(MapResponse {
                result: ResultCode::CannotProvideExternal,
                lifetime_seconds: 0,
                epoch_time: epoch,
                nonce: data.nonce,
                protocol: data.protocol,
                local_port: data.local_port,
                external_port: 0,
                external_address: Ipv4Addr::UNSPECIFIED,
            });
        }
    };

    // Nonce authentication: if a mapping already exists for this
    // client's (ip, local_port, proto), any new request must carry the
    // same nonce.
    let existing_nonce: Option<[u8; 12]> = {
        let registry = ctx.registry.lock().await;
        let found = registry.iter().find(|m| {
            m.internal_ip == client_ip && m.internal_port == local && m.proto == data.protocol
        });
        found.and_then(|m| m.pcp_nonce)
    };
    if let Some(existing) = existing_nonce {
        if existing != data.nonce {
            return encode_map_response(MapResponse {
                result: ResultCode::NotAuthorized,
                lifetime_seconds: 0,
                epoch_time: epoch,
                nonce: data.nonce,
                protocol: data.protocol,
                local_port: data.local_port,
                external_port: 0,
                external_address: Ipv4Addr::UNSPECIFIED,
            });
        }
    }

    // Lifetime 0 deletes per RFC 6887 section 15.
    if lifetime_seconds == 0 {
        let mut registry = ctx.registry.lock().await;
        registry.remove_by_internal(data.protocol, client_ip, local);
        let rules_snapshot: Vec<_> = registry.iter().cloned().collect();
        drop(registry);
        if let Err(e) =
            nft::apply_portmap_rules(&ctx.netns, &ctx.ns, ctx.wan_ip, &rules_snapshot).await
        {
            tracing::warn!(error = %e, "pcp: apply nft on delete");
        }
        return encode_map_response(MapResponse {
            result: ResultCode::Success,
            lifetime_seconds: 0,
            epoch_time: epoch,
            nonce: data.nonce,
            protocol: data.protocol,
            local_port: data.local_port,
            external_port: 0,
            external_address: ctx.wan_ip,
        });
    }

    let lifetime = Duration::from_secs(lifetime_seconds.into()).min(ctx.max_lifetime);
    let preferred = NonZeroU16::new(data.external_port);

    let mut registry = ctx.registry.lock().await;
    let outcome = registry.allocate(
        data.protocol,
        client_ip,
        local,
        preferred,
        lifetime,
        Some(data.nonce),
    );
    let (result, granted_ext, granted_lifetime) = match outcome {
        AllocOutcome::Created(ref m) | AllocOutcome::Renewed(ref m) => (
            ResultCode::Success,
            m.external_port.get(),
            lifetime.as_secs() as u32,
        ),
        AllocOutcome::Conflict => (ResultCode::CannotProvideExternal, 0, 0),
        AllocOutcome::NoPortsAvailable => (ResultCode::NoResources, 0, 0),
    };
    let rules_snapshot: Vec<_> = registry.iter().cloned().collect();
    drop(registry);

    if matches!(outcome, AllocOutcome::Created(_) | AllocOutcome::Renewed(_)) {
        if let Err(e) =
            nft::apply_portmap_rules(&ctx.netns, &ctx.ns, ctx.wan_ip, &rules_snapshot).await
        {
            tracing::warn!(error = %e, "pcp: apply nft");
        }
    }

    let external_ip = if granted_ext == 0 {
        Ipv4Addr::UNSPECIFIED
    } else {
        ctx.wan_ip
    };

    encode_map_response(MapResponse {
        result,
        lifetime_seconds: granted_lifetime,
        epoch_time: epoch,
        nonce: data.nonce,
        protocol: data.protocol,
        local_port: data.local_port,
        external_port: granted_ext,
        external_address: external_ip,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_announce() {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0] = VERSION;
        buf[1] = Opcode::Announce as u8;
        match decode(&buf).unwrap().request {
            Request::Announce => (),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn decode_map_udp() {
        let mut buf = vec![0u8; HEADER_SIZE + MAP_DATA_SIZE];
        buf[0] = VERSION;
        buf[1] = Opcode::Map as u8;
        buf[4..8].copy_from_slice(&300u32.to_be_bytes());
        // client_addr = 10.0.0.5 IPv4-mapped
        let client = Ipv4Addr::new(10, 0, 0, 5);
        buf[8..24].copy_from_slice(&client.to_ipv6_mapped().octets());
        let data = &mut buf[HEADER_SIZE..];
        data[0..12].copy_from_slice(&[1u8; 12]);
        data[12] = PROTO_UDP;
        data[16..18].copy_from_slice(&1234u16.to_be_bytes());
        data[18..20].copy_from_slice(&5678u16.to_be_bytes());
        let decoded = decode(&buf).unwrap();
        assert_eq!(decoded.client_addr, client.to_ipv6_mapped());
        match decoded.request {
            Request::Map {
                lifetime_seconds,
                data,
            } => {
                assert_eq!(lifetime_seconds, 300);
                assert_eq!(data.protocol, MapProto::Udp);
                assert_eq!(data.local_port, 1234);
                assert_eq!(data.external_port, 5678);
                assert_eq!(data.nonce, [1u8; 12]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn decode_wrong_version_fails() {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0] = 1;
        buf[1] = Opcode::Announce as u8;
        assert!(decode(&buf).is_none());
    }

    #[test]
    fn decode_truncated_map_fails() {
        let mut buf = vec![0u8; HEADER_SIZE + 10];
        buf[0] = VERSION;
        buf[1] = Opcode::Map as u8;
        assert!(decode(&buf).is_none());
    }

    #[test]
    fn decode_unsupported_protocol() {
        let mut buf = vec![0u8; HEADER_SIZE + MAP_DATA_SIZE];
        buf[0] = VERSION;
        buf[1] = Opcode::Map as u8;
        buf[HEADER_SIZE + 12] = 99; // unknown protocol
        match decode(&buf).unwrap().request {
            Request::UnsupportedProtocol { .. } => (),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn encoded_map_response_round_trips_through_portmapper_decoder() {
        let buf = encode_map_response(MapResponse {
            result: ResultCode::Success,
            lifetime_seconds: 300,
            epoch_time: 7,
            nonce: [2u8; 12],
            protocol: MapProto::Tcp,
            local_port: 80,
            external_port: 8080,
            external_address: Ipv4Addr::new(198, 51, 100, 1),
        });
        assert_eq!(buf.len(), HEADER_SIZE + MAP_DATA_SIZE);
        assert_eq!(buf[0], VERSION);
        assert_eq!(buf[1], RESPONSE_INDICATOR | Opcode::Map as u8);
        assert_eq!(buf[3], ResultCode::Success as u8);
        assert_eq!(u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]), 300);
        assert_eq!(&buf[HEADER_SIZE..HEADER_SIZE + 12], &[2u8; 12]);
        assert_eq!(buf[HEADER_SIZE + 12], PROTO_TCP);
    }
}
