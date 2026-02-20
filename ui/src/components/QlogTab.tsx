import { useState, useEffect, useMemo } from 'react'
import type { Manifest, QlogEvent } from '../types'

// ── qlog parsing ──────────────────────────────────────────────────────────────
// iroh emits JSON-seq: first line is the header, subsequent lines are events.
// Record separator (0x1e) may or may not be present.

interface ParsedQlog {
  title?: string
  vantage?: string
  events: QlogEvent[]
  error?: string
  truncated: boolean
}

const MAX_QLOG_EVENTS = 5000

function parseQlog(text: string): ParsedQlog {
  const lines = text.split('\n').map(l => l.trimStart().replace(/^\x1e/, '').trim()).filter(Boolean)
  let title: string | undefined
  let vantage: string | undefined
  const events: QlogEvent[] = []

  for (const line of lines) {
    try {
      const obj = JSON.parse(line) as Record<string, unknown>
      // Header line has file_schema / trace
      if (obj.file_schema || obj.trace) {
        const trace = obj.trace as Record<string, unknown> | undefined
        title = trace?.title as string | undefined
        const vp = trace?.vantage_point as Record<string, unknown> | undefined
        vantage = vp?.type as string | undefined
        // Some formats embed events in the header
        if (Array.isArray(trace?.events)) {
          for (const e of (trace.events as QlogEvent[])) {
            events.push(e)
          }
        }
      } else if (obj.time !== undefined && obj.name !== undefined) {
        events.push(obj as unknown as QlogEvent)
      }
    } catch { /* skip */ }
    if (events.length >= MAX_QLOG_EVENTS) {
      return { title, vantage, events, truncated: true }
    }
  }
  return { title, vantage, events, truncated: false }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

function nameClass(name: string): string {
  if (name.startsWith('quic:')) return 'qlog-name-transport'
  if (name.startsWith('recovery:')) return 'qlog-name-recovery'
  if (name.startsWith('security:') || name.startsWith('tls:')) return 'qlog-name-security'
  if (name.startsWith('http:') || name.startsWith('h3:')) return 'qlog-name-http3'
  return ''
}

function dataSummary(data: Record<string, unknown>): string {
  return Object.entries(data)
    .slice(0, 4)
    .map(([k, v]) => {
      const vs = typeof v === 'object' && v !== null ? '{…}' : String(v)
      return `${k}=${vs.slice(0, 30)}`
    })
    .join('  ')
}

function fmtTime(ms: number): string {
  if (ms >= 1000) return `${(ms / 1000).toFixed(3)}s`
  return `${ms.toFixed(3)}ms`
}

// ── File list from manifest ───────────────────────────────────────────────────
// qlog-dir entries need to be expanded. We can't list directories from fetch()
// so we rely on a sibling index. For now, we try fetching known patterns from
// the manifest and fall back to asking the user to paste a URL.
// TODO: have Rust write a qlog-index.json with actual filenames.

async function findQlogFiles(path: string): Promise<string[]> {
  // Try fetching an index file written by netsim
  try {
    const r = await fetch(path + 'index.json')
    if (r.ok) {
      const idx = await r.json() as { files: string[] }
      return idx.files.map(f => path + f)
    }
  } catch { /* */ }
  return [] // TODO: need server-side support or netsim to write index.json
}

// ── Component ─────────────────────────────────────────────────────────────────

export default function QlogTab({ manifest, base: _base }: { manifest: Manifest; base: string }) {
  const qlogDirs = manifest.logs.filter(l => l.kind === 'qlog-dir')

  const [fileUrl, setFileUrl] = useState('')
  const [customUrl, setCustomUrl] = useState('')
  const [discoveredFiles, setDiscoveredFiles] = useState<string[]>([])
  const [qlog, setQlog] = useState<ParsedQlog | null>(null)
  const [loading, setLoading] = useState(false)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [selected, setSelected] = useState<number | null>(null)
  const [filter, setFilter] = useState('')

  // Discover qlog files from manifest dirs
  useEffect(() => {
    ;(async () => {
      const all: string[] = []
      for (const d of qlogDirs) {
        const files = await findQlogFiles(d.path)
        all.push(...files)
      }
      setDiscoveredFiles(all)
      if (all.length > 0) setFileUrl(all[0])
    })()
  }, [manifest]) // eslint-disable-line

  const loadFile = async (url: string) => {
    if (!url) return
    setLoading(true)
    setQlog(null)
    setLoadError(null)
    setSelected(null)
    try {
      const r = await fetch(url)
      if (!r.ok) throw new Error(`HTTP ${r.status}: ${url}`)
      // qlog files can be very large — warn if > 50MB
      const cl = r.headers.get('content-length')
      if (cl && parseInt(cl) > 50 * 1024 * 1024) {
        setLoadError(`file is large (${(parseInt(cl) / 1e6).toFixed(0)} MB), loading anyway…`)
      }
      const text = await r.text()
      setQlog(parseQlog(text))
    } catch (e) {
      setLoadError(String(e))
    } finally {
      setLoading(false)
    }
  }

  useEffect(() => {
    if (fileUrl) loadFile(fileUrl)
  }, [fileUrl]) // eslint-disable-line

  const filterRe = useMemo(() => {
    if (!filter) return null
    try { return new RegExp(filter, 'i') } catch { return null }
  }, [filter])

  const visibleEvents = useMemo(() => {
    if (!qlog) return []
    return qlog.events.filter(e => {
      if (!filterRe) return true
      return filterRe.test(e.name) || filterRe.test(dataSummary(e.data ?? {}))
    })
  }, [qlog, filterRe])

  const selectedEvent = selected != null ? visibleEvents[selected] : null

  const allOptions = [
    ...discoveredFiles,
    ...(customUrl && !discoveredFiles.includes(customUrl) ? [customUrl] : []),
  ]

  return (
    <div className="qlog-layout">
      <div className="qlog-toolbar">
        {allOptions.length > 0 && (
          <select
            value={fileUrl}
            onChange={e => setFileUrl(e.target.value)}
            style={{ maxWidth: 400 }}
          >
            {allOptions.map(f => (
              <option key={f} value={f}>{f.split('/').slice(-2).join('/')}</option>
            ))}
          </select>
        )}
        <input
          type="text"
          placeholder="paste qlog path/url…"
          value={customUrl}
          onChange={e => setCustomUrl(e.target.value)}
          onKeyDown={e => { if (e.key === 'Enter') { setFileUrl(customUrl); loadFile(customUrl) } }}
          style={{ minWidth: 280 }}
        />
        <button className="btn" onClick={() => loadFile(customUrl || fileUrl)}>load</button>
        <input
          type="search"
          placeholder="filter events…"
          value={filter}
          onChange={e => setFilter(e.target.value)}
          style={{ minWidth: 180 }}
        />
        {qlog && (
          <span style={{ fontSize: 11, color: 'var(--text-muted)', marginLeft: 'auto' }}>
            {qlog.title && <>{qlog.title} · </>}
            {qlog.vantage && <>{qlog.vantage} · </>}
            {visibleEvents.length}/{qlog.events.length} events
            {qlog.truncated && ' (truncated)'}
          </span>
        )}
      </div>

      {loadError && <div className="error-msg">{loadError}</div>}
      {loading && <div className="loading">loading qlog…</div>}

      {!loading && !qlog && (
        <div className="empty">
          {discoveredFiles.length === 0
            ? 'no qlog files discovered — netsim needs to write a qlog index, or paste a path above'
            : 'select a qlog file above'}
        </div>
      )}

      {!loading && qlog && (
        <>
          <div className="tbl-wrap" style={{ flex: 1, overflow: 'auto' }}>
            <table>
              <thead>
                <tr>
                  <th style={{ width: 90 }}>time</th>
                  <th>name</th>
                  <th>data</th>
                </tr>
              </thead>
              <tbody>
                {visibleEvents.map((evt, i) => (
                  <tr
                    key={i}
                    className={selected === i ? 'selected-row' : ''}
                    onClick={() => setSelected(selected === i ? null : i)}
                    style={{ cursor: 'pointer' }}
                  >
                    <td style={{ color: 'var(--text-muted)', whiteSpace: 'nowrap' }}>
                      {fmtTime(evt.time)}
                    </td>
                    <td className={nameClass(evt.name)} style={{ whiteSpace: 'nowrap' }}>
                      {evt.name}
                    </td>
                    <td style={{ color: 'var(--text-muted)', fontSize: 11 }}>
                      {dataSummary(evt.data ?? {})}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          {selectedEvent && (
            <div className="qlog-detail">
              <div style={{ color: 'var(--text-muted)', marginBottom: 6, fontSize: 11 }}>
                {selectedEvent.name} @ {fmtTime(selectedEvent.time)}
              </div>
              {JSON.stringify(selectedEvent.data, null, 2)}
            </div>
          )}
        </>
      )}
    </div>
  )
}
