# Plan: initial

## Goals
1. Fix all compile errors
2. Clean up API / improve type safety
3. Parse `lab.toml` → `Lab::load("lab.toml")`
4. End-state API: `lab.run_on("home-eu2", Command::new("ping").arg("8.8.8.8"))`

---

## 1. Fix Compile Errors

### 1a. Remove dead `InfoKind` import (lib.rs:5)
The `rtnl` sub-module was removed in `netlink-packet-route` 0.19; the import is also
unused — just delete the line.

### 1b. Add `user` feature to nix (Cargo.toml)
`nix::unistd::Uid` is gated behind the `user` feature in nix 0.28.
```toml
nix = { version = "0.28", features = ["sched", "mount", "fs", "signal", "process", "user"] }
```

### 1c. Fix `[u8]` Display at line 380
The route-building code collects octets into a `Vec<u8>` and tries to format it as a
string. Instead, build an `Ipv4Addr` directly:
```rust
let oct = dc.lan_gw.octets();
let net = Ipv4Addr::new(oct[0], oct[1], oct[2], 0);
add_route_v4(h, &format!("{}/24", net), dc.ix_ip).await.ok();
```

### 1d. Fix `setns(x.as_raw_fd(), ...)` — 8 occurrences
`nix::sched::setns` now takes `impl AsFd`; `i32` does not implement it but `&File`
does. Remove the `.as_raw_fd()` calls and pass the `File` reference directly:
```rust
// before
setns(ns_fd.as_raw_fd(), CloneFlags::CLONE_NEWNET)?;
// after
setns(&ns_fd, CloneFlags::CLONE_NEWNET)?;
```
`setns_by_fd` in rtnetlink still takes `RawFd` — that call (line 659) stays unchanged.

### 1e. Remove unused imports
Delete `RawFd` and `ffi::OsStr` from the `use` block (warned by compiler).

---

## 2. API Cleanup & Type Safety

### 2a. Make `Lab::new()` synchronous
No async work is done inside it. Change signature to `fn new() -> Self`.

### 2b. Name-indexed lookup maps
Add per-type name→id maps to `Lab` so entities can be retrieved by the string names
used in `lab.toml` and the user-facing API:
```rust
isp_by_name:    HashMap<String, IspId>,
dc_by_name:     HashMap<String, DcId>,
lan_by_name:    HashMap<String, HomeId>,   // "lan" == "home" internally
device_by_name: HashMap<String, DeviceId>,
```
Populate them in each `add_*` method alongside the existing id-keyed maps.

### 2c. Replace `run_in` with `run_on`
Current `run_in(DeviceId, &str, &[&str])` is awkward. New API:
```rust
pub fn run_on(&self, name: &str, cmd: std::process::Command) -> Result<std::process::ExitStatus>
```
Internally: fork → setns into device ns → `cmd.spawn()?.wait()`. Accepts any
fully-configured `Command`, so callers write:
```rust
lab.run_on("home-eu2", Command::new("ping").arg("-c1").arg("1.1.1.1"))?;
```

Add a background variant for long-running processes (replaces current reflector helpers):
```rust
pub fn spawn_on(&mut self, name: &str, cmd: std::process::Command) -> Result<Pid>
```

### 2d. Typed `Gateway` enum on `Device`
`lab.toml` devices can be attached to a LAN, a DC, or directly to an ISP. Replace
`home: HomeId` on `Device` with:
```rust
enum Gateway { Lan(HomeId), Dc(DcId), Isp(IspId) }
struct Device { ns: Ns, gateway: Gateway, ip: Ipv4Addr, impair: Impair }
```
`add_device` becomes `add_device(name, gateway: Gateway)`.

---

## 3. New Model Types

```rust
enum Impair { None, Wifi, Mobile }

struct ImpairDownstream { latency_ms: u32 }
// stored on Isp; applied via tc netem on the ISP↔home veth (ISP side)

struct RegionLatency { from: String, to: String, ms: u32 }
// stored on Lab; applied via tc netem on IX-facing veths after build
```

`Impair` profiles (applied via `tc qdisc add dev <if> root netem`):
- `Wifi`   → `delay 20ms 5ms`
- `Mobile` → `delay 50ms 20ms loss 1%`

---

## 4. TOML Config Structs + `Lab::load`

### 4a. New Cargo.toml dependencies
```toml
serde = { version = "1", features = ["derive"] }
toml  = "0.8"
```

