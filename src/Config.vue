<template>
  <ErrorBoundary>
    <div class="app-shell h-screen flex flex-col select-none overflow-hidden">
      <!-- macOS style titlebar (simulated) -->
      <div
        class="config-titlebar flex items-center justify-between px-3 shrink-0"
        :class="isMobileApp ? 'h-14' : 'h-[36px]'"
      >
        <div class="flex items-center gap-2">
          <Settings :size="14" class="text-muted" />
          <span class="text-[12px] font-semibold tracking-tight text-main">Configuration</span>
        </div>
        <div class="flex items-center gap-1.5">
          <button
            type="button"
            @click="handleReset"
            class="p-1 rounded-md hover:bg-black/[0.04] dark:hover:bg-white/[0.06] transition-colors text-muted hover:text-main"
            title="Reset to defaults"
          >
            <RotateCcw :size="12" />
          </button>
          <UiButton
            variant="primary"
            size="sm"
            class="!h-[22px] gap-1"
            :loading="saving"
            :disabled="!configLoaded"
            title="Save changes"
            @click="handleSave"
          >
            <Save v-if="!saving" :size="10" />
            <span>Save</span>
          </UiButton>
          <button
            type="button"
            @click="handleClose"
            aria-label="Close settings"
            :class="{ '!h-11 !w-11': isMobileApp }"
            class="p-1 rounded-md hover:bg-consumption hover:text-white transition-colors text-muted hover:text-main"
          >
            <X :size="12" />
          </button>
        </div>
      </div>

      <!-- Main Layout -->
      <div class="flex-1 flex overflow-hidden min-h-0" :class="{ 'flex-col': isMobileApp }">
        <!-- Sidebar -->
        <div
          class="config-sidebar p-1.5 flex gap-0.5 shrink-0"
          :class="isMobileApp ? 'overflow-x-auto' : 'w-[160px] flex-col'"
        >
          <button
            type="button"
            v-for="s in sections"
            :key="s.id"
            @click="activeTab = s.id"
            class="config-nav-item"
            :class="{
              'config-nav-item-active': activeTab === s.id,
              'shrink-0 whitespace-nowrap !min-h-11': isMobileApp,
            }"
          >
            <component :is="s.icon" :size="14" />
            {{ s.label }}
          </button>
        </div>

        <!-- Content Area -->
        <div
          class="flex-1 min-w-0 overflow-y-auto bg-[#f7f7f8] dark:bg-[#121214]"
          :class="isMobileApp ? 'p-3' : 'p-5'"
        >
          <div class="max-w-xl mx-auto flex flex-col gap-6">
            <!-- MQTT Section -->
            <div v-if="activeTab === 'mqtt'" class="flex flex-col gap-4">
              <header class="border-b border-black/[0.06] dark:border-white/[0.07] pb-2">
                <h2 class="classic-section-title">Broker Settings</h2>
              </header>

              <div class="grid grid-cols-2 gap-3">
                <div class="flex flex-col gap-1">
                  <label for="mqtt_host" class="classic-label px-1">Host</label>
                  <input
                    id="mqtt_host"
                    v-model="config.mqtt_host"
                    type="text"
                    class="classic-input w-full"
                    placeholder="Cerbo.local"
                  />
                </div>
                <div class="flex flex-col gap-1">
                  <label for="mqtt_port" class="classic-label px-1">Port</label>
                  <input
                    id="mqtt_port"
                    v-model.number="config.mqtt_port"
                    type="number"
                    class="classic-input w-full"
                  />
                </div>
                <div class="flex flex-col gap-1">
                  <label for="mqtt_login" class="classic-label px-1">Username</label>
                  <input
                    id="mqtt_login"
                    v-model="config.mqtt_login"
                    type="text"
                    class="classic-input w-full"
                    placeholder="Optional"
                  />
                </div>
                <div class="flex flex-col gap-1">
                  <label for="mqtt_password" class="classic-label px-1">Password</label>
                  <input
                    id="mqtt_password"
                    v-model="config.mqtt_password"
                    type="password"
                    class="classic-input w-full"
                    placeholder="Optional"
                  />
                </div>
              </div>

              <label class="flex items-center gap-2 text-[12px]">
                <input id="mqtt_tls" v-model="config.mqtt_tls" type="checkbox" />
                TLS encrypted connection
              </label>
              <p class="text-[11px] text-muted">
                TLS is required when using a username or password.
              </p>

              <div class="flex flex-col gap-1">
                <label for="portal_id" class="classic-label px-1">VRM Portal ID</label>
                <input
                  id="portal_id"
                  v-model="config.portal_id"
                  type="text"
                  class="classic-input w-full"
                  placeholder="e.g. a1b2c3d4e5f6"
                />
                <p class="text-[10px] text-muted px-1 italic">Keep-alive for Cerbo GX.</p>
              </div>

              <div class="flex flex-col gap-2" role="radiogroup" aria-label="Interface Theme">
                <span class="classic-label px-1">Interface Theme</span>
                <div class="flex gap-1">
                  <UiButton
                    class="flex-1"
                    toggle
                    :active="config.color_scheme === 'dark'"
                    @click="config.color_scheme = 'dark'"
                  >
                    Dark
                  </UiButton>
                  <UiButton
                    class="flex-1"
                    toggle
                    :active="config.color_scheme === 'light'"
                    @click="config.color_scheme = 'light'"
                  >
                    Light
                  </UiButton>
                </div>
              </div>
            </div>

            <TariffConfiguration v-if="activeTab === 'tariff'" />

            <!-- HTTPS inverter-gateway, with optional Cloudflare Access -->
            <div v-if="activeTab === 'gateway'" class="flex flex-col gap-4">
              <header class="border-b border-black/[0.06] dark:border-white/[0.07] pb-2">
                <h2 class="classic-section-title">Remote Gateway</h2>
                <p class="text-[10px] text-muted mt-1">
                  Reach inverter-gateway over HTTPS on your network or through a public endpoint.
                  Cloudflare Access credentials are needed only when Access protects that URL.
                </p>
              </header>

              <label class="flex items-center gap-2 px-1 cursor-pointer">
                <input
                  id="gateway_enabled"
                  v-model="config.gateway_enabled"
                  type="checkbox"
                  class="rounded border-black/20 dark:border-white/20"
                />
                <span class="text-[12px] text-main">Enable remote gateway</span>
              </label>

              <div class="flex flex-col gap-3 p-3 classic-inset !rounded-lg !p-3">
                <div class="flex flex-col gap-1">
                  <label for="gateway_url" class="classic-label px-1">Gateway URL</label>
                  <input
                    id="gateway_url"
                    v-model="config.gateway_url"
                    type="url"
                    class="classic-input w-full"
                    placeholder="https://victron.example.com"
                    autocomplete="off"
                  />
                  <p class="text-[10px] text-muted px-1 italic">
                    Local or public HTTPS URL with a certificate trusted by your system. Trailing
                    slash optional.
                  </p>
                </div>

                <div class="flex flex-col gap-1">
                  <label for="gateway_access_client_id" class="classic-label px-1"
                    >Cloudflare Access Client ID (optional)</label
                  >
                  <input
                    id="gateway_access_client_id"
                    v-model="config.gateway_access_client_id"
                    type="text"
                    class="classic-input w-full"
                    placeholder="….access"
                    autocomplete="off"
                  />
                </div>

                <div class="flex flex-col gap-1">
                  <label for="gateway_access_client_secret" class="classic-label px-1"
                    >Cloudflare Access Client Secret (optional)</label
                  >
                  <input
                    id="gateway_access_client_secret"
                    v-model="config.gateway_access_client_secret"
                    type="password"
                    class="classic-input w-full"
                    placeholder="Service token secret"
                    autocomplete="new-password"
                  />
                  <p class="text-[10px] text-muted px-1 italic">
                    Leave both Access fields blank for a direct HTTPS gateway, or supply both.
                  </p>
                </div>

                <div class="flex flex-col gap-1">
                  <label for="gateway_api_token" class="classic-label px-1"
                    >Gateway API Token</label
                  >
                  <input
                    id="gateway_api_token"
                    v-model="config.gateway_api_token"
                    type="password"
                    class="classic-input w-full"
                    placeholder="Bearer token (GATEWAY_API_TOKEN)"
                    autocomplete="new-password"
                  />
                  <p class="text-[10px] text-muted px-1 italic">
                    Enter the API token configured on your gateway.
                  </p>
                </div>

                <div class="flex items-center gap-2 pt-1">
                  <UiButton class="flex-1" :loading="testingGateway" @click="testGatewayConnection">
                    Test connection
                  </UiButton>
                </div>
                <p
                  v-if="gatewayTestResult"
                  class="text-[11px] px-1"
                  :class="gatewayTestSuccess ? 'text-generation' : 'text-consumption'"
                >
                  {{ gatewayTestResult }}
                </p>
              </div>
            </div>

            <FeaturePluginManager v-if="activeTab === featurePluginManagerTabId" />
            <div v-if="activeTab === 'devices'" class="flex flex-col gap-4">
              <!-- Cerbo Water & EV (MQTT instance discovery) -->
              <div class="flex flex-col gap-3 p-3 classic-inset !rounded-lg !p-3">
                <h3 class="classic-subsection-title">Cerbo Water &amp; EV</h3>
                <p class="text-[10px] text-muted px-1">
                  Instances are auto-discovered from the Cerbo GX MQTT portal (dbus-pump / dbus-ev /
                  dbus-evcharger) after connect. Select which instance feeds the dashboard; values
                  come directly from Cerbo MQTT.
                </p>

                <div class="grid grid-cols-1 sm:grid-cols-2 gap-3">
                  <div class="flex flex-col gap-1.5">
                    <span class="classic-label px-1">Water tanks</span>
                    <p v-if="!discoveredTanks.length" class="text-[10px] text-muted px-1 italic">
                      Waiting for tank/+/… MQTT topics…
                    </p>
                    <button
                      v-for="item in discoveredTanks"
                      :key="'tank-' + item.instance"
                      type="button"
                      class="flex items-center justify-between gap-2 px-2 py-1.5 rounded-lg border text-left text-[11px] transition-colors"
                      :class="
                        config.water_tank_instance === item.instance
                          ? 'border-accent/60 bg-accent/10 text-main'
                          : 'border-black/[0.08] dark:border-white/[0.1] bg-white dark:bg-[#1c1c1e] text-main hover:border-accent/40'
                      "
                      @click="config.water_tank_instance = item.instance"
                    >
                      <span class="font-bold truncate">{{
                        item.name || 'Tank ' + item.instance
                      }}</span>
                      <span class="text-muted shrink-0">#{{ item.instance }}</span>
                    </button>
                  </div>

                  <div class="flex flex-col gap-1.5">
                    <span class="classic-label px-1">Pumps &amp; valves</span>
                    <p v-if="!discoveredPumps.length" class="text-[10px] text-muted px-1 italic">
                      Waiting for pump/+/… MQTT topics…
                    </p>
                    <div
                      v-for="item in discoveredPumps"
                      :key="'pump-' + item.instance"
                      class="flex flex-col gap-1 px-2 py-1.5 rounded-lg border border-black/[0.08] dark:border-white/[0.1] bg-white dark:bg-[#1c1c1e]"
                    >
                      <div class="flex items-center justify-between gap-2 text-[11px] text-main">
                        <span class="font-bold truncate">{{
                          item.name || 'Pump ' + item.instance
                        }}</span>
                        <span class="text-muted shrink-0">#{{ item.instance }}</span>
                      </div>
                      <div class="flex gap-2">
                        <button
                          type="button"
                          class="flex-1 py-0.5 rounded text-[10px] font-semibold border transition-colors"
                          :class="
                            config.water_pump_instance === item.instance
                              ? 'border-accent/60 bg-accent/10 text-main'
                              : 'border-black/[0.08] dark:border-white/[0.1] text-muted'
                          "
                          @click="config.water_pump_instance = item.instance"
                        >
                          Use as pump
                        </button>
                        <button
                          type="button"
                          class="flex-1 py-0.5 rounded text-[10px] font-semibold border transition-colors"
                          :class="
                            config.water_valve_instance === item.instance
                              ? 'border-accent/60 bg-accent/10 text-main'
                              : 'border-black/[0.08] dark:border-white/[0.1] text-muted'
                          "
                          @click="config.water_valve_instance = item.instance"
                        >
                          Use as valve
                        </button>
                      </div>
                    </div>
                  </div>

                  <div class="flex flex-col gap-1.5">
                    <span class="classic-label px-1">EV vehicles</span>
                    <p v-if="!discoveredEvs.length" class="text-[10px] text-muted px-1 italic">
                      Waiting for ev/+/… MQTT topics…
                    </p>
                    <button
                      v-for="item in discoveredEvs"
                      :key="'ev-' + item.instance"
                      type="button"
                      class="flex items-center justify-between gap-2 px-2 py-1.5 rounded-lg border text-left text-[11px] transition-colors"
                      :class="
                        config.ev_instance === item.instance
                          ? 'border-accent/60 bg-accent/10 text-main'
                          : 'border-black/[0.08] dark:border-white/[0.1] bg-white dark:bg-[#1c1c1e] text-main hover:border-accent/40'
                      "
                      @click="config.ev_instance = item.instance"
                    >
                      <span class="font-bold truncate">{{
                        item.name || 'EV ' + item.instance
                      }}</span>
                      <span class="text-muted shrink-0">#{{ item.instance }}</span>
                    </button>
                  </div>

                  <div class="flex flex-col gap-1.5">
                    <span class="classic-label px-1">EV chargers</span>
                    <p
                      v-if="!discoveredEvchargers.length"
                      class="text-[10px] text-muted px-1 italic"
                    >
                      Waiting for evcharger/+/… MQTT topics…
                    </p>
                    <button
                      v-for="item in discoveredEvchargers"
                      :key="'evc-' + item.instance"
                      type="button"
                      class="flex items-center justify-between gap-2 px-2 py-1.5 rounded-lg border text-left text-[11px] transition-colors"
                      :class="
                        config.evcharger_instance === item.instance
                          ? 'border-accent/60 bg-accent/10 text-main'
                          : 'border-black/[0.08] dark:border-white/[0.1] bg-white dark:bg-[#1c1c1e] text-main hover:border-accent/40'
                      "
                      @click="config.evcharger_instance = item.instance"
                    >
                      <span class="font-bold truncate">{{
                        item.name || 'Charger ' + item.instance
                      }}</span>
                      <span class="text-muted shrink-0">#{{ item.instance }}</span>
                    </button>
                  </div>
                </div>

                <details class="mt-1">
                  <summary
                    class="text-[10px] text-muted px-1 cursor-pointer select-none hover:text-main"
                  >
                    Manual instance override
                  </summary>
                  <div class="flex flex-col gap-2 mt-2 px-1">
                    <div class="flex flex-wrap items-center gap-3">
                      <label for="water_tank_instance" class="classic-label">Tank</label>
                      <input
                        id="water_tank_instance"
                        v-model.number="config.water_tank_instance"
                        type="number"
                        min="0"
                        class="classic-input w-20"
                      />
                      <label for="water_pump_instance" class="classic-label">Pump</label>
                      <input
                        id="water_pump_instance"
                        v-model.number="config.water_pump_instance"
                        type="number"
                        min="1"
                        class="classic-input w-20"
                      />
                      <label for="water_valve_instance" class="classic-label">Valve</label>
                      <input
                        id="water_valve_instance"
                        v-model.number="config.water_valve_instance"
                        type="number"
                        min="1"
                        class="classic-input w-20"
                      />
                    </div>
                    <div class="flex flex-wrap items-center gap-3">
                      <label for="ev_instance" class="classic-label">EV vehicle</label>
                      <input
                        id="ev_instance"
                        v-model.number="config.ev_instance"
                        type="number"
                        min="1"
                        class="classic-input w-20"
                      />
                      <label for="evcharger_instance" class="classic-label">EV charger</label>
                      <input
                        id="evcharger_instance"
                        v-model.number="config.evcharger_instance"
                        type="number"
                        min="1"
                        class="classic-input w-20"
                      />
                    </div>
                    <p class="text-[10px] text-muted italic">
                      Optional fallback when discovery is empty. Saved values are kept when they
                      still appear in the discovered list; otherwise the first found instance is
                      used for live tiles.
                    </p>
                  </div>
                </details>
              </div>
            </div>

            <!-- Sections Visibility -->
            <div v-if="activeTab === 'sections'" class="flex flex-col gap-4">
              <header class="border-b border-black/[0.06] dark:border-white/[0.07] pb-2">
                <h2 class="classic-section-title">Section Visibility</h2>
              </header>

              <!-- Group 1: Inverter & Solar -->
              <div class="flex flex-col gap-2 p-3 classic-inset !rounded-lg !p-3">
                <h3 class="classic-subsection-title">Inverter & Solar</h3>
                <label
                  class="flex items-center gap-2 px-2 py-1.5 rounded-lg border border-black/[0.08] dark:border-white/[0.1] bg-white dark:bg-[#1c1c1e] cursor-pointer group hover:border-accent/40 transition-colors"
                >
                  <input
                    type="checkbox"
                    v-model="config.show_batteries"
                    class="rounded border-slate-300 text-accent focus:ring-accent"
                  />
                  <span class="text-[11px] font-bold text-main">Batteries</span>
                </label>
                <label
                  class="flex items-center gap-2 px-2 py-1.5 rounded-lg border border-black/[0.08] dark:border-white/[0.1] bg-white dark:bg-[#1c1c1e] cursor-pointer group hover:border-accent/40 transition-colors"
                >
                  <input
                    type="checkbox"
                    v-model="config.show_solar_production"
                    class="rounded border-slate-300 text-accent focus:ring-accent"
                  />
                  <span class="text-[11px] font-bold text-main">Solar Production</span>
                </label>
              </div>

              <!-- Group 2: Energy Stats -->
              <div class="flex flex-col gap-2 p-3 classic-inset !rounded-lg !p-3">
                <h3 class="classic-subsection-title">Energy Stats</h3>
                <label
                  class="flex items-center gap-2 px-2 py-1.5 rounded-lg border border-black/[0.08] dark:border-white/[0.1] bg-white dark:bg-[#1c1c1e] cursor-pointer group hover:border-accent/40 transition-colors"
                >
                  <input
                    type="checkbox"
                    v-model="config.show_active_loads"
                    class="rounded border-slate-300 text-accent focus:ring-accent"
                  />
                  <span class="text-[11px] font-bold text-main">Active Loads</span>
                </label>
                <label
                  class="flex items-center gap-2 px-2 py-1.5 rounded-lg border border-black/[0.08] dark:border-white/[0.1] bg-white dark:bg-[#1c1c1e] cursor-pointer group hover:border-accent/40 transition-colors"
                >
                  <input
                    type="checkbox"
                    v-model="config.show_daily_stats"
                    class="rounded border-slate-300 text-accent focus:ring-accent"
                  />
                  <span class="text-[11px] font-bold text-main">Daily Stats</span>
                </label>
              </div>

              <!-- Group 3: Home Area -->
              <div class="flex flex-col gap-2 p-3 classic-inset !rounded-lg !p-3">
                <h3 class="classic-subsection-title">Home Area</h3>
                <label
                  class="flex items-center gap-2 px-2 py-1.5 rounded-lg border border-black/[0.08] dark:border-white/[0.1] bg-white dark:bg-[#1c1c1e] cursor-pointer group hover:border-accent/40 transition-colors"
                >
                  <input
                    type="checkbox"
                    v-model="config.show_ev"
                    class="rounded border-slate-300 text-accent focus:ring-accent"
                  />
                  <span class="text-[11px] font-bold text-main">EV</span>
                </label>

                <label
                  class="flex items-center gap-2 px-2 py-1.5 rounded-lg border border-black/[0.08] dark:border-white/[0.1] bg-white dark:bg-[#1c1c1e] cursor-pointer group hover:border-accent/40 transition-colors"
                >
                  <input
                    type="checkbox"
                    v-model="config.show_home_section"
                    class="rounded border-slate-300 text-accent focus:ring-accent"
                  />
                  <span class="text-[11px] font-bold text-main">Home Buttons</span>
                </label>
                <label
                  v-if="!isMobileApp"
                  class="flex items-center gap-2 px-2 py-1.5 rounded-lg border border-black/[0.08] dark:border-white/[0.1] bg-white dark:bg-[#1c1c1e] cursor-pointer group hover:border-accent/40 transition-colors"
                >
                  <input
                    type="checkbox"
                    v-model="config.show_header_toggles"
                    class="rounded border-slate-300 text-accent focus:ring-accent"
                  />
                  <span class="text-[11px] font-bold text-main">{{
                    $t('config.headerToggles')
                  }}</span>
                </label>

                <!-- App Settings -->
                <div class="flex flex-col gap-2 p-3 classic-inset !rounded-lg !p-3">
                  <h3 class="classic-subsection-title">App Settings</h3>
                  <label
                    class="flex items-center gap-2 px-2 py-1.5 rounded-lg border border-black/[0.08] dark:border-white/[0.1] bg-white dark:bg-[#1c1c1e] cursor-pointer group hover:border-accent/40 transition-colors"
                  >
                    <input
                      type="checkbox"
                      :checked="config.auto_start"
                      @change="config.auto_start = ($event.target as HTMLInputElement).checked"
                      class="rounded border-slate-300 text-accent focus:ring-accent"
                    />
                    <span class="text-[11px] font-bold text-main"> Launch at system startup </span>
                  </label>
                </div>
              </div>
            </div>

            <!-- Backup Section -->
            <div v-if="activeTab === 'backup'" class="flex flex-col gap-4">
              <header class="border-b border-black/[0.06] dark:border-white/[0.07] pb-2">
                <h2 class="classic-section-title">Backup</h2>
              </header>

              <div class="flex flex-col gap-3 p-3 classic-inset !rounded-lg !p-3">
                <p class="text-[10px] text-muted px-1 italic">
                  Export the current configuration to a JSON file or import one previously saved.
                </p>
                <UiButton class="flex-1 w-full" :loading="backupBusy" @click="handleBackup">
                  <Download v-if="!backupBusy" :size="12" />
                  Save Configuration
                </UiButton>
                <UiButton class="flex-1 w-full" :disabled="backupBusy" @click="handleRestore">
                  <Upload :size="12" />
                  Load Configuration
                </UiButton>
              </div>
            </div>
          </div>
        </div>
      </div>

      <div class="px-3 py-2 shrink-0"><PrivacyLink /></div>

      <!-- Toast Notification -->
      <div
        v-if="message"
        class="fixed bottom-4 left-1/2 -translate-x-1/2 z-[60] px-4 py-1.5 rounded-full shadow-lg text-[10px] font-bold border animate-in slide-in-from-bottom duration-200"
        :class="
          messageType === 'error'
            ? 'bg-red-500 border-red-600 text-white'
            : 'bg-green-500 border-green-600 text-white'
        "
      >
        {{ message }}
      </div>
    </div>
  </ErrorBoundary>
