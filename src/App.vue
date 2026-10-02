<template>
  <ErrorBoundary>
    <section
      id="app"
      aria-label="Inverter dashboard"
      class="app-shell dashboard h-screen flex flex-col p-1.5 gap-1 select-none overflow-hidden"
      @contextmenu.prevent="onContextMenu"
    >
      <!-- Dashboard Header: Compact buttons and theme switcher -->
      <div class="flex items-center justify-between">
        <AppHeader
          :dryRun="coerceBoolean(state.dry_run)"
          :essClass="essClass"
          :essText="essText"
          :essMode="state.ess_mode"
          :essConnected="mqttConnected"
          :essFresh="!!telemetry.fields.ess_mode && !telemetry.stale_fields.includes('ess_mode')"
          :headerControls="headerControls"
          :controlStates="headerControlStates"
          :isDark="isDark"
          :showHeaderToggles="visibleSections.show_header_toggles"
          @send="send"
          @toggle-theme="toggleTheme"
          @open-config="openConfig"
        >
          <template #actions><DashboardFeatureActions @error="showError" /></template>
        </AppHeader>
      </div>

      <!-- Dashboard Content: Grid and Panels -->
      <div
        class="dashboard-content flex-1 overflow-y-auto pr-0.5 flex flex-col gap-1.5 scrollbar-hide min-h-0"
      >
        <DailyStats
          v-if="visibleSections.show_daily_stats"
          :tariff-scope="tariffScope(appConfig)"
        />

        <NotificationBanner />

        <StatCards
          :gt="state.gt"
          :g1="state.g1"
          :g2="state.g2"
          :gridL1Available="state.grid_l1_available"
          :gridL2Available="state.grid_l2_available"
          :gridBackup="state.grid_backup"
          :gridUsingBackup="state.grid_using_backup"
          :gridBackupObservedAt="state.grid_backup_observed_at"
          :tt="state.tt"
          :t1="state.t1"
          :t2="state.t2"
          :solarTotal="state.solar_total"
          :mpptTotal="mpptTotal"
          :pvInvertersTotal="pvInvertersTotal"
          :batterySoc="state.battery_soc"
          :batteryPower="state.battery_power"
          :batteryVoltage="state.battery_voltage"
          :batteryCurrent="state.battery_current"
          :setpoint="state.setpoint"
          :inverterState="state.inverter_state"
        />

        <div class="dashboard-panels grid grid-cols-1 md:grid-cols-12 gap-1.5 md:auto-rows-fr">
          <div class="dashboard-chart md:col-span-8 h-[280px] md:h-auto md:min-h-[280px]">
            <ChartPanel :chartOption="chartOption" />
          </div>
          <div class="dashboard-side md:col-span-4">
            <SidePanel
              :showEv="visibleSections.show_ev"
              :evSectionVisible="evSectionVisible"
              :carSoc="evSoc"
              :carChargingPower="evChargingKw != null ? evChargingKw * 1000 : null"
              :evChargingPower="evPowerWatts"
              :waterVisible="waterSectionVisible"
              :waterValve="waterValveState"
              :pumpSwitch="pumpSwitchState"
              :waterLevel="waterLevel"
              :waterPumpMode="waterPumpMode"
              :waterValveMode="waterValveMode"
              :homeButtons="homeButtons"
              :buttonStates="homeButtonStates"
              :controlsConnected="controlsConnected"
              :getControlLabel="features.getControlLabel"
              :getControlIcon="features.getControlIcon"
              :showHomeSection="visibleSections.show_home_section"
              @send="send"
            >
              <DashboardFeaturePanels @send="send" />
            </SidePanel>
          </div>
        </div>

        <BatterySolarPanel
          v-if="visibleSections.show_batteries || visibleSections.show_solar_production"
          :batteries="batteries"
          :solarSources="solarSources"
          :showBatteries="visibleSections.show_batteries"
          :showSolar="visibleSections.show_solar_production"
        />

        <LoadsTable v-if="visibleSections.show_active_loads" :loads="acloads" />
      </div>

      <!-- Bottom Status Bar: Classic dot layout -->
      <StatusBar
        :mqttConnected="mqttConnected"
        :dataSource="dataSource"
        :uptime="state.uptime"
        :appVersion="appVersion"
        :stateVersion="state.version"
      >
        <template #leading><DashboardFeatureStatus /></template>
        <template #connections><DashboardConnectionStatus /></template>
      </StatusBar>

      <ContextMenu
        :show="contextMenu.show"
        :x="contextMenu.x"
        :y="contextMenu.y"
        @open-config="openConfig"
        @check-updates="checkForUpdates"
      />

      <!-- First-run setup wizard -->
      <SetupWizard v-if="showSetupWizard" @complete="handleSetupComplete" />

      <!-- Toast Notification -->
      <div
        v-if="message"
        class="app-toast fixed bottom-4 left-1/2 -translate-x-1/2 z-[60] animate-in slide-in-from-bottom duration-200"
        :class="messageType === 'error' ? 'app-toast-err' : 'app-toast-ok'"
      >
        {{ message }}
      </div>
    </section>
  </ErrorBoundary>
