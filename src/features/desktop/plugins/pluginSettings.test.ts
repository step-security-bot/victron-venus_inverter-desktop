import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createI18n } from 'vue-i18n'
import homeAssistantManifest from '../../../../scripts/plugins/home-assistant-manifest.json'
import en from '../messages.en'
import ru from '../messages.ru'
import PluginSettingsEditor from './PluginSettingsEditor.vue'
import type {
  JsonEditorSchema,
  PluginSettingsField,
  PluginSettingsSaveResult,
  PluginSettingsView,
} from './types'
import { createPluginSettings } from './usePluginSettings'

const native = vi.hoisted(() => ({ invoke: vi.fn() }))
vi.mock('@tauri-apps/api/core', () => ({ invoke: native.invoke }))

function field(key: string, overrides: Partial<PluginSettingsField> = {}): PluginSettingsField {
  return {
    key,
    title: key,
    description: null,
    type: 'string',
    required: false,
    secret: false,
    enum: null,
    minimum: null,
    maximum: null,
    min_length: null,
    max_length: null,
    ...overrides,
  }
}

let view: PluginSettingsView
let wrapper: VueWrapper | undefined
const controllers: Array<ReturnType<typeof createPluginSettings>> = []

function deferred<T>() {
  let resolve!: (value: T) => void
  let reject!: (value: unknown) => void
  const promise = new Promise<T>((done, fail) => {
    resolve = done
    reject = fail
  })
  return { promise, resolve, reject }
}

function controller() {
  const value = createPluginSettings(view.plugin_id, view.version, () => 'Settings unavailable')
  controllers.push(value)
  return value
}

async function open(locale = 'en') {
  wrapper = mount(PluginSettingsEditor, {
    props: { pluginId: view.plugin_id, version: view.version, editorKey: 3 },
    global: {
      plugins: [createI18n({ legacy: false, locale, fallbackLocale: 'en', messages: { en, ru } })],
    },
  })
  await flushPromises()
  return wrapper
}

function button(label: string) {
  const found = wrapper?.findAll('button').find((item) => item.text() === label)
  if (!found) throw new Error(`Missing button: ${label}`)
  return found
}

function saves() {
  return native.invoke.mock.calls.filter(([command]) => command === 'save_plugin_settings')
}

beforeEach(() => {
  view = {
    plugin_id: 'example.monitor',
    version: '2.0.0',
    revision: 'revision-1',
    fields: [
      field('host', { title: 'Server', required: true, min_length: 1, max_length: 256 }),
      field('enabled', { type: 'boolean', title: 'Enable events' }),
      field('port', { type: 'integer', minimum: 1, maximum: 65535 }),
      field('threshold', { type: 'number', minimum: 0, maximum: 1 }),
      field('provider', { enum: ['kerberos', 'frigate'] }),
      field('token', { secret: true, max_length: 4096 }),
      field('password', { secret: true }),
    ],
    values: { host: 'home.local', enabled: true, port: 1883, threshold: 0.5, provider: 'kerberos' },
    secret_present: { token: true, password: true },
  }
  native.invoke.mockReset().mockImplementation(async (command: string, args) => {
    if (command === 'get_plugin_settings') return structuredClone(view)
    if (command === 'save_plugin_settings') {
      return {
        settings: { ...structuredClone(view), revision: 'revision-2', values: args.values },
        restart_error: null,
      }
    }
    throw new Error(`Unexpected IPC: ${command}`)
  })
})

afterEach(() => {
  wrapper?.unmount()
  wrapper = undefined
  for (const value of controllers) value.stop()
  controllers.length = 0
})

