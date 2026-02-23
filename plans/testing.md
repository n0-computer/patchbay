# Comprehensive Networking Test Suite

## TODO

- [x] Write plan
- [ ] Add `strum` dev-dependency; derive `EnumIter`/`Display` on `NatMode`, `UplinkWiring`
- [ ] New public APIs: `set_nat_mode`, `rebind_nats`, `set_hairpin`, `set_router_impair`, `router_downlink_bridge`
- [ ] Supporting test helpers: `NatTestCtx`, `DualNatLab`, `probe_reflexive`, TCP reflector/sink, `udp_send_recv_count`
- [ ] Tests 5a–5b: TCP reflector smoke + reflexive IP all combos (48 combinations)
- [ ] Tests 5c: port mapping behavior (EIM stable, EDM changes)
- [ ] Tests 5d: route switching + reflexive IP/TCP behavior (multiple switch scenarios)
- [ ] Tests 5e: link down/up connectivity (UDP + TCP)
- [ ] Tests 5f: NAT rebinding (mode change + conntrack flush)
- [ ] Tests 5g: multi-device cross-NAT isolation
- [ ] Tests 5h: hairpinning (off/on/toggle)
- [ ] Tests 5i: rate limiting (TCP/UDP, upload/download, multi-hop, asymmetric)
- [ ] Tests 5j: packet loss (UDP moderate/high, TCP integrity)
- [ ] Tests 5k: latency multi-hop, accumulation, directionality, regions
- [ ] Tests 5l: dynamic rate and latency changes
- [ ] Final review

## Context

The current test suite in `src/lib.rs` covers basic NAT reflexive-IP, port mapping, link/route switching,
and latency — but leaves gaps:

- No TCP reflexive-address verification
- No binding to specific device IP (vs `0.0.0.0`)
- No reflexive IP check *after* `switch_route`
- No NAT rebinding or conntrack flush at runtime
- Combinations of NAT mode × protocol × bind mode not crossed
- No hairpinning support or tests
- Rate limiting (TBF) never verified against actual throughput
- Packet loss never counted/verified
- Download direction (impair on router side) untested
- Multi-hop latency accumulation (device impair + region latency) not crossed
- Asymmetric up/down rates, dynamic rate changes untested

The new suite covers everything existing tests cover and adds these dimensions systematically.
All tests run rootless (no `check_caps()`). Tests drop `#[serial]` — each `Lab` creates unique
namespaces via the existing prefix scheme, so labs are fully isolated and can run in parallel.

---

## 1. Testing API Style — Option B with strum

Add `strum` as a dev-dependency. Derive `EnumIter` + `Display` on all test dimension enums.
Assemble the full cross-product into a `Vec`, run every combination, collect all failures,
then panic once with the full failure list.

```rust
// Add to Cargo.toml [dev-dependencies]
// strum = { version = "0.26", features = ["derive"] }

// NatMode (in lib.rs) and UplinkWiring (test-local) get strum derives added.
// New test-local enums:
#[derive(Debug, Clone, Copy, strum::EnumIter, strum::Display)]
enum Proto    { Udp, Tcp }

#[derive(Debug, Clone, Copy, strum::EnumIter, strum::Display)]
enum BindMode { Unspecified, SpecificIp }
```

Pattern used in every matrix test:

```rust
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn reflexive_ip_all_combos() -> Result<()> {
    use strum::IntoEnumIterator;
    let combos: Vec<_> = NatMode::iter()
        .flat_map(|m| UplinkWiring::iter().map(move |w| (m, w)))
        .flat_map(|(m, w)| Proto::iter().map(move |p| (m, w, p)))
        .flat_map(|(m, w, p)| BindMode::iter().map(move |b| (m, w, p, b)))
        .collect();

    let mut port_base = 20_000u16;
    let mut failures = Vec::new();
    for (mode, wiring, proto, bind) in combos {
        let result: Result<()> = async {
            let (lab, ctx) = build_nat_case(mode, wiring, port_base).await?;
            let obs = probe_reflexive(proto, bind, &ctx)?;
            if obs.ip() != IpAddr::V4(ctx.expected_ip) {
                bail!("expected {} got {}", ctx.expected_ip, obs.ip());
            }
            Ok(())
        }.await;
        if let Err(e) = result {
            eprintln!("FAIL {mode}/{wiring}/{proto}/{bind}: {e:#}");
            failures.push(format!("{mode}/{wiring}/{proto}/{bind}: {e:#}"));
        }
        port_base += 10;
    }
    if !failures.is_empty() {
        bail!("{} combos failed:\n{}", failures.len(), failures.join("\n"));
    }
    Ok(())
}
```

