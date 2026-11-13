use std::{
    fs::File,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    os::fd::AsRawFd,
};

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;
use netlink_packet_core::{
    NetlinkHeader, NetlinkMessage, NetlinkPayload, NLM_F_ACK, NLM_F_CREATE, NLM_F_DUMP, NLM_F_EXCL,
    NLM_F_REPLACE, NLM_F_REQUEST,
};
use netlink_packet_route::{
    address::{AddressAttribute, AddressMessage},
    link::{InfoData, InfoKind, InfoVeth, LinkAttribute, LinkFlags, LinkInfo, LinkMessage},
    route::{
        RouteAddress, RouteAttribute, RouteHeader, RouteMessage, RouteProtocol, RouteScope,
        RouteType,
    },
    AddressFamily, RouteNetlinkMessage,
};
use netlink_proto::{
    sys::{protocols::NETLINK_ROUTE, SocketAddr},
    Connection, ConnectionHandle,
};
use tracing::trace;

const CREATE: u16 = NLM_F_CREATE | NLM_F_EXCL;
const REPLACE: u16 = NLM_F_CREATE | NLM_F_REPLACE;

/// Sends route netlink requests in one network namespace.
#[derive(Clone)]
pub(crate) struct Netlink {
    handle: ConnectionHandle<RouteNetlinkMessage>,
}

/// Opens a route netlink socket in the network namespace of the calling thread.
pub(crate) fn connect() -> Result<(Connection<RouteNetlinkMessage>, Netlink)> {
    let (connection, handle, _) =
        netlink_proto::new_connection(NETLINK_ROUTE).context("netlink new_connection")?;
    Ok((connection, Netlink { handle }))
}

impl Netlink {
    /// Collects the replies to one request; a kernel error becomes an [`io::Error`].
    async fn ask(
        &self,
        message: RouteNetlinkMessage,
        flags: u16,
    ) -> Result<Vec<RouteNetlinkMessage>> {
        let mut header = NetlinkHeader::default();
        header.flags = NLM_F_REQUEST | flags;
        let request = NetlinkMessage::new(header, NetlinkPayload::InnerMessage(message));
        let mut replies = self
            .handle
            .request(request, SocketAddr::new(0, 0))
            .map_err(|err| anyhow!("netlink request: {err}"))?;
        let mut got = Vec::new();
        while let Some(reply) = replies.next().await {
            match reply.payload {
                NetlinkPayload::InnerMessage(inner) => got.push(inner),
                NetlinkPayload::Error(err) => {
                    if let Some(code) = err.code {
                        return Err(io::Error::from_raw_os_error(-code.get()).into());
                    }
                }
                _ => {}
            }
        }
        Ok(got)
    }

    /// Sends a request and waits for the kernel's acknowledgement.
    async fn tell(&self, message: RouteNetlinkMessage, flags: u16) -> Result<()> {
        self.ask(message, flags | NLM_F_ACK).await.map(drop)
    }

    /// Creates an object; an object that exists already counts as success.
    async fn create(&self, message: RouteNetlinkMessage) -> Result<()> {
        match self.tell(message, CREATE).await {
            Err(err) if is_eexist(&err) => Ok(()),
            other => other,
        }
    }

    async fn change(&self, ifname: &str, edit: impl FnOnce(&mut LinkMessage)) -> Result<()> {
        let mut msg = LinkMessage::default();
        msg.header.index = self.link_index(ifname).await?;
        edit(&mut msg);
        self.tell(RouteNetlinkMessage::NewLink(msg), 0).await
    }

    pub(crate) async fn link_index(&self, ifname: &str) -> Result<u32> {
        let mut query = LinkMessage::default();
        query
            .attributes
            .push(LinkAttribute::IfName(ifname.to_string()));
        let replies = self
            .ask(RouteNetlinkMessage::GetLink(query), 0)
            .await
            .with_context(|| format!("link not found: {ifname}"))?;
        replies
            .into_iter()
            .find_map(|reply| match reply {
                RouteNetlinkMessage::NewLink(link) => Some(link.header.index),
                _ => None,
            })
            .ok_or_else(|| anyhow!("link not found: {ifname}"))
    }

    /// Deletes the link `ifname` if it exists.
    pub(crate) async fn ensure_link_deleted(&self, ifname: &str) -> Result<()> {
        if let Ok(idx) = self.link_index(ifname).await {
            trace!(ifname = %ifname, "delete link");
            let mut msg = LinkMessage::default();
            msg.header.index = idx;
            self.tell(RouteNetlinkMessage::DelLink(msg), 0).await?;
        }
        Ok(())
    }

