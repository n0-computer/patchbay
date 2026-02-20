import { useState, useMemo } from 'react'
import type { CombinedResults, SimResults, TransferResult, IperfResult, RunResults } from '../types'

interface Props {
  results: SimResults | null
  combined: CombinedResults | null
  onSelectRun: (run: string) => void
}

type SortDir = 'asc' | 'desc'
interface Sort { col: string; dir: SortDir }

function useSortable<T extends Record<string, unknown>>(data: T[], init: string) {
  const [sort, setSort] = useState<Sort>({ col: init, dir: 'asc' })
  const sorted = useMemo(() => {
    return [...data].sort((a, b) => {
      const av = a[sort.col] as number | string | boolean | undefined
      const bv = b[sort.col] as number | string | boolean | undefined
      const cmp = av == null ? 1 : bv == null ? -1
        : typeof av === 'number' && typeof bv === 'number' ? av - bv
        : String(av).localeCompare(String(bv))
      return sort.dir === 'asc' ? cmp : -cmp
    })
  }, [data, sort])

  const onSort = (col: string) =>
    setSort(s => ({ col, dir: s.col === col && s.dir === 'asc' ? 'desc' : 'asc' }))

  const th = (col: string, label: string) => (
    <th
      key={col}
      className={sort.col === col ? `sort-${sort.dir}` : ''}
      onClick={() => onSort(col)}
      title="click to sort"
    >{label}</th>
  )
  return { sorted, th }
}

function fmt(v: number | undefined | null, decimals = 1, suffix = '') {
  if (v == null) return <span style={{ color: 'var(--text-muted)' }}>—</span>
  return <>{v.toFixed(decimals)}{suffix}</>
}

function boolBadge(v: boolean | undefined | null) {
  if (v == null) return <span style={{ color: 'var(--text-muted)' }}>—</span>
  return <span className={`badge ${v ? 'badge-green' : 'badge-grey'}`}>{v ? 'yes' : 'no'}</span>
}

function DeltaCell({ v }: { v?: number | null }) {
  if (v == null) return <span style={{ color: 'var(--text-muted)' }}>—</span>
  const cls = v > 0 ? 'delta-pos' : v < 0 ? 'delta-neg' : ''
  return <span className={cls}>{v > 0 ? '+' : ''}{v.toFixed(2)}</span>
}