</template>

<script setup lang="ts">
import { useReleaseVersion } from './composables/useReleaseVersion'
import { invoke } from '@tauri-apps/api/core'
import { listen, type Event, type UnlistenFn } from '@tauri-apps/api/event'
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import AppHeader from './components/AppHeader.vue'
import { telemetry } from './composables/useInverterState'
import SetupWizard from './components/SetupWizard.vue'
import BatterySolarPanel from './components/BatterySolarPanel.vue'
import ChartPanel from './components/ChartPanel.vue'
import ContextMenu from './components/ContextMenu.vue'
import DailyStats from './components/DailyStats.vue'
import { tariffScope } from './tariffs/scope'
import ErrorBoundary from './components/ErrorBoundary.vue'
import LoadsTable from './components/LoadsTable.vue'
import NotificationBanner from './components/NotificationBanner.vue'
import SidePanel from './components/SidePanel.vue'
import StatCards from './components/StatCards.vue'
import StatusBar from './components/StatusBar.vue'
import { checkForUpdates } from './composables/useAutoUpdate'
import { addHistoryPoint, useChart } from './composables/useChart'
import { notify, useConnection } from './composables/useConnection'
import {
  useDashboardFeatures,
  DashboardFeaturePanels,
  DashboardFeatureActions,
  DashboardFeatureStatus,
  DashboardConnectionStatus,
} from '@features'
import { sendControlAction, useDashboardControls } from './composables/useDashboardControls'
import { useInverterVisibility } from './composables/useInverterVisibility'
import { useMQTTState } from './composables/useMQTTState'
import { initSystemNotifications } from './composables/useSystemNotifications'
import { useTheme } from './composables/useTheme'
import { getAppConfig, needsSetup, sectionVisibility } from './config'
import type { AppConfig } from './config'
import { logger } from './logger'
import { coerceBoolean } from './utils'

const {
  state,
  mqttConnected,
  dataSource,
  appConfig,
  connectMqtt,
  ensureNotificationPermission,
  cleanup: cleanupConnection,
} = useConnection()
const visibleSections = computed(() => sectionVisibility(appConfig.value))
const features = useDashboardFeatures()
const { controlsConnected } = features
const coreControls = useDashboardControls(features.getControlState, features.allowHomeControls)
const { headerControlStates, homeButtonStates } = coreControls
const headerControls = computed(() =>
  features.mergeControls('header', coreControls.headerControls.value)
)
const homeButtons = computed(() => features.mergeControls('home', coreControls.homeButtons.value))
const { setInverterWindowHidden, cleanupInverterVisibility } = useInverterVisibility()
const {
  waterLevel,
  pumpSwitchState,
  waterValveState,
  waterPumpMode,
  waterValveMode,
  waterSectionVisible,
  evSoc,
  evChargingKw,
  evPowerWatts,
  evSectionVisible,
  acloads,
} = useMQTTState()
const { isDark, toggleTheme } = useTheme()
const { chartOption, forceUpdateChart, setChartPaused } = useChart(isDark)
const isWindowHidden = ref(false)