</template>

<script setup lang="ts">
import { invoke } from '@tauri-apps/api/core'
import { emit, listen, type UnlistenFn } from '@tauri-apps/api/event'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import ErrorBoundary from './components/ErrorBoundary.vue'
import UiButton from './components/UiButton.vue'
import PrivacyLink from './components/PrivacyLink.vue'
import type { InverterState } from './composables/useInverterState'
import { logger } from './logger'

const { t: $t } = useI18n()

import {
  Archive,
  Cloud,
  Download,
  Eye,
  RotateCcw,
  Save,
  Settings,
  Upload,
  Wifi,
  X,
} from '@lucide/vue'
import {
  FeaturePluginManager,
  featurePluginManagerTabId,
  featureConfigSections,
  prepareFeatureConfig,
  subscribeFeatureConfig,
  isMobileApp,
} from '@features'
import { useConfigForm } from './composables/useConfigForm'
import TariffConfiguration from './components/TariffConfiguration.vue'
import { gatewayConfigError } from './connectionPolicy'

const {
  config,
  configLoaded,
  saving,
  message,
  messageType,
  loadConfig,
  saveConfig,
  resetToDefaults,
  clearMessage,
} = useConfigForm()
const activeTab = ref('mqtt')

type DiscoveredInst = {
  instance: number
  kind: string
  name?: string | null
}
const discoveredWaterEv = ref<DiscoveredInst[]>([])
const discoveredTanks = computed(() => discoveredWaterEv.value.filter((i) => i.kind === 'tank'))
const discoveredPumps = computed(() => discoveredWaterEv.value.filter((i) => i.kind === 'pump'))
const discoveredEvs = computed(() => discoveredWaterEv.value.filter((i) => i.kind === 'ev'))
const discoveredEvchargers = computed(() =>
  discoveredWaterEv.value.filter((i) => i.kind === 'evcharger')
)

