// Meter snapshots arrive every 30 seconds, followed by the display/transport update.
export const GRID_ENERGY_STALE_AFTER_MS = 90_000

export interface GridEnergyDaily {
  date: string
  time_zone: string
  import_kwh: number | null
  export_kwh: number | null
  observed_at: number | null
  started_at: number | null
  complete: boolean
  status: 'complete' | 'partial' | 'stale' | 'unknown' | 'reset'
  reason?: string
  source: { service: string; device_instance: number; serial?: string | null }
}

export interface DailyGridPresentation {
  label: string
  imported: string
  exported: string
  details: string
}

const record = (value: unknown): value is Record<string, unknown> =>
  !!value && typeof value === 'object' && !Array.isArray(value)
const nonnegative = (value: unknown): value is number =>
  typeof value === 'number' && Number.isFinite(value) && value >= 0
const timestamp = (value: unknown): value is number => nonnegative(value) && value * 1000 <= 8.64e15
const GRID_SERVICE_PREFIX = 'com.victronenergy.grid.'

// Keep the frequent freshness tick cheap without accumulating arbitrary server time zones.
let cachedZone: string | undefined
let cachedFormatter: Intl.DateTimeFormat | undefined
function siteParts(at: number, timeZone: string) {
  if (cachedZone !== timeZone || !cachedFormatter) {
    const formatter = new Intl.DateTimeFormat('en-GB', {
      timeZone,
      year: 'numeric',
      month: '2-digit',
      day: '2-digit',
      hour: '2-digit',
      minute: '2-digit',
      hourCycle: 'h23',
    })
    cachedZone = timeZone
    cachedFormatter = formatter
  }
  const parts = cachedFormatter.formatToParts(at)
  const part = (type: string) => parts.find((value) => value.type === type)?.value
  return {
    date: `${part('year')}-${part('month')}-${part('day')}`,
    time: `${part('hour')}:${part('minute')}`,
  }
}

export function dailyGridPresentation(value: unknown, now: number): DailyGridPresentation {
  let label = 'Today'
  let context = 'Grid energy: ↓ import / ↑ export.'
  const unavailable = (reason: string): DailyGridPresentation => ({
    label,
    imported: '—',
    exported: '—',
    details: `${context} ${reason}`,
  })
  if (!record(value)) return unavailable('Daily meter readings are unavailable.')
  if (
    typeof value.date !== 'string' ||
    !/^\d{4}-\d{2}-\d{2}$/.test(value.date) ||
    typeof value.time_zone !== 'string' ||
    !value.time_zone ||
    /^[+-]/.test(value.time_zone)
  )
    return unavailable('The meter date or IANA time zone is invalid.')

  try {
    const current = siteParts(now, value.time_zone)
    context += ` ${value.date} (${value.time_zone}).`
    if (value.date !== current.date)
      return unavailable('Readings do not belong to the current day at the meter site.')
    if (typeof value.status === 'string') context += ` Status: ${value.status}.`
    if (typeof value.reason === 'string' && value.reason.trim()) context += ` ${value.reason}`

    const source = value.source
    if (
      !record(source) ||
      typeof source.service !== 'string' ||
      !source.service.startsWith(GRID_SERVICE_PREFIX) ||
      !source.service.slice(GRID_SERVICE_PREFIX.length).trim() ||
      source.service !== source.service.trim() ||
      !nonnegative(source.device_instance) ||
      !Number.isSafeInteger(source.device_instance) ||
      (source.serial != null && typeof source.serial !== 'string')
    )
      return unavailable('The direct meter source is unavailable or invalid.')
    context += ` Source: ${source.service}, instance ${source.device_instance}${source.serial ? `, serial ${source.serial}` : ''}.`

    if (
      !timestamp(value.observed_at) ||
      !timestamp(value.started_at) ||
      typeof value.complete !== 'boolean'
    )
      return unavailable('The observation time or coverage is unavailable or invalid.')
    const observed = value.observed_at * 1000
    const started = value.started_at * 1000
    const start = siteParts(started, value.time_zone)
    if (start.date !== value.date || started > observed)
      return unavailable('Coverage must start within this site day, before the observation.')
    if (value.complete) {
      // The first instant of a local day can change with DST; never assume 24-hour days.
      if (!Number.isInteger(started) || siteParts(started - 1, value.time_zone).date === value.date)
        return unavailable('Full-day coverage has no verified start-of-day reading.')
      context += ' Coverage: from the start of the site day.'
    } else {
      label = `Since ${start.time}`
      context += ` Partial coverage: since ${start.time}; excludes earlier energy today.`
    }
    context += ` Observed: ${siteParts(observed, value.time_zone).time}.`
    // Allow only one second of clock skew between the controller and this device.
    if (observed > now + 1000) return unavailable('The meter observation is in the future.')
    if (siteParts(observed, value.time_zone).date !== value.date)
      return unavailable('The meter observation belongs to another site day.')
    if (now - observed > GRID_ENERGY_STALE_AFTER_MS)
      return unavailable(
        `The meter observation is stale (older than ${GRID_ENERGY_STALE_AFTER_MS / 1000} seconds).`
      )
    if (!(
      (value.status === 'complete' && value.complete) ||
      (value.status === 'partial' && !value.complete)
    ))
      return unavailable('The controller has not confirmed usable daily readings.')

    const imported = nonnegative(value.import_kwh) ? value.import_kwh.toFixed(2) : '—'
    const exported = nonnegative(value.export_kwh) ? value.export_kwh.toFixed(2) : '—'
    if (imported === '—') context += ' Import reading is unavailable or invalid.'
    if (exported === '—') context += ' Export reading is unavailable or invalid.'
    return { label, imported, exported, details: context }
  } catch {
    return unavailable('The meter date or IANA time zone is invalid.')
  }
}
