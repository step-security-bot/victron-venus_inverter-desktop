import { flushPromises } from '@vue/test-utils'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { resetInverterState, state } from '../composables/useInverterState'
import { useInverterVisibility } from '../composables/useInverterVisibility'

const boundary = vi.hoisted(() => ({ invoke: vi.fn(), session: 'current' as string | null }))
vi.mock('@tauri-apps/api/core', () => ({ invoke: boundary.invoke }))
vi.mock('../composables/transportSession', () => ({
  getCurrentTransportSession: () => boundary.session,
  acceptsCurrentTransportEvent: (payload: { notification_session?: string }) =>
    boundary.session !== null && payload?.notification_session === boundary.session,
}))

beforeEach(() => {
  resetInverterState()
  boundary.session = 'current'
  boundary.invoke.mockReset()
})

describe('core telemetry window lifecycle', () => {
  it('restores inverter telemetry without any HA calls', async () => {
    boundary.invoke.mockImplementation(async (command: string) =>
      command === 'get_state'
        ? { battery_soc: 76, notification_session: boundary.session }
        : undefined
    )
    const core = useInverterVisibility()
    await core.setInverterWindowHidden(false)
    expect(state.value.battery_soc).toBe(76)
    expect(boundary.invoke.mock.calls.map(([command]) => command)).toEqual([
      'set_window_hidden',
      'get_state',
    ])
  })

  it.each(['hidden', 'unmounted'])(
    'discards a late snapshot after the window is %s',
    async (reason) => {
      let resolve!: (value: unknown) => void
      const snapshot = new Promise((done) => {
        resolve = done
      })
      boundary.invoke.mockImplementation(async (command: string) =>
        command === 'get_state' ? snapshot : undefined
      )
      const core = useInverterVisibility()
      const pending = core.setInverterWindowHidden(false)
      await flushPromises()
      if (reason === 'hidden') await core.setInverterWindowHidden(true)
      else core.cleanupInverterVisibility()
      resolve({ battery_soc: 99, notification_session: 'current' })
      await pending
      expect(state.value.battery_soc).not.toBe(99)
    }
  )

  it.each(['replaced', 'disabled'])(
    'discards a pending foreground snapshot when %s',
    async (reason) => {
      let resolve!: (value: unknown) => void
      boundary.invoke.mockImplementation(async (command: string) =>
        command === 'get_state'
          ? new Promise((done) => {
              resolve = done
            })
          : undefined
      )
      const core = useInverterVisibility()
      const pending = core.setInverterWindowHidden(false)
      await flushPromises()
      boundary.session = reason === 'replaced' ? 'new-connection' : null
      resolve({ battery_soc: 99, notification_session: 'current' })
      await pending
      expect(state.value.battery_soc).not.toBe(99)
    }
  )

  it.each(['previous', undefined])(
    'rejects a snapshot from %s during a pending replacement',
    async (session) => {
      boundary.invoke.mockImplementation(async (command: string) =>
        command === 'get_state' ? { battery_soc: 99, notification_session: session } : undefined
      )
      await useInverterVisibility().setInverterWindowHidden(false)
      expect(state.value.battery_soc).not.toBe(99)
    }
  )

  it('does not request a foreground snapshot without an active connection', async () => {
    boundary.session = null
    boundary.invoke.mockResolvedValue(undefined)
    await useInverterVisibility().setInverterWindowHidden(false)
    expect(boundary.invoke).toHaveBeenCalledExactlyOnceWith('set_window_hidden', { hidden: false })
  })
})
