import { flushPromises, mount } from '@vue/test-utils'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { defineComponent, h } from 'vue'
import {
  featureConnection,
  FeaturePluginManager,
  featurePluginManagerTabId,
  featureSetupAvailable,
  getFeatureView,
  useDashboardFeatures,
} from '@features'
import Config from '../Config.vue'
import AppHeader from '../components/AppHeader.vue'
import StatCards from '../components/StatCards.vue'
import SetupWizard from '../components/SetupWizard.vue'
import { useDashboardControls, sendControlAction } from '../composables/useDashboardControls'
import { useConnection } from '../composables/useConnection'
import { appConfig, state } from '../composables/useInverterState'
import { defaultConfig } from '../config'

const { invoke, listen, emit } = vi.hoisted(() => ({
  invoke: vi.fn(),
  listen: vi.fn(),
  emit: vi.fn(),
}))
vi.mock('@tauri-apps/api/core', () => ({ invoke }))
vi.mock('@tauri-apps/api/event', () => ({ listen, emit }))
vi.mock('@tauri-apps/api/window', () => ({ getCurrentWindow: () => ({ close: vi.fn() }) }))
vi.mock('@tauri-apps/plugin-notification', () => ({
  isPermissionGranted: vi.fn(async () => true),
  requestPermission: vi.fn(async () => 'granted'),
}))
vi.mock('vue-i18n', () => ({ useI18n: () => ({ t: (key: string) => key }) }))

const config = {
  mqtt_host: 'Cerbo',
  mqtt_port: 1883,
  setup_completed: true,
  ha_url: 'http://saved-ha',
  ha_longlived_token: 'saved-token',
  ha_use_direct_api: true,
  mqtt_ha_host: 'camera-broker',
  mqtt_ha_port: 1883,
  camera_enabled: true,
  desktop_plugins: [
    {
      plugin_id: 'example.monitor',
      version: '1.2.3',
      enabled: true,
      artifacts: {
        'aarch64-apple-darwin': {
          url: 'https://packages.example.invalid/monitor.idplugin',
          sha256: 'a'.repeat(64),
        },
      },
    },
  ],
  header_toggles_config: [
    { id: 'limit', label: 'Export limit', entity: 'no_feed', state_key: 'no_feed' },
    {
      id: 'lamp',
      label: 'Living room lamp',
      entity: 'switch.lamp',
      state_key: 'home_lamp',
      future_field: true,
    },
  ],
  ha_entities: [
    {
      id: 'lamp',
      label: 'Living room lamp',
      entity: 'switch.lamp',
      domain: 'switch',
      enabled: true,
    },
  ],
}
const globalOptions = { mocks: { $t: (key: string) => key } }

beforeEach(() => {
  vi.stubGlobal('localStorage', {
    getItem: vi.fn(() => null),
    setItem: vi.fn(),
    removeItem: vi.fn(),
  })
  invoke.mockReset()
  listen.mockReset()
  emit.mockReset()
  listen.mockResolvedValue(() => {})
  invoke.mockImplementation(async (command: string) =>
    command === 'get_config' ? structuredClone(config) : command === 'get_state' ? {} : undefined
  )
  appConfig.value = { ...defaultConfig, ...structuredClone(config) }
  state.value = {
    booleans: { no_feed: true },
    ui_config: { header_toggles: [{ id: 'limit', label: 'Export limit', entity: 'no_feed' }] },
    features: { ha: true },
  }
})

