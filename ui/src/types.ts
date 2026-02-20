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

export interface SimResults {
  sim: string
  transfers: TransferResult[]
  iperf: IperfResult[]
}

export type LogKind = 'transfer' | 'text' | 'qlog'

export interface SimLogEntry {
  node: string
  kind: LogKind | string
  path: string
}

export interface SimSummary {
  sim: string
  sim_dir: string
  status: 'ok' | 'error' | string
  started_at: string
  ended_at: string
  runtime_ms: number
  logs: SimLogEntry[]
  error?: {
    phase?: string
    message?: string
  } | null
}

export interface ManifestSimSummary {
  sim: string
  sim_dir: string
  status: string
  runtime_ms?: number | null
  sim_json?: string | null
}

export interface RunManifest {
  run: string
  status?: 'running' | 'done' | string
  started_at?: string
  ended_at?: string | null
  runtime_ms?: number | null
  success?: boolean | null
  simulations: ManifestSimSummary[]
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

export interface RunIndex {
  workRoot: string
  runs: string[]
}

export interface CombinedRunResult {
  run: string
  sim_dir?: string
  sim: string
  transfers: TransferResult[]
  iperf: IperfResult[]
}

export interface CombinedResults {
  runs: CombinedRunResult[]
}
