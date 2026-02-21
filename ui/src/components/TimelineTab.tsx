import { useEffect, useMemo, useState } from 'react'
import type { SimLogEntry } from '../types'

const PREVIEW_BYTES = 256 * 1024
const ANSI_RE = /\x1b\[[0-9;]*m/g
const TRACING_RE = /^(\d{4}-\d{2}-\d{2}T[\d:.]+Z)\s+(ERROR|WARN|INFO|DEBUG|TRACE)\s+(.+?):\s*(.*)$/

type EventRow = {
  order: number
  node: string
  path: string
  kind: string
  details: string
  fields: string
  timeLabel: string
  timeMs: number | null
}

interface Props {
  base: string
  logs: SimLogEntry[]
  onJumpToLog?: (target: { node: string; path: string; timeLabel: string }) => void
}

function tryParseJsonEvent(line: string): { kind: string; fields: string; timeLabel: string } | null {
  try {
    const v = JSON.parse(line) as Record<string, unknown>
    if (typeof v.kind !== 'string') return null
    const fields = Object.entries(v)
      .filter(([k]) => k !== 'kind')
      .map(([k, val]) => `${k}=${typeof val === 'string' ? val : JSON.stringify(val)}`)
      .join(' ')
    const timeLabel = typeof v.time === 'number' ? String(v.time) : ''
    return { kind: v.kind, fields, timeLabel }
  } catch {
    return null
  }
}

function parseIsoMs(value: string): number | null {
  const ms = Date.parse(value)
  return Number.isFinite(ms) ? ms : null
}

function parseIrohEventKind(fragment: string): string | null {
  const marker = 'iroh::_events'
  const idx = fragment.indexOf(marker)
  if (idx < 0) return null
  let rest = fragment.slice(idx + marker.length).trim()
  rest = rest.replace(/^:+/, '')
  rest = rest.split(/[{:]/)[0]?.trim() ?? ''
  return rest || 'event'
}

function parseIrohFields(fragment: string): string {
  const marker = 'iroh::_events'
  const idx = fragment.indexOf(marker)
  if (idx < 0) return ''
  const tail = fragment.slice(idx)
  const afterKind = tail.replace(/^iroh::_events(?:::[^:\s{]+)*/, '')
  const out: string[] = []
  const re = /\b([a-zA-Z_][a-zA-Z0-9_]*)=(.*?)(?=\s+[a-zA-Z_][a-zA-Z0-9_]*=|$)/g
  let m: RegExpExecArray | null = null
  while ((m = re.exec(afterKind)) != null) {
    out.push(`${m[1]}=${m[2].trim()}`)
  }
  return out.join(' ')
}

function parseLogEvents(node: string, path: string, text: string, offset: number): EventRow[] {
  const out: EventRow[] = []
  let idx = 0
  for (const raw of text.split('\n')) {
    const line = raw.trim()
    if (!line) continue
    const stripped = line.replace(ANSI_RE, '')

    const jsonEv = tryParseJsonEvent(stripped)
    if (jsonEv) {
      out.push({
        order: offset + idx++,
        node,
        path,
        kind: jsonEv.kind,
        details: stripped,
        fields: jsonEv.fields,
        timeLabel: jsonEv.timeLabel,
        timeMs: typeof jsonEv.timeLabel === 'string' && jsonEv.timeLabel ? Number(jsonEv.timeLabel) : null,
      })
      continue
    }

    const m = stripped.match(TRACING_RE)
    if (!m) continue
    const target = m[3]
    const msg = m[4]
    const fragment = `${target}: ${msg}`
    const kind = parseIrohEventKind(fragment)
    if (!kind) continue
    const kv = parseIrohFields(fragment)
    out.push({
      order: offset + idx++,
      node,
      path,
      kind,
      details: stripped,
      fields: kv,
      timeLabel: m[1],
      timeMs: parseIsoMs(m[1]),
    })
  }
  return out
}

export default function TimelineTab({ base, logs, onJumpToLog }: Props) {
  const [events, setEvents] = useState<EventRow[]>([])
  const [selected, setSelected] = useState<EventRow | null>(null)
  const [timeMode, setTimeMode] = useState<'relative' | 'absolute'>('relative')

  const candidateLogs = useMemo(
    () => logs.filter((l) => l.kind !== 'qlog' && (l.kind === 'transfer' || l.path.endsWith('/out.log'))),
    [logs],
  )
  const candidateKey = useMemo(() => candidateLogs.map((l) => `${l.node}:${l.path}:${l.kind}`).join('|'), [candidateLogs])

  useEffect(() => {
    let dead = false
    const prev = selected
    Promise.all(
      candidateLogs.map(async (log, i) => {
        const r = await fetch(`${base}${log.path}`, {
          headers: { Range: `bytes=-${PREVIEW_BYTES}` },
        })
        if (!r.ok && r.status !== 206) return [] as EventRow[]
        const text = await r.text()
        return parseLogEvents(log.node, log.path, text, i * 10000)
      }),
    ).then((rows) => {
      if (!dead) {
        const flat = rows.flat().sort((a, b) => a.order - b.order)
        setEvents(flat)
        if (!flat.length) {
          setSelected(null)
          return
        }
        if (prev) {
          const keep = flat.find(
            (e) => e.node === prev.node && e.path === prev.path && e.kind === prev.kind && e.details === prev.details,
          )
          setSelected(keep ?? flat[0])
          return
        }
        setSelected(flat[0])
      }
    })
    return () => {
      dead = true
    }
  }, [base, candidateKey])

  const nodes = useMemo(() => [...new Set(events.map((e) => e.node))].sort(), [events])
  const firstTimeMs = useMemo(() => {
    const withTime = events.map((e) => e.timeMs).filter((v): v is number => v != null)
    return withTime.length ? Math.min(...withTime) : null
  }, [events])

  const displayTime = (ev: EventRow): string => {
    if (timeMode === 'absolute') return ev.timeLabel || ''
    if (ev.timeMs == null || firstTimeMs == null) return ''
    const delta = Math.max(0, ev.timeMs - firstTimeMs)
    return `+${(delta / 1000).toFixed(3)}s`
  }

  if (events.length === 0 || nodes.length === 0) {
    return <div className="empty">no timeline events yet</div>
  }

  return (
    <div className="timeline-grid-layout">
      <div className="logs-toolbar">
        <span>timeline</span>
        <button className="btn" onClick={() => setTimeMode((v) => (v === 'relative' ? 'absolute' : 'relative'))}>
          time: {timeMode}
        </button>
      </div>
      <div className="timeline-grid-scroll">
        <table className="timeline-grid-table">
          <thead>
            <tr>
              <th>time</th>
              {nodes.map((node) => (
                <th key={node}>{node}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            {events.map((ev, row) => (
              <tr key={row}>
                <td className="timeline-time-cell">{displayTime(ev)}</td>
                {nodes.map((node) => {
                  const isCell = node === ev.node
                  const selectedCell = isCell && selected === ev
                  return (
                    <td
                      key={`${row}-${node}`}
                      className={`timeline-event-cell${isCell ? ' has-event' : ''}${selectedCell ? ' selected' : ''}`}
                      onClick={() => isCell && setSelected(ev)}
                    >
                      {isCell ? ev.kind : ''}
                    </td>
                  )
                })}
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      <div className="timeline-detail-pane">
        {selected ? (
          <>
            <div className="section-header">
              <span>{selected.node} · {selected.kind}</span>
              {selected.timeLabel && <span className="timeline-detail-time">{selected.timeLabel}</span>}
              {onJumpToLog && selected.timeLabel && (
                <button
                  className="btn"
                  style={{ marginLeft: 'auto' }}
                  onClick={() => onJumpToLog({ node: selected.node, path: selected.path, timeLabel: selected.timeLabel })}
                >
                  jump to logs
                </button>
              )}
            </div>
            <div className="timeline-detail-body">
              <div className="timeline-detail-fields">{selected.fields || '(no parsed fields)'}</div>
              <div className="timeline-detail-raw">{selected.details}</div>
            </div>
          </>
        ) : (
          <div className="empty">hover/click an event cell to inspect details</div>
        )}
      </div>
    </div>
  )
}
