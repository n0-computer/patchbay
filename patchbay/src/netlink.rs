use std::{
    fs::File,
    net::{Ipv4Addr, Ipv6Addr},
    os::fd::AsRawFd,
};

use anyhow::{anyhow, Result};
use futures::stream::TryStreamExt;
use rtnetlink::{Handle, LinkBridge, LinkUnspec, LinkVeth, RouteMessageBuilder};
use tracing::trace;

/// Wraps an rtnetlink `Handle` for namespace-scoped network operations.
///
/// `Clone` is cheap; `Handle` is an `Arc`-based channel sender.
#[derive(Clone)]
pub(crate) struct Netlink {
    handle: Handle,
}

impl Netlink {
    pub(crate) fn new(handle: Handle) -> Self {
        Self { handle }
    }

    pub(crate) async fn link_index(&self, ifname: &str) -> Result<u32> {
        let mut links = self
            .handle
            .link()
            .get()
            .match_name(ifname.to_string())
            .execute();
        links
            .try_next()
            .await?
            .map(|msg| msg.header.index)
            .ok_or_else(|| anyhow!("link not found: {}", ifname))
    }

    pub(crate) async fn ensure_link_deleted(&self, ifname: &str) -> Result<()> {
        if let Ok(idx) = self.link_index(ifname).await {
            trace!(ifname = %ifname, "delete link");
            self.handle.link().del(idx).execute().await?;
        }
        Ok(())
    }

    pub(crate) async fn add_bridge(&self, name: &str) -> Result<()> {
        trace!(bridge = %name, "add bridge");
        if let Err(err) = self
            .handle
            .link()
            .add(LinkBridge::new(name).build())
            .execute()
            .await
        {
            if is_eexist(&err) {
                trace!(bridge = %name, "bridge already exists");
            } else {
                return Err(err.into());
            }
        }

        Ok(())
    }

    pub(crate) async fn add_veth(&self, a: &str, b: &str) -> Result<()> {
        trace!(a = %a, b = %b, "add veth pair");
        self.handle
            .link()
            .add(LinkVeth::new(a, b).build())
            .execute()
            .await?;
        Ok(())
    }

    pub(crate) async fn set_link_up(&self, ifname: &str) -> Result<()> {
        trace!(ifname = %ifname, "set link up");
        let idx = self.link_index(ifname).await?;
        let msg = LinkUnspec::new_with_index(idx).up().build();
        self.handle.link().change(msg).execute().await?;
        Ok(())
    }

    pub(crate) async fn set_link_down(&self, ifname: &str) -> Result<()> {
        trace!(ifname = %ifname, "set link down");
        let idx = self.link_index(ifname).await?;
        let msg = LinkUnspec::new_with_index(idx).down().build();
        self.handle.link().change(msg).execute().await?;
        Ok(())
    }

    /// Sets the MTU on an interface.
    pub(crate) async fn set_mtu(&self, ifname: &str, mtu: u32) -> Result<()> {
        trace!(ifname = %ifname, mtu, "set mtu");
        let idx = self.link_index(ifname).await?;
        let msg = LinkUnspec::new_with_index(idx).mtu(mtu).build();
        self.handle.link().change(msg).execute().await?;
        Ok(())
    }

    pub(crate) async fn rename_link(&self, from: &str, to: &str) -> Result<()> {
        trace!(from = %from, to = %to, "rename link");
        let idx = self.link_index(from).await?;
        let msg = LinkUnspec::new_with_index(idx).name(to.to_string()).build();
        self.handle.link().change(msg).execute().await?;
        Ok(())
    }

    pub(crate) async fn set_master(&self, ifname: &str, master: &str) -> Result<()> {
        trace!(ifname = %ifname, master = %master, "set master");
        let idx = self.link_index(ifname).await?;
        let midx = self.link_index(master).await?;
        let msg = LinkUnspec::new_with_index(idx).controller(midx).build();
        self.handle.link().set(msg).execute().await?;
        Ok(())
    }

    pub(crate) async fn move_link_to_netns(&self, ifname: &str, ns_fd: &File) -> Result<()> {
        trace!(ifname = %ifname, "move link to netns");
        let idx = self.link_index(ifname).await?;
        let msg = LinkUnspec::new_with_index(idx)
            .setns_by_fd(ns_fd.as_raw_fd())
            .build();
        self.handle.link().change(msg).execute().await?;
        Ok(())
    }

