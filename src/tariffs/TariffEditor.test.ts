import { mount, flushPromises } from '@vue/test-utils'
import { defineComponent, h } from 'vue'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { newDraft, rateGrid, validatePlan, type RateGrid } from './model'
import { loadTariff } from './storage'
import TariffEditor from './TariffEditor.vue'

const sheetState = vi.hoisted(() => ({ edits: null as RateGrid | null, fail: false }))
const exportDocument = vi.hoisted(() => vi.fn())
vi.mock('./export', () => ({ exportTariff: exportDocument }))
vi.mock('./TariffSheet.vue', () => ({
  default: defineComponent({
    props: ['rates'],
    setup(props, { expose }) {
      expose({
        getRates: async () => {
          if (sheetState.fail) throw new Error('Finish editing the cell.')
          return sheetState.edits ?? props.rates
        },
      })
      return () => h('div', { 'data-testid': 'sheet' }, String(props.rates[0][0]))
    },
  }),
}))
beforeEach(() => {
  vi.useFakeTimers()
  vi.setSystemTime(new Date('2026-09-24T20:00:00Z'))
  HTMLDialogElement.prototype.showModal = vi.fn()
  HTMLDialogElement.prototype.close = vi.fn()
  sheetState.edits = null
  sheetState.fail = false
  exportDocument.mockReset().mockResolvedValue(true)
  const data = new Map<string, string>()
  vi.stubGlobal('localStorage', {
    getItem: (key: string) => data.get(key) ?? null,
    setItem: vi.fn((key: string, value: string) => data.set(key, value)),
  })
})
afterEach(() => {
  vi.useRealTimers()
  vi.unstubAllGlobals()
  vi.restoreAllMocks()
})
const tariff = () =>
  validatePlan({
    ...newDraft(),
    timeZone: 'America/Los_Angeles',
    billingDay: 17,
    rates: rateGrid(0.3),
    seasons: [{ name: 'Summer', months: [6, 7, 8, 9], rates: rateGrid(0.5) }],
  })

it('exports pending cells without saving the tariff and blocks duplicate actions until the dialog closes', async () => {
  let finish!: (saved: boolean) => void
  exportDocument.mockReturnValue(
    new Promise<boolean>((resolve) => {
      finish = resolve
    })
  )
  const savePlan = vi.fn()
  const wrapper = mount(TariffEditor, { props: { plan: tariff(), savePlan } })
  const cells = rateGrid(0.31)
  cells[0][0] = 0.1234
  sheetState.edits = cells
  const button = wrapper.findAll('button').find((b) => b.text() === 'Export tariff')!
  await button.trigger('click')
  await flushPromises()
  expect(exportDocument).toHaveBeenCalledTimes(1)
  expect(exportDocument.mock.calls[0][0].seasons[0].rates).toEqual(cells)
  expect(button.attributes('disabled')).toBeDefined()
  expect(wrapper.get('.tariff-save').attributes('disabled')).toBeDefined()
  expect(savePlan).not.toHaveBeenCalled()
  expect(localStorage.setItem).not.toHaveBeenCalled()
  expect(wrapper.emitted('saved')).toBeUndefined()
  finish(false)
  await flushPromises()
  expect(wrapper.text()).toContain('Export cancelled.')
  expect(button.attributes('disabled')).toBeUndefined()
  expect(savePlan).not.toHaveBeenCalled()
  wrapper.unmount()
})

