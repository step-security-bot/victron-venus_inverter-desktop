<template>
  <div ref="root" class="ess-mode-control" :class="{ 'ess-mode-mobile': mobile }">
    <UiButton
      class="ess-mode-trigger"
      :size="mobile ? 'md' : 'sm'"
      :active="active"
      :aria-label="`ESS mode: ${currentLabel}`"
      aria-haspopup="menu"
      :aria-expanded="open"
      :aria-controls="menuId"
      :aria-busy="!!pending || undefined"
      @click="toggleMenu"
      @keydown.down.prevent="showMenu"
      @keydown.up.prevent="showMenu"
    >
      <Zap class="ess-mode-icon" :size="mobile ? 14 : 10" />
      <span class="ess-mode-label">{{ currentLabel }}</span>
      <Loader2 v-if="pending" :size="12" class="animate-spin" />
      <ChevronDown v-else :size="12" :class="{ 'ess-mode-chevron-open': open }" />
    </UiButton>
    <Teleport to="body">
      <div v-if="open" ref="panel" class="ess-mode-panel" :style="position">
        <div class="ess-mode-title">ESS mode</div>
        <div :id="menuId" role="menu" aria-label="ESS mode" @keydown="navigate">
          <button
            v-for="option in ESS_MODES"
            :key="option.id"
            type="button"
            role="menuitemradio"
            class="ess-mode-option"
            :class="{ 'ess-mode-selected': selected === option.id }"
            :aria-checked="selected === option.id"
            :aria-disabled="!!unavailable || !!pending"
            :tabindex="selected === option.id ? 0 : -1"
            @click="choose(option.id)"
          >
            <Check v-if="selected === option.id" :size="17" />
            <span v-else class="ess-mode-check-space" />
            <span>{{ option.label }}</span>
          </button>
        </div>
        <p v-if="error" class="ess-mode-message ess-mode-error" role="alert">{{ error }}</p>
        <p v-else-if="pending" class="ess-mode-message" role="status">
          Waiting for the controller…
        </p>
        <p v-else-if="unavailable" class="ess-mode-message" role="status">{{ unavailable }}</p>
        <p v-else class="ess-mode-message">
          Off / On changes inverter power. Other choices keep the power switch unchanged.
        </p>
      </div>
    </Teleport>
  </div>
</template>

<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, useId, watch } from 'vue'
import { Check, ChevronDown, Loader2, Zap } from '@lucide/vue'
import UiButton from './UiButton.vue'
import { ESS_MODES, selectedEssMode, type EssModeId, type EssModeState } from '../essMode'
import { sendControlAction } from '../composables/useDashboardControls'

const props = defineProps<{
  mode?: EssModeState
  label: string
  active: boolean
  mobile?: boolean
  connected?: boolean
  fresh?: boolean
  dryRun: boolean
}>()
const root = ref<HTMLElement>()
const panel = ref<HTMLElement>()
const open = ref(false)
const menuId = `ess-menu-${useId()}`
const position = ref<Record<string, string>>({})
const pending = ref<string | null>(null)
const error = ref('')
let timeout: ReturnType<typeof setTimeout> | undefined
const selected = computed(() => selectedEssMode(props.mode))
const currentLabel = computed(
  () => ESS_MODES.find((option) => option.id === selected.value)?.short ?? props.label
)
const unavailable = computed(() => {
  if (!props.connected || !props.fresh) return 'Waiting for fresh ESS status.'
  if (!props.mode?.selection_supported) return 'Update inverter-control to enable mode selection.'
  if (props.dryRun) return 'Mode changes are unavailable in DRY mode.'
  return ''
})
function trigger() {
  return root.value?.querySelector('button')
}
function finish() {
  clearTimeout(timeout)
  pending.value = null
}
watch(
  () => props.mode,
  (mode) => {
    if (pending.value && mode?.request_id === pending.value) {
      error.value = mode.error || ''
      finish()
    }
  },
  { deep: true }
)
async function showMenu() {
  const rect = trigger()?.getBoundingClientRect()
  if (!rect) return
  const width = Math.min(320, window.innerWidth - 16)
  const top = rect.bottom + 6
  position.value = {
    width: `${width}px`,
    left: `${Math.max(8, Math.min(rect.left, window.innerWidth - width - 8))}px`,
    top: `${top}px`,
    maxHeight: `${Math.max(44, window.innerHeight - top - 8)}px`,
  }
  open.value = true
  await nextTick()
  const buttons = panel.value?.querySelectorAll<HTMLButtonElement>('[role="menuitemradio"]')
  const index = Math.max(
    0,
    ESS_MODES.findIndex((option) => option.id === selected.value)
  )
  buttons?.[index]?.focus()
}
function closeMenu(restoreFocus = false) {
  open.value = false
  if (restoreFocus) trigger()?.focus()
}
function toggleMenu() {
  if (open.value) closeMenu(true)
  else void showMenu()
}
function outside(event: PointerEvent) {
  const target = event.target as Node
  if (!root.value?.contains(target) && !panel.value?.contains(target)) closeMenu()
}
function navigate(event: KeyboardEvent) {
  if (event.key === 'Escape' || event.key === 'Tab') {
    if (event.key === 'Escape') event.preventDefault()
    closeMenu(true)
    return
  }
  const buttons = Array.from(
    panel.value?.querySelectorAll<HTMLButtonElement>('[role="menuitemradio"]') ?? []
  )
  const index = buttons.indexOf(document.activeElement as HTMLButtonElement)
  const next = {
    ArrowDown: (index + 1) % buttons.length,
    ArrowUp: (index + buttons.length - 1) % buttons.length,
    Home: 0,
    End: buttons.length - 1,
  }[event.key]
  if (next !== undefined) {
    event.preventDefault()
    buttons[next]?.focus()
  }
}
async function choose(mode: EssModeId) {
  if (unavailable.value || pending.value) return
  if (mode === selected.value) {
    closeMenu(true)
    return
  }
  error.value = ''
  const requestId = crypto.randomUUID()
  pending.value = requestId
  timeout = setTimeout(() => {
    error.value = 'Change unconfirmed. Check the current mode before retrying.'
    finish()
  }, 20_000)
  try {
    await sendControlAction('set_ess_mode', { mode, request_id: requestId })
  } catch (cause) {
    if (pending.value === requestId) {
      error.value = `Change unconfirmed: ${String(cause)}`
      finish()
    }
  }
}
function viewportChanged() {
  closeMenu()
}
onMounted(() => {
  document.addEventListener('pointerdown', outside)
  window.addEventListener('resize', viewportChanged)
})
onBeforeUnmount(() => {
  finish()
  document.removeEventListener('pointerdown', outside)
  window.removeEventListener('resize', viewportChanged)
})
</script>

