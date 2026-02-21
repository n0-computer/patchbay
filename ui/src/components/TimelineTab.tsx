import { useEffect, useMemo, useState } from 'react'
import type { SimLogEntry } from '../types'

type EventRow = {
  node: string
  t: number
  label: string
}

interface Props {
  base: string
  logs: SimLogEntry[]
}

const TRACING_RE = /^(\d{4}-\d{2}-\d{2}T[\d:.]+Z)\s+(ERROR|WARN|INFO|DEBUG|TRACE)\s+([^\s:]+):\s*(.*)/

function parseTransferEvents(node: string, text: string): EventRow[] {
  const out: EventRow[] = []
  let idx = 0
  for (const line of text.split('\n')) {
    const s = line.trim()
    if (!s) continue
    try {
      const v = JSON.parse(s) as Record<string, unknown>
      if (typeof v.kind === 'string') {
        out.push({ node, t: idx * 10, label: String(v.kind) })
        idx++
        continue
      }
    } catch { }
    const m = s.match(TRACING_RE)
    if (m) {
      const ts = Date.parse(m[1])
      out.push({ node, t: Number.isFinite(ts) ? ts : idx * 10, label: `${m[2]} ${m[4]}` })
      idx++
    }
  }
  return out
}

export default function TimelineTab({ base, logs }: Props) {
  const [events, setEvents] = useState<EventRow[]>([])

  useEffect(() => {
    let dead = false
    const transferLogs = logs.filter((l) => l.kind === 'transfer')
    Promise.all(
      transferLogs.map(async (log) => {
        const r = await fetch(`${base}${log.path}`)
        if (!r.ok) return [] as EventRow[]
        const text = await r.text()
        return parseTransferEvents(log.node, text)
      }),
    ).then((rows) => {
      if (!dead) setEvents(rows.flat())
    })
    return () => {
      dead = true
    }
  }, [base, logs])

  const nodes = useMemo(() => [...new Set(events.map((e) => e.node))], [events])
  const minT = useMemo(() => (events.length ? Math.min(...events.map((e) => e.t)) : 0), [events])
  const maxT = useMemo(() => (events.length ? Math.max(...events.map((e) => e.t)) : 1), [events])
  const span = Math.max(1, maxT - minT)

  if (events.length === 0) {
    return <div className="empty">no transfer timeline data yet</div>
  }

  return (
    <div className="timeline-main">
      <div className="tbl-wrap">
        <table>
          <thead>
            <tr>
              <th>node</th>
              <th>t(ms)</th>
              <th>event</th>
              <th>bar</th>
            </tr>
          </thead>
          <tbody>
            {events
              .slice()
              .sort((a, b) => a.t - b.t)
              .map((e, i) => {
                const pct = ((e.t - minT) / span) * 100
                return (
                  <tr key={i}>
                    <td>{e.node}</td>
                    <td>{Math.round(e.t - minT)}</td>
                    <td>{e.label}</td>
                    <td>
                      <div style={{ position: 'relative', height: 8, background: 'var(--surface2)' }}>
                        <div style={{ position: 'absolute', left: `${pct}%`, top: 0, width: 2, height: 8, background: 'var(--accent)' }} />
                      </div>
                    </td>
                  </tr>
                )
              })}
          </tbody>
        </table>
      </div>
      <div style={{ marginTop: 8, color: 'var(--text-muted)' }}>{nodes.length} nodes</div>
    </div>
  )
}
