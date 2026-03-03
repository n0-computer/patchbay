# Devtools & UI: Observability, Event System, and Live Server

## Status: Complete

All 8 phases implemented. Additional work beyond the original plan:

- **Unified UI** — merged DevtoolsApp and runner App into single unified shell
  with run selector, topology/events/logs/timeline/perf tabs
- **Lazy file creation** — `LazyFile` defers `File::create` until first write,
  no empty files for silent namespaces
- **Removed `devtools` subcommand** — merged into `serve` (takes outdir positional arg)
- **Path traversal fix** in `run_events_sse` SSE handler
- **Writer subscription ordering** — subscribe before initial events to capture
  `LabCreated`/`IxCreated`
- **Per-namespace tracing guard** properly dropped on thread exit (no `mem::forget`)
- **LogsTab** — JSON tracing format parsing, level filter toggles, search with
  next/prev navigation
- **TimelineTab** — lab lifecycle events integrated into timeline grid
- **TopologyGraph** — node selection highlighting, differentiated shapes
  (circular IX, rounded routers, square devices)
- **E2E test** — playwright test verifying all views with data-driven assertions

### Remaining future work
- Counter collection (Phase 4 — `PacketCounters` events at 1Hz)
- `patchbay fmt-log -r .` recursive conversion
- Migrate `patchbay-vm` from `patchbay-utils/serve.rs` to `patchbay-server`
- `applyEvent` reducer handles only 6 event kinds — add remaining kinds
  (`link_condition_changed`, `interface_added`, etc.)
- SSE historical+live race window (subscribe before file read)

## Overview

This plan adds first-class observability to patchbay: typed lab events with
total ordering, per-device tracing and log capture, a file-based event log
with derived state, an axum-based server for live inspection, and UI updates
for topology visualization and multi-device log viewing.

The core design principle is **events as the single source of truth**.
`events.jsonl` is an append-only event log. `state.json` is a pure function
of `events.jsonl`. The server streams live updates by seeking to a given
`opid` position. No WebSockets, no database — just files and HTTP.

---

## Key design decisions

- **No backwards compat anywhere** — public API surface changes freely.
  `Serialize` derives added to `Nat`, `Firewall`, `LinkCondition`, etc.
  `serve.rs` removed. Runner output format may change.
- **Add `Serialize` to all config types** — `Nat`, `NatConfig`, `NatMapping`,
  `NatFiltering`, `ConntrackTimeouts`, `NatV6Mode`, `IpSupport`, `Firewall`,
  `FirewallConfig`, `PortPolicy`, `LinkCondition`, `LinkLimits`. Event and
  state payloads use these types directly (serde serialization), not string
  representations.
- **Events isolated to `patchbay/src/event.rs`** — event types, the writer,
  the state reducer, and the counter collector all live in one module.
  Other modules only call `self.inner.emit(kind)`.
- **Minimize new Rust code** — keep the event module lean. One file for
  events, writer, and state. No separate `counters.rs` or `writer.rs`.
- **PacketCounters are LabEvents**, debounced at 1s per node, only emitted
  on delta.
- **Lab-global state.json** — one file, all nodes. ~60KB at 100 nodes,
  written at 1Hz.
- **Flat file layout** — `{node_name}.log` not `{node_name}/log`. Files
  named by router/device name (not namespace name) for navigability.
- **No separate manifest.json** — topology is dynamic, state.json serves
  both roles.
- **JSON logs** with `patchbay fmt-log` for ANSI terminal viewing. JSON is
  canonical; ANSI is derived.
- **`fmt-log` matches tracing_subscriber default ANSI format** — including
  full span context, colored levels, target, message, and fields. Honors
  `NO_COLOR=1`.
- **`::_event::` target convention** — `patchbay::_event::nat_changed`,
  `iroh::_event::connected` etc. These are captured to per-node
  `{name}.events.jsonl` files AND served via a dedicated API route.
- **NetnsManager gets generic `on_worker_init`** callback, not tracing-
  specific code.
- **Broadcast channel capacity 256** — sufficient for typical lab usage.
  If receiver falls behind, it gets `Lagged` and can re-read from file.
- **`remove_device` / `remove_router` exist** — `DeviceRemoved` and
  `RouterRemoved` events are needed.
- **Topology tab is the first/default tab** in devtools mode.
- **Single-file build** — keep `vite-plugin-singlefile` + `include_str!`.
  Lazy-loading deferred to future work.
- **`InitialState` event** — emitted by `Lab::load()` / `Lab::from_config()`
  after reconstruction, carrying a full state snapshot.

---

## 1. Prerequisite: Add Serialize derives

Before any event work, add `#[derive(serde::Serialize)]` to these types.
No backwards compat concerns — this is a public API surface change.

### 1.1 `patchbay/src/nat.rs`

Add `Serialize` to all types. `Nat` already has `Deserialize` + `strum`:

```rust
// Nat — add Serialize to existing derives
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize,
         strum::EnumIter, strum::Display)]
#[serde(rename_all = "kebab-case")]
pub enum Nat { ... }

// NatMapping, NatFiltering, ConntrackTimeouts, NatConfig, NatV6Mode, IpSupport
// — add Serialize to existing derives on each
```

For `Nat::Custom(NatConfig)` — serde will serialize as
`{"custom": {"mapping": "...", ...}}`. The `#[serde(skip)]` on Custom for
Deserialize must be changed to allow deserialization too, or kept as-is
(Serialize-only for Custom is fine since it only appears in events/state,
not in TOML config).

