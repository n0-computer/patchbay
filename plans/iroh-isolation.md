# Transfer Isolation — Option A: Concrete Implementation Plan

The goal is to remove all iroh-specific knowledge from netsim's core so the
iroh integration lives entirely in sim TOML files (shippable from the iroh
repo) without losing the conciseness of today's `kind = "iroh-transfer"`.

---

## 1. Full inventory of iroh-specific code to remove

| File | LOC | What it is |
|------|-----|------------|
| `sim/transfer.rs` | ~490 | Entire file — provider+fetcher spawn, `EndpointBound` NDJSON polling, `PathStats` wait, `DownloadComplete`/`ConnectionTypeChanged` parsing, `add_env_to_cmd` |
| `sim/mod.rs` | 7 fields | `Step`: `kind`, `provider`, `fetcher`, `fetchers`, `relay_url`, `fetch_args`, `strategy` |
| `sim/runner.rs` | ~120 | `SimState`: `transfers`, `results`, `relay_assets`; spawn iroh branch; wait-for iroh branch; `maybe_inject_relay_config_path`; `ensure_relay_runtime_assets`; `generate_self_signed_relay_cert`; `result_field`; iroh=info default in `prepare_cmd` |
| `sim/report.rs` | ~120 | `TransferResult` + `parse_fetcher_log`; iroh columns in `write_results`/`write_combined_results_for_runs` |
| `sim/report.rs` | ~80 | `IperfResult`, `parse_iperf3_json_log`, `IperfMetrics`, `extract_json_object` |
| `sim/runner.rs` | ~80 | `StepParser::Iperf3Json`, `build_parser_config`, `apply_parser_result`, `iperf_result_field`; `SimState.iperf_results` |

`rcgen` stays — moves from implicit to a generic `gen-certs` step.

---

## 2. Design: captures as the unified data model

Every named value a process emits — whether needed live to unblock a
downstream step, or collected silently for reporting — is a **capture**.

A capture slot records:
- `history` — all matched values in order (appended on every match)
- `value()` — a method returning the latest entry (`history.last()`)

**Timing is on-demand:** nothing waits for a capture until something needs it.
- `${step_id.capture}` in a `cmd`/`args`/`content` field blocks exactly when
  that field is about to be evaluated, until the capture resolves.
- `requires = ["step_id.capture"]` forces a wait before the step executes,
  for captures not directly interpolated.
- A capture with no consumer resolves passively and silently.

This replaces three earlier mechanisms (live polling, `pick` result collection,
`NdjsonResult`) with a single concept.

---

## 3. Output parsers

Three parser modes, declared on `spawn` or `run` steps as `parser = "..."`:

| Parser | When it fires | How captures are populated |
|--------|---------------|---------------------------|
| `"text"` | **default** — streaming, per line | `regex` only (no `match`/`pick`) |
| `"ndjson"` | streaming, per line | `match` + `pick` on JSON lines; `regex` still works on raw text |
| `"json"` | after process exits | `pick` on the single parsed JSON object; no line-by-line matching |

**`"json"` replaces `"iperf3-json"`:** iperf3 outputs one JSON blob at exit;
`parser = "json"` with the appropriate `pick` paths extracts the same fields.
No special-case parser name needed. `baseline`/delta logic is removed — use
the `[results]` mapping (§6) and UI-level comparison instead.

**Capture spec** (applies to all three parsers):

```toml
[step.captures.endpoint_id]
pipe  = "stdout"          # "stdout" (default) or "stderr"
# text/ndjson:
regex = "some pattern"    # applied to raw line; group 1 or full match
# ndjson/json:
match = { kind = "EndpointBound" }   # key=value guards on parsed JSON
pick  = ".endpoint_id"               # dot-path into matched JSON
```

Rules:
- `regex` and `pick` may not both be set.
- `match` requires `pick` (and a JSON-capable parser).
- `pick` without `match` matches any JSON line/object.
- `match`/`pick` with `parser = "text"` is an error.

---

## 4. Assert syntax: `lhs @selector op rhs`

All assert expressions follow the form:

```
step_id.capture_name [@selector] operator [rhs]
```

### Selectors

| Selector | Source | When omitted |
|----------|--------|--------------|
| `@last` | `history.last()` | default for all operators except `count` |
| `@first` | `history[0]` | — |
| `@any` | any entry in `history` | — |
| `@all` | all entries in `history` | — |

### Value operators (work with `@last`, `@first`, `@any`, `@all`)

| Operator | Passes when |
|----------|-------------|
| `== rhs` | exact match |
| `!= rhs` | not exact match |
| `contains rhs` | substring present |
| `matches rhs` | regex match (full Rust regex syntax) |

With `@any`: passes if *any* history entry satisfies the condition.
With `@all`: passes if *all* history entries satisfy the condition.

### Count operators (no `@selector` — always operate on `history`)

```
step_id.capture_name count >= N
step_id.capture_name count <= N
step_id.capture_name count == N
step_id.capture_name count != N
step_id.capture_name count > N
step_id.capture_name count < N
```

`N` is a non-negative integer. `count >= 2` is the replacement for `changed-once`.
`count >= 1` checks that the capture fired at all.

### Examples

```toml
# fetcher received a download
"fetcher.size count >= 1"

# final connection was a direct IP
"fetcher.conn_type @last contains Direct"

# connection was relayed at some point
"fetcher.conn_type @any contains Relay"

# connection transitioned (at least two distinct values observed)
"fetcher.conn_type count >= 2"

# addr matches IP pattern at any point
"fetcher.remote-addr @any matches ^Ip\\(.*\\)$"

# all observed connection types were either Relay or Direct (no unexpected type)
"fetcher.conn_type @all matches ^(Relay|Direct)"

# iperf throughput was non-zero
"iperf-run.bps != 0"
```

Multi-assert shorthand: `checks = [...]` array on one step (all must pass).

---

## 5. Step results mapping

A `[results]` sub-table on a step maps **well-known normalized fields** to
capture references. Two reference forms are supported:

- `"step_id.capture_name"` — any step's capture (cross-step reference)
- `".capture_name"` — shorthand for this step's own capture (dot-prefix,
  no step id). Rewritten to `"<this_step_id>.capture_name"` during template
  expansion, so it works correctly in `[[step-template]]` without knowing
  the step's id in advance.

```toml
# In iroh-defaults.toml transfer-fetcher template — self-referencing:
[[step-template]]
name = "transfer-fetcher"
...
[step-template.results]
duration   = ".duration"   # → {step.id}.duration at expansion time
down_bytes = ".size"       # → {step.id}.size

# In a sim file — cross-step reference (less common):
[step.results]
duration  = "other-step.seconds"
```

```toml
[[step]]
kind   = "run"
id     = "iperf-run"
device = "client"
parser = "json"
cmd    = ["iperf3", "-J", "-c", "$NETSIM_IP_SERVER"]
[step.captures.bps]
pick = ".end.sum_received.bits_per_second"
[step.captures.bytes]
pick = ".end.sum_received.bytes"
[step.captures.seconds]
pick = ".end.sum_received.seconds"
[step.results]
duration   = "iperf-run.seconds"
down_bytes = "iperf-run.bytes"
```

**Well-known result fields:**

| Field | Type | Description |
|-------|------|-------------|
| `duration` | float (s) | Transfer/test duration |
| `up_bytes` | integer | Bytes sent (upload) |
| `down_bytes` | integer | Bytes received (download) |