Same collect-all-failures pattern is used in `port_mapping_*`, `nat_rebind_*`, etc.

---

## 2. Supporting Types

### Enums (test-local, `strum` derives)

```rust
#[derive(Debug, Clone, Copy, strum::EnumIter, strum::Display)]
enum Proto    { Udp, Tcp }

#[derive(Debug, Clone, Copy, strum::EnumIter, strum::Display)]
enum BindMode { Unspecified, SpecificIp }
```

Also add `#[derive(strum::EnumIter, strum::Display)]` to the existing `NatMode` and `UplinkWiring`.

### `NatTestCtx` — named context struct

```rust
struct NatTestCtx {
    dev_ns:      String,
    dev_ip:      Ipv4Addr,
    expected_ip: Ipv4Addr,
    r_dc:        SocketAddr,   // reflector in DC namespace
    r_ix:        SocketAddr,   // reflector on IX bridge
}
```

`build_nat_case(mode, wiring, port_base) -> Result<(Lab, NatTestCtx)>` replaces the existing 5-tuple.

### `DualNatLab` — named return for dual-router topology

```rust
struct DualNatLab {
    lab:       Lab,
    dev:       NodeId,
    nat_a:     NodeId,
    nat_b:     NodeId,
    reflector: SocketAddr,   // UDP+TCP in DC
}
```

`build_dual_nat_lab(mode_a, mode_b, port_base) -> Result<DualNatLab>` — DC router on IX,
two NAT routers, device with `eth0→nat_a` (default) and `eth1→nat_b`, reflectors spawned.

### `probe_reflexive` — unified dispatch

```rust
fn probe_reflexive(proto: Proto, bind: BindMode, ctx: &NatTestCtx) -> Result<ObservedAddr> {
    probe_reflexive_addr(proto, bind, &ctx.dev_ns, ctx.dev_ip, ctx.r_dc)
}

fn probe_reflexive_addr(
    proto: Proto, bind: BindMode, ns: &str, dev_ip: Ipv4Addr, reflector: SocketAddr,
) -> Result<ObservedAddr> {
    let bind_addr = match bind {
        BindMode::Unspecified => "0.0.0.0:0".parse().unwrap(),
        BindMode::SpecificIp  => SocketAddr::new(IpAddr::V4(dev_ip), 0),
    };
    match proto {
        Proto::Udp => probe_udp(ns, reflector, bind_addr),
        Proto::Tcp => probe_tcp(ns, reflector, bind_addr),
    }
}
```

---

## 3. New Test Helpers (inside `mod tests`)

### Throughput and loss helpers

```rust
/// Starts a TCP sink in `server_ns` at `addr` (reads until EOF, ignores bytes).
/// Returns a join handle; the server exits after one connection.
fn spawn_tcp_sink(server_ns: &str, addr: SocketAddr) -> thread::JoinHandle<Result<()>> { ... }

/// Connects TCP from `client_ns`, writes `bytes` bytes, shuts down write side,
/// waits for server to close.  Returns elapsed time and computed throughput in kbit/s.
fn tcp_measure_throughput(client_ns: &str, server_addr: SocketAddr, bytes: usize)
    -> Result<(Duration, u32)> { ... }

/// Sends `total` UDP datagrams of `payload` bytes from `ns` to `target` (which echoes them).
/// Collects echoes for up to `wait` after the last send.
/// Returns (sent, received) counts — received/sent ratio indicates effective delivery rate.
fn udp_send_recv_count(
    ns: &str, target: SocketAddr, total: usize, payload: usize, wait: Duration,
) -> Result<(usize, usize)> { ... }
```

**Direction convention** (matches qdisc egress semantics):
- **Upload**: impair on `dev`'s interface → limits egress from device → use `set_impair`
- **Download**: impair on router's downlink bridge → limits egress toward all devices → use `set_router_impair(dc, downlink_bridge, ...)`

