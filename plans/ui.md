# netsim UI Plan

Interactive browser UI for viewing simulation results, logs, timelines and
qlogs. Served from the sim work root (or a run dir) via a future `netsim serve`
command. Built with Vite + React; output is a single `index.html` via
`vite-plugin-singlefile` so it can also be dropped anywhere and opened.

---

## Output layout (as-observed)

```
<work_root>/
  combined-results.json          {runs: [{run, sim, transfers[], iperf[]}]}
  combined-results.md
  latest -> <run-dir>            symlink
  index.html                     <-- UI lives here (embedded by netsim)
  <sim-name>-YYMMDD-HHMMSS/     run dir
    results.json                 {sim, transfers[], iperf[]}
    manifest.json                <-- NEW: log index for UI (see below)
    logs/
      relay.log                  tracing NDJSON
      <step-id>.log              tracing NDJSON or plain text
      xfer/
        provider.log             iroh NDJSON events (ConnectionTypeChanged, etc.)
        provider/                qlog files (*.qlog)
        fetcher-0.log            iroh NDJSON events
        fetcher-0/               qlog files (*.qlog)
          <ts>-<hash>-client.qlog
    report/                      chuck-compat (optional)
    keylog_*.txt
```

### Key JSON schemas

**results.json**
```json
{
  "sim": "iroh-1to1-nat",
  "transfers": [{
    "id": "xfer", "provider": "provider", "fetcher": "fetcher",
    "size_bytes": 521359157, "elapsed_s": 10.02, "mbps": 416.2,
    "final_conn_direct": false, "conn_upgrade": false, "conn_events": 1
  }],
  "iperf": [{
    "id": "...", "device": "...", "bytes": null, "seconds": null,
    "bits_per_second": null, "mbps": null, "retransmits": null,
    "baseline": null, "delta_mbps": null, "delta_pct": null
  }]
}
```

**tracing NDJSON** (relay.log, step logs — emitted by tracing-subscriber json):
```jsonc
{"timestamp":"2026-02-20T11:02:05.334Z","level":"INFO","target":"netsim::sim::transfer",
 "fields":{"message":"iroh-transfer: provider ready","step_id":"xfer","endpoint_id":"ab6d..."}}
{"timestamp":"...","level":"DEBUG","target":"netsim::core",
 "fields":{"message":"netlink: add bridge","bridge":"br-p12680-1"}}
// span open/close also appear:
{"timestamp":"...","level":"TRACE","target":"iroh::_events::conn_type",
 "fields":{"message":"new","conn_type":"Relay","remote":"..."},"span":{"name":"..."}}
```

**iroh event NDJSON** (xfer/fetcher-N.log — written by iroh-transfer binary):
```jsonc
{"kind":"ConnectionTypeChanged","status":"Selected","addr":"Relay(http://r)"}
{"kind":"ConnectionTypeChanged","status":"Selected","addr":"Ip(1.2.3.4:9999)"}
{"kind":"DownloadComplete","size":521359157,"duration":10021468}  // duration µs
```

**combined-results.json**
```json
{"runs": [{"run": "iroh-1to1-nat-260220-142402", "sim": "iroh-1to1-nat",
           "transfers": [...], "iperf": [...]}]}
```

---

## Tech stack

| Concern | Choice |
|---|---|
| Framework | React 18 + TypeScript |
| Build | Vite |
| Single-file output | `vite-plugin-singlefile` (inlines all JS/CSS into one HTML) |
| Styling | Tailwind (or plain CSS modules — keep it small) |
| Timeline/Canvas | `@visx/shape` or plain SVG — no heavy charting lib |
| State | Zustand or plain React context — nothing heavy |

Project lives at `ui/` in the repo root. `npm run build` → `dist/index.html`.
The Rust binary embeds `dist/index.html` at compile time (via `include_str!`)
and writes it to `<work_root>/index.html` after each run (and on `netsim serve`).

---

## UI architecture

Tab layout: **Perf** | **Logs** | **Timeline** | **Qlog**

### Boot sequence

1. `fetch("manifest.json")` in the current dir → discover run structure.
2. `fetch("../combined-results.json")` (if in a run dir) or
   `fetch("combined-results.json")` (if at work root).
3. `fetch("results.json")` for the current run dir.
4. Show whichever tabs have data; grey out empty ones.

URL hash routing (`#perf`, `#logs`, `#timeline`, `#qlog`) so deep-links work
and back/forward navigate between tabs.

---

## Tabs

### 1. Perf

**Summary cards** at top: total transfers, avg mbps, % direct connections.

