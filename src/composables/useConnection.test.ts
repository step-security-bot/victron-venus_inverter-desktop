import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { defaultConfig } from '../config'
import { useConnection } from './useConnection'
import { mount } from '@vue/test-utils'
import { nextTick } from 'vue'
import NotificationBanner from '../components/NotificationBanner.vue'
import {
  dataSource,
  mqttConnected,
  state,
  telemetry,
  resetInverterState,
  TELEMETRY_STALE_AFTER_MS,
  bannerNotifications,
  notifications,
} from './useInverterState'

const boundary = vi.hoisted(() => ({ invoke: vi.fn(), listen: vi.fn(), getConfig: vi.fn() }))
vi.mock('@tauri-apps/api/core', () => ({ invoke: boundary.invoke }))
vi.mock('@tauri-apps/api/event', () => ({ listen: boundary.listen }))
vi.mock('../config', async (original) => ({
  ...(await original<object>()),
  getAppConfig: boundary.getConfig,
}))
vi.mock('./useSystemNotifications', () => ({ notify: vi.fn() }))
vi.mock('@tauri-apps/plugin-notification', () => ({
  isPermissionGranted: vi.fn(),
  requestPermission: vi.fn(),
}))

type Callback = (event: { payload: unknown }) => void
let events: Map<string, Set<Callback>>
let connection: ReturnType<typeof useConnection>
const configured = () => ({
  ...defaultConfig,
  mqtt_host: 'cerbo',
  gateway_enabled: false,
  camera_enabled: false,
})
const disabled = () => ({ ...configured(), mqtt_host: '' })
function emit(name: string, payload: unknown) {
  for (const callback of events.get(name) ?? []) callback({ payload })
}
function deferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((done) => {
    resolve = done
  })
  return { promise, resolve }
}

beforeEach(() => {
  vi.useFakeTimers()
  vi.stubGlobal('localStorage', { setItem: vi.fn() })
  boundary.invoke
    .mockReset()
    .mockImplementation(async (name: string) => (name === 'get_state' ? { gt: 123 } : undefined))
  boundary.getConfig.mockReset().mockResolvedValue(configured())
  events = new Map()
  boundary.listen.mockReset().mockImplementation(async (name: string, callback: Callback) => {
    const callbacks = events.get(name) ?? new Set<Callback>()
    events.set(name, callbacks)
    callbacks.add(callback)
    return () => callbacks.delete(callback)
  })
  resetInverterState()
  bannerNotifications.value = []
  notifications.value = []
  mqttConnected.value = false
  connection = useConnection()
})
afterEach(() => {
  connection.cleanup()
  vi.useRealTimers()
  vi.unstubAllGlobals()
})

async function connectAndDisable() {
  await connection.connectMqtt()
  emit('mqtt-connection-status', true)
  expect(mqttConnected.value).toBe(true)
  const oldStateCallbacks = [...(events.get('mqtt-state-update') ?? [])]
  boundary.getConfig.mockResolvedValue(disabled())
  await connection.connectMqtt()
  return oldStateCallbacks
}