### 4b. Config structs (inside lib.rs, `mod config` block)
Mirror the TOML structure with serde:
```rust
#[derive(Deserialize)]
struct LabConfig {
    region:  HashMap<String, RegionConfig>,
    isp:     Vec<IspConfig>,
    dc:      Vec<DcConfig>,
    lan:     Vec<LanConfig>,
    device:  Vec<DeviceConfig>,
}

#[derive(Deserialize)]
struct RegionConfig {
    latencies: HashMap<String, u32>,  // target_region -> ms
}

#[derive(Deserialize)]
struct IspConfig {
    name:               String,
    region:             String,
    nat:                Option<String>,              // "cgnat"
    impair_downstream:  Option<ImpairDownstreamCfg>,
}

#[derive(Deserialize)]
struct ImpairDownstreamCfg { latency: u32 }

#[derive(Deserialize)]
struct DcConfig  { name: String, region: String }

#[derive(Deserialize)]
struct LanConfig {
    name:   String,
    isp:    String,           // ISP name ref
    nat:    String,           // "destination-independent" | "destination-dependent"
}

#[derive(Deserialize)]
struct DeviceConfig {
    name:    String,
    gateway: String,          // name of a lan, dc, or isp entry
    impair:  Option<String>,  // "wifi" | "mobile"
}
```

### 4c. `Lab::load(path: &str) -> Result<Lab>`
```
parse file → LabConfig
for each isp  → lab.add_isp(...)
for each dc   → lab.add_dc(...)
for each lan  → lab.add_home(...)   // resolve isp name → IspId
for each device → resolve gateway name across dc_by_name / lan_by_name / isp_by_name
                  → lab.add_device(name, Gateway::..., impair)
store region latency pairs for use during build()
return lab  (caller still calls lab.build().await)
```

String → enum conversions with clear errors:
- `"cgnat"` → `IspMode::Cgnat { pool_cidr: "100.64.0.0/10" }`
- `"destination-independent"` → `NatMode::DestinationIndependent`
- `"destination-dependent"`   → `NatMode::DestinationDependent`
- `"wifi"` → `Impair::Wifi`, `"mobile"` → `Impair::Mobile`

---

## 5. Build Changes for New Device Types

### 5a. DC devices (relays/servers)
When `gateway = Gateway::Dc(dc_id)`:
- Create a veth pair: one end in DC ns (`dc-dev<N>`), one end in device ns (`eth0`)
- Assign device IP from DC LAN subnet (`dc.lan_gw` /24, host `.10+N`)
- Default route via `dc.lan_gw`
- No NAT needed (DC devices are already "public")

### 5b. ISP-direct devices (mobile, e.g. `phone-eu1`)
When `gateway = Gateway::Isp(isp_id)`:
- Veth pair: one end in ISP ns, one end in device ns
- Assign device IP from a subscriber range per ISP (e.g. `10.100.<isp_id>.<N>/24`)
- Default route via ISP-side veth IP
- ISP applies CGNAT (if enabled) on the outbound interface as normal

### 5c. Impair via tc netem
After link setup, if device has `impair != None`, apply in device ns:
```
tc qdisc add dev eth0 root netem delay 20ms 5ms          # wifi
tc qdisc add dev eth0 root netem delay 50ms 20ms loss 1% # mobile
```
For `impair_downstream` on ISPs: apply netem on the ISP-side veth toward home.

### 5d. Region latency via tc netem
After all links built, for each region-pair with a configured latency, find the veths
on the IX bridge that belong to ISPs/DCs in those regions and apply half the latency
on each side (split delay).

---

## 6. File Structure (stays 2-file)

`src/lib.rs` sections:
1. Public types: `IspMode`, `NatMode`, `Gateway`, `Impair`, `IspId`, `DcId`, `HomeId`, `DeviceId`
2. Internal structs: `Ns`, `Isp`, `Dc`, `Home`, `Device`
3. `Lab` struct + impl (`new`, `load`, `add_*`, `build`, `run_on`, `spawn_on`, `isp_public_ip`)
4. `mod config` — serde structs, `LabConfig`, `parse_lab_config`
5. Netns helpers (`create_named_netns`, `with_netns`, `run_in_netns`, `set_sysctl_*`)
6. rtnetlink helpers (`add_veth`, `add_bridge`, `add_addr4`, `add_route_v4`, …)
7. nft helpers (`apply_home_nat`, `apply_isp_cgnat`, `run_nft_in`)
8. STUN reflector/probe (`spawn_reflector`, `probe_in_ns`)
9. `#[cfg(test)] mod tests`

`Cargo.toml`: add `serde`, `toml`, nix `user` feature.

---

## Implementation Order

1. Fix compile errors (§1) — get it building
2. Add serde+toml deps, write config structs + `Lab::load` (§4)
3. Add name maps + `run_on` / `spawn_on` (§2)
4. Add `Gateway` enum, update `add_device` + build logic for DC/ISP devices (§3, §5)
5. Impair via tc netem (§5c, §5d)
