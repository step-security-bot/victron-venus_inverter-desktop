<template>
  <dialog
    ref="dialog"
    class="tariff-dialog"
    aria-labelledby="tariff-title"
    @cancel.prevent="!busy && emit('close')"
  >
    <div class="tariff-content">
      <header>
        <div>
          <h2 id="tariff-title">Electricity tariff</h2>
          <p>
            Seasonal energy prices per kWh.
            {{
              destination === 'controller'
                ? 'Saved on the controller and shared by all connected dashboards.'
                : destination === 'local'
                  ? 'Saved for this dashboard on this device.'
                  : 'Apply to the configuration draft, then save the configuration.'
            }}
          </p>
        </div>
        <button
          type="button"
          aria-label="Close tariff editor"
          :disabled="busy"
          @click="emit('close')"
        >
          Close
        </button>
      </header>
      <div class="tariff-fields">
        <label>Name<input v-model="draft.name" maxlength="120" /></label>
        <label>Currency<input v-model="draft.currency" maxlength="3" placeholder="USD" /></label>
        <label>Time zone<input v-model="draft.timeZone" placeholder="America/Los_Angeles" /></label>
        <label
          >Billing period starts on day
          <input
            v-model="billingDay"
            type="number"
            min="1"
            max="31"
            step="1"
            placeholder="Not set"
          />
        </label>
      </div>
      <div class="tariff-schedule">
        <label
          >Schedule
          <select :value="selectedSeason" :disabled="busy" @change="switchSeason">
            <option :value="-1">{{ defaultLabel }}</option>
            <option v-for="(season, index) in draft.seasons" :key="index" :value="index">
              {{ season.name }} · {{ formatMonths(season.months) }}
            </option>
          </select>
        </label>
        <p>
          Seasons follow calendar months in the tariff time zone, independently of the billing date.
          Billing days 29–31 use the last day of shorter months. Leave the day blank if unknown.
        </p>
      </div>
      <div v-if="selectedSeason >= 0" class="tariff-season-details">
        <label
          >Season name<input
            v-model="draft.seasons[selectedSeason].name"
            maxlength="80"
            :disabled="busy"
        /></label>
        <fieldset :disabled="busy">
          <legend>Calendar months</legend>
          <label v-for="(month, index) in MONTHS" :key="month">
            <input
              v-model="draft.seasons[selectedSeason].months"
              type="checkbox"
              :value="index + 1"
            />{{ month }}
          </label>
        </fieldset>
        <button type="button" :disabled="busy" @click="removeSeason">Remove this season</button>
      </div>
      <div class="tariff-actions">
        <button type="button" :disabled="busy || draft.seasons.length >= 12" @click="addSeason">
          Add season
        </button>
        <label
          >Flat price / kWh<input v-model="flat" type="number" step="any" placeholder="Enter price"
        /></label>
        <button type="button" :disabled="busy" @click="fill">Fill selected week</button>
        <label class="file-button"
          >Import tariff<input
            type="file"
            accept=".json,application/json"
            :disabled="busy"
            @change="importFile"
        /></label>
        <button type="button" :disabled="busy" @click="exportFile">Export tariff</button>
      </div>
      <output v-if="message" class="tariff-note">{{ message }}</output>
      <p v-if="draft.source === 'emporia'">
        Source: Emporia{{ draft.reference ? ` · utility plan ${draft.reference}` : '' }}. Review the
        imported copy against the app.
      </p>
      <p class="tariff-help">
        Rows are half-hour periods in the selected time zone; columns are Monday–Sunday. Paste
        prices into the grid or fill the week, then adjust peak periods. All cells need a price;
        zero is allowed.
      </p>
      <TariffSheet :key="sheetVersion" ref="sheet" :rates="selectedRates" @error="error = $event" />
      <p class="tariff-help">
        This estimate covers energy charges only. Tiers, demand charges, fixed fees and taxes are
        not included. The billing date sets the period, not an invoice total. Daily time-of-use cost
        needs interval consumption data.
      </p>
      <p v-if="error" role="alert" class="tariff-error">{{ error }}</p>
      <footer>
        <button v-if="plan" type="button" :disabled="busy" @click="clear">{{ clearLabel }}</button
        ><button type="button" :disabled="busy" @click="emit('close')">Cancel</button
        ><button type="button" class="tariff-save" :disabled="busy" @click="save">
          {{
            busy
              ? 'Working…'
              : destination === 'controller'
                ? 'Save to controller'
                : destination === 'local'
                  ? 'Save tariff'
                  : 'Apply tariff'
          }}
        </button>
      </footer>
    </div>
  </dialog>
