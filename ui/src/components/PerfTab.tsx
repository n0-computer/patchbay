import type { SimResults } from '../types'

function fmt(v: number | undefined | null, decimals = 1, suffix = '') {
  if (v == null) return <span style={{ color: 'var(--text-muted)' }}>—</span>
  return <>{v.toFixed(decimals)}{suffix}</>
}

type NodeRow = {
  node: string
  up: number
  down: number
}

function transferNodeRows(results: SimResults): NodeRow[] {
  const byNode = new Map<string, NodeRow>()
  for (const transfer of results.transfers) {
    const mbps = transfer.mbps ?? 0
    if (!byNode.has(transfer.provider)) {
      byNode.set(transfer.provider, { node: transfer.provider, up: 0, down: 0 })
    }
    if (!byNode.has(transfer.fetcher)) {
      byNode.set(transfer.fetcher, { node: transfer.fetcher, up: 0, down: 0 })
    }
    byNode.get(transfer.provider)!.up += mbps
    byNode.get(transfer.fetcher)!.down += mbps
  }
  return [...byNode.values()].sort((a, b) => a.node.localeCompare(b.node))
}

export default function PerfTab({ results }: { results: SimResults | null }) {
  if (!results) return <div className="empty">no results for this simulation yet</div>
  const nodeRows = transferNodeRows(results)

  return (
    <div className="perf-layout">
      {results.transfers.length > 0 && (
        <div className="section">
          <div className="section-header">transfer per-node throughput</div>
          <div className="tbl-wrap">
            <table>
              <thead>
                <tr>
                  <th>node</th>
                  <th>up_mbps</th>
                  <th>down_mbps</th>
                </tr>
              </thead>
              <tbody>
                {nodeRows.map((r) => (
                  <tr key={r.node}>
                    <td>{r.node}</td>
                    <td>{fmt(r.up, 2)}</td>
                    <td>{fmt(r.down, 2)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}

      {results.transfers.length > 0 && (
        <div className="section">
          <div className="section-header">transfer details</div>
          <div className="tbl-wrap">
            <table>
              <thead>
                <tr>
                  <th>id</th>
                  <th>provider</th>
                  <th>fetcher</th>
                  <th>mbps</th>
                  <th>elapsed</th>
                  <th>size</th>
                  <th>direct</th>
                  <th>events</th>
                </tr>
              </thead>
              <tbody>
                {results.transfers.map((r, i) => (
                  <tr key={i}>
                    <td>{r.id}</td>
                    <td>{r.provider}</td>
                    <td>{r.fetcher}</td>
                    <td>{fmt(r.mbps, 1)}</td>
                    <td>{fmt(r.elapsed_s, 2, 's')}</td>
                    <td>{fmt(r.size_bytes ? r.size_bytes / 1e6 : undefined, 1, ' MB')}</td>
                    <td>{r.final_conn_direct == null ? '—' : (r.final_conn_direct ? 'yes' : 'no')}</td>
                    <td>{r.conn_events}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}

      {results.iperf.length > 0 && (
        <div className="section">
          <div className="section-header">iperf</div>
          <div className="tbl-wrap">
            <table>
              <thead>
                <tr>
                  <th>id</th>
                  <th>device</th>
                  <th>mbps</th>
                  <th>retx</th>
                  <th>baseline</th>
                  <th>delta</th>
                </tr>
              </thead>
              <tbody>
                {results.iperf.map((r, i) => (
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
