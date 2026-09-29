import { enableAutoUnmount, mount } from '@vue/test-utils'
import { afterEach, describe, expect, it, vi } from 'vitest'
import DailyGridEnergy from '../components/DailyGridEnergy.vue'
import { dailyGridPresentation, GRID_ENERGY_STALE_AFTER_MS } from '../components/dailyGridEnergy'

const now = Date.parse('2026-09-29T18:30:00Z')
const seconds = (iso: string) => Date.parse(iso) / 1000
const reading = (overrides: Record<string, unknown> = {}) => ({
  date: '2026-09-29',
  time_zone: 'America/Los_Angeles',
  import_kwh: 12.34,
  export_kwh: 4.56,
  observed_at: now / 1000,
  started_at: seconds('2026-09-29T07:00:00Z'),
  complete: true,
  status: 'complete',
  source: { service: 'com.victronenergy.grid.meter', device_instance: 40, serial: 'fixture-meter' },
  ...overrides,
})

enableAutoUnmount(afterEach)
afterEach(() => vi.useRealTimers())

describe('daily grid energy coverage', () => {
  it('shows the measured import and export independently, including real zero', () => {
    const result = dailyGridPresentation(reading({ import_kwh: 0 }), now)
    expect(result).toMatchObject({ label: 'Today', imported: '0.00', exported: '4.56' })
    expect(result.details).toContain('2026-09-29 (America/Los_Angeles)')
    expect(result.details).toContain('instance 40, serial fixture-meter')
    expect(result.details).toContain('from the start of the site day')
  })

  it('labels a partial ledger with the site start time, not a full-day total', () => {
    const result = dailyGridPresentation(
      reading({ complete: false, status: 'partial', started_at: seconds('2026-09-29T17:23:00Z') }),
      now
    )
    expect(result).toMatchObject({ label: 'Since 10:23', imported: '12.34', exported: '4.56' })
    expect(result.details).toContain('excludes earlier energy today')
  })

  it.each([
    ['spring forward', '2026-03-08', '2026-03-08T08:00:00Z', '2026-03-09T06:59:59Z'],
    ['fall back', '2026-11-01', '2026-11-01T07:00:00Z', '2026-11-02T07:59:59Z'],
  ])(
    'accepts complete coverage across %s without assuming a 24-hour day',
    (_, date, start, end) => {
      const at = Date.parse(end)
      expect(
        dailyGridPresentation(
          reading({ date, started_at: seconds(start), observed_at: at / 1000 }),
          at
        )
      ).toMatchObject({ label: 'Today', imported: '12.34', exported: '4.56' })
    }
  )

  it('accepts the first instant of a site day when DST skips local midnight', () => {
    const at = Date.parse('2018-11-04T06:00:00Z')
    expect(
      dailyGridPresentation(
        reading({
          date: '2018-11-04',
          time_zone: 'America/Sao_Paulo',
          started_at: seconds('2018-11-04T03:00:00Z'),
          observed_at: at / 1000,
        }),
        at
      )
    ).toMatchObject({ imported: '12.34', exported: '4.56' })
  })

  it('uses the meter site day even when the UTC date is already tomorrow', () => {
    const at = Date.parse('2026-09-30T06:59:59Z')
    const value = reading({ observed_at: at / 1000 })
    expect(dailyGridPresentation(value, at).imported).toBe('12.34')
    expect(dailyGridPresentation(value, at + 1000)).toMatchObject({ imported: '—', exported: '—' })
    expect(dailyGridPresentation(value, at + 1000).details).toContain('current day at the meter')
  })

  it.each([
    ['complete without midnight coverage', { started_at: seconds('2026-09-29T07:00:01Z') }],
    ['fractional start after midnight', { started_at: seconds('2026-09-29T07:00:00Z') + 0.0005 }],
    [
      'partial start on a previous site day',
      { complete: false, status: 'partial', started_at: seconds('2026-09-29T06:59:59Z') },
    ],
    ['start after observation', { complete: false, status: 'partial', started_at: now / 1000 + 1 }],
    ['missing coverage flag', { complete: undefined }],
    ['missing start timestamp', { started_at: null }],
    ['missing observation timestamp', { observed_at: null }],
    ['invalid time zone', { time_zone: 'Not/A_Zone' }],
    ['offset instead of IANA time zone', { time_zone: '+08:00' }],
    ['impossible date', { date: '2026-09-31' }],
    ['missing direct source', { source: null }],
    [
      'invalid meter instance',
      { source: { service: 'com.victronenergy.grid.meter', device_instance: '40' } },
    ],
  ])('hides readings with %s', (_, override) => {
    expect(dailyGridPresentation(reading(override), now)).toMatchObject({
      imported: '—',
      exported: '—',
    })
  })
})

