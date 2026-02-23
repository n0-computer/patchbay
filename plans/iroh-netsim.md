# Plan: Iroh Netsims (v4)

## TODO

- [x] Write plan
- [x] Phase 1: `core.rs` multi-iface `Device` types, `DeviceBuilder`, `IfaceBuild`/`wire_iface`, TOML parse for `[[router]]`/`[device.*.*]`
- [x] Phase 2: dynamic ops (`set_impair`, `link_down/up`, `switch_route`, `remove_qdisc_r`) + tests
- [x] Phase 3: `sim/topology.rs`, `env.rs`, `build.rs`, `transfer.rs`, `report.rs`, `runner.rs`, CLI wiring, integration files
- [x] Phase 4: `count` expansion, `fetchers = [...]` in runner, 1→N sim files
- [x] Phase 5: shared binary manifests (`[sim] binaries`), CLI `--binary` overrides, path-copy semantics
- [ ] End-to-end `cargo make run-vm` proof recorded
- [ ] Override validation startup summary table + full merge/precedence tests
- [ ] Final review

Goal: `cargo run -- iroh-integration/sims/iroh-1to1-public.toml` builds a network, runs an iroh transfer
sim, applies scheduled network events, and reports results.

Reference: `resources/chuck/netsim/` — the Python/mininet implementation we're
porting. `resources/dogfood/` — Rust transfer logic to port.

---

## Status: DRAFT v4

**Key changes from v3:**
- Process log structure and result parsing added (were entirely missing)
- `kind = "iroh-transfer"` sequence corrected: `PathStats` = end-of-connection on
  provider side (not a metric, not readiness); `connected_via` comes from
  `ConnectionTypeChanged` events; `--output json --logs-path` flags documented
- Transfer runs always pass `--logs-path` and generate `results.md` in addition to `results.json`
- Added `strategy = "endpoint_id_with_direct_addrs"` for direct-address fetches
- `[[binary]]` gains `url` source option; 
- `wait-for` default timeout specified: 300 s
- `count` restored (Phase 3, needed for 1→N sims)
- Capture→substitution dependency ordering documented
- IROH_DATA_DIR dropped (no longer needed in modern iroh)

---

## 1. Router Format — Unified `[[router]]`

All topology nodes are `[[router]]`. No more `[[isp]]` / `[[dc]]` / `[[lan]]`.

```toml
[[router]]
name   = "dc-eu"
region = "eu"
# no upstream → IX-attached; public downstream pool; no NAT

[[router]]
name   = "isp-eu"
region = "eu"
nat    = "cgnat"
# cgnat → private downstream pool; SNAT on IX interface

[[router]]
name     = "lan-eu"
upstream = "isp-eu"
nat      = "destination-independent"
# upstream set → subscriber link into isp-eu's bridge
```

### Field semantics

| Field               | Values                                                                            | Default                                    |
|---------------------|-----------------------------------------------------------------------------------|--------------------------------------------|
| `name`              | string                                                                            | required                                   |
| `region`            | string (for inter-region latency)                                                 | none                                       |
| `upstream`          | router name                                                                       | none → connects to IX bridge               |
| `nat`               | `"none"` / `"cgnat"` / `"destination-independent"` / `"destination-dependent"`   | `"none"`                                   |
| `alloc_global_ipv4` | bool — downstream pool selection                                                  | `true` if no upstream; `false` otherwise   |

**`nat` values:**
- `"none"` — no NAT; downstream addresses are publicly routable (DC behaviour)
- `"cgnat"` — SNAT subscriber traffic on the IX-facing interface
- `"destination-independent"` — EIM home NAT (`snat … persistent`)
- `"destination-dependent"` — EDM/symmetric home NAT (`masquerade random`)

**`alloc_global_ipv4`:** inferred from `nat`; only override when the default is
wrong. Drop from TOML for v1.

### Rust API

```rust
impl Lab {
    pub fn add_router(
        &mut self,
        name: &str,
        region: Option<&str>,
        upstream: Option<NodeId>,
        nat: NatMode,
    ) -> Result<NodeId>;
}

pub enum NatMode {
    None,
    Cgnat,
    DestinationIndependent,
    DestinationDependent,
}
```

`add_router` infers `alloc_global_ipv4` and `cgnat` from `nat`.

---

## 2. Device Format — Per-Interface Tables

Each interface is declared as a sub-table `[device.<name>.<ifname>]`.
Device-level settings go in `[device.<name>]` (optional).

