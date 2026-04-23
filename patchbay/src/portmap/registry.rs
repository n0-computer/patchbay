//! Shared registry of active port mappings.
//!
//! A single registry per router tracks every mapping granted by any
//! supported protocol. The registry enforces uniqueness of the external port
//! per protocol and deduplicates repeated requests for the same internal
//! socket across protocols: when a client asks for a second mapping to an
//! already-mapped `(internal_ip, internal_port, proto)` the registry hands
//! back the existing external port instead of allocating a fresh one, which
//! matches miniupnpd and Apple Airport behavior.
//!
//! The registry does not touch nftables itself. Callers apply the DNAT rule
//! corresponding to a returned mapping via [`crate::nft`]; revocation is
//! symmetric. The separation keeps the registry fast (pure in-memory) and
//! testable without any root or netns setup.

use std::{
    collections::HashMap,
    net::Ipv4Addr,
    num::NonZeroU16,
    time::{Duration, Instant},
};

/// IP transport protocol a mapping applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum MapProto {
    /// UDP.
    Udp,
    /// TCP.
    Tcp,
}

/// External-side key for a mapping.
///
/// Two protocols cannot simultaneously hold the same `(proto, ext_port)`.
/// Each external slot belongs to exactly one internal socket at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MappingKey {
    pub proto: MapProto,
    pub external_port: NonZeroU16,
}

/// A single active port mapping.
///
/// The registry owns the authoritative copy. Protocol servers read or clone
/// it as needed to form responses.
#[derive(Clone, Debug)]
pub(crate) struct Mapping {
    pub proto: MapProto,
    pub external_port: NonZeroU16,
    pub internal_ip: Ipv4Addr,
    pub internal_port: NonZeroU16,
    /// Absolute deadline at which the mapping expires and is eligible for
    /// removal. Renewals push the deadline forward.
    pub deadline: Instant,
    /// PCP nonce, carried so `Map` renewals and releases authenticate.
    /// `None` for mappings created via NAT-PMP or UPnP.
    pub pcp_nonce: Option<[u8; 12]>,
}

/// Outcome of an allocation request.
#[derive(Debug)]
pub(crate) enum AllocOutcome {
    /// A new mapping was created.
    Created(Mapping),
    /// An existing mapping for this internal socket was renewed and
    /// returned as-is (same external port).
    Renewed(Mapping),
    /// The specific external port requested is held by another internal
    /// client. Callers should translate this into the protocol-specific
    /// conflict error code.
    Conflict,
    /// The server exhausted its external port pool.
    NoPortsAvailable,
}

/// External port pool boundaries.
///
/// Allocations walk `[low, high]` inclusive. Real gateways typically allocate
/// above 1024 to avoid privileged ports; the default mirrors that and stays
/// clear of the Linux ephemeral range default (32768-60999) so that test
/// ports do not collide with kernel-picked ephemerals.
pub(crate) const EXTERNAL_PORT_LOW: u16 = 1024;
pub(crate) const EXTERNAL_PORT_HIGH: u16 = 32767;

/// In-memory mapping registry.
#[derive(Debug, Default)]
pub(crate) struct PortmapRegistry {
    by_external: HashMap<MappingKey, Mapping>,
    by_internal: HashMap<(Ipv4Addr, NonZeroU16, MapProto), NonZeroU16>,
    /// Next port to try. Wraps within the `[LOW, HIGH]` range.
    cursor: u16,
}

impl PortmapRegistry {
    /// Creates an empty registry.
    pub(crate) fn new() -> Self {
        Self {
            by_external: HashMap::new(),
            by_internal: HashMap::new(),
            cursor: EXTERNAL_PORT_LOW,
        }
    }

