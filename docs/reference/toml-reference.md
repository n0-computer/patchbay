# Sim TOML Reference

A simulation is defined by one TOML file. That file describes what topology to
use, what binaries to run, and the sequence of steps to execute. This page
covers every field.

---

## File layout

```
[[extends]]          # optional: inherit from a shared defaults file
file = "..."

[sim]                # simulation metadata
name     = "..."
topology = "..."

[[binary]]           # optional: binary definitions (repeatable)
...

[[prepare]]          # optional: prebuild configuration (repeatable)
...

[[step-template]]    # optional: reusable single-step templates (repeatable)
...

[[step-group]]       # optional: reusable multi-step groups (repeatable)
...

[[step]]             # the actual steps to execute (repeatable)
...
```

Inline topology tables (`[[router]]`, `[device.*]`, `[region.*]`) can also
appear directly in the sim file instead of referencing an external topology.

---

## `[[extends]]`

Pulls in definitions from another TOML file. The loaded file can contribute
`[[binary]]`, `[[prepare]]`, `[[step-template]]`, and `[[step-group]]` entries.
The sim file's own declarations always win on name collision. Multiple
`[[extends]]` blocks are supported and processed in order.

| Key    | Type   | Description |
|--------|--------|-------------|
| `file` | string | Path to the shared file. Searched relative to the sim file, then one directory up, then the working directory. |

Example:

```toml
[[extends]]
file = "iroh-defaults.toml"
```

---

## `[sim]`

| Key        | Type   | Description |
|------------|--------|-------------|
| `name`     | string | Identifier used in output filenames and the report header. |
| `topology` | string | Name of a topology file to load from the `topos/` directory next to the sim file. Overrides any topology from `[[extends]]`. |

---

## `[[binary]]`

Declares a named binary that steps can reference as `${binary.<name>}`. Exactly
one source field is required (or `mode` can be set explicitly).

| Key            | Type     | Description |
|----------------|----------|-------------|
| `name`         | string   | Reference key. Used as `${binary.relay}`, `${binary.transfer}`, etc. |
| `mode`         | string   | Source mode: `"path"`, `"fetch"`, `"build"`, or `"target"`. Inferred from other fields when omitted. |
| `path`         | string   | Local path to a prebuilt binary or source directory. Prefix `target:` to resolve relative to the Cargo target directory. |
| `url`          | string   | Download URL. Supports `.tar.gz` archives; the binary is extracted automatically. |
| `repo`         | string   | Git repository URL. Must pair with `example` or `bin`. |
| `commit`       | string   | Branch, tag, or SHA for `repo` source. Defaults to `"main"`. |
| `example`      | string   | Build with `cargo --example <name>`. Works with `repo` (build mode) or `mode = "target"`. |
| `bin`          | string   | Build with `cargo --bin <name>`. Works with `repo` (build mode) or `mode = "target"`. |
| `features`     | array    | Cargo feature list to enable when building. |
| `all-features` | boolean  | Build with `--all-features`. |

**Mode inference:** When `mode` is omitted, it is inferred: `path` → `"path"`;
`url` → `"fetch"`; `repo`, `example`, or `bin` → `"build"`. Use
`mode = "target"` explicitly to reference a pre-built artifact in the Cargo target
directory by `example` or `bin` name (skips building).

---

## `[[prepare]]`

Declares binaries to prebuild from the project workspace before execution.
Multiple entries are supported; each produces release-mode artifacts.

| Key            | Type     | Description |
|----------------|----------|-------------|
| `mode`         | string   | Preparation mode. Currently only `"build"` (the default). |
| `examples`     | array    | Example names to build with `cargo build --example`. |
| `bins`         | array    | Binary names to build with `cargo build --bin`. |
| `features`     | array    | Cargo feature list to enable. |
| `all-features` | boolean  | Build with `--all-features`. |

---

## `[[step-template]]`

A named, reusable step definition. Contains the same fields as a `[[step]]`
plus a `name`. Referenced with `use = "<name>"` in a step; the call-site fields
are merged on top before the step executes.