function ingestDiscovered(list: DiscoveredInst[] | null | undefined) {
  if (!list?.length) return
  discoveredWaterEv.value = list
  // Auto-pick sensible defaults when saved config is missing from discovery.
  const tanks = list.filter((i) => i.kind === 'tank')
  const pumps = list.filter((i) => i.kind === 'pump')
  const evs = list.filter((i) => i.kind === 'ev')
  const chargers = list.filter((i) => i.kind === 'evcharger')
  const pick = (current: number | undefined, items: DiscoveredInst[]) => {
    if (!items.length) return current
    if (current != null && items.some((i) => i.instance === current)) return current
    return items[0].instance
  }
  config.water_tank_instance = pick(config.water_tank_instance, tanks)
  config.water_pump_instance = pick(config.water_pump_instance, pumps)
  // Valve: keep saved if present; else first pump that is not the active pump
  if (pumps.length) {
    const valve = config.water_valve_instance
    if (valve == null || !pumps.some((i) => i.instance === valve)) {
      const alt = pumps.find((i) => i.instance !== config.water_pump_instance)
      if (alt) config.water_valve_instance = alt.instance
    }
  }
  config.ev_instance = pick(config.ev_instance, evs)
  config.evcharger_instance = pick(config.evcharger_instance, chargers)
}