    /// Creates a bridge; a bridge that exists already counts as success.
    pub(crate) async fn add_bridge(&self, name: &str) -> Result<()> {
        trace!(bridge = %name, "add bridge");
        self.create(RouteNetlinkMessage::NewLink(new_link(
            name,
            InfoKind::Bridge,
            None,
        )))
        .await
    }

    pub(crate) async fn add_veth(&self, a: &str, b: &str) -> Result<()> {
        trace!(a = %a, b = %b, "add veth pair");
        let mut peer = LinkMessage::default();
        peer.attributes.push(LinkAttribute::IfName(b.to_string()));
        let data = InfoData::Veth(InfoVeth::Peer(peer));
        let msg = new_link(a, InfoKind::Veth, Some(data));
        self.tell(RouteNetlinkMessage::NewLink(msg), CREATE).await
    }

    /// Creates a dummy network interface.
    pub(crate) async fn add_dummy(&self, name: &str) -> Result<()> {
        trace!(name = %name, "add dummy");
        let msg = new_link(name, InfoKind::Dummy, None);
        self.tell(RouteNetlinkMessage::NewLink(msg), CREATE).await
    }

    pub(crate) async fn set_link_up(&self, ifname: &str) -> Result<()> {
        trace!(ifname = %ifname, "set link up");
        self.change(ifname, |msg| {
            msg.header.flags = LinkFlags::Up;
            msg.header.change_mask = LinkFlags::Up;
        })
        .await
    }

    pub(crate) async fn set_link_down(&self, ifname: &str) -> Result<()> {
        trace!(ifname = %ifname, "set link down");
        self.change(ifname, |msg| msg.header.change_mask = LinkFlags::Up)
            .await
    }

    /// Sets the MTU on an interface.
    pub(crate) async fn set_mtu(&self, ifname: &str, mtu: u32) -> Result<()> {
        trace!(ifname = %ifname, mtu, "set mtu");
        self.change(ifname, |msg| msg.attributes.push(LinkAttribute::Mtu(mtu)))
            .await
    }

    pub(crate) async fn rename_link(&self, from: &str, to: &str) -> Result<()> {
        trace!(from = %from, to = %to, "rename link");
        let name = LinkAttribute::IfName(to.to_string());
        self.change(from, |msg| msg.attributes.push(name)).await
    }

    pub(crate) async fn set_master(&self, ifname: &str, master: &str) -> Result<()> {
        trace!(ifname = %ifname, master = %master, "set master");
        let controller = LinkAttribute::Controller(self.link_index(master).await?);
        self.change(ifname, |msg| msg.attributes.push(controller))
            .await
    }

    pub(crate) async fn move_link_to_netns(&self, ifname: &str, ns_fd: &File) -> Result<()> {
        trace!(ifname = %ifname, "move link to netns");
        let target = LinkAttribute::NetNsFd(ns_fd.as_raw_fd());
        self.change(ifname, |msg| msg.attributes.push(target)).await
    }

    /// Removes an IPv4 address from an interface.
    pub(crate) async fn del_addr4(&self, ifname: &str, ip: Ipv4Addr, prefix: u8) -> Result<()> {
        trace!(ifname = %ifname, ip = %ip, prefix, "del addr4");
        let idx = self.link_index(ifname).await?;
        let mut query = AddressMessage::default();
        query.header.family = AddressFamily::Inet;
        let wanted = AddressAttribute::Address(ip.into());
        let replies = self
            .ask(RouteNetlinkMessage::GetAddress(query), NLM_F_DUMP)
            .await?;
        let found = replies.into_iter().find_map(|reply| match reply {
            RouteNetlinkMessage::NewAddress(addr)
                if addr.header.index == idx
                    && addr.header.prefix_len == prefix
                    && addr.attributes.contains(&wanted) =>
            {
                Some(addr)
            }
            _ => None,
        });
        if let Some(addr) = found {
            self.tell(RouteNetlinkMessage::DelAddress(addr), 0).await?;
        }
        Ok(())
    }

    /// Adds an IPv4 address to an interface, ignoring EEXIST.
    pub(crate) async fn add_addr4(&self, ifname: &str, ip: Ipv4Addr, prefix: u8) -> Result<()> {
        trace!(ifname = %ifname, ip = %ip, prefix, "add addr4");
        self.add_addr(ifname, ip.into(), prefix).await
    }

    /// Adds an IPv6 address to an interface, ignoring EEXIST.
    pub(crate) async fn add_addr6(&self, ifname: &str, ip: Ipv6Addr, prefix: u8) -> Result<()> {
        trace!(ifname = %ifname, ip = %ip, prefix, "add addr6");
        self.add_addr(ifname, ip.into(), prefix).await
    }

