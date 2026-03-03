//! Lab event system: typed events, state reducer, counter collection.

use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, Ipv6Addr},
};

#[cfg(target_os = "linux")]
use std::sync::atomic::Ordering;

use chrono::{DateTime, Utc};
use ipnet::{Ipv4Net, Ipv6Net};
use serde::{Deserialize, Serialize};

// Import types from the appropriate module based on platform
#[cfg(target_os = "linux")]
use crate::{
    firewall::Firewall,
    lab::LinkCondition,
    nat::{IpSupport, Nat, NatV6Mode},
};

#[cfg(not(target_os = "linux"))]
use crate::types_portable::{Firewall, IpSupport, LinkCondition, Nat, NatV6Mode};

// ─────────────────────────────────────────────
// Event types
// ─────────────────────────────────────────────

/// A single lab event with global ordering.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LabEvent {
    /// Monotonically increasing operation id.
    pub opid: u64,
    /// Wall-clock timestamp.
    pub timestamp: DateTime<Utc>,
    /// The event payload.
    #[serde(flatten)]
    pub kind: LabEventKind,
}

/// All event variants. Internally tagged with a `"kind"` field.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LabEventKind {
    // ── Lifecycle ──
    /// Emitted once during `Lab::new()`.
    LabCreated {
        /// The lab's process-unique prefix (e.g. `"lab-p12340"`).
        lab_prefix: String,
        /// Optional human-readable label.
        label: Option<String>,
    },
    /// Full state snapshot, emitted by `Lab::from_config()`.
    InitialState {
        /// Serialized [`LabState`].
        state: serde_json::Value,
    },
    /// Emitted when the lab is shutting down.
    LabStopping,

    // ── IX ──
    /// Emitted once during `Lab::new()` after IX bridge creation.
    IxCreated {
        /// IX bridge name (e.g. `"br-p1230-1"`).
        bridge: String,
        /// IPv4 CIDR (e.g. `"198.18.0.0/24"`).
        cidr: Ipv4Net,
        /// IPv4 gateway (e.g. `"198.18.0.1"`).
        gw: Ipv4Addr,
        /// IPv6 CIDR.
        cidr_v6: Ipv6Net,
        /// IPv6 gateway.
        gw_v6: Ipv6Addr,
    },

    // ── Topology ──
    /// A router was added to the lab.
    RouterAdded {
        /// Router name.
        name: String,
        /// Full router state.
        #[serde(flatten)]
        state: Box<RouterState>,
    },
    /// A router was removed from the lab.
    RouterRemoved {
        /// Router name.
        name: String,
    },
    /// A device was added to the lab.
    DeviceAdded {
        /// Device name.
        name: String,
        /// Full device state.
        #[serde(flatten)]
        state: DeviceState,
    },
    /// A device was removed from the lab.
    DeviceRemoved {
        /// Device name.
        name: String,
    },

    // ── Regions ──
    /// A region was added.
    RegionAdded {
        /// Region name.
        name: String,
        /// Router assigned to this region.
        router: String,
    },
    /// An inter-region link was created.
    RegionLinkAdded {
        /// Router A name.
        router_a: String,
        /// Router B name.
        router_b: String,
    },
    /// An inter-region link was broken/impaired.
    RegionLinkBroken {
        /// Router A name.
        router_a: String,
        /// Router B name.
        router_b: String,
        /// Link condition applied (if any, vs total severing).
        condition: Option<LinkCondition>,
    },
    /// An inter-region link was restored.
    RegionLinkRestored {
        /// Router A name.
        router_a: String,
        /// Router B name.
        router_b: String,
    },

    // ── Mutations ──
    /// Router NAT mode changed.
    NatChanged {
        /// Router name.
        router: String,
        /// New NAT configuration.
        nat: Nat,
    },
    /// Router IPv6 NAT mode changed.
    NatV6Changed {
        /// Router name.
        router: String,
        /// New IPv6 NAT mode.
        nat_v6: NatV6Mode,
    },
    /// Router NAT conntrack state flushed.
    NatStateFlushed {
        /// Router name.
        router: String,
    },
    /// Router firewall changed.
    FirewallChanged {
        /// Router name.
        router: String,
        /// New firewall configuration.
        firewall: Firewall,
    },
    /// Device interface link condition changed.
    LinkConditionChanged {
        /// Device name.
        device: String,
        /// Interface name.
        iface: String,
        /// New link condition (`None` = removed).
        condition: Option<LinkCondition>,
    },
    /// Router downlink condition changed (affects all downstream traffic).
    DownlinkConditionChanged {
        /// Router name.
        router: String,
        /// New condition (`None` = removed).
        condition: Option<LinkCondition>,
    },
    /// Device interface brought up.
    LinkUp {
        /// Device name.
        device: String,
        /// Interface name.
        iface: String,
    },
    /// Device interface brought down.
    LinkDown {
        /// Device name.
        device: String,
        /// Interface name.
        iface: String,
    },
    /// A new interface was added to a device.
    InterfaceAdded {
        /// Device name.
        device: String,
        /// Interface snapshot.
        iface: IfaceSnapshot,
    },
    /// An interface was removed from a device.
    InterfaceRemoved {
        /// Device name.
        device: String,
        /// Interface name.
        iface_name: String,
    },
    /// A device interface was replugged to a different router.
    InterfaceReplugged {
        /// Device name.
        device: String,
        /// Interface name.
        iface_name: String,
        /// Previous router name.
        from_router: String,
        /// New router name.
        to_router: String,
        /// New IPv4 address.
        new_ip: Option<Ipv4Addr>,
        /// New IPv6 address.
        new_ip_v6: Option<Ipv6Addr>,
    },
    /// Device IP address changed (e.g. DHCP renewal).
    DeviceIpChanged {
        /// Device name.
        device: String,
        /// Interface name.
        iface_name: String,
        /// New IPv4 address.
        new_ip: Option<Ipv4Addr>,
        /// New IPv6 address.
        new_ip_v6: Option<Ipv6Addr>,
    },

    // ── Processes ──
    /// A command was spawned in a node namespace.
    CommandSpawned {
        /// Node name (device or router).
        node: String,
        /// OS process ID.
        pid: u32,
        /// Command string.
        cmd: String,
    },
    /// A spawned command exited.
    CommandExited {
        /// Node name.
        node: String,
        /// OS process ID.
        pid: u32,
        /// Exit code (`None` if killed by signal).
        exit_code: Option<i32>,
    },

    // ── Counters ──
    /// Packet counters snapshot for a node.
    PacketCounters {
        /// Node name.
        node: String,
        /// Per-interface counters.
        counters: Vec<IfaceCounters>,
    },
}

