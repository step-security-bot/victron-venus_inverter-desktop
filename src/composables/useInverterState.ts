import type { EssModeState } from '../essMode'
import { markRaw, ref, shallowRef } from 'vue'
// shallowRef + replace-with-new-object (see applyInverterState): nested loads/etc.
// update when MQTT sends a fresh snapshot. markRaw avoids deep-proxying big payloads.
import type { AppConfig } from '../config'
import type { DashboardControl } from '../inverterControl'
import { invoke } from '@tauri-apps/api/core'

export interface GridBackupStatus {
  enabled: boolean
  available: boolean
  service: string | null
  device_instance: number | null
  name: string | null
  power: number | null
  measurement_time: number | null
  age_seconds: number | null
}

export interface InverterState {
  /** Backend IGW snapshots replace their owned fields; MQTT events stay partial. */
  gateway_snapshot?: boolean
  /** Cached event (failed backend poll or frontend get_state hydration). */
  cached_snapshot?: boolean
  grid_backup?: GridBackupStatus | null
  grid_using_backup?: boolean
  grid_backup_observed_at?: number
  gt?: number
  g1?: number
  g2?: number
  /** False explicitly invalidates a phase; omitted means no availability update. */
  grid_l1_available?: boolean
  grid_l2_available?: boolean
  tt?: number
  t1?: number
  t2?: number
  solar_total?: number
  mppt_total?: number
  battery_soc?: number
  battery_power?: number
  battery_voltage?: number
  battery_current?: number
  setpoint?: number
  inverter_state?: string
  version?: string
  uptime?: number
  ha_connected?: boolean
  ha_direct_connected?: boolean
  dry_run?: boolean
  ess_mode?: EssModeState
  /** Live ESS observation time (seconds); absent/null after retained-only state. */
  ess_mode_observed_at?: number | null
  booleans?: Record<string, boolean>
  features?: Record<string, boolean>
  mppt_individual?: number[]
  mppt_chargers?: Array<{
    name?: string
    serial?: string
    instance?: number
    pv_voltage?: number
    current?: number
    power?: number
  }>
  // AC PV inverters of any vendor (V/I/P per device)
  pv_inverters?: Array<{
    name?: string
    serial?: string
    instance?: number
    voltage?: number
    current?: number
    power?: number
  }>
  // Legacy daemon aggregate: power per inverter (no V/I), fallback when pv_inverters empty
  pv_inverter_individual?: number[]
  batteries?: Array<{
    name?: string
    serial?: string
    instance?: number
    voltage?: number
    current?: number
    power?: number
    soc?: number
    state?: string
    time_to_go?: string
  }>
  loads?: Record<string, number>
  /** Cerbo acload instance id → CustomName/ProductName (stable; loads stay id-keyed). */
  load_names?: Record<string, string>
  ui_config?: {
    electricity_tariff?: unknown
    electricity_tariff_status?: {
      writable?: boolean
      revision?: string
      request_id?: string
      error?: string | null
    }
    home_buttons?: DashboardControl[]
    header_toggles?: DashboardControl[]
  }
  daily_stats?: {
    produced_today?: number
    produced_yesterday?: number
    produced_dollars?: number
    grid_kwh?: number | null
    /** Controller-owned meter counters; the presentation validates this optional wire payload. */
    grid_energy?: unknown
    battery_in?: number
    battery_out?: number
    battery_in_yesterday?: number
    battery_out_yesterday?: number
    pv_inverter_daily?: number[]
    mppt_daily?: number[]
    pv_total_daily?: number
  }
  solar_forecast?: {
    date?: string
    today_kwh?: number
    tomorrow_kwh?: number
  }
  latest_version?: string
  /** Water system, fed by dbus-pump via Cerbo GX MQTT */
  water_level?: number | null
  water_valve?: boolean | null
  pump_switch?: boolean | null
  /** dbus-pump /Mode (0 auto, 1 always-on, 2 always-off); null until known */
  water_pump_mode?: number | null
  water_valve_mode?: number | null
  /** EV vehicle battery % from dbus-ev via Cerbo MQTT (N/<portal>/ev/<i>/Soc) */
  car_soc?: number | null
  /** EV vehicle charging power (W) from dbus-ev (N/<portal>/ev/<i>/Ac/Power) */
  car_charging_power?: number | null
  /** Wallbox charging power (W) from dbus-evcharger (N/<portal>/evcharger/<i>/Ac/Power) */
  ev_charging_power?: number | null
  /** True once any ev/<i>/... message for the configured instance has been
   *  seen on Cerbo MQTT. Survives daemon merges; gates the EV card. */
  ev_present?: boolean
  /** True once any evcharger/<i>/... message has been seen on Cerbo MQTT. */
  evcharger_present?: boolean
  /** Cerbo GX tank/pump/ev/evcharger instances discovered via MQTT. */
  discovered_water_ev?: Array<{
    instance: number
    kind: 'tank' | 'pump' | 'ev' | 'evcharger' | string
    name?: string | null
  }> | null
}

