//! nftables rule generation, NAT application, and tc impairment.

use std::net::{Ipv4Addr, Ipv6Addr};

use anyhow::{anyhow, Context, Result};
use ipnet::Ipv6Net;
use tracing::{debug, trace};

use crate::{
    core::RouterConfig, netns, qdisc, wiring::set_sysctl_root, ConntrackTimeouts, LinkCondition,
    NatConfig, NatFiltering, NatMapping, NatV6Mode,
};

/// Applies nftables rules (assumes caller is already in the target namespace).
async fn run_nft(rules: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let nft = if std::path::Path::new("/usr/sbin/nft").exists() {
        "/usr/sbin/nft"
    } else {
        "nft"
    };
    let mut child = tokio::process::Command::new(nft)
        .args(["-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .context("spawn nft")?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(rules.as_bytes())
        .await
        .context("write nft stdin")?;
    let st = child.wait().await.context("wait nft")?;
    if st.success() {
        Ok(())
    } else {
        Err(anyhow!("nft apply failed"))
    }
}

/// Applies nftables rules inside `ns` on the namespace's async worker.
pub(crate) async fn run_nft_in(netns: &netns::NetnsManager, ns: &str, rules: &str) -> Result<()> {
    trace!(ns = %ns, rules = %rules, "nft: apply rules");
    let rules = rules.to_string();
    let rt = netns.rt_handle_for(ns)?;
    rt.spawn(async move { run_nft(&rules).await })
        .await
        .context("nft task panicked")?
}

/// Generates nftables rules for a [`NatConfig`].
///
/// EIM uses a dynamic fullcone map to preserve source ports across destinations.
/// EDM uses `masquerade random` for per-flow port randomization.
/// EIF adds unconditional fullcone DNAT in prerouting.
/// APDF adds a forward filter that only allows established/related flows.
fn generate_nat_rules(cfg: &NatConfig, wan_if: &str, wan_ip: Ipv4Addr) -> String {
    let use_fullcone_map = cfg.mapping == NatMapping::EndpointIndependent;
    let hairpin = cfg.hairpin;

    let map_decl = if use_fullcone_map {
        r#"    map fullcone {
        type inet_service : ipv4_addr . inet_service
        flags dynamic,timeout
        timeout 300s
        size 65536
    }"#
    } else {
        ""
    };

    // Prerouting: for EIM, DNAT via fullcone map so inbound UDP reaches
    // the correct internal host.  For EDM, an empty prerouting chain is
    // still needed for conntrack reverse-NAT on reply packets.
    //
    // With hairpin: match on `ip daddr <wan_ip>` instead of `iif "<wan>"` so
    // packets from the LAN side destined to the router's public IP also get
    // DNAT'd.
    let prerouting_rules = if use_fullcone_map {
        if hairpin {
            format!(
                r#"        ip daddr {ip} meta l4proto udp dnat to udp dport map @fullcone"#,
                ip = wan_ip
            )
        } else {
            format!(
                r#"        iif "{wan}" meta l4proto udp dnat to udp dport map @fullcone"#,
                wan = wan_if
            )
        }
    } else if hairpin {
        // EDM + hairpin: redirect traffic destined for the WAN IP back.
        format!(r#"        ip daddr {ip} redirect"#, ip = wan_ip,)
    } else {
        String::new()
    };

    // Postrouting: EIM uses snat + fullcone map update. EDM uses masquerade random.
    // With hairpin: masquerade DNAT'd packets so the return path goes through
    // the router (otherwise the LAN peer replies directly, confusing conntrack).
    let hairpin_masq = if hairpin {
        "        ct status dnat masquerade\n".to_string()
    } else {
        String::new()
    };

    let postrouting_rules = if use_fullcone_map {
        format!(
            r#"{hairpin}        oif "{wan}" meta l4proto udp update @fullcone {{ udp sport timeout 300s : ip saddr . udp sport }}
        oif "{wan}" snat to {ip}"#,
            hairpin = hairpin_masq,
            wan = wan_if,
            ip = wan_ip,
        )
    } else {
        format!(
            r#"{hairpin}        oif "{wan}" masquerade random"#,
            hairpin = hairpin_masq,
            wan = wan_if,
        )
    };

    let postrouting_priority = if use_fullcone_map { "srcnat" } else { "100" };

    // APDF filter: forward inbound packets only when they match an
    // existing conntrack flow, a DNAT that the router itself installed
    // (port mapping server), or a related flow. Static port forwards from
    // the portmap table create `ct status dnat` packets that would
    // otherwise be dropped by the blanket `iif "wan" drop` rule.
    let filter_table = if cfg.filtering == NatFiltering::AddressAndPortDependent {
        format!(
            r#"
table ip filter {{
    chain forward {{
        type filter hook forward priority 0; policy accept;
        iif "{wan}" ct state established,related accept
        iif "{wan}" ct status dnat accept
        iif "{wan}" drop
    }}
}}"#,
            wan = wan_if
        )
    } else {
        String::new()
    };

    format!(
        r#"
table ip nat {{
{map_decl}
    chain prerouting {{
        type nat hook prerouting priority dstnat; policy accept;
{prerouting_rules}
    }}
    chain postrouting {{
        type nat hook postrouting priority {postrouting_priority}; policy accept;
{postrouting_rules}
    }}
}}
{filter_table}
"#
    )
}

/// Applies NAT rules from a [`NatConfig`] in the given namespace.
pub(crate) async fn apply_nat_config(
    netns: &netns::NetnsManager,
    ns: &str,
    cfg: &NatConfig,
    wan_if: &str,
    wan_ip: Ipv4Addr,
) -> Result<()> {
    let rules = generate_nat_rules(cfg, wan_if, wan_ip);
    run_nft_in(netns, ns, &rules).await?;
    apply_conntrack_timeouts_from_config(netns, ns, &cfg.timeouts)
}

/// Configures conntrack timeouts from a [`ConntrackTimeouts`].
fn apply_conntrack_timeouts_from_config(
    netns: &netns::NetnsManager,
    ns: &str,
    t: &ConntrackTimeouts,
) -> Result<()> {
    let (udp, udp_stream, tcp_est) = (t.udp, t.udp_stream, t.tcp_established);
    netns.run_closure_in(ns, move || {
        set_sysctl_root("net/netfilter/nf_conntrack_udp_timeout", &udp.to_string())?;
        set_sysctl_root(
            "net/netfilter/nf_conntrack_udp_timeout_stream",
            &udp_stream.to_string(),
        )?;
        set_sysctl_root(
            "net/netfilter/nf_conntrack_tcp_timeout_established",
            &tcp_est.to_string(),
        )?;
        Ok(())
    })
}

/// Applies router NAT rules for the configured mode.
///
/// Uses the effective NAT config from the router's [`Nat`] variant.
/// Otherwise expands the [`Nat`] preset via [`Nat::to_config`].
/// CGNAT and None are handled separately.
pub(crate) async fn apply_nat_for_router(
    netns: &netns::NetnsManager,
    ns: &str,
    router_cfg: &RouterConfig,
    wan_if: &str,
    wan_ip: Ipv4Addr,
) -> Result<()> {
    match router_cfg.effective_nat_config() {
        None => Ok(()),
        Some(cfg) => apply_nat_config(netns, ns, &cfg, wan_if, wan_ip).await,
    }
}

/// Derives a unique WAN /64 for NPTv6 from a router's upstream IP.
///
/// For an IX-level router with upstream IP `2001:db8::11` on IX CIDR `2001:db8::/64`,
/// this produces `2001:db8:0:11::/64` — a unique /64 outside the IX on-link range
/// that matches the LAN-side /64 prefix length required by NPTv6.
///
/// For sub-routers where the upstream is already on a /64 parent LAN, we use the host
/// part of the upstream IP to derive a /64 within the parent's subnet space.
pub(crate) fn nptv6_wan_prefix(upstream_ip: Ipv6Addr, lan_prefix_len: u8) -> Ipv6Net {
    // Place the host portion (last segment) of the upstream IP into segment 3,
    // zeroing segments 4-7 to form a clean /64 network prefix.
    let seg = upstream_ip.segments();
    let host = seg[7];
    let wan_net = Ipv6Addr::new(seg[0], seg[1], seg[2], host, 0, 0, 0, 0);
    Ipv6Net::new(wan_net, lan_prefix_len).unwrap_or_else(|_| Ipv6Net::new(wan_net, 64).unwrap())
}

/// Applies IPv6 NAT rules in `ns`.
pub(crate) async fn apply_nat_v6(
    netns: &netns::NetnsManager,
    ns: &str,
    mode: NatV6Mode,
    wan_if: &str,
    lan_prefix: Ipv6Net,
    wan_prefix: Ipv6Net,
) -> Result<()> {
    match mode {
        NatV6Mode::None => Ok(()),
        NatV6Mode::Nptv6 => {
            // Match only packets within the LAN/WAN prefix ranges so that
            // NDP, ICMPv6, and other traffic to/from the router's own IX
            // address is not inadvertently translated.
            let rules = format!(
                r#"
table ip6 nat {{
    chain postrouting {{
        type nat hook postrouting priority 100; policy accept;
        oif "{wan}" ip6 saddr {lan_pfx} snat prefix to {wan_pfx}
    }}
    chain prerouting {{
        type nat hook prerouting priority -100; policy accept;
        iif "{wan}" ip6 daddr {wan_pfx} dnat prefix to {lan_pfx}
    }}
}}
"#,
                wan = wan_if,
                wan_pfx = wan_prefix,
                lan_pfx = lan_prefix,
            );
            run_nft_in(netns, ns, &rules).await
        }
        NatV6Mode::Masquerade => {
            let rules = format!(
                r#"
table ip6 nat {{
    chain postrouting {{
        type nat hook postrouting priority 100; policy accept;
        oif "{wan}" masquerade
    }}
    chain forward {{
        type filter hook forward priority 0; policy accept;
        meta l4proto ipv6-icmp accept
    }}
}}
"#,
                wan = wan_if,
            );
            run_nft_in(netns, ns, &rules).await
        }
        NatV6Mode::Nat64 => {
            // NAT64 is handled separately in setup_nat64 — the apply_nat_v6
            // call is a no-op for this mode (the SIIT translator and nft
            // masquerade are set up after the router's downlink bridge exists).
            Ok(())
        }
    }
}

/// Generates nftables rules from a [`FirewallConfig`].
///
/// Uses a separate `table inet fw` at priority 10 to avoid conflicts with the
/// NAT filter table (`ip filter` at priority 0). Handles both inbound blocking
/// and outbound port restrictions in a single unified chain.
fn generate_firewall_rules(cfg: &crate::firewall::FirewallConfig, wan_if: &str) -> String {
    use crate::firewall::PortPolicy;

    let mut rules = String::new();
    rules.push_str("table inet fw {\n");
    rules.push_str("    chain forward {\n");
    rules.push_str("        type filter hook forward priority 10; policy accept;\n");
    rules.push_str("        ct state established,related accept\n");

    // Block unsolicited inbound on the WAN interface (RFC 6092).
    if cfg.block_inbound {
        rules.push_str(&format!("        iif \"{}\" drop\n", wan_if));
    }

    // Outbound TCP policy.
    match &cfg.outbound_tcp {
        PortPolicy::AllowAll => {}
        PortPolicy::Allow(ports) if !ports.is_empty() => {
            let ports: Vec<String> = ports.iter().map(|p| p.to_string()).collect();
            rules.push_str(&format!(
                "        tcp dport {{ {} }} accept\n",
                ports.join(", ")
            ));
            rules.push_str("        meta l4proto tcp drop\n");
        }
        // Allow(empty) or BlockAll → drop all TCP.
        _ => {
            rules.push_str("        meta l4proto tcp drop\n");
        }
    }

    // Outbound UDP policy.
    match &cfg.outbound_udp {
        PortPolicy::AllowAll => {}
        PortPolicy::Allow(ports) if !ports.is_empty() => {
            let ports: Vec<String> = ports.iter().map(|p| p.to_string()).collect();
            rules.push_str(&format!(
                "        udp dport {{ {} }} accept\n",
                ports.join(", ")
            ));
            rules.push_str("        meta l4proto udp drop\n");
        }
        // Allow(empty) or BlockAll → drop all UDP.
        _ => {
            rules.push_str("        meta l4proto udp drop\n");
        }
    }

    rules.push_str("    }\n");
    rules.push_str("}\n");
    rules
}

/// Applies firewall rules for a router. No-op for [`Firewall::None`].
pub(crate) async fn apply_firewall(
    netns: &netns::NetnsManager,
    ns: &str,
    firewall: &crate::Firewall,
    wan_if: &str,
) -> Result<()> {
    match firewall.to_config() {
        None => Ok(()),
        Some(cfg) => {
            let rules = generate_firewall_rules(&cfg, wan_if);
            run_nft_in(netns, ns, &rules).await
        }
    }
}

/// Removes firewall rules by flushing the `inet fw` table.
pub(crate) async fn remove_firewall(netns: &netns::NetnsManager, ns: &str) -> Result<()> {
    // Flush and delete; ignore errors (table may not exist).
    run_nft_in(netns, ns, "delete table inet fw\n").await.ok();
    // Also clean up legacy `ip fw` table from older configurations.
    run_nft_in(netns, ns, "delete table ip fw\n").await.ok();
    Ok(())
}

/// Applies ICMP fragmentation-needed blocking rule.
pub(crate) async fn apply_icmp_frag_block(netns: &netns::NetnsManager, ns: &str) -> Result<()> {
    run_nft_in(
        netns,
        ns,
        r#"
table ip filter {
    chain forward {
        type filter hook forward priority 0; policy accept;
        icmp type destination-unreachable icmp code frag-needed drop
    }
}
"#,
    )
    .await
}

/// Applies an impairment preset or manual limits on `ifname` inside `ns`.
pub(crate) async fn apply_impair_in(
    netns: &netns::NetnsManager,
    ns: &str,
    ifname: &str,
    impair: LinkCondition,
) {
    debug!(ns = %ns, ifname = %ifname, impair = ?impair, "tc: apply impairment");
    let limits = impair.to_limits();
    let ifname = ifname.to_string();
    let rt = match netns.rt_handle_for(ns) {
        Ok(rt) => rt,
        Err(e) => {
            tracing::warn!(ns = %ns, error = %e, "apply_impair_in: no rt handle");
            return;
        }
    };
    if let Err(e) = rt
        .spawn(async move { qdisc::apply_impair(&ifname, limits).await })
        .await
    {
        tracing::warn!(ns = %ns, error = %e, "apply_impair_in failed");
    }
}

/// Applies or removes impairment on `ifname` inside `ns`.
pub(crate) async fn apply_or_remove_impair(
    netns: &netns::NetnsManager,
    ns: &str,
    ifname: &str,
    impair: Option<LinkCondition>,
) {
    match impair {
        Some(imp) => apply_impair_in(netns, ns, ifname, imp).await,
        None => {
            let ifname = ifname.to_string();
            let rt = match netns.rt_handle_for(ns) {
                Ok(rt) => rt,
                Err(_) => return,
            };
            let _ = rt
                .spawn(async move { qdisc::remove_qdisc(&ifname).await })
                .await;
        }
    }
}
