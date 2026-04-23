//! UPnP IGD:1 server (SSDP + HTTP/SOAP) for the router namespace.
//!
//! The server is split into two tasks:
//!
//! - An SSDP responder bound to UDP 1900, joined to the
//!   `239.255.255.250` multicast group on the router's downstream bridge,
//!   that replies to `M-SEARCH` discoveries with an HTTP-over-UDP 200 OK
//!   carrying a `LOCATION` header.
//! - An HTTP server bound to an ephemeral TCP port on `downstream_gw`
//!   that serves three routes. `GET /rootDesc.xml` returns the device
//!   description whose shape matches the `xmltree` parser in
//!   `igd-next::common::parsing`. `GET /WANIPCn.xml` returns the service
//!   control protocol description (SCPD) listing actions and their
//!   arguments. `POST /ctl/IPConn` handles the SOAP actions
//!   `GetExternalIPAddress`, `AddPortMapping`, `AddAnyPortMapping`, and
//!   `DeletePortMapping`.
//!
//! The HTTP surface is small and fixed, so a hand-rolled HTTP/1.1
//! server avoids adding hyper and axum to the core crate. The SOAP
//! action handler matches action names with a byte-level search and
//! extracts arguments with a minimal tag parser; that is sufficient for
//! the well-formed bodies that `igd-next` sends.

use std::{
    net::{Ipv4Addr, SocketAddrV4},
    num::NonZeroU16,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context as _, Result};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
    sync::Semaphore,
    task::JoinHandle,
};
use tracing::{debug, trace, warn};

use super::{
    nft,
    registry::{AllocOutcome, MapProto, MappingKey},
    server::ServerContext,
};
use crate::netns::NetnsManager;

/// Service type advertised in SSDP and the device description.
const SERVICE_TYPE: &str = "urn:schemas-upnp-org:service:WANIPConnection:1";

/// Device type advertised by the IGD root device.
const DEVICE_TYPE: &str = "urn:schemas-upnp-org:device:InternetGatewayDevice:1";

/// Fixed UUID for the lab IGD root device. Real routers pick a unique
/// UUID per unit; a fixed value is fine here because each router
/// namespace is isolated.
const DEVICE_UUID: &str = "uuid:d1ce0000-0000-0000-0000-000000000001";

/// SSDP multicast address.
const SSDP_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);

/// SSDP port.
const SSDP_PORT: u16 = 1900;

/// Control URL served by the HTTP server.
const CONTROL_URL: &str = "/ctl/IPConn";

/// SCPD URL served by the HTTP server.
const SCPD_URL: &str = "/WANIPCn.xml";

/// Device description URL served by the HTTP server.
const ROOT_DESC_URL: &str = "/rootDesc.xml";

/// Upper bound on an HTTP request body. Generously sized for SOAP.
/// Bodies above this are rejected with HTTP 413 before they are buffered.
const MAX_BODY_SIZE: usize = 64 * 1024;

/// Per-request header read timeout. Defends against slowloris clients
/// that open a TCP connection and dribble bytes.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-request body read timeout.
const BODY_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on concurrent HTTP connections. Accepted connections beyond this
/// count wait on a semaphore; no per-source fairness, just a backstop.
const MAX_HTTP_CONNECTIONS: usize = 128;

/// Spawns the UPnP IGD server tasks and returns their abort handles.
pub(crate) async fn spawn(
    netns: Arc<NetnsManager>,
    ns: Arc<str>,
    ctx: Arc<ServerContext>,
    downstream_gw: Ipv4Addr,
) -> Result<(JoinHandle<()>, JoinHandle<()>)> {
    let listener = bind_http_listener(&netns, &ns, downstream_gw).await?;
    let http_port = listener
        .local_addr()
        .context("http listener local_addr")?
        .port();
    let http_port = NonZeroU16::new(http_port).context("http port is zero")?;
    let ssdp = bind_ssdp_socket(&netns, &ns, downstream_gw).await?;

    let rt = netns.rt_handle_for(&ns)?;
    let ssdp_task = rt.spawn(async move {
        run_ssdp(ssdp, downstream_gw, http_port).await;
    });
    let http_task = rt.spawn(run_http(listener, ctx.clone()));
    debug!(
        ns = %ns,
        gw = %downstream_gw,
        http_port = http_port.get(),
        "upnp: listening"
    );
    Ok((ssdp_task, http_task))
}

