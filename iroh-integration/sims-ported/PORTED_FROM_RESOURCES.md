# Ported Legacy Iroh Sims

Generated from `resources/iroh-sims/*/*.json` into current netsim TOML format.

- Source cases: 63
- Ported files: 63

## Notes

- Relay+DNS legacy scenarios are ported with relay flow only (no dedicated DNS server step).
- Multi-provider legacy scenarios are mapped to one transfer step per provider with fetchers distributed round-robin.
- Legacy link `bw/loss/latency` is mapped to `set-impair` manual settings.

## Files

- `integration-iroh-1_to_1.toml`
- `integration-iroh-1_to_3.toml`
- `integration-iroh-2_to_2.toml`
- `integration-iroh-2_to_4.toml`
- `integration-iroh_full-1_to_1.toml`
- `integration-iroh_full-1_to_1ro.toml`
- `integration-iroh_full-1_to_1x3.toml`
- `integration-iroh_full-1_to_3.toml`
- `integration-relay-1_to_1.toml`
- `integration-relay_only-1_to_1.toml`
- `integration-relay_only-1_to_3.toml`
- `iroh-iroh-1_to_1.toml`
- `iroh-iroh-1_to_10.toml`
- `iroh-iroh-1_to_3.toml`
- `iroh-iroh-1_to_5.toml`
- `iroh-iroh-2_to_10.toml`
- `iroh-iroh-2_to_2.toml`
- `iroh-iroh-2_to_4.toml`
- `iroh-iroh-2_to_6.toml`
- `iroh-iroh_10gb-1_to_1.toml`
- `iroh-iroh_10gb-1_to_10.toml`
- `iroh-iroh_10gb-1_to_3.toml`
- `iroh-iroh_10gb-1_to_5.toml`
- `iroh-iroh_10gb-2_to_10.toml`
- `iroh-iroh_10gb-2_to_2.toml`
- `iroh-iroh_10gb-2_to_4.toml`
- `iroh-iroh_10gb-2_to_6.toml`
- `iroh-iroh_200ms-1_to_1.toml`
- `iroh-iroh_200ms-1_to_10.toml`
- `iroh-iroh_200ms-1_to_3.toml`
- `iroh-iroh_200ms-1_to_5.toml`
- `iroh-iroh_200ms-2_to_10.toml`
- `iroh-iroh_200ms-2_to_2.toml`
- `iroh-iroh_200ms-2_to_4.toml`
- `iroh-iroh_200ms-2_to_6.toml`
- `iroh-iroh_20ms-1_to_1.toml`
- `iroh-iroh_20ms-1_to_10.toml`
- `iroh-iroh_20ms-1_to_3.toml`
- `iroh-iroh_20ms-1_to_5.toml`
- `iroh-iroh_20ms-2_to_10.toml`
- `iroh-iroh_20ms-2_to_2.toml`
- `iroh-iroh_20ms-2_to_4.toml`
- `iroh-iroh_20ms-2_to_6.toml`
- `iroh-iroh_relay_only-1_to_1.toml`
- `iroh-iroh_relay_only-1_to_10.toml`
- `iroh-iroh_relay_only-1_to_3.toml`
- `iroh-iroh_relay_only-1_to_5.toml`
- `iroh-iroh_relay_only-2_to_10.toml`
- `iroh-iroh_relay_only-2_to_2.toml`
- `iroh-iroh_relay_only-2_to_4.toml`
- `iroh-iroh_relay_only-2_to_6.toml`
- `paused-adverse-direct_lossy.toml`
- `paused-adverse-direct_throttled.toml`
- `paused-adverse-nat_both_lossy.toml`
- `paused-adverse-nat_both_throttled.toml`
- `paused-adverse-relay_dns_lossy.toml`
- `paused-adverse-relay_dns_throttled.toml`
- `paused-interface_switch-interface_down_up.toml`
- `paused-interface_switch-route_switch_both_multi_nat.toml`
- `paused-interface_switch-route_switch_mid_transfer.toml`
- `paused-relay_nat-1_to_1_NAT_both.toml`
- `paused-relay_nat-1_to_1_NAT_get.toml`
- `paused-relay_nat-1_to_1_NAT_provide.toml`
