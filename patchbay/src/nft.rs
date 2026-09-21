//! nftables rule generation, NAT application, and tc impairment.

use std::net::{Ipv4Addr, Ipv6Addr};

use anyhow::{anyhow, Context, Result};
use ipnet::Ipv6Net;
use tracing::{debug, trace};

use crate::{
    core::RouterConfig, netns, qdisc, wiring::set_sysctl_root, ConntrackTimeouts, LinkCondition,
    NatConfig, NatFiltering, NatMapping, NatV6Mode,
};

/// Returns `true` when the NAT uses the fullcone DNAT map (Endpoint-Independent
/// Mapping). This is the single check that both postrouting and prerouting
/// chain generation branch on.
fn uses_fullcone_map(cfg: &NatConfig) -> bool {
    matches!(cfg.mapping, NatMapping::EndpointIndependent)
}

// ── libnftables dlopen fast path ─────────────────────────────────────

mod libnft {
    use std::{
        ffi::{CStr, CString},
        sync::OnceLock,
    };

    use anyhow::{anyhow, Result};

    type NftCtx = *mut std::ffi::c_void;
    type NftCtxNew = unsafe extern "C" fn(flags: u32) -> NftCtx;
    type NftCtxFree = unsafe extern "C" fn(ctx: NftCtx);
    type NftRunCmdFromBuffer =
        unsafe extern "C" fn(ctx: NftCtx, buf: *const std::ffi::c_char) -> i32;
    type NftCtxBufferOutput = unsafe extern "C" fn(ctx: NftCtx) -> i32;
    type NftCtxGetOutputBuffer = unsafe extern "C" fn(ctx: NftCtx) -> *const std::ffi::c_char;
    type NftCtxBufferError = unsafe extern "C" fn(ctx: NftCtx) -> i32;
    type NftCtxGetErrorBuffer = unsafe extern "C" fn(ctx: NftCtx) -> *const std::ffi::c_char;

    struct Lib {
        _lib: libloading::Library,
        ctx_new: NftCtxNew,
        ctx_free: NftCtxFree,
        run_cmd: NftRunCmdFromBuffer,
        buffer_output: NftCtxBufferOutput,
        _get_output: NftCtxGetOutputBuffer,
        buffer_error: NftCtxBufferError,
        get_error: NftCtxGetErrorBuffer,
    }

    // SAFETY: the function pointers are from a shared library that is
    // loaded once and stays loaded. The functions themselves are thread-safe
    // when called with distinct nft_ctx pointers.
    unsafe impl Send for Lib {}
    unsafe impl Sync for Lib {}

    static LIB: OnceLock<Option<Lib>> = OnceLock::new();

