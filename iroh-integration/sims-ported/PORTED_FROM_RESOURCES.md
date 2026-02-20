# Ported Legacy Iroh Sims

Generated from `resources/iroh-sims/*/*.json` into current netsim TOML format.

- Source cases: 63
- Ported files: 63

## Notes

- Relay+DNS legacy scenarios are ported with relay flow only (no dedicated DNS server step).
- Multi-provider legacy scenarios are mapped to one transfer step per provider with fetchers distributed round-robin.
- Legacy link `bw/loss/latency` is mapped to `set-impair` manual settings.

## Files

- `ported-integration-iroh-1_to_1.toml`
- `ported-integration-iroh-1_to_3.toml`
- `ported-integration-iroh-2_to_2.toml`
- `ported-integration-iroh-2_to_4.toml`
- `ported-integration-iroh_full-1_to_1.toml`
- `ported-integration-iroh_full-1_to_1ro.toml`
- `ported-integration-iroh_full-1_to_1x3.toml`
- `ported-integration-iroh_full-1_to_3.toml`
- `ported-integration-relay-1_to_1.toml`
- `ported-integration-relay_only-1_to_1.toml`
- `ported-integration-relay_only-1_to_3.toml`
- `ported-iroh-iroh-1_to_1.toml`
- `ported-iroh-iroh-1_to_10.toml`
- `ported-iroh-iroh-1_to_3.toml`
- `ported-iroh-iroh-1_to_5.toml`
- `ported-iroh-iroh-2_to_10.toml`
- `ported-iroh-iroh-2_to_2.toml`
- `ported-iroh-iroh-2_to_4.toml`
- `ported-iroh-iroh-2_to_6.toml`
- `ported-iroh-iroh_10gb-1_to_1.toml`
- `ported-iroh-iroh_10gb-1_to_10.toml`
- `ported-iroh-iroh_10gb-1_to_3.toml`
- `ported-iroh-iroh_10gb-1_to_5.toml`
- `ported-iroh-iroh_10gb-2_to_10.toml`
- `ported-iroh-iroh_10gb-2_to_2.toml`
- `ported-iroh-iroh_10gb-2_to_4.toml`
- `ported-iroh-iroh_10gb-2_to_6.toml`
- `ported-iroh-iroh_200ms-1_to_1.toml`
- `ported-iroh-iroh_200ms-1_to_10.toml`
- `ported-iroh-iroh_200ms-1_to_3.toml`
- `ported-iroh-iroh_200ms-1_to_5.toml`
- `ported-iroh-iroh_200ms-2_to_10.toml`
- `ported-iroh-iroh_200ms-2_to_2.toml`
- `ported-iroh-iroh_200ms-2_to_4.toml`
- `ported-iroh-iroh_200ms-2_to_6.toml`
- `ported-iroh-iroh_20ms-1_to_1.toml`
- `ported-iroh-iroh_20ms-1_to_10.toml`
- `ported-iroh-iroh_20ms-1_to_3.toml`
- `ported-iroh-iroh_20ms-1_to_5.toml`
- `ported-iroh-iroh_20ms-2_to_10.toml`
- `ported-iroh-iroh_20ms-2_to_2.toml`
- `ported-iroh-iroh_20ms-2_to_4.toml`
- `ported-iroh-iroh_20ms-2_to_6.toml`
- `ported-iroh-iroh_relay_only-1_to_1.toml`
- `ported-iroh-iroh_relay_only-1_to_10.toml`
- `ported-iroh-iroh_relay_only-1_to_3.toml`
- `ported-iroh-iroh_relay_only-1_to_5.toml`
- `ported-iroh-iroh_relay_only-2_to_10.toml`
- `ported-iroh-iroh_relay_only-2_to_2.toml`
- `ported-iroh-iroh_relay_only-2_to_4.toml`
- `ported-iroh-iroh_relay_only-2_to_6.toml`
- `ported-paused-adverse-direct_lossy.toml`
- `ported-paused-adverse-direct_throttled.toml`
- `ported-paused-adverse-nat_both_lossy.toml`
- `ported-paused-adverse-nat_both_throttled.toml`
- `ported-paused-adverse-relay_dns_lossy.toml`
- `ported-paused-adverse-relay_dns_throttled.toml`
- `ported-paused-interface_switch-interface_down_up.toml`
- `ported-paused-interface_switch-route_switch_both_multi_nat.toml`
- `ported-paused-interface_switch-route_switch_mid_transfer.toml`
- `ported-paused-relay_nat-1_to_1_NAT_both.toml`
- `ported-paused-relay_nat-1_to_1_NAT_get.toml`
- `ported-paused-relay_nat-1_to_1_NAT_provide.toml`
