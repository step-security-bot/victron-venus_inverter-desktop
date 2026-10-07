<template>
  <div v-if="bannerNotifications.length > 0" class="flex flex-col gap-1">
    <div
      v-for="banner in bannerNotifications"
      :key="banner.id"
      class="flex items-center gap-2 rounded-[0.5rem] border px-2.5 py-1.5 text-[12px] leading-tight"
      :class="levelClasses[banner.level] ?? levelClasses.info"
    >
      <AlertOctagon v-if="banner.level === 'alarm'" :size="14" class="shrink-0 opacity-90" />
      <TriangleAlert
        v-else-if="banner.level === 'warning'"
        :size="14"
        class="shrink-0 opacity-90"
      />
      <Info v-else :size="14" class="shrink-0 opacity-90" />
      <div class="flex items-center gap-2 min-w-0 flex-1">
        <span class="font-semibold shrink-0 tracking-tight">{{ banner.title }}</span>
        <span v-if="banner.body" class="opacity-80 truncate min-w-0">{{ banner.body }}</span>
      </div>
      <time
        v-if="notificationTimestampMs(banner.ts) !== null"
        :datetime="banner.ts"
        :title="new Date(banner.ts!).toLocaleString(undefined, { timeZoneName: 'short' })"
        class="text-[10px] opacity-60 shrink-0 whitespace-nowrap tabular"
      >
        {{ formatTimestamp(banner.ts, now) }}
      </time>
      <span v-else class="text-[10px] opacity-60 shrink-0">
        {{ $t('notifications.timeUnavailable') }}
      </span>
      <button
        type="button"
        class="shrink-0 rounded p-0.5 opacity-50 transition-opacity hover:opacity-100 cursor-pointer"
        :title="$t('notifications.dismiss')"
        @click="dismissBanner(banner.id)"
      >
        <X :size="13" />
      </button>
    </div>
  </div>
</template>

<script setup lang="ts">
import { AlertOctagon, Info, TriangleAlert, X } from '@lucide/vue'
import { onMounted, onUnmounted, ref, watch } from 'vue'
import { bannerNotifications, dismissBanner } from '../composables/useInverterState'
import { formatTimestamp, notificationTimestampMs } from '../utils'

const now = ref(Date.now())
const refreshClock = () => {
  now.value = Date.now()
}
let clock: ReturnType<typeof setInterval> | undefined
// Native notifications can stay unchanged for hours. Their age must keep moving,
// including after a suspended/background webview resumes or a replay arrives.
watch(bannerNotifications, refreshClock, { deep: true })
onMounted(() => {
  refreshClock()
  clock = setInterval(refreshClock, 15_000)
  document.addEventListener('visibilitychange', refreshClock)
  window.addEventListener('focus', refreshClock)
})
onUnmounted(() => {
  clearInterval(clock)
  document.removeEventListener('visibilitychange', refreshClock)
  window.removeEventListener('focus', refreshClock)
})

const levelClasses: Record<string, string> = {
  info: 'banner-info',
  warning: 'banner-warning',
  alarm: 'banner-alarm',
}
</script>
