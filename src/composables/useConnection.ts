import { featureConnection } from '@features'
import { invoke } from '@tauri-apps/api/core'
import { listen, type Event } from '@tauri-apps/api/event'
import { isPermissionGranted, requestPermission } from '@tauri-apps/plugin-notification'
import { type AppConfig, getAppConfig } from '../config'
import {
  chooseStartupSource,
  isIgwConfigured,
  isMqttConfigured,
  MQTT_CONNECT_WATCHDOG_MS,
  MQTT_OFFLINE_DELAY_MS,
  MQTT_RECOVERY_PROBE_MS,
  mqttReconnectDelayMs,
  shouldWatchdogFailoverToIgw,
} from '../connectionPolicy'
import { logger } from '../logger'
import {
  addNotification,
  appConfig,
  applyInverterState,
  type BannerNotification,
  clearBanner,
  dataSource,
  type InverterState,
  mqttConnected,
  refreshTelemetryQuality,
  resetInverterState,
  state,
  upsertBanner,
} from './useInverterState'
import { notify } from './useSystemNotifications'

export { notify }

async function ensureNotificationPermission() {
  try {
    const granted = await isPermissionGranted()
    if (!granted) {
      await requestPermission()
    }
  } catch (e) {
    logger.error('Notification permission error:', e)
  }
}

async function send(action: string, payload: Record<string, unknown> = {}) {
  try {
    await invoke('perform_action', { action, payload })
  } catch (e) {
    logger.error('Failed to send command:', e)
  }
}

/** Both MQTT + IGW configured → prefer MQTT; recover to MQTT while on IGW. */
let dualPathPreferMqtt = false
let mqttRecoveryTimer: ReturnType<typeof setInterval> | null = null
let mqttOnlyReconnectAttempt = 0
/** One-shot: dual-path MQTT started but never got status-true → IGW. */
let mqttConnectWatchdogTimer: ReturnType<typeof setTimeout> | null = null

function mqttConnectArgs(config: AppConfig) {
  return {
    host: config.mqtt_host,
    port: config.mqtt_port,
    tls: config.mqtt_tls === true,
    username: config.mqtt_login || null,
    password: config.mqtt_password || null,
    portalId: config.portal_id || null,
    waterTankInstance: config.water_tank_instance ?? null,
    waterPumpInstance: config.water_pump_instance ?? null,
    waterValveInstance: config.water_valve_instance ?? null,
    evchargerInstance: config.evcharger_instance ?? 40,
    evInstance: config.ev_instance ?? 22,
  }
}

function gatewayConnectArgs(config: AppConfig) {
  return {
    url: (config.gateway_url || '').trim(),
    accessClientId: (config.gateway_access_client_id || '').trim(),
    accessClientSecret: (config.gateway_access_client_secret || '').trim(),
    apiToken: (config.gateway_api_token || '').trim() || null,
    waterTankInstance: config.water_tank_instance ?? null,
    waterPumpInstance: config.water_pump_instance ?? null,
    waterValveInstance: config.water_valve_instance ?? null,
    evInstance: config.ev_instance ?? null,
    evchargerInstance: config.evcharger_instance ?? null,
  }
}

async function probeMqttReachable(config: AppConfig): Promise<boolean> {
  try {
    await invoke('test_mqtt_connection', {
      host: config.mqtt_host,
      port: config.mqtt_port,
      tls: config.mqtt_tls === true,
      username: config.mqtt_login || null,
      password: config.mqtt_password || null,
    })
    return true
  } catch {
    return false
  }
}

