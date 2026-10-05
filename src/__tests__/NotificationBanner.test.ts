import { enableAutoUnmount, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { nextTick } from 'vue'
import NotificationBanner from '../components/NotificationBanner.vue'
import { bannerNotifications, upsertBanner } from '../composables/useInverterState'

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }))
enableAutoUnmount(afterEach)

const now = new Date('2026-10-05T20:02:00Z')
const eventTime = '2026-10-05T11:47:00-07:00'
const alarm = (ts = eventTime) => ({
  id: 'victron-platform-0-1',
  level: 'alarm' as const,
  title: 'Internal failure',
  body: 'JBD Battery Chain 1',
  source: 'victron',
  ts,
})
const render = () => mount(NotificationBanner, { global: { mocks: { $t: (key: string) => key } } })

beforeEach(() => {
  vi.useFakeTimers()
  vi.setSystemTime(now)
  bannerNotifications.value = []
})
afterEach(() => vi.useRealTimers())

describe('notification event time', () => {
  it('shows the source event age on receipt, including timezone offsets and exact time', () => {
    upsertBanner(alarm())
    const wrapper = render()
    expect(wrapper.text()).toContain('1h 15m ago')
    expect(wrapper.get('time').attributes('datetime')).toBe(eventTime)
    expect(wrapper.get('time').attributes('title')).toBe(
      new Date(eventTime).toLocaleString(undefined, { timeZoneName: 'short' })
    )
  })

  it('ages a banner without any subsequent MQTT messages', async () => {
    upsertBanner(alarm(now.toISOString()))
    const wrapper = render()
    expect(wrapper.text()).toContain('just now')
    await vi.advanceTimersByTimeAsync(75 * 60_000)
    expect(wrapper.text()).toContain('1h 15m ago')
    expect(wrapper.text()).not.toContain('just now')
  })

  it('refreshes after background throttling and cleans up its timer and listeners', async () => {
    upsertBanner(alarm(now.toISOString()))
    const baselineTimers = vi.getTimerCount()
    const wrapper = render()
    vi.setSystemTime(new Date(now.getTime() + 75 * 60_000))
    document.dispatchEvent(new Event('visibilitychange'))
    await nextTick()
    expect(wrapper.text()).toContain('1h 15m ago')
    vi.setSystemTime(new Date(now.getTime() + 76 * 60_000))
    window.dispatchEvent(new Event('focus'))
    await nextTick()
    expect(wrapper.text()).toContain('1h 16m ago')
    const removeDocument = vi.spyOn(document, 'removeEventListener')
    const removeWindow = vi.spyOn(window, 'removeEventListener')
    wrapper.unmount()
    expect(vi.getTimerCount()).toBe(baselineTimers)
    expect(removeDocument).toHaveBeenCalledWith('visibilitychange', expect.any(Function))
    expect(removeWindow).toHaveBeenCalledWith('focus', expect.any(Function))
    removeDocument.mockRestore()
    removeWindow.mockRestore()
  })

  it.each(['', 'not-a-date', '1970-01-01T00:00:00Z'])(
    'keeps an alarm visible without inventing a time for %j, then accepts its DateTime',
    async (ts) => {
      upsertBanner(alarm(ts))
      const wrapper = render()
      expect(wrapper.text()).toContain('Internal failure')
      expect(wrapper.text()).toContain('notifications.timeUnavailable')
      expect(wrapper.text()).not.toContain('just now')
      expect(wrapper.find('time').exists()).toBe(false)
      upsertBanner(alarm())
      await nextTick()
      expect(wrapper.text()).toContain('1h 15m ago')
      expect(wrapper.text()).not.toContain('notifications.timeUnavailable')
    }
  )

  it('does not describe a future source time as just now', () => {
    upsertBanner(alarm(new Date(now.getTime() + 60_000).toISOString()))
    const wrapper = render()
    expect(wrapper.text()).not.toContain('just now')
    expect(wrapper.get('time').text()).toBeTruthy()
  })
})
