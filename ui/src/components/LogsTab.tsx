import { useState, useEffect, useMemo, useCallback } from 'react'
import type { Manifest, ManifestLog, LogLine, TracingEntry, IrohEvent, LogLevel } from '../types'

// ── ANSI stripping ─────────────────────────────────────────────────────────────
const ANSI_RE = /\x1b\[[0-9;]*m/g
const stripAnsi = (s: string) => s.replace(ANSI_RE, '')

// ── Tracing text line parser ───────────────────────────────────────────────────
// Format after ANSI strip: "TIMESTAMP  LEVEL target: message"
// with optional key=val or key="val" fields after a newline in multiline entries
const TRACING_RE = /^(\d{4}-\d{2}-\d{2}T[\d:.]+Z)\s+(ERROR|WARN|INFO|DEBUG|TRACE)\s+([^\s:]+):\s*(.*)/

function parseTracingLine(raw: string): LogLine {
  const stripped = stripAnsi(raw.trimEnd())
  const m = stripped.match(TRACING_RE)
  if (m) {
    return {
      type: 'tracing',
      raw,
      timestamp: m[1],
      level: m[2] as LogLevel,
      target: m[3],
      message: m[4],
    }
  }
  return { type: 'raw', raw: stripped }
}

function parseIrohLine(raw: string): LogLine {
  const trimmed = raw.trim()
  if (!trimmed) return { type: 'raw', raw: '' }
  try {
    const obj = JSON.parse(trimmed) as Record<string, unknown>
    if (typeof obj.kind === 'string') {
      return { type: 'iroh', raw, kind: obj.kind, ...obj }
    }
  } catch { /* fall through */ }
  return { type: 'raw', raw: trimmed }
}

function parseLines(text: string, kind: ManifestLog['kind']): LogLine[] {
  const lines = text.split('\n')
  if (kind === 'iroh-ndjson') {
    return lines.map(parseIrohLine)
  }
  // tracing-ansi or text: handle multiline tracing entries (continuation lines
  // that don't start with a timestamp get appended to message)
  const out: LogLine[] = []
  for (const line of lines) {
    if (!line.trim()) continue
    const parsed = parseTracingLine(line)
    if (parsed.type === 'tracing' || out.length === 0) {
      out.push(parsed)
    } else {
      // Continuation line — append to previous entry's message
      const prev = out[out.length - 1]
      if (prev.type === 'tracing') {
        prev.message += '\n' + stripAnsi(line.trimEnd())
      } else if (prev.type === 'raw') {
        prev.raw += '\n' + stripAnsi(line.trimEnd())
      }
    }
  }
  return out
}

// ── Rendering ─────────────────────────────────────────────────────────────────

const LEVELS: LogLevel[] = ['ERROR', 'WARN', 'INFO', 'DEBUG', 'TRACE']

function IrohBadge({ event }: { event: IrohEvent }) {
  const { kind } = event
  if (kind === 'ConnectionTypeChanged') {
    const addr = String((event as { addr?: string }).addr ?? '')
    const isDirect = addr.includes('Ip(') || addr.includes('"Ip"')
    return isDirect
      ? <span className="badge badge-green">⚡ DIRECT</span>
      : <span className="badge badge-grey">↔ RELAY</span>
  }
  if (kind === 'DownloadComplete') {
    const dur = (event as { duration?: number }).duration
    const size = (event as { size?: number }).size
    const mbps = dur && size ? ((size * 8) / (dur / 1e6) / 1e6).toFixed(1) : null
    return <span className="badge badge-blue">✓ DONE{mbps ? `  ${mbps} Mbit/s` : ''}</span>
  }
  return <span className="badge badge-grey">{kind}</span>
}

function formatTimestamp(ts: string): string {
  // show only time part for compact display
  const t = ts.split('T')[1]
  return t ? t.replace('Z', '') : ts
}

function LogLineView({ line, show }: { line: LogLine; show: boolean }) {
  if (!show) return null

  if (line.type === 'iroh') {
    const isIrohEvent = line.kind === 'ConnectionTypeChanged' || line.kind === 'DownloadComplete'
    return (
      <div className={`log-entry${isIrohEvent ? ' log-iroh-events' : ''}`}>
        <IrohBadge event={line} />
        {' '}
        <span style={{ color: 'var(--text-muted)', fontSize: 11 }}>
          {Object.entries(line)
            .filter(([k]) => !['type', 'raw', 'kind'].includes(k))
            .map(([k, v]) => `${k}=${JSON.stringify(v)}`)
            .join(' ')
          }
        </span>
      </div>
    )
  }

  if (line.type === 'tracing') {
    const l = line as TracingEntry
    const isIrohSpan = l.target.includes('iroh::_events')
    const parts = l.message.split('\n')
    const firstLine = parts[0]
    const rest = parts.slice(1).join('\n')
    return (
      <div className={`log-entry${isIrohSpan ? ' log-iroh-events' : ''}`}>
        <span className="log-ts">{formatTimestamp(l.timestamp)}</span>
        <span className={`level-${l.level}`} style={{ marginRight: 8, fontWeight: 600, minWidth: 36, display: 'inline-block' }}>
          {l.level.slice(0, 4)}
        </span>
        <span className="log-target">{l.target}:</span>
        <span className="log-msg">{firstLine}</span>
        {rest && <div className="log-fields">{rest}</div>}
      </div>
    )
  }

  // raw
  const raw = line.raw.trim()
  if (!raw) return null
  return (
    <div className="log-entry">
      <span className="log-raw">{raw}</span>
    </div>
  )
}

// ── File loading ───────────────────────────────────────────────────────────────

interface LoadedFile {
  path: string
  kind: ManifestLog['kind']
  lines: LogLine[]
  truncated: boolean
}

const MAX_LINES = 20_000

async function loadLogFile(path: string, kind: ManifestLog['kind']): Promise<LoadedFile> {
  const r = await fetch(path)
  if (!r.ok) throw new Error(`HTTP ${r.status}`)
  const text = await r.text()
  const all = parseLines(text, kind)
  const truncated = all.length > MAX_LINES
  return { path, kind, lines: truncated ? all.slice(0, MAX_LINES) : all, truncated }
}

// ── Component ─────────────────────────────────────────────────────────────────

export default function LogsTab({ manifest, base }: { manifest: Manifest; base: string }) {
  const [activeFile, setActiveFile] = useState<ManifestLog | null>(null)
  const [loaded, setLoaded] = useState<LoadedFile | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)

  const [filter, setFilter] = useState('')
  const [enabledLevels, setEnabledLevels] = useState<Set<LogLevel>>(new Set(LEVELS))
  const [irohOnly, setIrohOnly] = useState(false)

  // Select first non-qlog file on mount
  useEffect(() => {
    const first = manifest.logs.find(l => l.kind !== 'qlog-dir')
    if (first) setActiveFile(first)
  }, [manifest])

  const selectFile = useCallback(async (log: ManifestLog) => {
    if (log.kind === 'qlog-dir') return
    setActiveFile(log)
    setLoaded(null)
    setLoadError(null)
    setLoading(true)
    try {
      const result = await loadLogFile(log.path, log.kind)
      setLoaded(result)
    } catch (e) {
      setLoadError(String(e))
    } finally {
      setLoading(false)
    }
  }, [])

  // Auto-load when activeFile changes
  useEffect(() => {
    if (activeFile) selectFile(activeFile)
  }, [activeFile]) // eslint-disable-line

  const toggleLevel = (l: LogLevel) => {
    setEnabledLevels(s => {
      const next = new Set(s)
      next.has(l) ? next.delete(l) : next.add(l)
      return next
    })
  }

  // Group sidebar entries by node
  const groups = useMemo(() => {
    const m = new Map<string, ManifestLog[]>()
    for (const log of manifest.logs) {
      if (!m.has(log.node)) m.set(log.node, [])
      m.get(log.node)!.push(log)
    }
    return [...m.entries()]
  }, [manifest])

  const filterRe = useMemo(() => {
    if (!filter) return null
    try { return new RegExp(filter, 'i') } catch { return null }
  }, [filter])

  const visibleLines = useMemo(() => {
    if (!loaded) return []
    return loaded.lines.filter(line => {
      // Level filter (only applies to tracing lines)
      if (line.type === 'tracing' && !enabledLevels.has(line.level)) return false
      // Iroh-only toggle
      if (irohOnly) {
        if (line.type === 'iroh') {
          const isKnown = ['ConnectionTypeChanged', 'DownloadComplete'].includes(line.kind)
          if (!isKnown) return false
        } else if (line.type === 'tracing') {
          if (!line.target.includes('iroh::_events')) return false
        } else {
          return false
        }
      }
      // Text filter
      if (filterRe) {
        const text = line.type === 'tracing'
          ? `${line.target} ${line.message}`
          : line.type === 'iroh'
            ? line.raw
            : line.raw
        if (!filterRe.test(text)) return false
      }
      return true
    })
  }, [loaded, enabledLevels, irohOnly, filterRe])

  const filename = (path: string) => path.split('/').pop() ?? path

  return (
    <div className="logs-layout">
      {/* Sidebar */}
      <div className="logs-sidebar">
        {groups.map(([node, logs]) => (
          <div key={node} className="node-group">
            <div className="node-label">{node}</div>
            {logs.filter(l => l.kind !== 'qlog-dir').map(l => (
              <div
                key={l.path}
                className={`file-item${activeFile?.path === l.path ? ' active' : ''}`}
                onClick={() => selectFile(l)}
                title={l.path}
              >
                {filename(l.path)}
                {' '}
                <span style={{ fontSize: 10, color: 'var(--text-muted)' }}>
                  {l.kind === 'iroh-ndjson' ? 'iroh' : 'log'}
                </span>
              </div>
            ))}
          </div>
        ))}
      </div>

      {/* Main */}
      <div className="logs-main">
        <div className="logs-toolbar">
          <input
            type="search"
            placeholder="filter (regex)…"
            value={filter}
            onChange={e => setFilter(e.target.value)}
          />
          {LEVELS.map(l => (
            <span
              key={l}
              className={`level-toggle level-${l} ${enabledLevels.has(l) ? 'on' : 'off'}`}
              onClick={() => toggleLevel(l)}
              title={`toggle ${l}`}
            >{l[0]}</span>
          ))}
          <button
            className={`btn${irohOnly ? ' active' : ''}`}
            onClick={() => setIrohOnly(v => !v)}
            title="show only iroh events and iroh::_events spans"
          >iroh events</button>
          {loaded && (
            <span style={{ color: 'var(--text-muted)', fontSize: 11, marginLeft: 'auto' }}>
              {visibleLines.length}/{loaded.lines.length} lines
              {loaded.truncated && ' (truncated)'}
            </span>
          )}
        </div>

        <div className="logs-content">
          {loading && <div className="loading">loading…</div>}
          {loadError && <div className="error-msg">{loadError}</div>}
          {!loading && !loadError && loaded && visibleLines.length === 0 && (
            <div className="empty">no matching lines</div>
          )}
          {!loading && !loadError && loaded && visibleLines.map((line, i) => (
            <LogLineView key={i} line={line} show={true} />
          ))}
          {!loading && !loaded && !loadError && (
            <div className="empty">select a log file</div>
          )}
        </div>
      </div>
    </div>
  )
}
