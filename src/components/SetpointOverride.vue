<template>
  <span class="setpoint-control inline-flex items-center gap-1 normal-case">
    <button
      ref="trigger"
      type="button"
      class="classic-btn classic-btn-sm !px-1"
      :class="{ 'classic-btn-on': active }"
      title="Setpoint override"
      aria-label="Setpoint override"
      aria-haspopup="dialog"
      :aria-expanded="opened"
      :aria-pressed="ready ? active : undefined"
      @click="openDialog"
    >
      <SlidersHorizontal :size="11" aria-hidden="true" />
    </button>
    <output v-if="active" class="text-[9px] text-accent tabular whitespace-nowrap">
      {{ status?.value }} W · 2s
    </output>
    <span v-if="!ready && !loading" class="text-[9px] text-text-secondary" role="status">
      Status unknown
    </span>
    <span
      v-if="error && !opened"
      class="text-[9px] text-consumption truncate max-w-28"
      role="alert"
      :title="error"
    >
      {{ error }}
    </span>
  </span>

  <Teleport to="body">
    <div v-if="opened" class="override-backdrop">
      <dialog
        :id="dialogId"
        ref="dialog"
        class="classic-card override-dialog"
        open
        aria-modal="true"
        :aria-labelledby="`${dialogId}-title`"
        :aria-describedby="`${dialogId}-help`"
        tabindex="-1"
        @keydown="onDialogKeydown"
      >
        <h2 :id="`${dialogId}-title`" class="classic-section-title">Setpoint override</h2>
        <p :id="`${dialogId}-help`" class="mt-2 text-[11px] text-text-secondary">
          Cerbo applies this setpoint every 2 seconds until stopped, even after this app closes.
          Positive = import; negative = export.
        </p>
        <form class="mt-3" novalidate @submit.prevent="submit">
          <label :for="`${dialogId}-watts`" class="classic-label block mb-1">Watts</label>
          <input
            :id="`${dialogId}-watts`"
            ref="input"
            :value="draft"
            type="text"
            inputmode="decimal"
            class="classic-input !h-8 w-full tabular"
            :disabled="saving"
            :aria-invalid="inputError ? 'true' : undefined"
            :aria-describedby="error ? `${dialogId}-error` : `${dialogId}-help`"
            @input="editDraft"
          />
          <p
            v-if="error"
            :id="`${dialogId}-error`"
            class="mt-2 text-[11px] text-consumption break-words"
            role="alert"
          >
            {{ error }}
          </p>
          <div class="mt-4 flex flex-wrap items-center justify-end gap-2">
            <UiButton
              v-if="active"
              variant="danger"
              class="mr-auto"
              :disabled="saving"
              @click="apply(null)"
            >
              Stop override
            </UiButton>
            <UiButton v-if="!ready" :disabled="saving || loading" @click="initialize">
              Retry
            </UiButton>
            <UiButton :disabled="saving" @click="closeDialog">Cancel</UiButton>
            <UiButton type="submit" variant="primary" :disabled="!ready" :loading="saving">
              OK
            </UiButton>
          </div>
        </form>
      </dialog>
    </div>
  </Teleport>
</template>

<script setup lang="ts">
import { SlidersHorizontal } from '@lucide/vue'
import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { computed, nextTick, onMounted, onUnmounted, ref, useId } from 'vue'
import { acceptsCurrentTransportEvent } from '../composables/transportSession'
import UiButton from './UiButton.vue'

interface OverrideStatus {
  value: number | null
  last_error: string | null
}

const props = defineProps<{ currentSetpoint?: number }>()
const dialogId = `setpoint-override-${useId()}`
const status = ref<OverrideStatus | null>(null)
const active = computed(() => status.value !== null && status.value.value !== null)
const opened = ref(false)
const draft = ref('')
const ready = ref(false)
const loading = ref(false)
const saving = ref(false)
const inputError = ref<string | null>(null)
const commandError = ref<string | null>(null)
const loadError = ref<string | null>(null)
const error = computed(
  () => inputError.value || commandError.value || status.value?.last_error || loadError.value
)
const trigger = ref<HTMLButtonElement>()
const input = ref<HTMLInputElement>()
const dialog = ref<HTMLDialogElement>()
let unlisten: UnlistenFn | undefined
let unlistenConnection: UnlistenFn | undefined
let disposed = false
let eventRevision = 0

function describeError(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause)
}

function receiveStatus(result: OverrideStatus | null) {
  status.value = result
  ready.value = result !== null
  loadError.value =
    result === null ? 'Setpoint override status is unavailable. Retry to refresh.' : null
}

