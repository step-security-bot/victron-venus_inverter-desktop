import { invoke, isTauri } from '@tauri-apps/api/core'
import { isMobileApp } from '@features'
import type { TariffPlan } from './model'

export async function exportTariff(plan: TariffPlan): Promise<boolean> {
  if (isTauri() && !isMobileApp) {
    // WKWebView downloads can block the macOS main thread while obtaining a
    // sandbox extension. Let the native save dialog choose the destination.
    try {
      return await invoke<boolean>('export_tariff', { plan })
    } catch (cause) {
      // Tauri rejects Result::Err(String) with a string, not an Error instance.
      throw cause instanceof Error
        ? cause
        : new Error(typeof cause === 'string' ? cause : 'Tariff export failed.')
    }
  }

  const url = URL.createObjectURL(
    new Blob([JSON.stringify(plan, null, 2)], { type: 'application/json' })
  )
  try {
    const anchor = document.createElement('a')
    anchor.href = url
    anchor.download = 'electricity-tariff.json'
    anchor.click()
    return true
  } finally {
    setTimeout(() => URL.revokeObjectURL(url), 1000)
  }
}