describe('mobile build feature boundary', () => {
  it('uses the mobile port with no feature routes or transport calls', async () => {
    expect(featureSetupAvailable).toBe(false)
    expect(featurePluginManagerTabId).toBeUndefined()
    const manager = mount(FeaturePluginManager)
    expect(manager.text()).toBe('')
    manager.unmount()
    expect(getFeatureView('/camera-video')).toBeUndefined()
    await featureConnection.connect(config)
    featureConnection.cleanup()
    expect(invoke).not.toHaveBeenCalled()
    expect(listen).not.toHaveBeenCalled()
  })

  it('renders and dispatches core inverter controls while hiding stored external controls', async () => {
    const Harness = defineComponent({
      setup() {
        const features = useDashboardFeatures()
        const controls = useDashboardControls(features.getControlState, features.allowHomeControls)
        return () =>
          h(AppHeader, {
            dryRun: false,
            essClass: 'on',
            essText: 'ESS',
            isDark: true,
            showHeaderToggles: true,
            headerControls: controls.headerControls.value,
            controlStates: controls.headerControlStates.value,
            onSend: sendControlAction,
          })
      },
    })
    const wrapper = mount(Harness, { global: globalOptions, attachTo: document.body })
    expect(wrapper.text()).toContain('Export limit')
    expect(wrapper.text()).not.toContain('Living room lamp')
    const disclosure = wrapper.find('button[aria-expanded]')
    expect(disclosure.attributes('aria-label')).toBe('Controls')
    expect(disclosure.text()).toBe('Controls')
    expect(disclosure.attributes('aria-expanded')).toBe('false')
    const button = wrapper.findAll('button').find((entry) => entry.text() === 'Export limit')!
    expect(button.isVisible()).toBe(false)
    await disclosure.trigger('click')
    expect(disclosure.attributes('aria-expanded')).toBe('true')
    expect(button.isVisible()).toBe(true)
    expect(invoke).not.toHaveBeenCalled()
    await button.trigger('click')
    expect(invoke).toHaveBeenCalledWith('perform_action', {
      action: 'toggle',
      payload: { entity: 'no_feed' },
    })
    const dryRunButton = wrapper.findAll('button').find((entry) => entry.text() === 'DRY')!
    await dryRunButton.trigger('click')
    expect(invoke).toHaveBeenCalledWith('perform_action', {
      action: 'dry_run',
      payload: { value: true },
    })
    wrapper.unmount()
  })

  it('keeps core header actions available while expanding and collapsing long control labels', async () => {
    const longEss = 'Charger only with an extended operating status'
    const longLabel = 'Allow battery charging from available surplus solar generation'
    const wrapper = mount(AppHeader, {
      attachTo: document.body,
      props: {
        dryRun: true,
        essClass: 'on',
        essText: longEss,
        headerControls: [{ id: 'charge', label: longLabel, entity: 'charge_battery' }],
        controlStates: { charge: 'on' },
        isDark: true,
        showHeaderToggles: true,
      },
    })
    try {
      const row = wrapper.find('.mobile-header-row')
      const disclosure = row.find('button[aria-expanded]')
      const region = wrapper.find('fieldset[aria-label="Inverter controls"]')
      expect(disclosure.attributes('aria-controls')).toBe(region.attributes('id'))
      expect(region.isVisible()).toBe(false)
      expect(row.findAll('button')).toHaveLength(5)
      await disclosure.trigger('click')
      expect(region.isVisible()).toBe(true)
      expect(region.text()).toBe(longLabel)
      expect(wrapper.emitted('send')).toBeUndefined()
      expect(row.findAll('button').every((button) => button.isVisible())).toBe(true)

      await row.find('button[aria-label="Settings"]').trigger('click')
      await row.find('button[aria-label="Light mode"]').trigger('click')
      expect(wrapper.emitted('open-config')).toEqual([[]])
      expect(wrapper.emitted('toggle-theme')).toEqual([[]])
      expect(wrapper.emitted('send')).toBeUndefined()
      await row
        .findAll('button')
        .find((button) => button.text() === longEss)!
        .trigger('click')
      await row
        .findAll('button')
        .find((button) => button.text() === 'DRY')!
        .trigger('click')
      expect(wrapper.emitted('send')).toEqual([['ess_mode'], ['dry_run', { value: false }]])

      await disclosure.trigger('click')
      expect(disclosure.attributes('aria-expanded')).toBe('false')
      expect(region.isVisible()).toBe(false)
      expect(wrapper.emitted('send')).toHaveLength(2)
    } finally {
      wrapper.unmount()
    }
  })

  it('keeps published controls reachable when the saved desktop header preference is false', async () => {
    const wrapper = mount(AppHeader, {
      props: {
        dryRun: false,
        essClass: 'off',
        essText: 'ESS',
        headerControls: [],
        controlStates: { minimize: 'off' },
        isDark: false,
        showHeaderToggles: false,
      },
    })
    try {
      expect(wrapper.find('button[aria-expanded]').exists()).toBe(false)
      expect(wrapper.find('fieldset').exists()).toBe(false)
      expect(wrapper.find('.mobile-header-row').findAll('button')).toHaveLength(4)
      // A fresh controller snapshot can arrive after initial configuration.
      await wrapper.setProps({
        headerControls: [
          { id: 'minimize', label: 'Minimize charging', entity: 'minimize_charging' },
        ],
      })
      const disclosure = wrapper.get('button[aria-label="Controls"]')
      expect(disclosure.text()).toBe('Controls')
      expect(disclosure.attributes('aria-expanded')).toBe('false')
      await disclosure.trigger('click')
      const control = wrapper
        .findAll('button')
        .find((button) => button.text() === 'Minimize charging')!
      expect(control.isVisible()).toBe(true)
      expect(wrapper.emitted('send')).toBeUndefined()
      await control.trigger('click')
      expect(wrapper.emitted('send')).toEqual([['toggle', { entity: 'minimize_charging' }]])
      // Withdrawing the published control also withdraws the rendered action.
      await wrapper.setProps({ headerControls: [] })
      expect(wrapper.find('button[aria-expanded]').exists()).toBe(false)
      expect(wrapper.find('fieldset').exists()).toBe(false)
      expect(wrapper.find('button[aria-label="Dark mode"]').exists()).toBe(true)
      expect(wrapper.emitted('send')).toHaveLength(1)
    } finally {
      wrapper.unmount()
    }
  })

  it('starts and stops the core setpoint override from mobile StatCards without optional services', async () => {
    let override: number | null = null
    invoke.mockImplementation(async (command: string, args?: { value: number | null }) => {
      if (command === 'set_setpoint_override') override = args!.value
      return { value: override, last_error: null }
    })
    const wrapper = mount(StatCards, {
      props: { mpptTotal: 0, pvInvertersTotal: 0, setpoint: 125 },
      global: { ...globalOptions, stubs: { Teleport: true } },
    })
    try {
      await flushPromises()
      expect(featureSetupAvailable).toBe(false)
      expect(listen).toHaveBeenCalledWith('setpoint-override-update', expect.any(Function))
      expect(invoke).toHaveBeenCalledWith('get_setpoint_override')
      await wrapper.find('button[aria-label="Setpoint override"]').trigger('click')
      expect((wrapper.find('dialog input').element as HTMLInputElement).value).toBe('125')
      await wrapper.find('dialog input').setValue('-250')
      await wrapper.find('dialog form').trigger('submit')
      await flushPromises()
      expect(invoke).toHaveBeenCalledWith('set_setpoint_override', { value: -250 })
      expect(wrapper.find('output').text()).toContain('-250 W')
      await wrapper.find('button[aria-label="Setpoint override"]').trigger('click')
      const stop = wrapper.findAll('button').find((button) => button.text() === 'Stop override')!
      await stop.trigger('click')
      await flushPromises()
      expect(invoke).toHaveBeenCalledWith('set_setpoint_override', { value: null })
      expect(wrapper.find('output').exists()).toBe(false)
      expect(
        invoke.mock.calls.every(([command]) =>
          ['get_setpoint_override', 'set_setpoint_override'].includes(command)
        )
      ).toBe(true)
      expect(
        listen.mock.calls.every(([name]) =>
          ['setpoint-override-update', 'mqtt-connection-status'].includes(name)
        )
      ).toBe(true)
    } finally {
      wrapper.unmount()
    }
  })

  it('starts the shared connection without optional-service commands or listeners', async () => {
    const connection = useConnection()
    await connection.connectMqtt()
    expect(invoke).toHaveBeenCalledWith('connect_mqtt', expect.objectContaining({ host: 'Cerbo' }))
    expect(
      invoke.mock.calls
        .map(([command]) => command)
        .some((command) => /ha|camera|plugin/.test(command))
    ).toBe(false)
    expect(
      listen.mock.calls.map(([name]) => name).some((name) => /ha-|camera|plugin/.test(name))
    ).toBe(false)
    connection.cleanup()
  })

  it('shows core setup and settings and preserves hidden definitions when saving the shared form', async () => {
    const setup = mount(SetupWizard, { global: globalOptions })
    await flushPromises()
    expect(setup.text()).toContain('Cerbo MQTT')
    expect(setup.text()).not.toContain('Advanced')
    expect(setup.find('#setup_ha_url').exists()).toBe(false)
    setup.unmount()

    const wrapper = mount(Config, { global: globalOptions })
    await flushPromises()
    expect(wrapper.text()).toContain('Cerbo Devices')
    expect(wrapper.text()).not.toContain('Home Assistant')
    expect(wrapper.text()).not.toContain('Plugins')
    expect(wrapper.text()).not.toContain('plugins.manager')
    expect(wrapper.text()).not.toContain('Stored plugin data')
    expect(wrapper.text()).not.toContain('Unidentified stored data')
    expect(wrapper.find('input[name="delete-plugin-settings"]').exists()).toBe(false)
    expect(wrapper.find('input[type="password"][autocomplete="new-password"]').exists()).toBe(false)
    expect(wrapper.find('#ha_url').exists()).toBe(false)
    const devicesTab = wrapper.findAll('button').find((entry) => entry.text() === 'Cerbo Devices')!
    await devicesTab.trigger('click')
    expect(wrapper.text()).toContain('Water tanks')
    expect(wrapper.find('#water_tank_instance').exists()).toBe(true)
    const mqttTab = wrapper.findAll('button').find((entry) => entry.text() === 'MQTT Broker')!
    await mqttTab.trigger('click')
    await wrapper.find('#mqtt_host').setValue('NewCerbo')
    await wrapper.find('button[title="Save changes"]').trigger('click')
    await flushPromises()
    const saved = invoke.mock.calls
      .filter(([command]) => command === 'save_config')
      .slice(-1)[0]?.[1].config
    expect(saved.mqtt_host).toBe('NewCerbo')
    expect(saved.header_toggles_config[1]).toEqual(config.header_toggles_config[1])
    expect(saved.ha_entities).toEqual(config.ha_entities)
    expect(saved.ha_url).toBe(config.ha_url)
    expect(saved.desktop_plugins).toEqual(config.desktop_plugins)
    expect(invoke.mock.calls.some(([command]) => /plugin_settings/.test(command))).toBe(false)
    expect(invoke.mock.calls.some(([command]) => /retained_plugin_data/.test(command))).toBe(false)
    wrapper.unmount()
  })
})