</template>
<script setup lang="ts">
import { computed, onMounted, onBeforeUnmount, ref } from 'vue'
import TariffSheet from './TariffSheet.vue'
import {
  activeSeasonIndex,
  MONTHS,
  importTariff,
  newDraft,
  rateGrid,
  validatePlan,
  type RateGrid,
  type TariffDraft,
  type TariffPlan,
} from './model'
import { clearTariff, saveTariff } from './storage'
import { exportTariff } from './export'

function formatMonths(months: number[]): string {
  return months.map((month) => MONTHS[month - 1]).join(', ')
}

const props = withDefaults(
  defineProps<{
    plan: TariffPlan | null
    tariffScope?: string
    persist?: boolean
    /** Caption destination; a save callback can persist locally or on the controller. */
    destination?: 'controller' | 'local' | 'draft'
    clearLabel?: string
    savePlan?: (plan: TariffPlan | null) => Promise<void>
  }>(),
  { tariffScope: 'dashboard', persist: true, clearLabel: 'Clear local tariff' }
)
// Preserve existing callers while allowing native local persistence through savePlan.
const destination = computed(
  () => props.destination ?? (props.savePlan ? 'controller' : props.persist ? 'local' : 'draft')
)
const emit = defineEmits<{ close: []; saved: [plan: TariffPlan | null] }>()
const draft = ref<TariffDraft>(props.plan ? JSON.parse(JSON.stringify(props.plan)) : newDraft())
const dialog = ref<HTMLDialogElement | null>(null)
const sheet = ref<InstanceType<typeof TariffSheet> | null>(null)
const sheetVersion = ref(0)
const error = ref('')
const message = ref('')
const flat = ref('')
const billingDay = ref<string | number>(draft.value.billingDay ?? '')
const selectedSeason = ref(activeSeasonIndex(draft.value))
const selectedRates = computed(() =>
  selectedSeason.value === -1 ? draft.value.rates : draft.value.seasons[selectedSeason.value].rates
)
const defaultLabel = computed(() => {
  const months = MONTHS.filter(
    (_, index) => !draft.value.seasons.some((s) => s.months.includes(index + 1))
  )
  return draft.value.seasons.length === 0
    ? 'All year'
    : `Default · ${months.join(', ') || 'no active months'}`
})
function setRates(rates: RateGrid) {
  if (selectedSeason.value === -1) draft.value.rates = rates
  else draft.value.seasons[selectedSeason.value].rates = rates
}
async function captureSheet() {
  if (!sheet.value) throw new Error('The spreadsheet is still loading.')
  setRates(await sheet.value.getRates())
}
async function switchSeason(event: Event) {
  const select = event.target as HTMLSelectElement
  const next = Number(select.value)
  select.value = String(selectedSeason.value)
  if (busy.value) return
  busy.value = true
  try {
    await captureSheet()
    selectedSeason.value = next
    sheetVersion.value++
    error.value = ''
  } catch (cause) {
    fail(cause)
  } finally {
    busy.value = false
  }
}
const busy = ref(false)
onMounted(() => dialog.value?.showModal())
onBeforeUnmount(() => dialog.value?.close())
const fail = (cause: unknown) => {
  error.value = cause instanceof Error ? cause.message : 'The tariff could not be read.'
}