```toml
# Simple: single interface
[device.provider.eth0]
gateway = "dc-eu"

# Multi-interface: two pre-wired uplinks
[device.fetcher]
default_via = "eth1"          # eth1 is active at startup

[device.fetcher.eth0]
gateway = "isp-eu"
impair  = "mobile"

[device.fetcher.eth1]
gateway = "lan-eu"
impair  = "wifi"
```

### Field semantics

`[device.<name>]` (device-level, optional):

| Field        | Values              | Default                     |
|--------------|---------------------|-----------------------------|
| `default_via`| interface name      | first interface encountered |
| `count`      | integer ≥ 1         | 1 (Phase 3)                 |

`[device.<name>.<ifname>]` (per-interface):

| Field     | Values                                          | Default  |
|-----------|-------------------------------------------------|----------|
| `gateway` | router name                                     | required |
| `impair`  | `"wifi"` / `"mobile"` / `{ rate, loss, latency }` | none  |

### `count` shorthand (Phase 3)

```toml
[device.fetcher]
count = 3

[device.fetcher.eth0]
gateway = "dc-eu"
impair  = { loss = 1, latency = 200, rate_kbit = 1000 }
```

Creates `fetcher-0`, `fetcher-1`, `fetcher-2`, each with one `eth0`.
Env vars: `$NETSIM_IP_fetcher_0`, `$NETSIM_IP_fetcher_1`, etc.
Only valid when all interfaces are identical (single-template expansion).

### TOML parsing note

Parse `device` as `HashMap<String, toml::Value>` (raw), then post-process:
- String/integer values at device level → device-level config
- Table values → interface definitions (key = ifname)

---

## 3. Environment Variables

Every process started in a step receives:

| Variable                        | Value                                           |
|---------------------------------|-------------------------------------------------|
| `NETSIM_IP_<device>`            | IP of the `default_via` interface               |
| `NETSIM_IP_<device>_<ifname>`   | IP of the named interface                       |
| `NETSIM_IP_<device>_<N>`        | IP of the Nth `count`-expanded device (Phase 3) |
| `NETSIM_NS_<device>`            | netns name (one namespace per device always)    |

Variable name normalisation: device/interface names have `-` → `_`, uppercased.

Additionally, chuck-compatible variables set for every process:

| Variable       | Value                                         |
|----------------|-----------------------------------------------|
| `RUST_LOG_STYLE` | `never` (disables ANSI colour; required for NDJSON parsing) |
| `RUST_LOG`     | `warn,iroh::_events::conn_type=trace` unless caller sets it |
| `SSLKEYLOGFILE`| `<work_dir>/logs/keylog_<step_id>.txt`        |

User can override any of these via `env = { KEY = "value" }` on the step.

Do not set `IROH_DATA_DIR`; modern iroh no longer requires it.

---

## 4. Rust Types — Device

```rust
// In core.rs

pub struct Device {
    pub id:          DeviceId,
    pub name:        String,
    pub ns:          String,
    pub interfaces:  Vec<DeviceIface>,  // in declaration order
    pub default_via: String,            // ifname of the active default route
}

pub struct DeviceIface {
    pub ifname: String,
    pub uplink: SwitchId,
    pub ip:     Option<Ipv4Addr>,
    pub impair: Option<Impair>,
}

impl Device {
    pub fn iface(&self, name: &str) -> Option<&DeviceIface> {
        self.interfaces.iter().find(|i| i.ifname == name)
    }
    pub fn default_iface(&self) -> &DeviceIface {
        self.iface(&self.default_via).expect("default_via is valid")
    }
}
```

Old `Device` fields `uplink`, `ip`, `impair_upstream` are removed.

---

## 5. Rust API — DeviceBuilder

```rust
impl Lab {
    /// Start building a device.  Call `.iface(…)` one or more times, then `.build()`.
    pub fn add_device(&mut self, name: &str) -> DeviceBuilder<'_>;
}

pub struct DeviceBuilder<'a> {
    lab:         &'a mut Lab,
    name:        String,
    interfaces:  Vec<IfaceCfg>,
    default_via: Option<String>,
}

struct IfaceCfg {
    ifname:  String,
    gateway: NodeId,
    impair:  Option<Impair>,
}

impl<'a> DeviceBuilder<'a> {
    pub fn iface(mut self, ifname: &str, gateway: NodeId, impair: Option<Impair>) -> Self;
    pub fn default_via(mut self, ifname: &str) -> Self;
    pub fn build(self) -> Result<NodeId>;
}
```

