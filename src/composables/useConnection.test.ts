import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { defaultConfig } from '../config'
import {
  MQTT_CONNECT_WATCHDOG_MS,
  MQTT_OFFLINE_DELAY_MS,
  MQTT_RECOVERY_PROBE_MS,
} from '../connectionPolicy'
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
function currentNotificationSession() {
  const call = [...boundary.invoke.mock.calls]
    .reverse()
    .find(([name]) => name === 'connect_mqtt' || name === 'connect_gateway')
  const token = call?.[1]?.notificationSession
  expect(token).toEqual(expect.any(String))
  return token as string
}
function emitCurrentNotification(name: string, payload: Record<string, unknown>) {
  emit(name, { ...payload, notification_session: currentNotificationSession() })
}
function emitCurrentTransport(name: string, payload: unknown) {
  emit(name, {
    ...(name === 'mqtt-connection-status' ? { connected: payload } : (payload as object)),
    notification_session: currentNotificationSession(),
  })
}
const nativeAlarm = (title = 'Native alarm') => ({
  id: 'victron-platform-0-1',
  title,
  body: 'Battery',
  level: 'alarm',
  source: 'victron',
  ts: '2026-10-04T21:02:10Z',
})
const igwConfig = () => ({
  ...disabled(),
  gateway_enabled: true,
  gateway_url: 'https://igw.example',
})

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
    .mockImplementation(async (name: string) =>
      name === 'get_state'
        ? { gt: 123, notification_session: currentNotificationSession() }
        : undefined
    )
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
  emitCurrentTransport('mqtt-connection-status', true)
  expect(mqttConnected.value).toBe(true)
  const oldStateCallbacks = [...(events.get('mqtt-state-update') ?? [])]
  boundary.getConfig.mockResolvedValue(disabled())
  await connection.connectMqtt()
  return oldStateCallbacks
}

