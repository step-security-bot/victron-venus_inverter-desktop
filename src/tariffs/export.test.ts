import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { newDraft, rateGrid, validatePlan } from './model'
import { exportTariff } from './export'

const platform = vi.hoisted(() => ({ native: true, mobile: false, invoke: vi.fn() }))
vi.mock('@tauri-apps/api/core', () => ({
  isTauri: () => platform.native,
  invoke: platform.invoke,
}))
vi.mock('@features', () => ({
  get isMobileApp() {
    return platform.mobile
  },
}))

const plan = () => validatePlan({ ...newDraft(), rates: rateGrid(0.31) })
beforeEach(() => {
  vi.useFakeTimers()
  platform.native = true
  platform.mobile = false
  platform.invoke.mockReset().mockResolvedValue(true)
  vi.stubGlobal('URL', { createObjectURL: vi.fn(() => 'blob:test'), revokeObjectURL: vi.fn() })
})
afterEach(() => {
  vi.useRealTimers()
  vi.unstubAllGlobals()
  vi.restoreAllMocks()
})

it('uses only the native document command on desktop, including cancellation and failure', async () => {
  const draft = plan()
  expect(await exportTariff(draft)).toBe(true)
  expect(platform.invoke).toHaveBeenCalledWith('export_tariff', { plan: draft })
  platform.invoke.mockResolvedValueOnce(false)
  expect(await exportTariff(draft)).toBe(false)
  platform.invoke.mockRejectedValueOnce('Write failed')
  await expect(exportTariff(draft)).rejects.toThrow('Write failed')
  expect(URL.createObjectURL).not.toHaveBeenCalled()
})

it.each([
  { native: false, mobile: false },
  { native: true, mobile: true },
])('retains browser download behavior outside native desktop: %j', async ({ native, mobile }) => {
  platform.native = native
  platform.mobile = mobile
  const click = vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(() => {})
  expect(await exportTariff(plan())).toBe(true)
  expect(platform.invoke).not.toHaveBeenCalled()
  expect(click).toHaveBeenCalledOnce()
  expect(click.mock.instances[0]).toHaveProperty('download', 'electricity-tariff.json')
  await vi.runAllTimersAsync()
  expect(URL.revokeObjectURL).toHaveBeenCalledWith('blob:test')
})
