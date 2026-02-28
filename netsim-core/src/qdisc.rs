use std::process::Command;

use anyhow::{bail, Context, Result};
use ipnet::IpNet;

/// Parameters for `tc netem` impairment.
///
/// All fields default to zero (no impairment). Set only the fields you need.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LinkLimits {
    /// Rate limit in kbit/s (0 = unlimited).
    pub rate_kbit: u32,
    /// Packet loss percentage (0.0–100.0).
    pub loss_pct: f32,
    /// One-way latency in milliseconds.
    pub latency_ms: u32,
    /// Jitter in milliseconds (uniform ±jitter around latency).
    pub jitter_ms: u32,
    /// Packet reordering percentage (0.0–100.0).
    pub reorder_pct: f32,
    /// Packet duplication percentage (0.0–100.0).
    pub duplicate_pct: f32,
    /// Bit-error corruption percentage (0.0–100.0).
    pub corrupt_pct: f32,
}

/// Applies netem impairment on `ifname`. Caller must already be in the target ns.
pub(crate) fn apply_impair(ifname: &str, limits: LinkLimits) -> Result<()> {
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
pub(crate) fn apply_region_latency_dual(ifname: &str, filters: &[(IpNet, u32)]) -> Result<()> {
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

    fn add_netem_root(&self, limits: LinkLimits) -> Result<()> {
        let mut args: Vec<String> = vec![
            "qdisc",
            "add",
            "dev",
            self.ifname,
            "root",
            "handle",
            "1:",
            "netem",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        if limits.latency_ms > 0 || limits.jitter_ms > 0 {
            args.push("delay".into());
            args.push(format!("{}ms", limits.latency_ms));
            if limits.jitter_ms > 0 {
                args.push(format!("{}ms", limits.jitter_ms));
            }
        }
        if limits.loss_pct > 0.0 {
            args.push("loss".into());
            args.push(format!("{:.3}%", limits.loss_pct));
        }
        if limits.reorder_pct > 0.0 {
            args.push("reorder".into());
            args.push(format!("{:.3}%", limits.reorder_pct));
        }
        if limits.duplicate_pct > 0.0 {
            args.push("duplicate".into());
            args.push(format!("{:.3}%", limits.duplicate_pct));
        }
        if limits.corrupt_pct > 0.0 {
            args.push("corrupt".into());
            args.push(format!("{:.3}%", limits.corrupt_pct));
        }

        let mut cmd = Command::new("tc");
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        cmd.args(&arg_refs);
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
        ensure_success(cmd, "tc class htb add")?;
        Ok(())
    }

    fn add_netem_class(&self, class_id: &str, handle: &str, latency_ms: u32) -> Result<()> {
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
        ensure_success(cmd, "tc filter v6 add")?;
        Ok(())
    }
}

fn ensure_success(mut cmd: Command, context: &str) -> Result<()> {
    let out = cmd
        .stderr(std::process::Stdio::piped())
        .output()
        .with_context(|| format!("{context}: spawn"))?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("{context} failed: {stderr}");
    }
}