```toml
[[step-template]]
name   = "transfer-fetcher"
action = "spawn"
parser = "ndjson"
cmd    = ["${binary.transfer}", "--output", "json", "fetch"]
[step-template.captures.size]
match = { kind = "DownloadComplete" }
pick  = ".size"
[step-template.results]
down_bytes = ".size"
```

Call site:

```toml
[[step]]
use    = "transfer-fetcher"
id     = "fetcher"
device = "fetcher"
args   = ["${provider.endpoint_id}"]
```

The call site's `id`, `device`, `timeout`, `args`, `env`, `requires`,
`captures`, and `results` fields are merged into the template. `args` is
appended to the template's `cmd`. `env` is merged (call site wins on
collision). `captures` is merged (call site wins). `results` replaces entirely
if supplied.

---

## `[[step-group]]`

A named sequence of steps that expands inline wherever `use = "<group-name>"`
appears. Groups support variable substitution for parameterization.

| Key                   | Type   | Description |
|-----------------------|--------|-------------|
| `name`                | string | Group identifier. |
| `[[step-group.step]]` | array  | Ordered step definitions. |

The call site uses a `[[step]]` with `use` and `vars`:

```toml
[[step]]
use  = "relay-setup"
vars = { device = "relay" }
```

Inside group steps, `${group.<key>}` is substituted with the caller-supplied
value before the steps execute. This substitution happens at expansion time
(before runtime), so a two-stage pattern is used for nested references:

```toml
# In the group step:
content = "cert_path = \"${${group.device}-cert.cert_pem_path}\""
# After group expansion (e.g. device="relay"):
#   -> cert_path = "${relay-cert.cert_pem_path}"
# Then resolved at runtime as a capture reference.
```

Group steps can themselves use `use = "<step-template-name>"` to inherit from
a template. Groups cannot nest other groups.

---

## `[[step]]`

### Common fields

These fields apply to most or all step types.

| Key       | Type            | Description |
|-----------|-----------------|-------------|
| `action`  | string          | Step type. See the sections below for valid values. Defaults to `"run"` when `cmd` is present. |
| `id`      | string          | Step identifier. Required for `spawn`, `gen-certs`, `gen-file`. Referenced as `${id.capture_name}` in later steps. |
| `use`     | string          | Template or group name. When referencing a group, only `vars` is used from this entry; all other fields come from the group. |
| `vars`    | table           | Group substitution variables. Only meaningful when `use` references a `[[step-group]]`. |
| `device`  | string          | Name of the network namespace to run the command in. |
| `env`     | table           | Extra environment variables, merged with any template `env`. |
| `requires`| array of strings| Capture keys to wait for before this step starts. Format: `"step_id.capture_name"`. Blocks until all are resolved. |

### Counted device expansion

When a step targets a device that has `count > 1` in the topology, the step is
automatically expanded into N copies. Each copy's `device` and `id` fields are
suffixed with `-0`, `-1`, ..., `-N-1`. For example, a step with `device = "peer"`
against a topology with `[device.peer] count = 3` produces three steps targeting
`peer-0`, `peer-1`, and `peer-2`.

`wait-for` steps are similarly expanded when their `id` matches a counted device
name.

---

### `action = "run"`

Runs a command and waits for it to exit before moving to the next step.

