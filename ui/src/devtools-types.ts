// ── NAT types ──

/** NAT behavior preset (kebab-case from Rust `Nat` enum). */
export type NatPreset = 'none' | 'home' | 'corporate' | 'cgnat' | 'cloud-nat' | 'full-cone'

/** Custom NAT configuration (Rust `Nat::Custom(NatConfig)`). */
export interface NatConfig {
  mapping: 'endpoint_independent' | 'endpoint_dependent'
  filtering: 'endpoint_independent' | 'address_and_port_dependent'
  timeouts: { udp: number; udp_stream: number; tcp_established: number }
  hairpin: boolean
}

/** Matches Rust `Nat` — either a preset string or `{ custom: NatConfig }`. */
export type Nat = NatPreset | { custom: NatConfig }

/** Matches Rust `NatV6Mode`. */
export type NatV6Mode = 'none' | 'nptv6' | 'masquerade' | 'nat64'

/** Matches Rust `IpSupport`. */
export type IpSupport = 'v4-only' | 'v6-only' | 'dual-stack'

// ── Firewall types ──

export type FirewallPreset = 'none' | 'block_inbound' | 'corporate' | 'captive_portal'

export type PortPolicy = 'allow_all' | 'block_all' | { allow: number[] }

export interface FirewallConfig {
  block_inbound: boolean
  outbound_tcp: PortPolicy
  outbound_udp: PortPolicy
}

/** Matches Rust `Firewall`. */
export type Firewall = FirewallPreset | { custom: FirewallConfig }

// ── Link condition types ──

export interface LinkLimits {
  latency_ms: number
  jitter_ms: number
  loss_pct: number
  rate_kbit: number | null
}

/** Matches Rust `LinkCondition` — preset string or manual limits object. */
export type LinkCondition =
  | 'lan'
  | 'wifi'
  | 'wifi_bad'
  | 'mobile_4g'
  | 'mobile_3g'
  | 'satellite'
  | 'satellite_geo'
  | LinkLimits

// ── State types ──

export interface LabState {
  opid: number
  lab_prefix: string
  label: string | null
  status: string
  created_at: string | null
  ix: IxState | null
  routers: Record<string, RouterState>
  devices: Record<string, DeviceState>
  regions: Record<string, RegionState>
  region_links: RegionLinkState[]
}

export interface IxState {
  bridge: string
  cidr: string
  gw: string
  cidr_v6: string
  gw_v6: string
}

export interface RouterState {
  ns: string
  region: string | null
  nat: Nat
  nat_v6: NatV6Mode
  firewall: Firewall
  ip_support: IpSupport
  mtu: number | null
  upstream: string | null
  uplink_ip: string | null
  uplink_ip_v6: string | null
  downstream_cidr: string | null
  downstream_gw: string | null
  downstream_cidr_v6: string | null
  downstream_gw_v6: string | null
  downstream_bridge: string
  downlink_condition: LinkCondition | null
  devices: string[]
  counters: Record<string, IfaceCounters>
}

export interface DeviceState {
  ns: string
  default_via: string
  mtu: number | null
  interfaces: IfaceState[]
  counters: Record<string, IfaceCounters>
}

export interface IfaceState {
  name: string
  router: string
  ip: string | null
  ip_v6: string | null
  link_condition: LinkCondition | null
}

export interface IfaceCounters {
  iface: string
  rx_bytes: number
  tx_bytes: number
  rx_packets: number
  tx_packets: number
}

export interface RegionState {
  router: string
}

export interface RegionLinkState {
  a: string
  b: string
  condition: LinkCondition | null
  broken: boolean
}

export interface LabEvent {
  opid: number
  timestamp: string
  kind: string
  [key: string]: unknown
}