it('saves the host video window preference as a boolean without changing secrets', async () => {
  const key = '_host_video_always_on_top'
  view.fields.push(field(key, { type: 'boolean', title: 'Always on top' }))
  view.values[key] = false
  const editor = await open()
  const checkbox = editor.get(`input[name="${key}"]`)
  expect((checkbox.element as HTMLInputElement).checked).toBe(false)
  await checkbox.setValue(true)
  await editor.get('form').trigger('submit')
  await flushPromises()
  expect(saves()[0][1]).toMatchObject({
    pluginId: view.plugin_id,
    revision: 'revision-1',
    values: { [key]: true },
    secretChanges: {},
  })
  expect((editor.get(`input[name="${key}"]`).element as HTMLInputElement).checked).toBe(true)
  await editor.get(`input[name="${key}"]`).setValue(false)
  await editor.get('form').trigger('submit')
  await flushPromises()
  expect(saves()[1][1]).toMatchObject({
    revision: 'revision-2',
    values: { [key]: false },
    secretChanges: {},
  })
})

describe('desktop plugin settings', () => {
  it('renders labeled native descriptors as text and never fills saved secret inputs', async () => {
    view.fields[0].title = '<img src=x onerror=alert(1)>'
    view.fields[0].description = '<script>metadata</script>'
    view.values.token = 'unexpected-native-secret'
    const mounted = await open()
    expect(native.invoke).toHaveBeenCalledWith('get_plugin_settings', { pluginId: view.plugin_id })
    expect(mounted.text()).toContain('<img src=x onerror=alert(1)>')
    expect(mounted.text()).toContain('<script>metadata</script>')
    expect(mounted.find('img, script').exists()).toBe(false)
    expect(mounted.html()).not.toContain('unexpected-native-secret')
    const host = mounted.find('input[name="host"]')
    expect(mounted.find(`label[for="${host.attributes('id')}"]`).exists()).toBe(true)
    expect(host.attributes('required')).toBeDefined()
    expect(host.attributes('maxlength')).toBe('512')
    expect(host.attributes('minlength')).toBeUndefined()
    expect(mounted.find('input[name="port"]').attributes('step')).toBe('1')
    expect(mounted.find('input[name="threshold"]').attributes('step')).toBe('any')
    const password = mounted.find('input[name="token"]')
    expect(password.attributes('type')).toBe('password')
    expect((password.element as HTMLInputElement).value).toBe('')
    expect(mounted.text()).toContain('A secret is stored.')
    expect(saves()).toHaveLength(0)
  })

  it('accepts complete 64-entity fields and expanded secrets within the native scalar bound', async () => {
    const entities = Array.from(
      { length: 64 },
      (_, index) => `sensor.${String(index).padStart(2, '0')}${'x'.repeat(119)}`
    ).join(',')
    expect(entities).toHaveLength(8255)
    view.fields[0].max_length = 8255
    view.fields[5].max_length = 8255
    const mounted = await open()
    const host = mounted.find<HTMLInputElement>('input[name="host"]')
    const token = mounted.find<HTMLInputElement>('input[name="token"]')
    expect(host.element.maxLength).toBe(16_510)
    expect(token.element.maxLength).toBe(16_510)
    expect(mounted.find<HTMLInputElement>('input[name="password"]').element.maxLength).toBe(24_576)
    expect(mounted.find('input[name="port"]').attributes('maxlength')).toBeUndefined()
    await host.setValue(entities)
    await token.setValue('x'.repeat(8255))
    expect(host.element.checkValidity()).toBe(true)
    expect(token.element.checkValidity()).toBe(true)
    await mounted.find('form').trigger('submit')
    await flushPromises()
    expect(saves()[0][1].values.host).toBe(entities)
    expect(saves()[0][1].secretChanges.token).toBe('x'.repeat(8255))
  })

  it('saves ordinary primitives with the native revision and keeps unchanged secrets', async () => {
    const mounted = await open()
    await mounted.find('input[name="host"]').setValue('new.local')
    await mounted.find('input[name="enabled"]').setValue(false)
    await mounted.find('input[name="port"]').setValue('8883')
    await mounted.find('input[name="threshold"]').setValue('0.75')
    await mounted.find('select[name="provider"]').setValue('frigate')
    await mounted.find('form').trigger('submit')
    await flushPromises()
    expect(native.invoke).toHaveBeenCalledWith('save_plugin_settings', {
      pluginId: view.plugin_id,
      revision: 'revision-1',
      values: {
        host: 'new.local',
        enabled: false,
        port: 8883,
        threshold: 0.75,
        provider: 'frigate',
      },
      secretChanges: {},
    })
    expect(mounted.text()).toContain('Settings saved.')
    expect(mounted.emitted('saved')).toEqual([[3]])
    expect(mounted.emitted('busy')).toEqual([
      [3, true],
      [3, false],
      [3, true],
      [3, false],
    ])
    await mounted.find('input[name="host"]').setValue('another.local')
    expect(mounted.text()).not.toContain('Settings saved.')
  })

  it('distinguishes secret replacement, removal, and leaving blank while suppressing duplicate saves', async () => {
    const mounted = await open()
    const pending = deferred<PluginSettingsSaveResult>()
    native.invoke.mockImplementation(() => pending.promise)
    await mounted.find('input[name="token"]').setValue('replacement-token')
    await mounted.find('input[name="password-remove"]').setValue(true)
    expect(mounted.find('input[name="password"]').attributes('disabled')).toBeDefined()
    await mounted.find('form').trigger('submit')
    await mounted.find('form').trigger('submit')
    expect(saves()).toHaveLength(1)
    expect(saves()[0][1].secretChanges).toEqual({ token: 'replacement-token', password: null })
    expect((mounted.find('input[name="token"]').element as HTMLInputElement).value).toBe('')
    expect(button('Save settings').attributes('disabled')).toBeDefined()
    pending.resolve({
      settings: {
        ...view,
        revision: 'revision-2',
        secret_present: { token: true, password: false },
      },
      restart_error: null,
    })
    await flushPromises()
    expect(mounted.find('input[name="password-remove"]').exists()).toBe(false)
    expect(mounted.text()).toContain('No secret is stored.')
  })

  it('clearing a newly typed secret restores keep semantics and reload drops all drafts', async () => {
    const value = controller()
    await value.load()
    value.setSecret('token', 'temporary')
    value.setSecret('token', '')
    value.removeSecret('password', true)
    value.removeSecret('password', false)
    expect(value.secretChanges.value).toEqual({})
    value.setSecret('token', 'temporary')
    value.values.value.host = 'unsaved.local'
    await value.load()
    expect(value.secretChanges.value).toEqual({})
    expect(value.values.value.host).toBe('home.local')
  })

  it('clears secrets on a failed save and renders a bounded redacted error without losing ordinary edits', async () => {
    const mounted = await open()
    native.invoke.mockRejectedValueOnce(
      new Error(`<script>bad replacement-token</script>${'x'.repeat(600)}`)
    )
    await mounted.find('input[name="host"]').setValue('unsaved.local')
    await mounted.find('input[name="token"]').setValue('replacement-token')
    await mounted.find('form').trigger('submit')
    await flushPromises()
    const alert = mounted.find('[role="alert"]').text()
    expect(alert).toContain('<script>bad ••••</script>')
    expect(alert).not.toContain('replacement-token')
    expect(alert).toHaveLength(512)
    expect(mounted.find('script').exists()).toBe(false)
    expect((mounted.find('input[name="token"]').element as HTMLInputElement).value).toBe('')
    expect((mounted.find('input[name="host"]').element as HTMLInputElement).value).toBe(
      'unsaved.local'
    )
    expect(mounted.text()).not.toContain('Settings saved.')
  })

  it('reports a committed save separately from restart failure and uses the new revision', async () => {
    const value = controller()
    await value.load()
    native.invoke.mockResolvedValueOnce({
      settings: { ...view, revision: 'revision-2' },
      restart_error: '<b>Worker did not start</b>',
    })
    expect(await value.save()).toBe(true)
    expect(value.saved.value).toBe(true)
    expect(value.error.value).toBeNull()
    expect(value.restartError.value).toBe('<b>Worker did not start</b>')
    await value.save()
    expect(saves()[1][1].revision).toBe('revision-2')
  })

  it('renders saved-but-restart-failed feedback as escaped text', async () => {
    const mounted = await open()
    native.invoke.mockResolvedValueOnce({ settings: view, restart_error: '<b>Worker failed</b>' })
    await mounted.find('form').trigger('submit')
    await flushPromises()
    expect(mounted.text()).toContain('Settings were saved, but the plugin could not restart.')
    expect(mounted.text()).toContain('<b>Worker failed</b>')
    expect(mounted.find('b').exists()).toBe(false)
  })

  it('cancel clears password drafts, closes this editor instance, and prevents a later save', async () => {
    const mounted = await open()
    await mounted.find('input[name="token"]').setValue('temporary')
    await button('Cancel').trigger('click')
    expect(mounted.emitted('close')).toEqual([[3]])
    expect(mounted.find('input[name="token"]').exists()).toBe(false)
    await mounted.find('form').trigger('submit')
    expect(saves()).toHaveLength(0)
  })

  it('ignores read responses and errors after unmount and never enables stale saving', async () => {
    const pending = deferred<PluginSettingsView>()
    native.invoke.mockReturnValueOnce(pending.promise)
    const value = controller()
    const loading = value.load()
    value.stop()
    pending.resolve(view)
    await loading
    expect(value.settings.value).toBeNull()
    expect(value.busy.value).toBe(false)
    expect(await value.save()).toBe(false)
    expect(saves()).toHaveLength(0)
  })

  it('does not restore saved metadata or secrets when a pending save completes after teardown', async () => {
    const value = controller()
    await value.load()
    const pending = deferred<PluginSettingsSaveResult>()
    native.invoke.mockReturnValueOnce(pending.promise)
    value.setSecret('token', 'temporary')
    const saving = value.save()
    value.stop()
    pending.resolve({ settings: view, restart_error: 'stale result' })
    expect(await saving).toBe(false)
    expect(value.settings.value).toBeNull()
    expect(value.secretChanges.value).toEqual({})
    expect(value.error.value).toBeNull()
    expect(value.restartError.value).toBeNull()
  })

  it('fails closed for a different plugin version and offers an explicit reload', async () => {
    native.invoke.mockResolvedValueOnce({ ...view, version: '3.0.0' })
    const mounted = await open()
    expect(mounted.find('[role="alert"]').exists()).toBe(true)
    expect(button('Save settings').attributes('disabled')).toBeDefined()
    expect(mounted.find('input').exists()).toBe(false)
    await button('Reload saved settings').trigger('click')
    await flushPromises()
    expect(mounted.find('input[name="host"]').exists()).toBe(true)
  })

  it('omits blank optional numeric fields instead of silently changing them to zero', async () => {
    const mounted = await open()
    await mounted.find('input[name="port"]').setValue('')
    await mounted.find('form').trigger('submit')
    await flushPromises()
    expect(saves()[0][1].values).not.toHaveProperty('port')
  })

  it('saves an unchecked required boolean as false while keeping untouched optional values absent', async () => {
    view.fields[1].required = true
    delete view.values.enabled
    delete view.values.port
    const mounted = await open()
    expect((mounted.find('input[name="enabled"]').element as HTMLInputElement).checked).toBe(false)
    await mounted.find('form').trigger('submit')
    await flushPromises()
    expect(saves()[0][1].values.enabled).toBe(false)
    expect(saves()[0][1].values).not.toHaveProperty('port')
  })

  it('translates the editor and secret presence in Russian', async () => {
    view.secret_present.token = false
    const mounted = await open('ru')
    expect(mounted.text()).toContain('Настройки')
    expect(mounted.text()).toContain('Секрет не сохранён.')
    expect(button('Сохранить настройки').exists()).toBe(true)
  })

  it('leaves Unicode scalar length validation to native code for strings and secrets', async () => {
    view.fields[0].max_length = 1
    view.fields[5].max_length = 1
    const mounted = await open()
    await mounted.find('input[name="host"]').setValue('😀')
    await mounted.find('input[name="token"]').setValue('😀')
    expect(mounted.find('input[name="token"]').attributes('maxlength')).toBe('2')
    expect(mounted.find('input[name="token"]').attributes('minlength')).toBeUndefined()
    await mounted.find('form').trigger('submit')
    await flushPromises()
    expect(saves()[0][1].values.host).toBe('😀')
    expect(saves()[0][1].secretChanges.token).toBe('😀')
  })

  it.each(['9007199254740992', '-9007199254740992', '9007199254740991.01', '1.5'])(
    'rejects an unsafe or fractional integer lexeme %s before IPC',
    async (input) => {
      const mounted = await open()
      await mounted.find('input[name="port"]').setValue(input)
      await mounted.find('form').trigger('submit')
      await flushPromises()
      expect(saves()).toHaveLength(0)
      expect(mounted.find('[role="alert"]').text()).toBe('Check the value for port.')
    }
  )
})

