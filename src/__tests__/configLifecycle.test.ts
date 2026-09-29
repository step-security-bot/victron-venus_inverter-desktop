import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import Config from '../Config.vue'
import { defaultConfig, type AppConfig } from '../config'

const native = vi.hoisted(() => ({ invoke: vi.fn(), emit: vi.fn(), listen: vi.fn() }))
vi.mock('@tauri-apps/api/core', () => ({ invoke: native.invoke }))
vi.mock('@tauri-apps/api/event', () => ({ emit: native.emit, listen: native.listen }))
vi.mock('@tauri-apps/api/window', () => ({ getCurrentWindow: () => ({ close: vi.fn() }) }))
vi.mock('vue-i18n', () => ({ useI18n: () => ({ t: (key: string) => key }) }))
vi.mock('../logger', () => ({ logger: { log: vi.fn(), warn: vi.fn(), error: vi.fn() } }))

type Callback = (event: { payload: unknown }) => void
const featureEvent = 'plugin-configuration-changed'
let wrapper: VueWrapper | undefined
let callbacks: Map<string, Set<Callback>>
let stops: Array<ReturnType<typeof vi.fn>>
let registration: Promise<void> | undefined
let registrationFailures: number
let readConfiguration: () => Promise<AppConfig>
let restore: () => Promise<boolean>

function deferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((done) => {
    resolve = done
  })
  return { promise, resolve }
}

function listeners(name: string) {
  return callbacks.get(name)?.size ?? 0
}

function registrationCount() {
  return native.listen.mock.calls.filter(([name]) => name === featureEvent).length
}

async function click(text: string) {
  const button = wrapper?.findAll('button').find((item) => item.text() === text)
  expect(button, `Missing button: ${text}`).toBeDefined()
  await button?.trigger('click')
  await flushPromises()
}

async function open() {
  wrapper = mount(Config)
  await flushPromises()
  await click('Backup')
}

function unmount() {
  wrapper?.unmount()
  wrapper = undefined
}

beforeEach(() => {
  vi.useFakeTimers()
  callbacks = new Map()
  stops = []
  registration = undefined
  registrationFailures = 0
  readConfiguration = async () => ({ ...defaultConfig })
  restore = async () => true
  native.invoke.mockReset().mockImplementation(async (command: string) => {
    if (command === 'get_config') return readConfiguration()
    if (command === 'restore_config') return restore()
    if (command === 'get_state') return {}
    if (command === 'save_config' || command === 'set_auto_start') return undefined
    throw new Error(`Unexpected command: ${command}`)
  })
  native.emit.mockReset().mockResolvedValue(undefined)
  native.listen.mockReset().mockImplementation(async (name: string, callback: Callback) => {
    if (name === featureEvent) {
      if (registrationFailures > 0) {
        registrationFailures -= 1
        throw new Error('Event registration failed')
      }
      await registration
    }
    const subscribers = callbacks.get(name) ?? new Set<Callback>()
    callbacks.set(name, subscribers)
    subscribers.add(callback)
    const stop = vi.fn(() => subscribers.delete(callback))
    stops.push(stop)
    return stop
  })
})

afterEach(() => {
  unmount()
  vi.clearAllTimers()
  vi.useRealTimers()
})

