import { useCallback, useEffect, useMemo, useState } from 'react'
import LogsTab from './components/LogsTab'
import PerfTab from './components/PerfTab'
import TimelineTab from './components/TimelineTab'
import type {
  CombinedResults,
  IperfResult,
  RunIndex,
  RunManifest,
  RunProgress,
  SimResults,
  SimSummary,
  TransferResult,
} from './types'

type Tab = 'perf' | 'logs' | 'timeline'
type SelectedItem = 'overview' | string

type NodeThroughput = { node: string; up: number; down: number }
type SimOverviewRow = {
  sim: string
  sim_dir: string
  status: string
  error: string | null
  nodes: number | null
  up: number | null
  down: number | null
}

async function fetchJson<T>(url: string): Promise<T | null> {
  try {
    const res = await fetch(url)
    if (!res.ok) return null
    return await res.json() as T
  } catch {
    return null
  }
}

function baseForRun(run: string | null): string {
  return run ? `./${run}/` : './'
}

function sum(nums: number[]): number {
  return nums.reduce((acc, v) => acc + v, 0)
}

function avg(nums: number[]): number | null {
  return nums.length ? sum(nums) / nums.length : null
}

function transferNodeThroughput(transfers: TransferResult[]): NodeThroughput[] {
  const byNode = new Map<string, NodeThroughput>()
  for (const transfer of transfers) {
    const mbps = transfer.mbps ?? 0
    if (!byNode.has(transfer.provider)) {
      byNode.set(transfer.provider, { node: transfer.provider, up: 0, down: 0 })
    }
    if (!byNode.has(transfer.fetcher)) {
      byNode.set(transfer.fetcher, { node: transfer.fetcher, up: 0, down: 0 })
    }
    byNode.get(transfer.provider)!.up += mbps
    byNode.get(transfer.fetcher)!.down += mbps
  }
  return [...byNode.values()].sort((a, b) => a.node.localeCompare(b.node))
}

function throughputFromTransfersOrIperf(transfers: TransferResult[], iperf: IperfResult[]) {
  const nodeRows = transferNodeThroughput(transfers)
  if (nodeRows.length > 0) {
    return {
      up: sum(nodeRows.map((n) => n.up)),
      down: sum(nodeRows.map((n) => n.down)),
    }
  }
  const iperfMbps = iperf.map((row) => row.mbps).filter((v): v is number => v != null)
  const mean = avg(iperfMbps)
  return { up: mean, down: mean }
}

function nodeCount(summary: SimSummary | null, transfers: TransferResult[], iperf: IperfResult[]): number | null {
  if (summary?.setup) {
    const routers = summary.setup.routers ?? 0
    const devices = summary.setup.devices ?? 0
    if (routers + devices > 0) {
      return routers + devices
    }
  }
  if (summary?.logs?.length) {
    return new Set(summary.logs.map((l) => l.node)).size
  }
  const inferred = new Set<string>()
  for (const row of transfers) {
    inferred.add(row.provider)
    inferred.add(row.fetcher)
  }
  for (const row of iperf) {
    inferred.add(row.device)
  }
  return inferred.size > 0 ? inferred.size : null
}

function statusForSim(simDir: string, manifest: RunManifest | null, progress: RunProgress | null, summary: SimSummary | null): string {
  const p = progress?.simulations.find((s) => s.sim_dir === simDir)
  if (p?.status) return p.status
  const m = manifest?.simulations.find((s) => s.sim_dir === simDir)
  if (m?.status) return m.status
  if (summary?.status) return summary.status
  return 'pending'
}

function errorForSim(simDir: string, manifest: RunManifest | null, progress: RunProgress | null, summary: SimSummary | null): string | null {
  const p = progress?.simulations.find((s) => s.sim_dir === simDir)
  if (p?.error) return p.error
  const m = manifest?.simulations.find((s) => s.sim_dir === simDir)
  if (m?.error) return m.error
  return summary?.error_line ?? null
}

function shortText(v: string | null, max = 120): string {
  if (!v) return ''
  return v.length > max ? `${v.slice(0, max - 1)}…` : v
}

function simNameForDir(simDir: string, manifest: RunManifest | null, progress: RunProgress | null, summary: SimSummary | null, combined: CombinedResults | null): string {
  const p = progress?.simulations.find((s) => s.sim_dir === simDir)
  if (p?.sim) return p.sim
  const m = manifest?.simulations.find((s) => s.sim_dir === simDir)
  if (m?.sim) return m.sim
  const c = combined?.runs.find((s) => s.sim_dir === simDir)
  if (c?.sim) return c.sim
  if (summary?.sim) return summary.sim
  return simDir
}

function fmt(v: number | null): string {
  return v == null ? '—' : v.toFixed(2)
}

