import { useState, useEffect, useCallback } from 'react'
import type { CombinedResults, SimResults, Manifest, ManifestLog, RunProgress } from './types'
import PerfTab from './components/PerfTab'
import LogsTab from './components/LogsTab'
import TimelineTab from './components/TimelineTab'
import QlogTab from './components/QlogTab'

type Tab = 'perf' | 'logs' | 'timeline' | 'qlog'

function getHashTab(): Tab {
  const h = window.location.hash.slice(1) as Tab
  return ['perf', 'logs', 'timeline', 'qlog'].includes(h) ? h : 'perf'
}

function getRunParam(): string | null {
  return new URLSearchParams(window.location.search).get('run')
}

function runBase(runName: string | null): string {
  return runName ? `./${runName}/` : './'
}

async function fetchJson<T>(url: string): Promise<T | null> {
  try {
    const r = await fetch(url)
    if (!r.ok) return null
    return await r.json() as T
  } catch {
    return null
  }
}

function inferManifest(results: SimResults, base: string): Manifest {
  const logs: ManifestLog[] = [{ node: 'relay', path: `${base}logs/relay.log`, kind: 'tracing-ansi' }]
  const seen = new Set<string>()
  for (const t of results.transfers) {
    if (!seen.has(t.provider)) {
      seen.add(t.provider)
      logs.push({ node: t.provider, path: `${base}logs/xfer/provider.log`, kind: 'iroh-ndjson' })
      logs.push({ node: t.provider, path: `${base}logs/xfer/provider/`, kind: 'qlog-dir' })
    }
    const key = `${t.fetcher}-${t.id}`
    if (!seen.has(key)) {
      seen.add(key)
      logs.push({ node: t.fetcher, path: `${base}logs/xfer/${t.fetcher}.log`, kind: 'iroh-ndjson' })
      logs.push({ node: t.fetcher, path: `${base}logs/xfer/${t.fetcher}/`, kind: 'qlog-dir' })
    }
  }
  return { sim: results.sim, run: '', logs }
}

