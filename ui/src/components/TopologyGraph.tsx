import { useCallback, useEffect, useMemo } from 'react'
import {
  ReactFlow,
  type Node,
  type Edge,
  useNodesState,
  useEdgesState,
  Position,
  MarkerType,
} from '@xyflow/react'
import '@xyflow/react/dist/style.css'
import dagre from 'dagre'
import type { LabState, Nat } from '../devtools-types'

interface Props {
  state: LabState
  selectedNode?: string | null
  onNodeSelect: (name: string, kind: 'router' | 'device' | 'ix') => void
}

function layoutGraph(nodes: Node[], edges: Edge[]): Node[] {
  const g = new dagre.graphlib.Graph()
  g.setDefaultEdgeLabel(() => ({}))
  g.setGraph({ rankdir: 'TB', ranksep: 80, nodesep: 40 })

  for (const node of nodes) {
    g.setNode(node.id, { width: 180, height: 60 })
  }
  for (const edge of edges) {
    g.setEdge(edge.source, edge.target)
  }

  dagre.layout(g)

  return nodes.map((node) => {
    const pos = g.node(node.id)
    return {
      ...node,
      position: { x: pos.x - 90, y: pos.y - 30 },
      sourcePosition: Position.Bottom,
      targetPosition: Position.Top,
    }
  })
}

function natLabel(nat: Nat): string {
  if (typeof nat === 'string') return nat
  if ('custom' in nat) return 'custom'
  return '?'
}

function selCls(nodeId: string, sel: string | null | undefined): string {
  if (!sel) return ''
  if (nodeId === '_ix' && sel === 'ix') return ' topology-node--selected'
  if (nodeId === `router:${sel}` || nodeId === `device:${sel}`) return ' topology-node--selected'
  return ''
}

export default function TopologyGraph({ state, selectedNode, onNodeSelect }: Props) {
  const { rawNodes, rawEdges } = useMemo(() => {
    const ns: Node[] = []
    const es: Edge[] = []

    // IX node
    if (state.ix) {
      ns.push({
        id: '_ix',
        type: 'default',
        data: { label: `IX (${state.ix.gw})` },
        position: { x: 0, y: 0 },
        className: `topology-node topology-node--ix${selCls('_ix', selectedNode)}`,
        style: { borderRadius: '50%', width: 80, height: 80, display: 'flex', alignItems: 'center', justifyContent: 'center' },
      })
    }

    // Router nodes
    for (const [name, router] of Object.entries(state.routers)) {
      const badges: string[] = []
      badges.push(`nat:${natLabel(router.nat)}`)
      if (router.firewall && router.firewall !== 'none') {
        badges.push(`fw:${typeof router.firewall === 'string' ? router.firewall : 'custom'}`)
      }
      const label = `${name}${router.region ? ` [${router.region}]` : ''}\n${badges.join(' ')}`

      ns.push({
        id: `router:${name}`,
        type: 'default',
        data: { label },
        position: { x: 0, y: 0 },
        className: `topology-node topology-node--router${selCls(`router:${name}`, selectedNode)}`,
        style: { borderRadius: 12 },
      })

      // Edge: upstream → this router
      const upstream = router.upstream ? `router:${router.upstream}` : '_ix'
      es.push({
        id: `e-${upstream}-router:${name}`,
        source: upstream,
        target: `router:${name}`,
        markerEnd: { type: MarkerType.ArrowClosed },
        style: { stroke: '#30363d' },
      })
    }

    // Device nodes
    for (const [name, device] of Object.entries(state.devices)) {
      const ips = device.interfaces
        .map((i) => i.ip)
        .filter(Boolean)
        .join(', ')
      const label = `${name}\n${ips}`

      ns.push({
        id: `device:${name}`,
        type: 'default',
        data: { label },
        position: { x: 0, y: 0 },
        className: `topology-node topology-node--device${selCls(`device:${name}`, selectedNode)}`,
      })

      // Edge: each interface → its router
      for (const iface of device.interfaces) {
        es.push({
          id: `e-router:${iface.router}-device:${name}-${iface.name}`,
          source: `router:${iface.router}`,
          target: `device:${name}`,
          label: iface.name,
          style: { stroke: '#30363d' },
        })
      }
    }

    // Region links (between routers)
    for (const link of state.region_links) {
      es.push({
        id: `region-${link.a}-${link.b}`,
        source: `router:${link.a}`,
        target: `router:${link.b}`,
        animated: !link.broken,
        style: {
          stroke: link.broken ? '#f85149' : '#3fb950',
          strokeDasharray: link.broken ? '5 5' : undefined,
        },
        label: link.broken ? 'broken' : undefined,
      })
    }

    return { rawNodes: ns, rawEdges: es }
  }, [state, selectedNode])

  const laidOutNodes = useMemo(() => layoutGraph(rawNodes, rawEdges), [rawNodes, rawEdges])

  const [nodes, setNodes, onNodesChange] = useNodesState(laidOutNodes)
  const [edges, , onEdgesChange] = useEdgesState(rawEdges)

  useEffect(() => {
    setNodes(laidOutNodes)
  }, [laidOutNodes, setNodes])

  const handleNodeClick = useCallback(
    (_: React.MouseEvent, node: Node) => {
      if (node.id === '_ix') {
        onNodeSelect('ix', 'ix')
      } else if (node.id.startsWith('router:')) {
        onNodeSelect(node.id.slice(7), 'router')
      } else if (node.id.startsWith('device:')) {
        onNodeSelect(node.id.slice(7), 'device')
      }
    },
    [onNodeSelect],
  )

  return (
    <div style={{ width: '100%', height: '100%' }}>
      <ReactFlow
        nodes={nodes}
        edges={edges}
        onNodesChange={onNodesChange}
        onEdgesChange={onEdgesChange}
        onNodeClick={handleNodeClick}
        fitView
        proOptions={{ hideAttribution: true }}
      />
    </div>
  )
}