/// Snapshot of a device interface at the time of an event.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct IfaceSnapshot {
    /// Interface name.
    pub name: String,
    /// Owning router name.
    pub router: String,
    /// IPv4 address.
    pub ip: Option<Ipv4Addr>,
    /// IPv6 address.
    pub ip_v6: Option<Ipv6Addr>,
    /// Link condition.
    pub link_condition: Option<LinkCondition>,
}

/// Traffic counters for one interface (from `/proc/net/dev`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct IfaceCounters {
    /// Interface name.
    pub iface: String,
    /// Received bytes.
    pub rx_bytes: u64,
    /// Transmitted bytes.
    pub tx_bytes: u64,
    /// Received packets.
    pub rx_packets: u64,
    /// Transmitted packets.
    pub tx_packets: u64,
}

// ─────────────────────────────────────────────
// LabState — state.json reducer
// ─────────────────────────────────────────────

/// Lab state derived from the event stream. Mirrors the `state.json` schema.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LabState {
    /// Latest applied opid.
    pub opid: u64,
    /// Lab prefix (e.g. `"lab-p12340"`).
    pub lab_prefix: String,
    /// Human-readable label.
    pub label: Option<String>,
    /// Lab status (`"running"`, `"stopping"`).
    pub status: String,
    /// Lab creation timestamp.
    pub created_at: Option<DateTime<Utc>>,
    /// IX state.
    pub ix: Option<IxState>,
    /// Router states keyed by name.
    pub routers: BTreeMap<String, RouterState>,
    /// Device states keyed by name.
    pub devices: BTreeMap<String, DeviceState>,
    /// Region states keyed by name.
    pub regions: BTreeMap<String, RegionState>,
    /// Inter-region links.
    pub region_links: Vec<RegionLinkState>,
}

