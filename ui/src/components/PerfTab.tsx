import type { CombinedResults, CombinedRunResult, SimResults, StepResult } from '../types'

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

function StepsTable({ steps, simColumn }: { steps: { sim?: string; step: StepResult }[]; simColumn: boolean }) {
  const allSteps = steps.map((s) => s.step)
  const showDown = hasAny(allSteps, 'down_bytes')
  const showUp = hasAny(allSteps, 'up_bytes')
  const showDuration = hasAny(allSteps, 'duration')
  const showLatency = hasAny(allSteps, 'latency_ms')

  if (allSteps.length === 0) return null

  return (
    <div className="tbl-wrap">
      <table>
        <thead>
          <tr>
            {simColumn && <th>Sim</th>}
            <th>ID</th>
            {showDown && <th>Down MB/s</th>}
            {showUp && <th>Up MB/s</th>}
            {showLatency && <th>Latency (ms)</th>}
            {showDuration && <th>Elapsed (s)</th>}
            {showDown && <th>Down Bytes</th>}
            {showUp && <th>Up Bytes</th>}
          </tr>
        </thead>
        <tbody>
          {steps.map((row, i) => (
            <tr key={i}>
              {simColumn && <td>{row.sim}</td>}
              <td>{row.step.id}</td>
              {showDown && <td>{fmt(mbS(row.step.down_bytes, row.step.duration), 2)}</td>}
              {showUp && <td>{fmt(mbS(row.step.up_bytes, row.step.duration), 2)}</td>}
              {showLatency && <td>{fmt(row.step.latency_ms ? parseFloat(row.step.latency_ms) : null, 1, ' ms')}</td>}
              {showDuration && <td>{fmt(elapsedS(row.step.duration), 2, 's')}</td>}
              {showDown && <td>{row.step.down_bytes ?? '—'}</td>}
              {showUp && <td>{row.step.up_bytes ?? '—'}</td>}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

function CombinedSummary({ runs }: { runs: CombinedRunResult[] }) {
  // Group by sim name, compute summary stats per sim
  const bySim = new Map<string, CombinedRunResult[]>()
  for (const run of runs) {
    let list = bySim.get(run.sim)
    if (!list) {
      list = []
      bySim.set(run.sim, list)
    }
    list.push(run)
  }

  const summaryRows = Array.from(bySim.entries()).map(([sim, simRuns]) => {
    const allSteps = simRuns.flatMap((r) => r.steps)
    const maxDown = allSteps
      .map((s) => mbS(s.down_bytes, s.duration))
      .filter((v): v is number => v != null)
      .reduce((a, b) => Math.max(a, b), 0)
    const maxUp = allSteps
      .map((s) => mbS(s.up_bytes, s.duration))
      .filter((v): v is number => v != null)
      .reduce((a, b) => Math.max(a, b), 0)
    const minLatency = allSteps
      .map((s) => (s.latency_ms ? parseFloat(s.latency_ms) : null))
      .filter((v): v is number => v != null && !isNaN(v))
      .reduce((a, b) => Math.min(a, b), Infinity)
    return {
      sim,
      n: allSteps.length,
      maxDown: maxDown > 0 ? maxDown : null,
      maxUp: maxUp > 0 ? maxUp : null,
      minLatency: isFinite(minLatency) ? minLatency : null,
    }
  })

  const hasDown = summaryRows.some((r) => r.maxDown != null)
  const hasUp = summaryRows.some((r) => r.maxUp != null)
  const hasLatency = summaryRows.some((r) => r.minLatency != null)

  return (
    <div className="section">
      <div className="section-header">summary</div>
      <div className="tbl-wrap">
        <table>
          <thead>
            <tr>
              <th>Sim</th>
              <th>N</th>
              {hasDown && <th>Max Down (MB/s)</th>}
              {hasUp && <th>Max Up (MB/s)</th>}
              {hasLatency && <th>Min Latency (ms)</th>}
            </tr>
          </thead>
          <tbody>
            {summaryRows.map((r) => (
              <tr key={r.sim}>
                <td>{r.sim}</td>
                <td>{r.n}</td>
                {hasDown && <td>{fmt(r.maxDown, 2)}</td>}
                {hasUp && <td>{fmt(r.maxUp, 2)}</td>}
                {hasLatency && <td>{fmt(r.minLatency, 1, ' ms')}</td>}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  )
}

interface PerfTabProps {
  results: SimResults | null
  combined?: CombinedResults | null
}

export default function PerfTab({ results, combined }: PerfTabProps) {
  // Combined / invocation view
  if (combined) {
    const { runs } = combined
    if (runs.length === 0) {
      return <div className="empty">no combined results for this invocation</div>
    }

    const detailRows = runs.flatMap((run) =>
      run.steps.map((step) => ({ sim: run.sim, step })),
    )

    return (
      <div className="perf-layout">
        <CombinedSummary runs={runs} />
        <div className="section">
          <div className="section-header">all steps</div>
          <StepsTable steps={detailRows} simColumn={true} />
        </div>
      </div>
    )
  }

  // Single sim view
  if (!results) return <div className="empty">no results for this simulation yet</div>
  const rows = results.steps.map((step) => ({ step }))

  return (
    <div className="perf-layout">
      {rows.length > 0 && (
        <div className="section">
          <div className="section-header">steps</div>
          <StepsTable steps={rows} simColumn={false} />
        </div>
      )}
    </div>
  )
}
