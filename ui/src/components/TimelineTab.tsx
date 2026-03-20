import { useEffect, useMemo, useState } from 'react'
import type { LabEvent } from '../devtools-types'
import type { SimLogEntry } from '../types'
import { parseIsoMs, formatTimestamp, formatRelativeTime, kvPairs } from '../time-format'
import KvPairs from './KvPairs'

const PREVIEW_BYTES = 256 * 1024
const ANSI_RE = /\x1b\[[0-9;]*m/g
const TRACING_RE = /^(\d{4}-\d{2}-\d{2}T[\d:.]+Z)\s+(ERROR|WARN|INFO|DEBUG|TRACE)\s+(.+?):\s*(.*)$/

type EventRow = {
  order: number
  node: string
  path: string
  kind: string
  details: string
  fieldPairs: Array<{ key: string; value: string }>
  timeLabel: string
  timeMs: number
}

interface Props {
  base: string
  logs: SimLogEntry[]
  labEvents?: LabEvent[]
  onJumpToLog?: (target: { node: string; path: string; timeLabel: string }) => void
}

function tryParseJsonEvent(line: string): { kind: string; fieldPairs: Array<{ key: string; value: string }>; timeLabel: string; timeMs: number } | null {
  try {
    const v = JSON.parse(line) as Record<string, unknown>
    if (typeof v.kind !== 'string') return null
    const pairs = kvPairs(v, ['kind'])
    const ts = typeof v.timestamp === 'string' ? v.timestamp : null
    if (ts) {
      const ms = parseIsoMs(ts)
      if (ms != null) return { kind: v.kind, fieldPairs: pairs.filter((p) => p.key !== 'timestamp'), timeLabel: ts, timeMs: ms }
    }
    return null
  } catch {
    return null
  }
}

/** Extract event kind from a tracing target containing `_events::`. */
function parseTracingEventKind(fragment: string): string | null {
  const match = fragment.match(/_events::([a-zA-Z0-9_:]+)/)
  if (!match) return null
  return (match[1] || 'event').replace(/:+$/, '')
}

/** Extract key=value pairs from a tracing line containing `_events::`. */
function parseTracingEventPairs(fragment: string): Array<{ key: string; value: string }> {
  const match = fragment.match(/_events::([a-zA-Z0-9_:]+)/)
  if (!match) return []
  const afterKind = fragment.slice(fragment.indexOf(match[0]) + match[0].length)
  const out: Array<{ key: string; value: string }> = []
  const re = /\b([a-zA-Z_][a-zA-Z0-9_]*)=(.*?)(?=\s+[a-zA-Z_][a-zA-Z0-9_]*=|$)/g
  let m: RegExpExecArray | null = null
  while ((m = re.exec(afterKind)) != null) {
    const v = m[2].trim()
    out.push({ key: m[1], value: v.startsWith('"') && v.endsWith('"') ? v.slice(1, -1) : v })
  }
  return out
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
        fieldPairs: jsonEv.fieldPairs,
        timeLabel: jsonEv.timeLabel,
        timeMs: jsonEv.timeMs,
      })
      continue
    }

    const m = stripped.match(TRACING_RE)
    if (!m) continue
    const target = m[3]
    const msg = m[4]
    const fragment = `${target}: ${msg}`
    const kind = parseTracingEventKind(fragment)
    if (!kind) continue
    const pairs = parseTracingEventPairs(fragment)
    const parsedMs = parseIsoMs(m[1])
    if (parsedMs == null) continue
    out.push({
      order: offset + idx++,
      node,
      path,
      kind,
      details: stripped,
      fieldPairs: pairs,
      timeLabel: m[1],
      timeMs: parsedMs,
    })
  }
  return out
}

/** Convert LabEvents into EventRows for the timeline. */
function labEventsToRows(events: LabEvent[]): EventRow[] {
  return events
    .filter((e) => e.timestamp)
    .map((e, i) => {
      const ms = parseIsoMs(e.timestamp)
      if (ms == null) return null
      const pairs = kvPairs(e as Record<string, unknown>, ['opid', 'timestamp', 'kind'])
      return {
        order: -10000 + i,
        node: '_lab',
        path: '',
        kind: e.kind,
        details: JSON.stringify(e),
        fieldPairs: pairs,
        timeLabel: e.timestamp,
        timeMs: ms,
      } satisfies EventRow
    })
    .filter((r): r is EventRow => r != null)
}

export default function TimelineTab({ base, logs, labEvents, onJumpToLog }: Props) {
  const [events, setEvents] = useState<EventRow[]>([])
  const [selected, setSelected] = useState<EventRow | null>(null)
  const [timeMode, setTimeMode] = useState<'relative' | 'absolute'>('relative')

  const candidateLogs = useMemo(() => {
    const all = logs.filter(
      (l) =>
        l.node !== '_run' &&
        (l.kind === 'jsonl' || l.kind === 'ansi_text' || l.kind === 'text'),
    )
    // If a node has a structured jsonl log, skip its text/ansi logs to avoid
    // duplicate events (the same _events:: entries appear in both formats).
    const nodesWithJsonl = new Set(all.filter((l) => l.kind === 'jsonl').map((l) => l.node))
    return all.filter((l) => l.kind === 'jsonl' || !nodesWithJsonl.has(l.node))
  }, [logs])
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
        const labRows = labEvents ? labEventsToRows(labEvents) : []
        const flat = [...rows.flat(), ...labRows]
          .sort((a, b) => a.timeMs - b.timeMs || a.order - b.order || a.node.localeCompare(b.node) || a.kind.localeCompare(b.kind))
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
  }, [base, candidateKey, labEvents])

  const nodes = useMemo(() => [...new Set(events.map((e) => e.node))].sort((a, b) => {
    // _lab always first
    if (a === '_lab') return -1
    if (b === '_lab') return 1
    return a.localeCompare(b)
  }), [events])
  const firstTimeMs = useMemo(() => {
    return events.length ? Math.min(...events.map((e) => e.timeMs)) : null
  }, [events])

  const displayTime = (ev: EventRow): string => {
    if (timeMode === 'absolute') return formatTimestamp(ev.timeLabel)
    if (ev.timeMs == null || firstTimeMs == null) return ''
    return formatRelativeTime(ev.timeMs, firstTimeMs)
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
              {selected.timeLabel && <span className="timeline-detail-time">{formatTimestamp(selected.timeLabel)}</span>}
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
              <div className="timeline-detail-fields"><KvPairs pairs={selected.fieldPairs} vertical /></div>
            </div>
          </>
        ) : (
          <div className="empty">click an event cell to inspect details</div>
        )}
      </div>
    </div>
  )
}
