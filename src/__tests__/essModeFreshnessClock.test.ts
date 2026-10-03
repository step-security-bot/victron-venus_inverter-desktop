import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { isEssModeCommandFresh, ESS_MODE_COMMAND_FRESH_WITHIN_S } from '../essMode'
import {
  applyInverterState,
  refreshTelemetryQuality,
  resetInverterState,
  state,
  telemetryClockMs,
} from '../composables/useInverterState'

describe('ESS command freshness with reactive clock', () => {
  beforeEach(() => {
    vi.useFakeTimers()
    vi.setSystemTime(new Date('2026-01-01T00:00:00.000Z'))
    resetInverterState()
    refreshTelemetryQuality(Date.now())
  })

  afterEach(() => {
    vi.useRealTimers()
    resetInverterState()
  })

  it('retained → live → expired without further payloads', () => {
    // Retained: observed_at cleared / absent → not command-fresh.
    applyInverterState(
      {
        ess_mode: {
          selected: 'external_control',
          selection_supported: true,
          mode_name: 'External control',
        },
        ess_mode_observed_at: null,
      } as never,
      { observedAt: Date.now(), source: 'mqtt' }
    )
    expect(
      isEssModeCommandFresh(
        state.value.ess_mode,
        state.value.ess_mode_observed_at,
        telemetryClockMs.value
      )
    ).toBe(false)

    // Live observation stamp (seconds).
    const liveAtS = Date.now() / 1000
    applyInverterState(
      {
        ess_mode: {
          selected: 'external_control',
          selection_supported: true,
          mode_name: 'External control',
        },
        ess_mode_observed_at: liveAtS,
      } as never,
      { observedAt: Date.now(), source: 'mqtt' }
    )
    expect(
      isEssModeCommandFresh(
        state.value.ess_mode,
        state.value.ess_mode_observed_at,
        telemetryClockMs.value
      )
    ).toBe(true)

    // No subsequent payloads: only the 1 Hz freshness tick advances the clock.
    // Before the Rust 30s boundary, still fresh.
    vi.advanceTimersByTime(ESS_MODE_COMMAND_FRESH_WITHIN_S * 1000 - 500)
    refreshTelemetryQuality(Date.now())
    expect(
      isEssModeCommandFresh(
        state.value.ess_mode,
        state.value.ess_mode_observed_at,
        telemetryClockMs.value
      )
    ).toBe(true)

    // Cross the 30s boundary without a new state payload → expired.
    vi.advanceTimersByTime(1000)
    refreshTelemetryQuality(Date.now())
    expect(
      isEssModeCommandFresh(
        state.value.ess_mode,
        state.value.ess_mode_observed_at,
        telemetryClockMs.value
      )
    ).toBe(false)

    // telemetry quality may be unchanged; clock must still have moved.
    expect(telemetryClockMs.value).toBe(Date.now())
  })
})
