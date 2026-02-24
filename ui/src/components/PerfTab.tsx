import type { SimResults, StepResult } from '../types'

function fmt(v: number | undefined | null, decimals = 1, suffix = '') {
  if (v == null) return <span style={{ color: 'var(--text-muted)' }}>—</span>
  return <>{v.toFixed(decimals)}{suffix}</>
}

function elapsedS(dur: string | undefined): number | null {
  if (!dur) return null
  const trimmed = dur.trim()
  const asInt = parseInt(trimmed, 10)
  if (!isNaN(asInt) && String(asInt) === trimmed) return asInt / 1_000_000
  const asFloat = parseFloat(trimmed)
  return isNaN(asFloat) ? null : asFloat
}

function mbS(bytes: string | undefined, duration: string | undefined): number | null {
  if (!bytes) return null
  const b = parseFloat(bytes)
  const s = elapsedS(duration)
  if (isNaN(b) || s == null || s <= 0) return null
  return b / (s * 1_000_000)
}

function hasAny(steps: StepResult[], field: keyof StepResult): boolean {
  return steps.some((s) => s[field] != null && s[field] !== '')
}

export default function PerfTab({ results }: { results: SimResults | null }) {
  if (!results) return <div className="empty">no results for this simulation yet</div>
  const { steps, iperf = [] } = results

  const showDown = hasAny(steps, 'down_bytes')
  const showUp = hasAny(steps, 'up_bytes')
  const showDuration = hasAny(steps, 'duration')

  return (
    <div className="perf-layout">
      {steps.length > 0 && (
        <div className="section">
          <div className="section-header">steps</div>
          <div className="tbl-wrap">
            <table>
              <thead>
                <tr>
                  <th>ID</th>
                  {showDown && <th>Down MB/s</th>}
                  {showUp && <th>Up MB/s</th>}
                  {showDuration && <th>Elapsed (s)</th>}
                  {showDown && <th>Down Bytes</th>}
                  {showUp && <th>Up Bytes</th>}
                </tr>
              </thead>
              <tbody>
                {steps.map((r, i) => (
                  <tr key={i}>
                    <td>{r.id}</td>
                    {showDown && <td>{fmt(mbS(r.down_bytes, r.duration), 2)}</td>}
                    {showUp && <td>{fmt(mbS(r.up_bytes, r.duration), 2)}</td>}
                    {showDuration && <td>{fmt(elapsedS(r.duration), 2, 's')}</td>}
                    {showDown && <td>{r.down_bytes ?? '—'}</td>}
                    {showUp && <td>{r.up_bytes ?? '—'}</td>}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}

      {iperf.length > 0 && (
        <div className="section">
          <div className="section-header">iperf</div>
          <div className="tbl-wrap">
            <table>
              <thead>
                <tr>
                  <th>ID</th>
                  <th>Device</th>
                  <th>Mbps</th>
                  <th>Retx</th>
                  <th>Baseline</th>
                  <th>Delta</th>
                </tr>
              </thead>
              <tbody>
                {iperf.map((r, i) => (
                  <tr key={i}>
                    <td>{r.id}</td>
                    <td>{r.device}</td>
                    <td>{fmt(r.mbps, 2)}</td>
                    <td>{fmt(r.retransmits, 0)}</td>
                    <td>{r.baseline ?? '—'}</td>
                    <td>{fmt(r.delta_mbps, 2)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}
    </div>
  )
}