    async fn add_addr(&self, ifname: &str, ip: IpAddr, prefix: u8) -> Result<()> {
        let mut msg = AddressMessage::default();
        msg.header.family = family(ip);
        msg.header.prefix_len = prefix;
        msg.header.index = self.link_index(ifname).await?;
        msg.attributes.push(AddressAttribute::Address(ip));
        if let IpAddr::V4(v4) = ip {
            msg.attributes.push(AddressAttribute::Local(ip));
            if prefix < 31 {
                let broadcast = Ipv4Addr::from(u32::from(v4) | u32::MAX >> prefix);
                msg.attributes.push(AddressAttribute::Broadcast(broadcast));
            }
        }
        self.create(RouteNetlinkMessage::NewAddress(msg)).await
    }

    /// Adds an IPv4 default route via `via`, ignoring EEXIST.
    pub(crate) async fn add_default_route_v4(&self, via: Ipv4Addr) -> Result<()> {
        trace!(via = %via, "add default route v4");
        let msg = route(AddressFamily::Inet, None, Some(via.into()), None);
        self.create(RouteNetlinkMessage::NewRoute(msg)).await
    }

    /// Replaces the IPv4 default route via `ifname` (deletes all existing defaults first).
    pub(crate) async fn replace_default_route_v4(&self, ifname: &str, via: Ipv4Addr) -> Result<()> {
        trace!(ifname = %ifname, via = %via, "replace default route v4");
        let idx = self.link_index(ifname).await?;
        self.delete_default_routes(AddressFamily::Inet).await?;
        let msg = route(AddressFamily::Inet, None, Some(via.into()), Some(idx));
        self.tell(RouteNetlinkMessage::NewRoute(msg), CREATE).await
    }

    /// Adds a route for an IPv4 prefix via a gateway, ignoring EEXIST.
    pub(crate) async fn add_route_v4(
        &self,
        dst: Ipv4Addr,
        prefix: u8,
        via: Ipv4Addr,
    ) -> Result<()> {
        trace!(dst = %dst, prefix, via = %via, "add route v4");
        let msg = route(
            AddressFamily::Inet,
            Some((dst.into(), prefix)),
            Some(via.into()),
            None,
        );
        self.create(RouteNetlinkMessage::NewRoute(msg)).await
    }

    /// Replaces (or creates) a route for an IPv4 prefix via a gateway.
    ///
    /// Equivalent to `ip route replace <dst>/<prefix> via <via>`.
    pub(crate) async fn replace_route_v4(
        &self,
        dst: Ipv4Addr,
        prefix: u8,
        via: Ipv4Addr,
    ) -> Result<()> {
        trace!(dst = %dst, prefix, via = %via, "replace route v4");
        let msg = route(
            AddressFamily::Inet,
            Some((dst.into(), prefix)),
            Some(via.into()),
            None,
        );
        self.tell(RouteNetlinkMessage::NewRoute(msg), REPLACE).await
    }

    /// Adds a route for an IPv4 prefix via a specific output device (no gateway).
    pub(crate) async fn add_route_v4_dev(
        &self,
        dst: Ipv4Addr,
        prefix: u8,
        dev: &str,
    ) -> Result<()> {
        trace!(dst = %dst, prefix, dev = %dev, "add route v4 dev");
        let idx = self.link_index(dev).await?;
        let msg = route(
            AddressFamily::Inet,
            Some((dst.into(), prefix)),
            None,
            Some(idx),
        );
        self.create(RouteNetlinkMessage::NewRoute(msg)).await
    }

    /// Adds an IPv6 default route via `via`, ignoring EEXIST.
    pub(crate) async fn add_default_route_v6(&self, via: Ipv6Addr) -> Result<()> {
        trace!(via = %via, "add default route v6");
        let msg = route(AddressFamily::Inet6, None, Some(via.into()), None);
        self.create(RouteNetlinkMessage::NewRoute(msg)).await
    }

    /// Adds a scoped IPv6 default route via `ifname`, ignoring EEXIST.
    pub(crate) async fn add_default_route_v6_scoped(
        &self,
        ifname: &str,
        via: Ipv6Addr,
    ) -> Result<()> {
        trace!(ifname = %ifname, via = %via, "add scoped default route v6");
        let idx = self.link_index(ifname).await?;
        let msg = route(AddressFamily::Inet6, None, Some(via.into()), Some(idx));
        self.create(RouteNetlinkMessage::NewRoute(msg)).await
    }

    /// Replaces the IPv6 default route via `ifname` (deletes all existing defaults first).
    pub(crate) async fn replace_default_route_v6(&self, ifname: &str, via: Ipv6Addr) -> Result<()> {
        trace!(ifname = %ifname, via = %via, "replace default route v6");
        let idx = self.link_index(ifname).await?;
        self.delete_default_routes(AddressFamily::Inet6).await?;
        let msg = route(AddressFamily::Inet6, None, Some(via.into()), Some(idx));
        self.tell(RouteNetlinkMessage::NewRoute(msg), CREATE).await
    }