let unlistenMqttState: UnlistenFn | null = null
let unlistenFeatures: (() => void) | null = null
let featureSubscription: Promise<void> | null = null
let disposed = false

function ensureFeatureSubscription(): Promise<void> {
  if (disposed || unlistenFeatures) return Promise.resolve()
  // Restoring updates this same reactive config object. Keep one subscription,
  // including when a restore overlaps startup's pending registration.
  featureSubscription ??= (async () => {
    try {
      const stop = await subscribeFeatureConfig(config, () => !disposed)
      if (disposed) stop()
      else unlistenFeatures = stop
    } catch {
      if (!disposed) logger.warn('Optional settings updates are unavailable')
    } finally {
      featureSubscription = null
    }
  })()
  return featureSubscription
}

const sections = computed(() => [
  { id: 'mqtt', label: 'MQTT Broker', icon: Wifi },
  { id: 'gateway', label: 'Remote Gateway', icon: Cloud },
  { id: 'devices', label: 'Cerbo Devices', icon: Settings },
  ...featureConfigSections.map((section) => ({
    ...section,
    label: section.labelKey ? $t(section.labelKey) : section.label,
  })),
  { id: 'sections', label: 'Sections', icon: Eye },
  { id: 'tariff', label: 'Electricity tariff', icon: Settings },
  { id: 'backup', label: 'Backup', icon: Archive },
])