    /// Removes an IPv4 address from an interface.
    pub(crate) async fn del_addr4(&self, ifname: &str, ip: Ipv4Addr, prefix: u8) -> Result<()> {
        trace!(ifname = %ifname, ip = %ip, prefix, "del addr4");
        let idx = self.link_index(ifname).await?;
        let mut addrs = self
            .handle
            .address()
            .get()
            .set_link_index_filter(idx)
            .set_address_filter(ip.into())
            .execute();
        while let Some(addr) = addrs.try_next().await? {
            if addr.header.prefix_len == prefix {
                self.handle.address().del(addr).execute().await?;
                return Ok(());
            }
        }
        Ok(())
    }

    pub(crate) async fn add_addr4(&self, ifname: &str, ip: Ipv4Addr, prefix: u8) -> Result<()> {
        trace!(ifname = %ifname, ip = %ip, prefix, "add addr4");
        let idx = self.link_index(ifname).await?;
        if let Err(err) = self
            .handle
            .address()
            .add(idx, ip.into(), prefix)
            .execute()
            .await
        {
            if is_eexist(&err) {
                return Ok(());
            }
            return Err(err.into());
        }
        Ok(())
    }

    pub(crate) async fn add_default_route_v4(&self, via: Ipv4Addr) -> Result<()> {
        trace!(via = %via, "add default route v4");
        let msg = RouteMessageBuilder::<Ipv4Addr>::new().gateway(via).build();
        if let Err(err) = self.handle.route().add(msg).execute().await {
            if is_eexist(&err) {
                return Ok(());
            }
            return Err(err.into());
        }
        Ok(())
    }

    pub(crate) async fn replace_default_route_v4(&self, ifname: &str, via: Ipv4Addr) -> Result<()> {
        trace!(ifname = %ifname, via = %via, "replace default route v4");
        let ifindex = self.link_index(ifname).await?;

        let mut routes = self
            .handle
            .route()
            .get(RouteMessageBuilder::<Ipv4Addr>::new().build())
            .execute();
        while let Some(route) = routes.try_next().await? {
            if route.header.destination_prefix_length == 0 {
                let _ = self.handle.route().del(route).execute().await;
            }
        }

        let msg = RouteMessageBuilder::<Ipv4Addr>::new()
            .output_interface(ifindex)
            .gateway(via)
            .build();
        self.handle.route().add(msg).execute().await?;
        Ok(())
    }

    pub(crate) async fn add_route_v4(
        &self,
        dst: Ipv4Addr,
        prefix: u8,
        via: Ipv4Addr,
    ) -> Result<()> {
        trace!(dst = %dst, prefix, via = %via, "add route v4");
        let msg = RouteMessageBuilder::<Ipv4Addr>::new()
            .destination_prefix(dst, prefix)
            .gateway(via)
            .build();
        if let Err(err) = self.handle.route().add(msg).execute().await {
            if is_eexist(&err) {
                return Ok(());
            }
            return Err(err.into());
        }
        Ok(())
    }

    // ── IPv6 methods ──

    pub(crate) async fn add_addr6(&self, ifname: &str, ip: Ipv6Addr, prefix: u8) -> Result<()> {
        trace!(ifname = %ifname, ip = %ip, prefix, "add addr6");
        let idx = self.link_index(ifname).await?;
        if let Err(err) = self
            .handle
            .address()
            .add(idx, ip.into(), prefix)
            .execute()
            .await
        {
            if is_eexist(&err) {
                return Ok(());
            }
            return Err(err.into());
        }
        Ok(())
    }

    pub(crate) async fn add_default_route_v6(&self, via: Ipv6Addr) -> Result<()> {
        trace!(via = %via, "add default route v6");
        let msg = RouteMessageBuilder::<Ipv6Addr>::new().gateway(via).build();
        if let Err(err) = self.handle.route().add(msg).execute().await {
            if is_eexist(&err) {
                return Ok(());
            }
            return Err(err.into());
        }
        Ok(())
    }

    pub(crate) async fn add_route_v6(
        &self,
        dst: Ipv6Addr,
        prefix: u8,
        via: Ipv6Addr,
    ) -> Result<()> {
        trace!(dst = %dst, prefix, via = %via, "add route v6");
        let msg = RouteMessageBuilder::<Ipv6Addr>::new()
            .destination_prefix(dst, prefix)
            .gateway(via)
            .build();
        if let Err(err) = self.handle.route().add(msg).execute().await {
            if is_eexist(&err) {
                return Ok(());
            }
            return Err(err.into());
        }
        Ok(())
    }
}

fn is_eexist(err: &rtnetlink::Error) -> bool {
    match err {
        rtnetlink::Error::NetlinkError(msg) => msg
            .code
            .map(|code| -code.get() == libc::EEXIST)
            .unwrap_or(false),
        _ => false,
    }
}
