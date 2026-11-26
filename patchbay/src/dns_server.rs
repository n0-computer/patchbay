//! Minimal in-process authoritative DNS server for the lab.
//!
//! Runs on the IX bridge, serves A/AAAA/TXT records from a [`std::sync::RwLock`]-guarded
//! [`HashMap`](std::collections::HashMap). Record mutations via [`DnsServer::set_host`] / [`DnsServer::set_txt`] are
//! synchronous — the record is visible to DNS queries the instant the method returns.
//!
//! ## Limitations
//!
//! - **No TCP fallback.** Only UDP is supported. The server sends each response
//!   in full, also when it is longer than 512 bytes, and never sets the TC
//!   (truncated) flag. This is fine for lab use with short names.
//! - **No answer for unknown record types.** The server drops a query that
//!   `simple-dns` cannot parse, for example a query for a record type that
//!   `simple-dns` does not know. The client then times out. It does not get an
//!   empty answer.
//! - **FQDN trailing dots.** DNS names passed to [`DnsServer::set_host`] and
//!   [`DnsServer::set_txt`] are always fully qualified. The trailing dot is
//!   optional: `"relay.test"` and `"relay.test."` are the same name. Device-level
//!   [`set_host`](crate::Device::set_host) writes to `/etc/hosts` where trailing
//!   dots are not used — these are different namespaces.
//! - **TXT records are not queryable via [`DnsServer::resolve`]** — it only
//!   returns A/AAAA addresses. Query TXT records via DNS (e.g. `dig`).

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    sync::{Arc, RwLock},
};

use anyhow::{Context, Result};
use simple_dns::{
    rdata::{RData, A, AAAA, TXT},
    Name, Packet, PacketFlag, ResourceRecord, CLASS, QTYPE, RCODE, TYPE,
};
use tracing::{debug, warn};

use crate::netns;

/// Default TTL for DNS records (seconds).
const DEFAULT_TTL: u32 = 1;

/// Record types the lab DNS server stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RecordType {
    /// IPv4 address record.
    A,
    /// IPv6 address record.
    Aaaa,
    /// Text record.
    Txt,
}

impl RecordType {
    fn from_qtype(qtype: QTYPE) -> Option<Self> {
        match qtype {
            QTYPE::TYPE(TYPE::A) => Some(Self::A),
            QTYPE::TYPE(TYPE::AAAA) => Some(Self::Aaaa),
            QTYPE::TYPE(TYPE::TXT) => Some(Self::Txt),
            _ => None,
        }
    }
}

/// Parses a DNS name. Accepts both `"relay.test"` and `"relay.test."`.
///
/// [`Name`] compares labels without regard to ASCII case and ignores the
/// trailing dot, so the parsed name matches the query name directly.
fn parse_name(name: &str) -> Result<Name<'static>> {
    Ok(Name::new(name).context("invalid DNS name")?.into_owned())
}