describe('inverter transport configuration lifecycle', () => {
  it('renders native Victron banners delivered over IGW and clears an acknowledged slot', async () => {
    boundary.getConfig.mockResolvedValue({
      ...disabled(),
      gateway_enabled: true,
      gateway_url: 'https://igw.example',
    })
    await connection.connectMqtt()
    expect(dataSource.value).toBe('igw')
    const wrapper = mount(NotificationBanner, { global: { mocks: { $t: (key: string) => key } } })
    try {
      for (const [slot, title, body] of [
        [11, 'Internal failure', 'JBD Battery Chain 1'],
        [12, 'Internal failure', 'JBD Battery Chain 1'],
        [13, 'Low battery voltage', 'Quattro'],
      ] as const) {
        emit('mqtt-notification', {
          id: `victron-platform-0-${slot}`,
          title,
          body,
          level: 'alarm',
          source: 'victron',
          ts: '2026-10-04T21:02:10Z',
        })
      }
      await nextTick()
      expect(wrapper.text()).toContain('Low battery voltage')
      expect(wrapper.text().match(/Internal failure/g)).toHaveLength(2)
      expect(wrapper.findAll('button')).toHaveLength(3)
      expect(notifications.value).toHaveLength(3)
      emit('mqtt-notification-clear', { id: 'victron-platform-0-13' })
      await nextTick()
      expect(wrapper.text()).not.toContain('Low battery voltage')
      expect(wrapper.findAll('button')).toHaveLength(2)
      expect(notifications.value).toHaveLength(3)
      expect(boundary.invoke).not.toHaveBeenCalledWith(
        'acknowledge_victron_banner',
        expect.anything()
      )
    } finally {
      wrapper.unmount()
    }
  })

  it.each([null, undefined, '', '  '])(
    'connects native IGW with Rust string arguments when Access is %s',
    async (empty) => {
      boundary.getConfig.mockResolvedValue({
        ...disabled(),
        gateway_enabled: true,
        gateway_url: ' https://igw.example:9151 ',
        gateway_access_client_id: empty,
        gateway_access_client_secret: empty,
        gateway_api_token: ' read-token ',
      })
      await connection.connectMqtt()
      expect(boundary.invoke).toHaveBeenCalledWith('connect_gateway', {
        url: 'https://igw.example:9151',
        accessClientId: '',
        accessClientSecret: '',
        apiToken: 'read-token',
        waterTankInstance: defaultConfig.water_tank_instance ?? null,
        waterPumpInstance: defaultConfig.water_pump_instance ?? null,
        waterValveInstance: defaultConfig.water_valve_instance ?? null,
        evInstance: defaultConfig.ev_instance ?? null,
        evchargerInstance: defaultConfig.evcharger_instance ?? null,
      })
      expect(boundary.invoke).not.toHaveBeenCalledWith('connect_mqtt', expect.anything())
      expect(dataSource.value).toBe('igw')
    }
  )

  it('keeps paired Access compatible without requiring a bearer token', async () => {
    boundary.getConfig.mockResolvedValue({
      ...disabled(),
      gateway_enabled: true,
      gateway_url: 'https://igw.example',
      gateway_access_client_id: ' client-id ',
      gateway_access_client_secret: ' client-secret ',
      gateway_api_token: null,
    })
    await connection.connectMqtt()
    expect(boundary.invoke).toHaveBeenCalledWith('connect_gateway', {
      url: 'https://igw.example',
      accessClientId: 'client-id',
      accessClientSecret: 'client-secret',
      apiToken: null,
      waterTankInstance: defaultConfig.water_tank_instance ?? null,
      waterPumpInstance: defaultConfig.water_pump_instance ?? null,
      waterValveInstance: defaultConfig.water_valve_instance ?? null,
      evInstance: defaultConfig.ev_instance ?? null,
      evchargerInstance: defaultConfig.evcharger_instance ?? null,
    })
  })

  it.each([
    { gateway_access_client_id: 'client-id', gateway_access_client_secret: null },
    { gateway_access_client_id: undefined, gateway_access_client_secret: 'client-secret' },
  ])('does not dispatch an incomplete Access pair', async (pair) => {
    boundary.getConfig.mockResolvedValue({
      ...disabled(),
      gateway_enabled: true,
      gateway_url: 'https://igw.example',
      ...pair,
    })
    await connection.connectMqtt()
    expect(boundary.invoke).not.toHaveBeenCalledWith('connect_gateway', expect.anything())
  })

  it('passes explicit TLS to both reachability probes and live/recovery connections', async () => {
    boundary.getConfig.mockResolvedValue({
      ...configured(),
      mqtt_tls: true,
      mqtt_port: 8883,
      mqtt_login: 'user',
      mqtt_password: 'test-password',
      gateway_enabled: true,
      gateway_url: 'https://gateway.example',
      gateway_access_client_id: 'id',
      gateway_access_client_secret: 'test',
    })
    await connection.connectMqtt()
    expect(boundary.invoke).toHaveBeenCalledWith(
      'test_mqtt_connection',
      expect.objectContaining({
        tls: true,
        port: 8883,
        username: 'user',
        password: 'test-password',
      })
    )
    expect(boundary.invoke).toHaveBeenCalledWith(
      'connect_mqtt',
      expect.objectContaining({ tls: true })
    )
  })

  it('keeps old anonymous configurations on TCP and does not infer TLS from port 8883', async () => {
    const legacy = { ...configured(), mqtt_port: 8883, mqtt_tls: undefined }
    boundary.getConfig.mockResolvedValue(legacy)
    await connection.connectMqtt()
    expect(boundary.invoke).toHaveBeenCalledWith(
      'connect_mqtt',
      expect.objectContaining({ tls: false, port: 8883 })
    )
  })

  it('clears old telemetry when TLS policy changes on the same endpoint', async () => {
    await connection.connectMqtt()
    expect(state.value.gt).toBe(123)
    boundary.invoke.mockImplementation(async (name: string) =>
      name === 'get_state' ? {} : undefined
    )
    boundary.getConfig.mockResolvedValue({ ...configured(), mqtt_tls: true })
    await connection.connectMqtt()
    expect(state.value.gt).toBeUndefined()
  })

  it('disconnects both inverter transports, clears telemetry and rejects late events', async () => {
    await connectAndDisable()
    expect(boundary.invoke).toHaveBeenCalledWith('disconnect_inverter', undefined)
    expect(mqttConnected.value).toBe(false)
    expect(dataSource.value).toBe('mqtt')
    expect(state.value.gt).toBeUndefined()
    emit('mqtt-state-update', { gt: 999 })
    emit('mqtt-connection-status', true)
    emit('mqtt-connection-status', false)
    emit('window-focused', undefined)
    await vi.advanceTimersByTimeAsync(120000)
    expect(state.value.gt).toBeUndefined()
    expect(mqttConnected.value).toBe(false)
    expect(boundary.invoke.mock.calls.filter(([name]) => name === 'connect_mqtt')).toHaveLength(1)
  })

  it('cancels an already scheduled reconnect when configuration is removed', async () => {
    await connection.connectMqtt()
    emit('window-focused', undefined)
    boundary.getConfig.mockResolvedValue(disabled())
    await connection.connectMqtt()
    await vi.advanceTimersByTimeAsync(120000)
    expect(boundary.invoke.mock.calls.filter(([name]) => name === 'connect_mqtt')).toHaveLength(1)
  })

  it('ignores an older configuration load that resolves after disabling', async () => {
    const old = deferred<ReturnType<typeof configured>>()
    boundary.getConfig.mockReturnValueOnce(old.promise)
    const pending = connection.connectMqtt()
    boundary.getConfig.mockResolvedValue(disabled())
    await connection.connectMqtt()
    old.resolve(configured())
    await pending
    expect(boundary.invoke).not.toHaveBeenCalledWith('connect_mqtt', expect.anything())
    expect(mqttConnected.value).toBe(false)
  })

  it('ignores a pending MQTT reachability result after disabling', async () => {
    const probe = deferred<void>()
    boundary.getConfig.mockResolvedValue({
      ...configured(),
      gateway_enabled: true,
      gateway_url: 'https://gateway.example',
      gateway_access_client_id: 'id',
      gateway_access_client_secret: 'test',
    })
    boundary.invoke.mockImplementation(async (name: string) =>
      name === 'test_mqtt_connection' ? probe.promise : undefined
    )
    const pending = connection.connectMqtt()
    await vi.waitFor(() =>
      expect(boundary.invoke).toHaveBeenCalledWith('test_mqtt_connection', expect.anything())
    )
    boundary.getConfig.mockResolvedValue(disabled())
    await connection.connectMqtt()
    probe.resolve()
    await pending
    expect(boundary.invoke).not.toHaveBeenCalledWith('connect_mqtt', expect.anything())
    expect(boundary.invoke).not.toHaveBeenCalledWith('connect_gateway', expect.anything())
  })

  it('finishes an already dispatched startup before dispatching disconnect', async () => {
    const startup = deferred<void>()
    boundary.invoke.mockImplementation(async (name: string) =>
      name === 'connect_mqtt' ? startup.promise : undefined
    )
    const pendingStart = connection.connectMqtt()
    await vi.waitFor(() =>
      expect(boundary.invoke).toHaveBeenCalledWith('connect_mqtt', expect.anything())
    )
    boundary.getConfig.mockResolvedValue(disabled())
    const pendingStop = connection.connectMqtt()
    await vi.advanceTimersByTimeAsync(0)
    expect(boundary.invoke).not.toHaveBeenCalledWith('disconnect_inverter', undefined)
    startup.resolve()
    await Promise.all([pendingStart, pendingStop])
    expect(boundary.invoke).toHaveBeenCalledWith('disconnect_inverter', undefined)
    expect(mqttConnected.value).toBe(false)
    expect(state.value.gt).toBeUndefined()
  })

  it('permits a new configured session but rejects callbacks queued by its predecessor', async () => {
    const oldStateCallbacks = await connectAndDisable()
    boundary.getConfig.mockResolvedValue(configured())
    await connection.connectMqtt()
    emit('mqtt-connection-status', true)
    emit('mqtt-state-update', { gt: 456 })
    expect(mqttConnected.value).toBe(true)
    for (const callback of oldStateCallbacks) callback({ payload: { gt: 999 } })
    expect(state.value.gt).toBe(456)
  })
})