Bandwidth is computed in the UI (`bytes / duration`). Not all fields need to
be set; unset fields are omitted from the output.

The UI uses these normalized fields to render comparison tables across steps
(iperf vs iroh-transfer) and across runs (baseline vs impaired).

**`results.json` output:** contains `all_captures` (every step's full capture
map, including history) plus `results` (normalized field values per step-id
that declared a `[results]` mapping). The UI can use captures for raw data
and `results` for structured comparison.

---

## 6. Step groups — one `use` expands to multiple steps

A `[[step-group]]` template defines a named sequence of steps. A sim uses it
with a single `[[step]]` entry (`use = "group-name"`) that expands to the
group's steps inline before execution.

**Group variables:** the `[[step]]` entry may supply a `vars` table
(`{key = "value"}`). Inside group step fields, `${group.key}` is substituted
with the caller-supplied value during expansion. This is the only new
interpolation namespace — it is resolved entirely at expansion time, before
runtime capture interpolation.

**Example in `iroh-defaults.toml`:**

```toml
[[step-group]]
name = "relay-setup"
# caller supplies: vars.device

[[step-group.step]]
use    = "relay-certs"
id     = "${group.device}-cert"
device = "${group.device}"

[[step-group.step]]
use = "relay-config"
id  = "${group.device}-cfg"
# relay-config template reads ${relay-cert.cert_pem_path} etc., but we need
# to reference the group-prefixed id; so relay-config template uses
# ${group.cert_id} — which means relay-config also needs to be callable
# standalone. Simpler: just inline the content here with group vars:
content = """
enable_relay               = true
enable_metrics             = true
enable_quic_addr_discovery = true
[tls]
cert_mode        = "Manual"
manual_cert_path = "${${group.device}-cert.cert_pem_path}"
manual_key_path  = "${${group.device}-cert.key_pem_path}"
"""
kind = "gen-file"

[[step-group.step]]
use    = "relay-spawn"
id     = "${group.device}"
device = "${group.device}"
args   = ["--config-path", "${${group.device}-cfg.path}"]
```

**Sim usage:**

```toml
[[step]]
use  = "relay-setup"
vars = { device = "relay1" }
```

This expands to three steps before execution, with all `${group.*}` tokens
substituted. The `vars` table is the only field meaningful on a group caller
(other `UseStep` fields are ignored — the group defines all its steps fully).

**Nested `${group.*}` in interpolation strings** like
`"${${group.device}-cert.cert_pem_path}"` are handled by a two-pass
substitution: first replace `${group.*}` tokens, then the outer `${...}`
becomes a normal capture reference resolved at runtime.

**No new `[[step-group]]` — alternative:** the relay-config template can instead
take a `cert_id` var directly:

```toml
[[step-group.step]]
use  = "relay-config"
id   = "${group.device}-cfg"
vars = { cert_id = "${group.device}-cert" }
```

...but this requires `[[step-template]]` to also support `${group.*}`, making
them group-aware. For simplicity, inline the content in the group (as shown
above) and keep `[[step-template]]` group-unaware.

---

## 7. New step actions (gen-certs, gen-file)

### `kind = "gen-certs"`

```toml
[[step]]
kind   = "gen-certs"
id     = "relay-cert"
device = "relay1"
cn     = "localhost"               # default
san    = ["$NETSIM_IP_RELAY1", "localhost"]   # default when device set
```

| Field | Default | Description |
|-------|---------|-------------|
| `id` | required | Prefixes output captures |
| `device` | — | Device whose IP is auto-added to SANs |
| `cn` | `"localhost"` | Certificate Common Name |
| `san` | `[device_ip, "localhost"]` | SANs; `$NETSIM_*` vars expanded |

Output captures: `cert_pem`, `key_pem`, `cert_pem_path`, `key_pem_path`.

### `kind = "gen-file"`

```toml
[[step]]
kind    = "gen-file"
id      = "relay-cfg"
content = "..."
```

Interpolates `content` (blocking on unresolved `${...}`), writes to
`{work_dir}/files/{id}/content`. Output capture: `path`.

---

## 8. `[[extends]]` — manifest inheritance

Replaces `sim.binaries` / `sim.steps` / `sim.topology`:

```toml
[[extends]]
file = "iroh-defaults.toml"
```

Each file may contribute `[[binary]]`, `[[step-template]]`, and/or
`topology`. Processed in order; later entries override earlier; the sim's own
declarations always win.

---

## 9. What replaces each iroh-specific mechanism

| Today | After |
|-------|-------|
| `kind = "iroh-transfer"` | `use = "transfer-provider"` + `use = "transfer-fetcher"` |
| `fetchers = [...]` loop | repeated `use = "transfer-fetcher"` steps |
| `EndpointBound` polling | `parser = "ndjson"` capture with `match`/`pick` |
| `parse_fetcher_log` | persistent ndjson captures on fetcher step |
| `maybe_inject_relay_config_path` | `gen-certs` + `gen-file` + `relay-spawn` template |
| `TransferResult` + iroh report columns | `[step.results]` mapping + generic captures table |
| `IperfResult` + `iperf_result_field` | `parser = "json"` + `[step.results]` |
| `parser = "iperf3-json"` | `parser = "json"` |
| baseline delta comparison | UI-level comparison using `[step.results]` normalized fields |
| `xfer.final_conn_direct == true` | `fetcher.conn_type @last contains Direct` |
| `xfer.conn_upgrade == true` | `fetcher.conn_type @any contains Direct` |
| `switch-route` | `set-default-route` |

---

## 10. Changes per file

### `sim/transfer.rs` — **delete** (~490 LOC)

### `sim/mod.rs` (+~120 LOC net)
- Remove: 7 iroh fields from `Step`; `SimMeta.binaries/steps/topology` strings; `action` field (rename to `kind`); old `Parser` enum
- Add `StepEntry` (untagged enum); `UseStep`; `StepTemplateDef`, `StepGroupDef`, `ExtendsEntry`, `StepResults`; new `Parser` enum (`Text/Ndjson/Json`)
- Update `CaptureSpec`: add `pipe`, rename `stdout_regex` → `regex`, add `match_fields`, `pick`
- Add new `Step` variants: `GenCerts`, `GenFile`, `SetDefaultRoute`; remove iroh fields from `Spawn`; `parser` fields → `Parser` enum
- Add to `SimFile`: `extends`, `step_templates`, `step_groups`; change `steps` to `Vec<StepEntry>`

### `sim/steps.rs` (~net +120 LOC)
- Remove: `maybe_inject_relay_config_path`; `ensure_relay_runtime_assets`;
  `generate_self_signed_relay_cert`; `build_parser_config`; `apply_parser_result`;
  old `read_captures`; `NETSIM_RUST_LOG` default in `prepare_cmd`;
  iroh-transfer branch in Spawn match arm; all iroh/iperf imports
- Add: `spawn_capture_reader` (persistent streaming); `interpolate_with_captures`
  (blocking via `CaptureStore.wait`); `GenCerts`, `GenFile`, `SetDefaultRoute`
  match arms; extended `evaluate_assert` with selector/count syntax;
  `extract_json_path` helper

### `sim/runner.rs` (~net +60 LOC)
- Remove: `SimState.transfers`, `.results`, `.relay_assets`, `.iperf_results`;
  `result_field`; `iperf_result_field`
- Add: `load_extends`; `expand_step_groups`; `merge_use_step`; `expand_steps`;
  `.capture_name` rewrite pass in `merge_use_step`; `SimState.captures: CaptureStore`;
  `SimState.step_results: Vec<StepResultRecord>`; post-step results collection

