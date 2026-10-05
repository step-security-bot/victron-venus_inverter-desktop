import { invoke } from '@tauri-apps/api/core'
import { logger } from '../logger'
import { applyInverterState, type InverterState } from './useInverterState'
import { acceptsCurrentTransportEvent, getCurrentTransportSession } from './transportSession'

/** Resume core telemetry independently of optional home-device integrations. */
export function useInverterVisibility() {
  let revision = 0

  async function setInverterWindowHidden(hidden: boolean) {
    const current = ++revision
    const notificationSession = getCurrentTransportSession()
    const isCurrent = () =>
      current === revision &&
      acceptsCurrentTransportEvent({ notification_session: notificationSession ?? undefined })
    try {
      await invoke('set_window_hidden', { hidden })
      if (hidden || !isCurrent()) return
      const initial = await invoke<InverterState & { notification_session?: string }>('get_state')
      if (initial && isCurrent() && acceptsCurrentTransportEvent(initial))
        applyInverterState(initial, { snapshot: true })
    } catch (error) {
      logger.error('Failed to sync inverter window state:', error)
    }
  }

  function cleanupInverterVisibility() {
    revision += 1
  }

  return { setInverterWindowHidden, cleanupInverterVisibility }
}