describe('inverter observation lifecycle', () => {
  it('does not overwrite a ConnAck received before the connect IPC resolves', async () => {
    boundary.invoke.mockImplementation(async (command: string) => {
      if (command === 'connect_mqtt') emit('mqtt-connection-status', true)
      if (command === 'get_state') return { gt: 123 }
    })
    await connection.connectMqtt()
    expect(mqttConnected.value).toBe(true)
    expect(telemetry.value.observed_at).toBeNull()
  })

  it('expires a silent live feed and recovers quality on the next observed update', async () => {
    await connection.connectMqtt()
    emit('mqtt-connection-status', true)
    emit('mqtt-state-update', { gt: 10 })
    expect(telemetry.value.quality).toBe('live')
    await vi.advanceTimersByTimeAsync(TELEMETRY_STALE_AFTER_MS + 1001)
    expect(mqttConnected.value).toBe(true)
    expect(telemetry.value.quality).toBe('stale')
    emit('mqtt-state-update', { gt: 0 })
    expect(telemetry.value.quality).toBe('live')
    expect(state.value.gt).toBe(0)
  })

  it('keeps failed IGW poll replays cached through the connection debounce and resumes on recovery', async () => {
    boundary.getConfig.mockResolvedValue({
      ...disabled(),
      gateway_enabled: true,
      gateway_url: 'https://igw.example',
    })
    await connection.connectMqtt()
    emit('mqtt-connection-status', true)
    emit('mqtt-state-update', {
      gateway_snapshot: true,
      cached_snapshot: false,
      gt: 42,
      pump_switch: true,
    })
    const observedAt = telemetry.value.observed_at
    await vi.advanceTimersByTimeAsync(2000)
    emit('mqtt-connection-status', false)
    for (let failedPoll = 0; failedPoll < 3; failedPoll++) {
      emit('mqtt-state-update', {
        gateway_snapshot: true,
        cached_snapshot: true,
        gt: 42,
        pump_switch: null,
      })
      expect(mqttConnected.value).toBe(true)
      expect(state.value.gt).toBe(42)
      expect(state.value.pump_switch).toBeUndefined()
      expect(telemetry.value.observed_at).toBe(observedAt)
      expect(telemetry.value.fields.gt.observed_at).toBe(observedAt)
      expect(telemetry.value.fields.pump_switch).toBeUndefined()
      await vi.advanceTimersByTimeAsync(2000)
    }
    await vi.advanceTimersByTimeAsync(4001)
    expect(mqttConnected.value).toBe(false)
    expect(telemetry.value.quality).toBe('stale')
    emit('mqtt-connection-status', true)
    emit('mqtt-state-update', {
      gateway_snapshot: true,
      cached_snapshot: false,
      gt: 0,
      pump_switch: false,
    })
    expect(telemetry.value.observed_at).toBe(Date.now())
    expect(telemetry.value.quality).toBe('live')
    expect(state.value.gt).toBe(0)
    expect(state.value.pump_switch).toBe(false)
  })

  it('clears old installation values when endpoint or portal configuration changes', async () => {
    await connection.connectMqtt()
    emit('mqtt-state-update', { car_soc: 75, pump_switch: true })
    boundary.getConfig.mockResolvedValue({ ...configured(), mqtt_host: 'other-cerbo' })
    await connection.connectMqtt()
    expect(state.value.car_soc).toBeUndefined()
    expect(state.value.pump_switch).toBeUndefined()
    expect(telemetry.value.observed_at).toBeNull()
  })
})