/// IX state in state.json.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IxState {
    /// Bridge name.
    pub bridge: String,
    /// IPv4 CIDR.
    pub cidr: Ipv4Net,
    /// IPv4 gateway.
    pub gw: Ipv4Addr,
    /// IPv6 CIDR.
    pub cidr_v6: Ipv6Net,
    /// IPv6 gateway.
    pub gw_v6: Ipv6Addr,
}

/// Router state in state.json.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RouterState {
    /// Namespace name.
    pub ns: String,
    /// Region tag.
    pub region: Option<String>,
    /// NAT configuration.
    pub nat: Nat,
    /// IPv6 NAT mode.
    pub nat_v6: NatV6Mode,
    /// Firewall configuration.
    pub firewall: Firewall,
    /// IP family support.
    pub ip_support: IpSupport,
    /// MTU override.
    pub mtu: Option<u32>,
    /// Parent router name (`None` = IX-connected).
    pub upstream: Option<String>,
    /// WAN IPv4 address.
    pub uplink_ip: Option<Ipv4Addr>,
    /// WAN IPv6 address.
    pub uplink_ip_v6: Option<Ipv6Addr>,
    /// LAN IPv4 CIDR.
    pub downstream_cidr: Option<Ipv4Net>,
    /// LAN IPv4 gateway.
    pub downstream_gw: Option<Ipv4Addr>,
    /// LAN IPv6 CIDR.
    pub downstream_cidr_v6: Option<Ipv6Net>,
    /// LAN IPv6 gateway.
    pub downstream_gw_v6: Option<Ipv6Addr>,
    /// Downstream bridge name.
    pub downstream_bridge: String,
    /// Downlink condition (applies to all downstream traffic).
    pub downlink_condition: Option<LinkCondition>,
    /// Names of downstream devices.
    pub devices: Vec<String>,
    /// Per-interface counters.
    pub counters: BTreeMap<String, IfaceCounters>,
}

/// Device state in state.json.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceState {
    /// Namespace name.
    pub ns: String,
    /// Default route interface.
    pub default_via: String,
    /// MTU override.
    pub mtu: Option<u32>,
    /// Interface states.
    pub interfaces: Vec<IfaceSnapshot>,
    /// Per-interface counters.
    pub counters: BTreeMap<String, IfaceCounters>,
}

/// Region state in state.json.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegionState {
    /// Router assigned to this region.
    pub router: String,
}

/// Inter-region link state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegionLinkState {
    /// Router A name.
    pub a: String,
    /// Router B name.
    pub b: String,
    /// Link condition (if impaired).
    pub condition: Option<LinkCondition>,
    /// Whether the link is broken.
    pub broken: bool,
}

#[cfg(target_os = "linux")]
impl RouterState {
    /// Construct from core `RouterData`, resolving upstream name and downstream bridge.
    pub(crate) fn from_router_data(
        r: &crate::core::RouterData,
        upstream_name: Option<String>,
        downstream_bridge: String,
    ) -> Self {
        Self {
            ns: r.ns.to_string(),
            region: r.region.as_ref().map(|s| s.to_string()),
            nat: r.cfg.nat,
            nat_v6: r.cfg.nat_v6,
            firewall: r.cfg.firewall.clone(),
            ip_support: r.cfg.ip_support,
            mtu: r.cfg.mtu,
            upstream: upstream_name,
            uplink_ip: r.upstream_ip,
            uplink_ip_v6: r.upstream_ip_v6,
            downstream_cidr: r.downstream_cidr,
            downstream_gw: r.downstream_gw,
            downstream_cidr_v6: r.downstream_cidr_v6,
            downstream_gw_v6: r.downstream_gw_v6,
            downstream_bridge,
            downlink_condition: None,
            devices: Vec::new(),
            counters: BTreeMap::new(),
        }
    }
}