**Single-run transfers table** (from `results.json`):
- Columns: id | provider | fetcher | size | elapsed_s | mbps | direct | upgrade | events
- All columns sortable (click header toggles asc/desc).
- `direct` = coloured badge (green / grey).

**Single-run iperf table**:
- Columns: id | device | mbps | retransmits | baseline | Δmbps | Δ%
- `Δmbps` / `Δ%` cells colour-coded (green positive, red negative).

**All-runs table** (from `combined-results.json`):
- Columns: run | sim | transfers | avg_mbps | direct_pct
- Click a row → navigate to that run's logs/timeline.
- Filterable by sim name.

**Compare mode**:
- Two run-selectors (dropdowns populated from `combined-results.json`).
- Side-by-side diff table: matched by `(sim, id)`.
- Columns: id | mbps_A | mbps_B | Δmbps | Δ% | direct_A | direct_B
- Rows with regression highlighted red; improvements green.
- Summary line: overall Δavg.

### 2. Logs

Split layout: left sidebar (file tree) + right content pane.

**File tree** (from `manifest.json`):
- Nodes grouped by `node` field (relay, provider, fetcher-0, …).
- Click file → load and display in content pane.
- Badge showing line count once loaded.

**Content pane — tracing NDJSON rendering**:

Parse each line as JSON. If it has `timestamp`, `level`, `target`, `fields`,
render as a formatted log line mimicking the tracing-subscriber `pretty`/compact
format:

```
2026-02-20T11:02:05.334Z  INFO netsim::sim::transfer: iroh-transfer: provider ready
    step_id="xfer" endpoint_id="ab6d..."
```

- Level coloured: ERROR=red, WARN=yellow, INFO=green, DEBUG=blue, TRACE=dim.
- `target` in grey.
- Key=value fields on second line, dimmed.
- Lines that fail JSON parse rendered as plain text (grey).
- If `fields.message` is absent, show all fields inline.

**iroh event NDJSON rendering**:

Lines with `"kind"` field get a special inline badge:
- `ConnectionTypeChanged` + addr contains `Relay` → `🔀 RELAY` badge (grey)
- `ConnectionTypeChanged` + addr contains `Ip(` → `⚡ DIRECT` badge (green)
- `DownloadComplete` → `✓ DONE  {mbps} Mbit/s  {elapsed}s` (blue)

**Filtering**:
- Text box: substring or `/regex/` match against the rendered line text.
- Level filter: checkbox row (ERROR / WARN / INFO / DEBUG / TRACE).
- Quick toggles:
  - "iroh events only" — show only lines where `target` contains `iroh::_events`
    or `kind` is one of the known iroh event kinds.
  - "hide DEBUG/TRACE" shortcut.
- Non-matching lines hidden (not removed from DOM — use CSS `display:none` for
  performance on large files).

**Virtual scrolling**: for files > 5000 lines use a windowed list (react-window
or a simple manual implementation) to keep the DOM small.

### 3. Timeline

Vertical time axis, horizontal lanes per node. Sources overlaid on the same
canvas: tracing logs, iroh NDJSON events, qlog events.

**Layout**:
```
         relay    provider   fetcher-0   fetcher-1
t=0 ──────|──────────|──────────|────────────|────
          │          │          │            │
t=1s      │        [span]       │            │
          │          │         ●ConnType     │
t=2s      │          │          │            │
         ...         │         ●ConnType     │
t=10s     │          │         ★DownloadComplete
```

- Y axis = elapsed time from first event, labelled in seconds.
- One vertical lane per node (derived from `manifest.json` node list).
- Zoom: scroll wheel zooms the time axis; drag to pan.
- Min/max time range selector (range inputs) to focus on a window.

**Event sources**:

| Source | Event type | Visual |
|---|---|---|
| iroh NDJSON | `ConnectionTypeChanged → Relay` | grey circle tick |
| iroh NDJSON | `ConnectionTypeChanged → Direct` | green circle tick |
| iroh NDJSON | `DownloadComplete` | blue star + mbps label |
| tracing NDJSON | `iroh::_events` span enter/exit | coloured horizontal bar (span duration) |
| tracing NDJSON | WARN/ERROR | red triangle on the lane |
| qlog | any event | small diamond (colour by category) |

Iroh `_events` spans: tracing emits `new`/`close` messages in the `iroh::_events`
target. Track open spans by name and draw them as duration bars spanning the time
between `new` and `close` on the relevant node's lane.

**Tooltip**: hover any event → show raw JSON in a floating card.