async fn bind_http_listener(
    netns: &Arc<NetnsManager>,
    ns: &str,
    downstream_gw: Ipv4Addr,
) -> Result<TcpListener> {
    let sock_std: std::net::TcpListener = netns.run_closure_in(ns, move || {
        let addr = SocketAddrV4::new(downstream_gw, 0);
        let listener = std::net::TcpListener::bind(addr)
            .with_context(|| format!("bind http listener {addr}"))?;
        listener.set_nonblocking(true)?;
        Ok(listener)
    })?;
    let listener = TcpListener::from_std(sock_std)?;
    Ok(listener)
}

async fn bind_ssdp_socket(
    netns: &Arc<NetnsManager>,
    ns: &str,
    downstream_gw: Ipv4Addr,
) -> Result<UdpSocket> {
    let sock_std: std::net::UdpSocket = netns.run_closure_in(ns, move || {
        // SO_REUSEADDR lets the router coexist with any lab test that
        // also binds 1900; in an isolated ns nothing else binds here,
        // but keep it for defensive reasons.
        let sock = std::net::UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, SSDP_PORT))
            .with_context(|| format!("bind ssdp 0.0.0.0:{SSDP_PORT}"))?;
        sock.join_multicast_v4(&SSDP_ADDR, &downstream_gw)
            .context("join ssdp multicast group on downstream gateway")?;
        sock.set_multicast_loop_v4(false).ok();
        sock.set_nonblocking(true)?;
        Ok(sock)
    })?;
    Ok(UdpSocket::from_std(sock_std)?)
}

async fn run_ssdp(socket: UdpSocket, downstream_gw: Ipv4Addr, http_port: NonZeroU16) {
    let mut buf = vec![0u8; 2048];
    loop {
        let (len, peer) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "ssdp: recv error");
                continue;
            }
        };
        let data = &buf[..len];
        let text = match std::str::from_utf8(data) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if !is_m_search(text) {
            continue;
        }
        let want = extract_search_target(text);
        if !search_target_matches(want) {
            continue;
        }
        let location = format!(
            "http://{gw}:{port}{path}",
            gw = downstream_gw,
            port = http_port.get(),
            path = ROOT_DESC_URL
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             CACHE-CONTROL: max-age=1800\r\n\
             EXT:\r\n\
             LOCATION: {location}\r\n\
             SERVER: patchbay UPnP/1.0 IGD/1.0\r\n\
             ST: {DEVICE_TYPE}\r\n\
             USN: {DEVICE_UUID}::{DEVICE_TYPE}\r\n\
             \r\n"
        );
        if let Err(e) = socket.send_to(response.as_bytes(), peer).await {
            warn!(error = %e, peer = %peer, "ssdp: send error");
        }
    }
}

fn is_m_search(text: &str) -> bool {
    text.starts_with("M-SEARCH * HTTP/1.1")
}

fn extract_search_target(text: &str) -> Option<&str> {
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.to_ascii_lowercase().starts_with("st:") {
            if let Some((_, value)) = trimmed.split_once(':') {
                return Some(value.trim());
            }
        }
    }
    None
}

fn search_target_matches(target: Option<&str>) -> bool {
    match target {
        None => false,
        Some(t) => {
            t == "ssdp:all"
                || t == "upnp:rootdevice"
                || t == DEVICE_TYPE
                || t == SERVICE_TYPE
                || t.starts_with("uuid:")
        }
    }
}