```rust
/// UDP reflector: same as existing spawn_reflector_in but renamed.
fn spawn_udp_reflector(ns: &str, bind: SocketAddr) -> (TaskHandle, thread::JoinHandle<Result<()>>) { ... }

/// TCP server loop: accept → write "OBSERVED {peer}" → close → repeat.
fn spawn_tcp_reflector(ns: &str, bind: SocketAddr) -> (TaskHandle, thread::JoinHandle<Result<()>>) { ... }

/// UDP probe from `ns` with explicit bind address.
fn probe_udp(ns: &str, reflector: SocketAddr, bind: SocketAddr) -> Result<ObservedAddr> { ... }

/// TCP connect from `ns` bound to `bind`, reads "OBSERVED {addr}".
fn probe_tcp(ns: &str, target: SocketAddr, bind: SocketAddr) -> Result<ObservedAddr> { ... }
```

Non-test `probe_in_ns_from` is added to `lib.rs` (public) to support bind-to-specific-IP:

```rust
/// Like probe_in_ns but with an explicit bind address.
pub fn probe_in_ns_from(ns: &str, reflector: SocketAddr, bind: SocketAddr, timeout: Duration)
    -> Result<ObservedAddr> { ... }
```

---

## 4. New Public APIs

### `Lab::set_nat_mode` — runtime NAT rule replacement

**Why**: NAT rules applied once in `LabCore::build()`, never updated.

`src/core.rs`:
```rust
/// Returns `(ns, downlink_bridge_name, upstream_ip)` for a built router.
pub fn router_nat_params(&self, id: RouterId) -> Result<(String, String, Ipv4Addr)> { ... }

/// Stores an updated NAT mode on the router record.
pub fn set_router_nat_mode(&mut self, id: RouterId, mode: NatMode) -> Result<()> { ... }
```

`src/lib.rs`:
```rust
/// Replaces NAT rules on `router` with `mode` at runtime.
/// Flushes `table ip nat` then re-applies via `apply_nat`.
/// WAN interface name is always `"wan"` (assigned during build).
pub async fn set_nat_mode(&mut self, router: NodeId, mode: NatMode) -> Result<()> {
    let (ns, lan_if, wan_ip) = self.core.router_nat_params(router)?;
    run_nft_in(&ns, "flush table ip nat\n").await.ok();
    apply_nat(&ns, mode, &lan_if, "wan", wan_ip).await?;
    self.core.set_router_nat_mode(router, mode)
}
```

### `Lab::rebind_nats` — flush conntrack to reset all port mappings

Flushes the kernel conntrack table inside the router namespace. All active flows lose their
NAT state; the next packet from each flow gets a fresh port assignment.

```rust
/// Flushes the conntrack table for `router`, forcing all active NAT mappings to expire.
/// Subsequent flows get new external port assignments.
pub fn rebind_nats(&mut self, router: NodeId) -> Result<()> {
    let ns = self.core.router_ns(router)?.to_string();
    run_closure_in_namespace(&ns, || {
        let st = std::process::Command::new("conntrack").arg("-F").status()?;
        if !st.success() { bail!("conntrack -F failed: {st}"); }
        Ok(())
    })
}
```

Requires `conntrack-tools` (already a rootless-networking dependency per `plans/no-sudo.md`).

### `Lab::set_router_impair` — apply qdisc on a router interface

