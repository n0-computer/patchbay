import type { LabState } from '../devtools-types'

interface Props {
  state: LabState
  selectedNode: string
  selectedKind: 'router' | 'device' | 'ix'
}

function jsonField(label: string, value: unknown) {
  const display = typeof value === 'string' ? value : JSON.stringify(value)
  return (
    <tr key={label}>
      <td style={{ color: 'var(--text-muted)', paddingRight: 12 }}>{label}</td>
      <td>{display}</td>
    </tr>
  )
}

export default function NodeDetail({ state, selectedNode, selectedKind }: Props) {
  if (selectedKind === 'ix' && state.ix) {
    const ix = state.ix
    return (
      <div className="node-detail">
        <h3>IX</h3>
        <table>
          <tbody>
            {jsonField('bridge', ix.bridge)}
            {jsonField('cidr', ix.cidr)}
            {jsonField('gw', ix.gw)}
            {jsonField('cidr_v6', ix.cidr_v6)}
            {jsonField('gw_v6', ix.gw_v6)}
          </tbody>
        </table>
      </div>
    )
  }

  if (selectedKind === 'router') {
    const router = state.routers[selectedNode]
    if (!router) return <div className="node-detail">Router not found</div>
    return (
      <div className="node-detail">
        <h3>{selectedNode}</h3>
        <table>
          <tbody>
            {jsonField('namespace', router.ns)}
            {jsonField('region', router.region ?? '—')}
            {jsonField('nat', router.nat)}
            {jsonField('nat_v6', router.nat_v6)}
            {jsonField('firewall', router.firewall)}
            {jsonField('ip_support', router.ip_support)}
            {router.mtu != null && jsonField('mtu', router.mtu)}
            {jsonField('upstream', router.upstream ?? 'IX')}
            {jsonField('uplink_ip', router.uplink_ip ?? '—')}
            {router.uplink_ip_v6 && jsonField('uplink_ip_v6', router.uplink_ip_v6)}
            {jsonField('downstream_cidr', router.downstream_cidr ?? '—')}
            {jsonField('downstream_gw', router.downstream_gw ?? '—')}
            {router.downstream_cidr_v6 && jsonField('downstream_cidr_v6', router.downstream_cidr_v6)}
            {router.downstream_gw_v6 && jsonField('downstream_gw_v6', router.downstream_gw_v6)}
            {jsonField('downstream_bridge', router.downstream_bridge)}
            {router.downlink_condition != null ? jsonField('downlink_condition', router.downlink_condition) : null}
            {jsonField('devices', router.devices.join(', ') || '—')}
          </tbody>
        </table>
        {Object.keys(router.counters).length > 0 && (
          <>
            <h4 style={{ marginTop: 12 }}>Counters</h4>
            <table>
              <thead>
                <tr>
                  <th>iface</th>
                  <th>rx_bytes</th>
                  <th>tx_bytes</th>
                  <th>rx_pkts</th>
                  <th>tx_pkts</th>
                </tr>
              </thead>
              <tbody>
                {Object.entries(router.counters).map(([iface, c]) => (
                  <tr key={iface}>
                    <td>{iface}</td>
                    <td>{c.rx_bytes}</td>
                    <td>{c.tx_bytes}</td>
                    <td>{c.rx_packets}</td>
                    <td>{c.tx_packets}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </>
        )}
      </div>
    )
  }

  if (selectedKind === 'device') {
    const device = state.devices[selectedNode]
    if (!device) return <div className="node-detail">Device not found</div>
    return (
      <div className="node-detail">
        <h3>{selectedNode}</h3>
        <table>
          <tbody>
            {jsonField('namespace', device.ns)}
            {jsonField('default_via', device.default_via)}
            {device.mtu != null && jsonField('mtu', device.mtu)}
          </tbody>
        </table>
        <h4 style={{ marginTop: 12 }}>Interfaces</h4>
        <table>
          <thead>
            <tr>
              <th>name</th>
              <th>router</th>
              <th>ip</th>
              <th>ip_v6</th>
              <th>condition</th>
            </tr>
          </thead>
          <tbody>
            {device.interfaces.map((iface) => (
              <tr key={iface.name}>
                <td>{iface.name}</td>
                <td>{iface.router}</td>
                <td>{iface.ip ?? '—'}</td>
                <td>{iface.ip_v6 ?? '—'}</td>
                <td>{iface.link_condition ? JSON.stringify(iface.link_condition) : '—'}</td>
              </tr>
            ))}
          </tbody>
        </table>
        {Object.keys(device.counters).length > 0 && (
          <>
            <h4 style={{ marginTop: 12 }}>Counters</h4>
            <table>
              <thead>
                <tr>
                  <th>iface</th>
                  <th>rx_bytes</th>
                  <th>tx_bytes</th>
                  <th>rx_pkts</th>
                  <th>tx_pkts</th>
                </tr>
              </thead>
              <tbody>
                {Object.entries(device.counters).map(([iface, c]) => (
                  <tr key={iface}>
                    <td>{iface}</td>
                    <td>{c.rx_bytes}</td>
                    <td>{c.tx_bytes}</td>
                    <td>{c.rx_packets}</td>
                    <td>{c.tx_packets}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </>
        )}
      </div>
    )
  }

  return <div className="node-detail">Select a node</div>
}