export default function App() {
  const [tab, setTab] = useState<Tab>(getHashTab)
  const [combined, setCombined] = useState<CombinedResults | null>(null)
  const [results, setResults] = useState<SimResults | null>(null)
  const [manifest, setManifest] = useState<Manifest | null>(null)
  const [progress, setProgress] = useState<RunProgress | null>(null)
  const [selectedRun, setSelectedRun] = useState<string | null>(getRunParam)
  const [selectedSimDir, setSelectedSimDir] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)
  const [devRuns, setDevRuns] = useState<string[] | null>(null)
  const [workRoot, setWorkRoot] = useState<string | null>(null)

  useEffect(() => {
    const onHash = () => setTab(getHashTab())
    window.addEventListener('hashchange', onHash)
    return () => window.removeEventListener('hashchange', onHash)
  }, [])

  const switchTab = useCallback((t: Tab) => {
    window.location.hash = t
    setTab(t)
  }, [])

  const onSelectRun = useCallback((run: string) => {
    const url = new URL(window.location.href)
    if (run) {
      url.searchParams.set('run', run)
    } else {
      url.searchParams.delete('run')
    }
    window.history.pushState({}, '', url)
    setSelectedRun(run || null)
    setSelectedSimDir(null)
  }, [])

  const refreshRunList = useCallback(async () => {
    const data = await fetchJson<{ workRoot: string; runs: string[] }>('/__netsim/runs')
    if (!data) return
    setDevRuns(data.runs)
    setWorkRoot(data.workRoot)
    if (!selectedRun && data.runs.length > 0) {
      onSelectRun(data.runs[0])
    }
  }, [onSelectRun, selectedRun])

  useEffect(() => {
    refreshRunList()
    const id = setInterval(refreshRunList, 1500)
    return () => clearInterval(id)
  }, [refreshRunList])

  const refreshData = useCallback(async () => {
    setLoading(true)
    const base = runBase(selectedRun)
    const [c, p, m] = await Promise.all([
      fetchJson<CombinedResults>(`${base}combined-results.json`),
      fetchJson<RunProgress>(`${base}progress.json`),
      fetchJson<Manifest>(`${base}manifest.json`),
    ])
    setCombined(c)
    setProgress(p)

    let simDir = selectedSimDir
    if (!simDir) {
      const running = p?.simulations?.find(s => s.status === 'running' && s.sim_dir)?.sim_dir
      const firstDone = p?.simulations?.find(s => s.sim_dir)?.sim_dir
      const firstManifest = m?.simulations?.find(s => s.sim_dir)?.sim_dir
      simDir = running ?? firstDone ?? firstManifest ?? null
      setSelectedSimDir(simDir)
    }

    if (simDir) {
      const simBase = `${base}${simDir}/`
      const r = await fetchJson<SimResults>(`${simBase}results.json`)
      setResults(r)
      setManifest(r ? inferManifest(r, simBase) : null)
    } else {
      setResults(null)
      setManifest(null)
    }
    setLoading(false)
  }, [selectedRun, selectedSimDir])

  useEffect(() => {
    refreshData()
  }, [refreshData])

  useEffect(() => {
    if (progress?.status !== 'running') return
    const id = setInterval(refreshData, 1000)
    return () => clearInterval(id)
  }, [progress?.status, refreshData])

  const combinedRunNames = combined?.runs.map(r => r.run) ?? []
  const allRunNames = devRuns
    ? [...new Set([...devRuns, ...combinedRunNames])]
    : combinedRunNames

  return (
    <div className="app">
      <div className="topbar">
        <h1>netsim</h1>
        {progress && (
          <span style={{ color: 'var(--text-muted)', fontSize: 12 }}>
            {progress.status} · {progress.completed}/{progress.total}
            {progress.current_sim ? ` · ${progress.current_sim}` : ''}
          </span>
        )}

        {allRunNames.length > 0 && (
          <span style={{ display: 'flex', alignItems: 'center', gap: 6, fontSize: 12 }}>
            <span style={{ color: 'var(--text-muted)' }}>run</span>
            <select value={selectedRun ?? ''} onChange={e => onSelectRun(e.target.value)}>
              <option value="">— overview —</option>
              {allRunNames.map(r => <option key={r} value={r}>{r}</option>)}
            </select>
          </span>
        )}

        {workRoot && (
          <span
            title={workRoot}
            style={{
              marginLeft: 'auto', fontSize: 10, color: 'var(--text-muted)',
              maxWidth: 300, overflow: 'hidden', textOverflow: 'ellipsis',
              whiteSpace: 'nowrap', direction: 'rtl', textAlign: 'right',
            }}
          >
            {workRoot}
          </span>
        )}
      </div>

      <div className="tabs">
        {(['perf', 'logs', 'timeline', 'qlog'] as Tab[]).map(t => (
          <button
            key={t}
            className={`tab-btn${tab === t ? ' active' : ''}`}
            onClick={() => switchTab(t)}
            disabled={t !== 'perf' && !results}
          >
            {t}
          </button>
        ))}
      </div>

      <div className="tab-content">
        {loading && <div className="loading">loading…</div>}
        {!loading && tab === 'perf' && (
          <PerfTab
            results={results}
            combined={combined}
            onSelectRun={onSelectRun}
          />
        )}
        {!loading && tab === 'logs' && results && manifest && (
          <LogsTab manifest={manifest} base={runBase(selectedRun)} />
        )}
        {!loading && tab === 'timeline' && results && manifest && (
          <TimelineTab manifest={manifest} base={runBase(selectedRun)} results={results} />
        )}
        {!loading && tab === 'qlog' && results && manifest && (
          <QlogTab manifest={manifest} base={runBase(selectedRun)} />
        )}
      </div>
    </div>
  )
}
