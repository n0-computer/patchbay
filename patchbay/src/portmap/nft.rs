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
/// Runs at nft priority `-110`, which sits 10 below the `dstnat` priority
/// (`-100`) used by [`crate::nft::apply_nat_for_router`]. That placement
/// guarantees a static port forward DNAT applies before the fullcone map
/// rule, and that the packet's changed destination no longer matches the
/// fullcone criteria downstream.
///
/// The rule form is `ip daddr <wan_ip> meta l4proto <p> dport <ext> dnat
/// to <internal_ip>:<internal_port>`. Matching on `daddr` (rather than
/// `iif`) lets a LAN host hitting the router's WAN IP through hairpin
/// follow the same DNAT path as traffic arriving from the uplink.
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
    rules.push_str("}\n");
    rules
}

/// Flushes and repopulates the `ip portmap` table in `ns`.
///
/// The table is deleted if present, then re-declared with the new rule
/// set. `delete table` fails if the table does not exist; nft treats that
/// as an error and aborts the containing script, so the delete runs
/// separately and its error is swallowed before the fresh ruleset is
/// installed. Safe to call with an empty `mappings` slice, which leaves
/// the table declared but empty.
pub(crate) async fn apply_portmap_rules(
    netns: &netns::NetnsManager,
    ns: &str,
    wan_ip: Ipv4Addr,
    mappings: &[Mapping],
) -> Result<()> {
    run_nft_in(netns, ns, "delete table ip portmap\n")
        .await
        .ok();
    run_nft_in(netns, ns, &generate_portmap_rules(wan_ip, mappings)).await
}

/// Removes the `ip portmap` table entirely. Idempotent.
pub(crate) async fn clear_portmap_rules(netns: &netns::NetnsManager, ns: &str) -> Result<()> {
    run_nft_in(netns, ns, "delete table ip portmap\n")
        .await
        .ok();
    Ok(())
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
            "two mappings produce two rules",
        );
    }
}