async fn run_http(listener: TcpListener, ctx: Arc<ServerContext>) {
    let connection_limit = Arc::new(Semaphore::new(MAX_HTTP_CONNECTIONS));
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "upnp http: accept");
                continue;
            }
        };
        // Reject IPv6 peers early: the advertised LOCATION is IPv4 only.
        let peer_v4 = match peer {
            std::net::SocketAddr::V4(v) => *v.ip(),
            std::net::SocketAddr::V6(_) => continue,
        };
        let permit = match connection_limit.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                debug!(%peer, "upnp http: connection limit reached, rejecting");
                continue;
            }
        };
        let ctx = ctx.clone();
        // The accept task is wrapped in AbortOnDropHandle at the server
        // level; the per-connection handler runs as a detached
        // `tokio::spawn` but carries the semaphore permit, which releases
        // when the future drops. Dropping the accept task therefore
        // stops taking new connections; in-flight ones drain naturally.
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = handle_http_connection(stream, ctx, peer_v4).await {
                debug!(error = %e, %peer, "upnp http: connection error");
            }
        });
    }
}

async fn handle_http_connection(
    mut stream: tokio::net::TcpStream,
    ctx: Arc<ServerContext>,
    peer: Ipv4Addr,
) -> Result<()> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 2048];
    // Read until we have complete headers, or the read deadline fires.
    let header_end = tokio::time::timeout(HEADER_TIMEOUT, async {
        loop {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                return Ok::<usize, anyhow::Error>(0);
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = find_headers_end(&buf) {
                return Ok(pos);
            }
            if buf.len() > MAX_BODY_SIZE {
                anyhow::bail!("headers too large");
            }
        }
    })
    .await
    .context("upnp http: header read timeout")??;

    if header_end == 0 {
        return Ok(());
    }

    let (request_line, headers) =
        parse_request_headers(&buf[..header_end]).context("invalid http request headers")?;
    let content_length = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY_SIZE {
        let response = simple_status_response(413, "Payload Too Large");
        stream.write_all(&response).await.ok();
        return Ok(());
    }

    let body_start = header_end + 4;
    tokio::time::timeout(BODY_TIMEOUT, async {
        while buf.len() < body_start + content_length {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                anyhow::bail!("short body: peer closed before content-length bytes");
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("upnp http: body read timeout")??;
    let body = &buf[body_start..body_start + content_length];

    let response = route_request(&request_line, &headers, body, &ctx, peer).await;
    stream.write_all(&response).await?;
    stream.flush().await?;
    Ok(())
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_request_headers(bytes: &[u8]) -> Result<(String, Vec<(String, String)>)> {
    let text = std::str::from_utf8(bytes).context("http headers not utf-8")?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().context("missing request line")?.to_string();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok((request_line, headers))
}

async fn route_request(
    request_line: &str,
    headers: &[(String, String)],
    body: &[u8],
    ctx: &ServerContext,
    peer: Ipv4Addr,
) -> Vec<u8> {
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");

    match (method, target) {
        ("GET", p) if p == ROOT_DESC_URL => http_xml_response(build_root_description(ctx.wan_ip)),
        ("GET", p) if p == SCPD_URL => http_xml_response(build_scpd().to_string()),
        ("POST", p) if p == CONTROL_URL => {
            let action_header = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("soapaction"))
                .map(|(_, v)| v.trim_matches('"').to_string())
                .unwrap_or_default();
            let body_str = std::str::from_utf8(body).unwrap_or("");
            handle_soap(&action_header, body_str, ctx, peer).await
        }
        _ => simple_status_response(404, "Not Found"),
    }
}

fn http_xml_response(body: String) -> Vec<u8> {
    let mut resp = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/xml; charset=\"utf-8\"\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         Server: patchbay UPnP/1.0 IGD/1.0\r\n\
         \r\n",
        body.len()
    )
    .into_bytes();
    resp.extend_from_slice(body.as_bytes());
    resp
}

fn simple_status_response(code: u16, reason: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\
         \r\n"
    )
    .into_bytes()
}

