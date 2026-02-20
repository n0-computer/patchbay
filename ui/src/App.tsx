import { useState, useEffect, useCallback } from 'react'
import type { CombinedResults, SimResults, Manifest, ManifestLog } from './types'
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

/** Relative base URL for a run dir, always ends with '/'. */
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

// Build a manifest from results.json when manifest.json doesn't exist yet.
function inferManifest(results: SimResults, base: string): Manifest {
  const logs: ManifestLog[] = [
    { node: 'relay', path: `${base}logs/relay.log`, kind: 'tracing-ansi' },
  ]
  const seen = new Set<string>()
  for (const t of results.transfers) {
    if (!seen.has(t.provider)) {
      seen.add(t.provider)
      logs.push({ node: t.provider, path: `${base}logs/xfer/provider.log`, kind: 'iroh-ndjson' })
      logs.push({ node: t.provider, path: `${base}logs/xfer/provider/`, kind: 'qlog-dir' })
    }
    const fKey = `${t.fetcher}-${t.id}`
    if (!seen.has(fKey)) {
      seen.add(fKey)
      const idx = [...seen].filter(k => k.startsWith(t.fetcher)).length - 1
      const suffix = idx === 0 ? '' : `-${idx}`
      logs.push({ node: t.fetcher, path: `${base}logs/xfer/${t.fetcher}${suffix}.log`, kind: 'iroh-ndjson' })
      logs.push({ node: t.fetcher, path: `${base}logs/xfer/${t.fetcher}${suffix}/`, kind: 'qlog-dir' })
    }
  }
  return { sim: results.sim, run: '', logs }
}

export default function App() {
  const [tab, setTab] = useState<Tab>(getHashTab)
  const [combined, setCombined] = useState<CombinedResults | null>(null)
  const [results, setResults] = useState<SimResults | null>(null)
  const [manifest, setManifest] = useState<Manifest | null>(null)
  const [selectedRun, setSelectedRun] = useState<string | null>(getRunParam)
  const [loading, setLoading] = useState(true)

  // Dev-mode run listing from vite plugin
  const [devRuns, setDevRuns] = useState<string[] | null>(null)
  const [workRoot, setWorkRoot] = useState<string | null>(null)

  // Sync tab to URL hash
  useEffect(() => {
    const onHash = () => setTab(getHashTab())
    window.addEventListener('hashchange', onHash)
    return () => window.removeEventListener('hashchange', onHash)
  }, [])

  const switchTab = useCallback((t: Tab) => {
    window.location.hash = t
    setTab(t)
  }, [])

  // Try the dev-mode runs endpoint once on mount
  useEffect(() => {
    fetchJson<{ workRoot: string; runs: string[] }>('/__netsim/runs').then(data => {
      if (data) {
        setDevRuns(data.runs)
        setWorkRoot(data.workRoot)
        // Auto-select latest run if nothing is in the URL
        if (!getRunParam() && data.runs.length > 0) {
          setSelectedRun(data.runs[0])
        }
      }
    })
  }, [])

  // Load data whenever selectedRun changes
  useEffect(() => {
    ;(async () => {
      setLoading(true)
      setResults(null)
      setManifest(null)

      const c = await fetchJson<CombinedResults>('./combined-results.json')
      setCombined(c)

      const base = runBase(selectedRun)
      const r = await fetchJson<SimResults>(`${base}results.json`)
      setResults(r)

      if (r) {
        const m = await fetchJson<Manifest>(`${base}manifest.json`)
        setManifest(m ?? inferManifest(r, base))
      }
      setLoading(false)
    })()
  }, [selectedRun])

  const onSelectRun = useCallback((run: string) => {
    const url = new URL(window.location.href)
    if (run) {
      url.searchParams.set('run', run)
    } else {
      url.searchParams.delete('run')
    }
    window.history.pushState({}, '', url)
    setSelectedRun(run || null)
  }, [])

  // Merge run lists: dev API (has all subdirs) + combined-results (has perf data)
  const combinedRunNames = combined?.runs.map(r => r.run) ?? []
  const allRunNames = devRuns
    ? [...new Set([...devRuns, ...combinedRunNames])] // dev API is already newest-first
    : combinedRunNames

  return (
    <div className="app">
      <div className="topbar">
        <h1>netsim</h1>
        {results && (
          <span style={{ color: 'var(--text-muted)', fontSize: 12 }}>{results.sim}</span>
        )}

        {allRunNames.length > 0 && (
          <span style={{ display: 'flex', alignItems: 'center', gap: 6, fontSize: 12 }}>
            <span style={{ color: 'var(--text-muted)' }}>run</span>
            <select
              value={selectedRun ?? ''}
              onChange={e => onSelectRun(e.target.value)}
            >
              <option value="">— overview —</option>
              {allRunNames.map(r => (
                <option key={r} value={r}>{r}</option>
              ))}
            </select>
          </span>
        )}

        {selectedRun && (
          <button className="btn" style={{ fontSize: 11 }}
            onClick={() => onSelectRun('')}>
            ✕
          </button>
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
        {!loading && tab !== 'perf' && !results && (
          <div className="empty">select a run with results to view {tab}</div>
        )}
      </div>
    </div>
  )
}