### `sim/capture.rs` (new, ~80 LOC)
- `CaptureSlot`, `CaptureStore` with `Mutex + Condvar`

### `sim/report.rs` (−~160 LOC net)
- Remove: `TransferResult`, `parse_fetcher_log`, `is_direct_addr`, iroh columns,
  `IperfResult`, `IperfMetrics`, `parse_iperf3_json_log`, `extract_json_object`
- Add: generic per-step captures + results tables in `write_results`; `results.json` all_captures emission

### `Cargo.toml` — no changes

---

## 11. LOC summary

| | Removed | Added | Net |
|-|---------|-------|-----|
| `sim/transfer.rs` | 490 | 0 | −490 |
| `sim/mod.rs` | ~30 | ~120 | +90 |
| `sim/runner.rs` | ~200 | ~230 | +30 |
| `sim/report.rs` | ~200 | ~40 | −160 |
| **Total** | **~920** | **~390** | **−530** |

---

## 12. What iroh sim TOML files look like after

### `iroh-defaults.toml` (ships in iroh repo)

```toml
[[binary]]
name    = "transfer"
repo    = "https://github.com/n0-computer/iroh"
example = "transfer"

[[binary]]
name = "relay"
repo = "https://github.com/n0-computer/iroh"
bin  = "iroh-relay"

# ── Relay setup ───────────────────────────────────────────────────────────────
# Single step-group that expands to cert generation, config file, and spawn.
# Caller supplies: vars.device

[[step-group]]
name = "relay-setup"

[[step-group.step]]
kind   = "gen-certs"
id     = "${group.device}-cert"
device = "${group.device}"

[[step-group.step]]
kind    = "gen-file"
id      = "${group.device}-cfg"
content = """
enable_relay               = true
enable_metrics             = true
enable_quic_addr_discovery = true
[tls]
cert_mode        = "Manual"
manual_cert_path = "${${group.device}-cert.cert_pem_path}"
manual_key_path  = "${${group.device}-cert.key_pem_path}"
"""

[[step-group.step]]
kind   = "spawn"
id     = "${group.device}"
device = "${group.device}"
cmd    = ["${binary.relay}", "--config-path", "${${group.device}-cfg.path}"]
env    = { RUST_LOG = "iroh_relay=info" }
[step-group.step.captures.ready]
pipe  = "stderr"
regex = "relay: serving on"

# ── Transfer ──────────────────────────────────────────────────────────────────

[[step-template]]
name   = "transfer-provider"
kind   = "spawn"
parser = "ndjson"
cmd    = ["${binary.transfer}", "--output", "json", "provide", "--env", "dev"]
env    = { RUST_LOG = "iroh=info,iroh::_events=debug" }
[step-template.captures.endpoint_id]
match = { kind = "EndpointBound" }
pick  = ".endpoint_id"
[step-template.captures.direct_addr]
match = { kind = "EndpointBound" }
pick  = ".direct_addresses.0"

[[step-template]]
name   = "transfer-fetcher"
kind   = "spawn"
parser = "ndjson"
cmd    = ["${binary.transfer}", "--output", "json", "fetch", "--env", "dev"]
env    = { RUST_LOG = "iroh=info,iroh::_events=debug" }
# caller supplies via args: <endpoint_id> --relay-url <url> etc.
[step-template.captures.size]
match = { kind = "DownloadComplete" }
pick  = ".size"
[step-template.captures.duration]
match = { kind = "DownloadComplete" }
pick  = ".duration"
[step-template.captures.remote-addr]
match  = { kind = "ConnectionTypeChanged", status = "Selected" }
pick   = ".addr"
[step-template.captures.conn_type]
match  = { kind = "ConnectionTypeChanged", status = "Selected" }
pick   = ".conn_type"
[step-template.results]
duration   = ".duration"   # expanded to "{step.id}.duration" at template expansion time
down_bytes = ".size"
```

### `sims/iroh-direct.toml`

```toml
[[extends]]
file = "iroh-defaults.toml"

[sim]
name     = "iroh-direct"
topology = "two-device-relay"

[[step]]
use  = "relay-setup"
vars = { device = "relay1" }

[[step]]
use      = "transfer-provider"
id       = "provider"
device   = "d1"
args     = ["--relay-url", "https://${NETSIM_IP_RELAY1}:3340"]
requires = ["relay1.ready"]

[[step]]
use    = "transfer-fetcher"
id     = "fetcher"
device = "d2"
args   = ["${provider.endpoint_id}",
          "--relay-url",        "https://${NETSIM_IP_RELAY1}:3340",
          "--remote-relay-url", "https://${NETSIM_IP_RELAY1}:3340"]

[[step]]
kind    = "wait-for"
id      = "fetcher"
timeout = "120s"

[[step]]
kind   = "assert"
checks = [
  "fetcher.size count >= 1",
  "fetcher.remote-addr @last contains Ip(",
]
```

### `sims/iroh-switch-direct.toml`

The topology starts with relay-only connectivity between provider and fetcher
(no direct path). After 10 s the fetcher's default route is switched to the
interface with direct reachability. Assertions verify the full relay→direct
transition.

```toml
[[extends]]
file = "iroh-defaults.toml"

[sim]
name     = "iroh-switch-direct"
topology = "switch-direct"

[[step]]
use  = "relay-setup"
vars = { device = "relay1" }

[[step]]
use      = "transfer-provider"
id       = "provider"
device   = "d1"
args     = ["--relay-url", "https://${NETSIM_IP_RELAY1}:3340"]
requires = ["relay1.ready"]

[[step]]
use    = "transfer-fetcher"
id     = "fetcher"
device = "d2"
args   = ["${provider.endpoint_id}",
          "--relay-url",        "https://${NETSIM_IP_RELAY1}:3340",
          "--remote-relay-url", "https://${NETSIM_IP_RELAY1}:3340"]

[[step]]
kind     = "wait"
duration = "10s"

[[step]]
kind   = "set-default-route"
device = "fetcher"
to     = "eth1"

[[step]]
kind    = "wait-for"
id      = "fetcher"
timeout = "120s"

[[step]]
kind   = "assert"
checks = [
  "fetcher.conn_type @first contains Relay",    # started on relay
  "fetcher.conn_type @any contains Direct",     # transitioned to direct at some point
  "fetcher.conn_type @last contains Direct",    # ended on direct
  "fetcher.conn_type count >= 2",               # connection type changed
  "fetcher.remote-addr @any matches ^Ip\\(.*\\)$",  # saw a direct IP address
]
```

### `sims/iperf-1to1.toml` (example showing json parser and results)

```toml
[sim]
name     = "iperf-1to1"
topology = "two-device"

[[step]]
kind   = "spawn"
id     = "server"
device = "server"
cmd    = ["iperf3", "-s"]

[[step]]
kind   = "run"
id     = "iperf-run"
device = "client"
parser = "json"
cmd    = ["iperf3", "-J", "-c", "$NETSIM_IP_SERVER", "-t", "10"]
[step.captures.bps]
pick = ".end.sum_received.bits_per_second"
[step.captures.bytes]
pick = ".end.sum_received.bytes"
[step.captures.seconds]
pick = ".end.sum_received.seconds"
[step.results]
duration   = "iperf-run.seconds"
down_bytes = "iperf-run.bytes"

[[step]]
kind   = "assert"
checks = [
  "iperf-run.bps count >= 1",
  "iperf-run.bps != 0",
]
```

---