<style>
.ess-mode-control {
  display: flex;
  min-width: 0;
}
.ess-mode-trigger {
  min-width: 45px;
}
.ess-mode-trigger svg {
  flex-shrink: 0;
}
.ess-mode-label {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.ess-mode-mobile {
  min-width: 80px;
  flex: 1 1 0;
  max-width: 180px;
}
.ess-mode-mobile .ess-mode-trigger {
  width: 100%;
  min-width: 44px;
  min-height: 44px;
  height: auto;
  padding: calc(6px * var(--mobile-density, 1));
  gap: 4px;
  white-space: normal;
  font-size: var(--mobile-caption-size, 12px);
  line-height: 1.25;
}
.ess-mode-chevron-open {
  transform: rotate(180deg);
}
.ess-mode-panel {
  position: fixed;
  z-index: 1000;
  box-sizing: border-box;
  overflow-y: auto;
  overscroll-behavior: contain;
  padding: 6px;
  border: 1px solid var(--color-border-card);
  border-radius: 12px;
  background: var(--color-bg-elevated);
  color: var(--color-text-main);
  box-shadow: 0 12px 36px #0003;
}
.ess-mode-title {
  padding: 7px 12px 8px;
  font-size: 12px;
  font-weight: 600;
  color: var(--color-text-secondary);
}
.ess-mode-option {
  display: flex;
  align-items: center;
  gap: 10px;
  width: 100%;
  min-height: 44px;
  padding: 9px 12px;
  text-align: left;
  border: 0;
  border-radius: 7px;
  background: transparent;
  color: inherit;
  font-size: 14px;
  line-height: 1.4;
  cursor: pointer;
}
.ess-mode-option:hover,
.ess-mode-option:focus-visible {
  background: var(--color-bg-muted);
  outline: 2px solid var(--color-accent);
  outline-offset: -2px;
}
.ess-mode-selected {
  background: color-mix(in srgb, var(--color-accent) 16%, transparent);
  color: var(--color-accent);
  font-weight: 600;
}
.ess-mode-option[aria-disabled='true'] {
  cursor: default;
  opacity: 0.6;
}
.ess-mode-check-space,
.ess-mode-option svg {
  width: 17px;
  flex-shrink: 0;
}
.ess-mode-message {
  margin: 6px 12px 5px;
  font-size: 12px;
  line-height: 1.4;
  color: var(--color-text-secondary);
}
.ess-mode-error {
  color: #dc2626;
}
.dark .ess-mode-panel {
  background: var(--color-dark-bg-elevated);
  color: var(--color-dark-text-main);
  border-color: var(--color-dark-border-card);
}
.dark .ess-mode-title,
.dark .ess-mode-message {
  color: var(--color-dark-text-secondary);
}
.dark .ess-mode-error {
  color: #f87171;
}
.dark .ess-mode-option:hover,
.dark .ess-mode-option:focus-visible {
  background: var(--color-dark-bg-muted);
}
@media (max-width: 360px) {
  .ess-mode-mobile .ess-mode-icon {
    display: none;
  }
}
</style>