const testingGateway = ref(false)
const gatewayTestResult = ref('')
const gatewayTestSuccess = ref(false)
const backupBusy = ref(false)

async function testGatewayConnection() {
  const url = (config.gateway_url || '').trim()
  const clientId = (config.gateway_access_client_id || '').trim()
  const clientSecret = (config.gateway_access_client_secret || '').trim()
  const validationError = gatewayConfigError(config)
  if (validationError) {
    gatewayTestResult.value = validationError
    gatewayTestSuccess.value = false
    return
  }
  testingGateway.value = true
  gatewayTestResult.value = ''
  try {
    const result = await invoke<{ status: string; mqtt_connected?: boolean | null }>(
      'test_gateway_connection',
      {
        url,
        accessClientId: clientId,
        accessClientSecret: clientSecret,
        apiToken: (config.gateway_api_token || '').trim() || null,
      }
    )
    let mqtt = 'MQTT unknown'
    if (result.mqtt_connected === true) {
      mqtt = 'MQTT connected'
    } else if (result.mqtt_connected === false) {
      mqtt = 'MQTT disconnected'
    }
    gatewayTestResult.value = `OK (${result.status}) — ${mqtt}`
    gatewayTestSuccess.value = true
  } catch (e) {
    gatewayTestResult.value = `Failed: ${e?.toString() || e}`
    gatewayTestSuccess.value = false
  } finally {
    testingGateway.value = false
  }
}