## 13. Backwards compatibility note

`results.json`: `"transfers"` key replaced by `"captures"` (all step captures
including history) and `"results"` (normalized fields from `[step.results]`
mappings). Downstream tooling needs updating. Old `final_conn_direct`/
`conn_upgrade` booleans gone; use `conn_type` captures.

---

## 14. Reference: complete sim TOML specification

### `[[extends]]`

| Key | Type | Description |
|-----|------|-------------|
| `file` | string | Shared manifest file (search: adjacent to sim, `../`, `$CWD`). May contain `[[binary]]`, `[[step-template]]`, `topology`. Processed in order; later entries win; sim's own declarations always win. |

---

### `[sim]`

| Key | Type | Description |
|-----|------|-------------|
| `name` | string | Sim identifier; used in report filenames |
| `topology` | string | Topology name; overrides any from `[[extends]]` |

---

### `[[binary]]`

| Key | Type | Description |
|-----|------|-------------|
| `name` | string | `${binary.<name>}` reference key |
| `path` | path | Local prebuilt binary |
| `url` | string | HTTP(S) download URL |
| `repo` | string | Git repository URL |
| `commit` | string | Branch/tag/SHA (default: `"main"`) |
| `example` | string | `cargo --example <name>` |
| `bin` | string | `cargo --bin <name>` |

Exactly one source required: `path`, `url`, or `repo`+(`example`|`bin`).

---

### `[[step-template]]`

Same fields as `[[step]]` plus `name` (required). May be inline or in a file
referenced by `[[extends]]`. Referenced by `use = "<name>"` in a step.

---

### `[[step-group]]`

Defines a named sequence of steps that expands inline wherever
`use = "<group-name>"` appears in a `[[step]]`.

| Key | Type | Description |
|-----|------|-------------|
| `name` | string | Group identifier; referenced by `use = "<name>"` |
| `[[step-group.step]]` | array | Ordered step definitions (raw TOML tables, parsed as `Step` after `${group.*}` substitution) |

A `[[step]]` entry that references a group:

| Key | Type | Description |
|-----|------|-------------|
| `use` | string | Group name |
| `vars` | table | `{key = "value"}` — substituted for `${group.key}` in all group step fields before runtime |

`${group.key}` substitution is done at expansion time (before execution), not
at runtime. Nested references like `"${${group.device}-cert.path}"` are
two-pass: `${group.device}` is resolved first, yielding e.g.
`"${relay1-cert.path}"`, which is then resolved at runtime as a capture
reference.

Group steps may use `use = "<step-template-name>"` to inherit from a template.
They may not themselves use `use = "<group-name>"` (no nesting).

---

### `[[step]]`

#### Common fields

| Key | Type | Description |
|-----|------|-------------|
| `kind` | string | Step type (see below). Defaults to `"run"` if `cmd` present |
| `id` | string | Step identifier. Required for `spawn`, `gen-certs`, `gen-file`. Referenced as `${id.capture}` |
| `use` | string | Template or group name. If a group: step expands to multiple steps; only `vars` is used from this entry |
| `vars` | table | Group variables; only meaningful when `use` references a `[[step-group]]` |
| `device` | string | Target network namespace |
| `env` | table | Extra env vars; merged with template's `env` (step wins on collision) |
| `requires` | array of strings | `["id.capture"]` — wait for these captures before executing |

---

#### `kind = "run"` and `kind = "spawn"`

`run` executes and waits for exit before next step.
`spawn` starts in background; a `wait-for` step waits for its exit.

| Key | Type | Description |
|-----|------|-------------|
| `cmd` | array of strings | Command + arguments. Supports `${binary.<n>}`, `$NETSIM_*`, `${id.capture}` |
| `args` | array of strings | Appended to template's `cmd`; does not replace it |
| `parser` | string | `"text"` (default), `"ndjson"`, `"json"` |
| `ready_after` | duration | Static delay after spawn before considered started |
| `[captures.<name>]` | table | Named capture (see below) |
| `[results]` | table | Normalized result mapping (see below) |

---

#### `[captures.<name>]` — capture specification

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `pipe` | string | `"stdout"` | `"stdout"` or `"stderr"` |
| `regex` | string | — | Regex on raw text line; group 1 or full match. All parsers. |
| `match` | table | — | Key=value guards on parsed JSON. `ndjson`/`json` only. All keys must match. |
| `pick` | string | — | Dot-path into parsed JSON, e.g. `".endpoint_id"`, `".end.sum_received.bytes"`. `ndjson`/`json` only. |

Rules:
- `regex` and `pick` may not both be set.
- `match` requires `pick`.
- `match`/`pick` require `parser = "ndjson"` or `parser = "json"`.
- With `parser = "json"`: match/pick apply to the single document after process exit.
- With `parser = "ndjson"`: match/pick apply to each line; fires once per matching line (history accumulated).
- With `parser = "text"`: only `regex` is valid.

For `parser = "ndjson"`: each match appends to `history`; `value` = latest.
For `parser = "json"` and `regex` on `parser = "text"`: single-valued (no meaningful history).
Available for interpolation as `${id.name}` (latest value); in asserts via `@selector`.

---

#### `[results]` — normalized result fields

Maps well-known UI fields to capture references.

| Field | Type | Description |
|-------|------|-------------|
| `duration` | float (s) | Transfer/test duration |
| `up_bytes` | integer | Bytes sent (upload) |
| `down_bytes` | integer | Bytes received (download) |

Bandwidth is computed in the UI. Unset fields are omitted.

Reference forms:
- `"step_id.capture_name"` — any step's capture
- `".capture_name"` — this step's own capture (valid in `[[step-template]]`; rewritten to `"{id}.capture_name"` at expansion time)

Example:
```toml
# In a template (self-referencing):
[step-template.results]
duration   = ".duration"
down_bytes = ".size"

# In a sim step (cross-step):
[step.results]
duration = "other-step.seconds"
```

---

#### `kind = "wait-for"`

| Key | Type | Description |
|-----|------|-------------|
| `id` | string | **Required.** ID of a spawned step to wait for |
| `timeout` | duration | Default `"300s"` |

#### `kind = "wait"`

| Key | Type | Description |
|-----|------|-------------|
| `duration` | duration | **Required.** Sleep duration |

#### `kind = "set-impair"`

| Key | Type | Description |
|-----|------|-------------|
| `device` | string | Target device |
| `interface` | string | Interface name |
| `impair` | string or table | Preset `"wifi"` / `"mobile"`, or `{ rate, loss, latency }` |

#### `kind = "link-down"` / `"link-up"`

| Key | Type | Description |
|-----|------|-------------|
| `device` | string | Target device |
| `interface` | string | Interface name |

#### `kind = "set-default-route"`

| Key | Type | Description |
|-----|------|-------------|
| `device` | string | Target device |
| `to` | string | Interface to set as default route |

#### `kind = "gen-certs"`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `id` | string | required | Step ID; prefixes output captures |
| `device` | string | — | Device whose IP is auto-added to SANs |
| `cn` | string | `"localhost"` | Certificate Common Name |
| `san` | array of strings | `[device_ip, "localhost"]` | SANs; `$NETSIM_*` expanded |

Output captures (prefixed with `id`): `cert_pem`, `key_pem`, `cert_pem_path`, `key_pem_path`.

#### `kind = "gen-file"`

| Key | Type | Description |
|-----|------|-------------|
| `id` | string | required |
| `content` | string | required. `${...}` interpolated before writing; blocks on unresolved captures |

