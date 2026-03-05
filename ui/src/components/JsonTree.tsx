import { useState } from 'react'

interface Props {
  data: unknown
  defaultDepth?: number
}

export default function JsonTree({ data, defaultDepth = 1 }: Props) {
  return <JsonValue value={data} depth={0} defaultDepth={defaultDepth} />
}

function JsonValue({ value, depth, defaultDepth }: { value: unknown; depth: number; defaultDepth: number }) {
  if (value === null) return <span className="jt-null">null</span>
  if (typeof value === 'boolean') return <span className="jt-bool">{String(value)}</span>
  if (typeof value === 'number') return <span className="jt-num">{String(value)}</span>
  if (typeof value === 'string') return <span className="jt-str">"{value}"</span>
  if (Array.isArray(value)) return <JsonArray items={value} depth={depth} defaultDepth={defaultDepth} />
  if (typeof value === 'object') return <JsonObject obj={value as Record<string, unknown>} depth={depth} defaultDepth={defaultDepth} />
  return <span>{String(value)}</span>
}

function JsonArray({ items, depth, defaultDepth }: { items: unknown[]; depth: number; defaultDepth: number }) {
  const [open, setOpen] = useState(depth < defaultDepth)
  if (items.length === 0) return <span className="jt-brace">[]</span>
  const toggle = () => setOpen((v) => !v)
  if (!open) {
    return (
      <span className="jt-toggle" onClick={toggle}>
        <span className="jt-brace">[</span>
        <span className="jt-ellipsis">{items.length} {items.length === 1 ? 'item' : 'items'}</span>
        <span className="jt-brace">]</span>
      </span>
    )
  }
  return (
    <span>
      <span className="jt-toggle jt-brace" onClick={toggle}>[</span>
      <div className="jt-indent">
        {items.map((item, i) => (
          <div key={i} className="jt-row">
            <JsonValue value={item} depth={depth + 1} defaultDepth={defaultDepth} />
            {i < items.length - 1 && <span className="jt-comma">,</span>}
          </div>
        ))}
      </div>
      <span className="jt-brace">]</span>
    </span>
  )
}

function JsonObject({ obj, depth, defaultDepth }: { obj: Record<string, unknown>; depth: number; defaultDepth: number }) {
  const [open, setOpen] = useState(depth < defaultDepth)
  const entries = Object.entries(obj)
  if (entries.length === 0) return <span className="jt-brace">{'{}'}</span>
  const toggle = () => setOpen((v) => !v)
  if (!open) {
    return (
      <span className="jt-toggle" onClick={toggle}>
        <span className="jt-brace">{'{'}</span>
        <span className="jt-ellipsis">{entries.length} {entries.length === 1 ? 'field' : 'fields'}</span>
        <span className="jt-brace">{'}'}</span>
      </span>
    )
  }
  return (
    <span>
      <span className="jt-toggle jt-brace" onClick={toggle}>{'{'}</span>
      <div className="jt-indent">
        {entries.map(([key, val], i) => (
          <div key={key} className="jt-row">
            <span className="jt-key">"{key}"</span>
            <span className="jt-colon">: </span>
            <JsonValue value={val} depth={depth + 1} defaultDepth={defaultDepth} />
            {i < entries.length - 1 && <span className="jt-comma">,</span>}
          </div>
        ))}
      </div>
      <span className="jt-brace">{'}'}</span>
    </span>
  )
}
