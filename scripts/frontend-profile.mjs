import path from 'node:path'
import { realpathSync } from 'node:fs'

const profiles = new Set(['desktop', 'mobile'])
const mobilePlatforms = new Set(['android', 'androideabi', 'ios'])
const desktopPlatforms = new Set(['darwin', 'macos', 'linux', 'windows', 'freebsd'])

/** Resolve the target, not the OS running Vite (mobile builds are cross-builds). */
export function resolveFrontendProfile(env = process.env) {
  const requested = env.INVERTER_BUILD_PROFILE?.trim()
  if (requested && !profiles.has(requested)) {
    throw new Error(`Unsupported INVERTER_BUILD_PROFILE: ${requested}`)
  }
  const platform = env.TAURI_ENV_PLATFORM?.trim()
  let nativeProfile
  if (platform) {
    if (mobilePlatforms.has(platform)) nativeProfile = 'mobile'
    else if (desktopPlatforms.has(platform)) nativeProfile = 'desktop'
    else throw new Error(`Unsupported Tauri platform: ${platform}`)
  }
  const target = env.TAURI_ENV_TARGET_TRIPLE || env.CARGO_BUILD_TARGET
  if (target) {
    const targetProfile = /-(?:apple-ios|linux-android)/.test(target) ? 'mobile' : 'desktop'
    if (nativeProfile && nativeProfile !== targetProfile) {
      throw new Error('Tauri platform and compilation target disagree')
    }
    nativeProfile = targetProfile
  }
  if (requested && nativeProfile && requested !== nativeProfile) {
    throw new Error(`Cannot build ${requested} frontend for a ${nativeProfile} native target`)
  }
  return nativeProfile || requested || 'desktop'
}

export function featureAliases(root, profile) {
  if (!profiles.has(profile)) throw new Error(`Unsupported frontend profile: ${profile}`)
  return {
    '@features': path.resolve(root, `src/features/${profile}.ts`),
    '@feature-messages': path.resolve(root, `src/features/messages.${profile}.ts`),
    '@feature-defaults': path.resolve(
      root,
      profile === 'desktop'
        ? 'src/features/desktop/defaultConfig.ts'
        : 'src/features/defaults.mobile.ts'
    ),
  }
}

// A direct import added to a shared screen must fail the mobile build, even when
// the UI would be hidden by a runtime setting or the module is tree-shaken later.
// Keep removed entry points in the guard to catch accidental reintroduction.
export const desktopModulePatterns = [
  /^(?:desktop-plugins|scripts\/plugins)\//,
  /^src\/features\/desktop[/.]/,
  /^src\/features\/messages\.desktop\./,
  /^src\/plugins\//,
  /^src\/CameraVideo\.vue$/,
  /^src\/composables\/useHA\.ts$/,
  /^src\/composables\/useDashboardControlsConfig\.ts$/,
  /^src\/components\/(?:HaEntitiesEditor|EntityAutocompleteInput)\.vue$/,
]

// Core ships a generic desktop host. Provider implementations and worker package
// metadata belong exclusively to separately installed archives on every platform.
export const extractedModulePatterns = [
  /^(?:desktop-plugins|scripts\/plugins)\//,
  /^src\/(?:CameraVideo\.vue|types\/ha\.ts)$/,
  /^src\/composables\/(?:useHA|useDashboardControlsConfig)\.ts$/,
  /^src\/components\/(?:HaEntitiesEditor|EntityAutocompleteInput)\.vue$/,
  /^src\/features\/desktop\/(?:CameraAction|CameraStatus|DashboardPanels|DiscoveryDialog|HomeControlsEditor|HomePanels|HomeStatus|IntegrationConfig|SectionVisibility|Setup)\.vue$/,
  /^src\/features\/desktop\/(?:cameraConnection|homeNotifications|ha|camera)(?:\.ts|\/)/,
]

export function assertCoreModuleGraph(modules) {
  const forbidden = modules.filter((id) =>
    extractedModulePatterns.some((pattern) => pattern.test(id))
  )
  if (forbidden.length) {
    throw new Error(`Bundled provider modules in core build:\n${forbidden.join('\n')}`)
  }
}

export function assertMobileModuleGraph(modules) {
  const forbidden = modules.filter((id) =>
    desktopModulePatterns.some((pattern) => pattern.test(id))
  )
  if (forbidden.length) {
    throw new Error(`Desktop feature modules in mobile build:\n${forbidden.join('\n')}`)
  }
}

/** Follow only static chunk imports: dynamic pages are loaded by their owner. */
export function staticEntryModules(bundle) {
  const pending = Object.values(bundle)
    .filter((chunk) => chunk.type === 'chunk' && chunk.isEntry)
    .map((chunk) => chunk.fileName)
  const visited = new Set()
  const modules = new Set()
  while (pending.length) {
    const name = pending.pop()
    if (visited.has(name)) continue
    visited.add(name)
    const chunk = bundle[name]
    if (chunk?.type !== 'chunk') continue
    Object.keys(chunk.modules).forEach((id) => modules.add(id))
    pending.push(...chunk.imports)
  }
  return [...modules]
}

export function assertLightweightDesktopEntry(modules) {
  const heavy = modules.filter(
    (id) =>
      /^src\/(?:App|Config|About|MobileShell)\.vue$/.test(id) ||
      /(?:^|\/)node_modules\/(?:echarts|vue-echarts|zrender)\//.test(id)
  )
  if (heavy.length) {
    throw new Error(
      `Dashboard or configuration code in camera-window startup:\n${heavy.join('\n')}`
    )
  }
}

/** Vite/Rolldown plugin: inspect actual loaded modules and ship an audit receipt. */
export function frontendProfileAudit(root, profile) {
  const prefix = realpathSync(root).replaceAll('\\', '/') + '/'
  return {
    name: 'inverter-frontend-profile',
    generateBundle(_options, bundle) {
      const modules = [
        ...new Set(
          [...this.getModuleIds()]
            .map((id) => {
              const clean = id.replaceAll('\\', '/').split('?')[0]
              return clean.startsWith(prefix) ? clean.slice(prefix.length) : ''
            })
            .filter(
              (id) =>
                id.startsWith('src/') ||
                id.startsWith('desktop-plugins/') ||
                id.startsWith('scripts/plugins/')
            )
        ),
      ].sort((left, right) => left.localeCompare(right, 'en'))
      if (!modules.includes('src/main.ts')) throw new Error('Frontend module graph is empty')
      if (profile === 'mobile') assertMobileModuleGraph(modules)
      assertCoreModuleGraph(modules)
      if (profile === 'desktop') {
        assertLightweightDesktopEntry(
          staticEntryModules(bundle).map((id) => {
            const clean = id.replaceAll('\\', '/').split('?')[0]
            return clean.startsWith(prefix) ? clean.slice(prefix.length) : clean
          })
        )
      }
      this.emitFile({
        type: 'asset',
        fileName: 'build-profile.json',
        source: JSON.stringify({ schema_version: 1, profile, modules }, null, 2) + '\n',
      })
    },
  }
}