Output capture: `path`.

---

#### `kind = "assert"`

| Key | Type | Description |
|-----|------|-------------|
| `check` | string | Single assertion expression |
| `checks` | array of strings | Multiple expressions; all must pass |

**Assert expression syntax:**

```
step_id.capture_name [@selector] operator [rhs]
step_id.capture_name count comp_op N
```

**Selectors** (select which value(s) to test):

| Selector | Source | Default for |
|----------|--------|-------------|
| `@last` | latest `value` | `==`, `!=`, `contains`, `matches` when no selector given |
| `@first` | `history[0]` | — |
| `@any` | any entry in `history` | — |
| `@all` | all entries in `history` | — |

**Value operators:**

| Operator | Passes when |
|----------|-------------|
| `== rhs` | exact match |
| `!= rhs` | not exact match |
| `contains rhs` | substring present |
| `matches rhs` | full Rust regex match |

**Count operators** (always operate on `history`; no `@selector`):

| Expression | Passes when |
|------------|-------------|
| `count >= N` | history length ≥ N |
| `count <= N` | history length ≤ N |
| `count == N` | history length = N |
| `count != N` | history length ≠ N |
| `count > N` | history length > N |
| `count < N` | history length < N |

Examples:
```
fetcher.conn_type @first contains Relay
fetcher.conn_type @any contains Direct
fetcher.conn_type @last contains Direct
fetcher.conn_type @all matches ^(Relay|Direct)
fetcher.conn_type count >= 2
fetcher.remote-addr @any matches ^Ip\\(.*\\)$
iperf-run.bps count >= 1
iperf-run.bps != 0
```

---

### Variable interpolation

Supported in: `cmd`, `args`, `env` values, `gen-file` content, `gen-certs` `san` entries.

| Pattern | Resolves to |
|---------|-------------|
| `${binary.<name>}` | Resolved path to named binary |
| `$NETSIM_IP_<DEVICE>` | IP of device (name uppercased, non-alphanum → `_`) |
| `${step_id.<capture_name>}` | Latest capture value; blocks until resolved |

---

### Duration format

`"<n>s"`, `"<n>ms"`, `"<n>m"` — e.g. `"120s"`, `"500ms"`, `"2m"`.

---

## 15. Implementation order

**Commit groupings:**

- **Commit A (Steps 1–6):** purely additive — new types and infrastructure
  alongside existing code. No existing tests break.
- **Commit B (Step 7):** sim file porting — all sim TOML files updated to use
  new templates/groups; old and new code co-exist.
- **Commit C (Steps 8–9):** breaking removal — delete iroh-specific runner
  code, `SimState` fields, env functions. Requires Commit B to be complete.

**Threading model (critical context):** The step runner is entirely
synchronous. `execute_step` in `sim/steps.rs` is a plain `fn`, called in a
`for` loop from `async fn execute_single_sim` in `sim/runner.rs`. Pump
threads are `std::thread` threads using `std::sync::mpsc`. **Do not
introduce `tokio::sync` primitives or `.await` into the step loop.** All
blocking for on-demand capture resolution uses `std::sync::{Mutex, Condvar}`.

**`Step` is a tagged enum (critical context):** `Step` in `sim/mod.rs` is:

```rust
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum Step { Run { .. }, Spawn { .. }, Wait { .. }, WaitFor { .. },
                SetImpair { .. }, SwitchRoute { .. }, LinkDown { .. },
                LinkUp { .. }, Assert { .. } }
```

New step kinds (`gen-certs`, `gen-file`, `set-default-route`) must be new
enum variants, not match arms on a string field. Template expansion
(`use = "..."`) is handled via a `#[serde(untagged)]` `StepEntry` enum (see
Step 2): `UseStep` captures call-site fields, templates are stored as raw
`toml::value::Table`s, merged, then parsed into `Step` at load time.

---

### Step 1 — `CaptureStore`: persistent capture accumulation

**Goal:** replace `SimEnv.captures: HashMap<String, String>` (last-value only,
blocks until all captures resolve) with a persistent store that:
- accumulates history for ndjson captures
- allows downstream steps to block on a specific capture on demand
- is updated from pump threads without holding `SimState` lock

**New file `sim/capture.rs`** (~80 LOC):

```rust
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use anyhow::{bail, Result};

#[derive(Default, Clone)]
pub struct CaptureSlot {
    pub history: Vec<String>,      // all matched values, oldest first
}

impl CaptureSlot {
    /// Latest matched value, or `None` if never matched.
    pub fn value(&self) -> Option<&str> {
        self.history.last().map(|s| s.as_str())
    }
}

#[derive(Default)]
struct CaptureInner {
    slots: HashMap<String, CaptureSlot>,
}

/// Thread-safe capture store shared between the step loop and pump threads.
#[derive(Clone)]
pub struct CaptureStore {
    inner: Arc<(Mutex<CaptureInner>, Condvar)>,
}

impl CaptureStore {
    pub fn new() -> Self {
        Self { inner: Arc::new((Mutex::new(CaptureInner::default()), Condvar::new())) }
    }

    /// Record a new value for a capture key `"step_id.capture_name"`.
    /// Appends to `history`, wakes all waiters.
    pub fn record(&self, key: &str, value: String) {
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap();
        inner.slots.entry(key.to_string()).or_default().history.push(value);
        cvar.notify_all();
    }

    /// Block until `key` has at least one value, then return the latest.
    /// Returns `Err` on timeout.
    pub fn wait(&self, key: &str, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap();
        loop {
            if let Some(v) = inner.slots.get(key).and_then(|s| s.value()) {
                return Ok(v.to_owned());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("timeout waiting for capture '{}'", key);
            }
            let (guard, result) = cvar.wait_timeout(inner, remaining).unwrap();
            inner = guard;
            if result.timed_out() {
                bail!("timeout waiting for capture '{}'", key);
            }
        }
    }

    /// Non-blocking snapshot of all slots (for reporting).
    pub fn snapshot(&self) -> HashMap<String, CaptureSlot> {
        self.inner.0.lock().unwrap().slots.clone()
    }

    /// Non-blocking latest value for interpolation (returns `None` if unset).
    pub fn get(&self, key: &str) -> Option<String> {
        self.inner.0.lock().unwrap().slots.get(key)?.value().map(|s| s.to_owned())
    }
}
```

**`sim/runner.rs` — add to `SimState`:**

```rust
pub captures: CaptureStore,
```

Remove `SimEnv.captures` (the `HashMap<String, String>` field and its
`set_capture`/`get_capture` methods in `sim/env.rs`) once all callers are
migrated. Keep `SimEnv.interpolate_str` for `$NETSIM_*` and `${binary.*}`;
extend it to call `captures.get(key)` for `${step_id.capture}` patterns (see
Step 3).

---

### Step 2 — `SimFile` types: `StepEntry`, templates, extends, new Step variants

**Why not a flat `RawStep`:** `Step` is a `#[serde(tag = "action")]` enum, so
serde picks the variant from `"action"` (renamed to `"kind"`) at parse time.
A `use = "..."` step doesn't have a `kind` yet — the approach is to store the
call site as a small `UseStep` struct, keep templates as raw `toml::value::Table`
values, merge the TOML tables at call time, then parse the merged table into
`Step`. This avoids duplicating every Step field in a flat intermediary and
lets `#[serde(untagged)]` do the right thing.

**`sim/mod.rs` — `StepEntry` (replaces bare `Step` in `steps` vec):**