describe('daily grid energy validity and freshness', () => {
  it.each([null, undefined, -1, NaN, Infinity, '0'])(
    'does not coerce invalid import %s into zero',
    (value) => {
      expect(dailyGridPresentation(reading({ import_kwh: value }), now)).toMatchObject({
        imported: '—',
        exported: '4.56',
      })
      expect(dailyGridPresentation(reading({ export_kwh: value }), now)).toMatchObject({
        imported: '12.34',
        exported: '—',
      })
    }
  )

  it('keeps a 46-second-old reading live across the meter cadence and display delay', () => {
    expect(
      dailyGridPresentation(reading({ observed_at: (now - 46_000) / 1000 }), now)
    ).toMatchObject({ imported: '12.34', exported: '4.56' })
  })

  it('expires after 90 seconds and allows at most one second of clock skew', () => {
    const value = reading({ observed_at: (now - 90_000) / 1000 })
    expect(dailyGridPresentation(value, now).imported).toBe('12.34')
    expect(dailyGridPresentation(value, now + 1)).toMatchObject({ imported: '—', exported: '—' })
    expect(dailyGridPresentation(reading({ observed_at: now / 1000 + 1 }), now).imported).toBe(
      '12.34'
    )
    const future = dailyGridPresentation(reading({ observed_at: now / 1000 + 1.001 }), now)
    expect(future).toMatchObject({ imported: '—', exported: '—' })
    expect(future.details).toContain('in the future')
  })

  it('accepts fractional controller timestamps without rounding away freshness', () => {
    const result = dailyGridPresentation(
      reading({
        complete: false,
        status: 'partial',
        started_at: now / 1000 - 0.654321,
        observed_at: now / 1000 - 0.123456,
      }),
      now
    )
    expect(result).toMatchObject({ label: 'Since 11:29', imported: '12.34', exported: '4.56' })
  })

  it.each(['stale', 'unknown', 'reset'])(
    'honors controller %s status even with numeric values',
    (status) => {
      const result = dailyGridPresentation(
        reading({ status, reason: 'Meter counter changed.' }),
        now
      )
      expect(result).toMatchObject({ imported: '—', exported: '—' })
      expect(result.details).toContain('Meter counter changed.')
    }
  )

  it.each([
    'homeassistant.sensor.grid',
    'com.victronenergy.acload.meter',
    'com.victronenergy.system',
    'com.victronenergy.grid.',
    'com.victronenergy.grid.   ',
  ])('rejects a nonphysical or empty grid source: %s', (service) => {
    const result = dailyGridPresentation(reading({ source: { service, device_instance: 40 } }), now)
    expect(result).toMatchObject({ imported: '—', exported: '—' })
    expect(result.details).toContain('direct meter source is unavailable or invalid')
  })

  it.each([undefined, null, '', 'live', 1])('does not assume a usable status from %s', (status) => {
    expect(dailyGridPresentation(reading({ status }), now)).toMatchObject({
      imported: '—',
      exported: '—',
    })
  })

  it.each([
    { complete: true, status: 'partial' },
    { complete: false, status: 'complete' },
  ])('rejects status inconsistent with coverage: %j', (coverage) => {
    expect(dailyGridPresentation(reading(coverage), now)).toMatchObject({
      imported: '—',
      exported: '—',
    })
  })

  it('keeps old-server and malformed payloads unknown, without using legacy grid_kwh', () => {
    for (const value of [undefined, null, [], 'bad', { grid_kwh: 42 }]) {
      expect(dailyGridPresentation(value, now)).toMatchObject({ imported: '—', exported: '—' })
    }
  })
})

describe('DailyGridEnergy', () => {
  it('expires readings without another server update and releases its timer on unmount', async () => {
    vi.useFakeTimers()
    vi.setSystemTime(now)
    const wrapper = mount(DailyGridEnergy, { props: { energy: reading() } })
    expect(wrapper.text()).toBe('Today↓ 12.34 / ↑ 4.56 kWh')
    expect(wrapper.find('button').exists()).toBe(false)
    await vi.advanceTimersByTimeAsync(GRID_ENERGY_STALE_AFTER_MS)
    expect(wrapper.text()).toContain('12.34')
    await vi.advanceTimersByTimeAsync(1000)
    expect(wrapper.text()).toContain('↓ — / ↑ — kWh')
    expect(wrapper.get('.daily-grid-energy').attributes('aria-label')).toContain(
      'stale (older than 90 seconds)'
    )
    wrapper.unmount()
    expect(vi.getTimerCount()).toBe(0)
  })

  it('updates partial values from a fresh controller reading', async () => {
    vi.useFakeTimers()
    vi.setSystemTime(now)
    const wrapper = mount(DailyGridEnergy)
    expect(wrapper.text()).toContain('↓ — / ↑ — kWh')
    await wrapper.setProps({
      energy: reading({
        complete: false,
        status: 'partial',
        started_at: seconds('2026-09-29T18:00:00Z'),
        import_kwh: 0,
      }),
    })
    expect(wrapper.text()).toBe('Since 11:00↓ 0.00 / ↑ 4.56 kWh')
    expect(wrapper.get('.daily-grid-energy').attributes('aria-label')).toContain('Partial coverage')
  })
})