async function handleSave() {
  const gatewayError = config.gateway_enabled ? gatewayConfigError(config) : null
  if (gatewayError) {
    message.value = gatewayError
    messageType.value = 'error'
    return
  }
  if (!config.gateway_enabled && !config.mqtt_tls && (config.mqtt_login || config.mqtt_password)) {
    message.value = 'Enable TLS before using an MQTT username or password'
    messageType.value = 'error'
    return
  }
  prepareFeatureConfig(config)
  if (!(await saveConfig())) return
  // Apply auto-start setting
  if (!isMobileApp) {
    try {
      await invoke('set_auto_start', { enable: config.auto_start ?? false })
    } catch (e) {
      logger.warn('Failed to set auto-start:', e)
    }
  }
  await emit('config-saved', { color_scheme: config.color_scheme })
  message.value = 'Settings saved successfully'
  messageType.value = 'success'
  setTimeout(clearMessage, 2000)
}

async function handleBackup() {
  if (backupBusy.value) return
  backupBusy.value = true
  try {
    const done = await invoke<boolean>('backup_config')
    message.value = done ? 'Settings exported without passwords or tokens' : 'Backup cancelled'
    messageType.value = done ? 'success' : 'info'
  } catch (e) {
    message.value = `Backup failed: ${e?.toString() || e}`
    messageType.value = 'error'
  } finally {
    backupBusy.value = false
    setTimeout(clearMessage, 3000)
  }
}

