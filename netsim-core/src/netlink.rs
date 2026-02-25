use anyhow::{anyhow, Result};
use futures::stream::TryStreamExt;
use rtnetlink::{Handle, LinkBridge, LinkUnspec, LinkVeth, RouteMessageBuilder};
use std::fs::File;
use std::net::Ipv4Addr;
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};
use tracing::debug;

pub(crate) struct Netlink {
    handle: Handle,
    /// Receives names of links created by this handle; `None` for read-only contexts.
    tracker: Option<Arc<Mutex<Vec<String>>>>,
}

impl Netlink {
    /// Creates a handle that registers every created link into `tracker`.
    pub(crate) fn new_tracked(handle: Handle, tracker: Arc<Mutex<Vec<String>>>) -> Self {
        Self { handle, tracker: Some(tracker) }
    }

    fn register_link(&self, name: &str) {
        // Only track root-namespace links (lab-* veths and br-* bridges).
        // In-namespace interfaces (ix, wan, eth0, …) are cleaned up automatically
        // when their namespace is deleted, so we must not attempt to delete them
        // from outside.
        if !(name.starts_with("lab-") || name.starts_with("br-")) {
            return;
        }
        if let Some(t) = &self.tracker {
            t.lock().unwrap().push(name.to_string());
        }
    }

    /// Returns the underlying rtnetlink `Handle` for low-level operations.
    #[allow(dead_code)]
    pub(crate) fn handle(&self) -> &Handle {
        &self.handle
    }

    pub(crate) async fn link_index(&mut self, ifname: &str) -> Result<u32> {
        debug!(ifname = %ifname, "netlink: lookup link index");
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

    pub(crate) async fn ensure_link_deleted(&mut self, ifname: &str) -> Result<()> {
        debug!(ifname = %ifname, "netlink: ensure link deleted");
        if let Ok(idx) = self.link_index(ifname).await {
            debug!(ifname = %ifname, idx, "netlink: delete link");
            self.handle.link().del(idx).execute().await?;
        }
        Ok(())
    }

    pub(crate) async fn add_bridge(&mut self, name: &str) -> Result<()> {
        debug!(bridge = %name, "netlink: add bridge");
        if let Err(err) = self
            .handle
            .link()
            .add(LinkBridge::new(name).build())
            .execute()
            .await
        {
            if is_eexist(&err) {
                debug!(bridge = %name, "netlink: bridge already exists");
            } else {
                return Err(err.into());
            }
        }
        self.register_link(name);
        Ok(())
    }

    pub(crate) async fn add_veth(&mut self, a: &str, b: &str) -> Result<()> {
        debug!(a = %a, b = %b, "netlink: add veth pair");
        self.handle
            .link()
            .add(LinkVeth::new(a, b).build())
            .execute()
            .await?;
        self.register_link(a);
        self.register_link(b);
        Ok(())
    }

    pub(crate) async fn set_link_up(&mut self, ifname: &str) -> Result<()> {
        debug!(ifname = %ifname, "netlink: set link up");
        let idx = self.link_index(ifname).await?;
        let msg = LinkUnspec::new_with_index(idx).up().build();
        self.handle.link().change(msg).execute().await?;
        Ok(())
    }

    pub(crate) async fn set_link_down(&mut self, ifname: &str) -> Result<()> {
        debug!(ifname = %ifname, "netlink: set link down");
        let idx = self.link_index(ifname).await?;
        let msg = LinkUnspec::new_with_index(idx).down().build();
        self.handle.link().change(msg).execute().await?;
        Ok(())
    }

    pub(crate) async fn rename_link(&mut self, from: &str, to: &str) -> Result<()> {
        debug!(from = %from, to = %to, "netlink: rename link");
        let idx = self.link_index(from).await?;
        let msg = LinkUnspec::new_with_index(idx).name(to.to_string()).build();
        self.handle.link().change(msg).execute().await?;
        Ok(())
    }

    pub(crate) async fn set_master(&mut self, ifname: &str, master: &str) -> Result<()> {
        debug!(ifname = %ifname, master = %master, "netlink: set master");
        let idx = self.link_index(ifname).await?;
        let midx = self.link_index(master).await?;
        let msg = LinkUnspec::new_with_index(idx).controller(midx).build();
        self.handle.link().set(msg).execute().await?;
        Ok(())
    }

    pub(crate) async fn move_link_to_netns(&mut self, ifname: &str, ns_fd: &File) -> Result<()> {
        debug!(ifname = %ifname, "netlink: move link to netns");
        let idx = self.link_index(ifname).await?;
        let msg = LinkUnspec::new_with_index(idx)
            .setns_by_fd(ns_fd.as_raw_fd())
            .build();
        self.handle.link().change(msg).execute().await?;
        Ok(())
    }

    pub(crate) async fn add_addr4(&mut self, ifname: &str, ip: Ipv4Addr, prefix: u8) -> Result<()> {
        debug!(
            ifname = %ifname,
            ip = %ip,
            prefix,
            "netlink: add IPv4 address"
        );
        let idx = self.link_index(ifname).await?;
        if let Err(err) = self
            .handle
            .address()
            .add(idx, ip.into(), prefix)
            .execute()
            .await
        {
            if is_eexist(&err) {
                debug!(
                    ifname = %ifname,
                    ip = %ip,
                    prefix,
                    "netlink: IPv4 address already exists"
                );
                return Ok(());
            }
            return Err(err.into());
        }
        Ok(())
    }

    pub(crate) async fn add_default_route_v4(&mut self, via: Ipv4Addr) -> Result<()> {
        debug!(via = %via, "netlink: add default route");
        let msg = RouteMessageBuilder::<Ipv4Addr>::new().gateway(via).build();
        if let Err(err) = self.handle.route().add(msg).execute().await {
            if is_eexist(&err) {
                debug!(via = %via, "netlink: default route already exists");
                return Ok(());
            }
            return Err(err.into());
        }
        Ok(())
    }

    pub(crate) async fn replace_default_route_v4(
        &mut self,
        ifname: &str,
        via: Ipv4Addr,
    ) -> Result<()> {
        debug!(ifname = %ifname, via = %via, "netlink: replace default route");
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
        &mut self,
        dst: Ipv4Addr,
        prefix: u8,
        via: Ipv4Addr,
    ) -> Result<()> {
        debug!(dst = %dst, prefix, via = %via, "netlink: add route");
        let msg = RouteMessageBuilder::<Ipv4Addr>::new()
            .destination_prefix(dst, prefix)
            .gateway(via)
            .build();
        if let Err(err) = self.handle.route().add(msg).execute().await {
            if is_eexist(&err) {
                debug!(dst = %dst, prefix, via = %via, "netlink: route already exists");
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
            .map(|code| -code.get() == nix::libc::EEXIST)
            .unwrap_or(false),
        _ => false,
    }
}