#[cfg(target_os = "linux")]
impl DeviceState {
    /// Construct from core `DeviceData`, resolving router names via the core.
    pub(crate) fn from_device_data(
        d: &crate::core::DeviceData,
        core: &crate::core::NetworkCore,
    ) -> Self {
        let iface_snapshots = d
            .interfaces
            .iter()
            .map(|iface| {
                let router_name = core
                    .switch(iface.uplink)
                    .and_then(|sw| sw.owner_router)
                    .and_then(|rid| core.router(rid))
                    .map(|r| r.name.to_string())
                    .unwrap_or_default();
                IfaceSnapshot {
                    name: iface.ifname.to_string(),
                    router: router_name,
                    ip: iface.ip,
                    ip_v6: iface.ip_v6,
                    link_condition: iface.impair,
                }
            })
            .collect();
        Self {
            ns: d.ns.to_string(),
            default_via: d.default_via.to_string(),
            mtu: d.mtu,
            interfaces: iface_snapshots,
            counters: BTreeMap::new(),
        }
    }
}

impl LabState {
    /// Apply an event to update the state.
    pub fn apply(&mut self, event: &LabEvent) {
        self.opid = event.opid;
        match &event.kind {
            LabEventKind::LabCreated { lab_prefix, label } => {
                self.lab_prefix = lab_prefix.clone();
                self.label = label.clone();
                self.status = "running".into();
                self.created_at = Some(event.timestamp);
            }
            LabEventKind::InitialState { state } => {
                if let Ok(s) = serde_json::from_value(state.clone()) {
                    *self = s;
                    self.opid = event.opid;
                }
            }
            LabEventKind::LabStopping => {
                self.status = "stopping".into();
            }
            LabEventKind::IxCreated {
                bridge,
                cidr,
                gw,
                cidr_v6,
                gw_v6,
            } => {
                self.ix = Some(IxState {
                    bridge: bridge.clone(),
                    cidr: *cidr,
                    gw: *gw,
                    cidr_v6: *cidr_v6,
                    gw_v6: *gw_v6,
                });
            }
            LabEventKind::RouterAdded { name, state } => {
                self.routers.insert(name.clone(), *state.clone());
            }
            LabEventKind::RouterRemoved { name } => {
                self.routers.remove(name);
            }
            LabEventKind::DeviceAdded { name, state } => {
                // Update parent routers' device lists.
                for iface in &state.interfaces {
                    if let Some(r) = self.routers.get_mut(&iface.router) {
                        if !r.devices.contains(name) {
                            r.devices.push(name.clone());
                        }
                    }
                }
                self.devices.insert(name.clone(), state.clone());
            }
            LabEventKind::DeviceRemoved { name } => {
                // Remove from parent routers' device lists.
                if let Some(dev) = self.devices.get(name) {
                    for iface in &dev.interfaces {
                        if let Some(r) = self.routers.get_mut(&iface.router) {
                            r.devices.retain(|d| d != name);
                        }
                    }
                }
                self.devices.remove(name);
            }
            LabEventKind::RegionAdded { name, router } => {
                self.regions.insert(
                    name.clone(),
                    RegionState {
                        router: router.clone(),
                    },
                );
            }
            LabEventKind::RegionLinkAdded { router_a, router_b } => {
                self.region_links.push(RegionLinkState {
                    a: router_a.clone(),
                    b: router_b.clone(),
                    condition: None,
                    broken: false,
                });
            }
            LabEventKind::RegionLinkBroken {
                router_a,
                router_b,
                condition,
            } => {
                for link in &mut self.region_links {
                    if (&link.a == router_a && &link.b == router_b)
                        || (&link.a == router_b && &link.b == router_a)
                    {
                        link.broken = true;
                        link.condition = *condition;
                    }
                }
            }
            LabEventKind::RegionLinkRestored { router_a, router_b } => {
                for link in &mut self.region_links {
                    if (&link.a == router_a && &link.b == router_b)
                        || (&link.a == router_b && &link.b == router_a)
                    {
                        link.broken = false;
                        link.condition = None;
                    }
                }
            }
            LabEventKind::NatChanged { router, nat } => {
                if let Some(r) = self.routers.get_mut(router) {
                    r.nat = *nat;
                }
            }
            LabEventKind::NatV6Changed { router, nat_v6 } => {
                if let Some(r) = self.routers.get_mut(router) {
                    r.nat_v6 = *nat_v6;
                }
            }
            LabEventKind::NatStateFlushed { .. } => {}
            LabEventKind::FirewallChanged { router, firewall } => {
                if let Some(r) = self.routers.get_mut(router) {
                    r.firewall = firewall.clone();
                }
            }
            LabEventKind::LinkConditionChanged {
                device,
                iface,
                condition,
            } => {
                if let Some(d) = self.devices.get_mut(device) {
                    for i in &mut d.interfaces {
                        if i.name == *iface {
                            i.link_condition = *condition;
                        }
                    }
                }
            }
            LabEventKind::DownlinkConditionChanged { router, condition } => {
                if let Some(r) = self.routers.get_mut(router) {
                    r.downlink_condition = *condition;
                }
            }
            LabEventKind::LinkUp { .. } | LabEventKind::LinkDown { .. } => {
                // State doesn't track link up/down currently.
            }
            LabEventKind::InterfaceAdded { device, iface } => {
                if let Some(d) = self.devices.get_mut(device) {
                    d.interfaces.push(iface.clone());
                }
                if let Some(r) = self.routers.get_mut(&iface.router) {
                    if !r.devices.contains(device) {
                        r.devices.push(device.clone());
                    }
                }
            }
            LabEventKind::InterfaceRemoved { device, iface_name } => {
                // Find which router this interface was on before removing it.
                let router_name = self
                    .devices
                    .get(device)
                    .and_then(|d| d.interfaces.iter().find(|i| i.name == *iface_name))
                    .map(|i| i.router.clone());
                if let Some(d) = self.devices.get_mut(device) {
                    d.interfaces.retain(|i| i.name != *iface_name);
                }
                // Remove device from router's device list if it has no remaining
                // interfaces on that router.
                if let Some(rn) = router_name {
                    let still_connected = self
                        .devices
                        .get(device)
                        .map(|d| d.interfaces.iter().any(|i| i.router == rn))
                        .unwrap_or(false);
                    if !still_connected {
                        if let Some(r) = self.routers.get_mut(&rn) {
                            r.devices.retain(|d| d != device);
                        }
                    }
                }
            }
            LabEventKind::InterfaceReplugged {
                device,
                iface_name,
                from_router,
                to_router,
                new_ip,
                new_ip_v6,
            } => {
                if let Some(d) = self.devices.get_mut(device) {
                    for i in &mut d.interfaces {
                        if i.name == *iface_name {
                            i.router = to_router.clone();
                            i.ip = *new_ip;
                            i.ip_v6 = *new_ip_v6;
                        }
                    }
                }
                // Update router device lists.
                if let Some(r) = self.routers.get_mut(from_router) {
                    r.devices.retain(|d| d != device);
                }
                if let Some(r) = self.routers.get_mut(to_router) {
                    if !r.devices.contains(device) {
                        r.devices.push(device.clone());
                    }
                }
            }
            LabEventKind::DeviceIpChanged {
                device,
                iface_name,
                new_ip,
                new_ip_v6,
            } => {
                if let Some(d) = self.devices.get_mut(device) {
                    for i in &mut d.interfaces {
                        if i.name == *iface_name {
                            i.ip = *new_ip;
                            i.ip_v6 = *new_ip_v6;
                        }
                    }
                }
            }
            LabEventKind::CommandSpawned { .. } | LabEventKind::CommandExited { .. } => {
                // Process tracking deferred.
            }
            LabEventKind::PacketCounters { node, counters } => {
                let map = if let Some(r) = self.routers.get_mut(node) {
                    &mut r.counters
                } else if let Some(d) = self.devices.get_mut(node) {
                    &mut d.counters
                } else {
                    return;
                };
                for c in counters {
                    map.insert(c.iface.clone(), c.clone());
                }
            }
        }
    }
}

