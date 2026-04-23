//! NAT-PMP server (RFC 6886) bound to UDP 5351 inside the router namespace.
//!
//! The server accepts `DetermineExternalAddress` and `MapUdp`/`MapTcp`
//! requests, installs [`MappingKey`](super::registry::MappingKey) entries in
//! the shared registry, and emits nftables DNAT rules through
//! [`super::nft::apply_portmap_rules`]. A request is rejected with
//! `NotAuthorizedOrRefused` when the client's source IP is outside the
//! router's downstream CIDR. Lifetime=0 deletes the matching mapping per
//! RFC 6886 section 3.3.
//!
//! PCP (RFC 6887) shares UDP 5351 with NAT-PMP. The shared socket and
//! dispatch-by-version-byte live in [`super::server`]; this module only
//! handles packets whose version byte is `0`.

use std::{net::Ipv4Addr, num::NonZeroU16, time::Duration};

use tracing::{debug, trace};

use super::registry::{AllocOutcome, MapProto};

/// NAT-PMP wire-format version byte.
pub(crate) const VERSION: u8 = 0;

/// Server port per RFC 6886. Shared with PCP.
pub(crate) const SERVER_PORT: u16 = 5351;

/// Response indicator ORed into the opcode field.
const RESPONSE_INDICATOR: u8 = 0x80;

#[repr(u16)]
#[derive(Clone, Copy, Debug)]
enum ResultCode {
    Success = 0,
    #[allow(dead_code)]
    UnsupportedVersion = 1,
    NotAuthorizedOrRefused = 2,
    OutOfResources = 4,
    UnsupportedOpcode = 5,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::FromRepr)]
enum Opcode {
    DetermineExternalAddress = 0,
    MapUdp = 1,
    MapTcp = 2,
}

/// Parsed NAT-PMP request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Request {
    ExternalAddress,
    Map {
        proto: MapProto,
        local_port: u16,
        external_port: u16,
        lifetime_seconds: u32,
    },
}

impl Request {
    pub(crate) fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < 2 || buf[0] != VERSION {
            return None;
        }
        match Opcode::from_repr(buf[1])? {
            Opcode::DetermineExternalAddress => Some(Request::ExternalAddress),
            op @ (Opcode::MapUdp | Opcode::MapTcp) => {
                if buf.len() < 12 {
                    return None;
                }
                let local_port = u16::from_be_bytes([buf[4], buf[5]]);
                let external_port = u16::from_be_bytes([buf[6], buf[7]]);
                let lifetime_seconds = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
                let proto = if op == Opcode::MapUdp {
                    MapProto::Udp
                } else {
                    MapProto::Tcp
                };
                Some(Request::Map {
                    proto,
                    local_port,
                    external_port,
                    lifetime_seconds,
                })
            }
        }
    }
}

/// Encodes a `DetermineExternalAddress` response.
fn encode_external_response(result: ResultCode, epoch_time: u32, public_ip: Ipv4Addr) -> Vec<u8> {
    let mut buf = Vec::with_capacity(12);
    buf.push(VERSION);
    buf.push(RESPONSE_INDICATOR | Opcode::DetermineExternalAddress as u8);
    buf.extend_from_slice(&(result as u16).to_be_bytes());
    buf.extend_from_slice(&epoch_time.to_be_bytes());
    buf.extend_from_slice(&public_ip.octets());
    buf
}

/// Encodes a `MapUdp`/`MapTcp` response.
fn encode_map_response(
    proto: MapProto,
    result: ResultCode,
    epoch_time: u32,
    private_port: u16,
    external_port: u16,
    lifetime_seconds: u32,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16);
    let op_byte = match proto {
        MapProto::Udp => Opcode::MapUdp as u8,
        MapProto::Tcp => Opcode::MapTcp as u8,
    };
    buf.push(VERSION);
    buf.push(RESPONSE_INDICATOR | op_byte);
    buf.extend_from_slice(&(result as u16).to_be_bytes());
    buf.extend_from_slice(&epoch_time.to_be_bytes());
    buf.extend_from_slice(&private_port.to_be_bytes());
    buf.extend_from_slice(&external_port.to_be_bytes());
    buf.extend_from_slice(&lifetime_seconds.to_be_bytes());
    buf
}

/// Handles a single NAT-PMP request and returns the response bytes.
pub(crate) async fn handle_request(
    ctx: &super::server::ServerContext,
    client_ip: Ipv4Addr,
    packet: &[u8],
) -> Option<Vec<u8>> {
    let request = match Request::decode(packet) {
        Some(r) => r,
        None => {
            trace!(?client_ip, len = packet.len(), "nat-pmp: invalid request");
            return None;
        }
    };

    if !ctx.downstream_cidr.contains(&client_ip) {
        debug!(
            ?client_ip,
            cidr = %ctx.downstream_cidr,
            "nat-pmp: client not on downstream subnet"
        );
        return Some(match request {
            Request::ExternalAddress => encode_external_response(
                ResultCode::NotAuthorizedOrRefused,
                ctx.epoch_time(),
                Ipv4Addr::UNSPECIFIED,
            ),
            Request::Map { proto, .. } => encode_map_response(
                proto,
                ResultCode::NotAuthorizedOrRefused,
                ctx.epoch_time(),
                0,
                0,
                0,
            ),
        });
    }

    match request {
        Request::ExternalAddress => Some(encode_external_response(
            ResultCode::Success,
            ctx.epoch_time(),
            ctx.wan_ip,
        )),
        Request::Map {
            proto,
            local_port,
            external_port,
            lifetime_seconds,
        } => Some(
            handle_map(
                ctx,
                client_ip,
                proto,
                local_port,
                external_port,
                lifetime_seconds,
            )
            .await,
        ),
    }
}