Usage:

```rust
let provider = lab.add_device("provider")
    .iface("eth0", dc_eu, None)
    .build()?;

let fetcher = lab.add_device("fetcher")
    .iface("eth0", isp_eu, Some(Impair::Mobile))
    .iface("eth1", lan_eu, Some(Impair::Wifi))
    .default_via("eth1")
    .build()?;
```

`add_isp`, `add_dc`, `add_home`, and the `Gateway` enum are **removed**.
All existing tests updated to use `add_router` + `DeviceBuilder`.

---

## 6. Build Changes — Multi-Interface Wiring

`DevBuild` → `IfaceBuild` (one record per device-interface pair).
`wire_device` → `wire_iface`.

```rust
struct IfaceBuild {
    dev_ns:     String,
    gw_ns:      String,
    gw_ip:      Ipv4Addr,
    gw_br:      String,
    dev_ip:     Ipv4Addr,
    prefix_len: u8,
    impair:     Option<Impair>,
    ifname:     String,      // "eth0", "eth1", …
    is_default: bool,        // only this interface gets `ip route add default`
    idx:        u64,         // globally unique; drives veth naming
}
```

`wire_iface` identical to current `wire_device` except:
- Rename the device-side veth to `dev.ifname` instead of always `"eth0"`.
- Only call `add_default_route_v4` when `is_default == true`.

`LabCore::build` collects one `IfaceBuild` per `(device, interface)` pair and
calls `wire_iface` for each.

---

## 7. Dynamic Network Operations

### 7a. `qdisc::remove_qdisc_r`

The existing `remove_qdisc` silently swallows errors.  Add a fallible version:

```rust
pub(crate) fn remove_qdisc_r(ns: &str, ifname: &str) -> Result<()> {
    let status = run_in_netns(ns, {
        let mut cmd = Command::new("tc");
        cmd.args(["qdisc", "del", "dev", ifname, "root"]);
        cmd.stderr(Stdio::null());
        cmd
    })?;
    // exit code 2 = ENOENT (no such qdisc) — acceptable
    if !status.success() && status.code() != Some(2) {
        bail!("tc qdisc del failed on {} in {}", ifname, ns);
    }
    Ok(())
}
```

### 7b. `Lab::set_impair`

```rust
impl Lab {
    pub fn set_impair(
        &mut self,
        device: &str,
        ifname: Option<&str>,   // None → default_via
        impair: Option<Impair>, // None → remove
    ) -> Result<()>;
}
```

### 7c. `Lab::link_down` / `Lab::link_up`

```rust
impl Lab {
    pub fn link_down(&mut self, device: &str, ifname: &str) -> Result<()>;
    pub fn link_up  (&mut self, device: &str, ifname: &str) -> Result<()>;
}
```

`ip link set <ifname> down/up` via `with_netns_thread`.

### 7d. `Lab::switch_route`

```rust
impl Lab {
    pub fn switch_route(&mut self, device: &str, to: &str) -> Result<()>;
}
```

`to` is always an explicit interface name (`"eth0"`, `"eth1"`).

Implementation:
1. Resolve `iface = dev.iface(to)`.
2. In device netns: `ip route del default` then `ip route add default via <gw_ip> dev <ifname>`.
3. Re-apply impairment on the newly active interface (or remove if none).
4. Update `dev.default_via = to.to_string()`.

`LabCore` additions needed:
```rust
pub fn router_downlink_gw_for_switch(&self, sw: SwitchId) -> Result<Ipv4Addr>;
pub fn set_device_default_via(&mut self, name: &str, ifname: &str) -> Result<()>;
```

All dynamic-op methods take `&mut self` — the sim runner holds exclusive access.

---

## 8. Process Logs

Every `spawn` and `run` step tees the process's stdout+stderr to a log file
while also streaming live for `ready_when` / `captures` matching.

Directory layout under `<work_dir>/`:

```
<work_dir>/
  logs/
    <step_id>.log          # stdout+stderr of the process (non-transfer)
    keylog_<step_id>.txt   # SSLKEYLOGFILE
  results.json             # written at end of sim
  results.md               # table overview
```

`step_id` is the `id` field on `spawn` steps, or `<action>_<device>` for
unnamed `run` steps.

For `kind = "iroh-transfer"`, the transfer binary writes NDJSON logs via
`--logs-path`. These files are the primary log source for parsing.