const appVersion = useReleaseVersion()
const contextMenu = ref({ show: false, x: 0, y: 0 })
const showSetupWizard = ref(false)
const message = ref('')
const messageType = ref<'success' | 'error'>('success')
const unlisteners: UnlistenFn[] = []
let disposed = false
let configRevision = 0

async function listenWhileMounted<T>(name: string, callback: (event: Event<T>) => void) {
  const unlisten = await listen<T>(name, (event) => {
    if (!disposed) callback(event)
  })
  if (disposed) {
    unlisten()
    return false
  }
  unlisteners.push(unlisten)
  return true
}

function clearMessage() {
  message.value = ''
}

function showError(msg: string) {
  message.value = msg
  messageType.value = 'error'
  setTimeout(clearMessage, 3000)
}
async function handleSetupComplete(cfg: AppConfig) {
  if (disposed) return
  showSetupWizard.value = false
  appConfig.value = cfg
  await connectMqtt()
  if (!disposed) await features.init()
}

function onContextMenu(e: MouseEvent) {
  contextMenu.value = { show: true, x: e.clientX, y: e.clientY }
}

function closeContextMenu() {
  contextMenu.value.show = false
}

async function openConfig() {
  contextMenu.value.show = false
  try {
    await invoke('open_config_window')
  } catch (e) {
    logger.error('Failed to open config window:', e)
    showError(`Failed to open config: ${e?.toString() || e}`)
  }
}

async function send(action: string, payload: Record<string, unknown> = {}) {
  try {
    await sendControlAction(action, payload)
  } catch (e) {
    logger.error('Action failed:', action, payload, e)
    showError(`Failed: ${e?.toString() || e}`)
  }
}

const isInverterOff = computed(() => {
  const s = state.value.inverter_state
  if (!s) return false
  const normalized = s.trim().toLowerCase()
  return normalized === 'off'
})

const essClass = computed(() => {
  const m = state.value.ess_mode
  if (m?.selection_supported) return m.vebus_mode === 3 ? 'on' : 'off'
  if (isInverterOff.value) return 'off'
  if (!m) return 'off'
  if (m.mode_name === 'Off' || m.mode_name === 'Charger only') return 'off'
  return 'on'
})

const essText = computed(() => {
  if (isInverterOff.value) return 'Off'
  const m = state.value.ess_mode
  if (!m) return 'ESS'
  if (m.mode_name === 'Off' || m.mode_name === 'Charger only') return m.mode_name
  if (m.is_external) return 'External'
  return m.mode_name || 'ESS'
})

const mpptTotal = computed(() => state.value.mppt_total || 0)
const pvInvertersTotal = computed(() => {
  const invs = state.value.pv_inverters
  if (invs?.length) {
    return invs.reduce((sum, p) => sum + (p.power || 0), 0)
  }
  // Fallback: legacy daemon publishes per-inverter power aggregates
  return (state.value.pv_inverter_individual || []).reduce((sum, p) => sum + (p || 0), 0)
})

const batteries = computed(() => {
  const tiles: Array<{
    name: string
    serial?: string
    instance?: number
    voltage: number
    current?: number
    power?: number
    soc: number
    state: string
    timeToGo?: string
  }> = []
  for (const b of state.value.batteries || []) {
    tiles.push({
      name: b.name || 'Battery',
      serial: b.serial,
      instance: b.instance,
      voltage: b.voltage || 0,
      current: b.current,
      power: b.power,
      soc: b.soc || 0,
      state: b.state || 'Unknown',
      timeToGo: b.time_to_go || '',
    })
  }
  return tiles
})