**Filter panel** (collapsible sidebar):
- Checkboxes to show/hide each event source and type.
- Node visibility toggles.

**Implementation**: SVG (not canvas) — simpler to handle hover/tooltip, fine for
< 10k events. Use React and position SVG elements by computed pixel coords.

### 4. Qlog

Table viewer for QUIC qlog files (JSON-seq or JSON array format).

**File picker**: dropdown of qlog files discovered from `manifest.json`
(`kind: "qlog-dir"` entries are expanded by listing the directory).

**Event table**: virtualized list (react-window).
- Columns: time_ms | category | event_type | summary (first 80 chars of data)
- Sortable by time (default), category, event_type.
- Click row → expand full event JSON in a details panel below.

**Filter**:
- Text box: filter by event_type or data content.
- Category checkboxes: transport, recovery, security, http3, etc.

**Highlights**:
- `transport:packet_received` / `packet_sent` → normal
- `transport:connection_state_updated` → bold, coloured by new state
- `recovery:*` → orange
- `security:key_*` → purple

No full swimlane qlog viz in v1 — navigable table is sufficient. Qlog events
can be toggled into the Timeline via the filter panel (future).

---

## Rust side additions

### `manifest.json` (new, written by `write_results`)

Written to each run dir. Tells the UI exactly what files exist.

```json
{
  "sim": "iroh-1to1-nat",
  "run": "iroh-1to1-nat-260220-142402",
  "started_at": "2026-02-20T14:24:02Z",
  "logs": [
    {"node": "relay",     "path": "logs/relay.log",          "kind": "tracing-ndjson"},
    {"node": "provider",  "path": "logs/xfer/provider.log",  "kind": "iroh-ndjson"},
    {"node": "provider",  "path": "logs/xfer/provider/",     "kind": "qlog-dir"},
    {"node": "fetcher-0", "path": "logs/xfer/fetcher-0.log", "kind": "iroh-ndjson"},
    {"node": "fetcher-0", "path": "logs/xfer/fetcher-0/",    "kind": "qlog-dir"},
    {"node": "step/relay","path": "logs/relay.log",          "kind": "tracing-ndjson"}
  ]
}
```

Log `kind` values: `tracing-ndjson` | `iroh-ndjson` | `qlog-dir` | `text`.

The runner already knows which log files it creates; adding manifest writing to
`write_results` (and the step executor for generic step logs) is small.

### `index.html` embedding

The built `dist/index.html` is embedded in the binary via `include_str!` and
written to `<work_root>/index.html` at the end of each `netsim run`. Later,
`netsim serve [work_root]` starts a minimal HTTP server and opens the browser.

For development: `vite dev` with `NETSIM_WORK_ROOT` env var pointing at a real
work root so the dev server can proxy `fetch()` calls.

---

## Status

### Done ✅
- `ui/` scaffold: Vite + React 18 + TypeScript, `vite-plugin-singlefile` → `dist/index.html` (~175 KB).
- Vite dev plugin: serves `<repo_root>/.netsim-work` by default; `NETSIMS=/path` override; `GET /__netsim/runs` endpoint for run listing; prints resolved workRoot on startup.
- URL hash tab routing (`#perf`, `#logs`, `#timeline`, `#qlog`); `?run=name` run selection; auto-selects newest run in dev mode.
- **Perf tab**: sortable transfers + iperf tables; all-runs overview (click to jump); two-run compare diff (Δmbps + Δ% colour-coded).
- **Logs tab**: ANSI-stripped tracing text rendered as `TIME LEVL target: message` with level colours; iroh NDJSON events with inline badges (⚡ DIRECT / ↔ RELAY / ✓ DONE N Mbit/s); `iroh::_events` lines highlighted amber; sidebar file tree by node; regex + level-toggle + iroh-only filters; 20k-line cap with truncation notice.
- **Timeline tab**: SVG swimlane (Y=time, X=node lanes); iroh NDJSON events (ticks), tracing WARN/ERROR/INFO events, `iroh::_events` open/close spans (duration bars); scroll-to-pan, ctrl+scroll-to-zoom; hover tooltips; per-kind visibility toggles.
- **Qlog tab**: JSON-seq parser (handles record-separator prefix); virtualised event table (time/name/data); regex filter; expand-on-click detail panel; colour by qlog category.
- Manifest fallback: when `manifest.json` absent, infers log paths from `results.json` transfer/provider/fetcher names.

