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
`[[binary]]`, `[[step-template]]`, and `[[step-group]]` entries. The sim file's
own declarations always win on name collision. Multiple `[[extends]]` blocks
are supported and processed in order.

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
one source field is required.

| Key       | Type   | Description |
|-----------|--------|-------------|
| `name`    | string | Reference key. Used as `${binary.relay}`, `${binary.transfer}`, etc. |
| `path`    | string | Local path. Prefix `target:` to resolve relative to the Cargo target directory (e.g. `target:examples/transfer`). |
| `url`     | string | Download URL. Supports `.tar.gz` archives; the binary is extracted automatically. |
| `repo`    | string | Git repository URL. Must pair with `example` or `bin`. |
| `commit`  | string | Branch, tag, or SHA for `repo` source. Defaults to `"main"`. |
| `example` | string | Build with `cargo --example <name>` from the repo. |
| `bin`     | string | Build with `cargo --bin <name>` from the repo. |

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

The call site's `id`, `device`, `args`, `env`, `requires`, `captures`, and
`results` fields are merged into the template. `args` is appended to the
template's `cmd`. `env` is merged (call site wins on collision). `captures`
is merged (call site wins). `results` replaces entirely if supplied.

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

### `action = "set-link-condition"`

Applies link impairment (rate limit, loss, latency) to a device interface using
`tc netem` and `tc tbf`.

| Key              | Type            | Description |
|------------------|-----------------|-------------|
| `device`         | string          | Target device. |
| `interface`      | string          | Interface name, e.g. `"eth0"`. |
| `link_condition` | string or table | Preset name (`"wifi"`, `"mobile4g"`, etc.) or a custom table: `{ rate = 10000, loss = 0.5, latency = 40 }`. Rate in kbit/s, loss as percentage, latency in ms. |

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
| `cn`     | string          | `"localhost"`               | Certificate Common Name. |
| `san`    | array of strings| `[device_ip, "localhost"]`  | SANs. `$NETSIM_IP_<device>` variables are expanded. |

Output captures: `{id}.cert_pem`, `{id}.key_pem`, `{id}.cert_pem_path`, `{id}.key_pem_path`.

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

| Operator     | Passes when |
|--------------|-------------|
| `== rhs`     | Exact string match. |
| `!= rhs`     | Not an exact match. |
| `contains rhs` | `rhs` is a substring of the capture value. |
| `matches rhs` | `rhs` is a Rust regex that matches the capture value. |

Examples:

```toml
[[step]]
action = "assert"
checks = [
  "fetcher.conn_type contains Direct",
  "fetcher.size matches [0-9]+",
  "iperf-run.bps != 0",
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
| `regex` | string | none       | Regex applied to the raw text line. Group 1 is captured if present, otherwise the full match. Works with all parsers. Cannot be combined with `pick`. |
| `match` | table  | none       | Key=value guards on a parsed JSON object. All keys must match. Requires `pick`. Only valid with `"ndjson"` or `"json"` parser. |
| `pick`  | string | none       | Dot-path into the parsed JSON value, e.g. `".endpoint_id"` or `".end.sum_received.bytes"`. Requires `"ndjson"` or `"json"` parser. Cannot be combined with `regex`. |

With `"ndjson"`, every matching line appends to the capture's history. With
`"json"` or `regex`, the capture is set once from the final matched value.

The latest value is available for interpolation as `${step_id.capture_name}`.

---

## `[results]`

Maps well-known output fields to capture references, so the report can show
normalized throughput comparisons across steps and runs.

```toml
[step.results]
duration   = "iperf-run.seconds"
down_bytes = "iperf-run.bytes"
```

Inside a `[[step-template]]`, the shorthand `.capture_name` (leading dot, no
step ID) refers to the template step's own captures. It gets rewritten to
`{id}.capture_name` when the template is expanded:

```toml
[step-template.results]
duration   = ".duration"    # becomes "fetcher.duration" when id="fetcher"
down_bytes = ".size"
```

| Field       | Type    | Description |
|-------------|---------|-------------|
| `duration`  | float s | Duration of the transfer or test. |
| `up_bytes`  | integer | Bytes sent (upload). |
| `down_bytes`| integer | Bytes received (download). |

Bandwidth (`down_bytes / duration`) is computed in the UI. Unset fields are
omitted from the output.

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

For each sim run, patchbay writes to a timestamped directory under the work root
(default `.patchbay-work/`):

```
.patchbay-work/
  latest/                     # symlink to the most recent run
  <sim-name>-YYMMDD-HHMMSS/
    results.json              # captures and normalized results
    results.md                # human-readable summary table
    nodes/
      <device>/
        stdout.log
        stderr.log
    files/                    # gen-file outputs
      <id>/content
    certs/                    # gen-certs outputs
      <id>/cert.pem
      <id>/key.pem
  combined-results.json       # aggregated across all runs in the work root
  combined-results.md
```

`results.json` structure:

```json
{
  "sim": "iroh-1to1-nat",
  "captures": {
    "fetcher.conn_type": { "value": "Direct", "history": ["Relay", "Direct"] },
    "fetcher.size":      { "value": "104857600", "history": ["104857600"] }
  },
  "results": [
    { "id": "fetcher", "duration": "12.3", "down_bytes": "104857600" }
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

# Multiple devices of the same name (count expansion)
[device.fetcher]
count = 10

[device.fetcher.eth0]
gateway = "dc"
```

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
latency = { us-east = "80ms", eu-central = "140ms" }
```

Attach a router to a region with `region = "us-west"` in the `[[router]]` table.

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
