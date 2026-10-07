import { describe, expect, it } from 'vitest'
import { ESS_MODE_COMMAND_FRESH_WITHIN_S, isEssModeCommandFresh, selectedEssMode } from '../essMode'

describe('isEssModeCommandFresh', () => {
  const mode = { selection_supported: true, selected: 'external_control' as const }
  const now = 1_700_000_000_000

  it('rejects missing mode or observation', () => {
    expect(isEssModeCommandFresh(undefined, 1, now)).toBe(false)
    expect(isEssModeCommandFresh(mode, null, now)).toBe(false)
    expect(isEssModeCommandFresh(mode, undefined, now)).toBe(false)
  })

  it('accepts live observation within the Rust command window', () => {
    const at = now / 1000 - ESS_MODE_COMMAND_FRESH_WITHIN_S
    expect(isEssModeCommandFresh(mode, at, now)).toBe(true)
    expect(isEssModeCommandFresh(mode, now / 1000, now)).toBe(true)
  })

  it('rejects retained-style absence and stale or future stamps', () => {
    expect(
      isEssModeCommandFresh(mode, now / 1000 - ESS_MODE_COMMAND_FRESH_WITHIN_S - 0.001, now)
    ).toBe(false)
    expect(isEssModeCommandFresh(mode, now / 1000 + 1, now)).toBe(false)
  })
})

describe('selectedEssMode legacy mode_name aliases', () => {
  it.each(['constructor', '__proto__'])('rejects inherited object property %s', (mode_name) => {
    expect(selectedEssMode({ mode_name })).toBeUndefined()
  })

  it('maps inverter-control Optimized mode_name strings', () => {
    expect(selectedEssMode({ mode_name: 'Optimized (BatteryLife)' })).toBe(
      'optimized_with_battery_life'
    )
    expect(selectedEssMode({ mode_name: 'Optimized without BatteryLife' })).toBe(
      'optimized_without_battery_life'
    )
  })

  it('still matches desktop labels and selection_supported selected id', () => {
    expect(selectedEssMode({ mode_name: 'Optimized with battery life' })).toBe(
      'optimized_with_battery_life'
    )
    expect(selectedEssMode({ selection_supported: true, selected: 'keep_batteries_charged' })).toBe(
      'keep_batteries_charged'
    )
  })
})