describe('declarative structured settings', () => {
  it.each([false, true])(
    'opens and edits the Home Assistant manifest layout with a saved value: %s',
    async (hasSavedLayout) => {
      const layout = homeAssistantManifest.config_schema.properties.dashboard_layout
      view.plugin_id = homeAssistantManifest.plugin_id
      view.fields = [
        field('dashboard_layout', {
          title: layout.title,
          editor: { kind: 'json', schema: layout['x-editor'].schema as JsonEditorSchema },
        }),
      ]
      const savedLayout = {
        version: 1,
        controls: [],
        sections: { sensors: false },
        appliances: {},
      }
      view.values = hasSavedLayout ? { dashboard_layout: JSON.stringify(savedLayout) } : {}
      native.invoke.mockImplementation(async (command: string, args) => {
        if (command === 'get_plugin_settings') return structuredClone(view)
        if (command === 'get_plugin_settings_choices') return { revision: null, sources: {} }
        if (command === 'save_plugin_settings')
          return { settings: { ...view, values: args.values }, restart_error: null }
        throw new Error(`Unexpected IPC: ${command}`)
      })

      const mounted = await open()
      const controls = mounted
        .findAll('fieldset')
        .find((item) => item.find('legend').text() === 'Header and Home controls')!
      expect(controls.exists()).toBe(true)
      expect(saves()).toHaveLength(0)
      await controls.get('button').trigger('click')
      await mounted.get('input[aria-label="Stable control ID"]').setValue('porch')
      await mounted.get('input[aria-label="Label"]').setValue('Porch')
      await mounted.get('input[aria-label="Home Assistant entity"]').setValue('switch.porch')
      await mounted.get('form').trigger('submit')
      await flushPromises()

      expect(JSON.parse(saves()[0][1].values.dashboard_layout)).toEqual({
        ...(hasSavedLayout ? savedLayout : { version: 1 }),
        controls: [
          {
            id: 'porch',
            surface: 'home',
            order: 0,
            label: 'Porch',
            entity: 'switch.porch',
            icon: 'plug',
          },
        ],
      })
      expect(mounted.text()).toContain('Settings saved.')
      expect(layout['x-editor'].schema.properties.controls.default).toEqual([])
    }
  )

  it('edits nested layout fields and exact discovered targets without exposing raw JSON', async () => {
    view.fields = [
      field('layout', {
        editor: {
          kind: 'json',
          schema: {
            type: 'object',
            properties: {
              version: { type: 'integer', const: 1 },
              controls: {
                type: 'array',
                title: 'Controls',
                maxItems: 64,
                items: {
                  type: 'object',
                  required: ['label', 'entity'],
                  properties: {
                    label: { type: 'string', title: 'Label' },
                    entity: {
                      type: 'string',
                      title: 'Target',
                      'x-options-source': 'entities',
                      'x-options-prefixes': ['light.'],
                    },
                  },
                },
              },
            },
          },
        },
      }),
    ]
    view.values = {
      layout: JSON.stringify({
        version: 1,
        controls: [{ label: 'Porch', entity: 'light.porch' }],
        future: { retained: true },
      }),
    }
    native.invoke.mockImplementation(async (command: string, args) => {
      if (command === 'get_plugin_settings') return structuredClone(view)
      if (command === 'get_plugin_settings_choices')
        return {
          revision: 'worker:catalog',
          sources: {
            entities: [
              { value: 'light.porch', label: 'Porch' },
              { value: 'sensor.temp', label: 'Temperature' },
            ],
          },
        }
      if (command === 'save_plugin_settings')
        return { settings: { ...view, values: args.values }, restart_error: null }
    })
    const mounted = await open()
    expect(native.invoke).toHaveBeenCalledWith('get_plugin_settings_choices', {
      pluginId: view.plugin_id,
    })
    expect(mounted.find('textarea').exists()).toBe(false)
    expect(mounted.findAll('datalist option').map((option) => option.attributes('value'))).toEqual([
      'light.porch',
    ])
    await mounted.get('input[aria-label="Label"]').setValue('Front porch')
    await mounted.find('form').trigger('submit')
    await flushPromises()
    expect(JSON.parse(saves()[0][1].values.layout)).toEqual({
      version: 1,
      controls: [{ label: 'Front porch', entity: 'light.porch' }],
      future: { retained: true },
    })
  })
  it('keeps stored secret maps opaque and requires explicit replacement before editing entries', async () => {
    view.fields = [
      field('live_urls', {
        secret: true,
        editor: {
          kind: 'json',
          schema: {
            type: 'object',
            maxProperties: 32,
            additionalProperties: { type: 'string', title: 'Live URL', maxLength: 2048 },
          },
        },
      }),
    ]
    view.secret_present = { live_urls: true }
    view.values = { live_urls: 'unexpected-native-secret' }
    const mounted = await open()
    expect(mounted.html()).not.toContain('unexpected-native-secret')
    expect(mounted.find('input[type="password"]').exists()).toBe(false)
    await button('Replace stored value').trigger('click')
    await button('Add entry').trigger('click')
    await mounted.get('input[type="password"]').setValue('https://private.invalid/live')
    await mounted.find('form').trigger('submit')
    await flushPromises()
    expect(saves()[0][1].secretChanges).toEqual({
      live_urls: '{"new":"https://private.invalid/live"}',
    })
    expect(mounted.html()).not.toContain('private.invalid')
    expect(mounted.find('input[type="password"]').exists()).toBe(false)
    expect(saves()[0][1].values).toEqual({})
  })
  it('does not erase an unparsable stored value and discards catalog replies after editor teardown', async () => {
    view.fields = [
      field('layout', {
        editor: {
          kind: 'json',
          schema: {
            type: 'object',
            properties: { target: { type: 'string', 'x-options-source': 'entities' } },
          },
        },
      }),
    ]
    view.values = { layout: 'future-not-json' }
    const result = deferred<{
      revision: string
      sources: Record<string, Array<{ value: string; label: string }>>
    }>()
    native.invoke.mockImplementation(async (command: string) =>
      command === 'get_plugin_settings' ? structuredClone(view) : result.promise
    )
    const value = controller()
    await value.load()
    value.stop()
    result.resolve({
      revision: 'late',
      sources: { entities: [{ value: 'light.late', label: 'Late' }] },
    })
    await flushPromises()
    expect(value.choices.value.sources).toEqual({})
    native.invoke.mockImplementation(async (command: string, args) =>
      command === 'get_plugin_settings'
        ? structuredClone(view)
        : command === 'get_plugin_settings_choices'
          ? { revision: null, sources: {} }
          : { settings: { ...view, values: args.values }, restart_error: null }
    )
    const mounted = await open()
    expect(mounted.text()).toContain('It has been preserved.')
    await mounted.find('form').trigger('submit')
    await flushPromises()
    expect(saves()[0][1].values.layout).toBe('future-not-json')
  })
})

it('redacts individual values from a newly entered structured secret on native failure', async () => {
  view.fields = [field('private_map', { secret: true })]
  view.values = {}
  view.secret_present = { private_map: true }
  const value = controller()
  await value.load()
  value.setSecret('private_map', '{"front":"https://private.invalid/live"}')
  native.invoke.mockRejectedValueOnce(new Error('Could not load https://private.invalid/live'))
  expect(await value.save()).toBe(false)
  expect(value.error.value).toBe('Could not load ••••')
  expect(value.secretChanges.value).toEqual({})
})