describe('configuration event subscription lifecycle', () => {
  it('keeps restored settings and theme when an older initial read completes afterward', async () => {
    const initial = deferred<AppConfig>()
    let reads = 0
    readConfiguration = () =>
      ++reads === 1
        ? initial.promise
        : Promise.resolve({
            ...defaultConfig,
            mqtt_host: 'restored-host',
            color_scheme: 'light',
            show_active_loads: true,
          })
    await open()
    await click('Load Configuration')
    expect(wrapper?.text()).toContain('Configuration loaded')
    expect(document.documentElement.classList.contains('dark')).toBe(false)
    initial.resolve({ ...defaultConfig, mqtt_host: 'outdated-host', color_scheme: 'dark' })
    await flushPromises()
    await click('MQTT Broker')
    expect(wrapper?.get<HTMLInputElement>('#mqtt_host').element.value).toBe('restored-host')
    expect(document.documentElement.classList.contains('dark')).toBe(false)
    expect(wrapper?.text()).toContain('Configuration loaded')
    expect(registrationCount()).toBe(1)
    expect(native.emit).toHaveBeenCalledExactlyOnceWith('config-saved', { color_scheme: 'light' })
    expect(native.invoke.mock.calls.filter(([command]) => command === 'get_state')).toHaveLength(1)
    expect(listeners('mqtt-state-update')).toBe(1)
    for (const callback of callbacks.get('mqtt-state-update') ?? []) {
      callback({
        payload: {
          discovered_water_ev: [{ instance: 21, kind: 'tank', name: 'Restored water tank' }],
        },
      })
    }
    await click('Cerbo Devices')
    expect(wrapper?.text()).toContain('Restored water tank')
  })

  it('keeps one subscription across repeated restores and releases every listener on unmount', async () => {
    await open()
    expect(listeners(featureEvent)).toBe(1)
    for (const mqtt_host of ['restored-first', 'restored-second']) {
      readConfiguration = async () => ({ ...defaultConfig, mqtt_host })
      await click('Load Configuration')
      expect(wrapper?.text()).toContain('Configuration loaded')
      expect(registrationCount()).toBe(1)
      expect(listeners(featureEvent)).toBe(1)
    }
    const declarations = [
      { plugin_id: 'example.camera', version: '1.0.0', enabled: false, artifacts: {} },
    ]
    for (const callback of callbacks.get(featureEvent) ?? []) {
      callback({ payload: { desktop_plugins: declarations } })
    }
    await wrapper?.get('button[title="Save changes"]').trigger('click')
    await flushPromises()
    expect(native.invoke).toHaveBeenCalledWith('save_config', {
      config: expect.objectContaining({
        mqtt_host: 'restored-second',
        desktop_plugins: declarations,
      }),
    })
    unmount()
    expect([...callbacks.values()].every((set) => set.size === 0)).toBe(true)
    expect(stops).toHaveLength(2)
    for (const stop of stops) expect(stop).toHaveBeenCalledOnce()
  })

  it('shares pending startup registration with an overlapping restore', async () => {
    const pending = deferred<void>()
    registration = pending.promise
    await open()
    await click('Load Configuration')
    expect(registrationCount()).toBe(1)
    pending.resolve()
    await flushPromises()
    expect(listeners(featureEvent)).toBe(1)
    expect(wrapper?.text()).toContain('Configuration loaded')
    unmount()
    expect(listeners(featureEvent)).toBe(0)
    for (const stop of stops) expect(stop).toHaveBeenCalledOnce()
  })

  it('retries a failed initial registration on restore without duplicating other listeners', async () => {
    registrationFailures = 1
    await open()
    expect(listeners(featureEvent)).toBe(0)
    expect(listeners('mqtt-state-update')).toBe(1)
    await click('Load Configuration')
    expect(registrationCount()).toBe(2)
    expect(listeners(featureEvent)).toBe(1)
    expect(listeners('mqtt-state-update')).toBe(1)
    unmount()
    expect([...callbacks.values()].every((set) => set.size === 0)).toBe(true)
  })

  it('immediately releases registration that completes after the settings window unmounts', async () => {
    const pending = deferred<void>()
    registration = pending.promise
    await open()
    unmount()
    pending.resolve()
    await flushPromises()
    expect(listeners(featureEvent)).toBe(0)
    expect(stops).toHaveLength(1)
    expect(stops[0]).toHaveBeenCalledOnce()
    expect(native.invoke).not.toHaveBeenCalledWith('get_state')
  })

  it('does not restart config work after a restore completes in a closed window', async () => {
    const pending = deferred<boolean>()
    restore = () => pending.promise
    await open()
    await click('Load Configuration')
    unmount()
    native.invoke.mockClear()
    pending.resolve(true)
    await flushPromises()
    expect(native.invoke).not.toHaveBeenCalled()
    expect(native.emit).not.toHaveBeenCalled()
    expect(registrationCount()).toBe(1)
    expect([...callbacks.values()].every((set) => set.size === 0)).toBe(true)
    expect(vi.getTimerCount()).toBe(0)
  })
})