type RecordStore = HashMap<(Name<'static>, RecordType), ResourceRecord<'static>>;

/// In-process DNS server running on the IX bridge.
///
/// Records are stored in a [`HashMap`] behind a [`std::sync::RwLock`].
/// All mutation methods are synchronous and guarantee the record is queryable
/// via DNS before returning.
#[derive(Clone)]
pub struct DnsServer {
    records: Arc<RwLock<RecordStore>>,
    /// Aborts the serve task when the last clone drops.
    _task: Arc<tokio_util::task::AbortOnDropHandle<()>>,
}

impl DnsServer {
    /// Starts the DNS server inside the root namespace.
    ///
    /// Binds a dual-stack socket on `[::]:53` inside the root namespace
    /// that handles both IPv4 and IPv6 queries. Wildcard bind avoids DAD
    /// timing issues with specific IPs. The socket is bound synchronously;
    /// the serve loop runs as an async task.
    pub(crate) fn start(netns: &Arc<netns::NetnsManager>, root_ns: &str) -> Result<Self> {
        let records = Arc::new(RwLock::new(RecordStore::new()));

        // Bind a dual-stack socket on [::]:53 inside the root namespace.
        // A single IPv6 socket with IPV6_V6ONLY disabled (Linux default)
        // handles both IPv4 and IPv6 queries.
        let socket: std::net::UdpSocket = netns.run_closure_in(root_ns, || {
            let addr = SocketAddr::from((Ipv6Addr::UNSPECIFIED, 53));
            let sock = std::net::UdpSocket::bind(addr)
                .with_context(|| format!("bind dns server to {addr}"))?;
            sock.set_nonblocking(true)?;
            Ok(sock)
        })?;

        // Spawn the serve loop on the root namespace's tokio runtime.
        // The task is aborted when the last DnsServer clone drops.
        let rt = netns.rt_handle_for(root_ns)?;
        let addr = socket.local_addr().ok();
        let serve_records = records.clone();
        let handle = rt.spawn(async move {
            let socket =
                tokio::net::UdpSocket::from_std(socket).expect("convert std UdpSocket to tokio");
            serve(serve_records, socket).await;
        });
        debug!(?addr, "dns server listening");

        Ok(Self {
            records,
            _task: Arc::new(tokio_util::task::AbortOnDropHandle::new(handle)),
        })
    }

    /// Sets an A or AAAA record, replacing any previous record of the same type
    /// for this name. Immediately visible to DNS queries.
    pub fn set_host(&self, name: &str, ip: IpAddr) -> Result<()> {
        let name = parse_name(name)?;
        let (rtype, rdata) = match ip {
            IpAddr::V4(v4) => (RecordType::A, RData::A(A::from(v4))),
            IpAddr::V6(v6) => (RecordType::Aaaa, RData::AAAA(AAAA::from(v6))),
        };
        let record = ResourceRecord::new(name.clone(), CLASS::IN, DEFAULT_TTL, rdata);
        self.records
            .write()
            .expect("poisoned")
            .insert((name, rtype), record);
        Ok(())
    }

    /// Sets a TXT record, replacing any previous TXT record for this name.
    ///
    /// Immediately visible to DNS queries. Each value becomes one TXT string.
    ///
    /// # Errors
    ///
    /// Returns an error if `name` is not a valid DNS name, or if a value is
    /// longer than 255 bytes, the limit for one TXT string.
    pub fn set_txt(&self, name: &str, values: &[&str]) -> Result<()> {
        let name = parse_name(name)?;
        let mut txt = TXT::new();
        for value in values {
            txt.add_string(value)
                .with_context(|| format!("invalid TXT value {value:?}"))?;
        }
        let record = ResourceRecord::new(
            name.clone(),
            CLASS::IN,
            DEFAULT_TTL,
            RData::TXT(txt.into_owned()),
        );
        self.records
            .write()
            .expect("poisoned")
            .insert((name, RecordType::Txt), record);
        Ok(())
    }

    /// Removes all records matching the given name and type.
    pub fn remove(&self, name: &str, rtype: RecordType) -> Result<()> {
        let name = parse_name(name)?;
        self.records
            .write()
            .expect("poisoned")
            .remove(&(name, rtype));
        Ok(())
    }

    /// In-process lookup. Returns the first matching A or AAAA address.
    pub fn resolve(&self, name: &str) -> Option<IpAddr> {
        let name = parse_name(name).ok()?;
        let store = self.records.read().expect("poisoned");
        [RecordType::A, RecordType::Aaaa]
            .into_iter()
            .find_map(|rtype| match &store.get(&(name.clone(), rtype))?.rdata {
                RData::A(a) => Some(IpAddr::V4(a.clone().into())),
                RData::AAAA(aaaa) => Some(IpAddr::V6(aaaa.clone().into())),
                _ => None,
            })
    }
}

// ── UDP server loop ──────────────────────────────────────────────────

async fn serve(records: Arc<RwLock<RecordStore>>, socket: tokio::net::UdpSocket) {
    let mut buf = vec![0u8; 4096];
    loop {
        let (len, src) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "dns recv error");
                continue;
            }
        };
        if let Some(bytes) = handle_query(&records, &buf[..len]) {
            if let Err(e) = socket.send_to(&bytes, src).await {
                warn!(error = %e, "dns send error");
            }
        }
    }
}

