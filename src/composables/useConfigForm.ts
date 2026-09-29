import { invoke } from '@tauri-apps/api/core'
import { getCurrentScope, onScopeDispose, reactive, ref } from 'vue'
import type { AppConfig } from '../config'
import { defaultConfig, normalizeConfig } from '../config'

export function useConfigForm() {
  const config = reactive<AppConfig>({ ...defaultConfig })
  const saving = ref(false)
  const configLoaded = ref(false)
  const message = ref('')
  const messageType = ref<'success' | 'error' | 'info'>('info')
  let active = true
  let loadGeneration = 0
  if (getCurrentScope()) {
    onScopeDispose(() => {
      active = false
      loadGeneration += 1
    })
  }

  async function loadConfig(): Promise<AppConfig | null> {
    if (!active) return null
    const generation = ++loadGeneration
    configLoaded.value = false
    try {
      const loaded = await invoke<AppConfig>('get_config')
      if (!active || generation !== loadGeneration) return null
      Object.assign(config, normalizeConfig(loaded))
      if (!config.color_scheme) config.color_scheme = 'dark'
      message.value = ''
      configLoaded.value = true
    } catch (e) {
      if (!active || generation !== loadGeneration) return null
      message.value = `Failed to load config: ${e}`
      messageType.value = 'error'
      throw e
    }
    return config
  }

  async function saveConfig(): Promise<boolean> {
    if (!configLoaded.value) return false
    saving.value = true
    try {
      await invoke('save_config', { config })
      message.value = 'Configuration saved successfully'
      messageType.value = 'success'
      return true
    } catch (e) {
      message.value = `Failed to save config: ${e}`
      messageType.value = 'error'
      return false
    } finally {
      saving.value = false
    }
  }

  function resetToDefaults() {
    // Core defaults do not own installed or unknown modules' persistent data.
    Object.assign(config, defaultConfig, { modules: config.modules })
    message.value = 'Reset to defaults (unsaved)'
    messageType.value = 'info'
  }

  function clearMessage() {
    message.value = ''
  }

  return {
    config,
    defaultConfig,
    configLoaded,
    saving,
    message,
    messageType,
    loadConfig,
    saveConfig,
    resetToDefaults,
    clearMessage,
  }
}
