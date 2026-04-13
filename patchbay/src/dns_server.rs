//! Minimal in-process authoritative DNS server for the lab.
//!
//! Runs on the IX bridge, serves A/AAAA/TXT records from a [`std::sync::RwLock`]-guarded
//! [`HashMap`]. Record mutations via [`DnsServer::set_host`] / [`DnsServer::set_txt`] are
//! synchronous — the record is visible to DNS queries the instant the method returns.
//!
//! ## Limitations
//!
//! - **No TCP fallback.** Only UDP is supported. Responses exceeding 512 bytes
//!   will be truncated. This is fine for lab use with short names.
//! - **FQDN trailing dots.** DNS names passed to [`DnsServer::set_host`] and
//!   [`DnsServer::set_txt`] are parsed as DNS wire names. Use a trailing dot
//!   for fully-qualified names (`"relay.test."`). Device-level
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
use hickory_proto::{
    op::{Message, MessageType, ResponseCode},
    rr::{
        rdata::{A, AAAA, TXT},
        DNSClass, LowerName, Name, RData, Record, RecordType,
    },
    serialize::binary::{BinDecodable, BinEncodable},
};
use tracing::{debug, warn};

use crate::netns;

/// Default TTL for DNS records (seconds).
const DEFAULT_TTL: u32 = 1;

/// Parses a DNS name and ensures it is fully qualified (has a trailing dot).
///
/// DNS wire queries always use absolute names, so records must be stored
/// as FQDN to match. Accepts both `"relay.test"` and `"relay.test."`.
fn parse_name(name: &str) -> Result<Name> {
    let fqdn = if name.ends_with('.') {
        name.to_string()
    } else {
        format!("{name}.")
    };
    Name::from_ascii(&fqdn).context("invalid DNS name")
}

type RecordStore = HashMap<(LowerName, RecordType), Vec<Record>>;

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
            IpAddr::V6(v6) => (RecordType::AAAA, RData::AAAA(AAAA::from(v6))),
        };
        let key = (LowerName::new(&name), rtype);
        let record = Record::from_rdata(name, DEFAULT_TTL, rdata);
        self.records
            .write()
            .expect("poisoned")
            .insert(key, vec![record]);
        Ok(())
    }

    /// Sets a TXT record, replacing any previous TXT record for this name.
    /// Immediately visible to DNS queries.
    pub fn set_txt(&self, name: &str, values: &[&str]) -> Result<()> {
        let name = parse_name(name)?;
        let txt = TXT::new(values.iter().map(|s| s.to_string()).collect());
        let key = (LowerName::new(&name), RecordType::TXT);
        let record = Record::from_rdata(name, DEFAULT_TTL, RData::TXT(txt));
        self.records
            .write()
            .expect("poisoned")
            .insert(key, vec![record]);
        Ok(())
    }

    /// Removes all records matching the given name and type.
    pub fn remove(&self, name: &str, rtype: RecordType) -> Result<()> {
        let name = parse_name(name)?;
        self.records
            .write()
            .expect("poisoned")
            .remove(&(LowerName::new(&name), rtype));
        Ok(())
    }

    /// In-process lookup. Returns the first matching A or AAAA address.
    pub fn resolve(&self, name: &str) -> Option<IpAddr> {
        let name = parse_name(name).ok()?;
        let lower = LowerName::new(&name);
        let store = self.records.read().expect("poisoned");
        for rtype in [RecordType::A, RecordType::AAAA] {
            if let Some(recs) = store.get(&(lower.clone(), rtype)) {
                for r in recs {
                    match r.data() {
                        RData::A(a) => return Some(IpAddr::V4((*a).into())),
                        RData::AAAA(aaaa) => return Some(IpAddr::V6((*aaaa).into())),
                        _ => {}
                    }
                }
            }
        }
        None
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
    let query = Message::from_bytes(buf).ok()?;
    if query.message_type() != MessageType::Query {
        return None;
    }

    let mut response = Message::new();
    response.set_id(query.id());
    response.set_message_type(MessageType::Response);
    response.set_op_code(query.op_code());
    response.set_recursion_desired(query.recursion_desired());
    response.set_recursion_available(false);
    response.set_authoritative(true);
    response.add_queries(query.queries().iter().cloned());

    let store = records.read().expect("poisoned");
    let mut found = false;
    let mut name_exists = false;
    for q in query.queries() {
        let qname: LowerName = q.name().into();
        if !name_exists {
            name_exists = store.keys().any(|(n, _)| *n == qname);
        }
        if let Some(recs) = store.get(&(qname, q.query_type())) {
            for r in recs {
                let mut answer = r.clone();
                answer.set_dns_class(DNSClass::IN);
                response.add_answer(answer);
                found = true;
            }
        }
    }
    if !found {
        // Name exists but queried type doesn't → NOERROR (empty answer).
        // Name doesn't exist at all → NXDomain.
        let code = if name_exists {
            ResponseCode::NoError
        } else {
            ResponseCode::NXDomain
        };
        response.set_response_code(code);
    }

    response.to_bytes().ok()
}
