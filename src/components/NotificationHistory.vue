<template>
  <div class="notification-bell relative">
    <UiButton class="relative !px-2" variant="ghost" @click="showPanel = !showPanel">
      🔔
      <span
        v-if="unreadCount > 0"
        class="absolute -top-1 -right-1 min-w-3.5 h-3.5 px-0.5 rounded-full bg-consumption text-white text-[8px] flex items-center justify-center font-bold tabular"
      >
        {{ unreadCount > 9 ? '9+' : unreadCount }}
      </span>
    </UiButton>

    <div
      v-if="showPanel"
      class="notification-history-panel absolute bottom-full right-0 mb-2 w-72 max-h-80 overflow-y-auto apple-card z-50"
    >
      <div
        class="flex items-center justify-between px-2.5 py-1.5 border-b border-black/[0.06] dark:border-white/[0.08]"
      >
        <span class="text-[10px] font-semibold text-muted tracking-tight">{{
          $t('notifications.title')
        }}</span>
        <div class="flex gap-2">
          <button
            type="button"
            v-if="notifications.length > 0"
            class="text-[9px] font-semibold text-accent hover:opacity-80"
            @click="markAllRead"
          >
            {{ $t('notifications.markAllRead') }}
          </button>
          <button
            type="button"
            v-if="notifications.length > 0"
            class="text-[9px] font-semibold text-consumption hover:opacity-80"
            @click="clearAll"
          >
            {{ $t('notifications.clear') }}
          </button>
        </div>
      </div>
      <div v-if="notifications.length === 0" class="px-2.5 py-4 text-[10px] text-muted text-center">
        {{ $t('notifications.noNotifications') }}
      </div>
      <button
        type="button"
        v-for="n in notifications"
        :key="n.id"
        class="w-full text-left row-hover px-2.5 py-1.5 border-b border-black/[0.04] dark:border-white/[0.05] last:border-0 cursor-pointer"
        :class="{ 'opacity-50': n.read }"
        @click="markRead(n.id)"
      >
        <span class="flex items-start justify-between gap-2">
          <span class="text-[10px] font-semibold text-main tracking-tight">{{ n.title }}</span>
          <span
            class="text-[8px] text-muted whitespace-nowrap tabular"
            :title="
              n.timestamp === null
                ? undefined
                : new Date(n.timestamp).toLocaleString(undefined, { timeZoneName: 'short' })
            "
          >
            {{ formatTime(n.timestamp) }}
          </span>
        </span>
        <span class="block text-[9px] text-muted truncate mt-0.5">{{ n.body }}</span>
      </button>
    </div>
  </div>
</template>

<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted } from 'vue'
import UiButton from './UiButton.vue'
import { useI18n } from 'vue-i18n'
import {
  notifications,
  unreadNotificationCount,
  markNotificationRead,
  markAllNotificationsRead,
  clearNotifications,
} from '../composables/useInverterState'

const { t: $t } = useI18n()
const showPanel = ref(false)
const unreadCount = computed(() => unreadNotificationCount())

function formatTime(ts: number | null): string {
  if (ts === null) return $t('notifications.timeUnavailable')
  const d = new Date(ts)
  return `${d.getHours().toString().padStart(2, '0')}:${d.getMinutes().toString().padStart(2, '0')}`
}

function markRead(id: number) {
  markNotificationRead(id)
}

function markAllRead() {
  markAllNotificationsRead()
}

function clearAll() {
  clearNotifications()
}

function onClickOutside(e: MouseEvent) {
  const target = e.target as HTMLElement
  if (!target.closest('.notification-bell')) {
    showPanel.value = false
  }
}

onMounted(() => {
  document.addEventListener('click', onClickOutside)
})

onUnmounted(() => {
  document.removeEventListener('click', onClickOutside)
})
</script>
