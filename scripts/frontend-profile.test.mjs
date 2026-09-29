import assert from 'node:assert/strict'
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises'
import os from 'node:os'
import path from 'node:path'
import test from 'node:test'
import { build } from 'vite'
import {
  assertCoreModuleGraph,
  assertLightweightDesktopEntry,
  assertMobileModuleGraph,
  frontendProfileAudit,
  resolveFrontendProfile,
  staticEntryModules,
} from './frontend-profile.mjs'

test('camera startup follows static shared chunks without loading dynamic pages', () => {
  const chunk = (fileName, modules, imports = [], dynamicImports = [], isEntry = false) => ({
    type: 'chunk',
    fileName,
    modules: Object.fromEntries(modules.map((id) => [id, {}])),
    imports,
    dynamicImports,
    isEntry,
  })
  const bundle = {
    'entry.js': chunk('entry.js', ['src/main.ts'], ['shared.js'], ['dashboard.js'], true),
    'shared.js': chunk('shared.js', ['src/components/AuthGate.vue'], ['entry.js', 'vue.js']),
    'vue.js': chunk('vue.js', ['node_modules/@vue/runtime-core/index.js']),
    'dashboard.js': chunk('dashboard.js', ['src/App.vue'], ['charts.js']),
    'charts.js': chunk('charts.js', ['node_modules/echarts/lib/core.js']),
  }
  assert.doesNotThrow(() => assertLightweightDesktopEntry(staticEntryModules(bundle)))
  // A vendor group accidentally capturing Vue and ECharts makes charts an
  // eager dependency even though the dashboard route itself stays lazy.
  bundle['vue.js'].imports.push('charts.js')
  assert.throws(() => assertLightweightDesktopEntry(staticEntryModules(bundle)), /echarts/)
  bundle['vue.js'].imports = []
  bundle['entry.js'].imports.push('dashboard.js')
  assert.throws(() => assertLightweightDesktopEntry(staticEntryModules(bundle)), /src\/App.vue/)
})

test('native mobile targets cannot select desktop assets, including armv7 Android', () => {
  for (const platform of ['android', 'androideabi', 'ios']) {
    assert.equal(resolveFrontendProfile({ TAURI_ENV_PLATFORM: platform }), 'mobile')
    assert.throws(() =>
      resolveFrontendProfile({ TAURI_ENV_PLATFORM: platform, INVERTER_BUILD_PROFILE: 'desktop' })
    )
  }
  for (const triple of [
    'aarch64-apple-ios',
    'aarch64-apple-ios-sim',
    'aarch64-linux-android',
    'armv7-linux-androideabi',
  ]) {
    assert.equal(resolveFrontendProfile({ CARGO_BUILD_TARGET: triple }), 'mobile')
    assert.throws(() =>
      resolveFrontendProfile({ TAURI_ENV_TARGET_TRIPLE: triple, INVERTER_BUILD_PROFILE: 'desktop' })
    )
  }
})

test('standalone builds choose explicitly; invalid or conflicting target hints fail', () => {
  assert.equal(resolveFrontendProfile({}), 'desktop')
  assert.equal(resolveFrontendProfile({ INVERTER_BUILD_PROFILE: 'mobile' }), 'mobile')
  assert.equal(resolveFrontendProfile({ TAURI_ENV_PLATFORM: 'darwin' }), 'desktop')
  assert.throws(() => resolveFrontendProfile({ INVERTER_BUILD_PROFILE: 'full' }))
  assert.throws(() => resolveFrontendProfile({ TAURI_ENV_PLATFORM: 'typo' }))
  assert.throws(() =>
    resolveFrontendProfile({
      TAURI_ENV_PLATFORM: 'ios',
      CARGO_BUILD_TARGET: 'x86_64-unknown-linux-gnu',
    })
  )
  assert.throws(() =>
    resolveFrontendProfile({ TAURI_ENV_PLATFORM: 'windows', INVERTER_BUILD_PROFILE: 'mobile' })
  )
})

test('audit accepts shared core but rejects desktop implementation and translations', () => {
  assert.doesNotThrow(() =>
    assertMobileModuleGraph(['src/main.ts', 'src/features/mobile.ts', 'src/inverterControl.ts'])
  )
  for (const id of [
    'src/features/desktop/ha/session.ts',
    'src/features/desktop.ts',
    'src/features/messages.desktop.ts',
    'src/composables/useHA.ts',
    'src/CameraVideo.vue',
    'src/plugins/manager.ts',
    'desktop-plugins/home-assistant/src/main.rs',
    'desktop-plugins/worker-protocol/src/lib.rs',
    'scripts/plugins/home-assistant-manifest.json',
  ]) {
    assert.throws(() => assertMobileModuleGraph(['src/main.ts', id]), /Desktop feature modules/)
  }
})