export const state = shallowRef<InverterState>({
  booleans: {},
  features: {},
  ui_config: {},
})

export const mqttConnected = ref(false)
export const dataSource = ref<'mqtt' | 'igw'>('mqtt')
export const appConfig = ref<AppConfig | null>(null)

export type TelemetryQuality = 'live' | 'stale' | 'unknown'
export const TELEMETRY_STALE_AFTER_MS = 30_000
export interface TelemetryMetadata {
  /** Time this desktop observed a live transport update, not the device measurement time. */
  observed_at: number | null
  source: 'mqtt' | 'igw' | null
  quality: TelemetryQuality
  fields: Record<string, { observed_at: number; source: 'mqtt' | 'igw' }>
  stale_fields: string[]
}
export const telemetry = shallowRef<TelemetryMetadata>({
  observed_at: null,
  source: null,
  quality: 'unknown',
  fields: {},
  stale_fields: [],
})

/** Wall clock updated by refreshTelemetryQuality so ESS freshness re-renders. */
export const telemetryClockMs = shallowRef(Date.now())

export function refreshTelemetryQuality(now = Date.now()) {
  // Always advance the reactive clock so bindings that depend on wall time
  // (ESS command freshness) re-evaluate even when quality/stale_fields are unchanged.
  telemetryClockMs.value = now
  const previous = telemetry.value
  const staleFields = Object.keys(previous.fields).filter(
    (field) => now - previous.fields[field].observed_at > TELEMETRY_STALE_AFTER_MS
  )
  const quality: TelemetryQuality =
    previous.observed_at === null
      ? 'unknown'
      : !mqttConnected.value ||
          now - previous.observed_at > TELEMETRY_STALE_AFTER_MS ||
          staleFields.length > 0
        ? 'stale'
        : 'live'
  if (quality !== previous.quality || staleFields.join() !== previous.stale_fields.join()) {
    telemetry.value = { ...previous, quality, stale_fields: staleFields }
  }
}

export function resetInverterState() {
  state.value = { booleans: {}, features: {}, ui_config: {} }
  telemetry.value = {
    observed_at: null,
    source: null,
    quality: 'unknown',
    fields: {},
    stale_fields: [],
  }
}

// Independent Home Assistant/plugin overlays (notably ha_direct_connected and
// appliance fields) are not part of IGW's authoritative snapshot.
const GATEWAY_OWNED_FIELDS = [
  'booleans',
  'dry_run',
  'ess_mode',
  'ess_mode_observed_at',
  'ui_config',
  'features',
  'version',
  'uptime',
  'ha_connected',
  'daily_stats',
  'solar_forecast',
  'setpoint_override',
  'grid_backup',
  'grid_using_backup',
  'grid_backup_observed_at',
  'water_level',
  'pump_switch',
  'water_valve',
  'water_pump_mode',
  'water_valve_mode',
  'car_soc',
  'car_charging_power',
  'ev_charging_power',
  'ev_power',
  'ev_charging_kw',
  'ev_present',
  'evcharger_present',
  'discovered_water_ev',
] as const

