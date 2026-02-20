import { useCallback, useEffect, useMemo, useState } from 'react'
import PerfTab from './components/PerfTab'
import LogsTab from './components/LogsTab'
import TimelineTab from './components/TimelineTab'
import type {
  CombinedResults,
  RunIndex,
  RunManifest,
  RunProgress,
  SimResults,
  SimSummary,
} from './types'

type Tab = 'perf' | 'logs' | 'timeline'

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

export default function App() {
  const [runs, setRuns] = useState<string[]>([])
  const [workRoot, setWorkRoot] = useState<string>('')
  const [selectedRun, setSelectedRun] = useState<string | null>(null)

  const [manifest, setManifest] = useState<RunManifest | null>(null)
  const [progress, setProgress] = useState<RunProgress | null>(null)
  const [combined, setCombined] = useState<CombinedResults | null>(null)

  const [selectedItem, setSelectedItem] = useState<string>('overview') // overview | <sim_dir>
  const [simSummary, setSimSummary] = useState<SimSummary | null>(null)
  const [simResults, setSimResults] = useState<SimResults | null>(null)
  const [tab, setTab] = useState<Tab>('perf')

  const refreshRuns = useCallback(async () => {
    const idx = await fetchJson<RunIndex>('/__netsim/runs')
    if (!idx) return
    setRuns(idx.runs)
    setWorkRoot(idx.workRoot)
    if (!selectedRun && idx.runs.length > 0) {
      setSelectedRun(idx.runs[0])
      setSelectedItem('overview')
    }
  }, [selectedRun])

  const refreshRun = useCallback(async () => {
    const base = baseForRun(selectedRun)
    const [m, p, c] = await Promise.all([
      fetchJson<RunManifest>(`${base}manifest.json`),
      fetchJson<RunProgress>(`${base}progress.json`),
      fetchJson<CombinedResults>(`${base}combined-results.json`),
    ])
    setManifest(m)
    setProgress(p)
    setCombined(c)
  }, [selectedRun])

  const refreshSim = useCallback(async () => {
    if (!selectedRun || selectedItem === 'overview') {
      setSimSummary(null)
      setSimResults(null)
      return
    }
    const simBase = `${baseForRun(selectedRun)}${selectedItem}/`
    const [summary, results] = await Promise.all([
      fetchJson<SimSummary>(`${simBase}sim.json`),
      fetchJson<SimResults>(`${simBase}results.json`),
    ])
    setSimSummary(summary)
    setSimResults(results)
  }, [selectedRun, selectedItem])

  useEffect(() => {
    refreshRuns()
    const id = setInterval(refreshRuns, 2000)
    return () => clearInterval(id)
  }, [refreshRuns])

  useEffect(() => {
    refreshRun()
  }, [refreshRun])

  useEffect(() => {
    refreshSim()
  }, [refreshSim])

  useEffect(() => {
    if (progress?.status !== 'running') return
    const id = setInterval(() => {
      refreshRun()
      refreshSim()
    }, 1000)
    return () => clearInterval(id)
  }, [progress?.status, refreshRun, refreshSim])

  const simRows = useMemo(() => {
    const byProgress = new Map((progress?.simulations ?? []).map((s) => [s.sim_dir ?? '', s]))
    const byManifest = new Map((manifest?.simulations ?? []).map((s) => [s.sim_dir, s]))
    const byCombined = new Map((combined?.runs ?? []).map((s) => [s.sim_dir ?? '', s]))

    const simDirs = new Set<string>([
      ...(manifest?.simulations ?? []).map((s) => s.sim_dir),
      ...(progress?.simulations ?? []).map((s) => s.sim_dir ?? '').filter(Boolean),
      ...(combined?.runs ?? []).map((s) => s.sim_dir ?? '').filter(Boolean),
    ])

    return [...simDirs].map((simDir) => {
      const m = byManifest.get(simDir)
      const p = byProgress.get(simDir)
      const c = byCombined.get(simDir)
      const transfers = c?.transfers ?? []
      const iperf = c?.iperf ?? []
      const downRows = transfers.map((t) => t.mbps).filter((v): v is number => v != null)
      const upRows = iperf.map((i) => i.mbps).filter((v): v is number => v != null)
      const down = downRows.length ? downRows.reduce((a, b) => a + b, 0) / downRows.length : undefined
      const up = upRows.length ? upRows.reduce((a, b) => a + b, 0) / upRows.length : undefined
      return {
        sim: p?.sim ?? m?.sim ?? c?.sim ?? simDir,
        sim_dir: simDir,
        status: p?.status ?? m?.status ?? 'pending',
        down,
        up,
      }
    }).sort((a, b) => a.sim.localeCompare(b.sim))
  }, [combined, manifest, progress])

  const runBase = baseForRun(selectedRun)
  const isOverview = selectedItem === 'overview'

  return (
    <div className="app">
      <div className="topbar">
        <h1>netsim</h1>
        <select
          value={selectedRun ?? ''}
          onChange={(e) => {
            setSelectedRun(e.target.value || null)
            setSelectedItem('overview')
          }}
        >
          <option value="">select run</option>
          {runs.map((r) => <option key={r} value={r}>{r}</option>)}
        </select>
        {progress && (
          <span style={{ color: 'var(--text-muted)', fontSize: 12 }}>
            {progress.status} · {progress.completed}/{progress.total}
            {progress.current_sim ? ` · ${progress.current_sim}` : ''}
          </span>
        )}
        {workRoot && <span style={{ marginLeft: 'auto', color: 'var(--text-muted)' }}>{workRoot}</span>}
      </div>

      <div style={{ display: 'flex', flex: 1, minHeight: 0 }}>
        <div className="logs-sidebar" style={{ width: 260 }}>
          <div className="node-label">run</div>
          <div
            className={`file-item${isOverview ? ' active' : ''}`}
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
              </div>
            ))}
          </div>
        </div>

        <div style={{ display: 'flex', flexDirection: 'column', flex: 1, minWidth: 0 }}>
          {isOverview ? (
            <div className="perf-layout">
              <div className="section">
                <div className="section-header">overview</div>
                <div className="tbl-wrap">
                  <table>
                    <thead>
                      <tr>
                        <th>sim</th>
                        <th>status</th>
                        <th>down_mbps (iroh)</th>
                        <th>up_mbps (iperf)</th>
                        <th>open</th>
                      </tr>
                    </thead>
                    <tbody>
                      {simRows.map((row) => (
                        <tr key={row.sim_dir}>
                          <td>{row.sim}</td>
                          <td>{row.status}</td>
                          <td>{row.down == null ? '—' : row.down.toFixed(2)}</td>
                          <td>{row.up == null ? '—' : row.up.toFixed(2)}</td>
                          <td><button className="btn" onClick={() => setSelectedItem(row.sim_dir)}>open</button></td>
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
                {(['perf', 'logs', 'timeline'] as Tab[]).map((t) => (
                  <button
                    key={t}
                    className={`tab-btn${tab === t ? ' active' : ''}`}
                    onClick={() => setTab(t)}
                  >
                    {t}
                  </button>
                ))}
              </div>
              <div className="tab-content">
                {tab === 'perf' && <PerfTab results={simResults} />}
                {tab === 'logs' && (
                  <LogsTab
                    base={`${runBase}${selectedItem}/`}
                    logs={simSummary?.logs ?? []}
                  />
                )}
                {tab === 'timeline' && (
                  <TimelineTab
                    base={`${runBase}${selectedItem}/`}
                    logs={simSummary?.logs ?? []}
                  />
                )}
              </div>
            </>
          )}
        </div>
      </div>
    </div>
  )
}