async function addSeason() {
  if (busy.value || draft.value.seasons.length >= 12) return
  busy.value = true
  try {
    await captureSheet()
    draft.value.seasons.push({
      name: 'New season',
      months: [],
      rates: draft.value.rates.map((row) => [...row]),
    })
    selectedSeason.value = draft.value.seasons.length - 1
    sheetVersion.value++
    error.value = ''
  } catch (cause) {
    fail(cause)
  } finally {
    busy.value = false
  }
}
function removeSeason() {
  if (busy.value || selectedSeason.value < 0) return
  draft.value.seasons.splice(selectedSeason.value, 1)
  selectedSeason.value = -1
  sheetVersion.value++
  error.value = ''
}
function fill() {
  if (busy.value) return
  if (!String(flat.value).trim() || !Number.isFinite(Number(flat.value))) {
    error.value = 'Enter a numeric flat price first.'
    return
  }
  setRates(rateGrid(Number(flat.value)))
  sheetVersion.value++
  error.value = ''
}
async function importFile(event: Event) {
  const input = event.target as HTMLInputElement
  if (busy.value) return
  busy.value = true
  try {
    const file = input.files?.[0]
    if (!file) return
    if (file.size > 100_000) throw new Error('Choose a tariff JSON file smaller than 100 KB.')
    const result = importTariff(JSON.parse(await file.text()))
    draft.value = result.draft
    billingDay.value = result.draft.billingDay ?? ''
    selectedSeason.value = activeSeasonIndex(result.draft)
    message.value = result.message
    sheetVersion.value++
    error.value = ''
  } catch (cause) {
    fail(cause)
  } finally {
    input.value = ''
    busy.value = false
  }
}
async function readPlan() {
  await captureSheet()
  return validatePlan({
    ...draft.value,
    currency: draft.value.currency.toUpperCase(),
    billingDay: String(billingDay.value).trim() === '' ? undefined : Number(billingDay.value),
  })
}
async function save() {
  if (busy.value) return
  busy.value = true
  try {
    const plan = await readPlan()
    if (props.savePlan) await props.savePlan(plan)
    else if (props.persist) saveTariff(props.tariffScope, plan)
    emit('saved', plan)
  } catch (cause) {
    fail(cause)
  } finally {
    busy.value = false
  }
}
async function clear() {
  if (busy.value) return
  busy.value = true
  try {
    if (props.savePlan) await props.savePlan(null)
    else if (props.persist) clearTariff(props.tariffScope)
    emit('saved', null)
  } catch (cause) {
    fail(cause)
  } finally {
    busy.value = false
  }
}
async function exportFile() {
  if (busy.value) return
  busy.value = true
  try {
    const plan = await readPlan()
    message.value = (await exportTariff(plan)) ? 'Tariff exported.' : 'Export cancelled.'
    error.value = ''
  } catch (cause) {
    fail(cause)
  } finally {
    busy.value = false
  }
}
</script>
<style scoped>
.tariff-dialog {
  position: fixed;
  inset: 0;
  margin: auto;
  width: min(1100px, 96vw);
  max-width: 96vw;
  max-height: 94dvh;
  padding: 0;
  border: 1px solid #596372;
  border-radius: 12px;
  color: #e5eaf0;
  background: #171d26;
  font: 14px/1.5 system-ui;
  user-select: text;
}
.tariff-dialog::backdrop {
  background: #0009;
}
.tariff-content {
  padding: 20px;
}
.tariff-content header,
.tariff-content footer {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
}
.tariff-content h2 {
  font-size: 22px;
  font-weight: 650;
  margin: 0;
}
.tariff-content p,
.tariff-content output {
  margin: 8px 0;
  color: #b9c4d2;
}
.tariff-fields {
  display: grid;
  grid-template-columns: 2fr 1fr 2fr 1fr;
  gap: 12px;
  margin: 16px 0;
}
.tariff-content label {
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.tariff-content input,
.tariff-content select {
  min-width: 0;
  background: #242e3c;
  color: #f5f7fa;
  padding: 8px;
  border: 1px solid #64748b;
  border-radius: 5px;
}
.tariff-schedule {
  margin: 12px 0;
}
.tariff-schedule p {
  font-size: 12px;
}
.tariff-season-details {
  margin: 12px 0;
}
.tariff-season-details fieldset {
  display: flex;
  flex-wrap: wrap;
  gap: 12px;
  margin: 8px 0;
}
.tariff-season-details fieldset label {
  flex-direction: row;
  align-items: center;
}
.tariff-actions {
  display: flex;
  align-items: end;
  flex-wrap: wrap;
  gap: 10px;
}
.tariff-content button,
.file-button {
  border: 1px solid #64748b;
  border-radius: 5px;
  padding: 8px 12px;
  background: #283546;
  color: white;
  cursor: pointer;
}
.file-button input {
  max-width: 220px;
  font-size: 12px;
  padding: 4px;
}
.tariff-help {
  font-size: 12px;
}
.tariff-note {
  display: block;
  padding: 10px;
  background: #273649;
}
.tariff-content .tariff-error {
  color: #ffb4a8;
}
.tariff-content footer {
  justify-content: flex-end;
  margin-top: 14px;
}
.tariff-content .tariff-save {
  background: #25634b;
}
.tariff-content button:disabled {
  opacity: 0.6;
}
.tariff-content :focus-visible {
  outline: 2px solid #93c5fd;
  outline-offset: 2px;
}
@media (max-width: 600px) {
  .tariff-content {
    padding: 12px;
  }
  .tariff-fields {
    grid-template-columns: 1fr 1fr;
  }
  .tariff-fields label:last-child {
    grid-column: 1/-1;
  }
}
</style>
