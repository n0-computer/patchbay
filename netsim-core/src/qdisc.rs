use std::process::Command;

use anyhow::{bail, Context, Result};
use ipnet::IpNet;

#[derive(Debug, Clone, Copy)]
pub(crate) struct ImpairLimits {
    pub(crate) rate_kbit: u32,
    pub(crate) loss_pct: f32,
    pub(crate) latency_ms: u32,
}

/// Applies netem impairment on `ifname`. Caller must already be in the target ns.
pub(crate) fn apply_impair(ifname: &str, limits: ImpairLimits) -> Result<()> {
    remove_qdisc(ifname);
    let qdisc = Qdisc::new(ifname);
    qdisc.add_netem_root(limits)?;
    if limits.rate_kbit > 0 {
        qdisc.add_tbf(limits.rate_kbit)?;
    }
    Ok(())
}

/// Applies region latency filters for both v4 and v6 CIDRs.
///
/// Each `IpNet` entry maps to either a v4 or v6 tc filter on the same HTB class tree.
/// Caller must already be in the target ns.
pub(crate) fn apply_region_latency_dual(
    ifname: &str,
    filters: &[(IpNet, u32)],
) -> Result<()> {
    if filters.is_empty() {
        return Ok(());
    }

    remove_qdisc(ifname);
    let qdisc = Qdisc::new(ifname);
    qdisc.add_htb_root()?;
    qdisc.add_base_class()?;

    for (idx, (cidr, latency)) in filters.iter().enumerate() {
        let class_id = format!("1:{}", 10 + idx as u16);
        let handle = format!("{}:", 10 + idx as u16);
        let cidr_str = format!("{}/{}", cidr.addr(), cidr.prefix_len());

        qdisc.add_htb_class(&class_id)?;
        qdisc.add_netem_class(&class_id, &handle, *latency)?;
        match cidr {
            IpNet::V4(_) => qdisc.add_filter(&cidr_str, &class_id)?,
            IpNet::V6(_) => qdisc.add_filter_v6(&cidr_str, &class_id)?,
        }
    }

    Ok(())
}

pub(crate) fn remove_qdisc(ifname: &str) {
    let qdisc = Qdisc::new(ifname);
    qdisc.clear_root();
}

struct Qdisc<'a> {
    ifname: &'a str,
}

impl<'a> Qdisc<'a> {
    fn new(ifname: &'a str) -> Self {
        Self { ifname }
    }

    fn clear_root(&self) {
        let mut cmd = Command::new("tc");
        cmd.args(["qdisc", "del", "dev", self.ifname, "root"]);
        cmd.stderr(std::process::Stdio::null());
        let _ = cmd.status();
    }

    fn add_netem_root(&self, limits: ImpairLimits) -> Result<()> {
        let mut cmd = Command::new("tc");
        cmd.args([
            "qdisc",
            "add",
            "dev",
            self.ifname,
            "root",
            "handle",
            "1:",
            "netem",
            "delay",
            &format!("{}ms", limits.latency_ms),
            "loss",
            &format!("{:.3}%", limits.loss_pct),
        ]);
        cmd.stderr(std::process::Stdio::null());
        ensure_success(cmd, "tc qdisc netem add")?;
        Ok(())
    }

    fn add_tbf(&self, rate_kbit: u32) -> Result<()> {
        let mut cmd = Command::new("tc");
        cmd.args([
            "qdisc",
            "add",
            "dev",
            self.ifname,
            "parent",
            "1:1",
            "handle",
            "10:",
            "tbf",
            "rate",
            &format!("{}kbit", rate_kbit),
            "burst",
            "32kbit",
            "latency",
            "400ms",
        ]);
        cmd.stderr(std::process::Stdio::null());
        ensure_success(cmd, "tc qdisc tbf add")?;
        Ok(())
    }

    fn add_htb_root(&self) -> Result<()> {
        let mut cmd = Command::new("tc");
        cmd.args([
            "qdisc",
            "add",
            "dev",
            self.ifname,
            "root",
            "handle",
            "1:",
            "htb",
            "default",
            "1",
            "r2q",
            "1000",
        ]);
        cmd.stderr(std::process::Stdio::null());
        ensure_success(cmd, "tc qdisc htb add")?;
        Ok(())
    }

    fn add_base_class(&self) -> Result<()> {
        let mut cmd = Command::new("tc");
        cmd.args([
            "class",
            "add",
            "dev",
            self.ifname,
            "parent",
            "1:",
            "classid",
            "1:1",
            "htb",
            "rate",
            "1000mbit",
        ]);
        cmd.stderr(std::process::Stdio::null());
        ensure_success(cmd, "tc class htb add base")?;
        Ok(())
    }

    fn add_htb_class(&self, class_id: &str) -> Result<()> {
        let mut cmd = Command::new("tc");
        cmd.args([
            "class",
            "add",
            "dev",
            self.ifname,
            "parent",
            "1:",
            "classid",
            class_id,
            "htb",
            "rate",
            "1000mbit",
        ]);
        cmd.stderr(std::process::Stdio::null());
        ensure_success(cmd, "tc class htb add")?;
        Ok(())
    }

    fn add_netem_class(
        &self,
        class_id: &str,
        handle: &str,
        latency_ms: u32,
    ) -> Result<()> {
        let mut cmd = Command::new("tc");
        cmd.args([
            "qdisc",
            "add",
            "dev",
            self.ifname,
            "parent",
            class_id,
            "handle",
            handle,
            "netem",
            "delay",
            &format!("{}ms", latency_ms),
        ]);
        cmd.stderr(std::process::Stdio::null());
        ensure_success(cmd, "tc qdisc netem class add")?;
        Ok(())
    }

    fn add_filter(&self, cidr: &str, class_id: &str) -> Result<()> {
        let mut cmd = Command::new("tc");
        cmd.args([
            "filter",
            "add",
            "dev",
            self.ifname,
            "protocol",
            "ip",
            "parent",
            "1:",
            "prio",
            "1",
            "u32",
            "match",
            "ip",
            "dst",
            cidr,
            "flowid",
            class_id,
        ]);
        cmd.stderr(std::process::Stdio::null());
        ensure_success(cmd, "tc filter add")?;
        Ok(())
    }

    fn add_filter_v6(&self, cidr: &str, class_id: &str) -> Result<()> {
        let mut cmd = Command::new("tc");
        cmd.args([
            "filter",
            "add",
            "dev",
            self.ifname,
            "protocol",
            "ipv6",
            "parent",
            "1:",
            "prio",
            "2",
            "u32",
            "match",
            "ip6",
            "dst",
            cidr,
            "flowid",
            class_id,
        ]);
        cmd.stderr(std::process::Stdio::null());
        ensure_success(cmd, "tc filter v6 add")?;
        Ok(())
    }
}

fn ensure_success(mut cmd: Command, context: &str) -> Result<()> {
    let status = cmd.status().with_context(|| format!("{context}: spawn"))?;
    if status.success() {
        Ok(())
    } else {
        bail!("{} failed", context);
    }
}