/** Non-destructive merge into dashboard state. Skips null/undefined so partial
 *  MQTT snapshots and serde nulls cannot wipe live telemetry. Always assigns a
 *  new markRaw object so shallowRef watchers/tiles re-render. */
export function applyInverterState(
  newState: InverterState,
  observation: { snapshot?: boolean; observedAt?: number; source?: 'mqtt' | 'igw' } = {}
) {
  const prev = state.value
  const merged: InverterState = { ...prev }
  const cachedSnapshot = observation.snapshot === true || newState.cached_snapshot === true
  if (newState.gateway_snapshot) {
    for (const key of GATEWAY_OWNED_FIELDS) {
      if ((newState as Record<string, unknown>)[key] == null) {
        delete (merged as Record<string, unknown>)[key]
      }
    }
  }
  for (const [key, val] of Object.entries(newState)) {
    if (val !== undefined && val !== null) {
      ;(merged as Record<string, unknown>)[key] = val
    }
  }
  // Provenance belongs to this event, never to the held measurement values.
  merged.cached_snapshot = cachedSnapshot
  if (newState.grid_backup === null) {
    delete merged.grid_backup
    merged.grid_using_backup = false
  }
  // Retained inverter/state clears the command gate; do not hold a prior live stamp.
  if (Object.prototype.hasOwnProperty.call(newState, 'ess_mode_observed_at')) {
    if (newState.ess_mode_observed_at == null) delete merged.ess_mode_observed_at
    else merged.ess_mode_observed_at = newState.ess_mode_observed_at
  }
  // Cerbo explicitly publishes null for unavailable/unused grid phases. Keep
  // ordinary partial-message holding, but never resurrect an invalid phase
  // or add its previous value into the live total.
  if (merged.grid_l1_available === false) delete merged.g1
  if (merged.grid_l2_available === false) delete merged.g2
  if (
    typeof merged.grid_l1_available === 'boolean' ||
    typeof merged.grid_l2_available === 'boolean'
  ) {
    const phases = [merged.g1, merged.g2].filter(
      (value): value is number => typeof value === 'number' && Number.isFinite(value)
    )
    merged.gt = phases.length ? phases.reduce((sum, value) => sum + value, 0) : undefined
  }
  // IGW serde omits null time_to_go; a partial MQTT/IGW race must not blank the
  // "40h 48m" chip every couple of seconds while still Charging/Discharging.
  if (Array.isArray(merged.batteries) && Array.isArray(prev.batteries)) {
    merged.batteries = merged.batteries.map((bat) => {
      if (bat.time_to_go) return bat
      // Only drop sticky ETA on explicit Idle — Unknown/missing state still
      // keeps the last Charging/Discharging time_to_go so 2s IGW polls do not blink.
      if (bat.state === 'Idle') return bat
      const prevBat = prev.batteries?.find(
        (p) =>
          (p.serial && bat.serial && p.serial === bat.serial) ||
          (p.instance != null && bat.instance != null && p.instance === bat.instance) ||
          (p.name && bat.name && p.name === bat.name)
      )
      if (prevBat?.time_to_go) {
        return { ...bat, time_to_go: prevBat.time_to_go }
      }
      return bat
    })
  }
  state.value = markRaw(merged)
  const observedAt = observation.observedAt ?? Date.now()
  const source = observation.source ?? dataSource.value
  const fields = { ...telemetry.value.fields }
  let invalidated = false
  if (newState.gateway_snapshot) {
    for (const key of GATEWAY_OWNED_FIELDS) {
      if ((newState as Record<string, unknown>)[key] == null && key in fields) {
        delete fields[key]
        invalidated = true
      }
    }
  }
  let observed = false
  // Window snapshots and failed-poll replays may update visuals/invalidation,
  // but only a genuine transport update can renew observation timestamps.
  if (!cachedSnapshot) {
    for (const [key, value] of Object.entries(newState)) {
      if (value === null || value === undefined) continue
      // Discovery/config metadata has no periodic measurement cadence.
      if (
        [
          'gateway_snapshot',
          'cached_snapshot',
          'ui_config',
          'features',
          'version',
          'latest_version',
          'load_names',
          'discovered_water_ev',
          'ev_present',
          'evcharger_present',
        ].includes(key)
      )
        continue
      fields[key] = { observed_at: observedAt, source }
      observed = true
    }
  }
  if (observed || invalidated) {
    telemetry.value = {
      ...telemetry.value,
      ...(observed ? { observed_at: observedAt, source } : {}),
      fields,
    }
  }
  refreshTelemetryQuality(observedAt)
}

