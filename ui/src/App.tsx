import { useCallback, useEffect, useRef, useState } from 'react'
import { useLocation, useNavigate } from 'react-router-dom'
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
import type { CombinedResults, SimResults } from './types'
import {
  fetchRuns,
  fetchState,
  subscribeEvents,
  subscribeRuns,
  fetchLogs,
  fetchResults,
  fetchCombinedResults,
  runFilesBase,
} from './api'
import type { RunInfo, LogEntry } from './api'
import LogsTab from './components/LogsTab'
import PerfTab from './components/PerfTab'
import TimelineTab from './components/TimelineTab'
import TopologyGraph from './components/TopologyGraph'
import NodeDetail from './components/NodeDetail'

type Tab = 'topology' | 'logs' | 'timeline' | 'perf'

// ── Selection model ────────────────────────────────────────────────

type Selection =
  | { kind: 'run'; name: string }
  | { kind: 'invocation'; name: string }

function selectionKey(s: Selection | null): string {
  if (!s) return ''
  return s.kind === 'invocation' ? `inv:${s.name}` : s.name
}

function selectionPath(s: Selection | null): string {
  if (!s) return '/'
  return s.kind === 'invocation' ? `/inv/${s.name}` : `/run/${s.name}`
}

// ── Invocation grouping ────────────────────────────────────────────

interface InvocationGroup {
  invocation: string
  runs: RunInfo[]
}

function groupByInvocation(runs: RunInfo[]): { groups: InvocationGroup[]; ungrouped: RunInfo[] } {
  const grouped = new Map<string, RunInfo[]>()
  const ungrouped: RunInfo[] = []
  for (const r of runs) {
    if (r.invocation) {
      let list = grouped.get(r.invocation)
      if (!list) {
        list = []
        grouped.set(r.invocation, list)
      }
      list.push(r)
    } else {
      ungrouped.push(r)
    }
  }
  const groups: InvocationGroup[] = []
  for (const [invocation, groupRuns] of grouped) {
    groups.push({ invocation, runs: groupRuns })
  }
  return { groups, ungrouped }
}

/** Short display label for a run within an invocation group. */
function simLabel(run: RunInfo): string {
  if (run.invocation && run.name.startsWith(run.invocation + '/')) {
    return run.label ?? run.name.slice(run.invocation.length + 1)
  }
  return run.label ?? run.name
}

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

