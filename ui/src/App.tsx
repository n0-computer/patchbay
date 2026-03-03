import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type {
  Firewall,
  LabEvent,
  LabState,
  LinkCondition,
  Nat,
  NatV6Mode,
  RouterState,
  DeviceState,
  IfaceState,
} from './devtools-types'
import type { SimResults } from './types'
import {
  fetchRuns,
  fetchState,
  subscribeEvents,
  subscribeRuns,
  fetchLogs,
  fetchResults,
  runFilesBase,
} from './api'
import type { RunInfo, LogEntry } from './api'
import LogsTab from './components/LogsTab'
import PerfTab from './components/PerfTab'
import TimelineTab from './components/TimelineTab'
import TopologyGraph from './components/TopologyGraph'
import NodeDetail from './components/NodeDetail'
import KvPairs from './components/KvPairs'
import { formatTimestamp, formatRelativeTime, parseIsoMs, kvPairs } from './time-format'

type Tab = 'topology' | 'events' | 'logs' | 'timeline' | 'perf'

// ── State reducer (from DevtoolsApp) ──────────────────────────────

function applyEvent(state: LabState, event: LabEvent): LabState {
  const next = { ...state, opid: event.opid }
  const kind = event.kind

  if (kind === 'router_added') {
    const name = event.name as string
    const routerState: RouterState = {
      ns: event.ns as string,
      region: (event.region as string | null) ?? null,
      nat: event.nat as Nat,
      nat_v6: event.nat_v6 as NatV6Mode,
      firewall: event.firewall as Firewall,
      ip_support: event.ip_support as RouterState['ip_support'],
      mtu: (event.mtu as number | null) ?? null,
      upstream: (event.upstream as string | null) ?? null,
      uplink_ip: (event.uplink_ip as string | null) ?? null,
      uplink_ip_v6: (event.uplink_ip_v6 as string | null) ?? null,
      downstream_cidr: (event.downstream_cidr as string | null) ?? null,
      downstream_gw: (event.downstream_gw as string | null) ?? null,
      downstream_cidr_v6: (event.downstream_cidr_v6 as string | null) ?? null,
      downstream_gw_v6: (event.downstream_gw_v6 as string | null) ?? null,
      downstream_bridge: event.downstream_bridge as string,
      downlink_condition: (event.downlink_condition as LinkCondition | null) ?? null,
      devices: (event.devices as string[]) ?? [],
      counters: (event.counters as Record<string, RouterState['counters'][string]>) ?? {},
    }
    next.routers = { ...next.routers, [name]: routerState }
  } else if (kind === 'router_removed') {
    const { [event.name as string]: _, ...rest } = next.routers
    next.routers = rest
  } else if (kind === 'device_added') {
    const name = event.name as string
    const deviceState: DeviceState = {
      ns: event.ns as string,
      default_via: event.default_via as string,
      mtu: (event.mtu as number | null) ?? null,
      interfaces: (event.interfaces as IfaceState[]) ?? [],
      counters: (event.counters as Record<string, DeviceState['counters'][string]>) ?? {},
    }
    for (const iface of deviceState.interfaces) {
      const router = next.routers[iface.router]
      if (router && !router.devices.includes(name)) {
        next.routers = {
          ...next.routers,
          [iface.router]: { ...router, devices: [...router.devices, name] },
        }
      }
    }
    next.devices = { ...next.devices, [name]: deviceState }
  } else if (kind === 'device_removed') {
    const name = event.name as string
    const dev = next.devices[name]
    if (dev) {
      for (const iface of dev.interfaces) {
        const router = next.routers[iface.router]
        if (router) {
          next.routers = {
            ...next.routers,
            [iface.router]: { ...router, devices: router.devices.filter((d) => d !== name) },
          }
        }
      }
    }
    const { [name]: _, ...rest } = next.devices
    next.devices = rest
  } else if (kind === 'nat_changed') {
    const router = next.routers[event.router as string]
    if (router) {
      next.routers = { ...next.routers, [event.router as string]: { ...router, nat: event.nat as Nat } }
    }
  } else if (kind === 'firewall_changed') {
    const router = next.routers[event.router as string]
    if (router) {
      next.routers = { ...next.routers, [event.router as string]: { ...router, firewall: event.firewall as Firewall } }
    }
  }

  return next
}