```
logs/
  xfer_provider.ndjson
  xfer_fetcher.ndjson          # (or xfer_fetcher_0.ndjson, xfer_fetcher_1.ndjson for count > 1)
  keylog_xfer_provider.txt
  keylog_xfer_fetcher.txt
```

Stdout/stderr are only captured for readiness/captures and are not the source
of truth for transfer stats.

---

## 9. Result Parsing and Reporting

After all steps complete, the runner post-processes logs and writes:

- `<work_dir>/results.json`
- `<work_dir>/results.md` (table overview suitable for GH comment/preview)

### Transfer stats (iroh)

Scan the fetcher NDJSON log (from `--logs-path`) for `DownloadComplete`:

```json
{"kind": "DownloadComplete", "size": 1073741824, "duration": 12345678}
```

- `size` = bytes transferred
- `duration` = microseconds
- Derived: `elapsed_s = duration / 1e6`, `mbps = size * 8 / (elapsed_s * 1e6)`

### Connection type (iroh)

Scan the fetcher NDJSON log for `ConnectionTypeChanged` events:

```json
{"kind": "ConnectionTypeChanged", "status": "Selected", "addr": "Ip(...)"}
```

- Collect all events where `status == "Selected"`.
- The **last** such event determines `final_conn_direct`:
  - `addr` containing `"Ip"` → direct
  - anything else (relay URL) → relay
- Also capture `conn_upgrade` (ever went direct) and `conn_events` (total count).

### Output format

```json
{
  "sim": "iroh-1to1-public",
  "transfers": [
    {
      "id":               "xfer",
      "provider":         "provider",
      "fetcher":          "fetcher",
      "size_bytes":       1073741824,
      "elapsed_s":        12.345,
      "mbps":             695.4,
      "final_conn_direct": true,
      "conn_upgrade":     true,
      "conn_events":      2
    }
  ]
}
```

### `parser` field on generic `spawn` steps

```toml
[[step]]
action = "spawn"
id     = "get"
device = "fetcher"
cmd    = ["${binary.transfer}", "--output", "json", "fetch", "${srv.endpoint_id}"]
parser = "iroh_json"   # post-process log for DownloadComplete after step exits
```

Supported parsers for generic steps:

| `parser`    | Extracts                                 |
|-------------|------------------------------------------|
| `iroh_json` | `DownloadComplete` + `ConnectionTypeChanged` |
| `iperf`     | iperf throughput lines                   |
| none        | no post-processing                       |

For iroh integration, only `iroh_json` is required initially.

### `results.md` format

```
| sim | id | provider | fetcher | size_bytes | elapsed_s | mbps | final_conn_direct | conn_upgrade | conn_events |
| --- | -- | -------- | ------- | ---------- | --------- | ---- | ----------------- | ------------ | ----------- |
| iroh-1to1-public | xfer | provider | fetcher | 1073741824 | 12.345 | 695.4 | true | true | 2 |
```

---

## 10. Sim File Format

```toml
[sim]
name     = "iroh-switch-direct"
topology = "switch-direct"     # loads topos/switch-direct.toml

[[binary]]
name    = "transfer"
repo    = "https://github.com/n0-computer/iroh"
commit  = "main"
example = "transfer"

[[binary]]
name = "relay"
url  = "https://github.com/n0-computer/iroh/releases/download/v0.96.1/iroh-relay-x86_64-unknown-linux-musl.tar.gz"
```

Inline topology (no `topology` ref):

```toml
[sim]
name = "ping-basic"

[[router]]
name   = "dc-eu"
region = "eu"

[device.server.eth0]
gateway = "dc-eu"

[device.client.eth0]
gateway = "dc-eu"

[[step]]
action = "run"
device = "client"
cmd    = ["ping", "-c4", "$NETSIM_IP_server"]
```

When `topology` is set, router/device tables are loaded from a `topos/`
directory adjacent to the sim file (`../topos/<topology>.toml`). If not found,
fall back to `topos/<topology>.toml` at repo root. Router/device tables must
not appear inline when `topology` is set.

---

## 11. Binary Spec

### `[[binary]]` — named binaries

Each binary has a unique `name` used for substitution in `cmd` or in built-in
kinds like `iroh-transfer`. Three mutually exclusive sources:

