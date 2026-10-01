<template>
  <div v-if="isMobileApp" class="classic-card mobile-header">
    <div class="mobile-header-row">
      <UiButton
        class="mobile-header-touch mobile-header-dry"
        toggle
        :active="dryRun"
        @click="$emit('send', 'dry_run', { value: !dryRun })"
      >
        <FlaskConical :size="14" /> DRY
      </UiButton>
      <UiButton
        class="mobile-header-touch mobile-header-ess"
        toggle
        :active="essClass === 'on'"
        @click="$emit('send', 'ess_mode')"
      >
        <Zap :size="14" /><span class="mobile-header-label">{{ essText }}</span>
      </UiButton>
      <UiButton
        v-if="headerControls.length > 0"
        class="mobile-header-touch mobile-header-disclosure"
        aria-label="Controls"
        title="Controls"
        :aria-expanded="controlsExpanded"
        :aria-controls="controlsId"
        @click="controlsExpanded = !controlsExpanded"
      >
        <SlidersHorizontal :size="14" />
        <span class="mobile-header-disclosure-label">Controls</span>
        <ChevronUp v-if="controlsExpanded" :size="12" /><ChevronDown v-else :size="12" />
      </UiButton>
      <UiButton
        class="mobile-header-touch mobile-header-icon mobile-header-settings"
        variant="ghost"
        aria-label="Settings"
        title="Settings"
        @click="$emit('open-config')"
      >
        <Settings :size="18" />
      </UiButton>
      <UiButton
        class="mobile-header-touch mobile-header-icon"
        variant="ghost"
        :aria-label="isDark ? 'Light mode' : 'Dark mode'"
        :title="isDark ? 'Light mode' : 'Dark mode'"
        @click="$emit('toggle-theme')"
      >
        <Sun v-if="isDark" :size="18" /><Moon v-else :size="18" />
      </UiButton>
    </div>
    <fieldset
      v-if="headerControls.length > 0"
      v-show="controlsExpanded"
      :id="controlsId"
      class="mobile-header-controls"
      aria-label="Inverter controls"
    >
      <UiButton
        v-for="toggle in headerControls"
        :key="toggle.id"
        class="mobile-header-touch mobile-header-control"
        toggle
        :active="(toggle.state ?? controlStates?.[toggle.id]) === 'on'"
        :unavailable="isToggleUnavailable(toggle.state ?? controlStates?.[toggle.id])"
        :disabled="toggle.disabled"
        :loading="toggle.pending"
        @click="activate(toggle)"
      >
        <span class="mobile-header-label">{{ toggle.label }}</span
        ><span
          v-if="toggle.failed"
          role="alert"
          title="Action unconfirmed; check the current state before retrying."
          >!</span
        >
      </UiButton>
    </fieldset>
    <slot name="actions" />
  </div>
  <div v-else class="classic-card px-1.5 py-1 flex items-center gap-1 w-full">
    <div class="flex flex-wrap gap-1 items-center flex-1 min-w-0">
      <UiButton
        class="min-w-[28px]"
        size="sm"
        toggle
        :active="dryRun"
        @click="$emit('send', 'dry_run', { value: !dryRun })"
      >
        <FlaskConical :size="10" /> DRY
      </UiButton>

      <UiButton
        class="min-w-[45px]"
        size="sm"
        toggle
        :active="essClass === 'on'"
        @click="$emit('send', 'ess_mode')"
      >
        <Zap :size="10" /> {{ essText }}
      </UiButton>

      <template v-if="showHeaderToggles !== false && headerControls.length > 0">
        <div class="soft-divider mx-0.5"></div>

        <UiButton
          v-for="toggle in headerControls"
          :key="toggle.id"
          class="min-w-[55px]"
          size="sm"
          toggle
          :active="(toggle.state ?? controlStates?.[toggle.id]) === 'on'"
          :unavailable="isToggleUnavailable(toggle.state ?? controlStates?.[toggle.id])"
          :disabled="toggle.disabled"
          :loading="toggle.pending"
          @click="activate(toggle)"
        >
          {{ toggle.label
          }}<span
            v-if="toggle.failed"
            role="alert"
            title="Action unconfirmed; check the current state before retrying."
            >!</span
          >
        </UiButton>
      </template>
    </div>

    <slot name="actions" />

    <UiButton
      class="shrink-0"
      :class="isMobileApp ? '!h-11 !min-w-11' : 'min-w-[22px] !px-1.5'"
      variant="ghost"
      aria-label="Settings"
      title="Settings"
      @click="$emit('open-config')"
    >
      <Settings :size="isMobileApp ? 18 : 12" />
    </UiButton>

    <UiButton
      class="min-w-[22px] !px-1.5 shrink-0"
      variant="ghost"
      :title="isDark ? 'Light mode' : 'Dark mode'"
      @click="$emit('toggle-theme')"
    >
      <Sun v-if="isDark" :size="11" />
      <Moon v-else :size="11" />
    </UiButton>
  </div>
