import { createApp, defineAsyncComponent, h, type Component } from 'vue'
import AuthGate from './components/AuthGate.vue'
import ErrorBoundary from './components/ErrorBoundary.vue'
import { getFeatureView, isMobileApp } from '@features'
import { i18n } from './i18n'

import { logger } from './logger'
import './style.css'

document.documentElement.dataset.appProfile = isMobileApp ? 'mobile' : 'desktop'

const path = globalThis.location.pathname
const isConfigWindow = path === '/config'
const isAboutWindow = path === '/about'

let rootComponent: Component
if (isMobileApp) {
  rootComponent = defineAsyncComponent(() => import('./MobileShell.vue'))
} else if (isConfigWindow) {
  rootComponent = defineAsyncComponent(() => import('./Config.vue'))
} else if (isAboutWindow) {
  rootComponent = defineAsyncComponent(() => import('./About.vue'))
} else {
  // Temporary media windows should not load dashboard charts or configuration.
  rootComponent = getFeatureView(path) ?? defineAsyncComponent(() => import('./App.vue'))
}

const app = createApp({
  render: () =>
    h(AuthGate, null, {
      default: () => h(ErrorBoundary, null, { default: () => h(rootComponent) }),
    }),
})
app.use(i18n)
app.config.errorHandler = (err, instance, info) => {
  logger.error('Unhandled Vue error:', err, 'Component:', instance, 'Info:', info)
}
app.mount('#app')