```toml
[[binary]]
name    = "transfer"
repo    = "https://github.com/n0-computer/iroh"
commit  = "main"     # branch, tag, or full SHA
example = "transfer" # cargo --example <name>

[[binary]]
name = "relay"
url  = "https://github.com/n0-computer/iroh/releases/download/v0.35.0/iroh-relay-x86_64-unknown-linux-musl.tar.gz"

[[binary]]
name = "transfer"
path = "/usr/local/bin/iroh-transfer"
```

Build function (`src/sim/build.rs`) per binary:
- **git**: clone if no `.git`; `git fetch + checkout`; `cargo build --example … --release`.
  Skip build if binary mtime > source mtime.
- **url**: download + extract to `<work_dir>/bins/`; skip if already present.
- **path**: use as-is.

Builder must pass through all `RUST_*` env vars to the `cargo` invocation.
If `RUST_TARGET` is set, add `--target <RUST_TARGET>` so VM/cross builds work.

Binary substitution variable available in steps: `${binary.<name>}`.

---

## 12. Step Actions

| `action`       | Required fields                                | Optional                                  |
|----------------|------------------------------------------------|-------------------------------------------|
| `spawn`        | `id`, `device`, `cmd` **or** `kind`            | `ready_when`, `ready_after`, `captures`, `parser`, `env` |
| `run`          | `device`, `cmd`                                | `env`                                     |
| `wait`         | `duration`                                     | —                                         |
| `wait-for`     | `id`                                           | `timeout` (default: `"300s"`)             |
| `set-impair`   | `device`, `impair`                             | `interface` (default: `default_via`)      |
| `switch-route` | `device`, `to`                                 | —                                         |
| `link-down`    | `device`, `interface`                          | —                                         |
| `link-up`      | `device`, `interface`                          | —                                         |
| `assert`       | `check`                                        | `timeout`                                 |

`ready_after = "2s"` — static delay before the step is considered ready
(useful for relay nodes that don't emit a startup event).

`captures` — block on stdout until a regex matches, extract a named group:

```toml
captures = { addr = { stdout_regex = "READY (.+)" } }
```

Captured values are available as `${<id>.<name>}` in later steps.

**Capture→substitution ordering:** `${<id>.<capture>}` in a step's `cmd` is
interpolated at execution time.  The referenced `id` must have already run and
its `captures` resolved before the current step executes.  Steps execute
sequentially, so placing the dependent step after its source in TOML is
sufficient.

### `kind = "iroh-transfer"`

```toml
[[step]]
action     = "spawn"
kind       = "iroh-transfer"
id         = "xfer"
provider   = "provider"          # device name
fetcher    = "fetcher"           # single device  — or —
# fetchers = ["f-0", "f-1"]     # multiple devices (count-expanded; Phase 3)
relay_url  = "http://..."        # optional; passed as --relay-url to both sides
fetch_args = ["--verify"]        # optional extra args for fetcher(s)
strategy   = "endpoint_id_only"       # or "endpoint_id_with_direct_addrs"
```

`strategy = "endpoint_id_only"` uses only the provider `endpoint_id`.
`strategy = "endpoint_id_with_direct_addrs"` also uses the first `direct_addresses`
entry from the provider `EndpointBound` event (if present) and passes it as
`--remote-direct-address` to the fetcher. If no direct address is available,
fall back to `endpoint_id` only.

`kind = "iroh-transfer"` uses the binary named `transfer` from `[[binary]]`.
Fail fast if no such binary is configured.

#### Binary invocation

```
# Provider (inside provider's netns):
${binary.transfer} --output json --logs-path <log_dir>/xfer_provider.ndjson provide

# Fetcher (inside fetcher's netns):
${binary.transfer} --output json --logs-path <log_dir>/xfer_fetcher.ndjson \
         fetch <endpoint_id> [--relay-url <url>] [fetch_args…]

# If strategy == "endpoint_id_with_direct_addrs" and a direct address is available:
${binary.transfer} --output json --logs-path <log_dir>/xfer_fetcher.ndjson \
         fetch --remote-direct-address <addr> <endpoint_id> [--relay-url <url>] [fetch_args…]
```

`--output json` switches the binary to NDJSON output (used by `--logs-path`).
Without this flag the binary emits human-readable text that cannot be parsed.

#### Execution sequence

1. Start provider subprocess in provider's netns.
2. Stream provider stdout/stderr only for readiness/capture (not parsing).
3. Tail provider `--logs-path` file until `{"kind":"EndpointBound","endpoint_id":"…"}`:
   extract `endpoint_id`. Expose as `${xfer.endpoint_id}`.
4. If `strategy == "endpoint_id_with_direct_addrs"`, also collect `direct_addresses`
   from the same event; pick the first address for `--remote-direct-address`
   if present.
5. Start fetcher subprocess(es) in their netns(es) with `endpoint_id` and
   optional `--remote-direct-address`.
6. Tail fetcher `--logs-path` file until fetcher emits `EndpointBound`
   (confirms it started).
7. **Concurrently:**
   - Fetcher side: wait for fetcher process to exit naturally (exits after
     `DownloadComplete`).
   - Provider side: tail remaining NDJSON until `{"kind":"PathStats"}` —
     this is the provider's **end-of-connection** signal (emitted when the
     peer disconnects after the transfer).  Then SIGINT the provider and drain
     its remaining stdout/stderr.
