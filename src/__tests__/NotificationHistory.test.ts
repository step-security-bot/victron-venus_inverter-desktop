import { enableAutoUnmount, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createI18n } from 'vue-i18n'
import NotificationHistory from '../components/NotificationHistory.vue'
import { addNotification, notifications } from '../composables/useInverterState'
import en from '../i18n/en'

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }))
enableAutoUnmount(afterEach)
beforeEach(() => {
  notifications.value = []
})

describe('notification history event time', () => {
  it('shows source time, a full date tooltip, and an honest unknown time', async () => {
    const eventTime = Date.parse('2026-10-05T18:47:00Z')
    addNotification('Native alarm', 'Battery', eventTime)
    addNotification('Partial alarm', 'Battery', null)
    const wrapper = mount(NotificationHistory, {
      global: { plugins: [createI18n({ legacy: false, locale: 'en', messages: { en } })] },
    })
    await wrapper.get('button').trigger('click')
    const panel = wrapper.get('.notification-history-panel')
    expect(panel.text()).toContain('Event time unavailable')
    const date = new Date(eventTime)
    expect(panel.text()).toContain(
      `${String(date.getHours()).padStart(2, '0')}:${String(date.getMinutes()).padStart(2, '0')}`
    )
    expect(panel.findAll('[title]').map((node) => node.attributes('title'))).toContain(
      date.toLocaleString(undefined, { timeZoneName: 'short' })
    )
  })
})