| Key        | Type    | Default  | Description |
|------------|---------|----------|-------------|
| `cmd`      | array   | required | Command and arguments. Supports `${binary.<n>}`, `$NETSIM_IP_<device>`, `${id.capture}`. |
| `args`     | array   | none     | Appended to the template's `cmd`. Does not replace it. |
| `parser`   | string  | `"text"` | Output parser. See [parsers](#parsers). |
| `captures` | table   | none     | Named captures. See [`[captures]`](#captures). |
| `results`  | table   | none     | Normalized result fields. See [`[results]`](#results). |

---

### `action = "spawn"`

Starts a process in the background. A later `wait-for` step waits for it to exit.

| Key           | Type    | Default  | Description |
|---------------|---------|----------|-------------|
| `cmd`         | array   | required | Command and arguments. |
| `args`        | array   | none     | Appended to the template's `cmd`. |
| `parser`      | string  | `"text"` | Output parser. See [parsers](#parsers). |
| `ready_after` | duration| none     | How long to wait after spawning before the next step runs. Useful when a process needs startup time but doesn't print a ready signal. |
| `captures`    | table   | none     | Named captures. See [`[captures]`](#captures). |
| `results`     | table   | none     | Normalized result fields. Collected when the process exits. |

---

### `action = "wait-for"`

Waits for a spawned process to exit. Collects its captures and results.

| Key       | Type     | Default   | Description |
|-----------|----------|-----------|-------------|
| `id`      | string   | required  | ID of a previously spawned step. |
| `timeout` | duration | `"300s"`  | How long to wait before failing. |

---

### `action = "wait"`

Sleeps for a fixed duration.

| Key        | Type     | Description |
|------------|----------|-------------|
| `duration` | duration | Required. How long to sleep. |

---

### `action = "set-link-condition"` (alias `"set-impair"`)

Applies link impairment (rate limit, loss, latency) to a device interface using
`tc netem` and `tc tbf`. Pass `null` / omit `condition` to clear impairment.

| Key         | Type            | Description |
|-------------|-----------------|-------------|
| `device`    | string          | Target device. |
| `interface` | string          | Interface name (e.g. `"eth0"`). Defaults to the device's first interface. |
| `condition` | string or table | Preset name or a custom table. See [link conditions](#link-conditions). Alias: `impair`. |

---

### `action = "link-down"` / `action = "link-up"`

Brings a device interface up or down.

| Key         | Type   | Description |
|-------------|--------|-------------|
| `device`    | string | Target device. |
| `interface` | string | Interface name. |

---

### `action = "set-default-route"`

Switches the default route on a device to a given interface. Useful for
simulating path changes.

| Key      | Type   | Description |
|----------|--------|-------------|
| `device` | string | Target device. |
| `to`     | string | Interface to set as the new default route. |

---

### `action = "gen-certs"`

Generates a self-signed TLS certificate and key using `rcgen`. The outputs are
written to `{work_dir}/certs/{id}/` and also stored as captures.

| Key      | Type            | Default                     | Description |
|----------|-----------------|-----------------------------|-------------|
| `id`     | string          | required                    | Step ID, prefixes the output captures. |
| `device` | string          | none                        | Device whose IP is automatically added to the Subject Alternative Names. |
| `cn`     | string          | `"patchbay"`                | Certificate Common Name. |
| `san`    | array of strings| `[device_ip]`               | Additional SANs. `$NETSIM_IP_<device>` variables are expanded. Values that parse as IP addresses become IP SANs; others become DNS SANs. |

Output captures: `{id}.cert_pem_path`, `{id}.key_pem_path`.

---

### `action = "gen-file"`

Writes an interpolated string to disk and records the path as a capture.
Useful for generating config files that reference captures from earlier steps.

| Key       | Type   | Description |
|-----------|--------|-------------|
| `id`      | string | Required. |
| `content` | string | Required. `${...}` tokens are interpolated; blocks on unresolved capture references. |

Output capture: `{id}.path`.

The file is written to `{work_dir}/files/{id}/content`.

---

### `action = "assert"`

Checks one or more assertion expressions. All must pass; the sim fails on the
first that doesn't.

| Key      | Type            | Description |
|----------|-----------------|-------------|
| `check`  | string          | Single assertion expression. |
| `checks` | array of strings| Multiple expressions; equivalent to multiple `check` fields. |

**Expression syntax:**

```
step_id.capture_name operator rhs
```

The LHS must be a capture key in the form `step_id.capture_name`. The value
used is the most recent one recorded for that capture.

| Operator       | Passes when |
|----------------|-------------|
| `== rhs`       | Exact string match. |
| `!= rhs`       | Not an exact match. |
| `contains rhs` | `rhs` is a substring of the capture value. |
| `matches rhs`  | `rhs` is a Rust regex that matches the capture value. |
| `>= rhs`       | Both sides parsed as numbers; LHS is greater or equal. |

Examples:

```toml
[[step]]
action = "assert"
checks = [
  "fetcher.conn_type contains Direct",
  "fetcher.size matches [0-9]+",
  "iperf-run.bps != 0",
  "ping-check.avg_rtt >= 50",
]
```

---

## Parsers

Set on `run` or `spawn` steps with `parser = "..."`.

| Value     | When it fires            | What it can do |
|-----------|--------------------------|----------------|
| `"text"`  | Streaming, per line      | `regex` captures only. |
| `"ndjson"`| Streaming, per line      | `regex` captures, plus `match`/`pick` on JSON lines. |
| `"json"`  | After process exits      | `pick` on the single JSON document. No per-line matching. |

---

## `[captures]`

Defined as sub-tables of a `run` or `spawn` step:

```toml
[[step]]
action = "run"
id     = "iperf"
parser = "json"
cmd    = ["iperf3", "-J", ...]
[step.captures.bytes]
pick = ".end.sum_received.bytes"
[step.captures.seconds]
pick = ".end.sum_received.seconds"
```

Or on a template:

```toml
[[step-template]]
name = "transfer-provider"
...
[step-template.captures.endpoint_id]
match = { kind = "EndpointBound" }
pick  = ".endpoint_id"
```

| Key     | Type   | Default    | Description |
|---------|--------|------------|-------------|
| `pipe`  | string | `"stdout"` | Which output stream to read: `"stdout"` or `"stderr"`. |
| `regex` | string | none       | Regex applied to the raw text line. Group 1 is captured if present, otherwise the full match. Works with all parsers. |
| `match` | table  | none       | Key=value guards on a parsed JSON object. All keys must match. Requires `pick`. Only valid with `"ndjson"` or `"json"` parser. |
| `pick`  | string | none       | Dot-path into the parsed JSON value, e.g. `".endpoint_id"` or `".end.sum_received.bytes"`. Requires `"ndjson"` or `"json"` parser. |

With `"ndjson"`, every matching line updates the capture value. With `"json"`,
the capture is set once from the parsed document. With `"text"`, only `regex`
matching is available.

The latest value is available for interpolation as `${step_id.capture_name}`.

---

## `[results]`

Maps well-known output fields to capture references, so the report and UI can
show normalized throughput and latency comparisons across steps and runs.

```toml
[step.results]
duration   = "iperf-run.seconds"
down_bytes = "iperf-run.bytes"
latency_ms = "ping-check.avg_rtt"
```

Inside a `[[step-template]]`, the shorthand `.capture_name` (leading dot, no
step ID) refers to the template step's own captures. It gets rewritten to
`{id}.capture_name` when the template is expanded:

```toml
[step-template.results]
duration   = ".duration"    # becomes "fetcher.duration" when id="fetcher"
down_bytes = ".size"
```

| Field        | Type    | Description |
|--------------|---------|-------------|
| `duration`   | string  | Capture key for the duration of the transfer or test (microseconds as integer, or seconds as float). |
| `up_bytes`   | string  | Capture key for bytes sent (upload). |
| `down_bytes` | string  | Capture key for bytes received (download). |
| `latency_ms` | string  | Capture key for round-trip or one-way latency in milliseconds. |

Throughput (`down_bytes / duration`) is computed in the UI. Unset fields are
omitted from the output.

---

## Link conditions

Used by the `set-link-condition` step (`condition` / `impair` field) and by
device interface `impair` fields in the topology.

**Presets:**

| Value           | Latency | Jitter | Loss   | Rate limit |
|-----------------|---------|--------|--------|------------|
| `"lan"`         | 0 ms    | 0 ms   | 0 %    | unlimited  |
| `"wifi"`        | 5 ms    | 2 ms   | 0.1 %  | unlimited  |
| `"wifi-bad"`    | 40 ms   | 15 ms  | 2 %    | 20 Mbit    |
| `"mobile-4g"`   | 25 ms   | 8 ms   | 0.5 %  | unlimited  |
| `"mobile-3g"`   | 100 ms  | 30 ms  | 2 %    | 2 Mbit     |
| `"satellite"`   | 40 ms   | 7 ms   | 1 %    | unlimited  |
| `"satellite-geo"` | 300 ms | 20 ms | 0.5 %  | 25 Mbit   |

**Custom table:**

```toml
impair = { latency_ms = 100, jitter_ms = 10, loss_pct = 0.5, rate_kbit = 10000 }
```

| Field          | Type   | Default | Description |
|----------------|--------|---------|-------------|
| `rate_kbit`    | u32    | 0       | Rate limit in kbit/s (0 = unlimited). |
| `loss_pct`     | f32    | 0.0     | Packet loss percentage (0.0–100.0). |
| `latency_ms`   | u32    | 0       | One-way latency in milliseconds. |
| `jitter_ms`    | u32    | 0       | Jitter in milliseconds (uniform ±jitter around latency). |
| `reorder_pct`  | f32    | 0.0     | Packet reordering percentage. |
| `duplicate_pct`| f32    | 0.0     | Packet duplication percentage. |
| `corrupt_pct`  | f32    | 0.0     | Bit-error corruption percentage. |

---

## Variable interpolation

Supported in `cmd`, `args`, `env` values, `content` (gen-file), and `san`
(gen-certs).

| Pattern                      | Resolves to |
|------------------------------|-------------|
| `${binary.<name>}`           | Resolved filesystem path to the named binary. |
| `$NETSIM_IP_<DEVICE>`        | IP address of the device (name uppercased, non-alphanumeric characters replaced with `_`). |
| `${step_id.capture_name}`    | Latest value of the named capture. Blocks until the capture resolves. |

---

## Duration format

Durations are strings of the form `"<n>s"`, `"<n>ms"`, or `"<n>m"`.

Examples: `"30s"`, `"500ms"`, `"2m"`, `"300s"`.

---

## Output files

For each invocation of `patchbay run`, a timestamped run directory is created
under the work root (default `.patchbay-work/`):

```
.patchbay-work/
  latest -> sim-YYMMDD-HHMMSS  # symlink to most recent run
  sim-YYMMDD-HHMMSS/           # run root
    manifest.json               # run-level metadata and sim summaries
    progress.json               # live progress (updated during execution)
    combined-results.json       # aggregated results across all sims
    combined-results.md         # human-readable combined summary
    <sim-name>/                 # per-sim subdirectory
      sim.json                  # sim-level summary (status, setup, errors)
      results.json              # captures and normalized results
      results.md                # human-readable results table
      events.jsonl              # lab lifecycle events
      nodes/
        <device>/
          stdout.log
          stderr.log
      files/                    # gen-file outputs
        <id>/content
      certs/                    # gen-certs outputs
        <id>/cert.pem
        <id>/key.pem
```

`results.json` structure:

```json
{
  "sim": "iperf-baseline",
  "steps": [
    {
      "id": "iperf-run",
      "duration": "10.05",
      "down_bytes": "1234567890",
      "latency_ms": null,
      "up_bytes": null
    }
  ]
}
```

---

## Topology files

A topology file (in `topos/`) defines the network graph: routers with optional
NAT, and devices with their interfaces and gateways.

```toml
# A datacenter router (no NAT)
[[router]]
name = "dc"

# A home NAT router (endpoint-independent mapping, port-restricted filtering)
[[router]]
name = "lan-client"
nat  = "home"

# A device with one interface behind the DC router
[device.server.eth0]
gateway = "dc"

# A device behind the NAT router
[device.client.eth0]
gateway = "lan-client"

# A device with initial link impairment
[device.sender.eth0]
gateway = "dc"
impair  = { latency_ms = 100 }

# Multiple devices of the same name (count expansion)
[device.fetcher]
count = 10

[device.fetcher.eth0]
gateway = "dc"
```

**Device interface fields:**

| Key       | Type            | Description |
|-----------|-----------------|-------------|
| `gateway` | string          | Required. Name of the upstream router. |
| `impair`  | string or table | Initial link impairment. Accepts the same values as [link conditions](#link-conditions). Applied after network setup. |

**Device-level fields:**

| Key     | Type    | Default | Description |
|---------|---------|---------|-------------|
| `count` | integer | 1       | Number of instances. Creates `{name}-0` through `{name}-{N-1}`. Steps targeting the base name are automatically expanded. |

**NAT modes:**

| Value          | Behavior |
|----------------|----------|
| (absent)       | No NAT; device has a public IP on the upstream network. |
| `"home"`       | EIM+APDF: same external port for all destinations (port-restricted cone). |
| `"corporate"`  | EDM+APDF: different port per destination (symmetric NAT). |
| `"cgnat"`      | EIM+EIF: carrier-grade NAT, stacks with home NAT. |
| `"cloud-nat"`  | EDM+APDF: symmetric NAT with longer timeouts (AWS/Azure/GCP). |
| `"full-cone"`  | EIM+EIF: any host can reach the mapped port. |

**Region latency** can be added to introduce inter-router delays:

```toml
[region.us-west]
latencies = { us-east = 80, eu-central = 140 }
```

Values are one-way latency in milliseconds. Attach a router to a region with
`region = "us-west"` in the `[[router]]` table.

---

## Example: minimal iperf sim

```toml
[sim]
name     = "iperf-baseline"
topology = "1to1-public"

[[step]]
action      = "spawn"
id          = "iperf-server"
device      = "provider"
cmd         = ["iperf3", "-s", "-1"]
ready_after = "1s"

[[step]]
action = "run"
id     = "iperf-run"
device = "fetcher"
parser = "json"
cmd    = ["iperf3", "-c", "$NETSIM_IP_provider", "-t", "10", "-J"]
[step.captures.bytes]
pick = ".end.sum_received.bytes"
[step.captures.seconds]
pick = ".end.sum_received.seconds"
[step.results]
duration   = "iperf-run.seconds"
down_bytes = "iperf-run.bytes"

[[step]]
action = "wait-for"
id     = "iperf-server"

[[step]]
action = "assert"
checks = [
  "iperf-run.bytes matches [0-9]+",
]
```

---

## Example: ping with latency capture

```toml
[sim]
name = "ping-latency"

[[router]]
name = "dc"

[device.sender.eth0]
gateway = "dc"
impair  = { latency_ms = 100 }

[device.receiver.eth0]
gateway = "dc"

[[step]]
action = "run"
id     = "ping-check"
device = "sender"
cmd    = ["ping", "-c", "3", "$NETSIM_IP_receiver"]
parser = "text"

[step.captures.avg_rtt]
pipe  = "stdout"
regex = "rtt min/avg/max/mdev = [\\d.]+/([\\d.]+)/"

[step.results]
latency_ms = "ping-check.avg_rtt"
```

---

## Example: iroh transfer with relay (NAT topology)

This uses templates and a step group defined in `iroh-defaults.toml`.

```toml
[[extends]]
file = "iroh-defaults.toml"

[sim]
name     = "iroh-1to1-nat"
topology = "1to1-nat"

# Expands to: gen-certs -> gen-file (relay config) -> spawn relay
[[step]]
use  = "relay-setup"
vars = { device = "relay" }

[[step]]
use      = "transfer-provider"
id       = "provider"
device   = "provider"
requires = ["relay.ready"]
args     = ["--relay-url", "https://$NETSIM_IP_relay:3340"]

[[step]]
use    = "transfer-fetcher"
id     = "fetcher"
device = "fetcher"
args   = ["${provider.endpoint_id}",
          "--relay-url",        "https://$NETSIM_IP_relay:3340",
          "--remote-relay-url", "https://$NETSIM_IP_relay:3340"]

[[step]]
action  = "wait-for"
id      = "fetcher"
timeout = "45s"

[[step]]
action = "assert"
checks = [
  "fetcher.size matches [0-9]+",
]
```

The `relay-setup` group (from `iroh-defaults.toml`) runs `gen-certs`, writes a
relay config file with `gen-file`, and spawns the relay binary. The relay step
captures a `ready` signal from stderr; provider uses `requires = ["relay.ready"]`
to block until it fires.