8. Post-process `xfer_fetcher.ndjson`:
   - Extract `DownloadComplete` → size, duration → Mbps.
   - Collect `ConnectionTypeChanged` events → `final_conn_direct`,
     `conn_upgrade`, `conn_events`.
9. Write results to `results.json` and `results.md`.

Exposes for `assert`:
- `xfer.mbps`, `xfer.elapsed_s`, `xfer.size_bytes`
- `xfer.final_conn_direct` (bool), `xfer.conn_upgrade` (bool), `xfer.conn_events` (int)
- `xfer.endpoint_id` (string)

### Variable substitution in `cmd`

- `$NETSIM_IP_<device>` — default-via IP
- `$NETSIM_IP_<device>_<ifname>` — specific interface IP
- `$NETSIM_NS_<device>` — netns name
- `${binary.<name>}` — path to built/downloaded named binary
- `${data}` — sim-specific data directory (`<work_dir>/data/`)
- `${<id>.<capture>}` — value captured from a prior `spawn`

---

## 13. Example Sim Files

### `iroh-integration/sims/iroh-1to1-public.toml` — both public, no NAT

```toml
[sim]
name     = "iroh-1to1-public"
topology = "1to1-public"

[[binary]]
name = "transfer"
url  = "https://github.com/n0-computer/iroh/releases/download/v0.35.0/iroh-transfer-x86_64-unknown-linux-musl.tar.gz"

[[step]]
action   = "spawn"
kind     = "iroh-transfer"
id       = "xfer"
provider = "provider"
fetcher  = "fetcher"
fetch_args = ["--duration=20"]

[[step]]
action  = "wait-for"
id      = "xfer"

[[step]]
action = "assert"
check  = "xfer.final_conn_direct == true"
```

### `iroh-integration/sims/iroh-1to1-nat.toml` — both behind NAT + relay

```toml
[sim]
name     = "iroh-1to1-nat"
topology = "1to1-nat"

[[binary]]
name = "transfer"
url  = "..."

[[binary]]
name = "relay"
url  = "https://github.com/n0-computer/iroh/releases/download/v0.35.0/iroh-relay-x86_64-unknown-linux-musl.tar.gz"

[[step]]
action      = "spawn"
id          = "relay"
device      = "relay"
cmd         = ["${binary.relay}", "--dev"]
ready_after = "2s"

[[step]]
action    = "spawn"
kind      = "iroh-transfer"
id        = "xfer"
provider  = "provider"
fetcher   = "fetcher"
relay_url = "http://$NETSIM_IP_relay:3340"
fetch_args = ["--duration=20"]

[[step]]
action = "wait-for"
id     = "xfer"

[[step]]
action = "assert"
check  = "xfer.final_conn_direct == true"
```

### `iroh-integration/sims/iroh-switch-direct.toml` — mid-transfer route switch

```toml
[sim]
name     = "iroh-switch-direct"
topology = "switch-direct"

[[binary]]
name = "transfer"
url  = "..."

[[step]]
action   = "spawn"
kind     = "iroh-transfer"
id       = "xfer"
provider = "provider"
fetcher  = "fetcher"
fetch_args = ["--duration=20"]

[[step]]
action   = "wait"
duration = "10s"

[[step]]
action = "switch-route"
device = "fetcher"
to     = "eth0"        # switch from wifi (eth1) to mobile (eth0)

[[step]]
action  = "wait-for"
id      = "xfer"
timeout = "600s"

[[step]]
action = "assert"
check  = "xfer.final_conn_direct == true"
```

---

## 14. Topology Files

### `iroh-integration/topos/1to1-public.toml`

```toml
[[router]]
name = "dc"

[device.provider.eth0]
gateway = "dc"

[device.fetcher.eth0]
gateway = "dc"
```

