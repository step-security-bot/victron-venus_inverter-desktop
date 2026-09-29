<template>
  <div
    class="daily-stats classic-card shrink-0 px-2 py-1 flex flex-wrap items-center gap-x-2 gap-y-1 text-[12px] font-medium leading-tight text-main"
  >
    <div v-if="hasSolar" class="daily-stat-group flex items-center gap-1.5 mr-0.5">
      <span class="text-solar font-semibold flex items-center gap-1 tabular">☀️ {{ prod }}kWh</span>
      <span v-if="fcToday" class="text-muted text-[10px] font-semibold tracking-tight tabular"
        >[{{ fcToday }}]</span
      >
      <span class="text-muted text-[10px] font-semibold tabular">({{ prodY }})</span>
      <span v-if="fcTomorrow" class="text-muted text-[10px] font-semibold tracking-tight tabular"
        >[{{ fcTomorrow }}]</span
      >
      <span class="text-muted font-medium text-[11px] tracking-tight">{{ solarStr }}</span>
      <span v-if="hasDollars" class="text-battery font-semibold tabular">(${{ dollars }})</span>
    </div>

    <div v-if="hasGrid" class="daily-stat-group flex items-center gap-1.5 mr-0.5">
      <div v-if="hasSolar" class="soft-divider"></div>
      <Zap :size="13" class="text-muted" />
      <span class="font-semibold text-main tabular">{{ grid }}kWh</span>
    </div>

    <TariffCost
      :kwh="ds.grid_kwh"
      :tariff-scope="tariffScope"
      :configured-tariff="state.ui_config?.electricity_tariff"
    />
    <DailyGridEnergy :energy="ds.grid_energy" />

    <div v-if="hasBattery" class="daily-stat-group flex items-center gap-1.5 flex-1 min-w-fit">
      <div v-if="hasSolar || hasGrid" class="soft-divider"></div>
      <BatteryIcon :size="13" class="text-battery" />
      <div class="daily-battery-values flex items-center gap-1.5 tabular">
        <span class="text-muted text-[10px] font-semibold tracking-tight">I:</span>
        <span class="font-semibold text-main">{{ batIn }}kWh</span>
        <span class="text-muted text-[10px] font-semibold">({{ batInY }})</span>

        <span class="text-muted text-[10px] font-semibold tracking-tight ml-0.5">O:</span>
        <span class="font-semibold text-main">{{ batOut }}kWh</span>
        <span class="text-muted text-[10px] font-semibold">({{ batOutY }})</span>

        <span class="text-muted text-[10px] font-semibold tracking-tight ml-0.5">Δ:</span>
        <span
          class="font-semibold"
          :class="parseFloat(batDelta) >= 0 ? 'text-battery' : 'text-consumption'"
          >{{ batDelta }}kWh</span
        >
        <span class="text-muted text-[10px] font-semibold">({{ batDeltaY }})</span>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import { Zap, Battery as BatteryIcon } from '@lucide/vue'
import { state } from '../composables/useInverterState'

import TariffCost from '../tariffs/TariffCost.vue'
import DailyGridEnergy from './DailyGridEnergy.vue'
withDefaults(defineProps<{ tariffScope?: string }>(), {
  tariffScope: 'dashboard',
})

const ds = computed(() => state.value.daily_stats || {})
const fc = computed(() => state.value.solar_forecast || {})

const prod = computed(() => (ds.value.produced_today || 0).toFixed(2))
const prodY = computed(() => (ds.value.produced_yesterday || 0).toFixed(1))
const fcToday = computed(() => (fc.value.today_kwh != null ? fc.value.today_kwh.toFixed(1) : ''))
const fcTomorrow = computed(() =>
  fc.value.tomorrow_kwh != null ? fc.value.tomorrow_kwh.toFixed(1) : ''
)
const dollars = computed(() => (ds.value.produced_dollars || 0).toFixed(2))
const grid = computed(() => (ds.value.grid_kwh || 0).toFixed(2))
const batIn = computed(() => (ds.value.battery_in || 0).toFixed(2))
const batOut = computed(() => (ds.value.battery_out || 0).toFixed(2))
const batInY = computed(() => (ds.value.battery_in_yesterday || 0).toFixed(1))
const batOutY = computed(() => (ds.value.battery_out_yesterday || 0).toFixed(1))
const batDelta = computed(() =>
  (Number.parseFloat(batIn.value) - Number.parseFloat(batOut.value)).toFixed(2)
)
const batDeltaY = computed(() =>
  (Number.parseFloat(batInY.value) - Number.parseFloat(batOutY.value)).toFixed(1)
)

const pvDaily = computed(() => ds.value.pv_inverter_daily || [])
const mpptDaily = computed(() => ds.value.mppt_daily || [])

const solarStr = computed(() => {
  const parts: string[] = pvDaily.value.filter((v) => v > 0).map((v) => v.toFixed(2))
  const mpptTotal = mpptDaily.value.reduce((a, v) => a + v, 0)
  const mpptPart =
    mpptDaily.value.length > 0 ? mpptDaily.value.map((v) => v.toFixed(2)).join('+') : '0.00'
  parts.push(`${mpptTotal.toFixed(2)}(${mpptPart})`)
  return `(${parts.join('+')})`
})

const hasSolar = computed(() => Number.parseFloat(prod.value) > 0)
const hasGrid = computed(() => Number.parseFloat(grid.value) > 0)
const hasBattery = computed(
  () => Number.parseFloat(batIn.value) > 0 || Number.parseFloat(batOut.value) > 0
)
const hasDollars = computed(() => Number.parseFloat(dollars.value) > 0)
</script>
