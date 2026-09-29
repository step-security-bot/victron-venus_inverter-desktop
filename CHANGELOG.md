# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added
- Ring-MQTT camera motion/ding handling via HA MQTT `camera_topic` wildcards and optional
  `ring_snapshot_url_template` (HTTP snapshot / HA camera_proxy). See `docs/ring-mqtt.md`.
- Per-plugin always-on-top video windows, disabled by default, and exclusions for
  individual legacy camera sources.
- Live Home Assistant camera previews from configured MQTT URL topics.

### Fixed
- Camera previews start with less repeated work and discard stale queued live-preview frames.
- Configuration restore no longer leaks subscriptions or lets an older load
  overwrite newly restored settings. Plugin status refreshes coalesce event bursts.
- Plugin restoration avoids repeatedly verifying every installed package for
  each declaration while retaining integrity checks for the selected package.
- Local macOS updates stage the replacement application before stopping the old
  one and restore the previous bundle if replacement fails.
- The tariff spreadsheet uses matching Univer 1.0.2 packages, fixing builds that
  mixed incompatible Facade types. Dependabot now updates the Univer family together.

### Changed
- Update Lucide icons and Prettier. Remove the unused direct Undici development
  dependency; jsdom keeps its compatible, patched Undici 7.29.1 dependency.

### Upgrade and rollback
- Application updates preserve the existing configuration, cached encryption key,
  plugin version pins and enabled/disabled choices. Camera packages remain managed
  separately through the Plugins settings.
- Keep the previous application bundle for rollback. Updating the app does not
  require exporting and importing configuration or enabling camera plugins.

## [2.5.41] - 2026-09-12

### Fixed
- Android ARM64 and x86_64 builds align both load segments and RELRO boundaries
  for 16 KB memory pages, preventing failures caused by the earlier ARM64 boundary.

### Added
- A manual Google Play upload-bundle workflow with a separate upload key, complete
  signature verification, and native alignment checks. Existing GitHub APK signing
  identity is preserved; the workflow does not submit the app to Google Play.

## [2.5.40] - 2026-09-12

### Fixed
- Installed iOS bundle and build versions now follow the app release version
  instead of the hardcoded `1.0.0` value.

## [2.5.39] - 2026-09-12

### Fixed
- Clearing the MQTT host with IGW disabled now stops both inverter transports and
  clears their displayed telemetry. Home Assistant camera MQTT remains independent.
- Pending startup, reconnect and configuration operations cannot revive a removed
  inverter connection; callbacks from the previous session cannot overwrite new data.

## [2.5.31] - 2026-09-08

### Fixed
- Water pump/valve buttons now publish GX MQTT-API `W/<portal>/pump/<n>/Mode`
  (Cerbo `{"value": mode}`) instead of the unused `inverter/cmd/water_mode_set`
  topic; Mode read-back updates AUTO chips (#369).

### Added
- Manual pump/valve buttons in the Water card, routed through dbus-pump's
  writable `/Mode` (0 auto, 1 always-on, 2 always-off) via the GX MQTT-API
  (`W/<portal>/pump/<n>/Mode`) - still no direct Home Assistant control.
  Opening the city valve asks for confirmation; an AUTO chip appears while a
  device is under manual override to hand it back to dbus-pump automation.
- dbus-pump fix required on the GX: its `/Mode` onchange handler must return
  true, otherwise vedbus rejects the write and the mode silently stays `auto`
  (deployed as dbus-pump main).

### Changed
- EV section now sources data from Cerbo MQTT (dbus-ev / dbus-evcharger) instead of
  Home Assistant. Subscribes to `N/<portal>/ev/<instance>/Soc` (%),
  `N/<portal>/ev/<instance>/Ac/Power` (W), and
  `N/<portal>/evcharger/<instance>/Ac/Power` (W).
  New config fields: `evcharger_instance` (default 40) and `ev_instance` (default 22).
  The `ha_ev_soc_entity`, `ha_ev_charging_entity`, and `ha_ev_clamp_entity` config
  fields are removed. EV section visibility no longer requires HA direct API — it shows
  when Cerbo MQTT is connected and at least one EV metric is live.

## [2.5.0] - 2026-08-24

### Added
- Portal ID auto-discovery: subscribes to the retained `inverter/portal`
  topic published by inverter-control and arms water/alarm subscriptions and
  the GX keepalive automatically - `portal_id` is now optional in app config.

### Changed
- Water section now uses **only** Cerbo GX MQTT (dbus-pump): the Home Assistant
  entity fallback (`ha_water_level_entity` / `ha_valve_switch_entity` /
  `ha_pump_switch_entity`) is removed, along with its config fields
- Water card shows level in % plus pump/valve status badges; toggle buttons
  removed — pump/valve automation lives in dbus-pump

## [2.4.4] - 2026-08-23

### Added
- Solar forecast display (#252) and forecast brackets in DailyStats (#251)
- Yesterday solar production (#241)
- Washer/dryer START/PAUSE buttons from HA button entities (#247)
- Persistent notification banner + Victron alarm watcher (#244)

### Fixed
- Tray icon keeps updating after poisoned lock, panic, or sleep/wake (#255)
- Solar breakdown now adds up to headline total (#254)
- Existing config/about windows brought to front on reopen (#253)

### Security
- Bump glob override to 11.1.0 (CVE-2025-64756) (#250)

## [2.4.3] - unreleased

### Fixed
- Various fixes and dependency updates

## [2.2.2] - 2026-07-20

### Fixed

- CI build failure: remove empty Apple code signing env vars causing `SecKeychainItemImport` error