### `iroh-integration/topos/1to1-nat.toml`

```toml
[[router]]
name = "dc"

[[router]]
name = "isp"
nat  = "cgnat"

[[router]]
name     = "lan-provider"
upstream = "isp"
nat      = "destination-independent"

[[router]]
name     = "lan-fetcher"
upstream = "isp"
nat      = "destination-dependent"

[device.relay.eth0]
gateway = "dc"

[device.provider.eth0]
gateway = "lan-provider"

[device.fetcher.eth0]
gateway = "lan-fetcher"
```

### `iroh-integration/topos/1to10-public.toml`

```toml
[[router]]
name = "dc"

[device.provider.eth0]
gateway = "dc"

[device.fetcher]
count = 10

[device.fetcher.eth0]
gateway = "dc"
```

### `iroh-integration/topos/switch-direct.toml`

```toml
[[router]]
name = "dc"

[[router]]
name = "isp"
nat  = "cgnat"

[[router]]
name     = "lan"
upstream = "isp"
nat      = "destination-independent"

[device.provider.eth0]
gateway = "dc"

[device.fetcher]
default_via = "eth1"

[device.fetcher.eth0]
gateway = "isp"
impair  = "mobile"

[device.fetcher.eth1]
gateway = "lan"
impair  = "wifi"
```

---

## 15. Module Layout

```
src/
  lib.rs          — Lab, DeviceBuilder, add_router, TOML config parsing
  core.rs         — LabCore, Device/DeviceIface, Netlink, build
  qdisc.rs        — tc helpers; add remove_qdisc_r
  sim/
    mod.rs        — SimConfig, run_sim
    topology.rs   — TopoConfig (sim-relative topos/ or inline)
    build.rs      — build_or_fetch_binary (git / url / path)
    transfer.rs   — iroh-transfer kind (ported from resources/dogfood/)
    runner.rs     — step executor
    env.rs        — env-var injection + ${} interpolation
    report.rs     — result parsing (DownloadComplete, ConnectionTypeChanged) + results.json/results.md
  main.rs         — CLI entry point

iroh-integration/
  sims/
    iroh-1to1-public.toml
    iroh-1to1-nat.toml
    iroh-1to10-public.toml
    iroh-switch-direct.toml
  topos/
    1to1-public.toml
    1to1-nat.toml
    1to10-public.toml
    switch-direct.toml
```

---

## 16. CLI

```
cargo run -- iroh-integration/sims/iroh-1to1-public.toml [--work-dir .netsim-work] [--set key=value …] [--binary.transfer=.]
```

```rust
#[derive(Parser)]
struct Cli {
    sim:       PathBuf,
    #[arg(long, default_value = ".netsim-work")]
    work_dir:  PathBuf,
    #[arg(long = "set", value_parser = parse_kv)]
    overrides: Vec<(String, String)>,
    #[arg(long = "binary.transfer")]
    binary_transfer: Option<String>,
}
```

`--set` applies dotted-key overrides to any scalar in the sim config.
`--binary.transfer=.` builds `transfer` from the current checkout in release mode
(honoring `RUST_TARGET`, e.g. MUSL in VM runs).

---

## 17. Cargo.toml additions

```toml
[dependencies]
clap        = { version = "4", features = ["derive"] }
serde_json  = "1"
tokio       = { version = "1", features = ["full"] }
regex       = "1"
reqwest     = { version = "0.12", features = ["blocking"] }   # binary URL download
flate2      = "1"                                              # .tar.gz extraction
tar         = "0.4"
# iroh = "0.35"   # add when implementing the transfer kind (for EndpointId type)
```

---

## 18. Implementation Order

### Phase 1 — Core types + builder (all existing tests stay green)

1. **`core.rs`**: `Device` → multi-iface version; add `DeviceIface`; add
   `router_downlink_gw_for_switch`, `set_device_default_via`.
2. **`lib.rs`**: `DeviceBuilder`; remove `add_isp`/`add_dc`/`add_home`/`Gateway`;
   add `add_router`; update all tests.
3. **`core.rs` build**: `DevBuild` → `IfaceBuild`; `wire_device` → `wire_iface`;
   loop over all interfaces; only add default route for `default_via`.
4. **`lib.rs` TOML config**: parse `[[router]]` + `[device.name.ethN]`; remove
   old section parsing; update `load_from_toml` test.

### Phase 2 — Dynamic ops