export function useConnection() {
  let session = 0
  let inverterEnabled = false
  let connectionKey: string | null = null
  let freshnessTimer: ReturnType<typeof setInterval> | null = null
  let listeners: Array<() => void> = []
  let transportOperations: Promise<unknown> = Promise.resolve()

  function invokeTransport(command: string, args?: Record<string, unknown>) {
    const current = session
    const operation = transportOperations
      .catch(() => {})
      .then(() => {
        // Preserve IPC completion order as well as backend lifecycle lock order.
        if (current === session) return invoke(command, args)
      })
    transportOperations = operation
    return operation
  }
  let mqttOfflineTimer: ReturnType<typeof setTimeout> | null = null

  let processStateCount = 0
  let processStateLastLogMs = 0

  function processState(newState: InverterState, snapshot = false) {
    applyInverterState(newState, { snapshot })
    processStateCount += 1
    const now = Date.now()
    if (now - processStateLastLogMs >= 5000) {
      processStateLastLogMs = now
      logger.log(
        `mqtt processState x${processStateCount} gt=${state.value.gt} tt=${state.value.tt} soc=${state.value.battery_soc}`
      )
    }
  }

  function clearMqttConnectWatchdog() {
    if (mqttConnectWatchdogTimer) {
      clearTimeout(mqttConnectWatchdogTimer)
      mqttConnectWatchdogTimer = null
    }
  }

  function startMqttConnectWatchdog() {
    if (!inverterEnabled) return
    clearMqttConnectWatchdog()
    if (!dualPathPreferMqtt) return
    mqttConnectWatchdogTimer = setTimeout(() => {
      mqttConnectWatchdogTimer = null
      if (
        shouldWatchdogFailoverToIgw({
          dualPath: dualPathPreferMqtt,
          dataSource: dataSource.value,
          mqttConnected: mqttConnected.value,
        })
      ) {
        logger.log('MQTT connect watchdog — no ConnAck; failing over to IGW')
        void failoverToIgw()
      }
    }, MQTT_CONNECT_WATCHDOG_MS)
  }

  async function startMqtt(config: AppConfig, note?: { title: string; body: string }) {
    if (!inverterEnabled) return
    const current = session
    dataSource.value = 'mqtt'
    // Set pending before invoking: ConnAck can arrive before invoke resolves.
    mqttConnected.value = false
    refreshTelemetryQuality()
    await invokeTransport('connect_mqtt', mqttConnectArgs(config))
    if (current !== session || !inverterEnabled) return
    mqttOnlyReconnectAttempt = 0
    stopMqttRecoveryProbe()
    if (note) void notify(note.title, note.body)
  }

  async function startIgw(config: AppConfig, note?: { title: string; body: string }) {
    if (!inverterEnabled) return
    clearMqttConnectWatchdog()
    const current = session
    dataSource.value = 'igw'
    mqttConnected.value = false
    refreshTelemetryQuality()
    await invokeTransport('connect_gateway', gatewayConnectArgs(config))
    if (current !== session || !inverterEnabled) return
    if (note) void notify(note.title, note.body)
  }

  async function connectMqtt() {
    cleanup()
    const current = session
    async function listenForSession<T>(name: string, handler: (event: Event<T>) => void) {
      if (current !== session) return
      const unlisten = await listen<T>(name, (event) => {
        if (current === session) handler(event)
      })
      if (current === session) listeners.push(unlisten)
      else unlisten()
    }
    try {
      const config = await getAppConfig()
      if (current !== session) return
      inverterEnabled = isMqttConfigured(config) || isIgwConfigured(config)
      const nextKey = JSON.stringify([
        config.mqtt_host,
        config.mqtt_port,
        config.mqtt_tls === true,
        config.portal_id,
        config.gateway_url,
        config.water_tank_instance,
        config.water_pump_instance,
        config.water_valve_instance,
        config.evcharger_instance,
        config.ev_instance,
      ])
      if (connectionKey !== null && connectionKey !== nextKey) resetInverterState()
      connectionKey = nextKey
      if (inverterEnabled) freshnessTimer = setInterval(refreshTelemetryQuality, 1000)
      appConfig.value = config
      if (config.color_scheme) {
        const isDark = config.color_scheme !== 'light'
        document.body.classList.toggle('light', !isDark)
        localStorage.setItem('theme', config.color_scheme)
      }

      await listenForSession<InverterState>('mqtt-state-update', (event) => {
        if (inverterEnabled) processState(event.payload)
      })

      await listenForSession<boolean>('mqtt-connection-status', (event) => {
        if (!inverterEnabled) return
        if (event.payload) {
          if (mqttOfflineTimer) {
            clearTimeout(mqttOfflineTimer)
            mqttOfflineTimer = null
          }
          clearMqttConnectWatchdog()
          mqttConnected.value = true
          refreshTelemetryQuality()
          mqttOnlyReconnectAttempt = 0
        } else if (!mqttOfflineTimer) {
          mqttOfflineTimer = setTimeout(() => {
            mqttOfflineTimer = null
            mqttConnected.value = false
            refreshTelemetryQuality()
            // MQTT lost while dual-path preferred MQTT → exclusive failover to IGW.
            if (dualPathPreferMqtt && dataSource.value === 'mqtt') {
              logger.log('Cerbo MQTT offline — failing over to IGW')
              void failoverToIgw()
            }
            // MQTT-only: Rust client reconnects with calm backoff; no frontend hammer.
          }, MQTT_OFFLINE_DELAY_MS)
        }
      })

      await listenForSession<{ title: string; body: string }>('notification', (event) => {
        addNotification(event.payload.title, event.payload.body)
      })

      await listenForSession<BannerNotification>('mqtt-notification', (event) => {
        upsertBanner(event.payload)
        addNotification(event.payload.title, event.payload.body)
      })

      await listenForSession<{ id: string }>('mqtt-notification-clear', (event) => {
        clearBanner(event.payload.id)
      })

      if (current !== session) return
      const mqttOk = isMqttConfigured(config)
      const igwOk = isIgwConfigured(config)
      dualPathPreferMqtt = mqttOk && igwOk

      let mqttReachable = false
      if (dualPathPreferMqtt) {
        mqttReachable = await probeMqttReachable(config)
        if (current !== session) return
      }

      const startup = chooseStartupSource({
        mqttConfigured: mqttOk,
        igwConfigured: igwOk,
        mqttReachable: dualPathPreferMqtt ? mqttReachable : mqttOk,
      })

      stopMqttRecoveryProbe()

      if (startup === 'mqtt') {
        try {
          await startMqtt(config, { title: 'MQTT', body: 'Connecting to inverter' })
          if (current !== session) return
          if (dualPathPreferMqtt) {
            startMqttConnectWatchdog()
          }
        } catch (e) {
          if (current !== session) return
          logger.error('MQTT connect failed:', e)
          if (igwOk) {
            logger.log('MQTT connect failed — starting IGW')
            await startIgw(config, { title: 'Gateway', body: 'MQTT unavailable — using IGW' })
            if (current !== session) return
            startMqttRecoveryProbe()
          } else {
            throw e
          }
        }
      } else if (startup === 'igw') {
        await startIgw(config, {
          title: 'Gateway',
          body: dualPathPreferMqtt ? 'MQTT unreachable — using IGW' : 'Connected remotely',
        })
        if (current !== session) return
        if (dualPathPreferMqtt) {
          startMqttRecoveryProbe()
        }
      } else {
        logger.log('Neither Cerbo MQTT nor IGW configured')
        dualPathPreferMqtt = false
        mqttConnected.value = false
        dataSource.value = 'mqtt'
        resetInverterState()
        await invokeTransport('disconnect_inverter')
      }

      if (current !== session) return
      await featureConnection.connect(config)
      if (current !== session) return

      // Auto-reconnect on wake (network change, IP renewal after sleep)
      await listenForSession('window-focused', () => {
        if (!inverterEnabled || mqttConnected.value) {
          return
        }
        logger.log(
          `Wake detected, reconnecting ${dataSource.value === 'igw' ? 'gateway' : 'MQTT'}...`
        )
        if (dualPathPreferMqtt && dataSource.value === 'igw') {
          void tryRecoverMqtt()
        } else if (!dualPathPreferMqtt && isMqttConfigured(config) && !isIgwConfigured(config)) {
          scheduleMqttOnlyReconnect(0)
        } else {
          reconnectAfterDelay(mqttReconnectDelayMs(0))
        }
      })

      try {
        if (!inverterEnabled) return
        const initial = await invoke<InverterState>('get_state')
        if (current === session && inverterEnabled) processState(initial, true)
      } catch (e) {
        logger.error('Failed to get initial state:', e)
      }
    } catch (e) {
      logger.error('Failed to connect to MQTT:', e)
      if (current === session) mqttConnected.value = false
    }
  }

  let mqttReconnectTimer: ReturnType<typeof setTimeout> | null = null

  async function failoverToIgw() {
    const current = session
    if (!inverterEnabled) return
    clearMqttConnectWatchdog()
    try {
      const config = await getAppConfig()
      if (current !== session || !inverterEnabled) return
      if (!isIgwConfigured(config)) {
        scheduleMqttOnlyReconnect()
        return
      }
      // connect_gateway stops MQTT (exclusive).
      await startIgw(config, { title: 'Gateway', body: 'MQTT lost — switched to IGW' })
      if (current === session) startMqttRecoveryProbe()
    } catch (e) {
      logger.error('IGW failover failed:', e)
      if (current === session) scheduleMqttOnlyReconnect()
    }
  }

  function stopMqttRecoveryProbe() {
    if (mqttRecoveryTimer) {
      clearInterval(mqttRecoveryTimer)
      mqttRecoveryTimer = null
    }
  }

  function startMqttRecoveryProbe() {
    stopMqttRecoveryProbe()
    if (!dualPathPreferMqtt) return
    mqttRecoveryTimer = setInterval(() => {
      void tryRecoverMqtt()
    }, MQTT_RECOVERY_PROBE_MS)
  }

  async function tryRecoverMqtt() {
    const current = session
    if (!dualPathPreferMqtt || dataSource.value === 'mqtt') return
    try {
      const config = await getAppConfig()
      if (current !== session || !inverterEnabled) return
      if (!isMqttConfigured(config)) return
      const reachable = await probeMqttReachable(config)
      if (!reachable || current !== session || !inverterEnabled) return
      logger.log('Cerbo MQTT reachable again — switching back (stops IGW)')
      stopMqttRecoveryProbe()
      // connect_mqtt stops gateway (exclusive).
      await startMqtt(config, { title: 'MQTT', body: 'Cerbo MQTT restored' })
      if (current === session) startMqttConnectWatchdog()
    } catch {
      // still down / connect failed — stay on IGW, probe again next minute
    }
  }

  function scheduleMqttOnlyReconnect(forceAttempt?: number) {
    if (!inverterEnabled) return
    if (forceAttempt !== undefined) {
      mqttOnlyReconnectAttempt = forceAttempt
    }
    const delay = mqttReconnectDelayMs(mqttOnlyReconnectAttempt)
    mqttOnlyReconnectAttempt += 1
    reconnectAfterDelay(delay)
  }

  function reconnectAfterDelay(delay = mqttReconnectDelayMs(0)) {
    if (!inverterEnabled) return
    if (mqttReconnectTimer) clearTimeout(mqttReconnectTimer)
    mqttReconnectTimer = setTimeout(() => {
      mqttReconnectTimer = null
      void connectMqtt()
    }, delay)
  }

  function cleanup() {
    if (freshnessTimer) clearInterval(freshnessTimer)
    freshnessTimer = null
    session += 1
    inverterEnabled = false
    stopMqttRecoveryProbe()
    clearMqttConnectWatchdog()
    for (const unlisten of listeners) unlisten()
    listeners = []

    if (mqttReconnectTimer) {
      clearTimeout(mqttReconnectTimer)
      mqttReconnectTimer = null
    }
    if (mqttOfflineTimer) {
      clearTimeout(mqttOfflineTimer)
      mqttOfflineTimer = null
    }
    featureConnection.cleanup()
  }

  return {
    state,
    mqttConnected,
    dataSource,
    appConfig,
    connectMqtt,
    send,
    ensureNotificationPermission,
    cleanup,
  }
}