### Rough / known issues ⚠️
- **Timeline time accuracy**: iroh NDJSON events have no timestamps. `ConnectionTypeChanged` events are distributed linearly across `DownloadComplete.duration`; only an approximation. Tracing logs have real timestamps but the relay.log ANSI format parsing may miss multiline continuations.
- **Qlog discovery**: requires `qlog-index.json` per qlog dir (not yet written by Rust). Currently the qlog tab shows no files automatically — user must paste a path manually.
- **Log virtual scroll**: files >20 000 lines are truncated. `react-window` or similar needed for proper virtualisation.
- **Timeline + qlog overlay**: qlog events are not yet rendered in the timeline (wired up but filtered out due to no discovery).

### Next steps (Rust side) ❌
1. **`manifest.json`**: write per run dir from `write_results` (log paths + kinds + `started_at`). Eliminates all heuristic inference in the UI.
2. **`qlog-index.json`**: write per qlog dir listing the actual `*.qlog` filenames so the UI can discover them without a directory listing API.
3. **Embed + serve**: `include_str!("../../ui/dist/index.html")` in `src/main.rs`, write to `<work_root>/index.html` after each run.
4. **`netsim serve [work_root]`**: minimal HTTP server (e.g. `hyper` or `axum`) + browser open; expose `/__netsim/runs` equivalent.

### Next steps (UI side)
- Virtual scrolling in logs (react-window).
- Qlog events overlaid on timeline once discovery works.
- iroh NDJSON: add real timestamps to events upstream (in iroh-transfer binary) so timeline placement is accurate.
- Timeline: align tracing and iroh event time bases using `manifest.started_at`.

## Dev workflow

```bash
cd ui && npm install

# Dev with real data (default: <repo_root>/.netsim-work):
npm run dev

# Override work root:
NETSIMS=/absolute/or/relative/path npm run dev

# Build single-file HTML:
npm run build   # → dist/index.html

# Serve manually (needs HTTP server, not file://):
cp dist/index.html /path/to/.netsim-work/
cd /path/to/.netsim-work && python3 -m http.server 8080
# open http://localhost:8080
```

## Implementation order (remaining Rust work)

1. **Scaffold**: `ui/` with Vite + React + TS + `vite-plugin-singlefile`.
   Basic tab shell, URL hash routing.

2. **Perf tab**: load `combined-results.json` + `results.json`, render sortable
   tables, summary cards. Compare mode with two-run diff.

3. **Rust: manifest.json**: add `write_manifest` to `src/sim/report.rs`,
   call from runner after logs are created.

4. **Logs tab**: file tree from manifest, tracing NDJSON renderer (formatted
   log lines with level colours), iroh NDJSON badges, filter/level controls.
   Virtual scroll for large files.

5. **Timeline tab**: SVG swimlane, Y=time, lanes from manifest nodes.
   iroh NDJSON events first (ConnectionTypeChanged, DownloadComplete).
   Zoom + pan. Tooltip.

6. **Timeline: tracing spans**: parse `iroh::_events` open/close from tracing
   NDJSON, draw duration bars.

7. **Qlog tab**: file picker, virtualized event table, expand-on-click,
   category filter.

8. **Qlog in Timeline**: toggle qlog events as diamonds on the timeline.

9. **Rust: embed + write index.html**: `include_str!` the built HTML, write
   to work root after run.

---

## Notes for implementer

- **Time alignment**: tracing logs have ISO timestamps; iroh NDJSON events have
  no timestamp — derive elapsed from `DownloadComplete.duration` (µs from start).
  The manifest `started_at` field helps anchor absolute time. For the timeline,
  normalise everything to "ms since first event in the run".

- **iroh `_events` spans**: tracing emits `{"fields":{"message":"new"},"target":"iroh::_events::conn_type","span":{"name":"conn_type",...}}` for span enter and `"message":"close"` for exit. Track by span id (if present) or by `(target, name)` to compute duration.

- **qlog format**: iroh emits JSON-seq (one JSON object per line, not an array).
  Parse line by line; skip lines that don't parse (partial writes).

- **Large log files**: don't load everything upfront. For the logs tab, load
  the file and display with virtual scrolling. For the timeline, parse fully
  but only render visible time window.

- **Dev proxy**: add a `vite.config.ts` proxy entry so `fetch("/work/...")` in
  dev mode hits the local filesystem via a simple express middleware or the
  Vite preview server serving the real work root.

- **Build artifact**: `npm run build` outputs `ui/dist/index.html` (single file,
  ~300–500 KB). The Rust build script or a `build.rs` step can run this
  automatically when the `ui/` directory exists.