fn build_root_description(wan_ip: Ipv4Addr) -> String {
    format!(
        r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>{DEVICE_TYPE}</deviceType>
    <friendlyName>patchbay IGD</friendlyName>
    <manufacturer>patchbay</manufacturer>
    <modelName>patchbay-igd</modelName>
    <UDN>{DEVICE_UUID}</UDN>
    <deviceList>
      <device>
        <deviceType>urn:schemas-upnp-org:device:WANDevice:1</deviceType>
        <friendlyName>WAN</friendlyName>
        <UDN>{DEVICE_UUID}</UDN>
        <deviceList>
          <device>
            <deviceType>urn:schemas-upnp-org:device:WANConnectionDevice:1</deviceType>
            <friendlyName>WAN Connection</friendlyName>
            <UDN>{DEVICE_UUID}</UDN>
            <serviceList>
              <service>
                <serviceType>{SERVICE_TYPE}</serviceType>
                <serviceId>urn:upnp-org:serviceId:WANIPConn1</serviceId>
                <controlURL>{CONTROL_URL}</controlURL>
                <eventSubURL>/evt/IPConn</eventSubURL>
                <SCPDURL>{SCPD_URL}</SCPDURL>
              </service>
            </serviceList>
          </device>
        </deviceList>
      </device>
    </deviceList>
    <presentationURL>http://{wan_ip}/</presentationURL>
  </device>
</root>"#
    )
}

/// Minimal SCPD covering the four actions that `igd-next` calls.
fn build_scpd() -> &'static str {
    r#"<?xml version="1.0"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <actionList>
    <action>
      <name>GetExternalIPAddress</name>
      <argumentList>
        <argument><name>NewExternalIPAddress</name><direction>out</direction><relatedStateVariable>ExternalIPAddress</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>AddPortMapping</name>
      <argumentList>
        <argument><name>NewRemoteHost</name><direction>in</direction><relatedStateVariable>RemoteHost</relatedStateVariable></argument>
        <argument><name>NewExternalPort</name><direction>in</direction><relatedStateVariable>ExternalPort</relatedStateVariable></argument>
        <argument><name>NewProtocol</name><direction>in</direction><relatedStateVariable>PortMappingProtocol</relatedStateVariable></argument>
        <argument><name>NewInternalPort</name><direction>in</direction><relatedStateVariable>InternalPort</relatedStateVariable></argument>
        <argument><name>NewInternalClient</name><direction>in</direction><relatedStateVariable>InternalClient</relatedStateVariable></argument>
        <argument><name>NewEnabled</name><direction>in</direction><relatedStateVariable>PortMappingEnabled</relatedStateVariable></argument>
        <argument><name>NewPortMappingDescription</name><direction>in</direction><relatedStateVariable>PortMappingDescription</relatedStateVariable></argument>
        <argument><name>NewLeaseDuration</name><direction>in</direction><relatedStateVariable>PortMappingLeaseDuration</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>AddAnyPortMapping</name>
      <argumentList>
        <argument><name>NewRemoteHost</name><direction>in</direction><relatedStateVariable>RemoteHost</relatedStateVariable></argument>
        <argument><name>NewExternalPort</name><direction>in</direction><relatedStateVariable>ExternalPort</relatedStateVariable></argument>
        <argument><name>NewProtocol</name><direction>in</direction><relatedStateVariable>PortMappingProtocol</relatedStateVariable></argument>
        <argument><name>NewInternalPort</name><direction>in</direction><relatedStateVariable>InternalPort</relatedStateVariable></argument>
        <argument><name>NewInternalClient</name><direction>in</direction><relatedStateVariable>InternalClient</relatedStateVariable></argument>
        <argument><name>NewEnabled</name><direction>in</direction><relatedStateVariable>PortMappingEnabled</relatedStateVariable></argument>
        <argument><name>NewPortMappingDescription</name><direction>in</direction><relatedStateVariable>PortMappingDescription</relatedStateVariable></argument>
        <argument><name>NewLeaseDuration</name><direction>in</direction><relatedStateVariable>PortMappingLeaseDuration</relatedStateVariable></argument>
        <argument><name>NewReservedPort</name><direction>out</direction><relatedStateVariable>ExternalPort</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>DeletePortMapping</name>
      <argumentList>
        <argument><name>NewRemoteHost</name><direction>in</direction><relatedStateVariable>RemoteHost</relatedStateVariable></argument>
        <argument><name>NewExternalPort</name><direction>in</direction><relatedStateVariable>ExternalPort</relatedStateVariable></argument>
        <argument><name>NewProtocol</name><direction>in</direction><relatedStateVariable>PortMappingProtocol</relatedStateVariable></argument>
      </argumentList>
    </action>
  </actionList>
  <serviceStateTable>
    <stateVariable sendEvents="no"><name>ExternalIPAddress</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>RemoteHost</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>ExternalPort</name><dataType>ui2</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>InternalPort</name><dataType>ui2</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>InternalClient</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>PortMappingProtocol</name><dataType>string</dataType><allowedValueList><allowedValue>TCP</allowedValue><allowedValue>UDP</allowedValue></allowedValueList></stateVariable>
    <stateVariable sendEvents="no"><name>PortMappingEnabled</name><dataType>boolean</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>PortMappingDescription</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>PortMappingLeaseDuration</name><dataType>ui4</dataType></stateVariable>
  </serviceStateTable>
