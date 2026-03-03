/** Renders key=value pairs with colored keys for easy scanning. */
export default function KvPairs({ pairs }: { pairs: Array<{ key: string; value: string }> }) {
  if (pairs.length === 0) return <span className="kv-empty">(no fields)</span>
  return (
    <span className="kv-pairs">
      {pairs.map((p, i) => (
        <span key={i} className="kv-pair">
          <span className="kv-key">{p.key}</span>
          <span className="kv-eq">=</span>
          <span className="kv-value">{p.value}</span>
        </span>
      ))}
    </span>
  )
}