async function refreshStatus() {
  const revision = eventRevision
  try {
    const result = await invoke<OverrideStatus | null>('get_setpoint_override')
    if (!disposed && eventRevision === revision) receiveStatus(result)
  } catch (cause) {
    // A newer live event wins over both an old getter value and its failure.
    if (disposed || eventRevision !== revision) return
    receiveStatus(null)
    loadError.value = describeError(cause)
    throw cause
  }
}

async function initialize() {
  if (loading.value || disposed) return
  loading.value = true
  loadError.value = null
  try {
    if (!unlisten) {
      const unsubscribe = await listen<OverrideStatus | null>(
        'setpoint-override-update',
        (event) => {
          if (disposed) return
          eventRevision += 1
          receiveStatus(event.payload)
        }
      )
      if (disposed) {
        unsubscribe()
        return
      }
      unlisten = unsubscribe
    }
    if (!unlistenConnection) {
      const unsubscribe = await listen<{ connected: boolean; notification_session?: string }>(
        'mqtt-connection-status',
        (event) => {
          if (
            disposed ||
            !acceptsCurrentTransportEvent(event.payload) ||
            event.payload.connected !== false
          )
            return
          eventRevision += 1
          receiveStatus(null)
        }
      )
      if (disposed) {
        unsubscribe()
        return
      }
      unlistenConnection = unsubscribe
    }
    await refreshStatus()
  } catch (cause) {
    if (!disposed && !ready.value) loadError.value = describeError(cause)
  } finally {
    if (!disposed) loading.value = false
  }
}

async function openDialog() {
  const value = status.value?.value ?? props.currentSetpoint
  draft.value = value === undefined || !Number.isFinite(value) ? '' : String(value)
  inputError.value = null
  commandError.value = null
  opened.value = true
  await nextTick()
  input.value?.focus()
  input.value?.select()
}

async function closeDialog() {
  if (saving.value) return
  opened.value = false
  await nextTick()
  trigger.value?.focus()
}

function editDraft(event: Event) {
  draft.value = (event.target as HTMLInputElement).value
  inputError.value = null
}

function submit() {
  if (!ready.value || saving.value) return
  const text = draft.value.trim()
  if (!text) {
    inputError.value = 'Enter a setpoint in watts.'
    return
  }
  const value = Number(text)
  if (
    !/^[+-]?\d+$/.test(text) ||
    !Number.isInteger(value) ||
    value < -2147483648 ||
    value > 2147483647
  ) {
    inputError.value = 'Enter a whole number of watts from -2147483648 to 2147483647.'
    return
  }
  void apply(value)
}

async function apply(value: number | null) {
  if (!ready.value || saving.value) return
  saving.value = true
  commandError.value = null
  inputError.value = null
  let succeeded = false
  const revision = eventRevision
  try {
    const result = await invoke<OverrideStatus>('set_setpoint_override', { value })
    if (disposed) return
    if (eventRevision === revision) receiveStatus(result)
    // Reconcile events that raced the command response without allowing an
    // older getter response to overwrite a newer background error/update.
    try {
      await refreshStatus()
    } catch (cause) {
      if (!disposed) loadError.value = describeError(cause)
    }
    succeeded = ready.value && !status.value?.last_error && !loadError.value
  } catch (cause) {
    if (!disposed) commandError.value = describeError(cause)
  } finally {
    if (!disposed) saving.value = false
  }
  if (succeeded && !disposed) await closeDialog()
}

function onDialogKeydown(event: KeyboardEvent) {
  if (event.key === 'Escape') {
    event.preventDefault()
    event.stopPropagation()
    void closeDialog()
  } else if (event.key === 'Tab') {
    const controls = Array.from(
      dialog.value?.querySelectorAll<HTMLElement>('input:not(:disabled), button:not(:disabled)') ??
        []
    )
    const first = controls[0]
    const last = controls[controls.length - 1]
    if (!first) {
      event.preventDefault()
      dialog.value?.focus()
    } else if (event.shiftKey && document.activeElement === first) {
      event.preventDefault()
      last?.focus()
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault()
      first.focus()
    }
  }
}

onMounted(initialize)
onUnmounted(() => {
  disposed = true
  unlisten?.()
  unlistenConnection?.()
})
</script>

<style scoped>
.override-backdrop {
  position: fixed;
  inset: 0;
  z-index: 1000;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 1rem;
  background: rgb(0 0 0 / 35%);
}

.override-dialog {
  position: relative;
  width: min(100%, 340px);
  margin: 0;
  padding: 1rem;
  color: inherit;
  box-shadow: var(--shadow-elevated);
}
</style>