```rust
/// Top-level `[[step]]` entry. Either a template call or a concrete step.
#[derive(Deserialize, Clone)]
#[serde(untagged)]
pub enum StepEntry {
    /// Deserialized when `use` key is present (no `kind` required).
    UseTemplate(UseStep),
    /// Deserialized when `kind` (or legacy `action`) key is present.
    Concrete(Step),
}

/// Call-site fields for `use = "template-or-group-name"`.
#[derive(Deserialize, Clone)]
pub struct UseStep {
    #[serde(rename = "use")]
    pub use_name: String,
    /// Group substitution variables (`${group.key}` tokens).
    #[serde(default)]
    pub vars: HashMap<String, String>,
    /// Override fields — merged on top of template before parsing into `Step`.
    pub id:           Option<String>,
    pub device:       Option<String>,
    #[serde(default)]
    pub env:          HashMap<String, String>,
    #[serde(default)]
    pub args:         Vec<String>,
    #[serde(default)]
    pub requires:     Vec<String>,
    pub results:      Option<StepResults>,
    pub timeout:      Option<String>,
    #[serde(default)]
    pub captures:     HashMap<String, CaptureSpec>,
}
```

`#[serde(untagged)]` tries `UseTemplate` first (succeeds iff `use` key is
present), then falls back to `Concrete`. Since `UseStep` has only optional
fields besides `use_name`, the discriminant is unambiguous.

**`sim/mod.rs` — template and group storage:**

```rust
/// `[[step-template]]` entry: name + raw TOML table for deferred parsing.
#[derive(Deserialize, Clone)]
pub struct StepTemplateDef {
    pub name: String,
    /// The remaining fields stored as a raw table for merge-then-parse.
    #[serde(flatten)]
    pub raw: toml::value::Table,
}

/// `[[step-group]]` entry: name + sequence of raw step tables.
#[derive(Deserialize, Clone)]
pub struct StepGroupDef {
    pub name: String,
    #[serde(default, rename = "step")]
    pub steps: Vec<toml::value::Table>,
}
```

Storing templates as `toml::value::Table` instead of a typed struct lets the
merge happen at the TOML layer: insert call-site override key/values into the
table, then call `.try_into::<Step>()` exactly once on the merged table.

**`sim/mod.rs` — `SimFile` changes:**

```rust
#[derive(Deserialize, Default)]
pub struct SimFile {
    #[serde(default)]
    pub extends: Vec<ExtendsEntry>,

    #[serde(default)]
    pub sim: SimMeta,

    #[serde(default, rename = "binary")]
    pub binaries: Vec<BinarySpec>,

    #[serde(default, rename = "step-template")]
    pub step_templates: Vec<StepTemplateDef>,

    #[serde(default, rename = "step-group")]
    pub step_groups: Vec<StepGroupDef>,

    #[serde(default, rename = "step")]
    pub steps: Vec<StepEntry>,

    // inline topology (unchanged)
    #[serde(default)]
    pub router: Vec<netsim::config::RouterCfg>,
    #[serde(default)]
    pub device: HashMap<String, toml::Value>,
    pub region: Option<HashMap<String, netsim::config::RegionConfig>>,
}

#[derive(Deserialize)]
pub struct ExtendsEntry { pub file: String }
```

Remove `SimMeta.binaries` (old `sim.binaries` string field). Remove old
`Parser` enum and `CaptureSpec.stdout_regex` in this step.

**New `CaptureSpec`:**

```rust
#[derive(Deserialize, Clone, Default)]
pub struct CaptureSpec {
    #[serde(default = "pipe_default")]
    pub pipe: String,                            // "stdout" (default) | "stderr"
    pub regex: Option<String>,                   // raw-text line regex
    #[serde(rename = "match")]
    pub match_fields: Option<HashMap<String, String>>,  // JSON key=value guards
    pub pick: Option<String>,                    // dot-path into JSON
}
fn pipe_default() -> String { "stdout".into() }
```

**`sim/mod.rs` — `Parser` enum** (replaces the old `Iperf3Json`-only enum and
`Option<String>` parser fields):

```rust
#[derive(Deserialize, Clone, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Parser {
    #[default]
    Text,
    Ndjson,
    Json,
}
```

**New `Step` variants** (add to the enum; `SwitchRoute` becomes
`SetDefaultRoute`; old iroh fields removed in Step 9):

```rust
#[serde(tag = "kind", alias = "action", rename_all = "kebab-case")]
pub enum Step {
    Run     { id: Option<String>, device: String, cmd: Vec<String>,
              env: HashMap<String,String>,
              #[serde(default)] parser: Parser,
              #[serde(default)] captures: HashMap<String,CaptureSpec>,
              #[serde(default)] requires: Vec<String>,
              results: Option<StepResults> },
    Spawn   { id: String, device: String, cmd: Vec<String>,
              env: HashMap<String,String>,
              #[serde(default)] parser: Parser,
              ready_after: Option<String>,
              #[serde(default)] captures: HashMap<String,CaptureSpec>,
              #[serde(default)] requires: Vec<String>,
              results: Option<StepResults> },
    Wait    { duration: String },
    WaitFor { id: String, timeout: Option<String> },
    SetImpair { device: String, interface: Option<String>, impair: Option<toml::Value> },
    SetDefaultRoute { device: String, to: String },  // replaces SwitchRoute
    LinkDown { device: String, interface: String },
    LinkUp   { device: String, interface: String },
    GenCerts { id: String, device: Option<String>, cn: Option<String>,
               san: Option<Vec<String>> },
    GenFile  { id: String, content: String },
    Assert   { check: Option<String>, #[serde(default)] checks: Vec<String> },
}
```

Note: keep `SwitchRoute` temporarily during Step 7 sim porting; remove in Step 9.

**`sim/mod.rs` — `StepResults`:**

```rust
#[derive(Deserialize, Clone, Default)]
pub struct StepResults {
    pub duration:   Option<String>,   // "step_id.capture_name" or ".capture_name"
    pub up_bytes:   Option<String>,
    pub down_bytes: Option<String>,
}
```

**`sim/runner.rs` — `load_extends`:**

```rust
fn load_extends(
    sim_file: &SimFile,
    sim_path: &Path,
) -> Result<(HashMap<String, BinarySpec>, HashMap<String, StepTemplateDef>, HashMap<String, StepGroupDef>)>
```

For each `ExtendsEntry`:
1. Search paths: `sim_path/../<file>`, `sim_path/../../<file>`, `cwd/<file>`.
2. Parse as `SimFile`.
3. Merge `binaries`, `step_templates`, and `step_groups` (name → entry, later wins).

The sim's own `[[binary]]`, `[[step-template]]`, and `[[step-group]]` entries override extends.

**`sim/runner.rs` — `expand_step_groups`** (run before template expansion):

```rust
fn expand_step_groups(
    steps: Vec<StepEntry>,
    groups: &HashMap<String, StepGroupDef>,
) -> Result<Vec<StepEntry>>
```

Produces a new flat `Vec<StepEntry>`:
- For each input entry:
  - If `UseTemplate(use_step)` and `use_step.use_name` references a known group:
    1. Clone the group's `Vec<toml::value::Table>` steps.
    2. For each raw table, walk all `toml::Value::String` leaves, replacing
       `${group.key}` tokens with values from `use_step.vars`. Unknown keys → error.
    3. Two-pass: first replace `${group.*}`, yielding strings like
       `"${relay1-cert.cert_pem_path}"` — leave those for runtime interpolation.
    4. Wrap each resulting table as `StepEntry::Concrete` by parsing it via
       `table.try_into::<Step>()?`.
    5. Push all expanded entries into output; do not push the original group caller.
  - Otherwise: push the entry unchanged.

