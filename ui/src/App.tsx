import { useCallback, useEffect, useMemo, useState } from 'react'
import PerfTab from './components/PerfTab'
import LogsTab from './components/LogsTab'
import TimelineTab from './components/TimelineTab'
import type { RunIndex, RunManifest, RunProgress, SimResults, SimSummary } from './types'

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
  const [selectedSimDir, setSelectedSimDir] = useState<string | null>(null)
  const [simSummary, setSimSummary] = useState<SimSummary | null>(null)
  const [simResults, setSimResults] = useState<SimResults | null>(null)
  const [tab, setTab] = useState<Tab>('perf')

  const refreshRuns = useCallback(async () => {
    const idx = await fetchJson<RunIndex>('/__netsim/runs')
    if (!idx) return
    setRuns(idx.runs)
    setWorkRoot(idx.workRoot)
    if (!selectedRun && idx.runs.length > 0) setSelectedRun(idx.runs[0])
  }, [selectedRun])

  const refreshRun = useCallback(async () => {
    const base = baseForRun(selectedRun)
    const [m, p] = await Promise.all([
      fetchJson<RunManifest>(`${base}manifest.json`),
      fetchJson<RunProgress>(`${base}progress.json`),
    ])
    setManifest(m)
    setProgress(p)

    const nextSim =
      selectedSimDir
      ?? p?.simulations.find((s) => s.status === 'running' && s.sim_dir)?.sim_dir
      ?? p?.simulations.find((s) => s.sim_dir)?.sim_dir
      ?? m?.simulations.find((s) => s.sim_dir)?.sim_dir
      ?? null
    setSelectedSimDir(nextSim)
  }, [selectedRun, selectedSimDir])

  const refreshSim = useCallback(async () => {
    if (!selectedRun || !selectedSimDir) {
      setSimSummary(null)
      setSimResults(null)
      return
    }
    const simBase = `${baseForRun(selectedRun)}${selectedSimDir}/`
    const [summary, results] = await Promise.all([
      fetchJson<SimSummary>(`${simBase}sim.json`),
      fetchJson<SimResults>(`${simBase}results.json`),
    ])
    setSimSummary(summary)
    setSimResults(results)
  }, [selectedRun, selectedSimDir])

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
    const fromManifest = manifest?.simulations ?? []
    return fromManifest.map((s) => {
      const p = byProgress.get(s.sim_dir)
      return {
        sim: s.sim,
        sim_dir: s.sim_dir,
        status: p?.status ?? s.status,
      }
    })
  }, [manifest, progress])

  const runBase = baseForRun(selectedRun)
  const showSimView = Boolean(selectedSimDir)

  return (
    <div className="app">
      <div className="topbar">
        <h1>netsim</h1>
        <select
          value={selectedRun ?? ''}
          onChange={(e) => {
            setSelectedRun(e.target.value || null)
            setSelectedSimDir(null)
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

      {!showSimView && (
        <div className="perf-layout">
          <div className="section">
            <div className="section-header">simulations</div>
            <div className="tbl-wrap">
              <table>
                <thead>
                  <tr><th>sim</th><th>status</th><th>open</th></tr>
                </thead>
                <tbody>
                  {simRows.map((row) => (
                    <tr key={row.sim_dir}>
                      <td>{row.sim}</td>
                      <td>{row.status}</td>
                      <td>
                        <button className="btn" onClick={() => setSelectedSimDir(row.sim_dir)}>
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
      )}

      {showSimView && (
        <div style={{ display: 'flex', flex: 1, minHeight: 0 }}>
          <div className="logs-sidebar" style={{ width: 260 }}>
            <div className="node-label">simulations</div>
            {simRows.map((row) => (
              <div
                key={row.sim_dir}
                className={`file-item${selectedSimDir === row.sim_dir ? ' active' : ''}`}
                onClick={() => setSelectedSimDir(row.sim_dir)}
              >
                {row.sim} [{row.status}]
              </div>
            ))}
            <div className="node-group">
              <div className="file-item" onClick={() => setSelectedSimDir(null)}>← back to run table</div>
            </div>
          </div>

          <div style={{ display: 'flex', flexDirection: 'column', flex: 1, minWidth: 0 }}>
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
                  base={`${runBase}${selectedSimDir}/`}
                  logs={simSummary?.logs ?? []}
                />
              )}
              {tab === 'timeline' && (
                <TimelineTab
                  base={`${runBase}${selectedSimDir}/`}
                  logs={simSummary?.logs ?? []}
                />
              )}
            </div>
          </div>
        </div>
      )}
    </div>
  )
}