describe('inverter transport configuration lifecycle', () => {
  it.each(['mqtt', 'igw'])(
    'keeps the Victron event time through a %s reconnect instead of using receipt time',
    async (source) => {
      vi.setSystemTime(new Date('2026-10-05T20:02:00Z'))
      boundary.getConfig.mockResolvedValue(source === 'mqtt' ? configured() : igwConfig())
      await connection.connectMqtt()
      const eventTime = '2026-10-05T18:47:00+00:00'
      const alarm = { ...nativeAlarm(), ts: eventTime }
      const wrapper = mount(NotificationBanner, { global: { mocks: { $t: (key: string) => key } } })
      try {
        emitCurrentNotification('mqtt-notification', alarm)
        await nextTick()
        expect(wrapper.get('time').text()).toBe('1h 15m ago')
        expect(notifications.value[0]?.timestamp).toBe(Date.parse(eventTime))
        await connection.connectMqtt()
        emitCurrentNotification('mqtt-notification', alarm)
        await nextTick()
        expect(wrapper.get('time').text()).toBe('1h 15m ago')
        expect(notifications.value[0]?.timestamp).toBe(Date.parse(eventTime))
        // A partially replayed slot has unknown time until its DateTime arrives.
        emitCurrentNotification('mqtt-notification', { ...alarm, ts: '' })
        await nextTick()
        expect(wrapper.text()).not.toContain('just now')
        expect(notifications.value[0]?.timestamp).toBeNull()
        emitCurrentNotification('mqtt-notification', alarm)
        await nextTick()
        expect(wrapper.get('time').text()).toBe('1h 15m ago')
        expect(notifications.value[0]?.timestamp).toBe(Date.parse(eventTime))
        emitCurrentNotification('notification', { title: 'Local event', body: 'Occurred here' })
        expect(notifications.value[0]?.timestamp).toBe(Date.now())
      } finally {
        wrapper.unmount()
      }
    }
  )

  it('repopulates native banners across automatic MQTT/IGW failover and recovery', async () => {
    boundary.getConfig.mockResolvedValue({
      ...configured(),
      gateway_enabled: true,
      gateway_url: 'https://igw.example',
    })
    await connection.connectMqtt()
    emitCurrentTransport('mqtt-connection-status', true)
    const alarm = {
      id: 'victron-platform-0-1',
      title: 'Native alarm',
      body: 'Battery',
      level: 'alarm',
      source: 'victron',
      ts: '2026-10-04T21:02:10Z',
    }
    emitCurrentNotification('mqtt-notification', alarm)
    emitCurrentTransport('mqtt-connection-status', false)
    await vi.advanceTimersByTimeAsync(MQTT_OFFLINE_DELAY_MS)
    expect(dataSource.value).toBe('igw')
    expect(bannerNotifications.value).toHaveLength(0)
    emitCurrentNotification('mqtt-notification', alarm)
    expect(bannerNotifications.value).toHaveLength(1)
    await vi.advanceTimersByTimeAsync(MQTT_RECOVERY_PROBE_MS)
    expect(dataSource.value).toBe('mqtt')
    expect(bannerNotifications.value).toHaveLength(0)
    emitCurrentNotification('mqtt-notification', alarm)
    expect(bannerNotifications.value).toHaveLength(1)
    expect(boundary.invoke.mock.calls.some(([name]) => name === 'acknowledge_victron_banner')).toBe(
      false
    )
  })

  it.each(['replace-gateway', 'switch-mqtt', 'disable', 'cleanup'])(
    'clears native banners on %s without dismissing alarms or erasing history',
    async (change) => {
      const gateway = {
        ...disabled(),
        gateway_enabled: true,
        gateway_url: 'https://old-igw.example',
      }
      boundary.getConfig.mockResolvedValue(gateway)
      await connection.connectMqtt()
      const alarm = {
        id: 'victron-platform-0-1',
        title: 'Native alarm',
        body: 'Battery',
        level: 'alarm',
        source: 'victron',
        ts: '2026-10-04T21:02:10Z',
      }
      emitCurrentNotification('mqtt-notification', alarm)
      emitCurrentNotification('mqtt-notification', {
        ...alarm,
        id: 'controller-1',
        source: 'controller',
      })
      emitCurrentTransport('mqtt-connection-status', false)
      expect(bannerNotifications.value).toHaveLength(2)
      if (change === 'cleanup') connection.cleanup()
      else {
        boundary.getConfig.mockResolvedValue(
          change === 'replace-gateway'
            ? { ...gateway, gateway_url: 'https://new-igw.example' }
            : change === 'switch-mqtt'
              ? configured()
              : disabled()
        )
        await connection.connectMqtt()
      }
      expect(bannerNotifications.value.map((banner) => banner.id)).toEqual(['controller-1'])
      expect(notifications.value).toHaveLength(2)
      expect(
        boundary.invoke.mock.calls.some(([name]) => name === 'acknowledge_victron_banner')
      ).toBe(false)
      if (change === 'replace-gateway') {
        expect(dataSource.value).toBe('igw')
        emitCurrentNotification('mqtt-notification', alarm)
        expect(bannerNotifications.value).toHaveLength(2)
      }
    }
  )

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
        emitCurrentNotification('mqtt-notification', {
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
      emitCurrentNotification('mqtt-notification-clear', { id: 'victron-platform-0-13' })
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
        notificationSession: expect.any(String),
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
      notificationSession: expect.any(String),
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
    emitCurrentTransport('mqtt-state-update', { gt: 999 })
    emitCurrentTransport('mqtt-connection-status', true)
    emitCurrentTransport('mqtt-connection-status', false)
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
    emitCurrentTransport('mqtt-connection-status', true)
    emitCurrentTransport('mqtt-state-update', { gt: 456 })
    expect(mqttConnected.value).toBe(true)
    for (const callback of oldStateCallbacks) callback({ payload: { gt: 999 } })
    expect(state.value.gt).toBe(456)
  })
})