    /// Sets the IPv6 default route for a device interface, handling link-local
    /// vs global dispatch. Pass `None` to clear the default route.
    pub(crate) async fn set_default_route_v6(
        &self,
        ifname: &str,
        via: Option<Ipv6Addr>,
    ) -> Result<()> {
        match via {
            Some(gw) => self.replace_default_route_v6(ifname, gw).await,
            None => self.clear_default_route_v6().await,
        }
    }

    pub(crate) async fn clear_default_route_v6(&self) -> Result<()> {
        trace!("clear default route v6");
        self.delete_default_routes(AddressFamily::Inet6).await
    }

    /// Deletes every default route of `family`; a failed delete is skipped.
    async fn delete_default_routes(&self, family: AddressFamily) -> Result<()> {
        let mut query = RouteMessage::default();
        query.header.address_family = family;
        let replies = self
            .ask(RouteNetlinkMessage::GetRoute(query), NLM_F_DUMP)
            .await?;
        for reply in replies {
            if let RouteNetlinkMessage::NewRoute(old) = reply {
                if old.header.destination_prefix_length == 0 {
                    let _ = self.tell(RouteNetlinkMessage::DelRoute(old), 0).await;
                }
            }
        }
        Ok(())
    }

    /// Adds a route for an IPv6 prefix via a gateway, ignoring EEXIST.
    pub(crate) async fn add_route_v6(
        &self,
        dst: Ipv6Addr,
        prefix: u8,
        via: Ipv6Addr,
    ) -> Result<()> {
        trace!(dst = %dst, prefix, via = %via, "add route v6");
        let msg = route(
            AddressFamily::Inet6,
            Some((dst.into(), prefix)),
            Some(via.into()),
            None,
        );
        self.create(RouteNetlinkMessage::NewRoute(msg)).await
    }

    /// Adds a route for an IPv6 prefix via a specific output device (no gateway).
    pub(crate) async fn add_route_v6_dev(
        &self,
        dst: Ipv6Addr,
        prefix: u8,
        dev: &str,
    ) -> Result<()> {
        trace!(dst = %dst, prefix, dev = %dev, "add route v6 dev");
        let idx = self.link_index(dev).await?;
        let msg = route(
            AddressFamily::Inet6,
            Some((dst.into(), prefix)),
            None,
            Some(idx),
        );
        self.create(RouteNetlinkMessage::NewRoute(msg)).await
    }
}

fn new_link(name: &str, kind: InfoKind, data: Option<InfoData>) -> LinkMessage {
    let mut info = vec![LinkInfo::Kind(kind)];
    info.extend(data.map(LinkInfo::Data));
    let mut msg = LinkMessage::default();
    msg.attributes.push(LinkAttribute::IfName(name.to_string()));
    msg.attributes.push(LinkAttribute::LinkInfo(info));
    msg
}

/// A unicast route in the main table with protocol static, as `ip route add` makes it.
fn route(
    family: AddressFamily,
    dst: Option<(IpAddr, u8)>,
    via: Option<IpAddr>,
    oif: Option<u32>,
) -> RouteMessage {
    let mut msg = RouteMessage::default();
    msg.header.address_family = family;
    msg.header.table = RouteHeader::RT_TABLE_MAIN;
    msg.header.protocol = RouteProtocol::Static;
    msg.header.scope = RouteScope::Universe;
    msg.header.kind = RouteType::Unicast;
    if let Some((ip, prefix)) = dst {
        msg.header.destination_prefix_length = prefix;
        msg.attributes
            .push(RouteAttribute::Destination(route_address(ip)));
    }
    if let Some(ip) = via {
        msg.attributes
            .push(RouteAttribute::Gateway(route_address(ip)));
    }
    if let Some(idx) = oif {
        msg.attributes.push(RouteAttribute::Oif(idx));
    }
    msg
}

fn route_address(ip: IpAddr) -> RouteAddress {
    match ip {
        IpAddr::V4(v4) => RouteAddress::Inet(v4),
        IpAddr::V6(v6) => RouteAddress::Inet6(v6),
    }
}

fn family(ip: IpAddr) -> AddressFamily {
    match ip {
        IpAddr::V4(_) => AddressFamily::Inet,
        IpAddr::V6(_) => AddressFamily::Inet6,
    }
}

fn is_eexist(err: &anyhow::Error) -> bool {
    err.downcast_ref::<io::Error>()
        .and_then(io::Error::raw_os_error)
        == Some(libc::EEXIST)
}