async function handleRestore() {
  if (backupBusy.value) return
  backupBusy.value = true
  let reportResult = false
  try {
    const done = await invoke<boolean>('restore_config')
    if (disposed) return
    if (done) {
      const cfg = await loadConfig()
      if (!cfg || disposed) return
      await ensureFeatureSubscription()
      if (disposed) return
      applyTheme(cfg.color_scheme)
      await emit('config-saved', { color_scheme: cfg.color_scheme })
      message.value = 'Configuration loaded'
      messageType.value = 'success'
    } else {
      message.value = 'Load cancelled'
      messageType.value = 'info'
    }
    reportResult = true
  } catch (e) {
    if (disposed) return
    message.value = `Restore failed: ${e?.toString() || e}`
    messageType.value = 'error'
    reportResult = true
  } finally {
    backupBusy.value = false
    if (!disposed && reportResult) setTimeout(clearMessage, 3000)
  }
}

async function handleClose() {
  if (isMobileApp) {
    try {
      await invoke('close_config_window')
    } catch (error) {
      logger.error('Could not close settings:', error)
      message.value = 'Could not return to the dashboard. Try again.'
      messageType.value = 'error'
    }
    return
  }
  try {
    const win = getCurrentWindow()
    await win.close()
  } catch (e) {
    logger.warn('Frontend close failed, trying backend:', e)
    try {
      await invoke('close_config_window')
    } catch (err) {
      logger.error('Close failed:', err)
    }
  }
}

