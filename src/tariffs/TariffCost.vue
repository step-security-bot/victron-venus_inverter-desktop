<template>
  <HoverTip v-if="plan || error" :text="details" class="tariff-cost">
    <template v-if="plan">
      <span v-if="cost !== null">≈ {{ money(cost) }} ·</span>
      <span>{{ rateNow }} {{ plan.currency }}/kWh</span>
    </template>
    <span v-else role="alert">Tariff unavailable</span>
  </HoverTip>
</template>
<script setup lang="ts">
import { computed, onMounted, onBeforeUnmount, ref } from 'vue'
import HoverTip from '../components/HoverTip.vue'
import { billingPeriod, currentRate, estimateDailyCost, flatRate, validatePlan } from './model'
import { useLocalTariff } from './useLocalTariff'

const props = defineProps<{
  kwh?: number | null
  tariffScope: string
  configuredTariff?: unknown
}>()
const local = useLocalTariff(() => props.tariffScope)
const configured = computed(() => {
  if (props.configuredTariff == null) return { plan: null, error: '' }
  try {
    return { plan: validatePlan(props.configuredTariff), error: '' }
  } catch {
    return { plan: null, error: 'The controller tariff is invalid. Correct its configuration.' }
  }
})
const plan = computed(() =>
  local.mode.value === 'local' ? local.plan.value : configured.value.plan
)
const error = computed(() =>
  local.mode.value === 'local' ? local.error.value : configured.value.error
)
const now = ref(new Date())
let timer: ReturnType<typeof setInterval> | undefined
onMounted(() => {
  timer = setInterval(() => {
    now.value = new Date()
  }, 30_000)
})
onBeforeUnmount(() => {
  if (timer) clearInterval(timer)
})
const cost = computed(() => estimateDailyCost(plan.value, props.kwh))
const rateNow = computed(() => (plan.value ? currentRate(plan.value, now.value).toFixed(4) : ''))
const details = computed(() => {
  if (!plan.value) return error.value
  const source =
    local.mode.value === 'local' ? 'Local tariff · this device only' : 'Controller tariff'
  const period = billingPeriod(plan.value, now.value)
  return [
    `${source}: ${plan.value.name}. Current rate in ${plan.value.timeZone}.`,
    flatRate(plan.value) === null
      ? 'Daily cost needs interval consumption; today’s total alone is insufficient for time-of-use prices.'
      : 'Estimated energy cost when daily consumption is available; excludes taxes and other charges.',
    period ? `Billing period: ${period.start} – ${period.end}.` : '',
    local.syncError.value,
  ]
    .filter(Boolean)
    .join(' ')
})
const money = (value: number) =>
  new Intl.NumberFormat(undefined, {
    style: 'currency',
    currency: plan.value?.currency ?? 'USD',
  }).format(value)
</script>
<style>
/* HoverTip forwards this class to a fragment child without this component's scope attribute. */
.tariff-cost {
  display: inline-flex;
  align-items: center;
  white-space: nowrap;
  gap: 4px;
  font-size: inherit;
  line-height: inherit;
  font-weight: inherit;
}
</style>
