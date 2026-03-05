export interface StepResult {
  id: string
  duration?: string
  down_bytes?: string
  up_bytes?: string
  latency_ms?: string
}

export interface SimResults {
  sim: string
  steps: StepResult[]
}

export type LogKind =
  | 'tracing_jsonl'
  | 'lab_events'
  | 'jsonl'
  | 'json'
  | 'qlog'
  | 'ansi_text'
  | 'text'

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
  setup?: {
    routers?: number
    devices?: number
    regions?: number
    steps?: number
  }
  logs: SimLogEntry[]
  error_line?: string | null
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
  error?: string | null
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
  error?: string | null
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
  steps: StepResult[]
}

export interface CombinedResults {
  runs: CombinedRunResult[]
}
