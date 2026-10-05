export function formatPower(w: number | undefined): string {
  const abs = Math.abs(Math.floor(w || 0))
  const sign = w && w < 0 ? '-' : ''
  return abs >= 1000 ? sign + (abs / 1000).toFixed(1) + 'kW' : sign + abs + 'W'
}

/** Prefer incoming when defined (including explicit 0); else keep previous.
 *  Used by StatCards to avoid flashing 0W / 0% / 0.00V on brief nullish gaps. */
export function holdNumber(
  incoming: number | null | undefined,
  previous: number | undefined
): number | undefined {
  if (incoming !== null && incoming !== undefined && Number.isFinite(incoming)) {
    return incoming
  }
  return previous
}

export function formatUptime(s: number): string {
  if (s < 60) return s + 's'
  if (s < 3600) return Math.floor(s / 60) + 'm'
  const h = Math.floor(s / 3600)
  const m = Math.floor((s % 3600) / 60)
  return h + 'h ' + m + 'm'
}

export function formatInverterState(state: string | undefined): string {
  if (!state) return 'Bulk'
  const normalized = state.trim().toLowerCase()
  if (normalized === 'off') return 'Off'
  return state
}

export function formatDuration(s: number | undefined): string {
  if (!s || s <= 0) return '0:00'
  const h = Math.floor(s / 3600)
  const m = Math.floor((s % 3600) / 60)
  const sec = Math.floor(s % 60)
  if (h > 0) return h + ':' + String(m).padStart(2, '0') + ':' + String(sec).padStart(2, '0')
  return m + ':' + String(sec).padStart(2, '0')
}

/** An absent/invalid source time is unknown, not the time we received a replay. */
export function notificationTimestampMs(tsString: string | undefined): number | null {
  const timestamp = tsString ? Date.parse(tsString) : NaN
  return Number.isFinite(timestamp) && timestamp > 0 ? timestamp : null
}

/** Relative age matching Victron GUIv2 style (e.g. "10h 16m ago"). */
export function formatTimestamp(tsString: string | undefined, now = Date.now()): string {
  const ms = notificationTimestampMs(tsString)
  if (ms === null) return ''
  const timestamp = new Date(ms)
  const diffMs = now - ms
  // Clock skew must not turn a future source timestamp into a new event.
  if (diffMs < 0) return timestamp.toLocaleString(undefined, { timeZoneName: 'short' })
  const diffSec = Math.floor(diffMs / 1000)
  if (diffSec < 60) return 'just now'
  const diffMin = Math.floor(diffSec / 60)
  if (diffMin < 60) return `${diffMin}m ago`
  const diffHours = Math.floor(diffMin / 60)
  const remMin = diffMin % 60
  if (diffHours < 24) {
    return remMin === 0 ? `${diffHours}h ago` : `${diffHours}h ${remMin}m ago`
  }
  const diffDays = Math.floor(diffHours / 24)
  if (diffDays < 7) return `${diffDays}d ago`
  return timestamp
    .toLocaleString(undefined, {
      year: 'numeric',
      month: '2-digit',
      day: '2-digit',
      hour: '2-digit',
      minute: '2-digit',
    })
    .replace(',', '')
}

/** Normalize boolean-like transport values without interpreting "false" as true. */
export function coerceBoolean(value: unknown): boolean {
  return (
    value === true ||
    value === 1 ||
    value === 'true' ||
    value === '1' ||
    value === 'on' ||
    value === 'online'
  )
}