fn handle_query(records: &RwLock<RecordStore>, buf: &[u8]) -> Option<Vec<u8>> {
    let query = Packet::parse(buf).ok()?;
    if query.has_flags(PacketFlag::RESPONSE) {
        return None;
    }

    let mut response = Packet::new_reply(query.id());
    *response.opcode_mut() = query.opcode();
    response.set_flags(PacketFlag::AUTHORITATIVE_ANSWER);
    if query.has_flags(PacketFlag::RECURSION_DESIRED) {
        response.set_flags(PacketFlag::RECURSION_DESIRED);
    }

    let store = records.read().expect("poisoned");
    let mut found = false;
    let mut name_exists = false;
    for q in &query.questions {
        if !name_exists {
            name_exists = store.keys().any(|(n, _)| *n == q.qname);
        }
        let Some(rtype) = RecordType::from_qtype(q.qtype) else {
            continue;
        };
        if let Some(record) = store.get(&(q.qname.clone().into_owned(), rtype)) {
            response.answers.push(record.clone());
            found = true;
        }
    }
    // Name exists but queried type doesn't → NOERROR (empty answer).
    // Name doesn't exist at all → NXDomain.
    if !found && !name_exists {
        *response.rcode_mut() = RCODE::NameError;
    }
    response.questions = query.questions;

    response.build_bytes_vec().ok()
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use simple_dns::{Question, QCLASS};

    use super::*;

    fn store_with(name: &str, ip: Ipv4Addr) -> RwLock<RecordStore> {
        let name = parse_name(name).unwrap();
        let record = ResourceRecord::new(name.clone(), CLASS::IN, DEFAULT_TTL, RData::A(ip.into()));
        RwLock::new(RecordStore::from([((name, RecordType::A), record)]))
    }

    fn query(name: &str, qtype: TYPE) -> Vec<u8> {
        let mut packet = Packet::new_query(42);
        packet.set_flags(PacketFlag::RECURSION_DESIRED);
        packet.questions.push(Question::new(
            Name::new(name).unwrap(),
            qtype.into(),
            QCLASS::CLASS(CLASS::IN),
            false,
        ));
        packet.build_bytes_vec().unwrap()
    }

    #[test]
    fn answers_a_query_case_insensitive() {
        let ip = Ipv4Addr::new(10, 0, 0, 1);
        let store = store_with("relay.test", ip);
        let bytes = handle_query(&store, &query("Relay.TEST.", TYPE::A)).unwrap();
        let response = Packet::parse(&bytes).unwrap();
        assert_eq!(response.id(), 42);
        assert!(response.has_flags(PacketFlag::RESPONSE));
        assert!(response.has_flags(PacketFlag::AUTHORITATIVE_ANSWER));
        assert!(response.has_flags(PacketFlag::RECURSION_DESIRED));
        assert_eq!(response.rcode(), RCODE::NoError);
        assert_eq!(response.questions.len(), 1);
        assert_eq!(response.answers.len(), 1);
        assert_eq!(response.answers[0].rdata, RData::A(ip.into()));
    }

    #[test]
    fn known_name_other_type_is_noerror() {
        let store = store_with("relay.test", Ipv4Addr::new(10, 0, 0, 1));
        let bytes = handle_query(&store, &query("relay.test", TYPE::AAAA)).unwrap();
        let response = Packet::parse(&bytes).unwrap();
        assert_eq!(response.rcode(), RCODE::NoError);
        assert!(response.answers.is_empty());
    }

    #[test]
    fn unknown_name_is_nxdomain() {
        let store = store_with("relay.test", Ipv4Addr::new(10, 0, 0, 1));
        let bytes = handle_query(&store, &query("other.test", TYPE::A)).unwrap();
        let response = Packet::parse(&bytes).unwrap();
        assert_eq!(response.rcode(), RCODE::NameError);
        assert!(response.answers.is_empty());
    }

    #[test]
    fn ignores_responses() {
        let store = store_with("relay.test", Ipv4Addr::new(10, 0, 0, 1));
        let mut packet = Packet::new_reply(1);
        packet.questions.push(Question::new(
            Name::new("relay.test").unwrap(),
            TYPE::A.into(),
            QCLASS::CLASS(CLASS::IN),
            false,
        ));
        assert!(handle_query(&store, &packet.build_bytes_vec().unwrap()).is_none());
    }
}