async fn handle_map(
    ctx: &super::server::ServerContext,
    client_ip: Ipv4Addr,
    proto: MapProto,
    local_port: u16,
    external_port: u16,
    requested_lifetime: u32,
) -> Vec<u8> {
    // Local port 0 is an error condition for maps per RFC 6886.
    let local = match NonZeroU16::new(local_port) {
        Some(p) => p,
        None => {
            return encode_map_response(
                proto,
                ResultCode::UnsupportedOpcode,
                ctx.epoch_time(),
                local_port,
                0,
                0,
            );
        }
    };

    // Lifetime 0 is the documented delete semantics.
    if requested_lifetime == 0 {
        let mut registry = ctx.registry.lock().await;
        registry.remove_by_internal(proto, client_ip, local);
        ctx.apply_after_mutation(&mut registry, None).await.ok();
        // RFC 6886 section 3.3: reply with the requested local port,
        // external port 0, lifetime 0.
        return encode_map_response(
            proto,
            ResultCode::Success,
            ctx.epoch_time(),
            local.get(),
            0,
            0,
        );
    }

    let lifetime = Duration::from_secs(requested_lifetime.into()).min(ctx.max_lifetime);
    let preferred = NonZeroU16::new(external_port);

    let mut registry = ctx.registry.lock().await;
    let outcome = registry.allocate(proto, client_ip, local, preferred, lifetime, None);
    let (result, granted_external, granted_lifetime) = match outcome {
        AllocOutcome::Created(ref m) | AllocOutcome::Renewed(ref m) => (
            ResultCode::Success,
            m.external_port.get(),
            lifetime.as_secs() as u32,
        ),
        AllocOutcome::Conflict | AllocOutcome::NoPortsAvailable => {
            (ResultCode::OutOfResources, 0, 0)
        }
    };

    // Apply while still holding the registry lock so two concurrent
    // requests never race their apply calls. `created` tells
    // `apply_after_mutation` which entry to roll back on nft failure.
    let created_key = if let AllocOutcome::Created(ref m) = outcome {
        Some(super::registry::MappingKey {
            proto: m.proto,
            external_port: m.external_port,
        })
    } else {
        None
    };
    let applied = if matches!(outcome, AllocOutcome::Created(_) | AllocOutcome::Renewed(_)) {
        ctx.apply_after_mutation(&mut registry, created_key).await
    } else {
        Ok(())
    };
    drop(registry);

    if applied.is_err() && created_key.is_some() {
        // Apply failed and the mapping was rolled back; report as
        // OutOfResources so the client retries later.
        return encode_map_response(
            proto,
            ResultCode::OutOfResources,
            ctx.epoch_time(),
            local.get(),
            0,
            0,
        );
    }

    encode_map_response(
        proto,
        result,
        ctx.epoch_time(),
        local.get(),
        granted_external,
        granted_lifetime,
    )
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    #[test]
    fn decode_external_address_request() {
        let req = Request::decode(&[0, 0]).unwrap();
        assert_eq!(req, Request::ExternalAddress);
    }

    #[test]
    fn decode_map_udp_request() {
        // version=0, opcode=1, reserved=0,0, local=1234 (0x04D2),
        // external=5678 (0x162E), lifetime=60 (0x3C).
        let packet = [0, 1, 0, 0, 0x04, 0xD2, 0x16, 0x2E, 0, 0, 0, 60];
        let req = Request::decode(&packet).unwrap();
        assert_eq!(
            req,
            Request::Map {
                proto: MapProto::Udp,
                local_port: 1234,
                external_port: 5678,
                lifetime_seconds: 60,
            }
        );
    }

    #[test]
    fn decode_wrong_version_fails() {
        assert!(Request::decode(&[1, 0]).is_none());
    }

    #[test]
    fn decode_short_packet_fails() {
        assert!(Request::decode(&[0]).is_none());
        assert!(Request::decode(&[0, 1, 0, 0]).is_none());
    }

    #[test]
    fn encode_external_response_matches_portmapper_decoder() {
        let buf = encode_external_response(ResultCode::Success, 42, Ipv4Addr::new(1, 2, 3, 4));
        assert_eq!(buf.len(), 12);
        assert_eq!(buf[0], 0);
        assert_eq!(buf[1], 0x80);
        assert_eq!(u16::from_be_bytes([buf[2], buf[3]]), 0);
        assert_eq!(u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]), 42);
        assert_eq!(&buf[8..12], &[1, 2, 3, 4]);
    }

    #[test]
    fn encode_map_response_carries_ports_and_lifetime() {
        let buf = encode_map_response(MapProto::Tcp, ResultCode::Success, 0, 80, 8080, 60);
        assert_eq!(buf.len(), 16);
        assert_eq!(buf[0], 0);
        assert_eq!(buf[1], 0x80 | 2);
        assert_eq!(u16::from_be_bytes([buf[8], buf[9]]), 80);
        assert_eq!(u16::from_be_bytes([buf[10], buf[11]]), 8080);
        assert_eq!(u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]), 60);
    }
}
