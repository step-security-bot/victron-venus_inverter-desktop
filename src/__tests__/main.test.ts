import { flushPromises } from '@vue/test-utils'
import { defineComponent, h, nextTick, type App } from 'vue'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const native = vi.hoisted(() => ({ invoke: vi.fn(), listen: vi.fn() }))
vi.mock('@tauri-apps/api/core', () => ({ invoke: native.invoke }))
vi.mock('@tauri-apps/api/event', () => ({ listen: native.listen }))

const initialLocation = globalThis.location.href
let app: App | undefined
let rootLoads: string[]
let authChanged: () => void
let stopListening: ReturnType<typeof vi.fn>
let logError: ReturnType<typeof vi.fn>
let mobile: boolean
let unavailablePage: string | null

function page(name: string) {
  return defineComponent({
    setup: () => () => h('div', { 'data-page': name }, name),
  })
}

function rootModule(name: string) {
  rootLoads.push(name)
  if (name === unavailablePage) throw new Error('Page chunk unavailable')
  return { __esModule: true, default: page(name) }
}

async function settle() {
  await flushPromises()
  await vi.dynamicImportSettled()
  await flushPromises()
  await nextTick()
}

async function boot(path: string, mobileProfile = false) {
  mobile = mobileProfile
  globalThis.history.replaceState({}, '', path)
  await import('../main')
  await settle()
}

beforeEach(() => {
  vi.resetModules()
  rootLoads = []
  mobile = false
  unavailablePage = null
  document.body.innerHTML = '<div id="app"></div>'
  delete document.documentElement.dataset.appProfile
  stopListening = vi.fn()
  logError = vi.fn()
  native.invoke.mockReset().mockResolvedValue({ unlocked: true })
  native.listen.mockReset().mockImplementation(async (_event: string, callback: () => void) => {
    authChanged = callback
    return stopListening
  })
  // Keep the real createApp, AuthGate and async-component lifecycle. Only the
  // page bodies and native services are replaced, so imports are observable.
  vi.doMock('@features', () => ({
    isMobileApp: mobile,
    getFeatureView: (path: string) => (path === '/camera-video' ? page('camera') : undefined),
  }))
  vi.doMock('../i18n', () => ({
    i18n: {
      install(instance: App) {
        app = instance
      },
    },
  }))
  vi.doMock('../logger', () => ({ logger: { error: logError } }))
  vi.doMock('../components/AuthScreen.vue', () => ({ default: page('auth') }))
  vi.doMock('../App.vue', () => rootModule('dashboard'))
  vi.doMock('../Config.vue', () => rootModule('config'))
  vi.doMock('../About.vue', () => rootModule('about'))
  vi.doMock('../MobileShell.vue', () => rootModule('mobile'))
})

afterEach(() => {
  app?.unmount()
  app = undefined
  document.body.innerHTML = ''
  delete document.documentElement.dataset.appProfile
  globalThis.history.replaceState({}, '', initialLocation)
  vi.restoreAllMocks()
})

describe('application entrypoint', () => {
  it('mounts a camera without loading the dashboard, configuration, about or mobile pages', async () => {
    await boot('/camera-video?pluginMedia=opaque')
    expect(document.querySelector('[data-page="camera"]')).not.toBeNull()
    expect(rootLoads).toEqual([])
    expect(document.documentElement.dataset.appProfile).toBe('desktop')
    expect(native.invoke).toHaveBeenCalledExactlyOnceWith('auth_status')
    expect(logError).not.toHaveBeenCalled()
  })

  it.each([
    ['/', false, 'dashboard'],
    ['/config', false, 'config'],
    ['/about', false, 'about'],
    ['/', true, 'mobile'],
    ['/camera-video', true, 'mobile'],
  ])('selects route %s in mobile=%s as %s', async (path, mobileProfile, selected) => {
    await boot(path, mobileProfile)
    expect(rootLoads).toEqual([selected])
    expect(logError.mock.calls).toEqual([])
    expect(document.querySelector(`[data-page="${selected}"]`)).not.toBeNull()
    expect(document.documentElement.dataset.appProfile).toBe(mobileProfile ? 'mobile' : 'desktop')
    expect(logError).not.toHaveBeenCalled()
  })

  it.each([
    ['/camera-video', 'camera'],
    ['/config', 'config'],
  ])('protects %s while authentication is pending or revoked', async (path, selected) => {
    let resolveStatus!: (status: { unlocked: boolean }) => void
    native.invoke.mockReturnValue(
      new Promise((resolve) => {
        resolveStatus = resolve
      })
    )
    await boot(path)
    expect(document.body.textContent).toContain('Opening secure settings')
    expect(document.querySelector(`[data-page="${selected}"]`)).toBeNull()
    expect(rootLoads).toEqual([])

    resolveStatus({ unlocked: false })
    await settle()
    expect(document.querySelector('[data-page="auth"]')).not.toBeNull()
    expect(rootLoads).toEqual([])

    native.invoke.mockResolvedValue({ unlocked: true })
    authChanged()
    await settle()
    expect(document.querySelector(`[data-page="${selected}"]`)).not.toBeNull()
    expect(rootLoads).toEqual(selected === 'camera' ? [] : ['config'])

    native.invoke.mockResolvedValue({ unlocked: false })
    authChanged()
    await settle()
    expect(document.querySelector(`[data-page="${selected}"]`)).toBeNull()
    expect(document.querySelector('[data-page="auth"]')).not.toBeNull()
    app?.unmount()
    app = undefined
    expect(stopListening).toHaveBeenCalledOnce()
  })

  it('shows recovery controls for a failed page import only after authentication', async () => {
    // Set the behavior of the existing factory. Queuing another doMock for the
    // same unresolved module can race its registration from beforeEach.
    unavailablePage = 'config'
    native.invoke.mockResolvedValue({ unlocked: false })
    await boot('/config')
    expect(rootLoads).toEqual([])
    expect(document.querySelector('[data-page="auth"]')).not.toBeNull()
    expect(document.body.textContent).not.toContain('Something went wrong')

    native.invoke.mockResolvedValue({ unlocked: true })
    authChanged()
    await settle()
    expect(rootLoads).toEqual(['config'])
    expect(document.body.textContent).toContain('Something went wrong')
    expect([...document.querySelectorAll('button')].map((button) => button.textContent)).toEqual([
      'Try Again',
      'Reload App',
    ])
    expect(logError).toHaveBeenCalledWith(
      'ErrorBoundary caught:',
      expect.any(Error),
      expect.any(String)
    )

    native.invoke.mockResolvedValue({ unlocked: false })
    authChanged()
    await settle()
    expect(document.querySelector('[data-page="auth"]')).not.toBeNull()
    expect(document.body.textContent).not.toContain('Something went wrong')
  })
})