const solarSources = computed(() => {
  const sources: Array<{
    name: string
    serial?: string
    instance?: number
    pvVoltage?: number
    current?: number
    power: number
  }> = []
  ;(state.value.mppt_chargers || []).forEach((m) => {
    sources.push({
      name: m.name || 'MPPT',
      serial: m.serial,
      instance: m.instance,
      pvVoltage: m.pv_voltage || 0,
      current: m.current || 0,
      power: m.power || 0,
    })
  })
  const pvInvs = state.value.pv_inverters
  if (pvInvs?.length) {
    pvInvs.forEach((p, i) => {
      sources.push({
        name: p.name || `PV Inverter ${i + 1}`,
        serial: p.serial,
        instance: p.instance,
        pvVoltage: p.voltage,
        current: p.current,
        power: p.power || 0,
      })
    })
  } else if (state.value.pv_inverter_individual?.length) {
    // Fallback: daemon publishes per-inverter power aggregates as Vec<f64>
    state.value.pv_inverter_individual.forEach((power, i) => {
      sources.push({
        name: `PV Inverter ${i + 1}`,
        power,
      })
    })
  }
  return sources
})

function onDocumentClick() {
  closeContextMenu()
}

watch(
  () => isDark.value,
  () => {
    forceUpdateChart()
  }
)

watch(
  () => state.value,
  (newState) => {
    if (isWindowHidden.value || newState.cached_snapshot) return
    if (newState.gt !== undefined) addHistoryPoint(newState)
  },
  { deep: false }
)

onMounted(async () => {
  // Settings can be saved while the initial permission/config/MQTT probe awaits.
  // Subscribe first so useConnection can cancel that older startup session.
  if (
    !(await listenWhileMounted<{ color_scheme?: string }>('config-saved', async (event) => {
      configRevision += 1
      const scheme = event.payload.color_scheme
      if (scheme) {
        isDark.value = scheme !== 'light'
        document.documentElement.classList.toggle('dark', isDark.value)
        localStorage.setItem('theme', scheme)
      }
      await connectMqtt()
    }))
  )
    return
  const startupRevision = configRevision
  await ensureNotificationPermission()
  if (disposed) return
  notify('Inverter Desktop', 'App started')

  // Load configuration first to check auth status
  let cfg = appConfig.value
  if (!cfg) {
    try {
      cfg = await getAppConfig()
      if (disposed) return
      if (startupRevision === configRevision) appConfig.value = cfg
    } catch (e) {
      logger.warn('Failed to load config for auth check:', e)
    }
  }

  if (disposed) return
  // A save already started the newer session; an older config read must not
  // supersede it or reopen first-run setup after that save completed setup.
  if (startupRevision === configRevision) {
    showSetupWizard.value = needsSetup(cfg)
    if (!showSetupWizard.value) await connectMqtt()
  }

  if (disposed) return
  if (!showSetupWizard.value) await features.init()
  if (disposed) return
  initSystemNotifications(evChargingKw, waterValveState, pumpSwitchState)

  document.addEventListener('click', onDocumentClick)

  // Pause updates and charts when window is minimized/closed to tray
  if (
    !(await listenWhileMounted('window-hidden', () => {
      isWindowHidden.value = true
      setChartPaused(true)
      void setInverterWindowHidden(true)
      void features.setWindowHidden(true)
    }))
  )
    return
  await listenWhileMounted('window-shown', () => {
    isWindowHidden.value = false
    setChartPaused(false)
    void setInverterWindowHidden(false)
    void features.setWindowHidden(false)
  })
})

onUnmounted(() => {
  disposed = true
  document.removeEventListener('click', onDocumentClick)
  cleanupConnection()
  features.cleanup()
  cleanupInverterVisibility()
  unlisteners.forEach((unlisten) => {
    unlisten()
  })
})
</script>