describe('native notification transport identities', () => {
  it.each(['same-gateway', 'other-gateway', 'mqtt'])(
    'rejects old upserts and clears during and after a pending %s replacement',
    async (target) => {
      boundary.getConfig.mockResolvedValue(igwConfig())
      await connection.connectMqtt()
      const oldToken = currentNotificationSession()
      emitCurrentNotification('mqtt-notification', nativeAlarm())
      emitCurrentNotification('mqtt-notification', {
        ...nativeAlarm(),
        id: 'controller-1',
        source: 'controller',
      })
      const config = deferred<ReturnType<typeof configured>>()
      boundary.getConfig.mockReturnValueOnce(config.promise)
      const replacement = connection.connectMqtt()
      expect(bannerNotifications.value.map(({ id }) => id)).toEqual(['controller-1'])
      emit('mqtt-notification', {
        ...nativeAlarm('Late during replacement'),
        notification_session: oldToken,
      })
      emit('mqtt-notification-clear', { id: 'controller-1', notification_session: oldToken })
      expect(bannerNotifications.value.map(({ id }) => id)).toEqual(['controller-1'])
      expect(notifications.value).toHaveLength(2)
      config.resolve(
        target === 'mqtt'
          ? configured()
          : {
              ...igwConfig(),
              gateway_url:
                target === 'same-gateway' ? 'https://igw.example' : 'https://other-igw.example',
            }
      )
      await replacement
      const newToken = currentNotificationSession()
      expect(newToken).not.toBe(oldToken)
      emitCurrentNotification('mqtt-notification', nativeAlarm('Current alarm'))
      emit('mqtt-notification', {
        ...nativeAlarm('Late after replacement'),
        notification_session: oldToken,
      })
      emit('mqtt-notification-clear', { id: nativeAlarm().id, notification_session: oldToken })
      expect(bannerNotifications.value.find(({ id }) => id === nativeAlarm().id)?.title).toBe(
        'Current alarm'
      )
      expect(notifications.value).toHaveLength(3)
      emitCurrentNotification('mqtt-notification-clear', { id: nativeAlarm().id })
      expect(bannerNotifications.value.map(({ id }) => id)).toEqual(['controller-1'])
    }
  )

  it('removes raw Victron fallback alarms on replacement while preserving other sources and history', async () => {
    await connection.connectMqtt()
    emitCurrentNotification('mqtt-notification', {
      ...nativeAlarm(),
      id: 'victron-N/portal/battery/1/Alarms/LowVoltage',
    })
    emitCurrentNotification('mqtt-notification', {
      ...nativeAlarm(),
      id: 'controller-1',
      source: 'controller',
    })
    await connection.connectMqtt()
    expect(bannerNotifications.value.map(({ id }) => id)).toEqual(['controller-1'])
    expect(notifications.value).toHaveLength(2)
    expect(boundary.invoke.mock.calls.some(([name]) => name === 'acknowledge_victron_banner')).toBe(
      false
    )
  })

  it('rejects stale status and telemetry, and does not let stale true cancel the new watchdog', async () => {
    await connection.connectMqtt()
    const oldToken = currentNotificationSession()
    boundary.getConfig.mockResolvedValue({
      ...configured(),
      gateway_enabled: true,
      gateway_url: 'https://igw.example',
    })
    await connection.connectMqtt()
    emitCurrentTransport('mqtt-state-update', { gt: 222 })
    emit('mqtt-connection-status', { connected: true, notification_session: oldToken })
    emit('mqtt-state-update', { gt: 999, notification_session: oldToken })
    expect(mqttConnected.value).toBe(false)
    expect(state.value.gt).toBe(222)
    await vi.advanceTimersByTimeAsync(MQTT_CONNECT_WATCHDOG_MS)
    expect(dataSource.value).toBe('igw')
    expect(boundary.invoke.mock.calls.filter(([name]) => name === 'connect_gateway')).toHaveLength(
      1
    )
  })

  it('rejects stale false and cancels the old offline timer when a live replacement starts', async () => {
    const dual = { ...configured(), gateway_enabled: true, gateway_url: 'https://igw.example' }
    boundary.getConfig.mockResolvedValue(dual)
    await connection.connectMqtt()
    const oldToken = currentNotificationSession()
    emitCurrentTransport('mqtt-connection-status', true)
    emitCurrentTransport('mqtt-connection-status', false)
    await vi.advanceTimersByTimeAsync(MQTT_OFFLINE_DELAY_MS - 1)
    await connection.connectMqtt()
    emitCurrentTransport('mqtt-connection-status', true)
    emit('mqtt-connection-status', { connected: false, notification_session: oldToken })
    await vi.advanceTimersByTimeAsync(MQTT_OFFLINE_DELAY_MS + MQTT_CONNECT_WATCHDOG_MS)
    expect(mqttConnected.value).toBe(true)
    expect(dataSource.value).toBe('mqtt')
    expect(boundary.invoke).not.toHaveBeenCalledWith('connect_gateway', expect.anything())
  })

  it('rejects delayed initial state from a transport replaced by automatic failover', async () => {
    boundary.getConfig.mockResolvedValue({
      ...configured(),
      gateway_enabled: true,
      gateway_url: 'https://igw.example',
    })
    const hydration = deferred<Record<string, unknown>>()
    boundary.invoke.mockImplementation(async (command: string) =>
      command === 'get_state' ? hydration.promise : undefined
    )
    const pending = connection.connectMqtt()
    await vi.waitFor(() => expect(boundary.invoke).toHaveBeenCalledWith('get_state'))
    const oldToken = currentNotificationSession()
    emitCurrentTransport('mqtt-connection-status', true)
    emitCurrentTransport('mqtt-connection-status', false)
    await vi.advanceTimersByTimeAsync(MQTT_OFFLINE_DELAY_MS)
    expect(dataSource.value).toBe('igw')
    emitCurrentTransport('mqtt-state-update', {
      gt: 222,
      gateway_snapshot: true,
      cached_snapshot: false,
    })
    hydration.resolve({ gt: 999, notification_session: oldToken })
    await pending
    expect(state.value.gt).toBe(222)
  })

  it('fails closed on malformed status and event envelopes', async () => {
    await connection.connectMqtt()
    for (const payload of [
      null,
      undefined,
      true,
      false,
      {},
      { connected: 'false', notification_session: currentNotificationSession() },
    ]) {
      emit('mqtt-connection-status', payload)
      if (payload === null || payload === undefined) {
        emit('mqtt-notification', payload)
        emit('mqtt-notification-clear', payload)
        emit('mqtt-state-update', payload)
      }
    }
    await vi.advanceTimersByTimeAsync(MQTT_OFFLINE_DELAY_MS)
    expect(mqttConnected.value).toBe(false)
    expect(notifications.value).toHaveLength(0)
  })

  it('a replaced composable owner cannot dispatch its queued connect or clear the new owner banners', async () => {
    const firstOwner = connection
    const config = deferred<ReturnType<typeof configured>>()
    boundary.getConfig.mockReturnValueOnce(config.promise)
    const oldPending = firstOwner.connectMqtt()
    connection = useConnection()
    await connection.connectMqtt()
    const newToken = currentNotificationSession()
    emitCurrentNotification('mqtt-notification', nativeAlarm('New owner'))
    config.resolve(configured())
    await oldPending
    firstOwner.cleanup()
    expect(currentNotificationSession()).toBe(newToken)
    expect(boundary.invoke.mock.calls.filter(([name]) => name === 'connect_mqtt')).toHaveLength(1)
    expect(bannerNotifications.value[0]?.title).toBe('New owner')
  })

  it('reconnects IGW with a new token after a recovery connect fails and rejects both retired clients', async () => {
    boundary.getConfig.mockResolvedValue({
      ...configured(),
      gateway_enabled: true,
      gateway_url: 'https://igw.example',
    })
    boundary.invoke.mockImplementation(async (command: string) => {
      if (command === 'test_mqtt_connection') throw new Error('Initial MQTT unavailable')
    })
    await connection.connectMqtt()
    const firstIgwToken = currentNotificationSession()
    let failedMqttToken: string | undefined
    boundary.invoke.mockImplementation(
      async (command: string, args?: { notificationSession?: string }) => {
        if (command === 'connect_mqtt') {
          failedMqttToken = args?.notificationSession
          throw new Error('MQTT connect failed after successful probe')
        }
      }
    )
    await vi.advanceTimersByTimeAsync(MQTT_RECOVERY_PROBE_MS)
    const fallbackToken = currentNotificationSession()
    expect(dataSource.value).toBe('igw')
    expect(fallbackToken).not.toBe(firstIgwToken)
    expect(fallbackToken).not.toBe(failedMqttToken)
    emitCurrentNotification('mqtt-notification', nativeAlarm('Fresh IGW fallback'))
    for (const token of [firstIgwToken, failedMqttToken]) {
      emit('mqtt-notification', { ...nativeAlarm('Retired'), notification_session: token })
      emit('mqtt-notification-clear', { id: nativeAlarm().id, notification_session: token })
      emit('mqtt-connection-status', { connected: true, notification_session: token })
    }
    expect(bannerNotifications.value[0]?.title).toBe('Fresh IGW fallback')
    expect(notifications.value).toHaveLength(1)
    await vi.advanceTimersByTimeAsync(MQTT_RECOVERY_PROBE_MS)
    expect(boundary.invoke.mock.calls.filter(([name]) => name === 'connect_gateway')).toHaveLength(
      3
    )
  })

  it('rejects stale generic local alarm history while accepting the current producer', async () => {
    await connection.connectMqtt()
    const oldToken = currentNotificationSession()
    await connection.connectMqtt()
    emit('notification', {
      title: 'Late local alarm',
      body: 'Old client',
      notification_session: oldToken,
    })
    emit('notification', { title: 'Unscoped local alarm', body: 'Missing identity' })
    expect(notifications.value).toHaveLength(0)
    emit('notification', {
      title: 'Current local alarm',
      body: 'Current client',
      notification_session: currentNotificationSession(),
    })
    expect(notifications.value).toHaveLength(1)
    expect(notifications.value[0]?.title).toBe('Current local alarm')
  })

  it('rejects missing, blank and foreign identities before banner or history mutation', async () => {
    await connection.connectMqtt()
    emitCurrentNotification('mqtt-notification', nativeAlarm())
    for (const token of [undefined, '', 'foreign-session']) {
      emit('mqtt-notification', { ...nativeAlarm('Untrusted'), notification_session: token })
      emit('mqtt-notification-clear', { id: nativeAlarm().id, notification_session: token })
    }
    expect(bannerNotifications.value[0]?.title).toBe('Native alarm')
    expect(notifications.value).toHaveLength(1)
  })

  it('accepts initial native events before connect invoke resolves', async () => {
    boundary.getConfig.mockResolvedValue(igwConfig())
    const startup = deferred<void>()
    boundary.invoke.mockImplementation(
      async (command: string, args?: { notificationSession?: string }) => {
        if (command === 'connect_gateway') {
          emit('mqtt-notification', {
            ...nativeAlarm('Initial snapshot'),
            notification_session: args?.notificationSession,
          })
          return startup.promise
        }
      }
    )
    const pending = connection.connectMqtt()
    await vi.waitFor(() => expect(bannerNotifications.value[0]?.title).toBe('Initial snapshot'))
    expect(notifications.value).toHaveLength(1)
    startup.resolve()
    await pending
  })

  it('invalidates retained callbacks on cleanup and uses a fresh identity for a new owner', async () => {
    await connection.connectMqtt()
    const oldToken = currentNotificationSession()
    emitCurrentNotification('mqtt-notification', nativeAlarm())
    const oldUpserts = [...(events.get('mqtt-notification') ?? [])]
    const oldClears = [...(events.get('mqtt-notification-clear') ?? [])]
    connection.cleanup()
    for (const callback of oldUpserts)
      callback({ payload: { ...nativeAlarm('After cleanup'), notification_session: oldToken } })
    expect(bannerNotifications.value).toHaveLength(0)
    expect(notifications.value).toHaveLength(1)
    connection = useConnection()
    await connection.connectMqtt()
    expect(currentNotificationSession()).not.toBe(oldToken)
    emitCurrentNotification('mqtt-notification', nativeAlarm('New owner'))
    for (const callback of oldClears)
      callback({ payload: { id: nativeAlarm().id, notification_session: oldToken } })
    emit('mqtt-notification', { ...nativeAlarm('Previous owner'), notification_session: oldToken })
    expect(bannerNotifications.value[0]?.title).toBe('New owner')
    expect(notifications.value).toHaveLength(2)
  })

  it('fences delayed events during automatic failover and pending recovery connect', async () => {
    const dual = { ...configured(), gateway_enabled: true, gateway_url: 'https://igw.example' }
    boundary.getConfig.mockResolvedValue(dual)
    await connection.connectMqtt()
    const mqttToken = currentNotificationSession()
    emitCurrentTransport('mqtt-connection-status', true)
    emitCurrentNotification('mqtt-notification', nativeAlarm('MQTT'))
    const failoverConfig = deferred<ReturnType<typeof configured>>()
    boundary.getConfig.mockReturnValueOnce(failoverConfig.promise)
    emitCurrentTransport('mqtt-connection-status', false)
    await vi.advanceTimersByTimeAsync(MQTT_OFFLINE_DELAY_MS)
    emit('mqtt-notification', { ...nativeAlarm('Late MQTT'), notification_session: mqttToken })
    expect(bannerNotifications.value).toHaveLength(0)
    expect(notifications.value).toHaveLength(1)
    failoverConfig.resolve(dual)
    await vi.waitFor(() => expect(dataSource.value).toBe('igw'))
    const igwToken = currentNotificationSession()
    expect(igwToken).not.toBe(mqttToken)
    emitCurrentNotification('mqtt-notification', nativeAlarm('IGW'))
    emit('mqtt-notification-clear', { id: nativeAlarm().id, notification_session: mqttToken })
    expect(bannerNotifications.value[0]?.title).toBe('IGW')
    const recovery = deferred<void>()
    boundary.invoke.mockImplementation(
      async (command: string, args?: { notificationSession?: string }) => {
        if (command === 'connect_mqtt') {
          emit('mqtt-notification', {
            ...nativeAlarm('Recovered MQTT'),
            notification_session: args?.notificationSession,
          })
          return recovery.promise
        }
      }
    )
    await vi.advanceTimersByTimeAsync(MQTT_RECOVERY_PROBE_MS)
    expect(currentNotificationSession()).not.toBe(igwToken)
    emit('mqtt-notification', { ...nativeAlarm('Late IGW'), notification_session: igwToken })
    emit('mqtt-notification-clear', { id: nativeAlarm().id, notification_session: igwToken })
    expect(bannerNotifications.value[0]?.title).toBe('Recovered MQTT')
    expect(notifications.value).toHaveLength(3)
    recovery.resolve()
    await vi.advanceTimersByTimeAsync(0)
  })

  it('retains current banners on failed HTTP polls and failed recovery probes', async () => {
    boundary.getConfig.mockResolvedValue({
      ...configured(),
      gateway_enabled: true,
      gateway_url: 'https://igw.example',
    })
    boundary.invoke.mockImplementation(async (command: string) => {
      if (command === 'test_mqtt_connection') throw new Error('MQTT unavailable')
    })
    await connection.connectMqtt()
    const token = currentNotificationSession()
    emitCurrentNotification('mqtt-notification', nativeAlarm('IGW retained'))
    emitCurrentTransport('mqtt-connection-status', false)
    emitCurrentTransport('mqtt-state-update', { gateway_snapshot: true, cached_snapshot: true })
    await vi.advanceTimersByTimeAsync(MQTT_RECOVERY_PROBE_MS)
    expect(currentNotificationSession()).toBe(token)
    expect(bannerNotifications.value[0]?.title).toBe('IGW retained')
    expect(notifications.value).toHaveLength(1)
    emitCurrentNotification('mqtt-notification-clear', { id: nativeAlarm().id })
    expect(bannerNotifications.value).toHaveLength(0)
  })

  it('skips an obsolete queued connect and cannot reactivate its predecessor after overlapping connects', async () => {
    const startup = deferred<void>()
    boundary.invoke.mockImplementationOnce(async () => startup.promise)
    const first = connection.connectMqtt()
    await vi.waitFor(() =>
      expect(boundary.invoke).toHaveBeenCalledWith('connect_mqtt', expect.anything())
    )
    const firstToken = currentNotificationSession()
    boundary.getConfig.mockResolvedValue(igwConfig())
    const second = connection.connectMqtt()
    await vi.waitFor(() => expect(dataSource.value).toBe('igw'))
    boundary.getConfig.mockResolvedValue({ ...configured(), mqtt_host: 'new-cerbo' })
    const third = connection.connectMqtt()
    await vi.waitFor(() => expect(dataSource.value).toBe('mqtt'))
    emit('mqtt-notification', {
      ...nativeAlarm('Late in-flight connection'),
      notification_session: firstToken,
    })
    expect(notifications.value).toHaveLength(0)
    startup.resolve()
    await Promise.all([first, second, third])
    expect(boundary.invoke).not.toHaveBeenCalledWith('connect_gateway', expect.anything())
    const connects = boundary.invoke.mock.calls.filter(([name]) => name === 'connect_mqtt')
    expect(connects).toHaveLength(2)
    expect(connects[1][1].host).toBe('new-cerbo')
    expect(currentNotificationSession()).not.toBe(firstToken)
    emitCurrentNotification('mqtt-notification', nativeAlarm('Newest connection'))
    emit('mqtt-notification-clear', { id: nativeAlarm().id, notification_session: firstToken })
    expect(bannerNotifications.value[0]?.title).toBe('Newest connection')
    expect(notifications.value).toHaveLength(1)
  })
})

