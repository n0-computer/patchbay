import { useMemo, useState } from 'react'
import { parseIsoMs, formatTimestamp, formatRelativeTime } from '../time-format'
import KvPairs from './KvPairs'

interface Props {
  /** Raw JSONL text content. */
  text: string
  /** Field name to extract as the row "kind" badge (default: "kind"). */
  kindField?: string
  /** Field name that holds a timestamp (default: "timestamp"). */
  timeField?: string
  /** Additional fields to exclude from the details column. */
  excludeFields?: string[]
}

interface Row {
  idx: number
  kind: string
  timeLabel: string
  timeMs: number | null
  pairs: Array<{ key: string; value: string }>
}

function parseRows(text: string, kindField: string, timeField: string, exclude: Set<string>): Row[] {
  const rows: Row[] = []
  let idx = 0
  for (const line of text.split('\n')) {
    const trimmed = line.trim()
    if (!trimmed) continue
    try {
      const obj = JSON.parse(trimmed) as Record<string, unknown>
      const kind = typeof obj[kindField] === 'string' ? (obj[kindField] as string) : ''
      const ts = typeof obj[timeField] === 'string' ? (obj[timeField] as string) : ''
      const pairs = Object.entries(obj)
        .filter(([k]) => !exclude.has(k))
        .map(([k, v]) => ({ key: k, value: typeof v === 'string' ? v : JSON.stringify(v) }))
      rows.push({ idx: idx++, kind, timeLabel: ts, timeMs: parseIsoMs(ts), pairs })
    } catch {
      idx++
    }
  }
  return rows
}

export default function JsonLinesTable({ text, kindField = 'kind', timeField = 'timestamp', excludeFields = [] }: Props) {
  const [timeMode, setTimeMode] = useState<'relative' | 'absolute'>('relative')

  const exclude = useMemo(() => new Set([kindField, timeField, ...excludeFields]), [kindField, timeField, excludeFields])
  const rows = useMemo(() => parseRows(text, kindField, timeField, exclude), [text, kindField, timeField, exclude])
  const baseMs = useMemo(() => {
    for (const r of rows) {
      if (r.timeMs != null) return r.timeMs
    }
    return null
  }, [rows])

  const displayTime = (r: Row): string => {
    if (!r.timeLabel) return ''
    if (timeMode === 'absolute') return formatTimestamp(r.timeLabel)
    if (r.timeMs != null && baseMs != null) return formatRelativeTime(r.timeMs, baseMs)
    return r.timeLabel
  }

  if (rows.length === 0) return <div className="empty">no events</div>

  const hasTime = rows.some((r) => r.timeLabel)

  return (
    <div style={{ display: 'flex', flexDirection: 'column', flex: 1, overflow: 'hidden' }}>
      <div className="logs-toolbar">
        <span>{rows.length} events</span>
        {hasTime && (
          <button className="btn" onClick={() => setTimeMode((v) => (v === 'relative' ? 'absolute' : 'relative'))}>
            time: {timeMode}
          </button>
        )}
      </div>
      <div style={{ flex: 1, overflow: 'auto', fontSize: 12 }}>
        <table>
          <thead>
            <tr>
              {hasTime && <th style={{ width: 100 }}>time</th>}
              <th style={{ width: 120 }}>{kindField}</th>
              <th>details</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => (
              <tr key={r.idx}>
                {hasTime && <td className="events-time-cell">{displayTime(r)}</td>}
                <td><span className="events-kind">{r.kind}</span></td>
                <td><KvPairs pairs={r.pairs} /></td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  )
}