it('keeps the draft editable after an export failure and does not export an uncommitted invalid cell', async () => {
  const wrapper = mount(TariffEditor, { props: { plan: tariff(), savePlan: vi.fn() } })
  const button = wrapper.findAll('button').find((b) => b.text() === 'Export tariff')!
  sheetState.fail = true
  await button.trigger('click')
  await flushPromises()
  expect(exportDocument).not.toHaveBeenCalled()
  sheetState.fail = false
  exportDocument.mockRejectedValueOnce(new Error('Disk is full'))
  await button.trigger('click')
  await flushPromises()
  expect(wrapper.get('[role="alert"]').text()).toContain('Disk is full')
  expect(button.attributes('disabled')).toBeUndefined()
  await button.trigger('click')
  await flushPromises()
  expect(wrapper.text()).toContain('Tariff exported.')
  expect(wrapper.find('[role="alert"]').exists()).toBe(false)
  expect(wrapper.emitted('saved')).toBeUndefined()
  wrapper.unmount()
})
it('commits pending cells before switching season and preserves both schedules on save', async () => {
  const wrapper = mount(TariffEditor, { props: { plan: tariff(), tariffScope: 'editor-site' } })
  expect(wrapper.get('select').element.value).toBe('0')
  sheetState.edits = rateGrid(0.6)
  await wrapper.get('select').setValue('-1')
  await flushPromises()
  expect(wrapper.get('[data-testid="sheet"]').text()).toBe('0.3')
  sheetState.edits = rateGrid(0.4)
  await wrapper.get('.tariff-save').trigger('click')
  await flushPromises()
  const saved = loadTariff('editor-site').plan
  expect(saved?.seasons[0].rates).toEqual(rateGrid(0.6))
  expect(saved?.rates).toEqual(rateGrid(0.4))
  expect(saved?.billingDay).toBe(17)
  expect(wrapper.emitted('saved')?.[0]).toEqual([saved])
  wrapper.unmount()
})
it('keeps the selected schedule when a cell cannot be committed', async () => {
  const wrapper = mount(TariffEditor, { props: { plan: tariff(), tariffScope: 'editor-site' } })
  sheetState.fail = true
  await wrapper.get('select').setValue('-1')
  await flushPromises()
  expect(wrapper.get('select').element.value).toBe('0')
  expect(wrapper.get('[role="alert"]').text()).toContain('Finish editing')
  expect(localStorage.setItem).not.toHaveBeenCalled()
  wrapper.unmount()
})
it('rejects a fractional billing day and allows clearing a previously configured date', async () => {
  const wrapper = mount(TariffEditor, { props: { plan: tariff(), tariffScope: 'editor-site' } })
  await wrapper.get('input[min="1"]').setValue('17.5')
  await wrapper.get('.tariff-save').trigger('click')
  await flushPromises()
  expect(wrapper.get('[role="alert"]').text()).toContain('whole number')
  expect(localStorage.setItem).not.toHaveBeenCalled()
  await wrapper.get('input[min="1"]').setValue('')
  await wrapper.get('.tariff-save').trigger('click')
  await flushPromises()
  expect(loadTariff('editor-site').plan?.billingDay).toBeUndefined()
  wrapper.unmount()
})

it('creates a manual season and applies a configuration draft without writing local storage', async () => {
  const base = validatePlan({
    ...newDraft(),
    rates: rateGrid(0.3),
    timeZone: 'America/Los_Angeles',
  })
  const wrapper = mount(TariffEditor, { props: { plan: base, persist: false } })
  const add = wrapper.findAll('button').find((button) => button.text() === 'Add season')!
  await add.trigger('click')
  await flushPromises()
  await wrapper.get('input[maxlength="80"]').setValue('Summer')
  await wrapper.get('input[type="checkbox"][value="6"]').setValue(true)
  sheetState.edits = rateGrid(0.5)
  await wrapper.get('.tariff-save').trigger('click')
  await flushPromises()
  expect(wrapper.emitted('saved')?.[0]?.[0]).toMatchObject({
    seasons: [{ name: 'Summer', months: [6], rates: rateGrid(0.5) }],
    rates: rateGrid(0.3),
  })
  expect(localStorage.setItem).not.toHaveBeenCalled()
  wrapper.unmount()
})

it.each([
  {
    persist: true,
    savePlan: undefined,
    caption: 'Saved for this dashboard on this device.',
    button: 'Save tariff',
  },
  {
    persist: false,
    savePlan: undefined,
    caption: 'Apply to the configuration draft, then save the configuration.',
    button: 'Apply tariff',
  },
  {
    persist: false,
    savePlan: async () => {},
    caption: 'Saved on the controller and shared by all connected dashboards.',
    button: 'Save to controller',
  },
])(
  'preserves legacy persistence captions when no destination is supplied: $button',
  ({ persist, savePlan, caption, button }) => {
    const wrapper = mount(TariffEditor, { props: { plan: tariff(), persist, savePlan } })
    expect(wrapper.text()).toContain(caption)
    expect(wrapper.get('.tariff-save').text()).toBe(button)
    wrapper.unmount()
  }
)
