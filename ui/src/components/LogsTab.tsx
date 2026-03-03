import { useEffect, useMemo, useRef, useState } from 'react'
import type { SimLogEntry } from '../types'

const ANSI_RE = /\x1b\[[0-9;]*m/g
const TRACING_RE = /^(\d{4}-\d{2}-\d{2}T[\d:.]+Z)\s+(ERROR|WARN|INFO|DEBUG|TRACE)\s+(.+?):\s*(.*)/
const PREVIEW_BYTES = 256 * 1024

type ParsedLine =
  | { type: 'tracing'; level: string; ts: string; target: string; spans: string; msg: string; fields: string }
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

const ALL_LEVELS = ['ERROR', 'WARN', 'INFO', 'DEBUG', 'TRACE'] as const

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

/** Format a spans array as "span1{k=v}:span2{k=v}" like tracing fmt subscriber. */
function formatSpans(spans: unknown): string {
  if (!Array.isArray(spans)) return ''
  const parts: string[] = []
  for (const span of spans) {
    if (typeof span !== 'object' || span == null) continue
    const obj = span as Record<string, unknown>
    const name = typeof obj.name === 'string' ? obj.name : '?'
    const fields = Object.entries(obj)
      .filter(([k]) => k !== 'name')
      .map(([k, val]) => `${k}=${valueString(val)}`)
    if (fields.length === 0) parts.push(name)
    else parts.push(`${name}{${fields.join(',')}}`)
  }
  return parts.join(':')
}

/** Parse a single line from a log file. Handles JSON tracing format, ANSI tracing, JSON events, and raw text. */
function parseLine(raw: string): ParsedLine {
  const stripped = raw.replace(ANSI_RE, '')

  // Try JSON parse first
  try {
    const v = JSON.parse(stripped) as Record<string, unknown>

    // JSON event format: { kind: "...", ... }
    if (typeof v.kind === 'string') return { type: 'event', kind: v.kind, raw: stripped }

    // tracing-subscriber JSON format:
    // { timestamp, level, fields: { message, ... }, target, span: {...}, spans: [...] }
    if (typeof v.level === 'string' && typeof v.timestamp === 'string') {
      const level = (v.level as string).toUpperCase()
      const ts = v.timestamp as string
      const target = (v.target as string) ?? ''
      const fieldsObj = v.fields as Record<string, unknown> | undefined

      // message lives inside fields (standard format)
      const msg = fieldsObj?.message != null ? String(fieldsObj.message) : ''

      // Extra fields (everything in fields except message)
      const extras = fieldsObj
        ? Object.entries(fieldsObj)
          .filter(([k]) => k !== 'message')
          .map(([k, val]) => `${k}=${valueString(val)}`)
          .join(' ')
        : ''

      // Spans chain: tracing-subscriber includes "spans" array
      const spans = formatSpans(v.spans)

      return { type: 'tracing', level, ts, target, spans, msg, fields: extras }
    }
  } catch { }

  // ANSI tracing format: 2026-03-03T14:30:01.200Z INFO target: message
  const m = stripped.match(TRACING_RE)
  if (m) return { type: 'tracing', ts: m[1], level: m[2], target: m[3], spans: '', msg: m[4], fields: '' }

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

export default function LogsTab({ base, logs, jumpTarget }: Props) {
  const [active, setActive] = useState<SimLogEntry | null>(null)
  const [text, setText] = useState('')
  const [loaded, setLoaded] = useState(false)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [renderMode, setRenderMode] = useState<RenderMode>('raw')
  const [jumpNeedle, setJumpNeedle] = useState<string | null>(null)
  const [jumpLine, setJumpLine] = useState<number | null>(null)
  const [jumpHandledNonce, setJumpHandledNonce] = useState<number | null>(null)
  const jumpingRef = useRef(false)

  // Level filter (for tracing logs)
  const [enabledLevels, setEnabledLevels] = useState<Set<string>>(new Set(ALL_LEVELS))

  // Search
  const [searchQuery, setSearchQuery] = useState('')
  const [searchMatches, setSearchMatches] = useState<number[]>([])
  const [searchIdx, setSearchIdx] = useState(0)
  const contentRef = useRef<HTMLDivElement>(null)

  // Auto-select first log
  useEffect(() => {
    setActive((prev) => {
      if (prev && logs.some((l) => l.path === prev.path)) return prev
      return logs[0] ?? null
    })
  }, [logs])

  // Clear content state when switching active log.
  // Skip clearing jump state if a jump triggered the switch.
  useEffect(() => {
    if (!active) return
    setLoaded(false)
    setText('')
    setError(null)
    setRenderMode('raw')
    if (!jumpingRef.current) {
      setJumpNeedle(null)
      setJumpLine(null)
    }
    jumpingRef.current = false
    setSearchQuery('')
    setSearchMatches([])
    setSearchIdx(0)
  }, [active, base])

  // Handle jump target from timeline
  useEffect(() => {
    if (!jumpTarget || logs.length === 0) return
    if (jumpHandledNonce === jumpTarget.nonce) return
    // Prefer tracing log for the target node
    const tracingLog = logs.find((l) => l.node === jumpTarget.node && l.kind === 'tracing')
    const direct = logs.find((l) => l.path === jumpTarget.path)
    const fallback = logs.find((l) => l.node === jumpTarget.node) ?? logs[0] ?? null
    jumpingRef.current = true
    setActive(tracingLog ?? direct ?? fallback)
    setJumpNeedle(jumpTarget.timeLabel)
    setJumpHandledNonce(jumpTarget.nonce)
  }, [jumpTarget, logs, jumpHandledNonce])

  // Load log content (fetches last 256KB via Range header, or full file)
  const loadContent = async () => {
    if (!active) return
    const url = `${base}${active.path}`
    setLoading(true)
    setError(null)
    try {
      // Try Range request for last chunk
      const rangeRes = await fetch(url, {
        headers: { Range: `bytes=-${PREVIEW_BYTES}` },
      })
      if (rangeRes.ok || rangeRes.status === 206) {
        setText(await rangeRes.text())
        setLoaded(true)
        return
      }
      // Fallback to full fetch
      const fullRes = await fetch(url)
      if (!fullRes.ok) throw new Error(`HTTP ${fullRes.status}`)
      setText(await fullRes.text())
      setLoaded(true)
    } catch (e) {
      setError(String(e))
    } finally {
      setLoading(false)
    }
  }

  // Auto-load when jump is pending
  useEffect(() => {
    if (!active || !jumpNeedle || loaded || loading) return
    loadContent()
  }, [active, jumpNeedle, loaded, loading])

  // Auto-load tracing/events logs immediately (they're typically small)
  useEffect(() => {
    if (!active || loaded || loading) return
    if (active.kind === 'tracing' || active.kind === 'events') {
      loadContent()
    }
  }, [active, loaded, loading])

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
  const supportsRendered = (active?.kind === 'transfer' && transferEvents.length > 0) || active?.kind === 'qlog'
  const isTracingLog = active?.kind === 'tracing'

  // Apply level filter to parsed lines
  const filteredLines = useMemo(() => {
    if (!isTracingLog) return parsed.map((line, i) => ({ line, origIdx: i }))
    return parsed
      .map((line, i) => ({ line, origIdx: i }))
      .filter(({ line }) => {
        if (line.type === 'tracing') return enabledLevels.has(line.level)
        return true // show raw/event lines always
      })
  }, [parsed, enabledLevels, isTracingLog])

  // Search matches
  useEffect(() => {
    if (!searchQuery) {
      setSearchMatches([])
      setSearchIdx(0)
      return
    }
    const q = searchQuery.toLowerCase()
    const matches: number[] = []
    filteredLines.forEach(({ line }, i) => {
      const text = line.type === 'tracing'
        ? `${line.ts} ${line.level} ${line.spans} ${line.target} ${line.msg} ${line.fields}`
        : line.type === 'event' ? `${line.kind} ${line.raw}`
          : line.raw
      if (text.toLowerCase().includes(q)) matches.push(i)
    })
    setSearchMatches(matches)
    setSearchIdx(0)
  }, [searchQuery, filteredLines])

  // Jump needle resolution
  useEffect(() => {
    if (!jumpNeedle) {
      setJumpLine(null)
      return
    }
    const idx = filteredLines.findIndex(({ line }) => {
      if (line.type === 'tracing') return line.ts === jumpNeedle || line.ts.includes(jumpNeedle)
      if (line.type === 'event') return line.raw.includes(jumpNeedle)
      return line.raw.includes(jumpNeedle)
    })
    setJumpLine(idx >= 0 ? idx : null)
  }, [filteredLines, jumpNeedle])

  // Scroll to jump target
  useEffect(() => {
    if (jumpLine == null) return
    const el = document.querySelector(`[data-log-line="${jumpLine}"]`)
    if (el instanceof HTMLElement) {
      el.scrollIntoView({ block: 'center' })
    }
  }, [jumpLine])

  // Scroll to search match
  useEffect(() => {
    if (searchMatches.length === 0) return
    const targetIdx = searchMatches[searchIdx]
    if (targetIdx == null) return
    const el = document.querySelector(`[data-log-line="${targetIdx}"]`)
    if (el instanceof HTMLElement) {
      el.scrollIntoView({ block: 'center' })
    }
  }, [searchIdx, searchMatches])

  const toggleLevel = (level: string) => {
    setEnabledLevels((prev) => {
      const next = new Set(prev)
      if (next.has(level)) next.delete(level)
      else next.add(level)
      return next
    })
  }

  const fileSize = text.length

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
              {loaded && (
                <span style={{ color: 'var(--text-muted)' }}>
                  {formatBytes(fileSize)} · {parsed.length} lines
                  {isTracingLog && filteredLines.length !== parsed.length
                    ? ` (${filteredLines.length} shown)`
                    : ''}
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
              {!loaded && !loading && (
                <button className="btn" onClick={loadContent}>
                  load log
                </button>
              )}
              {loading && <span style={{ color: 'var(--text-muted)' }}>loading...</span>}
              {jumpNeedle && (
                <span style={{ color: 'var(--yellow)' }}>
                  jump: {jumpNeedle} {jumpLine == null && loaded ? '(not in range)' : ''}
                </span>
              )}
            </div>

            {/* Level filter + search toolbar (for tracing logs) */}
            {isTracingLog && loaded && (
              <div className="logs-toolbar">
                {ALL_LEVELS.map((level) => (
                  <span
                    key={level}
                    className={`level-toggle level-${level} ${enabledLevels.has(level) ? 'on' : 'off'}`}
                    onClick={() => toggleLevel(level)}
                  >
                    {level}
                  </span>
                ))}
                <input
                  type="search"
                  placeholder="search..."
                  value={searchQuery}
                  onChange={(e) => setSearchQuery(e.target.value)}
                  style={{ marginLeft: 'auto' }}
                />
                {searchMatches.length > 0 && (
                  <>
                    <span style={{ color: 'var(--text-muted)', fontSize: 11 }}>
                      {searchIdx + 1}/{searchMatches.length}
                    </span>
                    <button className="btn" onClick={() => setSearchIdx((i) => (i - 1 + searchMatches.length) % searchMatches.length)}>
                      prev
                    </button>
                    <button className="btn" onClick={() => setSearchIdx((i) => (i + 1) % searchMatches.length)}>
                      next
                    </button>
                  </>
                )}
              </div>
            )}

            {!loaded && !loading && (
              <div className="empty">
                load log to view this file
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
              <div className="logs-content" ref={contentRef}>
                {filteredLines.map(({ line, origIdx }, i) => {
                  const isSearchHit = searchMatches.includes(i)
                  const isCurrentSearch = searchMatches[searchIdx] === i
                  const isJump = jumpLine === i
                  const highlight = isJump ? ' jump-hit' : isCurrentSearch ? ' search-current' : isSearchHit ? ' search-hit' : ''

                  if (line.type === 'tracing') {
                    return (
                      <div key={origIdx} data-log-line={i} className={`log-entry${highlight}`}>
                        <span className="log-ts">{line.ts.split('T')[1]?.replace('Z', '')}</span>
                        <span className={`level-${line.level}`}>{line.level.padStart(5)}</span>
                        {line.spans && <span className="log-spans"> {line.spans}</span>}
                        <span className="log-target">{line.spans ? ': ' : ' '}{line.target}:</span>
                        {line.msg && <span className="log-msg"> {line.msg}</span>}
                        {line.fields && <span className="log-fields"> {line.fields}</span>}
                      </div>
                    )
                  }
                  if (line.type === 'event') {
                    return <div key={origIdx} data-log-line={i} className={`log-entry log-events${highlight}`}>{line.kind} {line.raw}</div>
                  }
                  return <div key={origIdx} data-log-line={i} className={`log-entry log-raw${highlight}`}>{line.raw}</div>
                })}
              </div>
            )}
          </>
        )}
      </div>
    </div>
  )
}