    fn load() -> Option<&'static Lib> {
        LIB.get_or_init(|| {
            let lib = match unsafe { libloading::Library::new("libnftables.so.1") } {
                Ok(lib) => {
                    tracing::debug!("libnftables.so.1 loaded, using in-process nft");
                    lib
                }
                Err(e) => {
                    tracing::debug!(error = %e, "libnftables.so.1 not available, falling back to nft binary");
                    return None;
                }
            };
            unsafe {
                let ctx_new = *lib.get::<NftCtxNew>(b"nft_ctx_new\0").ok()?;
                let ctx_free = *lib.get::<NftCtxFree>(b"nft_ctx_free\0").ok()?;
                let run_cmd = *lib
                    .get::<NftRunCmdFromBuffer>(b"nft_run_cmd_from_buffer\0")
                    .ok()?;
                let buffer_output = *lib
                    .get::<NftCtxBufferOutput>(b"nft_ctx_buffer_output\0")
                    .ok()?;
                let _get_output = *lib
                    .get::<NftCtxGetOutputBuffer>(b"nft_ctx_get_output_buffer\0")
                    .ok()?;
                let buffer_error = *lib
                    .get::<NftCtxBufferError>(b"nft_ctx_buffer_error\0")
                    .ok()?;
                let get_error = *lib
                    .get::<NftCtxGetErrorBuffer>(b"nft_ctx_get_error_buffer\0")
                    .ok()?;
                Some(Lib {
                    _lib: lib,
                    ctx_new,
                    ctx_free,
                    run_cmd,
                    buffer_output,
                    _get_output,
                    buffer_error,
                    get_error,
                })
            }
        })
        .as_ref()
    }

    /// Applies nftables rules via libnftables in-process. Returns None if
    /// the library is not available.
    pub(super) fn try_apply(rules: &str) -> Option<Result<()>> {
        let lib = load()?;
        let ctx = unsafe { (lib.ctx_new)(0) };
        if ctx.is_null() {
            return Some(Err(anyhow!("nft_ctx_new returned null")));
        }
        // Buffer output and errors so they don't go to stdout/stderr.
        unsafe {
            (lib.buffer_output)(ctx);
            (lib.buffer_error)(ctx);
        }
        let c_rules = match CString::new(rules) {
            Ok(c) => c,
            Err(e) => {
                unsafe { (lib.ctx_free)(ctx) };
                return Some(Err(anyhow!("nft rules contain null byte: {e}")));
            }
        };
        let ret = unsafe { (lib.run_cmd)(ctx, c_rules.as_ptr()) };
        let result = if ret == 0 {
            Ok(())
        } else {
            let err = unsafe {
                let ptr = (lib.get_error)(ctx);
                if ptr.is_null() {
                    String::from("(no error message)")
                } else {
                    CStr::from_ptr(ptr).to_string_lossy().into_owned()
                }
            };
            Err(anyhow!("nft apply failed: {}", err.trim()))
        };
        unsafe { (lib.ctx_free)(ctx) };
        Some(result)
    }
}

/// Applies nftables rules (assumes caller is already in the target namespace).
/// Tries libnftables in-process first (~50us), falls back to spawning
/// the nft binary (~5ms) if the library is not available.
fn run_nft_sync(rules: &str) -> Result<()> {
    if let Some(result) = libnft::try_apply(rules) {
        tracing::trace!("nft: applied via libnftables (in-process)");
        return result;
    }
    tracing::trace!("nft: applying via nft binary (fallback)");
    run_nft_cmd(rules)
}

/// Fallback: apply rules by spawning the nft binary.
fn run_nft_cmd(rules: &str) -> Result<()> {
    use std::io::Write;
    let nft = if std::path::Path::new("/usr/sbin/nft").exists() {
        "/usr/sbin/nft"
    } else {
        "nft"
    };
    let mut child = std::process::Command::new(nft)
        .args(["-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawn nft")?;
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(rules.as_bytes())
        .context("write nft stdin")?;
    let output = child.wait_with_output().context("wait nft")?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(anyhow!("nft apply failed: {}", stderr.trim()))
    }
}

/// Applies nftables rules inside `ns` on the namespace's sync worker.
pub(crate) async fn run_nft_in(netns: &netns::NetnsManager, ns: &str, rules: &str) -> Result<()> {
    trace!(ns = %ns, rules = %rules, "nft: apply rules");
    let rules = rules.to_string();
    netns.run_closure_in(ns, move || run_nft_sync(&rules))
}

