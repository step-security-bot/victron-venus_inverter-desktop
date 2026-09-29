<template>
  <UiButton
    v-for="group in groups"
    :key="group.id"
    class="min-w-[22px] !px-1.5 shrink-0"
    toggle
    :active="group.enabled"
    :disabled="!available || busy.has(group.id)"
    :title="group.title"
    :aria-label="group.title"
    @click="toggle(group)"
  >
    <Camera v-if="group.icon === 'camera' && group.enabled" :size="11" /><CameraOff
      v-else-if="group.icon === 'camera'"
      :size="11"
    /><Puzzle v-else :size="11" />
  </UiButton>
</template>
<script setup lang="ts">
import { onMounted, onUnmounted, ref } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { Camera, CameraOff, Puzzle } from '@lucide/vue'
import UiButton from '../../../components/UiButton.vue'
import type { PluginGroup } from './types'
const emit = defineEmits<{ error: [message: string] }>()
const groups = ref<PluginGroup[]>([])
const available = ref(false)
const busy = ref(new Set<string>())
let active = false
let generation = 0
let authEpoch = 0
let refreshRequested = false
let refreshing: Promise<void> | undefined
let listeners: Array<() => void> = []
async function readGroups() {
  const session = generation
  const epoch = authEpoch
  try {
    const status = await invoke<{ unlocked: boolean }>('auth_status')
    if (!active || generation !== session || authEpoch !== epoch) return
    if (!status?.unlocked) {
      groups.value = []
      available.value = false
      return
    }
    const value = await invoke<PluginGroup[]>('get_plugin_groups')
    if (!active || generation !== session || authEpoch !== epoch) return
    groups.value = value
    available.value = true
  } catch {
    if (active && generation === session && authEpoch === epoch) available.value = false
  }
}
async function drainRefreshes() {
  try {
    while (active && refreshRequested) {
      refreshRequested = false
      // Telemetry may outpace IPC. Apply each completed read and coalesce newer
      // events into one follow-up; only an auth/lifecycle change invalidates it.
      await readGroups()
    }
  } finally {
    refreshing = undefined
  }
}
function refresh(): Promise<void> {
  if (!active) return Promise.resolve()
  refreshRequested = true
  refreshing ??= drainRefreshes()
  return refreshing
}
async function toggle(group: PluginGroup) {
  if (!active || !available.value || busy.value.has(group.id)) return
  const session = generation
  const epoch = authEpoch
  busy.value.add(group.id)
  try {
    await invoke('set_plugin_group_enabled', { groupId: group.id, enabled: !group.enabled })
  } catch {
    if (active && generation === session && authEpoch === epoch)
      emit('error', 'Could not update plugin monitoring. Reload settings and try again.')
  } finally {
    if (active && generation === session && authEpoch === epoch) {
      busy.value.delete(group.id)
      void refresh()
    }
  }
}
onMounted(async () => {
  active = true
  const session = ++generation
  for (const name of ['plugin-host-update', 'auth-state-changed']) {
    try {
      const stop = await listen(name, () => {
        if (!active || generation !== session) return
        if (name === 'auth-state-changed') {
          authEpoch += 1
          busy.value.clear()
          groups.value = []
          available.value = false
        }
        void refresh()
      })
      if (!active || generation !== session) {
        stop()
        return
      }
      listeners.push(stop)
    } catch {
      return
    }
  }
  await refresh()
})
onUnmounted(() => {
  active = false
  generation += 1
  refreshRequested = false
  for (const stop of listeners) stop()
  listeners = []
  groups.value = []
  available.value = false
  busy.value.clear()
})
</script>