test('real mobile graph rejects worker modules and metadata outside src', async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), 'inverter-mobile-worker-graph-'))
  try {
    await mkdir(path.join(root, 'src'), { recursive: true })
    await writeFile(
      path.join(root, 'index.html'),
      '<script type="module" src="/src/main.ts"></script>'
    )
    for (const [relative, content] of [
      ['desktop-plugins/home-assistant/source.ts', 'export default "ha worker"'],
      ['desktop-plugins/worker-protocol/source.ts', 'export default "worker protocol"'],
      ['scripts/plugins/home-assistant-manifest.json', '{"plugin_id":"test.home-assistant"}'],
    ]) {
      const filename = path.join(root, relative)
      await mkdir(path.dirname(filename), { recursive: true })
      await writeFile(filename, content)
      await writeFile(
        path.join(root, 'src/main.ts'),
        `import value from "../${relative}"; if (false) console.log(value)`
      )
      await assert.rejects(
        build({
          root,
          configFile: false,
          logLevel: 'silent',
          plugins: [frontendProfileAudit(root, 'mobile')],
          build: { write: false },
        }),
        /Desktop feature modules/
      )
    }
  } finally {
    await rm(root, { recursive: true, force: true })
  }
})

test('real bundler graph rejects a desktop import even behind an unused runtime branch', async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), 'inverter-mobile-graph-'))
  try {
    await mkdir(path.join(root, 'src/features/desktop'), { recursive: true })
    await writeFile(
      path.join(root, 'index.html'),
      '<script type="module" src="/src/main.ts"></script>'
    )
    await writeFile(
      path.join(root, 'src/features/desktop/feature.ts'),
      'export const feature = "desktop-only"'
    )
    await writeFile(
      path.join(root, 'src/main.ts'),
      'import { feature } from "./features/desktop/feature"; if (false) console.log(feature)'
    )
    await assert.rejects(
      build({
        root,
        configFile: false,
        logLevel: 'silent',
        plugins: [frontendProfileAudit(root, 'mobile')],
        build: { write: false },
      }),
      /Desktop feature modules/
    )
    await writeFile(path.join(root, 'src/main.ts'), 'console.log("core")')
    const result = await build({
      root,
      configFile: false,
      logLevel: 'silent',
      plugins: [frontendProfileAudit(root, 'mobile')],
      build: { write: false },
    })
    const receipt = result.output.find((item) => item.fileName === 'build-profile.json')
    assert.equal(JSON.parse(receipt.source).profile, 'mobile')
    assert.deepEqual(JSON.parse(receipt.source).modules, ['src/main.ts'])
  } finally {
    await rm(root, { recursive: true, force: true })
  }
})

test('desktop core accepts generic host UI and rejects bundled providers', () => {
  assert.doesNotThrow(() =>
    assertCoreModuleGraph([
      'src/main.ts',
      'src/features/desktop.ts',
      'src/features/desktop/plugins/PluginCompactPanels.vue',
      'src/features/desktop/plugins/PluginMedia.vue',
    ])
  )
  for (const id of [
    'src/composables/useHA.ts',
    'src/types/ha.ts',
    'src/CameraVideo.vue',
    'src/features/desktop/HomePanels.vue',
    'src/features/desktop/CameraAction.vue',
    'src/features/desktop/cameraConnection.ts',
    'src/features/desktop/ha/client.ts',
    'desktop-plugins/kerberos/src/main.rs',
    'desktop-plugins/ring/src/main.rs',
    'scripts/plugins/ring-manifest.json',
  ]) {
    assert.throws(() => assertCoreModuleGraph(['src/main.ts', id]), /Bundled provider modules/)
  }
})

test('real desktop graph rejects provider code even when its import is tree-shaken', async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), 'inverter-desktop-core-graph-'))
  try {
    await mkdir(path.join(root, 'src/composables'), { recursive: true })
    await writeFile(
      path.join(root, 'index.html'),
      '<script type="module" src="/src/main.ts"></script>'
    )
    await writeFile(path.join(root, 'src/composables/useHA.ts'), 'export const provider = "ha"')
    await writeFile(
      path.join(root, 'src/main.ts'),
      'import { provider } from "./composables/useHA"; if (false) console.log(provider)'
    )
    await assert.rejects(
      build({
        root,
        configFile: false,
        logLevel: 'silent',
        plugins: [frontendProfileAudit(root, 'desktop')],
        build: { write: false },
      }),
      /Bundled provider modules/
    )
  } finally {
    await rm(root, { recursive: true, force: true })
  }
})