function TransferTable({ rows }: { rows: TransferResult[] }) {
  const { sorted, th } = useSortable(rows as unknown as Record<string, unknown>[], 'id')
  return (
    <div className="tbl-wrap">
      <table>
        <thead><tr>
          {th('id', 'id')}
          {th('provider', 'provider')}
          {th('fetcher', 'fetcher')}
          {th('size_bytes', 'size')}
          {th('elapsed_s', 'elapsed_s')}
          {th('mbps', 'mbps')}
          {th('final_conn_direct', 'direct')}
          {th('conn_upgrade', 'upgrade')}
          {th('conn_events', 'events')}
        </tr></thead>
        <tbody>
          {(sorted as unknown as TransferResult[]).map((r, i) => (
            <tr key={i}>
              <td>{r.id}</td>
              <td>{r.provider}</td>
              <td>{r.fetcher}</td>
              <td>{fmt(r.size_bytes ? r.size_bytes / 1e6 : undefined, 1, ' MB')}</td>
              <td>{fmt(r.elapsed_s, 3, 's')}</td>
              <td style={{ fontWeight: 600 }}>{fmt(r.mbps, 1, ' Mbit/s')}</td>
              <td>{boolBadge(r.final_conn_direct)}</td>
              <td>{boolBadge(r.conn_upgrade)}</td>
              <td>{r.conn_events}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

function IperfTable({ rows }: { rows: IperfResult[] }) {
  const { sorted, th } = useSortable(rows as unknown as Record<string, unknown>[], 'id')
  return (
    <div className="tbl-wrap">
      <table>
        <thead><tr>
          {th('id', 'id')}
          {th('device', 'device')}
          {th('mbps', 'mbps')}
          {th('retransmits', 'retx')}
          {th('baseline', 'baseline')}
          {th('delta_mbps', 'Δmbps')}
          {th('delta_pct', 'Δ%')}
        </tr></thead>
        <tbody>
          {(sorted as unknown as IperfResult[]).map((r, i) => (
            <tr key={i}>
              <td>{r.id}</td>
              <td>{r.device}</td>
              <td style={{ fontWeight: 600 }}>{fmt(r.mbps, 3, ' Mbit/s')}</td>
              <td>{fmt(r.retransmits, 0)}</td>
              <td>{r.baseline ?? <span style={{ color: 'var(--text-muted)' }}>—</span>}</td>
              <td><DeltaCell v={r.delta_mbps} /></td>
              <td><DeltaCell v={r.delta_pct} /></td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

function AllRunsTable({ runs, onSelectRun }: { runs: RunResults[]; onSelectRun: (r: string) => void }) {
  const [filter, setFilter] = useState('')
  const rows = useMemo(() => {
    return runs
      .filter(r => !filter || r.run.includes(filter) || r.sim.includes(filter))
      .map(r => {
        const mbps = r.transfers.map(t => t.mbps).filter(Boolean) as number[]
        const avgMbps = mbps.length ? mbps.reduce((a, b) => a + b, 0) / mbps.length : undefined
        const directs = r.transfers.filter(t => t.final_conn_direct != null)
        const directPct = directs.length
          ? (100 * directs.filter(t => t.final_conn_direct).length / directs.length)
          : undefined
        return { ...r, avgMbps, directPct }
      })
  }, [runs, filter])

  const { sorted, th } = useSortable(rows as unknown as Record<string, unknown>[], 'run')

  return (
    <div>
      <div style={{ padding: '8px 12px' }}>
        <input placeholder="filter runs…" value={filter} onChange={e => setFilter(e.target.value)} />
      </div>
      <div className="tbl-wrap">
        <table>
          <thead><tr>
            {th('run', 'run')}
            {th('sim', 'sim')}
            {th('transfers', '# xfers')}
            {th('avgMbps', 'avg mbps')}
            {th('directPct', 'direct %')}
          </tr></thead>
          <tbody>
            {(sorted as unknown as (RunResults & { avgMbps?: number; directPct?: number })[]).map((r, i) => (
              <tr key={i} style={{ cursor: 'pointer' }} onClick={() => onSelectRun(r.run)}>
                <td style={{ color: 'var(--accent)' }}>{r.run}</td>
                <td>{r.sim}</td>
                <td>{r.transfers.length}</td>
                <td>{fmt(r.avgMbps, 1, ' Mbit/s')}</td>
                <td>{r.directPct != null ? `${r.directPct.toFixed(0)}%` : '—'}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  )
}

function ComparePane({ combined }: { combined: CombinedResults }) {
  const runs = combined.runs
  const [runA, setRunA] = useState(runs[0]?.run ?? '')
  const [runB, setRunB] = useState(runs[1]?.run ?? runs[0]?.run ?? '')

  const dataA = runs.find(r => r.run === runA)
  const dataB = runs.find(r => r.run === runB)

  const diffRows = useMemo(() => {
    if (!dataA || !dataB) return []
    return dataA.transfers.map(a => {
      const b = dataB.transfers.find(t => t.id === a.id && t.fetcher === a.fetcher)
      const delta = a.mbps != null && b?.mbps != null ? b.mbps - a.mbps : undefined
      const deltaPct = delta != null && a.mbps ? (delta / a.mbps) * 100 : undefined
      return { id: a.id, fetcher: a.fetcher, mbpsA: a.mbps, mbpsB: b?.mbps, delta, deltaPct,
               directA: a.final_conn_direct, directB: b?.final_conn_direct }
    })
  }, [dataA, dataB])

  const sel = (label: string, val: string, set: (v: string) => void) => (
    <span style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
      <span style={{ color: 'var(--text-muted)', fontSize: 11 }}>{label}</span>
      <select value={val} onChange={e => set(e.target.value)}>
        {runs.map(r => <option key={r.run} value={r.run}>{r.run}</option>)}
      </select>
    </span>
  )

  return (
    <div>
      <div className="compare-controls" style={{ padding: '8px 12px' }}>
        {sel('A', runA, setRunA)}
        <span style={{ color: 'var(--text-muted)' }}>vs</span>
        {sel('B', runB, setRunB)}
      </div>
      {diffRows.length === 0
        ? <div className="empty">select two runs with matching transfer ids</div>
        : (
          <div className="tbl-wrap">
            <table>
              <thead><tr>
                <th>id</th><th>fetcher</th>
                <th>mbps A</th><th>mbps B</th>
                <th>Δmbps (B−A)</th><th>Δ%</th>
                <th>direct A</th><th>direct B</th>
              </tr></thead>
              <tbody>
                {diffRows.map((r, i) => (
                  <tr key={i}>
                    <td>{r.id}</td>
                    <td>{r.fetcher}</td>
                    <td>{fmt(r.mbpsA, 1)}</td>
                    <td>{fmt(r.mbpsB, 1)}</td>
                    <td><DeltaCell v={r.delta} /></td>
                    <td><DeltaCell v={r.deltaPct} /></td>
                    <td>{boolBadge(r.directA)}</td>
                    <td>{boolBadge(r.directB)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )
      }
    </div>
  )
}

export default function PerfTab({ results, combined, onSelectRun }: Props) {
  return (
    <div className="perf-layout">
      {results && results.transfers.length > 0 && (
        <div className="section">
          <div className="section-header">transfers — {results.sim}</div>
          <TransferTable rows={results.transfers} />
        </div>
      )}
      {results && results.iperf.length > 0 && (
        <div className="section">
          <div className="section-header">iperf</div>
          <IperfTable rows={results.iperf} />
        </div>
      )}
      {combined && combined.runs.length > 0 && (
        <div className="section">
          <div className="section-header">all runs ({combined.runs.length})</div>
          <AllRunsTable runs={combined.runs} onSelectRun={onSelectRun} />
        </div>
      )}
      {combined && combined.runs.length >= 2 && (
        <div className="section">
          <div className="section-header">compare runs</div>
          <ComparePane combined={combined} />
        </div>
      )}
      {!results && !combined && (
        <div className="empty">no results.json or combined-results.json found</div>
      )}
    </div>
  )
}
