import { useEffect, useMemo, useState } from 'react'
import type { SimLogEntry } from '../types'

const ANSI_RE = /\x1b\[[0-9;]*m/g
const TRACING_RE = /^(\d{4}-\d{2}-\d{2}T[\d:.]+Z)\s+(ERROR|WARN|INFO|DEBUG|TRACE)\s+(.+?):\s*(.*)/
const PREVIEW_BYTES = 256 * 1024

type ParsedLine =
  | { type: 'tracing'; level: string; ts: string; target: string; msg: string }
  | { type: 'event'; kind: string; raw: string }
  | { type: 'raw'; raw: string }

type TransferPreviewEvent = {
  kind: string
  fields: string[]
}

type QlogEvent = {
  time?: number
  name?: string
}

type RenderMode = 'rendered' | 'raw'

type LogMeta = {
  size_bytes: number
  line_count: number
}

interface Props {
  base: string
  logs: SimLogEntry[]
  jumpTarget?: { node: string; path: string; timeLabel: string; nonce: number } | null
}

function shortenRemoteId(value: unknown): unknown {
  if (typeof value !== 'string') return value
  return value.length > 5 ? value.slice(0, 5) : value
}

function valueString(v: unknown): string {
  if (typeof v === 'string') return v
  if (typeof v === 'number' || typeof v === 'boolean') return String(v)
  if (v == null) return 'null'
  try {
    return JSON.stringify(v)
  } catch {
    return String(v)
  }
}

function parseLine(raw: string): ParsedLine {
  const stripped = raw.replace(ANSI_RE, '')
  try {
    const v = JSON.parse(stripped) as Record<string, unknown>
    if (typeof v.kind === 'string') return { type: 'event', kind: v.kind, raw: stripped }
  } catch { }

  const m = stripped.match(TRACING_RE)
  if (m) return { type: 'tracing', ts: m[1], level: m[2], target: m[3], msg: m[4] }
  return { type: 'raw', raw: stripped }
}

function parseTransferPreview(text: string): TransferPreviewEvent[] {
  const events: TransferPreviewEvent[] = []
  for (const line of text.split('\n')) {
    const s = line.trim()
    if (!s) continue
    try {
      const v = JSON.parse(s) as Record<string, unknown>
      if (typeof v.kind !== 'string') continue
      const fields = Object.entries(v)
        .filter(([k]) => k !== 'kind')
        .map(([k, val]) => {
          const next = k === 'remote_id' ? shortenRemoteId(val) : val
          return `${k}=${valueString(next)}`
        })
      events.push({ kind: v.kind, fields })
    } catch { }
  }
  return events
}

function parseQlogEvents(text: string): QlogEvent[] {
  const out: QlogEvent[] = []
  for (const line of text.split('\n')) {
    const s = line.trim().replace(/^\x1e/, '')
    if (!s) continue
    try {
      const v = JSON.parse(s) as Record<string, unknown>
      out.push({
        time: typeof v.time === 'number' ? v.time : undefined,
        name: typeof v.name === 'string' ? v.name : undefined,
      })
    } catch { }
  }
  return out
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GiB`
}

async function fetchLogMeta(url: string): Promise<LogMeta> {
  const res = await fetch(`${url}?__meta=1`)
  if (!res.ok) {
    throw new Error(`HTTP ${res.status}`)
  }
  const body = await res.json() as { size_bytes?: number; line_count?: number }
  return {
    size_bytes: body.size_bytes ?? 0,
    line_count: body.line_count ?? 0,
  }
}

async function fetchRangePreview(url: string, sizeBytes: number): Promise<string> {
  const start = Math.max(0, sizeBytes - PREVIEW_BYTES)
  const end = Math.max(0, sizeBytes - 1)
  const range = sizeBytes > 0 ? `bytes=${start}-${end}` : `bytes=0-${PREVIEW_BYTES - 1}`
  const res = await fetch(url, { headers: { Range: range } })
  if (!res.ok && res.status !== 206) {
    throw new Error(`HTTP ${res.status}`)
  }
  return await res.text()
}

export default function LogsTab({ base, logs, jumpTarget }: Props) {
  const [active, setActive] = useState<SimLogEntry | null>(null)
  const [meta, setMeta] = useState<LogMeta | null>(null)
  const [loaded, setLoaded] = useState(false)
  const [text, setText] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [loadingMeta, setLoadingMeta] = useState(false)
  const [loadingContent, setLoadingContent] = useState(false)
  const [renderMode, setRenderMode] = useState<RenderMode>('raw')
  const [jumpNeedle, setJumpNeedle] = useState<string | null>(null)
  const [jumpLine, setJumpLine] = useState<number | null>(null)
  const [jumpHandledNonce, setJumpHandledNonce] = useState<number | null>(null)

  useEffect(() => {
    setActive((prev) => {
      if (prev && logs.some((l) => l.path === prev.path)) return prev
      return logs[0] ?? null
    })
  }, [logs])

  useEffect(() => {
    if (!active) return
    let dead = false
    const url = `${base}${active.path}`
    setLoaded(false)
    setText('')
    setError(null)
    setMeta(null)
    setLoadingMeta(true)
    setLoadingContent(false)
    setRenderMode('raw')
    fetchLogMeta(url)
      .then((m) => {
        if (dead) return
        setMeta(m)
      })
      .catch((e) => {
        if (dead) return
        setError(String(e))
      })
      .finally(() => {
        if (!dead) setLoadingMeta(false)
      })
    return () => {
      dead = true
    }
  }, [active, base])

  useEffect(() => {
    if (!jumpTarget || logs.length === 0) return
    if (jumpHandledNonce === jumpTarget.nonce) return
    const direct = logs.find((l) => l.path === jumpTarget.path)
    const byNode = logs.find((l) => l.node === jumpTarget.node && l.kind === 'transfer' && /logs-(fetch|provide)$/.test(l.path))
    const fallback = logs.find((l) => l.node === jumpTarget.node) ?? logs[0] ?? null
    setActive(direct ?? byNode ?? fallback)
    setJumpNeedle(jumpTarget.timeLabel)
    setJumpHandledNonce(jumpTarget.nonce)
  }, [jumpTarget, logs, jumpHandledNonce])

  const loadPreview = async () => {
    if (!active) return
    const url = `${base}${active.path}`
    setLoadingContent(true)
    setError(null)
    try {
      const content = await fetchRangePreview(url, meta?.size_bytes ?? 0)
      setText(content)
      setLoaded(true)
      setRenderMode('raw')
    } catch (e) {
      setError(String(e))
    } finally {
      setLoadingContent(false)
    }
  }

  useEffect(() => {
    if (!active || !jumpNeedle || loaded || loadingContent) return
    loadPreview()
  }, [active, jumpNeedle, loaded, loadingContent])

  const byNode = useMemo(() => {
    const m = new Map<string, SimLogEntry[]>()
    for (const log of logs) {
      if (!m.has(log.node)) m.set(log.node, [])
      m.get(log.node)!.push(log)
    }
    return [...m.entries()].sort((a, b) => a[0].localeCompare(b[0]))
  }, [logs])

  const parsed = useMemo(() => text.split('\n').filter(Boolean).map(parseLine), [text])
  const transferEvents = useMemo(() => parseTransferPreview(text), [text])
  const qlogEvents = useMemo(() => parseQlogEvents(text), [text])
  const supportsRendered = active?.kind === 'transfer' || active?.kind === 'qlog'

  useEffect(() => {
    if (!jumpNeedle) {
      setJumpLine(null)
      return
    }
    const idx = parsed.findIndex((line) => {
      if (line.type === 'tracing') return line.ts === jumpNeedle
      if (line.type === 'event') return line.raw.includes(jumpNeedle)
      return line.raw.includes(jumpNeedle)
    })
    setJumpLine(idx >= 0 ? idx : null)
  }, [parsed, jumpNeedle])

  useEffect(() => {
    if (jumpLine == null) return
    const el = document.querySelector(`[data-log-line="${jumpLine}"]`)
    if (el instanceof HTMLElement) {
      el.scrollIntoView({ block: 'center' })
    }
  }, [jumpLine])

  return (
    <div className="logs-layout">
      <div className="logs-sidebar">
        {byNode.map(([node, files]) => (
          <div key={node} className="node-group">
            <div className="node-label">{node}</div>
            {files.map((f) => (
              <div
                key={f.path}
                className={`file-item${active?.path === f.path ? ' active' : ''}`}
                onClick={() => setActive(f)}
                title={f.path}
              >
                {f.path.split('/').pop()}
                <span style={{ marginLeft: 6, color: 'var(--text-muted)' }}>[{f.kind}]</span>
              </div>
            ))}
          </div>
        ))}
      </div>

      <div className="logs-main">
        {error && <div className="error-msg">{error}</div>}
        {!active && <div className="empty">no logs</div>}
        {active && (
          <>
            <div className="logs-toolbar">
              <span>{active.path}</span>
              {meta && (
                <span style={{ color: 'var(--text-muted)' }}>
                  {formatBytes(meta.size_bytes)} · {meta.line_count} lines
                </span>
              )}
              {supportsRendered && loaded && (
                <>
                  <button
                    className={`btn${renderMode === 'rendered' ? ' active' : ''}`}
                    onClick={() => setRenderMode('rendered')}
                  >
                    preview
                  </button>
                  <button
                    className={`btn${renderMode === 'raw' ? ' active' : ''}`}
                    onClick={() => setRenderMode('raw')}
                  >
                    raw
                  </button>
                </>
              )}
              {!loaded && (
                <button className="btn" onClick={loadPreview} disabled={loadingMeta || loadingContent}>
                  {loadingContent ? 'loading…' : 'load log'}
                </button>
              )}
              {jumpNeedle && (
                <span style={{ color: 'var(--yellow)' }}>
                  jump: {jumpNeedle} {jumpLine == null && loaded ? '(not in loaded range)' : ''}
                </span>
              )}
              <button className="btn" onClick={() => { setLoaded(false); setText(''); setJumpLine(null); setError(null); }}>
                clear
              </button>
            </div>

            {!loaded && (
              <div className="empty">
                {loadingMeta ? 'reading metadata…' : 'load log to view this file'}
              </div>
            )}

            {loaded && renderMode === 'rendered' && active.kind === 'transfer' && (
              <div className="tbl-wrap">
                <table>
                  <thead>
                    <tr>
                      <th>kind</th>
                      <th>fields</th>
                    </tr>
                  </thead>
                  <tbody>
                    {transferEvents.map((ev, i) => (
                      <tr key={i}>
                        <td>{ev.kind}</td>
                        <td>{ev.fields.join(' ') || '—'}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}

            {loaded && renderMode === 'rendered' && active.kind === 'qlog' && (
              <div className="tbl-wrap">
                <table>
                  <thead>
                    <tr>
                      <th>time</th>
                      <th>name</th>
                    </tr>
                  </thead>
                  <tbody>
                    {qlogEvents.map((ev, i) => (
                      <tr key={i}>
                        <td>{ev.time ?? '—'}</td>
                        <td>{ev.name ?? '—'}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}

            {loaded && (!supportsRendered || renderMode === 'raw') && (
              <div className="logs-content">
                {parsed.map((line, i) => {
                  if (line.type === 'tracing') {
                    return (
                      <div key={i} data-log-line={i} className={`log-entry${jumpLine === i ? ' jump-hit' : ''}`}>
                        <span className="log-ts">{line.ts.split('T')[1]?.replace('Z', '')}</span>
                        <span className={`level-${line.level}`} style={{ marginRight: 8 }}>{line.level}</span>
                        <span className="log-target">{line.target}:</span>
                        <span className="log-msg">{line.msg}</span>
                      </div>
                    )
                  }
                  if (line.type === 'event') {
                    return <div key={i} data-log-line={i} className={`log-entry log-iroh-events${jumpLine === i ? ' jump-hit' : ''}`}>{line.kind} {line.raw}</div>
                  }
                  return <div key={i} data-log-line={i} className={`log-entry log-raw${jumpLine === i ? ' jump-hit' : ''}`}>{line.raw}</div>
                })}
              </div>
            )}
          </>
        )}
      </div>
    </div>
  )
}