export interface NotificationEntry {
  id: number
  title: string
  body: string
  timestamp: number
  read: boolean
}

const notifId = ref(0)
const MAX_NOTIFICATIONS = 100

export const notifications = ref<NotificationEntry[]>([])

export function addNotification(title: string, body: string) {
  notifications.value = [
    { id: ++notifId.value, title, body, timestamp: Date.now(), read: false },
    ...notifications.value,
  ].slice(0, MAX_NOTIFICATIONS)
}

export function markNotificationRead(id: number) {
  const entry = notifications.value.find((n) => n.id === id)
  if (entry) entry.read = true
}

export function markAllNotificationsRead() {
  for (const n of notifications.value) {
    n.read = true
  }
}

export function clearNotifications() {
  notifications.value = []
}

export function unreadNotificationCount() {
  return notifications.value.filter((n) => !n.read).length
}

// ---------------------------------------------------------------------------
// Persistent banner notifications (inverter/notifications MQTT topic + Victron alarms)
// ---------------------------------------------------------------------------

export interface BannerNotification {
  id: string
  level: 'info' | 'warning' | 'alarm'
  title: string
  body: string
  source?: string
  ts?: string
}

export const bannerNotifications = ref<BannerNotification[]>([])

const DISMISSED_KEY = 'dismissed_banner_ids'
const MAX_DISMISSED = 200

function loadDismissedIds(): Set<string> {
  try {
    const raw = localStorage.getItem(DISMISSED_KEY)
    const arr: unknown = raw ? JSON.parse(raw) : []
    return Array.isArray(arr)
      ? new Set(arr.filter((x): x is string => typeof x === 'string'))
      : new Set()
  } catch {
    return new Set()
  }
}

const dismissedIds = loadDismissedIds()

function saveDismissedIds() {
  const arr = [...dismissedIds].slice(-MAX_DISMISSED)
  dismissedIds.clear()
  for (const id of arr) dismissedIds.add(id)
  localStorage.setItem(DISMISSED_KEY, JSON.stringify(arr))
}

export function isBannerDismissed(id: string): boolean {
  return dismissedIds.has(id)
}

/**
 * User dismissed the banner.
 * Victron platform notifications are acknowledged on Cerbo via MQTT
 * (AcknowledgeAll + per-slot Silenced/Acknowledged, GUIv2 ecosystem).
 * Local dismissedIds are not used for platform ids — Rust holds user_dismissed
 * until Active clears. Other sources stay locally dismissed until a fresh id.
 */
export async function dismissBanner(id: string): Promise<void> {
  clearBanner(id)
  if (id.startsWith('victron-platform-')) {
    try {
      await invoke('acknowledge_victron_banner', { id })
    } catch (e) {
      console.warn('Failed to acknowledge Victron alarm on Cerbo:', e)
    }
    return
  }
  dismissedIds.add(id)
  saveDismissedIds()
}

/** Add or replace by id (dedupe for hourly re-publishes). */
export function upsertBanner(notification: BannerNotification) {
  if (dismissedIds.has(notification.id)) return
  const idx = bannerNotifications.value.findIndex((b) => b.id === notification.id)
  if (idx >= 0) {
    bannerNotifications.value[idx] = notification
  } else {
    bannerNotifications.value = [...bannerNotifications.value, notification]
  }
}

/** Alarm resolved (value back to 0) — remove without recording a dismissal. */
export function clearBanner(id: string) {
  bannerNotifications.value = bannerNotifications.value.filter((b) => b.id !== id)
}