Mirrors `set_impair` for devices. Needed for download-direction rate limiting and multi-hop
latency injection (e.g., impair the DC router's downlink bridge to throttle traffic toward devices).

```rust
/// Applies or removes impairment on a named interface of `router`.
/// Use `router_downlink_bridge(router)` to get the LAN-facing bridge name.
pub fn set_router_impair(&mut self, router: NodeId, ifname: &str, impair: Option<Impair>) -> Result<()> {
    let ns = self.core.router_ns(router)?.to_string();
    match impair {
        Some(imp) => apply_impair_in(&ns, ifname, imp),
        None      => qdisc::remove_qdisc(&ns, ifname),
    }
    Ok(())
}

/// Returns the bridge interface name used for the router's downstream LAN.
pub fn router_downlink_bridge(&self, router: NodeId) -> Result<String> { ... }
```

The `downlink_bridge` is already stored in `router.cfg.downlink_bridge`; this just exposes it.

---

### `Lab::set_hairpin` — NAT loopback (hairpinning)

Allows devices behind the same NAT to reach each other via the router's public WAN IP.

Without hairpin: device A cannot reach device B using B's reflected (WAN) address — the packet
hits the router but is not redirected back into the LAN.

With hairpin: a POSTROUTING masquerade rule for intra-LAN traffic + a PREROUTING DNAT rule
for packets from LAN destined to the WAN IP redirect them back internally.

```rust
/// Enables or disables NAT hairpinning on `router`.
pub async fn set_hairpin(&mut self, router: NodeId, enabled: bool) -> Result<()> {
    // Flush and re-apply NAT rules with hairpin rule included/excluded.
    let (ns, lan_if, wan_ip) = self.core.router_nat_params(router)?;
    let mode = self.core.router_nat_mode(router)?;
    run_nft_in(&ns, "flush table ip nat\n").await.ok();
    apply_nat(&ns, mode, &lan_if, "wan", wan_ip).await?;
    if enabled {
        apply_hairpin(&ns, &lan_if, "wan", wan_ip).await?;
    }
    self.core.set_router_hairpin(router, enabled)
}
```

`apply_hairpin` adds nftables rules:
```
# prerouting: redirect WAN-IP-destined packets from LAN back into LAN
# postrouting: masquerade hairpin return traffic
```

`RouterConfig` gains `hairpin: bool` field (default `false`).

---

## 5. Test List

All use `#[tokio::test(flavor = "current_thread")] #[traced_test]` (no `#[serial]`).

### 5a. TCP reflector smoke
```
tcp_reflector_basic          — spawn TCP reflector, connect from same ns, verify "OBSERVED" reply
```

### 5b. Reflexive IP — full matrix (8 NAT×wiring × 2 proto × 2 bind = 48 combos)
```
reflexive_ip_all_combos      — NatMode::iter() × UplinkWiring::iter() × Proto::iter() × BindMode::iter()
                               collect all, run all, report all failures
```

### 5c. Port mapping behavior
```
port_mapping_eim_stable      — DestIndep × all wirings: probe r_dc and r_ix → same external port
port_mapping_edm_changes     — DestDep × all wirings: probe r_dc and r_ix → different external port
                               (both use collect-all-failures pattern over wiring variants)
```

### 5d. Route switching + reflexive IP and TCP behavior
```
switch_route_reflexive_ip    — DualNatLab(DestIndep, DestDep); for proto × bind:
                               probe → expect nat_a WAN; switch_route("eth1") → probe → expect nat_b WAN;
                               collect failures
switch_route_multiple        — A→B→A: reflexive IP tracks each switch, both protos
switch_route_tcp_roundtrip   — TCP roundtrip works after switch_route
switch_route_tcp_conn_reset  — establish TCP conn on eth0; switch_route to eth1;
                               existing conn errors or resets; new conn on eth1 succeeds
switch_route_udp_reflexive_change — UDP reflexive addr observed before switch ≠ after switch
                                    (verifies the new path gives a different external IP)
```

### 5e. Link down/up
```
link_down_up_connectivity    — for proto in [Udp, Tcp]: connectivity ok → link_down → fails →
                               link_up → works again; collect failures
```

### 5f. NAT rebinding
```
nat_rebind_mode_port         — pairs: [(DestIndep→DestDep, port_ne), (DestDep→DestIndep, port_eq)]
                               build, probe initial, set_nat_mode, probe again; collect failures
nat_rebind_mode_ip           — pairs: [(None→DestIndep, ip→WAN), (DestIndep→None, ip→device)]
                               probe before and after set_nat_mode; collect failures
nat_rebind_conntrack_flush   — DestIndep router: probe → record external port P1;
                               rebind_nats → probe again → port P2; assert P1 ≠ P2
```

### 5g. Multi-device cross-NAT
```
devices_same_nat_share_ip    — two devices, same router → same observed IP
devices_diff_nat_isolate     — two NAT routers; device on each → different IPs;
                               cross-ping to private IPs fails; public IPs reachable
```

### 5h. Intra-NAT communication (hairpinning)
```
same_nat_private_comm        — two devices behind same NAT can ping and TCP-connect via private IPs
same_nat_public_hairpin_off  — device A probes → gets reflected addr; device B tries to reach A
                               via A's reflected addr; fails (hairpin off by default)
same_nat_public_hairpin_on   — set_hairpin(router, true); same B→A via reflected addr; succeeds
hairpin_toggle               — enable hairpin → works; set_hairpin(false) → fails again
```

### 5i. Rate limiting — TCP + UDP, upload + download, multi-hop

All throughput assertions use `±30%` tolerance: `rate × 0.7 ≤ measured ≤ rate × 1.5`.
Rate is set low enough (2 Mbit/s) that the test completes in under 3 seconds (256 KB at 2 Mbit/s ≈ 1s).

```
rate_limit_tcp_upload        — Manual(rate=2000, latency=0, loss=0) on device eth0;
                               tcp_measure_throughput device→DC;
                               assert 1400 ≤ kbit/s ≤ 3000

rate_limit_tcp_download      — set_router_impair(dc, downlink_bridge, rate=2000);
                               tcp_measure_throughput DC→device (server on device, client in DC ns);
                               assert 1400 ≤ kbit/s ≤ 3000

rate_limit_udp_upload        — Manual(rate=2000) on device; udp_send_recv_count with ~300KB total;
                               measure elapsed ≥ 1.0s (300KB at 2 Mbit/s ≈ 1.2s)

rate_limit_udp_download      — set_router_impair(dc, downlink_bridge, rate=2000);
                               same from DC→device direction

rate_limit_asymmetric        — upload=1000kbit on device, download=4000kbit on dc;
                               measure both directions; assert upload ≤ 1500, download ≥ 2000

rate_limit_multihop_bottleneck — topology: device → NAT(wan Manual rate=1000) → ISP → DC;
                                  tcp_measure_throughput device→DC;
                                  assert kbit/s ≤ 1500 (NAT is the bottleneck regardless of upstream)

rate_limit_two_hops_stack    — device(rate=2000) AND dc_downlink(rate=2000);
                               effective throughput ≤ min(2000) × 1.5 = 3000 (neither link is free)
```

### 5j. Packet loss

```
loss_udp_moderate            — Manual(rate=0, latency=0, loss=50%) on device;
                               udp_send_recv_count(100 pkts); assert received in [30, 70]

loss_udp_high                — Manual(loss=90%); assert received ≤ 25

loss_tcp_integrity           — Manual(loss=5%) on device; tcp transfer 200KB;
                               verify all bytes received correctly (TCP retransmits mask loss)

loss_udp_both_directions     — loss on device (upload) and on dc downlink (download);
                               udp round-trip counts; both directions show loss
```

### 5k. Latency — multi-hop, accumulation, directionality, regions

Existing tests verify single-hop latency. These cross dimensions.

```
latency_device_plus_region   — device Manual(latency=30ms) + region eu→us 40ms;
                               UDP RTT device(eu)→DC(us) ≥ 2*(30+40) = 140ms;
                               device(eu)→DC(eu) ≥ 2*30 = 60ms (region skipped for same region)

latency_download_direction   — set_router_impair(dc, downlink_bridge, Manual latency=50ms);
                               device has NO impair; UDP RTT device→DC ≥ 50ms
                               (confirms download-path latency is observed end-to-end)

latency_upload_and_download  — Manual(latency=20ms) on device AND Manual(latency=30ms) on dc downlink;
                               RTT ≥ 20+30+30+20 = 100ms (each packet traverses both impairs twice)

latency_multihop_chain       — device(20ms) → NAT router WAN(30ms via set_router_impair) → ISP → DC;
                               RTT device→DC ≥ 2*(20+30) = 100ms

latency_region_asymmetric    — eu→us 10ms, us→eu 80ms; device(eu) probes DC(us) and DC(eu);
                               RTT eu→us ≈ 90ms, us→eu ≈ 90ms (round-trip crosses both directions)

latency_region_multi_router  — eu→us 30ms, us→eu 30ms; DC(eu), DC(us), ISP(us);
                               device(eu) → ISP(us) → DC(us): RTT crosses eu→us both ways ≥ 60ms
```

### 5l. Dynamic rate and latency changes

```
rate_dynamic_decrease        — apply rate=5000kbit; measure fast (expect ≥ 3000);
                               set_impair rate=500kbit; measure slow (expect ≤ 700);
                               assert slow ≤ fast / 4

rate_dynamic_remove          — apply rate=1000kbit; measure throttled;
                               set_impair(None); measure unthrottled;
                               assert unthrottled ≥ throttled × 3

latency_dynamic_add_remove   — baseline RTT; add Manual(latency=100ms); assert RTT +90ms;
                               remove; assert RTT returns near baseline
                               (already in existing dynamic_set_impair_changes_rtt but extend
                               to also verify RTT drops, not just increases)

rate_presets                 — Wifi preset: RTT ≥ 30ms (20ms latency); no rate cap → throughput ≥ 5000kbit;
                               Mobile preset: RTT ≥ 80ms; also has 1% loss (verify ≤ 98 of 100 pkts received)
```

---

## 6. Coverage Gaps vs Current Suite

| Gap | Test |
|---|---|
| TCP reflexive address | `reflexive_ip_all_combos` |
| Bind to device IP | `reflexive_ip_all_combos` (BindMode::SpecificIp) |
| Cgnat + TCP | `reflexive_ip_all_combos` (Cgnat × Tcp) |
| Reflexive IP after route switch | `switch_route_reflexive_ip`, `switch_route_udp_reflexive_change` |
| TCP connection behavior during route switch | `switch_route_tcp_conn_reset` |
| Multi-switch A→B→A IP tracking | `switch_route_multiple` |
| TCP link down/up | `link_down_up_connectivity` |
| Runtime NAT mode change | `nat_rebind_mode_port`, `nat_rebind_mode_ip` |
| Conntrack flush (new ports after rebind_nats) | `nat_rebind_conntrack_flush` |
| Intra-LAN private communication | `same_nat_private_comm` |
| Hairpinning (public IP loopback) | `same_nat_public_hairpin_off/on`, `hairpin_toggle` |
| Rate limiting — TCP upload | `rate_limit_tcp_upload` |
| Rate limiting — TCP download | `rate_limit_tcp_download` |
| Rate limiting — UDP upload | `rate_limit_udp_upload` |
| Rate limiting — UDP download | `rate_limit_udp_download` |
| Asymmetric up/down rates | `rate_limit_asymmetric` |
| Rate bottleneck across multi-hop | `rate_limit_multihop_bottleneck` |
| Stacked rate limits (both ends) | `rate_limit_two_hops_stack` |
| UDP packet loss counting | `loss_udp_moderate`, `loss_udp_high`, `loss_udp_both_directions` |
| TCP integrity under loss | `loss_tcp_integrity` |
| Device impair + region latency additive | `latency_device_plus_region` |
| Download-direction latency | `latency_download_direction` |
| Upload + download combined latency | `latency_upload_and_download` |
| Multi-hop latency chain (NAT WAN impair) | `latency_multihop_chain` |
| Asymmetric region latency | `latency_region_asymmetric` |
| Dynamic rate decrease/restore | `rate_dynamic_decrease`, `rate_dynamic_remove` |
| Preset verification (Wifi/Mobile) | `rate_presets` |

---

## 7. Files Changed

| File | Change |
|---|---|
| `Cargo.toml` | `strum` dev-dependency |
| `src/core.rs` | `router_nat_params`, `set_router_nat_mode`, `router_nat_mode`, `set_router_hairpin`; `RouterConfig` gains `hairpin: bool` |
| `src/lib.rs` | `set_nat_mode`, `rebind_nats`, `set_hairpin`, `set_router_impair`, `router_downlink_bridge` (pub); `probe_in_ns_from` (pub); `apply_hairpin` (internal); test module additions |

No new source files.

---

## 8. Implementation Order

1. `Cargo.toml`: add `strum` dev-dep
2. `core.rs`: `router_nat_params`, `set_router_nat_mode`, `router_nat_mode`, `RouterConfig.hairpin`, `set_router_hairpin`
3. `lib.rs` (non-test): `set_nat_mode`, `rebind_nats`, `probe_in_ns_from`, `apply_hairpin`, `set_hairpin`, `set_router_impair`, `router_downlink_bridge`
4. `lib.rs mod tests`: add `strum` derives to `NatMode`/`UplinkWiring`; `Proto`, `BindMode`, `NatTestCtx`, `DualNatLab`; refactor `build_single_nat_case` → `build_nat_case`; `probe_reflexive`, `probe_udp`, `probe_tcp`, `spawn_udp_reflector`, `spawn_tcp_reflector`, `spawn_tcp_sink`, `tcp_measure_throughput`, `udp_send_recv_count`, `build_dual_nat_lab`
5. Tests 5a → 5l in order
6. Confirm all existing tests pass

---

## 9. Verification

```sh
cargo test --lib -- tests::tcp_reflector_basic
cargo test --lib -- tests::reflexive_ip_all_combos
cargo test --lib -- tests::nat_rebind_mode_port
cargo test --lib -- tests::same_nat_public_hairpin_on
cargo test --lib -- tests::rate_limit_tcp_upload
cargo test --lib -- tests::latency_device_plus_region
cargo test --lib
```