**`sim/runner.rs` — `merge_use_step`** (run after group expansion):

```rust
fn merge_use_step(
    use_step: UseStep,
    template: &StepTemplateDef,
) -> Result<Step>
```

1. Clone `template.raw` (a `toml::value::Table`).
2. Apply `UseStep` override fields into the table (insert as `toml::Value`).
   - `id`, `device`, `timeout`: insert if `Some`.
   - `env`, `args`, `captures`, `requires`: merge (use_step wins on collision).
3. `results`: if `use_step.results` is `Some`, insert; otherwise keep template's.
   Then rewrite any `.capture_name` shorthand to `"{id}.capture_name"` in the table.
   A missing `id` when a `.` reference exists is an error at load time.
4. Call `merged_table.try_into::<Step>()` — serde handles `kind` dispatch.

**`sim/runner.rs` — `expand_steps`** (entry point, called from `load_sim`):

```rust
fn expand_steps(
    entries: Vec<StepEntry>,
    templates: &HashMap<String, StepTemplateDef>,
    groups: &HashMap<String, StepGroupDef>,
) -> Result<Vec<Step>>
```

1. Call `expand_step_groups(entries, groups)?` → `Vec<StepEntry>`.
2. For each remaining entry:
   - `Concrete(step)` → push as-is.
   - `UseTemplate(use_step)` → look up template; call `merge_use_step`; push result.

---

### Step 3 — Capture readers: persistent streaming into `CaptureStore`

**Replace `read_captures` in `sim/steps.rs`.** Currently `read_captures`
blocks until all captures resolve, then breaks. New behaviour: **runs to EOF**,
updating the store on every match. This supports history accumulation (ndjson)
and allows the step loop to continue while captures keep streaming.

**`sim/steps.rs` — `spawn_capture_reader`:**

```rust
/// Spawns a std::thread that reads lines from `rx` (fed by a pipe pump),
/// applies capture specs, and records matches into `store`.
/// Returns a JoinHandle. Must be joined before result collection.
fn spawn_capture_reader(
    rx: std::sync::mpsc::Receiver<String>,
    parser: Parser,       // Text | Ndjson | Json — determines strategy
    specs: HashMap<String, CaptureSpec>,
    step_id: String,
    store: CaptureStore,
) -> thread::JoinHandle<Result<()>>
```

Implementation inside the thread:
- **`Parser::Text` or `Parser::Ndjson` (streaming):** iterate `rx` line by line.
  For each line, for each capture spec:
  - if `regex` set: apply `Regex::find`/captures, group 1 or full match.
  - if `pick` set (ndjson only): try `serde_json::from_str::<Value>(&line)`;
    if `match_fields` all match the JSON object, extract dot-path value.
  - call `store.record(format!("{step_id}.{name}"), value)` on match.
  - Continue to EOF in both cases (allows multiple regex events / history accumulation).
- **`Parser::Json` (post-exit):** collect all lines into `full_output: String`.
  After the receiver closes (process exited), parse `full_output` as JSON.
  Apply `match_fields` + `pick` to the single document.

**`extract_json_path(v: &Value, path: &str) -> Option<String>`** (~25 LOC):
splits `path` on `.`, skipping leading `.`; indexes into Object/Array at each
segment; returns `v.to_string()` (with quotes stripped for strings).

**Pump wiring change in `execute_step` (`sim/steps.rs`):**

`spawn_pipe_pump` already forwards lines via `Option<mpsc::Sender<String>>`.
For steps with captures, create the `(tx, rx)` pair, pass `tx` to
`spawn_pipe_pump`, pass `rx` to `spawn_capture_reader`. Keep both handles in
`GenericProcess`:

```rust
pub struct GenericProcess {
    pub child:          std::process::Child,
    pub stdout_pump:    Option<thread::JoinHandle<Result<()>>>,
    pub stderr_pump:    Option<thread::JoinHandle<Result<()>>>,
    pub capture_reader: Option<thread::JoinHandle<Result<()>>>,
}
```

For `parser = "json"` (post-exit): wire stdout into a separate accumulator
thread that joins after the child exits, then calls `store.record` for each
capture. The accumulator is still a plain `std::thread`; the capture reader
handle is stored in `GenericProcess.capture_reader`.

**Handling stderr captures:** if a capture spec has `pipe = "stderr"`, wire a
second `(tx2, rx2)` pair to `stderr_pump` and a second capture reader for
those specs only. Most specs use the default `stdout`.

---

### Step 4 — On-demand capture blocking in `execute_step`

**`sim/steps.rs` — `interpolate_with_captures`:**

```rust
fn interpolate_with_captures(
    parts: &[String],
    env: &SimEnv,
    captures: &CaptureStore,
    default_timeout: Duration,
) -> Result<Vec<String>>
```

For each token `part`:
- `$NETSIM_*` or `${binary.*}`: delegate to `env.interpolate_str(part)`.
- `${step_id.capture_name}`: call `captures.wait(key, default_timeout)`.
  Blocks the calling thread (the step loop) via `Condvar::wait_timeout` until
  the pump thread records the value or timeout fires.
- Anything else: pass through.

This is a synchronous blocking call — no `.await` needed. The step loop thread
blocks here while pump threads for already-spawned processes continue running
(they are separate `std::thread`s, so no deadlock).

**`requires` pre-blocking:** at the top of `execute_step`, before the `match
step` dispatch, iterate `step.requires`:

```rust
for key in &step.requires {
    captures.wait(key, default_timeout)
        .with_context(|| format!("step '{}': requires '{}'", step_id, key))?;
}
```

**`gen-file` interpolation:** uses `interpolate_with_captures` on `content`
(single string, not `Vec<String>`). Blocks on any unresolved `${...}`.