// ─────────────────────────────────────────────
// Counter collection
// ─────────────────────────────────────────────

/// Parse `/proc/net/dev` content into per-interface counters.
pub fn parse_proc_net_dev(content: &str) -> Vec<IfaceCounters> {
    let mut result = Vec::new();
    for line in content.lines().skip(2) {
        let line = line.trim();
        let Some((iface, rest)) = line.split_once(':') else {
            continue;
        };
        let iface = iface.trim();
        if iface == "lo" {
            continue;
        }
        let fields: Vec<u64> = rest
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        if fields.len() >= 10 {
            result.push(IfaceCounters {
                iface: iface.to_string(),
                rx_bytes: fields[0],
                rx_packets: fields[1],
                tx_bytes: fields[8],
                tx_packets: fields[9],
            });
        }
    }
    result
}

// ─────────────────────────────────────────────
// Emit helper on LabInner
// ─────────────────────────────────────────────

#[cfg(target_os = "linux")]
impl crate::core::LabInner {
    /// Emits an event on the broadcast channel. Returns the assigned opid.
    pub(crate) fn emit(&self, kind: LabEventKind) -> u64 {
        let opid = self.opid.fetch_add(1, Ordering::Relaxed);
        let event = LabEvent {
            opid,
            timestamp: Utc::now(),
            kind,
        };
        // Ignore error — no receivers is fine.
        let _ = self.events_tx.send(event);
        opid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serde_roundtrip() {
        let events = vec![
            LabEventKind::LabCreated {
                lab_prefix: "lab-p1".into(),
                label: Some("test".into()),
            },
            LabEventKind::RouterAdded {
                name: "r1".into(),
                state: Box::new(RouterState {
                    ns: "ns-r1".into(),
                    region: None,
                    nat: Nat::Home,
                    nat_v6: NatV6Mode::None,
                    firewall: Firewall::None,
                    ip_support: IpSupport::V4Only,
                    mtu: None,
                    upstream: None,
                    uplink_ip: Some(Ipv4Addr::new(198, 18, 0, 2)),
                    uplink_ip_v6: None,
                    downstream_cidr: Some("10.0.1.0/24".parse().unwrap()),
                    downstream_gw: Some(Ipv4Addr::new(10, 0, 1, 1)),
                    downstream_cidr_v6: None,
                    downstream_gw_v6: None,
                    downstream_bridge: "br-1".into(),
                    downlink_condition: None,
                    devices: Vec::new(),
                    counters: BTreeMap::new(),
                }),
            },
            LabEventKind::DeviceAdded {
                name: "d1".into(),
                state: DeviceState {
                    ns: "ns-d1".into(),
                    default_via: "eth0".into(),
                    mtu: None,
                    interfaces: vec![IfaceSnapshot {
                        name: "eth0".into(),
                        router: "r1".into(),
                        ip: Some(Ipv4Addr::new(10, 0, 1, 2)),
                        ip_v6: None,
                        link_condition: None,
                    }],
                    counters: BTreeMap::new(),
                },
            },
            LabEventKind::NatChanged {
                router: "r1".into(),
                nat: Nat::Corporate,
            },
            LabEventKind::DeviceRemoved { name: "d1".into() },
            LabEventKind::RouterRemoved { name: "r1".into() },
        ];

        for (i, kind) in events.into_iter().enumerate() {
            let event = LabEvent {
                opid: i as u64,
                timestamp: Utc::now(),
                kind,
            };
            let json = serde_json::to_string(&event).expect("serialize");
            let back: LabEvent = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(event.opid, back.opid);
        }
    }

    #[test]
    fn state_reducer() {
        let mut state = LabState::default();

        let events = vec![
            LabEvent {
                opid: 0,
                timestamp: Utc::now(),
                kind: LabEventKind::LabCreated {
                    lab_prefix: "lab-test".into(),
                    label: None,
                },
            },
            LabEvent {
                opid: 1,
                timestamp: Utc::now(),
                kind: LabEventKind::IxCreated {
                    bridge: "br-1".into(),
                    cidr: "198.18.0.0/24".parse().unwrap(),
                    gw: Ipv4Addr::new(198, 18, 0, 1),
                    cidr_v6: "2001:db8::/64".parse().unwrap(),
                    gw_v6: "2001:db8::1".parse().unwrap(),
                },
            },
            LabEvent {
                opid: 2,
                timestamp: Utc::now(),
                kind: LabEventKind::RouterAdded {
                    name: "r1".into(),
                    state: Box::new(RouterState {
                        ns: "ns-r1".into(),
                        region: None,
                        nat: Nat::Home,
                        nat_v6: NatV6Mode::None,
                        firewall: Firewall::None,
                        ip_support: IpSupport::V4Only,
                        mtu: None,
                        upstream: None,
                        uplink_ip: Some(Ipv4Addr::new(198, 18, 0, 2)),
                        uplink_ip_v6: None,
                        downstream_cidr: Some("10.0.1.0/24".parse().unwrap()),
                        downstream_gw: Some(Ipv4Addr::new(10, 0, 1, 1)),
                        downstream_cidr_v6: None,
                        downstream_gw_v6: None,
                        downstream_bridge: "br-2".into(),
                        downlink_condition: None,
                        devices: Vec::new(),
                        counters: BTreeMap::new(),
                    }),
                },
            },
            LabEvent {
                opid: 3,
                timestamp: Utc::now(),
                kind: LabEventKind::DeviceAdded {
                    name: "d1".into(),
                    state: DeviceState {
                        ns: "ns-d1".into(),
                        default_via: "eth0".into(),
                        mtu: None,
                        interfaces: vec![IfaceSnapshot {
                            name: "eth0".into(),
                            router: "r1".into(),
                            ip: Some(Ipv4Addr::new(10, 0, 1, 2)),
                            ip_v6: None,
                            link_condition: None,
                        }],
                        counters: BTreeMap::new(),
                    },
                },
            },
        ];

        for event in &events {
            state.apply(event);
        }

        assert_eq!(state.opid, 3);
        assert_eq!(state.lab_prefix, "lab-test");
        assert!(state.ix.is_some());
        assert_eq!(state.routers.len(), 1);
        assert_eq!(state.devices.len(), 1);
        assert_eq!(state.routers["r1"].devices, vec!["d1"]);
    }

    #[test]
    fn parse_proc_net_dev_works() {
        let content = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:    1000      10    0    0    0     0          0         0     1000      10    0    0    0     0       0          0
  eth0:   50000     100    0    0    0     0          0         0    25000      80    0    0    0     0       0          0
";
        let counters = parse_proc_net_dev(content);
        assert_eq!(counters.len(), 1);
        assert_eq!(counters[0].iface, "eth0");
        assert_eq!(counters[0].rx_bytes, 50000);
        assert_eq!(counters[0].tx_bytes, 25000);
        assert_eq!(counters[0].rx_packets, 100);
        assert_eq!(counters[0].tx_packets, 80);
    }
}