**Decision**: Keep `#[serde(skip)]` on Custom for Deserialize (TOML config
doesn't need it), but Serialize will still work. Actually, `skip` skips
both — use `#[serde(skip_deserializing)]` instead so Serialize works.

### 1.2 `patchbay/src/firewall.rs`

Add `Serialize, Deserialize` to `Firewall`, `FirewallConfig`, `PortPolicy`:

```rust
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Firewall { ... }

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FirewallConfig { ... }

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortPolicy { ... }
```

### 1.3 `patchbay/src/lab.rs` — LinkCondition

`LinkCondition` has a custom `Deserialize` impl (handles string presets +
inline `LinkLimits` tables from TOML). Add `Serialize` derive:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkCondition { ... }
```

The custom `Deserialize` impl stays for TOML compatibility. `Serialize`
derive works independently and produces `"wifi"`, `"mobile_4g"`,
`{"manual": {...}}` etc.

### 1.4 `patchbay/src/qdisc.rs` — LinkLimits

Already has `Deserialize`. Add `Serialize`:

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkLimits { ... }
```

### 1.5 `patchbay/Cargo.toml`

Add `serde_json = "1"` to `[dependencies]`. `serde` is already there.

---

## 2. Data model: state.json schema

The state.json schema faithfully mirrors the Rust data structures. All types
that appear here now have `Serialize`, so we use structured values (not
string representations).

### Router → state.json mapping

| Rust source | state.json field | Type | Notes |
|---|---|---|---|
| `RouterData::name` | `name` | string | |
| `RouterData::ns` | `ns` | string | |
| `RouterData::region` | `region` | string\|null | |
| `RouterConfig::nat` | `nat` | Nat (serde) | e.g. `"home"`, `{"custom":{...}}` |
| `RouterConfig::nat_v6` | `nat_v6` | NatV6Mode | e.g. `"none"`, `"nat64"` |
| `RouterConfig::firewall` | `firewall` | Firewall | e.g. `"none"`, `"block_inbound"` |
| `RouterConfig::ip_support` | `ip_support` | IpSupport | e.g. `"v4_only"` |
| `RouterConfig::mtu` | `mtu` | u32\|null | |
| `RouterData::upstream_ip` | `uplink_ip` | string\|null | WAN addr, `.to_string()` |
| `RouterData::upstream_ip_v6` | `uplink_ip_v6` | string\|null | |
| `RouterData::downstream_cidr` | `downstream_cidr` | string\|null | e.g. `"10.0.1.0/24"` |
| `RouterData::downstream_gw` | `downstream_gw` | string\|null | e.g. `"10.0.1.1"` |
| `RouterData::downstream_cidr_v6` | `downstream_cidr_v6` | string\|null | |
| `RouterData::downstream_gw_v6` | `downstream_gw_v6` | string\|null | |
| uplink → switch → router resolve | `upstream` | string\|null | parent router name; null = IX |
| derived | `devices` | string[] | names of downstream devices |
| `set_downlink_condition()` | `downlink_condition` | LinkCondition\|null | |

### Device → state.json mapping

| Rust source | state.json field | Type | Notes |
|---|---|---|---|
| `DeviceData::name` | `name` | string | |
| `DeviceData::ns` | `ns` | string | |
| `DeviceData::default_via` | `default_via` | string | iface name |
| `DeviceData::mtu` | `mtu` | u32\|null | |
| `DeviceData::interfaces` | `interfaces` | IfaceState[] | see below |

### IfaceState (from `DeviceIfaceData`)

| Rust source | state.json field | Type | Notes |
|---|---|---|---|
| `ifname` | `name` | string | |
| uplink → switch → owner_router | `router` | string | owning router name |
| `ip` | `ip` | string\|null | |
| `ip_v6` | `ip_v6` | string\|null | |
| `impair` | `link_condition` | LinkCondition\|null | now serde-serialized |

### Complete state.json example

```jsonc
{
  "opid": 42,
  "lab_prefix": "lab-p12340",
  "label": null,
  "status": "running",
  "created_at": "2026-03-01T12:00:00Z",
  "ix": {
    "name": "ix",
    "bridge": "br-p1230-1",
    "cidr": "198.18.0.0/24",
    "gw": "198.18.0.1",
    "cidr_v6": "2001:db8::/64",
    "gw_v6": "2001:db8::1"
  },
  "routers": {
    "home": {
      "ns": "lab-p12340-r1",
      "region": null,
      "nat": "home",
      "nat_v6": "none",
      "firewall": "block_inbound",
      "ip_support": "v4_only",
      "mtu": null,
      "upstream": null,
      "uplink_ip": "198.18.0.2",
      "uplink_ip_v6": null,
      "downstream_cidr": "10.0.1.0/24",
      "downstream_gw": "10.0.1.1",
      "downstream_cidr_v6": null,
      "downstream_gw_v6": null,
      "downstream_bridge": "br-p1230-2",
      "downlink_condition": null,
      "devices": ["laptop", "phone"],
      "counters": {
        "ix": {"rx_bytes": 15000, "tx_bytes": 7500, "rx_packets": 100, "tx_packets": 50}
      }
    }
  },
  "devices": {
    "laptop": {
      "ns": "lab-p12340-d2",
      "default_via": "eth0",
      "mtu": null,
      "interfaces": [
        {
          "name": "eth0",
          "router": "home",
          "ip": "10.0.1.2",
          "ip_v6": null,
          "link_condition": null
        }
      ],
      "counters": {
        "eth0": {"rx_bytes": 7500, "tx_bytes": 15000, "rx_packets": 50, "tx_packets": 100}
      }
    }
  },
  "regions": {
    "us": {"router": "relay-us"},
    "eu": {"router": "relay-eu"}
  },
  "region_links": [
    {"a": "relay-us", "b": "relay-eu", "condition": null, "broken": false}
  ]
}
```

**Size estimate**: ~800 bytes/router, ~400 bytes/device. 100 nodes ≈ 60KB.

---

## 3. Physical topology reference

The UI must faithfully represent the Linux network topology. Every link type:

```
┌──────────────────────────────────────────────────────────────┐
│  ROOT NAMESPACE                                               │
│                                                               │
│  ┌────────────────────────────────────────────────────────┐  │
│  │ IX Bridge: br-{tag}-1  (198.18.0.1/24)                │  │
│  │                                                        │  │
│  │  veth {pfx}i{id}  ←─── router "r1"                    │  │
│  │  veth {pfx}i{id}  ←─── router "r2"                    │  │
│  └────────────────────────────────────────────────────────┘  │
│           │                              │                    │
│    ┌──────┼─────────────────────┐  ┌─────┼──────────────┐   │
│    │ ROUTER NS "r1"             │  │ ROUTER NS "r2"     │   │
│    │                            │  │                     │   │
│    │  "ix" (veth peer)          │  │  "ix" (veth peer)   │   │
│    │   uplink_ip: 198.18.0.2   │  │  uplink_ip: .3      │   │
│    │                            │  │                     │   │
│    │  ┌──────────────────────┐  │  └─────────────────────┘   │
│    │  │ downstream bridge    │  │                             │
│    │  │ br-{tag}-N           │  │                             │
│    │  │ 10.0.1.1/24          │  │                             │
│    │  │                      │  │                             │
│    │  │ veth {pfx}g{idx} ◄──┤──┼── peer in device NS "d1"   │
│    │  │ veth {pfx}g{idx} ◄──┤──┼── peer in device NS "d2"   │
│    │  └──────────────────────┘  │                             │
│    └────────────────────────────┘                             │
│                                                               │
│  ┌────────────────────────┐  ┌────────────────────────┐      │
│  │ DEVICE NS "d1"         │  │ DEVICE NS "d2"         │      │
│  │  {user-ifname}         │  │  {user-ifname}         │      │
│  │  ip: 10.0.1.2/24       │  │  ip: 10.0.1.3/24       │      │
│  │  default via 10.0.1.1  │  │  default via 10.0.1.1  │      │
│  └────────────────────────┘  └────────────────────────┘      │
│                                                               │
│  SUB-ROUTERS: parent NS has veth {pfx}a{id}, child has "wan" │
│  INTER-REGION: vr-{a}-{b} / vr-{b}-{a} point-to-point       │
└──────────────────────────────────────────────────────────────┘
```

### Veth naming conventions (from core.rs)

| Link type | Root/parent side | Namespace side | Notes |
|---|---|---|---|
| Router ↔ IX | `{pfx}i{router_id}` | `ix` | Port on IX bridge |
| Device ↔ Router | `{pfx}g{iface_idx}` | user-chosen ifname | Port on router bridge |
| Sub-router ↔ Parent | `{pfx}a{child_id}` | `wan` | Port on parent bridge |
| Inter-region | `vr-{a}-{b}` | `vr-{b}-{a}` | Point-to-point, each in its router NS |

`{pfx}` = `CoreConfig::veth_prefix` (default `"v"`),
`{tag}` = `CoreConfig::bridge_tag`.

---

## 4. Event system — `patchbay/src/event.rs`

**One new file.** Contains: event types, emit helper, LabWriter, LabState
reducer, and counter collection. Keep it lean.

### 4.1 Event types

```rust
use chrono::{DateTime, Utc};
use serde::{Serialize, Deserialize};

/// A single lab event with global ordering.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LabEvent {
    pub opid: u64,
    pub timestamp: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: LabEventKind,
}

/// All event variants. Internally-tagged with `"kind"` field.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LabEventKind {
    // ── Lifecycle ──
    LabCreated {
        lab_prefix: String,
        label: Option<String>,
    },
    InitialState {
        /// Full state snapshot, used by Lab::load()/from_config()
        /// so the event log captures loaded topology without replaying
        /// individual RouterAdded/DeviceAdded events.
        state: serde_json::Value,
    },
    LabStopping,

    // ── Topology ──
    IxCreated {
        bridge: String,
        cidr: String,
        gw: String,
        cidr_v6: String,
        gw_v6: String,
    },
    RouterAdded {
        name: String,
        ns: String,
        region: Option<String>,
        nat: serde_json::Value,          // Nat serialized
        nat_v6: serde_json::Value,       // NatV6Mode serialized
        firewall: serde_json::Value,     // Firewall serialized
        ip_support: serde_json::Value,   // IpSupport serialized
        mtu: Option<u32>,
        upstream: Option<String>,
        uplink_ip: Option<String>,
        uplink_ip_v6: Option<String>,
        downstream_cidr: Option<String>,
        downstream_gw: Option<String>,
        downstream_cidr_v6: Option<String>,
        downstream_gw_v6: Option<String>,
        downstream_bridge: String,
    },
    RouterRemoved {
        name: String,
    },
    DeviceAdded {
        name: String,
        ns: String,
        default_via: String,
        mtu: Option<u32>,
        interfaces: Vec<IfaceSnapshot>,
    },
    DeviceRemoved {
        name: String,
    },

    // ── Regions ──
    RegionAdded {
        name: String,
        router: String,
    },
    RegionLinkAdded {
        router_a: String,
        router_b: String,
    },
    RegionLinkBroken {
        router_a: String,
        router_b: String,
        condition: Option<serde_json::Value>,
    },
    RegionLinkRestored {
        router_a: String,
        router_b: String,
    },

    // ── Mutations ──
    NatChanged {
        router: String,
        nat: serde_json::Value,
    },
    NatV6Changed {
        router: String,
        nat_v6: serde_json::Value,
    },
    NatStateFlushed {
        router: String,
    },
    FirewallChanged {
        router: String,
        firewall: serde_json::Value,
    },
    LinkConditionChanged {
        device: String,
        iface: String,
        condition: Option<serde_json::Value>,
    },
    DownlinkConditionChanged {
        router: String,
        condition: Option<serde_json::Value>,
    },
    LinkUp {
        device: String,
        iface: String,
    },
    LinkDown {
        device: String,
        iface: String,
    },
    InterfaceAdded {
        device: String,
        iface: IfaceSnapshot,
    },
    InterfaceRemoved {
        device: String,
        iface_name: String,
    },
    InterfaceReplugged {
        device: String,
        iface_name: String,
        from_router: String,
        to_router: String,
        new_ip: Option<String>,
        new_ip_v6: Option<String>,
    },
    DeviceIpChanged {
        device: String,
        iface_name: String,
        new_ip: Option<String>,
        new_ip_v6: Option<String>,
    },

    // ── Processes ──
    CommandSpawned {
        node: String,
        pid: u32,
        cmd: String,
    },
    CommandExited {
        node: String,
        pid: u32,
        exit_code: Option<i32>,
    },

    // ── Counters ──
    PacketCounters {
        node: String,
        counters: Vec<IfaceCounters>,
    },
}
```

**Why `serde_json::Value` for config types?** Because the event struct uses
`#[serde(flatten)]` and the config types (Nat, Firewall, etc.) serialize to
varying shapes (string for presets, object for custom). Using
`serde_json::Value` avoids generic type parameters on the event enum while
keeping full fidelity. At emit sites:
```rust
self.inner.emit(LabEventKind::NatChanged {
    router: name.to_string(),
    nat: serde_json::to_value(&new_nat).unwrap(),
});
```

**Supporting types:**

```rust
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct IfaceSnapshot {
    pub name: String,
    pub router: String,
    pub ip: Option<String>,
    pub ip_v6: Option<String>,
    pub link_condition: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct IfaceCounters {
    pub iface: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_packets: u64,
    pub tx_packets: u64,
}
```

### 4.2 LabState — the state.json reducer

Also in `event.rs`. A struct that mirrors the state.json schema and applies
events to itself:

```rust
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LabState {
    pub opid: u64,
    pub lab_prefix: String,
    pub label: Option<String>,
    pub status: String,
    pub created_at: Option<DateTime<Utc>>,
    pub ix: Option<IxState>,
    pub routers: BTreeMap<String, RouterState>,
    pub devices: BTreeMap<String, DeviceState>,
    pub regions: BTreeMap<String, RegionState>,
    pub region_links: Vec<RegionLinkState>,
}

// RouterState, DeviceState, IxState, RegionState, RegionLinkState
// — structs matching the state.json schema above.
// All derive Serialize + Deserialize.
```

The reducer is a method:
```rust
impl LabState {
    pub fn apply(&mut self, event: &LabEvent) {
        self.opid = event.opid;
        match &event.kind {
            LabEventKind::LabCreated { lab_prefix, label } => {
                self.lab_prefix = lab_prefix.clone();
                self.label = label.clone();
                self.status = "running".into();
                self.created_at = Some(event.timestamp);
            }
            LabEventKind::RouterAdded { name, .. } => {
                // Insert RouterState from event fields
            }
            LabEventKind::RouterRemoved { name } => {
                self.routers.remove(name);
            }
            LabEventKind::DeviceAdded { name, .. } => {
                // Insert DeviceState, update parent router's devices list
            }
            LabEventKind::DeviceRemoved { name } => {
                // Remove from devices map, update parent router's devices list
                self.devices.remove(name);
            }
            LabEventKind::InitialState { state } => {
                // Replace self with deserialized state
                if let Ok(s) = serde_json::from_value(state.clone()) {
                    *self = s;
                }
            }
            // ... all other variants update the relevant state fields
        }
    }
}
```

### 4.3 LabWriter — file I/O (also in event.rs)

```rust
pub(crate) struct LabWriter {
    outdir: PathBuf,
    state: LabState,
    events_file: BufWriter<File>,
}

impl LabWriter {
    pub fn new(outdir: &Path) -> Result<Self> { ... }

    pub fn write_event(&mut self, event: &LabEvent) -> Result<()> {
        // 1. Append JSON line to events.jsonl
        serde_json::to_writer(&mut self.events_file, event)?;
        self.events_file.write_all(b"\n")?;
        self.events_file.flush()?;
        // 2. Apply to state
        self.state.apply(event);
        // 3. Atomic-write state.json (write to .tmp, rename)
        let tmp = self.outdir.join("state.json.tmp");
        let dst = self.outdir.join("state.json");
        std::fs::write(&tmp, serde_json::to_string_pretty(&self.state)?)?;
        std::fs::rename(&tmp, &dst)?;
        Ok(())
    }
}
```

The writer is spawned as a background tokio task that receives from the
broadcast channel:

```rust
pub(crate) fn spawn_writer(
    outdir: PathBuf,
    mut rx: broadcast::Receiver<LabEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut writer = match LabWriter::new(&outdir) {
            Ok(w) => w,
            Err(e) => { tracing::error!("LabWriter init failed: {e}"); return; }
        };
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Err(e) = writer.write_event(&event) {
                        tracing::error!("LabWriter error: {e}");
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("LabWriter lagged {n} events");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}
```

### 4.4 Counter collection (also in event.rs)

Parse `/proc/net/dev` inside each namespace. Run on the sync worker to
avoid namespace-enter overhead on the async RT.

```rust
pub(crate) fn parse_proc_net_dev(content: &str) -> Vec<IfaceCounters> {
    // Skip header lines, parse each interface line
    // Fields: iface: rx_bytes rx_packets ... tx_bytes tx_packets ...
}
```

Counter polling is a background task spawned per node, running at 1Hz,
only emitting when counters change (delta check).

### 4.5 Unit tests

In `#[cfg(test)] mod tests` at the bottom of `event.rs`:

- Serde round-trip for every `LabEventKind` variant
- `LabState::apply` test: create events, apply them, assert state
- `parse_proc_net_dev` test with sample `/proc/net/dev` content

---

## 5. Output directory structure

### Layout

```
{base}/
  {run_name}/
    events.jsonl          # lab-level event log
    state.json            # derived state (written at each event)
    {router_name}.log     # per-router JSON tracing log
    {device_name}.log     # per-device JSON tracing log
    {router_name}.events.jsonl  # per-router ::_event:: filtered events
    {device_name}.events.jsonl  # per-device ::_event:: filtered events
```

### Run name format

Single directory level: `{datetime}-{label_or_prefix}`

- `{datetime}` = `YYYYMMDD_HHMMSS` (local time, filesystem-friendly)
- `{label_or_prefix}` = lab label if set, otherwise lab prefix

Examples:
- `20260303_143021-my-holepunch-test/` (label set to "my-holepunch-test")
- `20260303_143021-lab-p12340/` (no label, falls back to prefix)

### Naming by node name, not namespace

Log files use the router/device **name** (e.g. `home.log`, `laptop.log`),
not the namespace name (e.g. `lab-p12340-r1.log`). This makes the output
directory navigable in a file tree without needing the UI.

The `on_worker_init` callback receives the namespace name; it must resolve
to the node name. Store a `ns_name → node_name` mapping in `LabInner`
(a `HashMap<String, String>` behind a `Mutex` or `RwLock`).

### Configuration

- `Lab::set_outdir(path)` — sets the base output directory
- `Lab::set_label(label)` — sets a human-readable label for the run
- `PATCHBAY_OUTDIR` env var — fallback if `set_outdir` not called
- If no outdir is configured, no files are written (events still flow
  through the broadcast channel)

Add fields to `LabInner`:
```rust
pub(crate) struct LabInner {
    pub core: std::sync::Mutex<NetworkCore>,
    pub netns: Arc<netns::NetnsManager>,
    pub cancel: CancellationToken,
    // NEW:
    pub opid: AtomicU64,
    pub events_tx: broadcast::Sender<LabEvent>,
    pub outdir: Option<PathBuf>,          // resolved run dir
    pub label: Mutex<Option<String>>,
    pub ns_to_name: Mutex<HashMap<String, String>>,
}
```

### Symlink

Create a `latest` symlink in the base dir pointing to the most recent run:
```
{base}/latest -> {base}/20260303_143021-my-test/
```

---

## 6. Per-namespace tracing & log capture

### 6.1 `on_worker_init` callback on NetnsManager

**Edit `patchbay/src/netns.rs`:**

Add a field to `NetnsManager`:
```rust
pub(crate) struct NetnsManager {
    parent_span: tracing::Span,
    workers: Mutex<HashMap<String, Worker>>,
    on_worker_init: Option<Arc<dyn Fn(&str) + Send + Sync>>,
}
```

Add a setter:
```rust
impl NetnsManager {
    pub fn set_on_worker_init(&mut self, f: impl Fn(&str) + Send + Sync + 'static) {
        self.on_worker_init = Some(Arc::new(f));
    }
}
```

Call it at the start of each async worker thread (after `unshare` but before
processing tasks), passing the namespace name. The callback is cloned into
the thread as an `Arc`.

NetnsManager does NOT import `tracing_subscriber` — it just calls the opaque
closure.

### 6.2 Install per-namespace tracing subscriber (lab.rs)

When `outdir` is set, provide the `on_worker_init` callback that:

1. Resolves ns name → node name via `LabInner::ns_to_name`
2. Opens `{outdir}/{node_name}.log` for writing
3. Opens `{outdir}/{node_name}.events.jsonl` for `::_event::` filtered writes
4. Creates a `tracing_subscriber` with two layers:
   - JSON format layer writing to the main `.log` file
   - `EventFilterLayer` writing `::_event::` events to `.events.jsonl`
5. Sets as thread-local default via `tracing::subscriber::set_default`
6. Stores the `DefaultGuard` in a `thread_local!` to keep it alive

```rust
thread_local! {
    static LOG_GUARD: RefCell<Option<tracing::subscriber::DefaultGuard>> =
        const { RefCell::new(None) };
}
```

### 6.3 EventFilterLayer

A minimal `tracing_subscriber::Layer` that filters for `::_event::` targets:

```rust
use tracing_subscriber::Layer;

struct EventFilterLayer {
    writer: Mutex<BufWriter<File>>,
}

impl<S: tracing::Subscriber> Layer<S> for EventFilterLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        if !event.metadata().target().contains("::_event::") {
            return;
        }
        // Serialize event fields as JSON line, write to file
    }
}
```

---

## 7. Event emission sites

### 7.1 Lab lifecycle (lab.rs)

| Site | Event |
|---|---|
| `Lab::new()` after IX bridge setup | `LabCreated` + `IxCreated` |
| `Lab::from_config()` after full reconstruction | `InitialState` |
| `LabInner::drop()` | `LabStopping` |
| `Lab::add_region()` | `RegionAdded` |
| `Lab::link_regions()` | `RegionLinkAdded` |
| `Lab::break_region_link()` | `RegionLinkBroken { condition }` |
| `Lab::restore_region_link()` | `RegionLinkRestored` |
| `Lab::remove_device()` | `DeviceRemoved` |
| `Lab::remove_router()` | `RouterRemoved` |

### 7.2 Builders (lab.rs)

| Site | Event |
|---|---|
| `RouterBuilder::build()` after setup_router_async | `RouterAdded` |
| `DeviceBuilder::build()` after setup_device_async | `DeviceAdded` |

### 7.3 Handle mutations (handles.rs)

| Method | Event |
|---|---|
| `Router::set_nat_mode()` | `NatChanged` |
| `Router::set_nat_v6_mode()` | `NatV6Changed` |
| `Router::flush_nat_state()` | `NatStateFlushed` |
| `Router::set_firewall()` | `FirewallChanged` |
| `Router::set_downlink_condition()` | `DownlinkConditionChanged` |
| `Device::set_link_condition()` | `LinkConditionChanged` |
| `Device::link_up()` | `LinkUp` |
| `Device::link_down()` | `LinkDown` |
| `Device::add_iface()` | `InterfaceAdded` |
| `Device::remove_iface()` | `InterfaceRemoved` |
| `Device::replug_iface()` | `InterfaceReplugged` |
| `Device::renew_ip()` | `DeviceIpChanged` |
| `Device::spawn_command()` | `CommandSpawned` |
| `Device::spawn_command_sync()` | `CommandSpawned` |
| `Router::spawn_command()` | `CommandSpawned` |
| `Router::spawn_command_sync()` | `CommandSpawned` |

Each site is a one-liner:
```rust
self.inner.emit(LabEventKind::NatChanged {
    router: self.name.to_string(),
    nat: serde_json::to_value(&mode).unwrap(),
});
```

### 7.4 Populating `ns_to_name` mapping

In `RouterBuilder::build()` and `DeviceBuilder::build()`, after the node is
created, insert into `LabInner::ns_to_name`:
```rust
self.inner.ns_to_name.lock().unwrap()
    .insert(router.ns().to_string(), router.name().to_string());
```

---

## 8. Server — `patchbay-server` crate

### 8.1 Crate setup

**New workspace member**: `patchbay-server`

```toml
# patchbay-server/Cargo.toml
[package]
name = "patchbay-server"
edition.workspace = true

[dependencies]
axum = { version = "0.8", features = ["tokio"] }
tokio = { version = "1", features = ["rt", "macros", "sync"] }
tokio-stream = "0.1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
patchbay = { path = "../patchbay" }
tower-http = { version = "0.6", features = ["cors", "fs"] }
```

Add to workspace `Cargo.toml`:
```toml
members = ["patchbay", "patchbay-utils", "patchbay-runner", "patchbay-vm", "patchbay-server"]
```

### 8.2 Server routes

**File: `patchbay-server/src/lib.rs`** (single file for the server crate)

```rust
pub struct DevtoolsServer { /* axum Router, lab handle, outdir */ }

impl DevtoolsServer {
    pub fn new(lab: patchbay::Lab, outdir: PathBuf, bind: &str) -> Self { ... }
    pub async fn run(self) -> Result<()> { ... }
}
```

**Routes:**

| Method | Path | Description |
|---|---|---|
| `GET /` | Serve embedded UI HTML (via `include_str!`) | |
| `GET /api/state` | Returns current `state.json` content | |
| `GET /api/events?after={opid}` | SSE stream of lab events after given opid | |
| `GET /api/logs/{name}?after={offset}` | Tail JSON log for a node by name | |
| `GET /api/logs/{name}/download` | Full log file download | |
| `GET /api/node-events/{name}?after={offset}` | Tail `::_event::` events for a node | |
| `GET /api/node-events/{name}/download` | Full node events file download | |

**SSE endpoint** (`/api/events`):

Subscribe to the lab's broadcast channel. For events before `after`, read
from `events.jsonl` file. For live events, stream from the subscription.
Each SSE message is a JSON-serialized `LabEvent`.

```rust
async fn events_sse(
    Query(params): Query<EventsQuery>,
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let after = params.after.unwrap_or(0);
    // 1. Read events.jsonl for historical events > after
    // 2. Subscribe to broadcast for live events
    // 3. Chain both streams
}
```

**Node events endpoint** (`/api/node-events/{name}`):

Reads the `{name}.events.jsonl` file. Supports `?after={offset}` for byte
offset tailing. Returns JSON lines.

### 8.3 UI serving

The embedded UI HTML is served from `include_str!` of the built
`dist/index.html`. In dev mode, proxy to Vite dev server instead.

---

## 9. UI changes

### 9.1 New dependencies

**Edit `ui/package.json`:**
```json
"dependencies": {
    "@xyflow/react": "^12",
    "dagre": "^0.8"
}
"devDependencies": {
    "@types/dagre": "^0.7"
}
```

### 9.2 TypeScript types — `ui/src/devtools-types.ts`

Mirror the state.json schema. All types derive from the Rust `LabState`:

```typescript
export interface LabState {
  opid: number;
  lab_prefix: string;
  label: string | null;
  status: string;
  created_at: string | null;
  ix: IxState | null;
  routers: Record<string, RouterState>;
  devices: Record<string, DeviceState>;
  regions: Record<string, RegionState>;
  region_links: RegionLinkState[];
}

export interface IxState {
  name: string;
  bridge: string;
  cidr: string;
  gw: string;
  cidr_v6: string;
  gw_v6: string;
}

export interface RouterState {
  ns: string;
  region: string | null;
  nat: unknown;        // serde_json::Value — string for presets, object for custom
  nat_v6: unknown;
  firewall: unknown;
  ip_support: unknown;
  mtu: number | null;
  upstream: string | null;
  uplink_ip: string | null;
  uplink_ip_v6: string | null;
  downstream_cidr: string | null;
  downstream_gw: string | null;
  downstream_cidr_v6: string | null;
  downstream_gw_v6: string | null;
  downstream_bridge: string;
  downlink_condition: unknown | null;
  devices: string[];
  counters: Record<string, IfaceCounters>;
}

export interface DeviceState {
  ns: string;
  default_via: string;
  mtu: number | null;
  interfaces: IfaceState[];
  counters: Record<string, IfaceCounters>;
}

export interface IfaceState {
  name: string;
  router: string;
  ip: string | null;
  ip_v6: string | null;
  link_condition: unknown | null;
}

export interface IfaceCounters {
  rx_bytes: number;
  tx_bytes: number;
  rx_packets: number;
  tx_packets: number;
}

export interface RegionState {
  router: string;
}

export interface RegionLinkState {
  a: string;
  b: string;
  condition: unknown | null;
  broken: boolean;
}

export interface LabEvent {
  opid: number;
  timestamp: string;
  kind: string;
  [key: string]: unknown;
}
```

### 9.3 API client — `ui/src/devtools-api.ts`

```typescript
const API_BASE = '/api';

export async function fetchState(): Promise<LabState> {
  const res = await fetch(`${API_BASE}/state`);
  return res.json();
}

export function subscribeEvents(
  afterOpid: number,
  onEvent: (event: LabEvent) => void,
): EventSource {
  const es = new EventSource(`${API_BASE}/events?after=${afterOpid}`);
  es.onmessage = (msg) => {
    onEvent(JSON.parse(msg.data));
  };
  return es;
}

export async function fetchNodeLogs(name: string, after = 0): Promise<string> {
  const res = await fetch(`${API_BASE}/logs/${name}?after=${after}`);
  return res.text();
}

export async function fetchNodeEvents(name: string, after = 0): Promise<string> {
  const res = await fetch(`${API_BASE}/node-events/${name}?after=${after}`);
  return res.text();
}
```

### 9.4 App.tsx changes

Detect devtools mode: if `window.location.pathname === '/'` and
`/api/state` responds successfully, enter devtools mode. Otherwise, show
the existing sim results UI.

In devtools mode, show tabs: **Topology** (default), **Logs**, **Events**.

### 9.5 TopologyGraph component — `ui/src/components/TopologyGraph.tsx`

Uses `@xyflow/react` + `dagre` for automatic hierarchical layout.
See Section 10 for the exact graph drawing algorithm.

### 9.6 NodeDetail component — `ui/src/components/NodeDetail.tsx`

Sidebar panel shown when a node is selected in the topology graph.
Shows all state fields for the selected router or device.

### 9.7 CSS additions to `ui/src/index.css`

Add classes for topology nodes:
```css
.topology-node { /* base node style */ }
.topology-node--ix { /* IX node: accent colored banner */ }
.topology-node--router { /* router node */ }
.topology-node--device { /* device node */ }
.badge { /* small pill for NAT/firewall labels */ }
.badge--nat { /* NAT badge color */ }
.badge--fw { /* firewall badge color */ }
.node-detail { /* sidebar panel */ }
```

### 9.8 vite.config.ts changes

Add `/api` proxy for dev mode:
```typescript
server: {
  proxy: {
    '/api': 'http://localhost:3000',
  },
},
```

---

## 10. Graph drawing algorithm

### Step 1: Build nodes

```typescript
import { Node } from '@xyflow/react';

function buildNodes(state: LabState): Node[] {
  const nodes: Node[] = [];

  // IX node (always one, at top)
  if (state.ix) {
    nodes.push({
      id: 'ix',
      type: 'ix',
      position: { x: 0, y: 0 },
      data: { label: 'Internet Exchange', ...state.ix },
    });
  }

  // Router nodes
  for (const [name, router] of Object.entries(state.routers)) {
    nodes.push({
      id: `router:${name}`,
      type: 'router',
      position: { x: 0, y: 0 },
      data: { label: name, ...router },
    });
  }

  // Device nodes
  for (const [name, device] of Object.entries(state.devices)) {
    nodes.push({
      id: `device:${name}`,
      type: 'device',
      position: { x: 0, y: 0 },
      data: { label: name, ...device },
    });
  }

  return nodes;
}
```

### Step 2: Build edges

```typescript
import { Edge } from '@xyflow/react';

function buildEdges(state: LabState): Edge[] {
  const edges: Edge[] = [];

  // Router → IX or Router → parent router
  for (const [name, router] of Object.entries(state.routers)) {
    if (router.upstream === null) {
      edges.push({
        id: `ix-to-${name}`,
        source: 'ix',
        target: `router:${name}`,
        type: 'smoothstep',
        label: router.uplink_ip ?? undefined,
        data: { linkType: 'ix-uplink' },
      });
    } else {
      edges.push({
        id: `${router.upstream}-to-${name}`,
        source: `router:${router.upstream}`,
        target: `router:${name}`,
        type: 'smoothstep',
        label: router.uplink_ip ?? undefined,
        data: { linkType: 'sub-router' },
      });
    }
  }

  // Device → Router (one edge per interface)
  for (const [name, device] of Object.entries(state.devices)) {
    for (const iface of device.interfaces) {
      edges.push({
        id: `${name}:${iface.name}-to-${iface.router}`,
        source: `router:${iface.router}`,
        target: `device:${name}`,
        type: 'smoothstep',
        label: iface.ip ?? undefined,
        data: { linkType: 'device-uplink', linkCondition: iface.link_condition },
      });
    }
  }

  // Inter-region links (router ↔ router, dashed)
  for (const link of state.region_links ?? []) {
    edges.push({
      id: `region:${link.a}-${link.b}`,
      source: `router:${link.a}`,
      target: `router:${link.b}`,
      type: 'smoothstep',
      animated: link.broken,
      style: { strokeDasharray: '5 5' },
      data: { linkType: 'region', condition: link.condition, broken: link.broken },
    });
  }

  return edges;
}
```

### Step 3: Dagre layout

```typescript
import dagre from 'dagre';

function layoutGraph(nodes: Node[], edges: Edge[]): Node[] {
  const g = new dagre.graphlib.Graph();
  g.setDefaultEdgeLabel(() => ({}));
  g.setGraph({
    rankdir: 'TB',
    nodesep: 80,
    ranksep: 120,
    marginx: 40,
    marginy: 40,
  });

  const dims: Record<string, { w: number; h: number }> = {
    ix:     { w: 160, h: 50 },
    router: { w: 180, h: 70 },
    device: { w: 160, h: 60 },
  };

  for (const node of nodes) {
    const d = dims[node.type ?? 'device'];
    g.setNode(node.id, { width: d.w, height: d.h });
  }
  for (const edge of edges) {
    g.setEdge(edge.source, edge.target);
  }

  dagre.layout(g);

  return nodes.map((node) => {
    const pos = g.node(node.id);
    const d = dims[node.type ?? 'device'];
    return {
      ...node,
      position: { x: pos.x - d.w / 2, y: pos.y - d.h / 2 },
    };
  });
}
```

### Step 4: Custom node components

```typescript
const nodeTypes = {
  ix: IxNode,
  router: RouterNode,
  device: DeviceNode,
};

function RouterNode({ data }: { data: RouterState & { label: string } }) {
  return (
    <div className="topology-node topology-node--router">
      <div className="topology-node__header">{data.label}</div>
      <div className="topology-node__badges">
        {data.nat !== 'none' && <span className="badge badge--nat">{String(data.nat)}</span>}
        {data.firewall !== 'none' && <span className="badge badge--fw">{String(data.firewall)}</span>}
      </div>
      <div className="topology-node__ip">{data.uplink_ip ?? 'no uplink'}</div>
    </div>
  );
}
```

### Step 5: Live updates via SSE

When an SSE event arrives:
1. Apply to local `LabState` via the same reducer logic.
2. Re-run `buildNodes()` + `buildEdges()` (cheap pure functions).
3. Re-run `layoutGraph()` **only if nodes were added/removed** (check count).
   For property-only changes (NAT mode, link condition), update node data
   without re-layout to avoid jarring movement.
4. React Flow's `useNodesState` / `useEdgesState` handles re-render.

### Step 6: Node selection → detail panel

Click a node → set `selectedNode` state. The `NodeDetail` sidebar shows:
- **Router**: all RouterState fields, device list, packet counters
- **Device**: all interfaces with IPs, link conditions, counters

---

## 11. CLI: fmt-log command

### 11.1 Subcommand

**Edit `patchbay-runner/src/main.rs`**, add to the `Command` enum:

```rust
/// Format JSON log files as human-readable ANSI output.
FmtLog {
    /// Path to a JSON log file ({name}.log).
    file: PathBuf,
    /// Follow the file for new lines (like tail -f).
    #[arg(short = 'f', long, default_value_t = false)]
    follow: bool,
},
```

### 11.2 Implementation — `patchbay-runner/src/fmt_log.rs`

**New file.** Reads JSON log lines (as produced by `tracing_subscriber`'s
JSON format layer) and re-renders them in the exact same format as
`tracing_subscriber`'s default ANSI format.

**tracing_subscriber's default ANSI format:**
```
2026-03-03T14:30:00.123456Z  INFO setup_router{name="home"}: patchbay::core: applying NAT rules nat=Home
```

Format: `{timestamp} {level:>5} {spans}: {target}: {message} {fields}`

Where:
- `{timestamp}` — ISO 8601 with microseconds
- `{level:>5}` — right-padded to 5 chars, ANSI colored:
  - ERROR = red, WARN = yellow, INFO = green, DEBUG = blue, TRACE = purple
- `{spans}` — chain of `span_name{field=value}` separated by `:`,
  representing the active span stack
- `{target}` — the tracing target (module path)
- `{message}` — the log message
- `{fields}` — key=value pairs after the message

**JSON input format** (from tracing_subscriber JSON layer):
```json
{
  "timestamp": "2026-03-03T14:30:00.123456Z",
  "level": "INFO",
  "fields": {"message": "applying NAT rules", "nat": "Home"},
  "target": "patchbay::core",
  "spans": [{"name": "setup_router", "name": "home"}]
}
```

**Note**: The spans array from tracing_subscriber's JSON layer contains
objects with the span name and fields. The exact field names depend on how
spans are instrumented (e.g. `#[instrument(name = "setup_router")]` with
fields). Parse them and reconstruct the `span_name{field=value}` format.

### 11.3 NO_COLOR support

Check `std::env::var("NO_COLOR")`. If set (to any value, per the
[NO_COLOR spec](https://no-color.org/)), disable all ANSI color codes.
Also disable if stdout is not a terminal (`!std::io::stdout().is_terminal()`).

```rust
fn use_color() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}
```

### 11.4 Follow mode

With `--follow`, after reading existing lines, inotify-watch the file
(or poll at 100ms) for new lines and format them as they arrive.

---

## 12. Implementation phases

### Phase 1: Serialize derives + event types + emit + subscribe

**Goal**: Typed events flowing through broadcast channel. No file I/O.

1. Add `Serialize` to all config types (Section 1)
2. Create `patchbay/src/event.rs` with types + LabState reducer (Section 4)
3. Add `pub mod event;` to `patchbay/src/lib.rs`, re-export key types
4. Add event infrastructure to `LabInner` (Section 5)
5. Add `emit()` to `LabInner`, `subscribe()` to `Lab`
6. Emit events from all sites (Section 7)
7. Unit tests for serde round-trip and state reducer

### Phase 2: LabWriter + output directory

**Goal**: File output. events.jsonl + state.json written to disk.

1. Add LabWriter to `event.rs` (Section 4.3)
2. Add outdir/label config to Lab (Section 5)
3. Spawn writer task on Lab creation when outdir is set
4. Add `ns_to_name` mapping, populate from builders

### Phase 3: Per-namespace tracing

**Goal**: Per-node JSON log files + per-node events.jsonl.

1. Add `on_worker_init` to NetnsManager (Section 6.1)
2. Install per-namespace subscriber in lab.rs (Section 6.2)
3. Implement EventFilterLayer (Section 6.3)

### Phase 4: Counter collection

**Goal**: PacketCounters events emitted at 1Hz.

1. Implement `parse_proc_net_dev` in event.rs (Section 4.4)
2. Spawn per-node counter polling tasks
3. Delta detection — only emit when counters change

### Phase 5: Server

**Goal**: axum server serving state, events SSE, logs, node events.

1. Create `patchbay-server` crate (Section 8)
2. Implement all routes
3. SSE endpoint with historical + live event streaming
4. Embedded UI serving via `include_str!`

### Phase 6: UI

**Goal**: Topology graph, node detail panel, devtools mode.

1. Add npm dependencies (Section 9.1)
2. Create TypeScript types (Section 9.2)
3. Create API client (Section 9.3)
4. Modify App.tsx for devtools mode detection (Section 9.4)
5. Build TopologyGraph component (Section 9.5, Section 10)
6. Build NodeDetail component (Section 9.6)
7. Add CSS classes (Section 9.7)
8. Update vite.config.ts (Section 9.8)

### Phase 7: CLI fmt-log

**Goal**: `patchbay fmt-log` command.

1. Add FmtLog subcommand to runner (Section 11.1)
2. Implement JSON → ANSI formatter (Section 11.2)
3. NO_COLOR support (Section 11.3)
4. Follow mode (Section 11.4)

### Phase 8: E2E test

Integration test that:
1. Creates a lab with outdir set
2. Adds routers + devices
3. Mutates state (change NAT, set link condition)
4. Verifies events.jsonl has expected events
5. Verifies state.json reflects final state
6. Starts server, fetches `/api/state`, asserts correct
7. Subscribes to SSE, triggers mutation, receives event
8. Reads per-node log files, verifies they contain expected tracing output
9. Reads per-node events.jsonl via `/api/node-events/{name}`
10. Removes a device, verifies DeviceRemoved in events + state updated

### Dependency graph

```
Phase 1 (events) ──→ Phase 2 (writer) ──→ Phase 3 (tracing)
                 ╲                    ╲
                  → Phase 4 (counters)  → Phase 5 (server) → Phase 6 (UI)
                                                           ╲
                                                            Phase 7 (fmt-log)
                                                           ╱
                                        Phase 8 (E2E, after all)
```

Critical path: 1 → 2 → 5 → 6.
Phases 3, 4, 7 are parallelizable once their dependencies are met.

---

## 13. File summary

### New files

| File | Description |
|---|---|
| `patchbay/src/event.rs` | Event types, LabState reducer, LabWriter, counter parser |
| `patchbay-server/Cargo.toml` | New crate manifest |
| `patchbay-server/src/lib.rs` | Server: routes, SSE, embedded UI |
| `patchbay-runner/src/fmt_log.rs` | JSON → ANSI log formatter |
| `ui/src/devtools-types.ts` | TypeScript state/event types |
| `ui/src/devtools-api.ts` | API client (fetch + SSE) |
| `ui/src/components/TopologyGraph.tsx` | React Flow topology viz |
| `ui/src/components/NodeDetail.tsx` | Node detail sidebar panel |

### Edited files

| File | Changes |
|---|---|
| `Cargo.toml` (workspace) | Add patchbay-server member |
| `patchbay/Cargo.toml` | Add `serde_json` dep |
| `patchbay/src/lib.rs` | Add `pub mod event`, re-exports |
| `patchbay/src/nat.rs` | Add Serialize derives, fix `#[serde(skip)]` → `#[serde(skip_deserializing)]` on Custom |
| `patchbay/src/firewall.rs` | Add Serialize + Deserialize derives |
| `patchbay/src/lab.rs` | Add Serialize to LinkCondition; emit events from builders, Lab::new, remove_device/router, regions; add set_outdir/set_label; spawn writer; populate ns_to_name |
| `patchbay/src/qdisc.rs` | Add Serialize to LinkLimits |
| `patchbay/src/core.rs` | Add fields to LabInner (opid, events_tx, outdir, label, ns_to_name); add emit() method |
| `patchbay/src/handles.rs` | Emit events from all mutation methods |
| `patchbay/src/netns.rs` | Add `on_worker_init` callback field + setter; call from async worker thread |
| `patchbay-runner/Cargo.toml` | Add patchbay-server dep |
| `patchbay-runner/src/main.rs` | Add FmtLog subcommand, update Serve to use patchbay-server |
| `ui/package.json` | Add @xyflow/react, dagre deps |
| `ui/src/App.tsx` | Devtools mode detection, topology tab (default) |
| `ui/src/index.css` | Topology node + badge CSS classes |
| `ui/vite.config.ts` | Add /api proxy for dev mode |

### Deleted files

| File | Reason |
|---|---|
| `patchbay-utils/src/serve.rs` | Replaced by patchbay-server |
| `patchbay-utils/src/ui.rs` | `start_ui_server` replaced |

**Total**: ~8 new files, ~17 edited files, ~2 deleted files.
