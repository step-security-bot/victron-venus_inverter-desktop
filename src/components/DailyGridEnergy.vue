<template>
  <HoverTip :text="display.details" class="daily-grid-energy tabular">
    <span class="text-muted">{{ display.label }}</span>
    <span>↓ {{ display.imported }} / ↑ {{ display.exported }} kWh</span>
  </HoverTip>
</template>

<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import HoverTip from './HoverTip.vue'
import { dailyGridPresentation } from './dailyGridEnergy'

const props = defineProps<{ energy?: unknown }>()
const now = ref(Date.now())
const display = computed(() => dailyGridPresentation(props.energy, now.value))
let timer: ReturnType<typeof setInterval> | undefined
onMounted(() => {
  timer = setInterval(() => {
    now.value = Date.now()
  }, 1000)
})
onBeforeUnmount(() => {
  if (timer !== undefined) clearInterval(timer)
})
</script>

<style>
/* HoverTip forwards this class to a fragment child, so a scoped selector would not match. */
.daily-grid-energy {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  white-space: nowrap;
  font-size: inherit;
  line-height: inherit;
  font-weight: inherit;
}
</style>
