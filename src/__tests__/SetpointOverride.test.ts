import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import SetpointOverride from '../components/SetpointOverride.vue'

const boundary = vi.hoisted(() => ({ invoke: vi.fn(), listen: vi.fn(), session: 'current' }))
vi.mock('@tauri-apps/api/core', () => ({ invoke: boundary.invoke }))
vi.mock('@tauri-apps/api/event', () => ({ listen: boundary.listen }))
vi.mock('../composables/transportSession', () => ({
  acceptsCurrentTransportEvent: (payload: { notification_session?: string }) =>
    payload?.notification_session === boundary.session,
}))

type Status = { value: number | null; last_error: string | null }
let onStatus: (event: { payload: Status | null }) => void
let onConnection: (event: {
  payload: { connected: boolean; notification_session?: string }
}) => void
let wrapper: VueWrapper | undefined
const inactive = { value: null, last_error: null }
const active = { value: 100, last_error: null }

beforeEach(() => {
  boundary.session = 'current'
  boundary.invoke.mockReset().mockResolvedValue(inactive)
  boundary.listen.mockReset().mockImplementation(async (name, callback) => {
    if (name === 'setpoint-override-update') onStatus = callback
    if (name === 'mqtt-connection-status') onConnection = callback
    return vi.fn()
  })
})
afterEach(() => {
  wrapper?.unmount()
  wrapper = undefined
  document.body.innerHTML = ''
})
async function render() {
  wrapper = mount(SetpointOverride, { props: { currentSetpoint: 25 }, attachTo: document.body })
  await flushPromises()
  return wrapper
}
async function open() {
  await wrapper!.get('button[aria-label="Setpoint override"]').trigger('click')
  await flushPromises()
}
const trigger = () => wrapper!.get('button[aria-label="Setpoint override"]')
const dialog = () => document.querySelector('dialog')!

