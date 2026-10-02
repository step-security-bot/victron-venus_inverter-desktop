export const ESS_MODES = [
  { id: 'off', label: 'Off', short: 'Off' },
  { id: 'on', label: 'On', short: 'On' },
  {
    id: 'optimized_with_battery_life',
    label: 'Optimized with battery life',
    short: 'Optimized · BL',
  },
  {
    id: 'optimized_without_battery_life',
    label: 'Optimized without battery life',
    short: 'Optimized',
  },
  { id: 'keep_batteries_charged', label: 'Keep batteries charged', short: 'Keep charged' },
  { id: 'external_control', label: 'External control', short: 'External' },
] as const
export type EssModeId = (typeof ESS_MODES)[number]['id']
export interface EssModeState {
  mode_name?: string
  is_external?: boolean
  selected?: EssModeId | null
  vebus_mode?: number | null
  selection_supported?: boolean
  request_id?: string | null
  error?: string | null
}

export function selectedEssMode(mode?: EssModeState): EssModeId | undefined {
  if (!mode) return undefined
  // A capable controller owns the selection, including unknown switch states.
  if (mode.selection_supported) return ESS_MODES.find((item) => item.id === mode.selected)?.id
  if (mode.mode_name === 'Off') return 'off'
  if (mode.is_external) return 'external_control'
  return ESS_MODES.find((item) => item.label.toLowerCase() === mode.mode_name?.toLowerCase())?.id
}