</scpd>"#
}

async fn handle_soap(action: &str, body: &str, ctx: &ServerContext, peer: Ipv4Addr) -> Vec<u8> {
    trace!(?action, %peer, "upnp soap request");
    let action_name = action.split('#').next_back().unwrap_or("");
    match action_name {
        "GetExternalIPAddress" => http_xml_response(soap_success(
            "GetExternalIPAddress",
            &[("NewExternalIPAddress", &ctx.wan_ip.to_string())],
        )),
        "AddPortMapping" => handle_add_port_mapping(body, ctx, peer, false).await,
        "AddAnyPortMapping" => handle_add_port_mapping(body, ctx, peer, true).await,
        "DeletePortMapping" => handle_delete_port_mapping(body, ctx, peer).await,
        _ => http_xml_response(soap_fault(401, "Invalid Action")),
    }
}

async fn handle_add_port_mapping(
    body: &str,
    ctx: &ServerContext,
    peer: Ipv4Addr,
    pick_any: bool,
) -> Vec<u8> {
    let external_port =
        match extract_tag(body, "NewExternalPort").and_then(|s| s.parse::<u16>().ok()) {
            Some(p) => p,
            None => {
                return http_xml_response(soap_fault(402, "Invalid Args"));
            }
        };
    let internal_port =
        match extract_tag(body, "NewInternalPort").and_then(|s| s.parse::<u16>().ok()) {
            Some(p) => p,
            None => return http_xml_response(soap_fault(402, "Invalid Args")),
        };
    let internal_client: Ipv4Addr =
        match extract_tag(body, "NewInternalClient").and_then(|s| s.parse().ok()) {
            Some(a) => a,
            None => return http_xml_response(soap_fault(402, "Invalid Args")),
        };
    let protocol = match extract_tag(body, "NewProtocol").map(|s| s.trim().to_ascii_uppercase()) {
        Some(ref s) if s == "UDP" => MapProto::Udp,
        Some(ref s) if s == "TCP" => MapProto::Tcp,
        _ => return http_xml_response(soap_fault(402, "Invalid Args")),
    };
    let lease_duration = extract_tag(body, "NewLeaseDuration")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);

    // Security: internal client must match the caller's source IP and
    // fall inside the downstream subnet. Enforcing caller==client stops
    // a LAN host from hijacking inbound traffic destined for a peer.
    if !ctx.downstream_cidr.contains(&internal_client) || internal_client != peer {
        debug!(
            %peer,
            %internal_client,
            "upnp: reject AddPortMapping with mismatched/off-subnet client"
        );
        return http_xml_response(soap_fault(606, "Action not authorized"));
    }

    let internal_local = match NonZeroU16::new(internal_port) {
        Some(p) => p,
        None => return http_xml_response(soap_fault(402, "Invalid Args")),
    };

    // IGD:1 treats lease_duration=0 as "permanent"; we interpret that
    // as the server maximum rather than forever.
    let lifetime = if lease_duration == 0 {
        ctx.max_lifetime
    } else {
        Duration::from_secs(u64::from(lease_duration)).min(ctx.max_lifetime)
    };

    let preferred = NonZeroU16::new(external_port);

    let (outcome, rules_snapshot) = {
        let mut registry = ctx.registry.lock().await;
        let outcome = registry.allocate(
            protocol,
            internal_client,
            internal_local,
            if pick_any { None } else { preferred },
            lifetime,
            None,
        );
        let rules: Vec<_> = registry.iter().cloned().collect();
        (outcome, rules)
    };

    match outcome {
        AllocOutcome::Created(ref m) | AllocOutcome::Renewed(ref m) => {
            if let Err(e) =
                nft::apply_portmap_rules(&ctx.netns, &ctx.ns, ctx.wan_ip, &rules_snapshot).await
            {
                warn!(error = %e, "upnp: apply nft");
                return http_xml_response(soap_fault(500, "Action failed"));
            }
            if pick_any {
                http_xml_response(soap_success(
                    "AddAnyPortMapping",
                    &[("NewReservedPort", &m.external_port.get().to_string())],
                ))
            } else {
                http_xml_response(soap_success("AddPortMapping", &[]))
            }
        }
        AllocOutcome::Conflict => http_xml_response(soap_fault(718, "Conflict in mapping entry")),
        AllocOutcome::NoPortsAvailable => http_xml_response(soap_fault(728, "No ports available")),
    }
}

