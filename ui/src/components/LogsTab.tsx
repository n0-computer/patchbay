import { useEffect, useMemo, useRef, useState } from 'react'
import type { SimLogEntry } from '../types'
import KvPairs from './KvPairs'
import JsonTree from './JsonTree'
import JsonLinesTable from './JsonLinesTable'

const ANSI_RE = /\x1b\[[0-9;]*m/g
const TRACING_RE = /^(\d{4}-\d{2}-\d{2}T[\d:.]+Z)\s+(ERROR|WARN|INFO|DEBUG|TRACE)\s+(.+)$/
const TARGET_WITH_SPAN_RE = /^(.+?):\s+([a-zA-Z_][a-zA-Z0-9_:]*)\s*(.*)$/
const TARGET_AND_MSG_RE = /^(.+?):\s*(.*)$/
const PREVIEW_BYTES = 256 * 1024

type ParsedLine =
  | { type: 'tracing'; level: string; ts: string; target: string; spans: string; msg: string; fields: string }
  | { type: 'event'; kind: string; raw: string }
  | { type: 'raw'; raw: string }

type QlogEvent = {
  time?: number
  name?: string
  filterName: string
  fieldPairs: Array<{ key: string; value: string }>
}

type RenderMode = 'rendered' | 'raw'
type TimeMode = 'absolute' | 'relative'

const ALL_LEVELS = ['ERROR', 'WARN', 'INFO', 'DEBUG', 'TRACE'] as const

/** Kinds that get their own dedicated renderer (not the generic parseLine flow). */
const STRUCTURED_KINDS = new Set(['lab_events', 'jsonl', 'json'])

/** Kinds that should auto-load on selection. */
const AUTO_LOAD_KINDS = new Set(['tracing_jsonl', 'jsonl', 'json', 'qlog', 'lab_events'])

interface Props {
  base: string
  logs: SimLogEntry[]
  jumpTarget?: { node: string; path: string; timeLabel: string; nonce: number } | null
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

function formatObjectFields(
  obj: Record<string, unknown>,
  exclude: Set<string> = new Set(),
): string {
  return Object.entries(obj)
    .filter(([k]) => !exclude.has(k))
    .map(([k, val]) => `${k}=${valueString(val)}`)
    .join(' ')
}

function objectPairs(
  obj: Record<string, unknown>,
  exclude: Set<string> = new Set(),
): Array<{ key: string; value: string }> {
  return Object.entries(obj)
    .filter(([k]) => !exclude.has(k))
    .map(([k, val]) => ({ key: k, value: valueString(val) }))
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

/** Parse a single line from a tracing/text log. */
function parseLine(raw: string): ParsedLine {
  const stripped = raw.replace(ANSI_RE, '')

  // Try JSON parse first
  try {
    const v = JSON.parse(stripped) as Record<string, unknown>

    // JSON event format: { kind: "...", ... }
    if (typeof v.kind === 'string') return { type: 'event', kind: v.kind, raw: stripped }

    // tracing-subscriber JSON format
    if (typeof v.level === 'string' && typeof v.timestamp === 'string') {
      const level = (v.level as string).toUpperCase()
      const ts = v.timestamp as string
      const target = (v.target as string) ?? ''
      const fieldsObj = v.fields as Record<string, unknown> | undefined
      const msg = fieldsObj?.message != null ? String(fieldsObj.message) : ''
      const extras = fieldsObj ? formatObjectFields(fieldsObj, new Set(['message'])) : ''
      const spans = formatSpans(v.spans)
      return { type: 'tracing', level, ts, target, spans, msg, fields: extras }
    }
  } catch { }

  // ANSI tracing format
  const m = stripped.match(TRACING_RE)
  if (m) {
    const ts = m[1]
    const level = m[2]
    const rest = m[3]
    const withSpan = rest.match(TARGET_WITH_SPAN_RE)
    if (withSpan && withSpan[2].includes('::')) {
      return { type: 'tracing', ts, level, spans: withSpan[1], target: withSpan[2], msg: withSpan[3]?.trim() ?? '', fields: '' }
    }
    const basic = rest.match(TARGET_AND_MSG_RE)
    if (basic) {
      return { type: 'tracing', ts, level, spans: '', target: basic[1], msg: basic[2], fields: '' }
    }
    return { type: 'tracing', ts, level, target: rest, spans: '', msg: '', fields: '' }
  }

  return { type: 'raw', raw: stripped }
}

function parseQlogEvents(text: string): QlogEvent[] {
  const out: QlogEvent[] = []
  for (const line of text.split('\n')) {
    const s = line.trim().replace(/^\x1e/, '')
    if (!s) continue
    try {
      const v = JSON.parse(s) as Record<string, unknown>
      const name = typeof v.name === 'string' ? v.name : undefined
      const dataObj =
        typeof v.data === 'object' && v.data != null && !Array.isArray(v.data)
          ? (v.data as Record<string, unknown>)
          : {}
      const topLevel = objectPairs(v, new Set(['time', 'name', 'data']))
      const dataDetails = objectPairs(dataObj).map((p) => ({
        key: `data.${p.key}`,
        value: p.value,
      }))
      out.push({
        time: typeof v.time === 'number' ? v.time : undefined,
        name,
        filterName: name ?? 'meta',
        fieldPairs: [...topLevel, ...dataDetails],
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
  const [showSpans, setShowSpans] = useState(true)
  const [showTarget, setShowTarget] = useState(true)
  const [timeMode, setTimeMode] = useState<TimeMode>('absolute')
  const [qlogNameFilter, setQlogNameFilter] = useState('all')

  const isStructured = active != null && STRUCTURED_KINDS.has(active.kind)
  const isTracingLog = active?.kind === 'tracing_jsonl'
  const isQlog = active?.kind === 'qlog'

  // Auto-select first log
  useEffect(() => {
    setActive((prev) => {
      if (prev && logs.some((l) => l.path === prev.path)) return prev
      return logs[0] ?? null
    })
  }, [logs])

  // Clear content state when switching active log.
  useEffect(() => {
    if (!active) return
    setLoaded(false)
    setText('')
    setError(null)
    setRenderMode(isQlog ? 'rendered' : 'raw')
    if (!jumpingRef.current) {
      setJumpNeedle(null)
      setJumpLine(null)
    }
    jumpingRef.current = false
    setSearchQuery('')
    setSearchMatches([])
    setSearchIdx(0)
    setQlogNameFilter('all')
  }, [active, base])

  // Handle jump target from timeline
  useEffect(() => {
    if (!jumpTarget || logs.length === 0) return
    if (jumpHandledNonce === jumpTarget.nonce) return
    const direct = logs.find((l) => l.path === jumpTarget.path)
    const tracingLog = logs.find((l) => l.node === jumpTarget.node && l.kind === 'tracing_jsonl')
    const fallback = logs.find((l) => l.node === jumpTarget.node) ?? logs[0] ?? null
    jumpingRef.current = true
    setActive(tracingLog ?? direct ?? fallback)
    setJumpNeedle(jumpTarget.timeLabel)
    setJumpHandledNonce(jumpTarget.nonce)
  }, [jumpTarget, logs, jumpHandledNonce])

  // Load log content
  const loadContent = async () => {
    if (!active) return
    const url = `${base}${active.path}`
    setLoading(true)
    setError(null)
    try {
      const rangeRes = await fetch(url, { headers: { Range: `bytes=-${PREVIEW_BYTES}` } })
      if (rangeRes.ok || rangeRes.status === 206) {
        setText(await rangeRes.text())
        setLoaded(true)
        return
      }
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

  // Auto-load structured logs immediately
  useEffect(() => {
    if (!active || loaded || loading) return
    if (AUTO_LOAD_KINDS.has(active.kind)) loadContent()
  }, [active, loaded, loading])

  const byNode = useMemo(() => {
    const m = new Map<string, SimLogEntry[]>()
    for (const log of logs) {
      if (!m.has(log.node)) m.set(log.node, [])
      m.get(log.node)!.push(log)
    }
    return [...m.entries()].sort((a, b) => a[0].localeCompare(b[0]))
  }, [logs])

  // ── Tracing/text log parsing (only for non-structured kinds) ──

  const parsed = useMemo(() => {
    if (isStructured) return []
    return text.split('\n').filter(Boolean).map(parseLine)
  }, [text, isStructured])

  const qlogEvents = useMemo(() => isQlog ? parseQlogEvents(text) : [], [text, isQlog])
  const qlogNames = useMemo(() => [...new Set(qlogEvents.map((ev) => ev.filterName))].sort(), [qlogEvents])
  const filteredQlogEvents = useMemo(() => {
    if (qlogNameFilter === 'all') return qlogEvents
    return qlogEvents.filter((ev) => ev.filterName === qlogNameFilter)
  }, [qlogEvents, qlogNameFilter])

  const traceStartMs = useMemo(() => {
    for (const line of parsed) {
      if (line.type !== 'tracing') continue
      const ms = Date.parse(line.ts)
      if (!Number.isNaN(ms)) return ms
    }
    return null
  }, [parsed])

  const filteredLines = useMemo(() => {
    if (!isTracingLog) return parsed.map((line, i) => ({ line, origIdx: i }))
    return parsed
      .map((line, i) => ({ line, origIdx: i }))
      .filter(({ line }) => {
        if (line.type === 'tracing') return enabledLevels.has(line.level)
        return true
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
    const needleMs = Date.parse(jumpNeedle)
    let nearestIdx = -1
    let nearestDelta = Number.POSITIVE_INFINITY
    if (!Number.isNaN(needleMs)) {
      filteredLines.forEach(({ line }, idx) => {
        if (line.type !== 'tracing') return
        const tsMs = Date.parse(line.ts)
        if (Number.isNaN(tsMs)) return
        const delta = Math.abs(tsMs - needleMs)
        if (delta < nearestDelta) {
          nearestDelta = delta
          nearestIdx = idx
        }
      })
      if (nearestIdx >= 0 && nearestDelta <= 2000) {
        setJumpLine(nearestIdx)
        return
      }
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
    if (el instanceof HTMLElement) el.scrollIntoView({ block: 'center' })
  }, [jumpLine])

  // Scroll to search match
  useEffect(() => {
    if (searchMatches.length === 0) return
    const targetIdx = searchMatches[searchIdx]
    if (targetIdx == null) return
    const el = document.querySelector(`[data-log-line="${targetIdx}"]`)
    if (el instanceof HTMLElement) el.scrollIntoView({ block: 'center' })
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
  const displayTs = (ts: string): string => {
    if (timeMode === 'relative') {
      const ms = Date.parse(ts)
      if (!Number.isNaN(ms) && traceStartMs != null) {
        const delta = Math.max(0, ms - traceStartMs)
        return `+${(delta / 1000).toFixed(3)}s`
      }
    }
    return ts.split('T')[1]?.replace('Z', '') ?? ts
  }

  // ── Structured content renderers ──

  const renderJson = () => {
    try {
      const data = JSON.parse(text)
      return (
        <div className="json-tree-wrap">
          <JsonTree data={data} defaultDepth={2} />
        </div>
      )
    } catch {
      return <div className="logs-content"><div className="log-entry log-raw">{text}</div></div>
    }
  }

  const renderJsonLines = () => {
    const isLabEvents = active?.kind === 'lab_events'
    return (
      <JsonLinesTable
        text={text}
        kindField="kind"
        excludeFields={isLabEvents ? ['opid'] : []}
      />
    )
  }

  // ── Render ──

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
              {loaded && !isStructured && (
                <span style={{ color: 'var(--text-muted)' }}>
                  {formatBytes(fileSize)} · {parsed.length} lines
                  {isTracingLog && filteredLines.length !== parsed.length
                    ? ` (${filteredLines.length} shown)`
                    : ''}
                </span>
              )}
              {isQlog && loaded && (
                <>
                  <button className={`btn${renderMode === 'rendered' ? ' active' : ''}`} onClick={() => setRenderMode('rendered')}>preview</button>
                  <button className={`btn${renderMode === 'raw' ? ' active' : ''}`} onClick={() => setRenderMode('raw')}>raw</button>
                </>
              )}
              {!loaded && !loading && (
                <button className="btn" onClick={loadContent}>load log</button>
              )}
              {loading && <span style={{ color: 'var(--text-muted)' }}>loading...</span>}
              {jumpNeedle && (
                <span style={{ color: 'var(--yellow)' }}>
                  jump: {jumpNeedle} {jumpLine == null && loaded ? '(not in range)' : ''}
                </span>
              )}
            </div>

            {/* Tracing log toolbar */}
            {isTracingLog && loaded && (
              <div className="logs-toolbar">
                <button className={`btn${showSpans ? ' active' : ''}`} onClick={() => setShowSpans((v) => !v)}>
                  {showSpans ? 'hide spans' : 'show spans'}
                </button>
                <button className={`btn${showTarget ? ' active' : ''}`} onClick={() => setShowTarget((v) => !v)}>
                  {showTarget ? 'hide target' : 'show target'}
                </button>
                <button className="btn" onClick={() => setTimeMode((v) => (v === 'absolute' ? 'relative' : 'absolute'))}>
                  time: {timeMode}
                </button>
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
                    <button className="btn" onClick={() => setSearchIdx((i) => (i - 1 + searchMatches.length) % searchMatches.length)}>prev</button>
                    <button className="btn" onClick={() => setSearchIdx((i) => (i + 1) % searchMatches.length)}>next</button>
                  </>
                )}
              </div>
            )}

            {/* Qlog filter toolbar */}
            {isQlog && loaded && renderMode === 'rendered' && (
              <div className="logs-toolbar">
                <span>qlog event</span>
                <button className={`btn${qlogNameFilter === 'all' ? ' active' : ''}`} onClick={() => setQlogNameFilter('all')}>all</button>
                {qlogNames.map((name) => (
                  <button key={name} className={`btn${qlogNameFilter === name ? ' active' : ''}`} onClick={() => setQlogNameFilter(name)}>{name}</button>
                ))}
                <span style={{ color: 'var(--text-muted)', marginLeft: 'auto' }}>
                  {filteredQlogEvents.length}/{qlogEvents.length} shown
                </span>
              </div>
            )}

            {!loaded && !loading && <div className="empty">load log to view this file</div>}

            {/* Structured kinds: json, jsonl, lab_events */}
            {loaded && active.kind === 'json' && renderJson()}
            {loaded && (active.kind === 'lab_events' || active.kind === 'jsonl') && renderJsonLines()}

            {/* Qlog rendered view */}
            {loaded && isQlog && renderMode === 'rendered' && (
              <div className="tbl-wrap">
                <table>
                  <thead>
                    <tr><th>time</th><th>name</th><th>details</th></tr>
                  </thead>
                  <tbody>
                    {filteredQlogEvents.map((ev, i) => (
                      <tr key={i}>
                        <td>{ev.time ?? '—'}</td>
                        <td>{ev.filterName}</td>
                        <td><KvPairs pairs={ev.fieldPairs} /></td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}

            {/* Tracing / text / raw log view */}
            {loaded && !isStructured && !(isQlog && renderMode === 'rendered') && (
              <div className="logs-content" ref={contentRef}>
                {filteredLines.map(({ line, origIdx }, i) => {
                  const isSearchHit = searchMatches.includes(i)
                  const isCurrentSearch = searchMatches[searchIdx] === i
                  const isJump = jumpLine === i
                  const highlight = isJump ? ' jump-hit' : isCurrentSearch ? ' search-current' : isSearchHit ? ' search-hit' : ''

                  if (line.type === 'tracing') {
                    return (
                      <div key={origIdx} data-log-line={i} className={`log-entry${highlight}`}>
                        <span className="log-ts">{displayTs(line.ts)}</span>
                        <span className={`level-${line.level}`}>{line.level.padStart(5)}</span>
                        {showSpans && line.spans && <span className="log-spans"> {line.spans}</span>}
                        {showTarget && <span className="log-target">{showSpans && line.spans ? ': ' : ' '}{line.target}:</span>}
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
