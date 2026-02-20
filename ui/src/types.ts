export interface TransferResult {
  id: string
  provider: string
  fetcher: string
  size_bytes?: number
  elapsed_s?: number
  mbps?: number
  final_conn_direct?: boolean
  conn_upgrade?: boolean
  conn_events: number
}

export interface IperfResult {
  id: string
  device: string
  bytes?: number
  seconds?: number
  bits_per_second?: number
  mbps?: number
  retransmits?: number
  baseline?: string
  delta_mbps?: number
  delta_pct?: number
}

export interface RunResults {
  run: string
  sim_dir?: string
  sim: string
  transfers: TransferResult[]
  iperf: IperfResult[]
}

export interface CombinedResults {
  runs: RunResults[]
}

export interface SimResults {
  sim: string
  transfers: TransferResult[]
  iperf: IperfResult[]
}

// manifest.json — written by netsim into each run dir
export type LogKind = 'tracing-ansi' | 'iroh-ndjson' | 'qlog-dir' | 'text'

export interface ManifestLog {
  node: string
  path: string
  kind: LogKind
}

export interface Manifest {
  run: string
  status?: string
  started_at?: string
  ended_at?: string | null
  runtime_ms?: number | null
  success?: boolean | null
  simulations?: Array<{
    sim: string
    sim_dir: string
    status: string
    runtime_ms?: number | null
    sim_json?: string | null
  }>
  sim?: string
  logs: ManifestLog[]
}

export interface ProgressSim {
  sim: string
  status: string
  sim_dir?: string | null
  runtime_ms?: number | null
  sim_json?: string | null
}

export interface RunProgress {
  run: string
  status: 'running' | 'done' | string
  started_at: string
  updated_at: string
  total: number
  completed: number
  ok: number
  error: number
  current_sim?: string | null
  simulations: ProgressSim[]
}

// ── parsed log line types ─────────────────────────────────────────────────────

export type LogLevel = 'ERROR' | 'WARN' | 'INFO' | 'DEBUG' | 'TRACE'

/** One parsed entry from a tracing-formatted text log (plain or ANSI) */
export interface TracingEntry {
  type: 'tracing'
  raw: string
  timestamp: string
  level: LogLevel
  target: string
  message: string
}

/** One event from iroh NDJSON log (kind-tagged) */
export interface IrohEvent {
  type: 'iroh'
  raw: string
  kind: string
  [key: string]: unknown
}

/** Unparseable line — render verbatim */
export interface RawLine {
  type: 'raw'
  raw: string
}

export type LogLine = TracingEntry | IrohEvent | RawLine

// ── qlog ─────────────────────────────────────────────────────────────────────

export interface QlogEvent {
  time: number   // ms relative to connection start
  name: string
  data: Record<string, unknown>
  tuple?: string
}

export interface QlogFile {
  file_schema?: string
  trace?: {
    title?: string
    vantage_point?: { type: string }
    events?: QlogEvent[]
  }
}