</template>

<script setup lang="ts">
import {
  FlaskConical,
  Zap,
  Sun,
  Moon,
  Settings,
  ChevronDown,
  ChevronUp,
  SlidersHorizontal,
} from '@lucide/vue'
import { isMobileApp } from '@features'
import { ref, useId } from 'vue'
import UiButton from './UiButton.vue'
import type { DashboardControlView } from '../dashboardControlView'

defineProps<{
  dryRun: boolean
  essClass: string
  essText: string
  headerControls: DashboardControlView[]
  controlStates: Record<string, string> | undefined
  isDark: boolean
  showHeaderToggles?: boolean
}>()

const emit = defineEmits<{
  send: [action: string, payload?: Record<string, unknown>]
  'toggle-theme': []
  'open-config': []
}>()

function activate(control: DashboardControlView) {
  if (control.disabled || control.pending) return
  if (control.activate) control.activate()
  else emit('send', 'toggle', { entity: control.entity })
}

// Mobile always offers access to published controls. The desktop section
// preference must not hide the only way to open this compact disclosure.
const controlsExpanded = ref(false)
const controlsId = `header-controls-${useId()}`

function isToggleUnavailable(state: string | undefined): boolean {
  return state === 'unavailable'
}
</script>

<style scoped>
.mobile-header {
  display: flex;
  flex-direction: column;
  gap: var(--mobile-gap, 6px);
  width: 100%;
  min-width: 0;
  padding: calc(4px * var(--mobile-density, 1)) calc(6px * var(--mobile-density, 1));
}

.mobile-header-row {
  display: flex;
  align-items: stretch;
  gap: calc(4px * var(--mobile-density, 1));
  min-width: 0;
}

.mobile-header .mobile-header-touch {
  min-width: 44px;
  min-height: 44px;
  height: auto;
  padding: calc(6px * var(--mobile-density, 1));
  gap: calc(4px * var(--mobile-density, 1));
  font-size: var(--mobile-caption-size, 12px);
  line-height: 1.25;
  white-space: normal;
}

.mobile-header-touch :deep(svg) {
  flex-shrink: 0;
}

.mobile-header-dry,
.mobile-header-disclosure,
.mobile-header-icon {
  flex-shrink: 0;
}

.mobile-header-icon {
  width: 44px;
}

.mobile-header-ess {
  flex: 1 1 0;
  max-width: 180px;
}

.mobile-header-settings {
  margin-left: auto;
}

.mobile-header-label {
  min-width: 0;
  overflow-wrap: anywhere;
}

.mobile-header-controls {
  display: flex;
  flex-wrap: wrap;
  gap: var(--mobile-gap, 6px);
  width: 100%;
  min-width: 0;
  margin: 0;
  padding: 0;
  border: 0;
  max-height: min(35svh, 240px);
  overflow-y: auto;
  overscroll-behavior-y: contain;
}

.mobile-header-control {
  flex: 1 1 140px;
  max-width: 100%;
}
</style>
