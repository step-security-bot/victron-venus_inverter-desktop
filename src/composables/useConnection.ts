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
import { notificationTimestampMs } from '../utils'
import {
  addNotification,
  appConfig,
  applyInverterState,
  type BannerNotification,
  clearBanner,
  clearVictronBanners,
  dataSource,
  type InverterState,
  mqttConnected,
  refreshTelemetryQuality,
  resetInverterState,
  state,
  upsertBanner,
} from './useInverterState'
import { notify } from './useSystemNotifications'
import {
  acceptsCurrentTransportEvent,
  activateTransportSession,
  deactivateTransportSession,
  invalidateTransportSession,
  isCurrentTransportSession,
  reserveTransportSession,
  type TransportEvent,
} from './transportSession'

export { notify }

// Serialize transport commands across composable owners as well as reconnects.
let transportOperations: Promise<unknown> = Promise.resolve()

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
  /** Both MQTT + IGW configured → prefer MQTT; recover to MQTT while on IGW. */
  let dualPathPreferMqtt = false
  let mqttRecoveryTimer: ReturnType<typeof setInterval> | null = null
  let mqttOnlyReconnectAttempt = 0
  let mqttConnectWatchdogTimer: ReturnType<typeof setTimeout> | null = null
  let session = 0
  let inverterEnabled = false
  let connectionKey: string | null = null
  let freshnessTimer: ReturnType<typeof setInterval> | null = null
  let listeners: Array<() => void> = []
  let transportGeneration = 0
  let notificationSession: string | null = null
  let recoveryProbe: object | null = null

  type TransportAttempt = { lifecycle: number; generation: number; notificationSession: string }

  function invalidateNotifications() {
    invalidateTransportSession(notificationSession)
    notificationSession = null
  }

  function beginTransportReplacement(): TransportAttempt {
    invalidateNotifications()
    transportGeneration += 1
    clearVictronBanners()
    clearMqttConnectWatchdog()
    if (mqttOfflineTimer) clearTimeout(mqttOfflineTimer)
    mqttOfflineTimer = null
    if (mqttReconnectTimer) clearTimeout(mqttReconnectTimer)
    mqttReconnectTimer = null
    notificationSession = reserveTransportSession()
    return {
      lifecycle: session,
      generation: transportGeneration,
      notificationSession,
    }
  }

  function isCurrentTransport(attempt: TransportAttempt, requireEnabled = true) {
    return (
      (!requireEnabled || inverterEnabled) &&
      attempt.lifecycle === session &&
      attempt.generation === transportGeneration &&
      isCurrentTransportSession(attempt.notificationSession)
    )
  }

  function transportIsOwned() {
    return inverterEnabled && isCurrentTransportSession(notificationSession)
  }

  function acceptsNotification(payload: unknown) {
    return (
      inverterEnabled &&
      notificationSession !== null &&
      isCurrentTransportSession(notificationSession) &&
      acceptsCurrentTransportEvent(payload)
    )
  }

  function invokeTransport(
    command: string,
    args?: Record<string, unknown>,
    attempt?: TransportAttempt
  ) {
    const current = session
    const operation = transportOperations
      .catch(() => {})
      .then(() => {
        // Preserve IPC completion order as well as backend lifecycle lock order.
        if (current !== session || (attempt && !isCurrentTransport(attempt, false))) return
        // Register the identity before invoke: a native initial event may arrive
        // before the command promise resolves.
        if (attempt && command !== 'disconnect_inverter')
          activateTransportSession(attempt.notificationSession)
        return invoke(command, args)
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
    if (!transportIsOwned()) return
    clearMqttConnectWatchdog()
    if (!dualPathPreferMqtt) return
    const token = notificationSession
    const watchdogTimer = setTimeout(() => {
      if (mqttConnectWatchdogTimer !== watchdogTimer) return
      mqttConnectWatchdogTimer = null
      if (token !== notificationSession || !transportIsOwned()) return
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
    mqttConnectWatchdogTimer = watchdogTimer
  }

  async function startMqtt(
    config: AppConfig,
    note: { title: string; body: string } | undefined,
    attempt: TransportAttempt
  ) {
    if (!isCurrentTransport(attempt)) return false
    dataSource.value = 'mqtt'
    // Set pending before invoking: ConnAck can arrive before invoke resolves.
    mqttConnected.value = false
    refreshTelemetryQuality()
    try {
      await invokeTransport(
        'connect_mqtt',
        {
          ...mqttConnectArgs(config),
          notificationSession: attempt.notificationSession,
        },
        attempt
      )
    } catch (error) {
      if (!isCurrentTransport(attempt)) return false
      deactivateTransportSession(attempt.notificationSession)
      throw error
    }
    if (!isCurrentTransport(attempt)) return false
    mqttOnlyReconnectAttempt = 0
    stopMqttRecoveryProbe()
    if (note) void notify(note.title, note.body)
    return true
  }

  async function startIgw(
    config: AppConfig,
    note: { title: string; body: string } | undefined,
    attempt: TransportAttempt
  ) {
    if (!isCurrentTransport(attempt)) return false
    clearMqttConnectWatchdog()
    dataSource.value = 'igw'
    mqttConnected.value = false
    refreshTelemetryQuality()
    try {
      await invokeTransport(
        'connect_gateway',
        {
          ...gatewayConnectArgs(config),
          notificationSession: attempt.notificationSession,
        },
        attempt
      )
    } catch (error) {
      if (!isCurrentTransport(attempt)) return false
      deactivateTransportSession(attempt.notificationSession)
      throw error
    }
    if (!isCurrentTransport(attempt)) return false
    if (note) void notify(note.title, note.body)
    return true
  }

  async function connectMqtt() {
    cleanup()
    let attempt = beginTransportReplacement()
    const current = session
    const requestIsCurrent = () => isCurrentTransport(attempt, false)
    async function listenForSession<T>(name: string, handler: (event: Event<T>) => void) {
      if (!requestIsCurrent()) return
      const unlisten = await listen<T>(name, (event) => {
        if (current === session && isCurrentTransportSession(notificationSession)) handler(event)
      })
      if (requestIsCurrent()) listeners.push(unlisten)
      else unlisten()
    }
    try {
      const config = await getAppConfig()
      if (!requestIsCurrent()) return
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
      if (inverterEnabled)
        freshnessTimer = setInterval(() => {
          if (transportIsOwned()) refreshTelemetryQuality()
        }, 1000)
      appConfig.value = config
      if (config.color_scheme) {
        const isDark = config.color_scheme !== 'light'
        document.body.classList.toggle('light', !isDark)
        localStorage.setItem('theme', config.color_scheme)
      }

      await listenForSession<TransportEvent<InverterState>>('mqtt-state-update', (event) => {
        if (acceptsNotification(event.payload)) processState(event.payload)
      })

      await listenForSession<TransportEvent<{ connected: boolean }>>(
        'mqtt-connection-status',
        (event) => {
          if (!acceptsNotification(event.payload)) return
          if (event.payload.connected === true) {
            if (mqttOfflineTimer) {
              clearTimeout(mqttOfflineTimer)
              mqttOfflineTimer = null
            }
            clearMqttConnectWatchdog()
            mqttConnected.value = true
            refreshTelemetryQuality()
            mqttOnlyReconnectAttempt = 0
          } else if (event.payload.connected === false && !mqttOfflineTimer) {
            const statusToken = notificationSession
            const offlineTimer = setTimeout(() => {
              if (mqttOfflineTimer !== offlineTimer) return
              mqttOfflineTimer = null
              if (statusToken !== notificationSession || !transportIsOwned()) return
              mqttConnected.value = false
              refreshTelemetryQuality()
              // MQTT lost while dual-path preferred MQTT → exclusive failover to IGW.
              if (dualPathPreferMqtt && dataSource.value === 'mqtt') {
                logger.log('Cerbo MQTT offline — failing over to IGW')
                void failoverToIgw()
              }
              // MQTT-only: Rust client reconnects with calm backoff; no frontend hammer.
            }, MQTT_OFFLINE_DELAY_MS)
            mqttOfflineTimer = offlineTimer
          }
        }
      )

      await listenForSession<TransportEvent<{ title: string; body: string }>>(
        'notification',
        (event) => {
          if (!acceptsNotification(event.payload)) return
          addNotification(event.payload.title, event.payload.body)
        }
      )

      await listenForSession<TransportEvent<BannerNotification>>('mqtt-notification', (event) => {
        if (!acceptsNotification(event.payload)) return
        upsertBanner(event.payload)
        addNotification(
          event.payload.title,
          event.payload.body,
          notificationTimestampMs(event.payload.ts)
        )
      })

      await listenForSession<TransportEvent<{ id: string }>>('mqtt-notification-clear', (event) => {
        if (!acceptsNotification(event.payload)) return
        clearBanner(event.payload.id)
      })

      if (!requestIsCurrent()) return
      const mqttOk = isMqttConfigured(config)
      const igwOk = isIgwConfigured(config)
      dualPathPreferMqtt = mqttOk && igwOk

      let mqttReachable = false
      if (dualPathPreferMqtt) {
        mqttReachable = await probeMqttReachable(config)
        if (!requestIsCurrent()) return
      }

      const startup = chooseStartupSource({
        mqttConfigured: mqttOk,
        igwConfigured: igwOk,
        mqttReachable: dualPathPreferMqtt ? mqttReachable : mqttOk,
      })

      stopMqttRecoveryProbe()

      if (startup === 'mqtt') {
        try {
          if (
            !(await startMqtt(config, { title: 'MQTT', body: 'Connecting to inverter' }, attempt))
          )
            return
          if (!requestIsCurrent()) return
          if (dualPathPreferMqtt) {
            startMqttConnectWatchdog()
          }
        } catch (e) {
          if (!requestIsCurrent()) return
          logger.error('MQTT connect failed:', e)
          if (igwOk) {
            logger.log('MQTT connect failed — starting IGW')
            attempt = beginTransportReplacement()
            if (
              !(await startIgw(
                config,
                { title: 'Gateway', body: 'MQTT unavailable — using IGW' },
                attempt
              ))
            )
              return
            if (!requestIsCurrent()) return
            startMqttRecoveryProbe()
          } else {
            throw e
          }
        }
      } else if (startup === 'igw') {
        if (
          !(await startIgw(
            config,
            {
              title: 'Gateway',
              body: dualPathPreferMqtt ? 'MQTT unreachable — using IGW' : 'Connected remotely',
            },
            attempt
          ))
        )
          return
        if (!requestIsCurrent()) return
        if (dualPathPreferMqtt) {
          startMqttRecoveryProbe()
        }
      } else {
        logger.log('Neither Cerbo MQTT nor IGW configured')
        dualPathPreferMqtt = false
        mqttConnected.value = false
        dataSource.value = 'mqtt'
        resetInverterState()
        await invokeTransport('disconnect_inverter', undefined, attempt)
      }

      if (!requestIsCurrent()) return
      await featureConnection.connect(config)
      if (!requestIsCurrent()) return

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
        const initialAttempt = attempt
        const initial = await invoke<TransportEvent<InverterState>>('get_state')
        if (isCurrentTransport(initialAttempt) && acceptsNotification(initial))
          processState(initial, true)
      } catch (e) {
        logger.error('Failed to get initial state:', e)
      }
    } catch (e) {
      logger.error('Failed to connect to MQTT:', e)
      if (requestIsCurrent()) mqttConnected.value = false
    }
  }

  let mqttReconnectTimer: ReturnType<typeof setTimeout> | null = null

  async function failoverToIgw() {
    if (!transportIsOwned()) return
    const attempt = beginTransportReplacement()
    clearMqttConnectWatchdog()
    try {
      const config = await getAppConfig()
      if (!isCurrentTransport(attempt)) return
      if (!isIgwConfigured(config)) {
        scheduleMqttOnlyReconnect()
        return
      }
      // connect_gateway stops MQTT (exclusive).
      if (
        await startIgw(config, { title: 'Gateway', body: 'MQTT lost — switched to IGW' }, attempt)
      )
        startMqttRecoveryProbe()
    } catch (e) {
      logger.error('IGW failover failed:', e)
      if (isCurrentTransport(attempt)) scheduleMqttOnlyReconnect()
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
    if (!dualPathPreferMqtt || !transportIsOwned()) return
    const token = notificationSession
    mqttRecoveryTimer = setInterval(() => {
      if (token !== notificationSession || !transportIsOwned()) return
      void tryRecoverMqtt()
    }, MQTT_RECOVERY_PROBE_MS)
  }

  async function tryRecoverMqtt() {
    const current = session
    const generation = transportGeneration
    if (!dualPathPreferMqtt || dataSource.value === 'mqtt' || recoveryProbe || !transportIsOwned())
      return
    const probe = {}
    recoveryProbe = probe
    const stillCurrent = () =>
      current === session && generation === transportGeneration && transportIsOwned()
    try {
      const config = await getAppConfig()
      if (!stillCurrent() || !isMqttConfigured(config)) return
      const reachable = await probeMqttReachable(config)
      if (!reachable || !stillCurrent()) return
      logger.log('Cerbo MQTT reachable again — switching back (stops IGW)')
      stopMqttRecoveryProbe()
      // A failed reachability probe keeps the live IGW identity. Rotate it only
      // when replacement is actually selected, before the connect invocation.
      const attempt = beginTransportReplacement()
      await recoverMqttOrReconnectIgw(config, attempt)
    } catch {
      // A failed configuration read / reachability probe keeps the current IGW.
    } finally {
      if (recoveryProbe === probe) recoveryProbe = null
    }
  }

  async function recoverMqttOrReconnectIgw(config: AppConfig, attempt: TransportAttempt) {
    try {
      if (await startMqtt(config, { title: 'MQTT', body: 'Cerbo MQTT restored' }, attempt))
        startMqttConnectWatchdog()
    } catch (error) {
      if (!isCurrentTransport(attempt)) return
      logger.error('MQTT recovery connect failed:', error)
      await reconnectIgwAfterFailedRecovery(config)
    }
  }

  async function reconnectIgwAfterFailedRecovery(config: AppConfig) {
    // The native connect may already have stopped IGW. Reconnect with a new
    // identity; never revive the previous client's queued events.
    const fallback = beginTransportReplacement()
    try {
      if (await startIgw(config, undefined, fallback)) startMqttRecoveryProbe()
    } catch (error) {
      if (!isCurrentTransport(fallback)) return
      logger.error('IGW recovery fallback failed:', error)
      scheduleMqttOnlyReconnect()
    }
  }

  function scheduleMqttOnlyReconnect(forceAttempt?: number) {
    if (!transportIsOwned()) return
    if (forceAttempt !== undefined) {
      mqttOnlyReconnectAttempt = forceAttempt
    }
    const delay = mqttReconnectDelayMs(mqttOnlyReconnectAttempt)
    mqttOnlyReconnectAttempt += 1
    reconnectAfterDelay(delay)
  }

  function reconnectAfterDelay(delay = mqttReconnectDelayMs(0)) {
    if (!transportIsOwned()) return
    const token = notificationSession
    if (mqttReconnectTimer) clearTimeout(mqttReconnectTimer)
    const reconnectTimer = setTimeout(() => {
      if (mqttReconnectTimer !== reconnectTimer) return
      mqttReconnectTimer = null
      if (token !== notificationSession || !transportIsOwned()) return
      void connectMqtt()
    }, delay)
    mqttReconnectTimer = reconnectTimer
  }

  function cleanup() {
    if (freshnessTimer) clearInterval(freshnessTimer)
    freshnessTimer = null
    const ownedTransport = isCurrentTransportSession(notificationSession)
    session += 1
    transportGeneration += 1
    invalidateNotifications()
    recoveryProbe = null
    inverterEnabled = false
    stopMqttRecoveryProbe()
    clearMqttConnectWatchdog()
    for (const unlisten of listeners) unlisten()
    listeners = []
    if (ownedTransport) clearVictronBanners()

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