describe('inverter observation lifecycle', () => {
  it('does not overwrite a ConnAck received before the connect IPC resolves', async () => {
    boundary.invoke.mockImplementation(async (command: string) => {
      if (command === 'connect_mqtt') emitCurrentTransport('mqtt-connection-status', true)
      if (command === 'get_state') return { gt: 123 }
    })
    await connection.connectMqtt()
    expect(mqttConnected.value).toBe(true)
    expect(telemetry.value.observed_at).toBeNull()
  })

  it('expires a silent live feed and recovers quality on the next observed update', async () => {
    await connection.connectMqtt()
    emitCurrentTransport('mqtt-connection-status', true)
    emitCurrentTransport('mqtt-state-update', { gt: 10 })
    expect(telemetry.value.quality).toBe('live')
    await vi.advanceTimersByTimeAsync(TELEMETRY_STALE_AFTER_MS + 1001)
    expect(mqttConnected.value).toBe(true)
    expect(telemetry.value.quality).toBe('stale')
    emitCurrentTransport('mqtt-state-update', { gt: 0 })
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
    emitCurrentTransport('mqtt-connection-status', true)
    emitCurrentTransport('mqtt-state-update', {
      gateway_snapshot: true,
      cached_snapshot: false,
      gt: 42,
      pump_switch: true,
    })
    const observedAt = telemetry.value.observed_at
    await vi.advanceTimersByTimeAsync(2000)
    emitCurrentTransport('mqtt-connection-status', false)
    for (let failedPoll = 0; failedPoll < 3; failedPoll++) {
      emitCurrentTransport('mqtt-state-update', {
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
    emitCurrentTransport('mqtt-connection-status', true)
    emitCurrentTransport('mqtt-state-update', {
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
    emitCurrentTransport('mqtt-state-update', { car_soc: 75, pump_switch: true })
    boundary.getConfig.mockResolvedValue({ ...configured(), mqtt_host: 'other-cerbo' })
    await connection.connectMqtt()
    expect(state.value.car_soc).toBeUndefined()
    expect(state.value.pump_switch).toBeUndefined()
    expect(telemetry.value.observed_at).toBeNull()
  })
})