describe('Setpoint Override transport status', () => {
  it('keeps failed initial status unknown and disallows submission', async () => {
    boundary.invoke.mockRejectedValue(new Error('Gateway status unavailable'))
    await render()
    expect(trigger().attributes('aria-pressed')).toBeUndefined()
    expect(wrapper!.text()).toContain('Status unknown')
    await open()
    expect(dialog().querySelector<HTMLButtonElement>('button[type="submit"]')!.disabled).toBe(true)
    expect(boundary.invoke).toHaveBeenCalledTimes(1)
  })

  it('distinguishes lost status from an acknowledged inactive override and recovers on events', async () => {
    boundary.invoke.mockResolvedValue(active)
    await render()
    expect(trigger().attributes('aria-pressed')).toBe('true')
    expect(wrapper!.text()).toContain('100 W')
    onStatus({ payload: null })
    await flushPromises()
    expect(trigger().attributes('aria-pressed')).toBeUndefined()
    expect(wrapper!.text()).toContain('Status unknown')
    expect(wrapper!.text()).not.toContain('100 W')
    await open()
    expect(dialog().textContent).not.toContain('Stop override')
    expect(dialog().querySelector<HTMLButtonElement>('button[type="submit"]')!.disabled).toBe(true)
    onStatus({ payload: inactive })
    await flushPromises()
    expect(trigger().attributes('aria-pressed')).toBe('false')
    expect(wrapper!.text()).not.toContain('Status unknown')
    expect(dialog().querySelector<HTMLButtonElement>('button[type="submit"]')!.disabled).toBe(false)
  })

  it('does not let a delayed cached getter erase a newer unavailable event', async () => {
    let resolve!: (status: Status) => void
    boundary.invoke.mockReturnValue(
      new Promise<Status>((done) => {
        resolve = done
      })
    )
    await render()
    onStatus({ payload: null })
    resolve(active)
    await flushPromises()
    expect(trigger().attributes('aria-pressed')).toBeUndefined()
    expect(wrapper!.text()).toContain('Status unknown')
  })

  it('does not let a delayed getter failure erase a newer live event', async () => {
    let reject!: (error: Error) => void
    boundary.invoke.mockReturnValue(
      new Promise<Status>((_done, fail) => {
        reject = fail
      })
    )
    await render()
    onStatus({ payload: active })
    reject(new Error('Earlier request failed'))
    await flushPromises()
    expect(trigger().attributes('aria-pressed')).toBe('true')
    expect(wrapper!.text()).toContain('100 W')
    expect(wrapper!.text()).not.toContain('Earlier request failed')
  })

  it('keeps the dialog open after missing acknowledgement and sends only once', async () => {
    boundary.invoke.mockImplementation(async (command) => {
      if (command === 'set_setpoint_override')
        throw new Error('Cerbo has not confirmed the override')
      return inactive
    })
    await render()
    await open()
    const input = dialog().querySelector<HTMLInputElement>('input')!
    input.value = '-250'
    input.dispatchEvent(new Event('input', { bubbles: true }))
    dialog()
      .querySelector('form')!
      .dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
    await flushPromises()
    expect(dialog().textContent).toContain('Cerbo has not confirmed')
    expect(
      boundary.invoke.mock.calls.filter(([command]) => command === 'set_setpoint_override')
    ).toEqual([['set_setpoint_override', { value: -250 }]])
  })

  it('reconciles acknowledged stop with fresh status instead of optimistic state', async () => {
    let current: Status = active
    boundary.invoke.mockImplementation(async (command, args) => {
      if (command === 'set_setpoint_override') current = { value: args.value, last_error: null }
      return current
    })
    await render()
    await open()
    const stop = [...dialog().querySelectorAll('button')].find((button) =>
      button.textContent?.includes('Stop override')
    )!
    stop.click()
    await flushPromises()
    expect(boundary.invoke).toHaveBeenCalledWith('set_setpoint_override', { value: null })
    expect(
      boundary.invoke.mock.calls.filter(([command]) => command === 'get_setpoint_override')
    ).toHaveLength(2)
    expect(trigger().attributes('aria-pressed')).toBe('false')
    expect(document.querySelector('dialog')).toBeNull()
  })
  it('clears an active status on transport loss until a fresh status arrives', async () => {
    boundary.invoke.mockResolvedValue(active)
    await render()
    onConnection({ payload: { connected: false, notification_session: 'current' } })
    await flushPromises()
    expect(trigger().attributes('aria-pressed')).toBeUndefined()
    expect(wrapper!.text()).toContain('Status unknown')
    onConnection({ payload: { connected: true, notification_session: 'current' } })
    await flushPromises()
    expect(trigger().attributes('aria-pressed')).toBeUndefined()
    onStatus({ payload: inactive })
    await flushPromises()
    expect(trigger().attributes('aria-pressed')).toBe('false')
  })

  it('does not restore old command status after a newer source-loss event', async () => {
    let resolveCommand!: (status: Status) => void
    let resolveRefresh!: (status: Status) => void
    let gets = 0
    boundary.invoke.mockImplementation((command) => {
      if (command === 'set_setpoint_override') {
        return new Promise<Status>((done) => {
          resolveCommand = done
        })
      }
      if (++gets === 1) return Promise.resolve(active)
      return new Promise<Status>((done) => {
        resolveRefresh = done
      })
    })
    await render()
    await open()
    dialog()
      .querySelector('form')!
      .dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
    await flushPromises()
    onConnection({ payload: { connected: false, notification_session: 'current' } })
    resolveCommand(active)
    await flushPromises()
    expect(trigger().attributes('aria-pressed')).toBeUndefined()
    expect(wrapper!.text()).toContain('Status unknown')
    // A genuinely new status event still beats the subsequent getter.
    onStatus({ payload: inactive })
    resolveRefresh(active)
    await flushPromises()
    expect(trigger().attributes('aria-pressed')).toBe('false')
  })

  it('ignores delayed connection loss from a replaced transport and untagged status', async () => {
    boundary.invoke.mockResolvedValue(active)
    await render()
    onConnection({ payload: { connected: false, notification_session: 'previous' } })
    onConnection({ payload: { connected: false } })
    await flushPromises()
    expect(trigger().attributes('aria-pressed')).toBe('true')
    expect(wrapper!.text()).toContain('100 W')
    onConnection({ payload: { connected: false, notification_session: 'current' } })
    await flushPromises()
    expect(wrapper!.text()).toContain('Status unknown')
  })
})