async fn handle_delete_port_mapping(body: &str, ctx: &ServerContext, peer: Ipv4Addr) -> Vec<u8> {
    let external_port =
        match extract_tag(body, "NewExternalPort").and_then(|s| s.parse::<u16>().ok()) {
            Some(p) => p,
            None => return http_xml_response(soap_fault(402, "Invalid Args")),
        };
    let external_nz = match NonZeroU16::new(external_port) {
        Some(p) => p,
        None => return http_xml_response(soap_fault(402, "Invalid Args")),
    };
    let protocol = match extract_tag(body, "NewProtocol").map(|s| s.trim().to_ascii_uppercase()) {
        Some(ref s) if s == "UDP" => MapProto::Udp,
        Some(ref s) if s == "TCP" => MapProto::Tcp,
        _ => return http_xml_response(soap_fault(402, "Invalid Args")),
    };

    // Security: only the mapping's owner may delete it. Look up the
    // mapping, compare internal IP to caller, then remove.
    let key = MappingKey {
        proto: protocol,
        external_port: external_nz,
    };
    let (authorized, removed, rules_snapshot) = {
        let mut registry = ctx.registry.lock().await;
        let owner = registry.get(key).map(|m| m.internal_ip);
        let authorized = owner == Some(peer);
        let removed = if authorized {
            registry.remove(key).is_some()
        } else {
            false
        };
        let rules: Vec<_> = registry.iter().cloned().collect();
        (authorized, removed, rules)
    };

    if !authorized {
        debug!(
            %peer,
            ext = external_nz.get(),
            "upnp: reject DeletePortMapping; caller is not the mapping owner",
        );
        return http_xml_response(soap_fault(606, "Action not authorized"));
    }
    if !removed {
        return http_xml_response(soap_fault(714, "NoSuchEntryInArray"));
    }
    if let Err(e) = nft::apply_portmap_rules(&ctx.netns, &ctx.ns, ctx.wan_ip, &rules_snapshot).await
    {
        warn!(error = %e, "upnp: apply nft on delete");
        return http_xml_response(soap_fault(500, "Action failed"));
    }
    http_xml_response(soap_success("DeletePortMapping", &[]))
}