export default function App() {
  const [runs, setRuns] = useState<string[]>([])
  const [workRoot, setWorkRoot] = useState('')
  const [selectedRun, setSelectedRun] = useState<string | null>(null)
  const [selectedItem, setSelectedItem] = useState<SelectedItem>('overview')
  const [tab, setTab] = useState<Tab>('perf')

  const [manifest, setManifest] = useState<RunManifest | null>(null)
  const [progress, setProgress] = useState<RunProgress | null>(null)
  const [combined, setCombined] = useState<CombinedResults | null>(null)
  const [simResults, setSimResults] = useState<SimResults | null>(null)
  const [simSummaries, setSimSummaries] = useState<Record<string, SimSummary>>({})
  const [leftCollapsed, setLeftCollapsed] = useState(false)
  const [logJump, setLogJump] = useState<{ node: string; path: string; timeLabel: string; nonce: number } | null>(null)
  const [manualReloadTick, setManualReloadTick] = useState(0)

  const refreshRuns = useCallback(async () => {
    const idx = await fetchJson<RunIndex>('/__netsim/runs')
    if (!idx) return
    setRuns(idx.runs)
    setWorkRoot(idx.workRoot)
    setSelectedRun((prev) => {
      if (idx.runs.length === 0) return null
      if (prev && idx.runs.includes(prev)) return prev
      return idx.runs[0]
    })
  }, [])

  useEffect(() => {
    refreshRuns()
    const id = setInterval(refreshRuns, 2000)
    return () => clearInterval(id)
  }, [refreshRuns])

  useEffect(() => {
    setSelectedItem('overview')
    setTab('perf')
    setManifest(null)
    setProgress(null)
    setCombined(null)
    setSimSummaries({})
    setSimResults(null)
  }, [selectedRun])

  useEffect(() => {
    if (!selectedRun) return
    let dead = false
    const load = async () => {
      const base = baseForRun(selectedRun)
      const [m, p, c] = await Promise.all([
        fetchJson<RunManifest>(`${base}manifest.json`),
        fetchJson<RunProgress>(`${base}progress.json`),
        fetchJson<CombinedResults>(`${base}combined-results.json`),
      ])
      if (dead) return
      setManifest(m)
      setProgress(p)
      setCombined(c)

      const simDirs = new Set<string>()
      for (const row of m?.simulations ?? []) simDirs.add(row.sim_dir)
      for (const row of p?.simulations ?? []) if (row.sim_dir) simDirs.add(row.sim_dir)
      for (const row of c?.runs ?? []) if (row.sim_dir) simDirs.add(row.sim_dir)

      const loaded = await Promise.all(
        [...simDirs].map(async (simDir) => {
          const summary = await fetchJson<SimSummary>(`${base}${simDir}/sim.json`)
          return summary ? ([simDir, summary] as const) : null
        }),
      )
      if (dead) return
      const next: Record<string, SimSummary> = {}
      for (const row of loaded) {
        if (row) next[row[0]] = row[1]
      }
      setSimSummaries(next)
    }
    load()
    const shouldPoll = tab !== 'logs'
    const id = shouldPoll ? setInterval(load, 1000) : null
    return () => {
      dead = true
      if (id) clearInterval(id)
    }
  }, [selectedRun, tab, manualReloadTick])

  useEffect(() => {
    if (!selectedRun || selectedItem === 'overview') {
      setSimResults(null)
      return
    }
    let dead = false
    const load = async () => {
      const base = baseForRun(selectedRun)
      const results = await fetchJson<SimResults>(`${base}${selectedItem}/results.json`)
      if (!dead) setSimResults(results)
    }
    load()
    if (tab !== 'perf') {
      return () => {
        dead = true
      }
    }
    const runStatus = progress?.status ?? manifest?.status
    const intervalMs = runStatus === 'running' ? 1000 : 3000
    const id = setInterval(load, intervalMs)
    return () => {
      dead = true
      clearInterval(id)
    }
  }, [selectedRun, selectedItem, manifest?.status, progress?.status, tab, manualReloadTick])

  const simRows = useMemo<SimOverviewRow[]>(() => {
    const simDirs = new Set<string>()
    for (const row of manifest?.simulations ?? []) simDirs.add(row.sim_dir)
    for (const row of progress?.simulations ?? []) if (row.sim_dir) simDirs.add(row.sim_dir)
    for (const row of combined?.runs ?? []) if (row.sim_dir) simDirs.add(row.sim_dir)

    return [...simDirs]
      .map((simDir) => {
        const simSummary = simSummaries[simDir] ?? null
        const row = combined?.runs.find((r) => r.sim_dir === simDir)
        const transfers = row?.transfers ?? []
        const iperf = row?.iperf ?? []
        const throughput = throughputFromTransfersOrIperf(transfers, iperf)
        return {
          sim: simNameForDir(simDir, manifest, progress, simSummary, combined),
          sim_dir: simDir,
          status: statusForSim(simDir, manifest, progress, simSummary),
          error: errorForSim(simDir, manifest, progress, simSummary),
          nodes: nodeCount(simSummary, transfers, iperf),
          up: throughput.up,
          down: throughput.down,
        }
      })
      .sort((a, b) => a.sim.localeCompare(b.sim))
  }, [combined, manifest, progress, simSummaries])

  const activeSummary = selectedItem === 'overview' ? null : (simSummaries[selectedItem] ?? null)
  const runBase = baseForRun(selectedRun)

  const handleJumpToLog = useCallback((target: { node: string; path: string; timeLabel: string }) => {
    setTab('logs')
    setLogJump({ ...target, nonce: Date.now() })
  }, [])

  return (
    <div className="app">
      <div className="topbar">
        <h1>netsim</h1>
        <select
          value={selectedRun ?? ''}
          onChange={(e) => setSelectedRun(e.target.value || null)}
        >
          <option value="">select run</option>
          {runs.map((run) => (
            <option key={run} value={run}>{run}</option>
          ))}
        </select>
        {progress && (
          <span style={{ color: 'var(--text-muted)', fontSize: 12 }}>
            {progress.status} · {progress.completed}/{progress.total}
            {progress.current_sim ? ` · ${progress.current_sim}` : ''}
          </span>
        )}
        <button className="btn" onClick={() => setManualReloadTick((v) => v + 1)}>reload</button>
        {workRoot && (
          <span style={{ marginLeft: 'auto', color: 'var(--text-muted)' }}>
            {workRoot}
          </span>
        )}
      </div>

      <div style={{ display: 'flex', flex: 1, minHeight: 0 }}>
        <div className={`logs-sidebar ${leftCollapsed ? 'collapsed' : ''}`} style={{ width: leftCollapsed ? 44 : 280 }}>
          <div className="node-label" style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
            <span>{leftCollapsed ? '≡' : 'run'}</span>
            <button className="btn" style={{ padding: '2px 6px' }} onClick={() => setLeftCollapsed((v) => !v)}>
              {leftCollapsed ? '>' : '<'}
            </button>
          </div>
          {!leftCollapsed && (
            <>
              <div
                className={`file-item${selectedItem === 'overview' ? ' active' : ''}`}
                onClick={() => setSelectedItem('overview')}
              >
                overview
              </div>
              <div className="node-group">
                <div className="node-label">simulations</div>
                {simRows.map((row) => (
                  <div
                    key={row.sim_dir}
                    className={`file-item${selectedItem === row.sim_dir ? ' active' : ''}`}
                    onClick={() => setSelectedItem(row.sim_dir)}
                  >
                    {row.sim} [{row.status}]
                    {row.error ? ` - ${shortText(row.error, 60)}` : ''}
                  </div>
                ))}
              </div>
            </>
          )}
        </div>

        <div style={{ display: 'flex', flexDirection: 'column', flex: 1, minWidth: 0 }}>
          {selectedItem === 'overview' ? (
            <div className="perf-layout">
              <div className="section">
                <div className="section-header">run overview</div>
                <div className="tbl-wrap">
                  <table>
                    <thead>
                      <tr>
                        <th>sim</th>
                        <th>status</th>
                        <th>error</th>
                        <th>nodes</th>
                        <th>up_mbps (iroh/iperf)</th>
                        <th>down_mbps (iroh/iperf)</th>
                        <th>open</th>
                      </tr>
                    </thead>
                    <tbody>
                      {simRows.map((row) => (
                        <tr key={row.sim_dir}>
                          <td>{row.sim}</td>
                          <td>{row.status}</td>
                          <td title={row.error ?? ''}>{row.error ? shortText(row.error, 140) : '—'}</td>
                          <td>{row.nodes ?? '—'}</td>
                          <td>{fmt(row.up)}</td>
                          <td>{fmt(row.down)}</td>
                          <td>
                            <button className="btn" onClick={() => setSelectedItem(row.sim_dir)}>
                              open
                            </button>
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              </div>
            </div>
          ) : (
            <>
              <div className="tabs">
                {(['perf', 'logs', 'timeline'] as Tab[]).map((viewTab) => (
                  <button
                    key={viewTab}
                    className={`tab-btn${tab === viewTab ? ' active' : ''}`}
                    onClick={() => setTab(viewTab)}
                  >
                    {viewTab}
                  </button>
                ))}
              </div>
              <div className="tab-content">
                {tab === 'perf' && <PerfTab results={simResults} />}
                {tab === 'logs' && (
                  <LogsTab base={`${runBase}${selectedItem}/`} logs={activeSummary?.logs ?? []} jumpTarget={logJump} />
                )}
                {tab === 'timeline' && (
                  <TimelineTab base={`${runBase}${selectedItem}/`} logs={activeSummary?.logs ?? []} onJumpToLog={handleJumpToLog} />
                )}
              </div>
            </>
          )}
        </div>
      </div>
    </div>
  )
}