/// Generates nftables rules for a [`NatConfig`].
///
/// EIM uses a dynamic fullcone map to preserve source ports across destinations.
/// EDM always uses `masquerade random`, allocating a fresh random external port
/// per destination 4-tuple (true symmetric NAT).
/// EIF produces an unconditional fullcone DNAT in prerouting.
/// ADF and APDF add a forward filter that only allows established/related
/// flows. ADF additionally permits packets from any port on a
/// previously-contacted external address.
fn generate_nat_rules(cfg: &NatConfig, wan_if: &str, wan_ip: Ipv4Addr) -> String {
    let use_fullcone_map = uses_fullcone_map(cfg);
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
    // the correct internal host. For EDM, an empty prerouting chain is
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
        unreachable!(
            "EndpointDependent + hairpin is rejected by NatConfigBuilder::build \
             and re-checked by NatConfig::validate in apply_nat_config \
             (NatConfigError::HairpinRequiresEim), so this branch is unreachable"
        )
    } else {
        String::new()
    };

    // Postrouting: EIM uses snat + fullcone map update. EDM uses masquerade
    // with or without the `random` flag based on port preservation. With
    // hairpin (EIM only): masquerade DNAT'd packets so the return path goes
    // through the router; otherwise the LAN peer replies directly and
    // confuses conntrack.
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
        // EDM: always `masquerade random` to allocate a fresh, random
        // external port per destination 4-tuple. Plain `masquerade` (no
        // flag) would try to preserve the source port, which conntrack
        // grants whenever the (ext_ip, ext_port, dst_ip, dst_port) tuple
        // is free. For a single internal source sending to multiple
        // destinations that converges on EIM-looking behavior, so it is
        // not a faithful symmetric NAT. See docs/reference/nat-limitations.md.
        format!(
            r#"{hairpin}        oif "{wan}" masquerade random"#,
            hairpin = hairpin_masq,
            wan = wan_if,
        )
    };

    let postrouting_priority = if use_fullcone_map { "srcnat" } else { "100" };

    // Filter table implements the filtering behavior on the WAN ingress path.
    //
    // APDF: `ct state established,related accept` is enough because conntrack
    // only matches when source IP and source port exactly match the outbound
    // flow.
    //
    // ADF: same conntrack accept plus a dynamic set of external IPs the
    // internal side has contacted. Packets from any port on those IPs are
    // allowed. The set is populated on the postrouting path; the forward
    // chain consults it on inbound.
    //
    // EIF: no filter table; the prerouting fullcone DNAT delivers packets
    // regardless of source.
    let filter_table = match cfg.filtering {
        NatFiltering::EndpointIndependent => String::new(),
        NatFiltering::AddressAndPortDependent => format!(
            r#"
table ip filter {{
    chain forward {{
        type filter hook forward priority 0; policy accept;
        iif "{wan}" ct state established,related accept
        iif "{wan}" drop
    }}
}}"#,
            wan = wan_if
        ),
        NatFiltering::AddressDependent => format!(
            // The contacted-address entry lives as long as the flow that
            // created it, so track the UDP stream timeout rather than a fixed
            // 300s (which happens to be the default). A shorter per-config
            // timeout then closes the address filter in step with conntrack.
            r#"
table ip filter {{
    set contacted {{
        type ipv4_addr
        flags dynamic,timeout
        timeout {stream}s
        size 8192
    }}
    chain forward {{
        type filter hook forward priority 0; policy accept;
        iif "{wan}" ct state established,related accept
        iif "{wan}" ip saddr @contacted accept
        iif "{wan}" drop
    }}
    chain postrouting {{
        type filter hook postrouting priority 0; policy accept;
        oif "{wan}" update @contacted {{ ip daddr }}
    }}
}}"#,
            wan = wan_if,
            stream = cfg.timeouts.udp_stream,
        ),
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
    // Re-validate cross-field invariants before generating rules. `NatConfig`
    // has public fields, so a caller can mutate a built config into a
    // combination the backend cannot express (EDM + hairpin, EDM + ADF).
    // Surface that as an error here rather than as a panic in
    // `generate_nat_rules`.
    cfg.validate().map_err(anyhow::Error::new)?;
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
/// Uses the stored `Option<NatConfig>` directly; `None` disables NAT.
pub(crate) async fn apply_nat_for_router(
    netns: &netns::NetnsManager,
    ns: &str,
    router_cfg: &RouterConfig,
    wan_if: &str,
    wan_ip: Ipv4Addr,
) -> Result<()> {
    match router_cfg.nat {
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
    let limits = impair;
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