/// Extracts the inner text of a direct-child tag from a SOAP body. The
/// parser is tolerant enough for the well-formed payloads `igd-next`
/// emits: first `<name>` wins, matching is case-sensitive on the tag
/// name.
fn extract_tag<'a>(body: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&close)? + start;
    Some(body[start..end].trim())
}

fn soap_success(action: &str, out_args: &[(&str, &str)]) -> String {
    let mut args = String::new();
    for (k, v) in out_args {
        args.push_str(&format!("<{k}>{v}</{k}>"));
    }
    format!(
        r#"<?xml version="1.0"?>
<s:Envelope s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/" xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
<s:Body>
<u:{action}Response xmlns:u="{SERVICE_TYPE}">{args}</u:{action}Response>
</s:Body>
</s:Envelope>"#
    )
}

fn soap_fault(code: u16, description: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<s:Envelope s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/" xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
<s:Body>
<s:Fault>
<faultcode>s:Client</faultcode>
<faultstring>UPnPError</faultstring>
<detail>
<UPnPError xmlns="urn:schemas-upnp-org:control-1-0">
<errorCode>{code}</errorCode>
<errorDescription>{description}</errorDescription>
</UPnPError>
</detail>
</s:Fault>
</s:Body>
</s:Envelope>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m_search_discovery_is_recognized() {
        let body = "M-SEARCH * HTTP/1.1\r\nHost: 239.255.255.250:1900\r\nMan: \"ssdp:discover\"\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\nMX: 3\r\n\r\n";
        assert!(is_m_search(body));
        let st = extract_search_target(body);
        assert!(search_target_matches(st));
    }

    #[test]
    fn rootdesc_mentions_wan_ip_connection_service() {
        let xml = build_root_description(Ipv4Addr::new(198, 51, 100, 1));
        assert!(xml.contains(SERVICE_TYPE));
        assert!(xml.contains(CONTROL_URL));
        assert!(xml.contains(SCPD_URL));
    }

    #[test]
    fn scpd_lists_required_actions() {
        let scpd = build_scpd();
        for action in [
            "GetExternalIPAddress",
            "AddPortMapping",
            "AddAnyPortMapping",
            "DeletePortMapping",
        ] {
            assert!(scpd.contains(action), "scpd missing {action}");
        }
    }

    #[test]
    fn extract_tag_handles_soap_like_body() {
        let body = r#"<s:Body><u:AddPortMapping>
  <NewRemoteHost></NewRemoteHost>
  <NewExternalPort>8080</NewExternalPort>
  <NewProtocol>UDP</NewProtocol>
  <NewInternalPort>80</NewInternalPort>
  <NewInternalClient>10.0.0.5</NewInternalClient>
</u:AddPortMapping></s:Body>"#;
        assert_eq!(extract_tag(body, "NewExternalPort"), Some("8080"));
        assert_eq!(extract_tag(body, "NewProtocol"), Some("UDP"));
        assert_eq!(extract_tag(body, "NewInternalClient"), Some("10.0.0.5"));
    }

    #[test]
    fn soap_success_includes_out_args() {
        let body = soap_success(
            "GetExternalIPAddress",
            &[("NewExternalIPAddress", "1.2.3.4")],
        );
        assert!(body.contains("<NewExternalIPAddress>1.2.3.4</NewExternalIPAddress>"));
        assert!(body.contains(SERVICE_TYPE));
    }
}