**Default timeout:** `Duration::from_secs(300)`. Can be per-step via a
`timeout` field on `UseStep` (or directly in the concrete step's TOML)
propagated through to `Step::Spawn` etc.

---

### Step 5 — `gen-certs` and `gen-file` as Step variants

Both are new match arms in `execute_step` (`sim/steps.rs`).

**`Step::GenCerts { id, device, cn, san }`** (~50 LOC):

```rust
Step::GenCerts { id, device, cn, san } => {
    let ip_str = device.as_deref().map(|dev| {
        let key = format!("$NETSIM_IP_{}", netsim::util::sanitize_for_env_key(dev));
        env.interpolate_str(&key)
    }).transpose()?;
    let ip = ip_str.as_deref().map(|s| s.parse::<IpAddr>()).transpose()?;
    let cn = cn.as_deref().unwrap_or("localhost");
    let mut sans: Vec<String> = san.as_deref()
        .map(|v| v.iter().map(|s| env.interpolate_str(s)).collect::<Result<_>>())
        .transpose()?
        .unwrap_or_default();
    if sans.is_empty() {
        if let Some(ip_s) = &ip_str { sans.push(ip_s.clone()); }
        sans.push("localhost".into());
    }
    // rcgen (reuse generate_self_signed_relay_cert logic, generalised):
    let mut params = rcgen::CertificateParams::new(vec![])?;
    params.distinguished_name.push(rcgen::DnType::CommonName, cn);
    for san_str in &sans {
        if let Ok(ip) = san_str.parse::<IpAddr>() {
            params.subject_alt_names.push(rcgen::SanType::IpAddress(ip));
        } else {
            params.subject_alt_names.push(rcgen::SanType::DnsName(san_str.as_str().try_into()?));
        }
    }
    let key = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    let dir = state.work_dir.join("certs").join(id);
    std::fs::create_dir_all(&dir)?;
    let cert_path = dir.join("cert.pem");
    let key_path  = dir.join("key.pem");
    std::fs::write(&cert_path, cert.pem())?;
    std::fs::write(&key_path,  key.serialize_pem())?;
    for (name, value) in [
        ("cert_pem",      cert.pem()),
        ("key_pem",       key.serialize_pem()),
        ("cert_pem_path", cert_path.display().to_string()),
        ("key_pem_path",  key_path.display().to_string()),
    ] {
        state.captures.record(&format!("{id}.{name}"), value);
    }
}
```

**`Step::GenFile { id, content }`** (~20 LOC):

```rust
Step::GenFile { id, content } => {
    let expanded = interpolate_with_captures(
        &[content.clone()], &state.env, &state.captures, default_timeout)?[0].clone();
    let dir  = state.work_dir.join("files").join(id);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("content");
    std::fs::write(&path, &expanded)?;
    state.captures.record(&format!("{id}.path"), path.display().to_string());
}
```

---

### Step 6 — `[step.results]` collection and extended `evaluate_assert`

**Results collection** (`sim/steps.rs` or `sim/runner.rs`):

After `execute_step` returns, if the completed `Step` has `results: Some(r)`,
look up each field in `state.captures` (non-blocking `get`):

```rust
pub struct StepResultRecord {
    pub id:         String,
    pub duration:   Option<String>,
    pub up_bytes:   Option<String>,
    pub down_bytes: Option<String>,
}
// stored in SimState.step_results: Vec<StepResultRecord>
```

**`results.json` shape** (written by `report.rs`):

```json
{
  "sim": "iroh-direct",
  "captures": {
    "provider.endpoint_id": { "value": "abc123", "history": ["abc123"] },
    "fetcher.conn_type":    { "value": "Direct", "history": ["Relay","Direct"] },
    "fetcher.size":         { "value": "104857600", "history": ["104857600"] }
  },
  "results": [
    { "id": "fetcher", "duration": "12.3", "down_bytes": "104857600" }
  ]
}
```

**Extend `evaluate_assert` in `sim/steps.rs`:**

Replace the current `evaluate_assert` (simple `==`/`!=` only) with a
hand-written tokeniser for the new assert syntax. No new deps.

```rust
fn parse_assert_expr(s: &str) -> Result<AssertExpr>
fn evaluate_assert(state: &SimState, check: &str) -> Result<()>
```

Parse flow:
1. Split on first `.` to get `step_id` and the rest.
2. If rest starts with a capture name followed by ` count `:
   → `AssertLhs::Count { key }`, parse `CountOp` and `N`.
3. Otherwise find `@selector` token (optional), then `op` (`==`, `!=`,
   `contains`, `matches`), then `rhs`.

```rust
#[derive(Debug)]
enum AssertLhs { Value { key: String, selector: Selector }, Count { key: String } }
#[derive(Debug)]
enum Selector  { Last, First, Any, All }
#[derive(Debug)]
enum ValueOp   { Eq, Ne, Contains, Matches }
#[derive(Debug)]
enum CountOp   { Ge, Le, Eq, Ne, Gt, Lt }

struct AssertExpr {
    lhs: AssertLhs,
    op:  EitherOp,
    rhs: String,
}
```

Evaluation reads from `state.captures` (non-blocking `get`/`snapshot`):
- `Value + Last`: `captures.get(key)` → latest value (error if None).
- `Value + First`: `captures.snapshot()[key].history[0]` (error if empty).
- `Value + Any`: any entry in history satisfies op.
- `Value + All`: all entries in history satisfy op.
- `Count`: `history.len()` compared to `N`.

For `Matches`: compile the regex once per eval; use `regex::Regex::is_match`.

Error messages must include: expression string, actual value(s), history
length (for `count` failures). Example:
```
assert FAILED: 'fetcher.conn_type @last contains Direct'
  actual value: "Relay"
  history (2 entries): ["Relay", "Relay"]
```

`Assert` variant now supports both `check` (single) and `checks` (array).
`execute_step` iterates `checks`, merges with `check.iter()`, evaluates all.

---

### Step 7 — Port iroh sim TOML files *(Commit B)*

All sim TOML porting in **one commit**. Order: simplest first (no-relay sims),
then relay sims, then switch-direct. For each file:
1. Add `[[extends]] file = "iroh-defaults.toml"`.
2. Replace the old `kind = "iroh-transfer"` block with:
   - `[[step]] use = "relay-setup"  vars = {device = "relay1"}` (expands to
     gen-certs + gen-file + spawn via the step group)
   - `[[step]] use = "transfer-provider"` with device/id overrides
   - `[[step]] use = "transfer-fetcher"` with requires, device/id overrides
3. Replace existing `assert` check strings with new selector syntax.
4. Remove any inline `[step.results]` — the fetcher template now provides them.

Run the full sim suite after completing all ports to verify no regression.
The old iroh-specific code paths remain alive in Step 7; removal is Step 8.

---

### Step 8 — Remove iroh-specific and iperf-specific code *(Commit C, part 1)*

Single atomic commit after all sims pass with Commit B.

**`sim/steps.rs`** — delete:
- `kind = "iroh-transfer"` branch in `Step::Spawn` match arm
- `maybe_inject_relay_config_path` and its callers
- `ensure_relay_runtime_assets` / `generate_self_signed_relay_cert`
  (functionality replaced by `Step::GenCerts`)
- `RelayRuntimeAssets` struct and `ParserConfig`
- `build_parser_config` / `apply_parser_result` (old iperf path)
- `NETSIM_RUST_LOG` default in `prepare_cmd`
- All `use crate::sim::transfer::*` imports

**`sim/runner.rs`** — delete from `SimState`:
- `.transfers: HashMap<String, TransferHandle>`
- `.results: Vec<TransferResult>`
- `.relay_assets: HashMap<String, RelayRuntimeAssets>`
- `.iperf_results: Vec<IperfResult>`
- `result_field` / `iperf_result_field` functions

**`sim/report.rs`** — delete:
- `TransferResult`, `parse_fetcher_log`, `is_direct_addr`
- Iroh-specific columns in `write_results` / `write_combined_results_for_runs`
- `IperfResult`, `IperfMetrics`, `parse_iperf3_json_log`, `extract_json_object`

Add to `sim/report.rs`:
- `write_captures_table` — generic Markdown table from `captures.snapshot()`
- `write_results_table` — table from `state.step_results`
- Update `results.json` serialisation to new shape (§6 above)

---

### Step 9 — Strip old `Step` fields and variants; delete `sim/transfer.rs` *(Commit C, part 2)*

- In `Step` enum: remove `SwitchRoute` variant (replaced by `SetDefaultRoute`);
  remove old iroh fields from `Spawn` (`kind`, `provider`, `fetcher`,
  `fetchers`, `relay_url`, `fetch_args`, `strategy`, `baseline`).
- Remove the `alias = "action"` compat shim from the tag attribute
  (change `#[serde(tag = "kind", alias = "action")]` to plain
  `#[serde(tag = "kind")]`).
- Delete `sim/transfer.rs`.
- Delete `sim/mod.rs: pub mod transfer;` line.
- Confirm `cargo build` is clean with no dead-code warnings.
