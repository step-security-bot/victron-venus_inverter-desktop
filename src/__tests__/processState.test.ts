import { beforeEach, describe, expect, it } from 'vitest'
import { applyInverterState, state } from '../composables/useInverterState'

describe('applyInverterState merge', () => {
  beforeEach(() => {
    state.value = { booleans: {}, features: {}, ui_config: {} }
  })

  it('replaces backup status so null power never retains an old sample', () => {
    const backup = {
      enabled: true,
      available: true,
      service: 'com.victronenergy.acload.example',
      device_instance: 78,
      name: 'Home',
      power: -750,
      measurement_time: 1000,
      age_seconds: 1,
    }
    applyInverterState({ grid_backup: backup, grid_using_backup: true })
    applyInverterState({
      grid_backup: { ...backup, available: false, power: null },
      grid_using_backup: false,
    })
    expect(state.value.grid_backup?.power).toBeNull()
    expect(state.value.grid_using_backup).toBe(false)
    applyInverterState({ grid_backup: { ...backup, service: null, power: null, available: false } })
    expect(state.value.grid_backup?.service).toBeNull()
    applyInverterState({ grid_backup: backup, grid_using_backup: true })
    applyInverterState({ grid_backup: null })
    expect(state.value.grid_backup).toBeUndefined()
    expect(state.value.grid_using_backup).toBe(false)
  })

  it('JSON null does not overwrite an existing number', () => {
    applyInverterState({ car_soc: 66, car_charging_power: 3200 })
    applyInverterState({ car_soc: null as unknown as number, car_charging_power: 3300 })
    expect(state.value.car_soc).toBe(66)
    expect(state.value.car_charging_power).toBe(3300)
  })

  it('JSON null does not clear the EV section fields', () => {
    applyInverterState({ car_soc: 66, ev_charging_power: 7400 })
    applyInverterState({
      car_soc: null as unknown as number,
      ev_charging_power: null as unknown as number,
      ev_present: false,
    })
    expect(state.value.car_soc).toBe(66)
    expect(state.value.ev_charging_power).toBe(7400)
  })

  it('undefined is treated as missing — keep previous', () => {
    applyInverterState({ car_soc: 66 })
    applyInverterState({ car_soc: undefined })
    expect(state.value.car_soc).toBe(66)
  })

  it('zero is a legitimate value for power (not skipped)', () => {
    applyInverterState({ car_charging_power: 3200 })
    applyInverterState({ car_charging_power: 0 })
    expect(state.value.car_charging_power).toBe(0)
  })

  it('replaces state object identity so shallowRef consumers re-render', () => {
    applyInverterState({ gt: 100 })
    const first = state.value
    applyInverterState({ gt: 200 })
    expect(state.value).not.toBe(first)
    expect(state.value.gt).toBe(200)
  })

  it('clears ess_mode_observed_at on retained null so the menu cannot stay writable', () => {
    applyInverterState({
      ess_mode: { selection_supported: true, selected: 'external_control' },
      ess_mode_observed_at: 1700000000,
    })
    expect(state.value.ess_mode_observed_at).toBe(1700000000)
    applyInverterState({
      ess_mode: { selection_supported: true, selected: 'external_control' },
      ess_mode_observed_at: null,
    })
    expect(state.value.ess_mode_observed_at).toBeUndefined()
  })
})