// ── Unified App ────────────────────────────────────────────────────

export default function App() {
  // Run selection
  const [runs, setRuns] = useState<RunInfo[]>([])
  const [selectedRun, setSelectedRun] = useState<string | null>(null)
  const [tab, setTab] = useState<Tab>('topology')

  // Lab state (from SSE)
  const [labState, setLabState] = useState<LabState | null>(null)
  const [labEvents, setLabEvents] = useState<LabEvent[]>([])
  const esRef = useRef<EventSource | null>(null)

  // Log files
  const [logList, setLogList] = useState<LogEntry[]>([])

  // Perf results
  const [simResults, setSimResults] = useState<SimResults | null>(null)

  // Topology selection
  const [selectedNode, setSelectedNode] = useState<string | null>(null)
  const [selectedKind, setSelectedKind] = useState<'router' | 'device' | 'ix'>('router')

  // Cross-tab log jump
  const [logJump, setLogJump] = useState<{ node: string; path: string; timeLabel: string; nonce: number } | null>(null)

  // Events tab time mode
  const [eventsTimeMode, setEventsTimeMode] = useState<'relative' | 'absolute'>('relative')
  const eventsBaseMs = useMemo(() => {
    if (labEvents.length === 0) return null
    const first = parseIsoMs(labEvents[0].timestamp)
    return first
  }, [labEvents])

  // ── Fetch and subscribe to runs ──

  const refreshRuns = useCallback(async () => {
    const r = await fetchRuns()
    setRuns(r)
    setSelectedRun((prev) => {
      if (r.length === 0) return null
      if (prev && r.some((ri) => ri.name === prev)) return prev
      return r[0].name
    })
  }, [])

  useEffect(() => {
    refreshRuns()
    const es = subscribeRuns(() => refreshRuns())
    return () => es.close()
  }, [refreshRuns])

  // ── Load run data when selection changes ──

  useEffect(() => {
    if (!selectedRun) {
      setLabState(null)
      setLabEvents([])
      setLogList([])
      setSimResults(null)
      return
    }

    let dead = false
    Promise.all([
      fetchState(selectedRun),
      fetchLogs(selectedRun),
      fetchResults(selectedRun),
    ]).then(([state, logs, results]) => {
      if (dead) return
      if (state) setLabState(state)
      setLogList(logs)
      setSimResults(results)
    })

    return () => { dead = true }
  }, [selectedRun])

  // ── SSE event subscription (from opid 0 to get historical + live) ──

  useEffect(() => {
    if (!selectedRun) return
    const es = subscribeEvents(selectedRun, 0, (event) => {
      setLabState((prev) => (prev ? applyEvent(prev, event) : prev))
      setLabEvents((prev) => [...prev.slice(-999), event])
    })
    esRef.current = es
    return () => {
      es.close()
      esRef.current = null
    }
  }, [selectedRun])

  // ── Callbacks ──

  const handleNodeSelect = useCallback((name: string, kind: 'router' | 'device' | 'ix') => {
    setSelectedNode(name)
    setSelectedKind(kind)
  }, [])

  const handleJumpToLog = useCallback((target: { node: string; path: string; timeLabel: string }) => {
    setTab('logs')
    setLogJump({ ...target, nonce: Date.now() })
  }, [])

  // ── Derived ──

  const runInfo = runs.find((r) => r.name === selectedRun)
  const base = selectedRun ? runFilesBase(selectedRun) : ''
  const availableTabs: Tab[] = ['topology', 'events', 'logs', 'timeline']
  if (simResults) availableTabs.push('perf')

  // Map LogEntry to SimLogEntry shape for LogsTab/TimelineTab compatibility
  const logsForTabs = logList.map((l) => ({ node: l.node, kind: l.kind, path: l.path }))

  // ── Render ──

  return (
    <div className="app">
      <div className="topbar">
        <h1>patchbay</h1>
        <select
          value={selectedRun ?? ''}
          onChange={(e) => {
            setSelectedRun(e.target.value || null)
            setLabState(null)
            setLabEvents([])
          }}
        >
          <option value="">select run</option>
          {runs.map((r) => (
            <option key={r.name} value={r.name}>
              {r.label ?? r.name}
            </option>
          ))}
        </select>
        {runInfo && (
          <span style={{ color: 'var(--text-muted)', fontSize: 12 }}>
            {runInfo.status ?? ''}
          </span>
        )}
        {labState && (
          <span style={{ color: 'var(--text-muted)', fontSize: 11 }}>
            opid: {labState.opid}
          </span>
        )}
      </div>

      <div className="tabs">
        {availableTabs.map((t) => (
          <button
            key={t}
            className={`tab-btn${tab === t ? ' active' : ''}`}
            onClick={() => setTab(t)}
          >
            {t}
          </button>
        ))}
      </div>

      <div className="tab-content" style={{ display: 'flex', flex: 1, minHeight: 0 }}>
        {tab === 'topology' && labState && (
          <div style={{ display: 'flex', flex: 1, minHeight: 0 }}>
            <div style={{ flex: 1 }}>
              <TopologyGraph state={labState} selectedNode={selectedNode} onNodeSelect={handleNodeSelect} />
            </div>
            {selectedNode && (
              <div
                style={{
                  width: 360,
                  borderLeft: '1px solid var(--border)',
                  overflow: 'auto',
                  padding: 12,
                  background: 'var(--surface)',
                }}
              >
                <NodeDetail state={labState} selectedNode={selectedNode} selectedKind={selectedKind} />
              </div>
            )}
          </div>
        )}
        {tab === 'topology' && !labState && (
          <div className="empty">Loading lab state...</div>
        )}

        {tab === 'events' && (
          <div style={{ flex: 1, display: 'flex', flexDirection: 'column', overflow: 'hidden' }}>
            <div className="logs-toolbar">
              <span>events</span>
              <button className="btn" onClick={() => setEventsTimeMode((v) => (v === 'relative' ? 'absolute' : 'relative'))}>
                time: {eventsTimeMode}
              </button>
            </div>
            <div style={{ flex: 1, overflow: 'auto', fontSize: 12 }}>
              <table>
                <thead>
                  <tr>
                    <th style={{ width: 40 }}>opid</th>
                    <th style={{ width: 100 }}>time</th>
                    <th style={{ width: 120 }}>kind</th>
                    <th>details</th>
                  </tr>
                </thead>
                <tbody>
                  {labEvents.map((e) => {
                    const ms = parseIsoMs(e.timestamp)
                    const timeStr = eventsTimeMode === 'absolute'
                      ? formatTimestamp(e.timestamp)
                      : ms != null && eventsBaseMs != null
                        ? formatRelativeTime(ms, eventsBaseMs)
                        : e.timestamp
                    const pairs = kvPairs(e as Record<string, unknown>, ['opid', 'timestamp', 'kind'])
                    return (
                      <tr key={e.opid}>
                        <td style={{ color: 'var(--text-muted)' }}>{e.opid}</td>
                        <td className="events-time-cell">{timeStr}</td>
                        <td><span className="events-kind">{e.kind}</span></td>
                        <td><KvPairs pairs={pairs} /></td>
                      </tr>
                    )
                  })}
                </tbody>
              </table>
              {labEvents.length === 0 && <div className="empty">No events yet</div>}
            </div>
          </div>
        )}

        {tab === 'logs' && selectedRun && (
          <LogsTab base={base} logs={logsForTabs} jumpTarget={logJump} />
        )}

        {tab === 'timeline' && selectedRun && (
          <TimelineTab base={base} logs={logsForTabs} labEvents={labEvents} onJumpToLog={handleJumpToLog} />
        )}

        {tab === 'perf' && <PerfTab results={simResults} />}
      </div>
    </div>
  )
}
