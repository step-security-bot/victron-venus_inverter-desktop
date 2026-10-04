import { invoke } from '@tauri-apps/api/core'
import { type Ref, ref, watch } from 'vue'

export async function notify(title: string, body: string) {
  try {
    await invoke('send_notification', { title, body })
  } catch {
    // Plugin may not be available — ignore
  }
}

const prevEvChargingKw = ref<number | null>(null)
const prevWaterValve = ref<boolean | null>(null)
const prevPumpSwitch = ref<boolean | null>(null)
let initialized = false

export function initSystemNotifications(
  evChargingKw: Ref<number | null>,
  waterValve: Ref<boolean | null>,
  pumpSwitch: Ref<boolean | null>
) {
  if (initialized) return
  initialized = true

  watch(evChargingKw, (val) => {
    if (val === null || val === undefined) return
    const prev = prevEvChargingKw.value
    if (prev !== null && prev !== undefined) {
      if (prev === 0 && val > 0) {
        void notify('EV Charging Started', `Charging at ${val.toFixed(1)} kW`)
      } else if (prev > 0 && val === 0) {
        void notify('EV Charging Stopped', 'Charging has ended')
      }
    }
    prevEvChargingKw.value = val
  })

  watch(waterValve, (val) => {
    if (val === null || val === undefined) return
    const prev = prevWaterValve.value
    if (prev !== null && prev !== undefined && val !== prev) {
      void notify('Water Valve', val ? 'Valve OPENED' : 'Valve CLOSED')
    }
    prevWaterValve.value = val
  })

  watch(pumpSwitch, (val) => {
    if (val === null || val === undefined) return
    const prev = prevPumpSwitch.value
    if (prev !== null && prev !== undefined && val !== prev) {
      void notify('Water Pump', val ? 'Pump ON' : 'Pump OFF')
    }
    prevPumpSwitch.value = val
  })
}