export default function App({ mode }: { mode: 'run' | 'inv' }) {
  const location = useLocation()
  const navigate = useNavigate()

  // Derive selection from the URL path.
  // Route is /run/* or /inv/* so everything after the prefix is the name.
  const nameFromUrl = location.pathname.slice(mode === 'run' ? 5 : 5) // "/run/" or "/inv/" = 5 chars
  const selection: Selection | null = nameFromUrl
    ? { kind: mode === 'inv' ? 'invocation' : 'run', name: nameFromUrl }
    : null

  const selectedRun = selection?.kind === 'run' ? selection.name : null
  const selectedInvocation = selection?.kind === 'invocation' ? selection.name : null

  const [tab, setTab] = useState<Tab>('topology')

  // Run list (for the dropdown)
  const [runs, setRuns] = useState<RunInfo[]>([])

  // Lab state (from SSE)
  const [labState, setLabState] = useState<LabState | null>(null)
  const [labEvents, setLabEvents] = useState<LabEvent[]>([])
  const esRef = useRef<EventSource | null>(null)
  const runsEsRef = useRef<EventSource | null>(null)

  // Log files
  const [logList, setLogList] = useState<LogEntry[]>([])

  // Perf results
  const [simResults, setSimResults] = useState<SimResults | null>(null)
  const [combinedResults, setCombinedResults] = useState<CombinedResults | null>(null)

  // Topology selection
  const [selectedNode, setSelectedNode] = useState<string | null>(null)
  const [selectedKind, setSelectedKind] = useState<'router' | 'device' | 'ix'>('router')

  // Cross-tab log jump
  const [logJump, setLogJump] = useState<{ node: string; path: string; timeLabel: string; nonce: number } | null>(null)

  // ── Fetch and subscribe to runs ──

  const refreshRuns = useCallback(async () => {
    const r = await fetchRuns()
    setRuns(r)
  }, [])

  useEffect(() => {
    refreshRuns()
    const es = subscribeRuns(() => refreshRuns())
    runsEsRef.current = es
    return () => {
      es.close()
      runsEsRef.current = null
    }
  }, [refreshRuns])

  // ── Load run data when an individual sim is selected ──

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

  // ── Load combined results when an invocation is selected ──

  useEffect(() => {
    if (!selectedInvocation) {
      setCombinedResults(null)
      return
    }

    let dead = false
    fetchCombinedResults(selectedInvocation).then((results) => {
      if (dead) return
      setCombinedResults(results)
    })

    return () => { dead = true }
  }, [selectedInvocation])

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

  // Close SSE connections on page unload/refresh.
  useEffect(() => {
    const cleanup = () => {
      runsEsRef.current?.close()
      esRef.current?.close()
    }
    window.addEventListener('beforeunload', cleanup)
    return () => window.removeEventListener('beforeunload', cleanup)
  }, [])

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

  const base = selectedRun ? runFilesBase(selectedRun) : ''
  const isSimView = selection?.kind === 'run'
  const isInvocationView = selection?.kind === 'invocation'

  const availableTabs: Tab[] = isSimView
    ? ['topology', 'logs', 'timeline', ...(simResults ? (['perf'] as Tab[]) : [])]
    : isInvocationView
      ? ['perf']
      : []

  // When available tabs change, ensure current tab is still valid.
  useEffect(() => {
    if (availableTabs.length > 0 && !availableTabs.includes(tab)) {
      setTab(availableTabs[0])
    }
  }, [availableTabs, tab])

  // Map LogEntry to SimLogEntry shape for LogsTab/TimelineTab compatibility
  const logsForTabs = logList.map((l) => ({ node: l.node, kind: l.kind, path: l.path }))

  // Group runs for the selector
  const { groups, ungrouped } = groupByInvocation(runs)

  // ── Render ──

  return (
    <div className="app">
      <div className="topbar">
        <h1><a href="#/" style={{ color: 'inherit', textDecoration: 'none' }}>patchbay</a></h1>
        <select
          value={selectionKey(selection)}
          onChange={(e) => {
            const val = e.target.value
            if (!val) {
              navigate('/')
              return
            }
            if (val.startsWith('inv:')) {
              navigate(`/inv/${val.slice(4)}`)
            } else {
              navigate(`/run/${val}`)
            }
          }}
        >
          <option value="">select run</option>
          {groups.map((g) => (
            <optgroup key={g.invocation} label={g.invocation}>
              {g.runs.length > 1 && (
                <option value={`inv:${g.invocation}`}>
                  combined ({g.runs.length} sims)
                </option>
              )}
              {g.runs.map((r) => (
                <option key={r.name} value={r.name}>
                  {simLabel(r)}
                </option>
              ))}
            </optgroup>
          ))}
          {ungrouped.map((r) => (
            <option key={r.name} value={r.name}>
              {r.label ?? r.name}
            </option>
          ))}
        </select>
        {isSimView && runs.find((r) => r.name === selectedRun) && (
          <span style={{ color: 'var(--text-muted)', fontSize: 12 }}>
            {runs.find((r) => r.name === selectedRun)?.status ?? ''}
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
        {tab === 'topology' && !labState && isSimView && (
          <div className="empty">Loading lab state...</div>
        )}

        {tab === 'logs' && selectedRun && (
          <LogsTab base={base} logs={logsForTabs} jumpTarget={logJump} />
        )}

        {tab === 'timeline' && selectedRun && (
          <TimelineTab base={base} logs={logsForTabs} labEvents={labEvents} onJumpToLog={handleJumpToLog} />
        )}

        {tab === 'perf' && isSimView && <PerfTab results={simResults} />}
        {tab === 'perf' && isInvocationView && <PerfTab results={null} combined={combinedResults} onSimSelect={(sim) => navigate(`/run/${sim}`)} />}
      </div>
    </div>
  )
}
