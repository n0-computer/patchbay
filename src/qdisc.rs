use anyhow::{bail, Result};
use ipnet::Ipv4Net;
use std::process::Command;

use crate::core::run_command_in_namespace;

#[derive(Debug, Clone, Copy)]
pub(crate) struct ImpairLimits {
    pub(crate) rate_kbit: u32,
    pub(crate) loss_pct: f32,
    pub(crate) latency_ms: u32,
}

pub(crate) fn apply_impair(ns: &str, ifname: &str, limits: ImpairLimits) -> Result<()> {
    remove_qdisc(ns, ifname);
    let qdisc = Qdisc::new(ifname);
    qdisc.add_netem_root(ns, limits)?;
    if limits.rate_kbit > 0 {
        qdisc.add_tbf(ns, limits.rate_kbit)?;
    }
    Ok(())
}

pub(crate) fn apply_region_latency(
    ns: &str,
    ifname: &str,
    filters: &[(Ipv4Net, u32)],
) -> Result<()> {
    if filters.is_empty() {
        return Ok(());
    }

    remove_qdisc(ns, ifname);
    let qdisc = Qdisc::new(ifname);
    qdisc.add_htb_root(ns)?;
    qdisc.add_base_class(ns)?;

    for (idx, (cidr, latency)) in filters.iter().enumerate() {
        let class_id = format!("1:{}", 10 + idx as u16);
        let handle = format!("{}:", 10 + idx as u16);
        let cidr_str = format!("{}/{}", cidr.addr(), cidr.prefix_len());

        qdisc.add_htb_class(ns, &class_id)?;
        qdisc.add_netem_class(ns, &class_id, &handle, *latency)?;
        qdisc.add_filter(ns, &cidr_str, &class_id)?;
    }

    Ok(())
}

pub(crate) fn remove_qdisc(ns: &str, ifname: &str) {
    let qdisc = Qdisc::new(ifname);
    qdisc.clear_root(ns);
}

struct Qdisc<'a> {
    ifname: &'a str,
}

impl<'a> Qdisc<'a> {
    fn new(ifname: &'a str) -> Self {
        Self { ifname }
    }

    fn clear_root(&self, ns: &str) {
        let _ = run_command_in_namespace(ns, {
            let mut cmd = Command::new("tc");
            cmd.args(["qdisc", "del", "dev", self.ifname, "root"]);
            cmd.stderr(std::process::Stdio::null());
            cmd
        });
    }

    fn add_netem_root(&self, ns: &str, limits: ImpairLimits) -> Result<()> {
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
        ensure_success(ns, cmd, "tc qdisc netem add")?;
        Ok(())
    }

    fn add_tbf(&self, ns: &str, rate_kbit: u32) -> Result<()> {
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
        ensure_success(ns, cmd, "tc qdisc tbf add")?;
        Ok(())
    }

    fn add_htb_root(&self, ns: &str) -> Result<()> {
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
        ensure_success(ns, cmd, "tc qdisc htb add")?;
        Ok(())
    }

    fn add_base_class(&self, ns: &str) -> Result<()> {
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
        ensure_success(ns, cmd, "tc class htb add base")?;
        Ok(())
    }

    fn add_htb_class(&self, ns: &str, class_id: &str) -> Result<()> {
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
        ensure_success(ns, cmd, "tc class htb add")?;
        Ok(())
    }

    fn add_netem_class(
        &self,
        ns: &str,
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
        ensure_success(ns, cmd, "tc qdisc netem class add")?;
        Ok(())
    }

    fn add_filter(&self, ns: &str, cidr: &str, class_id: &str) -> Result<()> {
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
        ensure_success(ns, cmd, "tc filter add")?;
        Ok(())
    }
}

fn ensure_success(ns: &str, cmd: Command, context: &str) -> Result<()> {
    let status = run_command_in_namespace(ns, cmd)?;
    if status.success() {
        Ok(())
    } else {
        bail!("{} failed for {}", context, ns);
    }
}
