import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import { effectScope } from 'vue'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import Config from '../Config.vue'
import { useConfigForm } from '../composables/useConfigForm'
import { defaultConfig, getAppConfig } from '../config'

const boundary = vi.hoisted(() => ({ invoke: vi.fn(), emit: vi.fn() }))
vi.mock('@tauri-apps/api/core', () => ({ invoke: boundary.invoke }))
vi.mock('@tauri-apps/api/event', () => ({
  emit: boundary.emit,
  listen: vi.fn(async () => () => {}),
}))
vi.mock('@tauri-apps/api/window', () => ({ getCurrentWindow: () => ({ close: vi.fn() }) }))
vi.mock('vue-i18n', () => ({ useI18n: () => ({ t: (key: string) => key }) }))
let wrapper: VueWrapper | undefined
beforeEach(() => {
  boundary.invoke.mockReset().mockImplementation(async (name: string) => {
    if (name === 'get_config') return { ...defaultConfig }
    if (name === 'get_state') return {}
    return undefined
  })
  boundary.emit.mockReset().mockResolvedValue(undefined)
})
afterEach(() => {
  wrapper?.unmount()
  wrapper = undefined
})

describe('Configuration save result', () => {
  it.each(['resolve', 'reject'] as const)(
    'ignores a superseded config read that later %s instead of overwriting current sections or messages',
    async (outcome) => {
      let resolveOld!: (config: typeof defaultConfig) => void
      let rejectOld!: (error: Error) => void
      boundary.invoke.mockImplementationOnce(
        () =>
          new Promise((resolve, reject) => {
            resolveOld = resolve
            rejectOld = reject
          })
      )
      const form = useConfigForm()
      const oldRead = form.loadConfig()
      boundary.invoke.mockResolvedValueOnce({
        ...defaultConfig,
        mqtt_host: 'restored-host',
        color_scheme: 'light',
        show_active_loads: true,
      })
      expect(await form.loadConfig()).toBe(form.config)
      form.message.value = 'Newest operation result'
      if (outcome === 'resolve') resolveOld({ ...defaultConfig, mqtt_host: 'outdated-host' })
      else rejectOld(new Error('Old load failed'))
      expect(await oldRead).toBeNull()
      expect(form.config).toMatchObject({
        mqtt_host: 'restored-host',
        color_scheme: 'light',
        show_active_loads: true,
      })
      expect(form.configLoaded.value).toBe(true)
      expect(form.message.value).toBe('Newest operation result')
    }
  )

  it('discards an in-flight config read when its component scope is disposed', async () => {
    let resolveRead!: (config: typeof defaultConfig) => void
    boundary.invoke.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveRead = resolve
        })
    )
    const scope = effectScope()
    const form = scope.run(useConfigForm)
    if (!form) throw new Error('Config scope was not active')
    const reading = form.loadConfig()
    scope.stop()
    resolveRead({ ...defaultConfig, mqtt_host: 'late-host' })
    expect(await reading).toBeNull()
    expect(form.config.mqtt_host).toBe(defaultConfig.mqtt_host)
    expect(form.configLoaded.value).toBe(false)
    boundary.invoke.mockClear()
    expect(await form.loadConfig()).toBeNull()
    expect(boundary.invoke).not.toHaveBeenCalled()
  })

  it('preserves public opaque module schemas during core load, reset and save', async () => {
    const modules = {
      'example.future': {
        schema_version: 407,
        values: { layout: [{ kind: 'new-kind', details: { nested: [null, false, 2.5] } }] },
      },
    }
    boundary.invoke.mockImplementation(async (name: string) =>
      name === 'get_config' ? { ...defaultConfig, modules: structuredClone(modules) } : undefined
    )
    expect((await getAppConfig()).modules).toEqual(modules)
    const form = useConfigForm()
    await form.loadConfig()
    form.config.show_batteries = false
    form.resetToDefaults()
    expect(form.config.modules).toEqual(modules)
    expect(await form.saveConfig()).toBe(true)
    const written = boundary.invoke.mock.calls.find(([name]) => name === 'save_config')?.[1].config
    expect(written.modules).toEqual(modules)
    expect(boundary.invoke.mock.calls.map(([name]) => name)).toEqual([
      'get_config',
      'get_config',
      'save_config',
    ])
  })

  it('disables Save and keeps the load failure visible instead of saving defaults', async () => {
    boundary.invoke.mockRejectedValue(new Error('Cannot decrypt configuration'))
    wrapper = mount(Config)
    await flushPromises()
    expect(wrapper.text()).toContain('Failed to load config: Error: Cannot decrypt configuration')
    const save = wrapper.get('button[title="Save changes"]')
    expect(save.attributes('disabled')).toBeDefined()
    await save.trigger('click')
    expect(boundary.invoke).not.toHaveBeenCalledWith('save_config', expect.anything())
    expect(boundary.emit).not.toHaveBeenCalled()
  })

  it('rejects failed loads and guards saves until a later load succeeds', async () => {
    const form = useConfigForm()
    boundary.invoke.mockRejectedValueOnce(new Error('Read denied'))
    await expect(form.loadConfig()).rejects.toThrow('Read denied')
    expect(form.configLoaded.value).toBe(false)
    expect(await form.saveConfig()).toBe(false)
    expect(boundary.invoke).not.toHaveBeenCalledWith('save_config', expect.anything())
    await form.loadConfig()
    expect(form.configLoaded.value).toBe(true)
    expect(await form.saveConfig()).toBe(true)
  })

  it('keeps the write error visible and does not publish settings or change autostart', async () => {
    wrapper = mount(Config)
    await flushPromises()
    boundary.invoke.mockImplementation(async (name: string) => {
      if (name === 'save_config') throw new Error('Permission denied')
    })
    await wrapper.get('button[title="Save changes"]').trigger('click')
    await flushPromises()
    expect(wrapper.text()).toContain('Failed to save config: Error: Permission denied')
    expect(wrapper.text()).not.toContain('Settings saved successfully')
    expect(boundary.invoke).not.toHaveBeenCalledWith('set_auto_start', expect.anything())
    expect(boundary.emit).not.toHaveBeenCalledWith('config-saved', expect.anything())
    expect(wrapper.get('button[title="Save changes"]').attributes('disabled')).toBeUndefined()
  })

  it('publishes config-saved and applies autostart only after durable save succeeds', async () => {
    wrapper = mount(Config)
    await flushPromises()
    await wrapper.get('button[title="Save changes"]').trigger('click')
    await flushPromises()
    expect(wrapper.text()).toContain('Settings saved successfully')
    expect(boundary.emit).toHaveBeenCalledWith('config-saved', {
      color_scheme: defaultConfig.color_scheme,
    })
    const commands = boundary.invoke.mock.calls.map(([command]) => command)
    expect(commands.indexOf('set_auto_start')).toBeGreaterThan(commands.indexOf('save_config'))
  })
})
