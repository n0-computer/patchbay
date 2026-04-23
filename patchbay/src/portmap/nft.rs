//! nftables rules for the dedicated `ip portmap` table.
//!
//! Port mapping DNAT rules live in their own table, separate from the `ip
//! nat` table that [`crate::nft::apply_nat_for_router`] manages, because
//! [`crate::Router::set_nat_mode`] flushes the latter wholesale. Using a
//! separate table means runtime NAT reconfiguration never discards active
//! mappings.
//!
//! The table is re-rendered in full whenever the mapping set changes, so
//! the server keeps the authoritative mapping list in the registry and
//! lets nftables state be derived from it. This is simple, atomic when
//! applied through a single `nft -f` invocation, and avoids the complexity
//! of rule handles.

use std::net::Ipv4Addr;

use anyhow::Result;

use super::registry::{MapProto, Mapping};
use crate::{netns, nft::run_nft_in};

/// Generates the full `ip portmap` table body for the given mappings.
///
/// The table has two chains. The prerouting chain at priority `-110`
/// (10 below `dstnat` at `-100`, used by
/// [`crate::nft::apply_nat_for_router`]) rewrites the destination of
/// inbound packets matching `(ip daddr <wan_ip>, proto, dport <ext>)` to
/// `<internal_ip>:<internal_port>`. Matching on `daddr` rather than `iif`
/// lets a LAN host hitting the router's WAN IP through hairpin follow the
/// same DNAT path as traffic arriving from the uplink.
///
/// The forward chain at priority `-10` (below the APDF filter produced
/// by the NAT config at `0`) accepts any packet whose conntrack entry
/// has been DNAT'd and whose destination matches a current mapping. This
/// is necessary because the Home NAT filter drops `NEW` inbound flows on
/// the WAN interface; without this chain the DNAT succeeds but the
/// forwarded packet is dropped before it reaches the internal host.
pub(crate) fn generate_portmap_rules(wan_ip: Ipv4Addr, mappings: &[Mapping]) -> String {
    let mut rules = String::new();
    rules.push_str("table ip portmap {\n");
    rules.push_str("    chain prerouting {\n");
    rules.push_str("        type nat hook prerouting priority -110; policy accept;\n");
    for m in mappings {
        let proto = match m.proto {
            MapProto::Udp => "udp",
            MapProto::Tcp => "tcp",
        };
        rules.push_str(&format!(
            "        ip daddr {wan} {proto} dport {ext} dnat to {ip}:{port}\n",
            wan = wan_ip,
            proto = proto,
            ext = m.external_port.get(),
            ip = m.internal_ip,
            port = m.internal_port.get(),
        ));
    }
    rules.push_str("    }\n");
    rules.push_str("    chain forward {\n");
    rules.push_str("        type filter hook forward priority -10; policy accept;\n");
    for m in mappings {
        let proto = match m.proto {
            MapProto::Udp => "udp",
            MapProto::Tcp => "tcp",
        };
        rules.push_str(&format!(
            "        ip daddr {ip} {proto} dport {port} ct status dnat accept\n",
            proto = proto,
            ip = m.internal_ip,
            port = m.internal_port.get(),
        ));
    }
    rules.push_str("    }\n");
    rules.push_str("}\n");
    rules
}

/// Flushes and repopulates the `ip portmap` table in `ns`.
///
/// The script prepends `add table ip portmap` (idempotent: noop if the
/// table already exists) followed by `flush table ip portmap` so the
/// update is atomic from a single `nft -f` invocation. Safe to call with
/// an empty `mappings` slice, which leaves the table declared but empty.
pub(crate) async fn apply_portmap_rules(
    netns: &netns::NetnsManager,
    ns: &str,
    wan_ip: Ipv4Addr,
    mappings: &[Mapping],
) -> Result<()> {
    let mut script = String::from("add table ip portmap\nflush table ip portmap\n");
    script.push_str(&generate_portmap_rules(wan_ip, mappings));
    run_nft_in(netns, ns, &script).await
}

/// Removes the `ip portmap` table entirely. Idempotent: missing table is
/// swallowed as a success.
pub(crate) async fn clear_portmap_rules(netns: &netns::NetnsManager, ns: &str) -> Result<()> {
    // `add table` makes the delete idempotent in a single script.
    run_nft_in(netns, ns, "add table ip portmap\ndelete table ip portmap\n").await
}

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, num::NonZeroU16, time::Instant};

    use super::*;

    fn mapping(proto: MapProto, ext: u16, internal_ip: [u8; 4], internal_port: u16) -> Mapping {
        Mapping {
            proto,
            external_port: NonZeroU16::new(ext).unwrap(),
            internal_ip: Ipv4Addr::from(internal_ip),
            internal_port: NonZeroU16::new(internal_port).unwrap(),
            deadline: Instant::now() + std::time::Duration::from_secs(60),
            pcp_nonce: None,
        }
    }

    #[test]
    fn empty_rules_render_declares_table_and_chain() {
        let rendered = generate_portmap_rules(Ipv4Addr::new(198, 51, 100, 1), &[]);
        assert!(rendered.contains("table ip portmap"));
        assert!(rendered.contains("chain prerouting"));
        assert!(rendered.contains("priority -110"));
        assert!(rendered.contains("chain forward"));
        assert!(rendered.contains("priority -10"));
        // No DNAT rules when mappings is empty.
        assert!(!rendered.contains("dnat"));
    }

    #[test]
    fn udp_mapping_renders_as_udp_dnat() {
        let rendered = generate_portmap_rules(
            Ipv4Addr::new(198, 51, 100, 1),
            &[mapping(MapProto::Udp, 5000, [10, 0, 0, 5], 1234)],
        );
        assert!(rendered.contains("ip daddr 198.51.100.1 udp dport 5000 dnat to 10.0.0.5:1234",));
    }

    #[test]
    fn tcp_mapping_renders_as_tcp_dnat() {
        let rendered = generate_portmap_rules(
            Ipv4Addr::new(198, 51, 100, 1),
            &[mapping(MapProto::Tcp, 8080, [10, 0, 0, 5], 80)],
        );
        assert!(rendered.contains("tcp dport 8080 dnat to 10.0.0.5:80"));
    }

    #[test]
    fn multiple_mappings_render_independent_rules() {
        let rendered = generate_portmap_rules(
            Ipv4Addr::new(198, 51, 100, 1),
            &[
                mapping(MapProto::Udp, 5000, [10, 0, 0, 5], 1234),
                mapping(MapProto::Tcp, 8080, [10, 0, 0, 6], 80),
            ],
        );
        assert_eq!(
            rendered.matches("dnat to").count(),
            2,
            "two mappings produce two DNAT rules",
        );
        assert_eq!(
            rendered.matches("ct status dnat accept").count(),
            2,
            "two mappings produce two forward-accept rules",
        );
    }
}
