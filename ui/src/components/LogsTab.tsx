import { useEffect, useMemo, useState } from 'react'
import type { SimLogEntry } from '../types'

const ANSI_RE = /\x1b\[[0-9;]*m/g
const TRACING_RE = /^(\d{4}-\d{2}-\d{2}T[\d:.]+Z)\s+(ERROR|WARN|INFO|DEBUG|TRACE)\s+([^\s:]+):\s*(.*)/

type ParsedLine =
  | { type: 'tracing'; level: string; ts: string; target: string; msg: string }
  | { type: 'event'; kind: string; raw: string }
  | { type: 'raw'; raw: string }

interface Props {
  base: string
  logs: SimLogEntry[]
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

function parseQlog(text: string): Array<{ time: number; name: string }> {
  const out: Array<{ time: number; name: string }> = []
  for (const line of text.split('\n')) {
    const trimmed = line.trim().replace(/^\x1e/, '')
    if (!trimmed) continue
    try {
      const v = JSON.parse(trimmed) as Record<string, unknown>
      if (typeof v.time === 'number' && typeof v.name === 'string') {
        out.push({ time: v.time, name: v.name })
      }
    } catch { }
  }
  return out
}

export default function LogsTab({ base, logs }: Props) {
  const [active, setActive] = useState<SimLogEntry | null>(null)
  const [text, setText] = useState('')
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    setActive(logs[0] ?? null)
  }, [logs])

  useEffect(() => {
    if (!active) return
    setError(null)
    setText('')
    fetch(`${base}${active.path}`)
      .then(async (r) => {
        if (!r.ok) throw new Error(`HTTP ${r.status}`)
        return r.text()
      })
      .then(setText)
      .catch((e) => setError(String(e)))
  }, [active, base])

  const byNode = useMemo(() => {
    const m = new Map<string, SimLogEntry[]>()
    for (const log of logs) {
      if (!m.has(log.node)) m.set(log.node, [])
      m.get(log.node)!.push(log)
    }
    return [...m.entries()].sort((a, b) => a[0].localeCompare(b[0]))
  }, [logs])

  const parsed = useMemo(() => text.split('\n').filter(Boolean).map(parseLine), [text])
  const qlogRows = useMemo(() => (active?.kind === 'qlog' ? parseQlog(text) : []), [active?.kind, text])

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
        {active?.kind === 'qlog' ? (
          <div className="tbl-wrap">
            <table>
              <thead><tr><th>time</th><th>name</th></tr></thead>
              <tbody>
                {qlogRows.map((r, i) => (
                  <tr key={i}><td>{r.time}</td><td>{r.name}</td></tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : (
          <div className="logs-content">
            {parsed.map((line, i) => {
              if (line.type === 'tracing') {
                return (
                  <div key={i} className="log-entry">
                    <span className="log-ts">{line.ts.split('T')[1]?.replace('Z', '')}</span>
                    <span className={`level-${line.level}`} style={{ marginRight: 8 }}>{line.level}</span>
                    <span className="log-target">{line.target}:</span>
                    <span className="log-msg">{line.msg}</span>
                  </div>
                )
              }
              if (line.type === 'event') {
                return <div key={i} className="log-entry log-iroh-events">{line.kind} {line.raw}</div>
              }
              return <div key={i} className="log-entry log-raw">{line.raw}</div>
            })}
          </div>
        )}
      </div>
    </div>
  )
}
