import { useState, useEffect, useRef, useCallback } from 'react'
import type { Manifest, ManifestLog, SimResults } from '../types'

// ── Event model ───────────────────────────────────────────────────────────────

type EventKind = 'relay' | 'direct' | 'download-done' | 'warn' | 'error' | 'info' | 'iroh-span' | 'other'

interface TlEvent {
  node: string
  t_ms: number       // ms from start
  kind: EventKind
  label: string
  raw: string        // JSON or text for tooltip
  duration_ms?: number  // for span events
}

// ── Colours ───────────────────────────────────────────────────────────────────

const KIND_COLOR: Record<EventKind, string> = {
  relay: '#6e7681',
  direct: '#3fb950',
  'download-done': '#58a6ff',
  warn: '#d29922',
  error: '#f85149',
  info: '#8b949e',
  'iroh-span': '#bc8cff',
  other: '#444c56',
}

// ── Log parsers ───────────────────────────────────────────────────────────────

const ANSI_RE = /\x1b\[[0-9;]*m/g
const strip = (s: string) => s.replace(ANSI_RE, '')
const TRACING_TS_RE = /^(\d{4}-\d{2}-\d{2}T[\d:.]+Z)\s+(ERROR|WARN|INFO|DEBUG|TRACE)\s+(\S+):\s*(.*)/

interface ParsedEvents { events: TlEvent[]; t0_iso?: string }

function parseIrohNdjson(text: string, node: string, t0_offset_ms = 0): TlEvent[] {
  const events: TlEvent[] = []
  let idx = 0
  let totalDur: number | undefined

  const lines = text.split('\n')
  for (const line of lines) {
    const t = line.trim()
    if (!t) continue
    try {
      const obj = JSON.parse(t) as Record<string, unknown>
      const kind_str = String(obj.kind ?? '')

      if (kind_str === 'DownloadComplete') {
        const dur_us = obj.duration as number | undefined
        if (dur_us) totalDur = dur_us / 1000
        const size = obj.size as number | undefined
        const mbps = dur_us && size ? ((size * 8) / (dur_us / 1e6) / 1e6).toFixed(1) : '?'
        events.push({
          node, t_ms: totalDur ?? idx * 10, kind: 'download-done',
          label: `✓ ${mbps} Mbit/s`, raw: t,
        })
      } else if (kind_str === 'ConnectionTypeChanged') {
        const addr = String(obj.addr ?? '')
        const isDirect = addr.includes('Ip(') || addr.includes('"Ip"')
        events.push({
          node, t_ms: idx * 10, // approximate until we have timestamps
          kind: isDirect ? 'direct' : 'relay',
          label: isDirect ? '⚡ direct' : '↔ relay', raw: t,
        })
      }
      idx++
    } catch { /* skip */ }
  }

  // If we have a DownloadComplete duration, redistribute conn events proportionally
  if (totalDur && events.length > 1) {
    const connEvents = events.filter(e => e.kind !== 'download-done')
    connEvents.forEach((e, i) => {
      e.t_ms = t0_offset_ms + (totalDur! * i) / (connEvents.length + 1)
    })
    events.filter(e => e.kind === 'download-done').forEach(e => {
      e.t_ms = t0_offset_ms + totalDur!
    })
  }
  return events
}

function parseTracingAnsi(text: string, node: string): ParsedEvents {
  const events: TlEvent[] = []
  let t0: number | undefined
  const lines = text.split('\n')
  const openSpans = new Map<string, { t: number; label: string }>()

  for (const line of lines) {
    const s = strip(line.trimEnd())
    const m = s.match(TRACING_TS_RE)
    if (!m) continue

    const [, ts, level, target, message] = m
    const t_abs = Date.parse(ts)
    if (isNaN(t_abs)) continue
    if (t0 === undefined) t0 = t_abs
    const t_ms = t_abs - t0

    const isIrohSpan = target.includes('iroh::_events')

    // Track span open/close for iroh::_events
    if (isIrohSpan) {
      const trimMsg = message.trim()
      if (trimMsg === 'new' || trimMsg.startsWith('new ')) {
        openSpans.set(target, { t: t_ms, label: target.split('::').pop() ?? target })
      } else if (trimMsg === 'close' || trimMsg.startsWith('close ')) {
        const open = openSpans.get(target)
        if (open) {
          events.push({
            node, t_ms: open.t, kind: 'iroh-span',
            label: open.label, raw: s,
            duration_ms: t_ms - open.t,
          })
          openSpans.delete(target)
        }
      }
      continue // don't also show as a regular event
    }

    const kind: EventKind = level === 'ERROR' ? 'error' : level === 'WARN' ? 'warn' : 'info'
    if (level !== 'INFO' && level !== 'WARN' && level !== 'ERROR') continue // skip DEBUG/TRACE on timeline
    events.push({ node, t_ms, kind, label: message.slice(0, 60), raw: s })
  }

  return { events, t0_iso: t0 ? new Date(t0).toISOString() : undefined }
}

async function loadNodeEvents(log: ManifestLog): Promise<TlEvent[]> {
  try {
    const r = await fetch(log.path)
    if (!r.ok) return []
    const text = await r.text()
    if (log.kind === 'iroh-ndjson') return parseIrohNdjson(text, log.node)
    if (log.kind === 'tracing-ansi' || log.kind === 'text') {
      return parseTracingAnsi(text, log.node).events
    }
  } catch { /* skip */ }
  return []
}

// ── SVG Timeline ──────────────────────────────────────────────────────────────

const LABEL_W = 80
const LANE_W = 160
const ROW_H = 14
const PAD_TOP = 30
const PAD_BOTTOM = 20
const TICK_R = 5

type TooltipState = { x: number; y: number; text: string } | null

function TimelineSvg({
  events, nodes, maxT,
}: {
  events: TlEvent[]
  nodes: string[]
  maxT: number
}) {
  const [tooltip, setTooltip] = useState<TooltipState | null>(null)
  const svgRef = useRef<SVGSVGElement>(null)

  const [zoom, setZoom] = useState(1)
  const [panY, setPanY] = useState(0)

  const svgW = LABEL_W + nodes.length * LANE_W
  const totalH = PAD_TOP + maxT * ROW_H * zoom + PAD_BOTTOM

  const tToY = useCallback((t: number) => PAD_TOP + (t / maxT) * (totalH - PAD_TOP - PAD_BOTTOM) - panY, [maxT, totalH, panY])

  const onWheel = useCallback((e: React.WheelEvent) => {
    e.preventDefault()
    if (e.ctrlKey || e.metaKey) {
      setZoom(z => Math.max(0.2, Math.min(20, z * (e.deltaY > 0 ? 0.9 : 1.1))))
    } else {
      setPanY(p => Math.max(0, p + e.deltaY))
    }
  }, [])

  const visH = 600
  const timeLabels: number[] = []
  const step = maxT > 10000 ? 1000 : maxT > 1000 ? 100 : 10
  for (let t = 0; t <= maxT; t += step) timeLabels.push(t)

  return (
    <div style={{ position: 'relative' }}>
      <div style={{ fontSize: 11, color: 'var(--text-muted)', marginBottom: 8 }}>
        scroll to pan · ctrl+scroll to zoom · hover events for details
      </div>
      <div style={{ overflow: 'hidden', height: visH, border: '1px solid var(--border)', borderRadius: 4 }}>
        <svg
          ref={svgRef}
          width={svgW}
          height={totalH}
          className="timeline-svg"
          onWheel={onWheel}
          style={{ display: 'block', transform: `translateY(${-panY}px)`, cursor: 'default' }}
        >
          {/* Node lane headers */}
          {nodes.map((node, i) => (
            <g key={node}>
              <rect x={LABEL_W + i * LANE_W} y={0} width={LANE_W} height={PAD_TOP - 4}
                fill="var(--surface2)" />
              <text x={LABEL_W + i * LANE_W + LANE_W / 2} y={18}
                textAnchor="middle" fontSize={11} fill="var(--text)" fontFamily="monospace">
                {node}
              </text>
              {/* Lane divider */}
              <line x1={LABEL_W + i * LANE_W} y1={PAD_TOP} x2={LABEL_W + i * LANE_W} y2={totalH}
                stroke="var(--border)" strokeWidth={1} />
            </g>
          ))}

          {/* Time axis labels */}
          {timeLabels.map(t => {
            const y = tToY(t) + panY
            return (
              <g key={t}>
                <line x1={0} y1={y} x2={LABEL_W} y2={y} stroke="var(--border)" strokeWidth={1} />
                <text x={LABEL_W - 6} y={y + 4} textAnchor="end" fontSize={10}
                  fill="var(--text-muted)" fontFamily="monospace">
                  {t < 1000 ? `${t}ms` : `${(t / 1000).toFixed(1)}s`}
                </text>
              </g>
            )
          })}

          {/* Events */}
          {events.map((evt, ei) => {
            const laneIdx = nodes.indexOf(evt.node)
            if (laneIdx < 0) return null
            const cx = LABEL_W + laneIdx * LANE_W + LANE_W / 2
            const y = tToY(evt.t_ms) + panY
            const color = KIND_COLOR[evt.kind]

            if (evt.kind === 'iroh-span' && evt.duration_ms != null) {
              const y2 = tToY(evt.t_ms + evt.duration_ms) + panY
              return (
                <g key={ei}
                  onMouseEnter={e => setTooltip({ x: e.clientX, y: e.clientY, text: evt.raw })}
                  onMouseLeave={() => setTooltip(null)}
                  style={{ cursor: 'pointer' }}>
                  <rect x={cx - TICK_R} y={y} width={TICK_R * 2} height={Math.max(4, y2 - y)}
                    fill={color} opacity={0.7} rx={2} />
                </g>
              )
            }

            return (
              <g key={ei}
                onMouseEnter={e => setTooltip({ x: e.clientX, y: e.clientY, text: `[${evt.t_ms.toFixed(0)}ms] ${evt.label}\n\n${evt.raw}` })}
                onMouseLeave={() => setTooltip(null)}
                style={{ cursor: 'pointer' }}>
                <circle cx={cx} cy={y} r={TICK_R} fill={color} opacity={0.85} />
                {evt.kind === 'download-done' && (
                  <text x={cx + TICK_R + 4} y={y + 4} fontSize={10} fill={color} fontFamily="monospace">
                    {evt.label}
                  </text>
                )}
              </g>
            )
          })}
        </svg>
      </div>
      {tooltip && (
        <div className="tl-tooltip" style={{ left: tooltip.x + 12, top: tooltip.y - 8 }}>
          {tooltip.text}
        </div>
      )}

      {/* Legend */}
      <div style={{ display: 'flex', gap: 16, marginTop: 8, flexWrap: 'wrap', fontSize: 11 }}>
        {(Object.entries(KIND_COLOR) as [EventKind, string][]).map(([k, c]) => (
          <span key={k} style={{ display: 'flex', alignItems: 'center', gap: 4, color: 'var(--text-muted)' }}>
            <svg width={10} height={10}><circle cx={5} cy={5} r={4} fill={c} /></svg>
            {k}
          </span>
        ))}
      </div>
    </div>
  )
}

// ── Main component ─────────────────────────────────────────────────────────────

export default function TimelineTab({
  manifest, base: _base, results: _results,
}: {
  manifest: Manifest
  base: string
  results: SimResults
}) {
  const [allEvents, setAllEvents] = useState<TlEvent[]>([])
  const [loading, setLoading] = useState(true)
  const [visibleKinds, setVisibleKinds] = useState<Set<EventKind>>(
    new Set(Object.keys(KIND_COLOR) as EventKind[])
  )

  useEffect(() => {
    let cancelled = false
    ;(async () => {
      setLoading(true)
      const logFiles = manifest.logs.filter(l => l.kind !== 'qlog-dir')
      const results = await Promise.all(logFiles.map(loadNodeEvents))
      if (!cancelled) {
        setAllEvents(results.flat())
        setLoading(false)
      }
    })()
    return () => { cancelled = true }
  }, [manifest])

  const nodes = [...new Set(manifest.logs.map(l => l.node))]

  const filteredEvents = allEvents.filter(e => visibleKinds.has(e.kind))
  const maxT = filteredEvents.length > 0
    ? Math.max(...filteredEvents.map(e => e.t_ms + (e.duration_ms ?? 0))) * 1.05
    : 10000

  const toggleKind = (k: EventKind) => {
    setVisibleKinds(s => { const n = new Set(s); n.has(k) ? n.delete(k) : n.add(k); return n })
  }

  return (
    <div className="timeline-layout">
      <div className="timeline-toolbar">
        <span style={{ color: 'var(--text-muted)', fontSize: 11 }}>show:</span>
        {(Object.keys(KIND_COLOR) as EventKind[]).map(k => (
          <button
            key={k}
            className={`btn${visibleKinds.has(k) ? ' active' : ''}`}
            style={{ fontSize: 10, padding: '2px 6px', color: KIND_COLOR[k] }}
            onClick={() => toggleKind(k)}
          >{k}</button>
        ))}
        <span style={{ color: 'var(--text-muted)', fontSize: 11, marginLeft: 'auto' }}>
          {filteredEvents.length} events · {nodes.length} nodes
        </span>
      </div>
      <div className="timeline-main">
        {loading && <div className="loading">loading events…</div>}
        {!loading && filteredEvents.length === 0 && (
          <div className="empty">no events found — check log files exist</div>
        )}
        {!loading && filteredEvents.length > 0 && (
          <TimelineSvg events={filteredEvents} nodes={nodes} maxT={maxT} />
        )}
      </div>
    </div>
  )
}