function handleReset() {
  if (confirm('Reset all settings to defaults?')) {
    resetToDefaults()
  }
}

const applyTheme = (scheme: string | null | undefined) => {
  const isDark = scheme === 'dark'
  logger.log('Applying theme to Config window:', scheme, isDark)
  document.documentElement.classList.toggle('dark', isDark)
  document.body.classList.toggle('dark', isDark)

  // Force background to prevent system-level dark mode overrides if any
  if (isDark) {
    document.documentElement.style.backgroundColor = '#0c0c0e'
    document.body.style.backgroundColor = '#0c0c0e'
  } else {
    document.documentElement.style.backgroundColor = '#f4f4f6'
    document.body.style.backgroundColor = '#f4f4f6'
  }
}

watch(
  () => config.color_scheme,
  (scheme) => {
    applyTheme(scheme)
  },
  { immediate: true }
)

async function handleKeyDown(e: KeyboardEvent) {
  const isW = e.key === 'w' || e.key === 'W' || e.code === 'KeyW'
  if ((e.metaKey || e.ctrlKey) && isW) {
    e.preventDefault()
    e.stopPropagation()
    await handleClose()
  }
}

onMounted(async () => {
  try {
    globalThis.addEventListener('keydown', handleKeyDown)
    const cfg = await loadConfig()
    if (disposed) return
    await ensureFeatureSubscription()
    if (disposed) return
    // A restore can supersede the startup read. Still initialize discovery,
    // without applying theme or data from that cancelled read.
    if (cfg) applyTheme(cfg.color_scheme)
    try {
      const st = await invoke<InverterState>('get_state')
      if (disposed) return
      ingestDiscovered(st.discovered_water_ev ?? undefined)
    } catch (e) {
      logger.warn('get_state for water/EV discovery failed:', e)
    }
    if (disposed) return
    const unlisten = await listen<InverterState>('mqtt-state-update', (event) => {
      if (!disposed) ingestDiscovered(event.payload?.discovered_water_ev ?? undefined)
    })
    if (disposed) unlisten()
    else unlistenMqttState = unlisten
  } catch (err) {
    logger.error('Config init failed:', err)
  }
})

onUnmounted(() => {
  disposed = true
  unlistenFeatures?.()
  unlistenFeatures = null
  globalThis.removeEventListener('keydown', handleKeyDown)
  if (unlistenMqttState) {
    unlistenMqttState()
    unlistenMqttState = null
  }
})
</script>