5. **`qdisc.rs`**: add `remove_qdisc_r`.
6. **`lib.rs`**: `Lab::set_impair`, `Lab::link_down`, `Lab::link_up`,
   `Lab::switch_route` (all `&mut self`).
7. **Tests**: `set_impair` (RTT changes), `link_down/up` (connectivity),
   `switch_route` (RTT change after switch).

### Phase 3 — Sim runner

8. **`sim/topology.rs`**: `TopoConfig` — reuse parsing logic from Phase 1 step 4.
9. **`sim/env.rs`**: env var map from lab state; `${}` interpolation.
10. **`sim/build.rs`**: `build_or_fetch_binary` — git / URL download / path.
    URL download: fetch, detect tar.gz vs bare binary, extract to `<work_dir>/bins/`.
    Honor `RUST_*` env vars and `RUST_TARGET` for cargo builds.
11. **`sim/transfer.rs`**: port `TransferCommand` / `LogReader` from
    `resources/dogfood/`; adapt to run in namespaces via `spawn_in_netns`;
    always pass `--logs-path` and support `strategy = "endpoint_id_with_direct_addrs"`.
12. **`sim/report.rs`**: `parse_iroh_log(path)` → `TransferResult`;
    `write_results_json(work_dir, results)`;
    `write_results_md(work_dir, results)`.
13. **`sim/runner.rs`**: step executor — all actions; `wait-for` default 300 s;
    `assert` evaluates simple `key == value` / `key != value` expressions.
14. **`src/main.rs`**: `Cli` + wire everything.
15. Write `iroh-integration/topos/*.toml` and `iroh-integration/sims/*.toml`.
16. End-to-end: `cargo make run-vm -- iroh-integration/sims/iroh-1to1-public.toml`.

### Phase 4 — `count` expansion

17. **`lib.rs` / `topology.rs`**: expand `count = N` into N devices;
    update env var naming (`$NETSIM_IP_fetcher_0`, etc.).
18. **`runner.rs`**: handle `fetchers = [...]` in `kind = "iroh-transfer"`;
    aggregate per-fetcher results in `results.json` and `results.md`.
19. Write `iroh-integration/sims/iroh-1toN.toml` family.

### Phase 5 — Shared binary manifests + generic binary overrides

20. **Shared binary file support**:
    - Add `[sim] binaries = "iroh-defaults.toml"` (parallel to `topology`).
    - Load shared binary entries from `../topos/`-style adjacent lookup (and repo-root fallback).
    - Keep inline `[[binary]]` support.
    - Merge rule: shared first, then inline overrides by `name`.

21. **CLI generic binary override**:
    - Add repeatable `--binary "<name>:<mode>:<value>"`.
    - Supported modes:
      - `build` (`transfer:build:.`) → build from local checkout path.
      - `fetch` (`relay:fetch:https://...`) → force URL download source.
      - `path` (`transfer:path:./bins/transfer`) → copy into workdir and use copied path.
    - Override precedence: CLI override > inline `[[binary]]` > shared binaries file.

22. **Path-copy semantics for `mode=path`**:
    - Resolve host path (relative to invocation cwd).
    - Copy to `<work_dir>/bins/<name>-override`.
    - `chmod +x` target (unix) and execute copied path for run stability/reproducibility.

23. **Validation + UX**:
    - Validate override syntax and duplicate conflicting overrides early.
    - Error messages include exact bad override and accepted format.
    - Emit resolved binary source map at startup (name + final mode/path).

24. **Tests + examples**:
    - Unit tests for parse/merge/precedence.
    - Add `iroh-integration/iroh-defaults.toml`.
    - Update sim examples to consume shared binaries file while still demonstrating inline overrides.

---

## 19. Resolved Questions

- **Single-interface shorthand**: always require explicit `[device.name.eth0]`.
  Parser stays uniform; no special-casing.
- **`switch_route` mutability**: `&mut self` throughout.
- **`alloc_global_ipv4`**: inferred from `nat`; no explicit TOML field in v1.
- **`NatMode::None` / `NatMode::Cgnat`**: add both variants.
- **`count`**: moved to Phase 4, not dropped.
- **`wait-for` timeout**: default 300 s; override with `timeout = "600s"`.
- **`PathStats` semantics**: provider end-of-connection signal; not a transfer
  metric; not readiness.  Provider streams until `PathStats`, then gets SIGINT.
- **`connected_via`**: from `ConnectionTypeChanged` events in fetcher log only.
