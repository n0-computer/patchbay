/** Shared time formatting utilities for events and timeline views. */

export function parseIsoMs(value: string): number | null {
  const ms = Date.parse(value)
  return Number.isFinite(ms) ? ms : null
}

/** Format an ISO timestamp to a short, readable form: HH:MM:SS.mmm */
export function formatTimestamp(iso: string): string {
  const d = new Date(iso)
  if (isNaN(d.getTime())) return iso
  const h = String(d.getHours()).padStart(2, '0')
  const m = String(d.getMinutes()).padStart(2, '0')
  const s = String(d.getSeconds()).padStart(2, '0')
  const ms = String(d.getMilliseconds()).padStart(3, '0')
  return `${h}:${m}:${s}.${ms}`
}

/** Format a relative offset: +0.000s, +1.234s, etc. */
export function formatRelativeTime(ms: number, baseMs: number): string {
  const delta = Math.max(0, ms - baseMs)
  return `+${(delta / 1000).toFixed(3)}s`
}

/** Render an object's entries as key=value pairs, excluding given keys. */
export function kvPairs(obj: Record<string, unknown>, exclude: string[]): Array<{ key: string; value: string }> {
  return Object.entries(obj)
    .filter(([k]) => !exclude.includes(k))
    .map(([k, v]) => ({
      key: k,
      value: typeof v === 'string' ? v : JSON.stringify(v),
    }))
}
