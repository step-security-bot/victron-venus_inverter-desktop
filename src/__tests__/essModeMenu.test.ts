import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import EssModeMenu from '../components/EssModeMenu.vue'
import { ESS_MODES } from '../essMode'

const { send } = vi.hoisted(() => ({ send: vi.fn() }))
vi.mock('../composables/useDashboardControls', () => ({ sendControlAction: send }))
let wrapper: VueWrapper
const status = { selected: 'external_control', selection_supported: true, vebus_mode: 3 } as const
const menuItems = () =>
  Array.from(document.querySelectorAll<HTMLButtonElement>('[role="menuitemradio"]'))
async function open(overrides: Record<string, unknown> = {}) {
  wrapper = mount(EssModeMenu, {
    attachTo: document.body,
    props: {
      mode: status,
      label: 'External',
      active: true,
      dryRun: false,
      connected: true,
      fresh: true,
      ...overrides,
    },
  })
  await wrapper.get('button').trigger('click')
}
beforeEach(() => {
  send.mockReset()
  send.mockResolvedValue(undefined)
})
afterEach(() => {
  wrapper?.unmount()
  document.body.innerHTML = ''
  vi.useRealTimers()
})

describe('ESS menu', () => {
  it('opens six choices with External selected without dispatching', async () => {
    await open()
    expect(menuItems().map((item) => item.textContent?.trim())).toEqual(
      ESS_MODES.map((mode) => mode.label)
    )
    expect(menuItems()[5].getAttribute('aria-checked')).toBe('true')
    expect(document.activeElement).toBe(menuItems()[5])
    expect(send).not.toHaveBeenCalled()
  })
  it.each(ESS_MODES)('highlights the observed $id mode', async ({ id }) => {
    await open({ mode: { ...status, selected: id } })
    expect(menuItems().filter((item) => item.getAttribute('aria-checked') === 'true')).toEqual([
      menuItems()[ESS_MODES.findIndex((mode) => mode.id === id)],
    ])
    await wrapper.setProps({ mode: { ...status, selected: null } })
    expect(menuItems().some((item) => item.getAttribute('aria-checked') === 'true')).toBe(false)
  })
  it.each(ESS_MODES)(
    'dispatches only explicit $id once and waits for controller status',
    async ({ id }) => {
      await open({ mode: { ...status, selected: null } })
      menuItems()[ESS_MODES.findIndex((mode) => mode.id === id)].click()
      await flushPromises()
      const payload = send.mock.calls[0][1]
      expect(send).toHaveBeenCalledExactlyOnceWith('set_ess_mode', {
        mode: id,
        request_id: expect.any(String),
      })
      expect(menuItems().some((item) => item.getAttribute('aria-checked') === 'true')).toBe(false)
      menuItems()[1].click()
      expect(send).toHaveBeenCalledTimes(1)
      await wrapper.setProps({ mode: { ...status, selected: id, request_id: payload.request_id } })
      expect(
        menuItems()[ESS_MODES.findIndex((mode) => mode.id === id)].getAttribute('aria-checked')
      ).toBe('true')
      expect(wrapper.get('button').attributes('aria-busy')).toBeUndefined()
    }
  )
  it('does not repeat the already selected mode', async () => {
    await open()
    menuItems()[5].click()
    await flushPromises()
    expect(send).not.toHaveBeenCalled()
    expect(menuItems()).toHaveLength(0)
  })
  it.each([
    { dryRun: true },
    { connected: false },
    { fresh: false },
    { mode: { is_external: true } },
  ])('prevents writes when unavailable: %j', async (overrides) => {
    await open(overrides)
    menuItems()[0].click()
    expect(send).not.toHaveBeenCalled()
    expect(document.querySelector('[role="status"]')).not.toBeNull()
  })
  it('updates stale or DRY state while open before a selection', async () => {
    await open()
    await wrapper.setProps({ fresh: false })
    menuItems()[0].click()
    expect(send).not.toHaveBeenCalled()
  })
  it('keeps observed selection after dispatch failure, without retry', async () => {
    send.mockRejectedValue(new Error('gateway unavailable'))
    await open()
    menuItems()[0].click()
    await flushPromises()
    expect(menuItems()[5].getAttribute('aria-checked')).toBe('true')
    expect(document.querySelector('[role="alert"]')?.textContent).toContain('gateway unavailable')
    expect(send).toHaveBeenCalledTimes(1)
  })
  it('handles matching errors and ignores another request receipt', async () => {
    await open()
    menuItems()[0].click()
    await flushPromises()
    const requestId = send.mock.calls[0][1].request_id
    await wrapper.setProps({ mode: { ...status, request_id: 'someone-else' } })
    expect(wrapper.get('button').attributes('aria-busy')).toBe('true')
    await wrapper.setProps({
      mode: { ...status, request_id: requestId, error: 'Switch not adjustable' },
    })
    expect(document.querySelector('[role="alert"]')?.textContent).toContain('Switch not adjustable')
    expect(menuItems()[5].getAttribute('aria-checked')).toBe('true')
  })
  it('times out without retry or optimistic success', async () => {
    vi.useFakeTimers()
    await open()
    menuItems()[0].click()
    await flushPromises()
    await vi.advanceTimersByTimeAsync(20_000)
    expect(document.querySelector('[role="alert"]')?.textContent).toContain('unconfirmed')
    expect(send).toHaveBeenCalledTimes(1)
    expect(menuItems()[5].getAttribute('aria-checked')).toBe('true')
  })
  it('supports arrows, Home, End, Escape and outside dismissal without commands', async () => {
    await open()
    for (const [key, index] of [
      ['Home', 0],
      ['ArrowDown', 1],
      ['End', 5],
      ['ArrowDown', 0],
      ['ArrowUp', 5],
    ] as const) {
      document.activeElement?.dispatchEvent(new KeyboardEvent('keydown', { key, bubbles: true }))
      expect(document.activeElement).toBe(menuItems()[index])
    }
    document.activeElement?.dispatchEvent(
      new KeyboardEvent('keydown', { key: 'Escape', bubbles: true })
    )
    await flushPromises()
    expect(menuItems()).toHaveLength(0)
    expect(document.activeElement).toBe(wrapper.get('button').element)
    await wrapper.get('button').trigger('click')
    document.body.dispatchEvent(new Event('pointerdown', { bubbles: true }))
    await flushPromises()
    expect(menuItems()).toHaveLength(0)
    expect(send).not.toHaveBeenCalled()
  })
})