    /// Returns the current mapping count.
    #[allow(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.by_external.len()
    }

    /// Looks up a mapping by its external slot.
    #[allow(dead_code)]
    pub(crate) fn get(&self, key: MappingKey) -> Option<&Mapping> {
        self.by_external.get(&key)
    }

    /// Iterates over all active mappings in insertion order of the
    /// `by_external` map. The order is not stable across inserts, which is
    /// acceptable: callers treat this as a bulk snapshot (for example on
    /// teardown) rather than a sorted view.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Mapping> {
        self.by_external.values()
    }

    /// Requests a mapping for `(internal_ip, internal_port, proto)`.
    ///
    /// `preferred_external` is the client's wish: `Some(p)` asks for a
    /// specific port, `None` lets the server pick any. `lifetime` is the
    /// requested lease; the deadline is computed as `Instant::now() +
    /// lifetime`. Callers enforce protocol-specific bounds on the lifetime
    /// before calling.
    pub(crate) fn allocate(
        &mut self,
        proto: MapProto,
        internal_ip: Ipv4Addr,
        internal_port: NonZeroU16,
        preferred_external: Option<NonZeroU16>,
        lifetime: Duration,
        pcp_nonce: Option<[u8; 12]>,
    ) -> AllocOutcome {
        let deadline = Instant::now() + lifetime;
        let internal_key = (internal_ip, internal_port, proto);

        // Dedup: re-requesting the same internal socket renews the existing
        // mapping. We still honor preferred_external if it matches; a
        // mismatched preferred_external is not a conflict as long as the
        // current mapping is for the same internal socket.
        if let Some(existing_port) = self.by_internal.get(&internal_key).copied() {
            let key = MappingKey {
                proto,
                external_port: existing_port,
            };
            if let Some(mapping) = self.by_external.get_mut(&key) {
                mapping.deadline = deadline;
                mapping.pcp_nonce = pcp_nonce.or(mapping.pcp_nonce);
                return AllocOutcome::Renewed(mapping.clone());
            }
        }

        // Honor the client's preferred external port if it's free.
        if let Some(ext) = preferred_external {
            let key = MappingKey {
                proto,
                external_port: ext,
            };
            match self.by_external.get(&key) {
                Some(existing) => {
                    let same_client = existing.internal_ip == internal_ip
                        && existing.internal_port == internal_port;
                    if same_client {
                        // Handled above; fall through to renewal.
                    } else {
                        return AllocOutcome::Conflict;
                    }
                }
                None => {
                    let mapping = Mapping {
                        proto,
                        external_port: ext,
                        internal_ip,
                        internal_port,
                        deadline,
                        pcp_nonce,
                    };
                    self.insert(mapping.clone());
                    return AllocOutcome::Created(mapping);
                }
            }
        }

        // Walk the port range looking for a free slot.
        let start = self.cursor.max(EXTERNAL_PORT_LOW);
        let span = (EXTERNAL_PORT_HIGH - EXTERNAL_PORT_LOW + 1) as u32;
        for i in 0..span {
            let raw =
                EXTERNAL_PORT_LOW + ((start as u32 - EXTERNAL_PORT_LOW as u32 + i) % span) as u16;
            let candidate = NonZeroU16::new(raw).expect("port range excludes zero");
            let key = MappingKey {
                proto,
                external_port: candidate,
            };
            if !self.by_external.contains_key(&key) {
                let next = raw.checked_add(1).unwrap_or(EXTERNAL_PORT_LOW);
                self.cursor = if next > EXTERNAL_PORT_HIGH {
                    EXTERNAL_PORT_LOW
                } else {
                    next
                };
                let mapping = Mapping {
                    proto,
                    external_port: candidate,
                    internal_ip,
                    internal_port,
                    deadline,
                    pcp_nonce,
                };
                self.insert(mapping.clone());
                return AllocOutcome::Created(mapping);
            }
        }

        AllocOutcome::NoPortsAvailable
    }

    /// Removes a mapping by external key. Returns the removed [`Mapping`],
    /// or `None` if no such mapping existed.
    pub(crate) fn remove(&mut self, key: MappingKey) -> Option<Mapping> {
        let mapping = self.by_external.remove(&key)?;
        let internal_key = (mapping.internal_ip, mapping.internal_port, mapping.proto);
        self.by_internal.remove(&internal_key);
        Some(mapping)
    }

    /// Removes all mappings matching `(internal_ip, internal_port, proto)`.
    /// Used when a client sends a NAT-PMP delete with `lifetime=0`.
    pub(crate) fn remove_by_internal(
        &mut self,
        proto: MapProto,
        internal_ip: Ipv4Addr,
        internal_port: NonZeroU16,
    ) -> Option<Mapping> {
        let internal_key = (internal_ip, internal_port, proto);
        let ext_port = self.by_internal.remove(&internal_key)?;
        self.by_external.remove(&MappingKey {
            proto,
            external_port: ext_port,
        })
    }

    /// Removes and returns every mapping whose deadline is at or before
    /// `now`. Called periodically by the reaper.
    #[allow(dead_code)]
    pub(crate) fn reap_expired(&mut self, now: Instant) -> Vec<Mapping> {
        let expired_keys: Vec<MappingKey> = self
            .by_external
            .iter()
            .filter(|(_, m)| m.deadline <= now)
            .map(|(k, _)| *k)
            .collect();
        expired_keys
            .into_iter()
            .filter_map(|k| self.remove(k))
            .collect()
    }

    /// Drains every mapping. Used on server shutdown.
    #[allow(dead_code)]
    pub(crate) fn drain(&mut self) -> Vec<Mapping> {
        self.by_internal.clear();
        self.by_external.drain().map(|(_, m)| m).collect()
    }

    fn insert(&mut self, mapping: Mapping) {
        let key = MappingKey {
            proto: mapping.proto,
            external_port: mapping.external_port,
        };
        let internal_key = (mapping.internal_ip, mapping.internal_port, mapping.proto);
        self.by_internal.insert(internal_key, mapping.external_port);
        self.by_external.insert(key, mapping);
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    fn nz(p: u16) -> NonZeroU16 {
        NonZeroU16::new(p).unwrap()
    }

    fn lifetime() -> Duration {
        Duration::from_secs(60)
    }

    #[test]
    fn allocate_picks_a_free_port() {
        let mut reg = PortmapRegistry::new();
        let client = Ipv4Addr::new(10, 0, 0, 5);
        let outcome = reg.allocate(MapProto::Udp, client, nz(1234), None, lifetime(), None);
        match outcome {
            AllocOutcome::Created(m) => {
                assert_eq!(m.internal_ip, client);
                assert_eq!(m.internal_port, nz(1234));
                assert!(m.external_port.get() >= EXTERNAL_PORT_LOW);
                assert!(m.external_port.get() <= EXTERNAL_PORT_HIGH);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn allocate_honors_preferred_port() {
        let mut reg = PortmapRegistry::new();
        let client = Ipv4Addr::new(10, 0, 0, 5);
        let outcome = reg.allocate(
            MapProto::Tcp,
            client,
            nz(80),
            Some(nz(8080)),
            lifetime(),
            None,
        );
        match outcome {
            AllocOutcome::Created(m) => assert_eq!(m.external_port, nz(8080)),
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn allocate_dedups_same_internal_socket() {
        let mut reg = PortmapRegistry::new();
        let client = Ipv4Addr::new(10, 0, 0, 5);
        let first = reg.allocate(MapProto::Udp, client, nz(1234), None, lifetime(), None);
        let first_port = match first {
            AllocOutcome::Created(m) => m.external_port,
            _ => panic!("expected Created"),
        };
        let second = reg.allocate(MapProto::Udp, client, nz(1234), None, lifetime(), None);
        match second {
            AllocOutcome::Renewed(m) => assert_eq!(m.external_port, first_port),
            other => panic!("expected Renewed, got {other:?}"),
        }
        assert_eq!(reg.len(), 1, "dedup should not add a second entry");
    }

    #[test]
    fn allocate_rejects_port_held_by_other_client() {
        let mut reg = PortmapRegistry::new();
        let a = Ipv4Addr::new(10, 0, 0, 5);
        let b = Ipv4Addr::new(10, 0, 0, 6);
        let _ = reg.allocate(MapProto::Tcp, a, nz(1111), Some(nz(9000)), lifetime(), None);
        let outcome = reg.allocate(MapProto::Tcp, b, nz(2222), Some(nz(9000)), lifetime(), None);
        assert!(matches!(outcome, AllocOutcome::Conflict));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn allocate_different_protocols_can_share_port() {
        let mut reg = PortmapRegistry::new();
        let client = Ipv4Addr::new(10, 0, 0, 5);
        let udp = reg.allocate(
            MapProto::Udp,
            client,
            nz(50),
            Some(nz(7000)),
            lifetime(),
            None,
        );
        let tcp = reg.allocate(
            MapProto::Tcp,
            client,
            nz(50),
            Some(nz(7000)),
            lifetime(),
            None,
        );
        assert!(
            matches!(udp, AllocOutcome::Created(_)) && matches!(tcp, AllocOutcome::Created(_)),
            "UDP and TCP slots are independent",
        );
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn remove_by_external_drops_both_indices() {
        let mut reg = PortmapRegistry::new();
        let client = Ipv4Addr::new(10, 0, 0, 5);
        let created = match reg.allocate(MapProto::Udp, client, nz(500), None, lifetime(), None) {
            AllocOutcome::Created(m) => m,
            _ => panic!("expected Created"),
        };
        let removed = reg
            .remove(MappingKey {
                proto: MapProto::Udp,
                external_port: created.external_port,
            })
            .expect("mapping was present");
        assert_eq!(removed.internal_port, nz(500));
        assert!(reg.by_internal.is_empty());
    }

    #[test]
    fn remove_by_internal_drops_same_mapping() {
        let mut reg = PortmapRegistry::new();
        let client = Ipv4Addr::new(10, 0, 0, 5);
        let _ = reg.allocate(MapProto::Udp, client, nz(500), None, lifetime(), None);
        let removed = reg
            .remove_by_internal(MapProto::Udp, client, nz(500))
            .expect("mapping was present");
        assert_eq!(removed.internal_ip, client);
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn reap_expired_removes_elapsed_entries() {
        let mut reg = PortmapRegistry::new();
        let client = Ipv4Addr::new(10, 0, 0, 5);
        let _ = reg.allocate(
            MapProto::Udp,
            client,
            nz(500),
            None,
            Duration::from_millis(1),
            None,
        );
        std::thread::sleep(Duration::from_millis(20));
        let reaped = reg.reap_expired(Instant::now());
        assert_eq!(reaped.len(), 1);
        assert_eq!(reg.len(), 0);
    }
}
