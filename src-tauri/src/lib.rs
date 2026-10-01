mod app_visibility;
mod auth;
mod config_backup;
mod config_file_io;
mod config_store;
mod gateway;
mod gateway_actions;
mod inverter_control;
#[cfg(test)]
#[path = "../mobile_build.rs"]
mod mobile_build;
#[cfg(any(target_os = "android", target_os = "ios"))]
mod mobile_credentials;
mod module_config;
pub(crate) mod mqtt;
mod plugin_config;
#[cfg(desktop)]
pub mod plugins;
mod release_info;
#[cfg(desktop)]
mod tariff_export;
mod tls;

#[cfg(all(desktop, feature = "native-media-smoke"))]
pub use plugins::native_media_smoke::run as run_native_media_smoke;
#[cfg(target_os = "macos")]
mod tray_icon;

#[cfg(target_os = "macos")]
extern "C" {
    fn biometric_available() -> bool;
    fn biometric_authenticate(reason: *const std::os::raw::c_char) -> bool;
}

use gateway::GatewayClient;
use log::{info, warn};
use mqtt::{HeaderToggle, InverterState, MqttClient, SetpointOverrideStatus};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

// Serialize native read/validate/write cycles, including migration and import.
// A stale frontend save is compared against the latest plugin declarations.
static CONFIG_UPDATE_GATE: Mutex<()> = Mutex::new(());
#[cfg(desktop)]
use std::time::Duration;

use config_store::{load_config, save_config_encrypted};

const DEFAULT_MQTT_HOST: &str = "Cerbo";
const DEFAULT_MQTT_PORT: u16 = 1883;
#[cfg(desktop)]
const DEFAULT_HA_PORT: u16 = 8123;
#[cfg(desktop)]
const ABOUT_WINDOW_W: f64 = 380.0;
#[cfg(desktop)]
const ABOUT_WINDOW_H: f64 = 320.0;
#[cfg(desktop)]
const CONFIG_WINDOW_W: f64 = 850.0;
#[cfg(desktop)]
const CONFIG_WINDOW_H: f64 = 700.0;
#[cfg(desktop)]
const CAMERA_VIDEO_WINDOW_W: f64 = 330.0;
/// Exact 16:9 of W so object-contain fills without letterbox (round(330*9/16)=186).
#[cfg(desktop)]
const CAMERA_VIDEO_WINDOW_H: f64 = 186.0;
/// Logical-pixel gap between stacked camera clip windows (0 = flush/seam). Also used as edge inset.
#[cfg(desktop)]
const CAMERA_VIDEO_WINDOW_MARGIN: f64 = 0.0;
#[cfg(desktop)]
use tauri::WindowEvent;
use tauri::{Emitter, Manager, State};
use tauri_plugin_store::StoreExt;

#[cfg(desktop)]
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};

// Global state for the MQTT clients
struct MqttState(Arc<Mutex<Option<MqttClient>>>);
struct GatewayState(Arc<Mutex<Option<GatewayClient>>>);
#[derive(Default)]
struct InverterLifecycle(Mutex<()>);

#[derive(Debug, Serialize, Deserialize, Clone)]
struct HomeButtonConfig {
    id: String,
    label: String,
    entity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state_key: Option<String>,
    domain: String,
    enabled: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct FullConfig {
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    modules: module_config::ModuleNamespaces,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    desktop_plugins: Vec<plugin_config::DesktopPluginConfig>,
    mqtt_host: String,
    mqtt_port: u16,
    #[serde(default)]
    mqtt_tls: bool,
    mqtt_login: Option<String>,
    mqtt_password: Option<String>,
    mqtt_ha_host: Option<String>,
    mqtt_ha_port: Option<u16>,
    mqtt_ha_login: Option<String>,
    mqtt_ha_password: Option<String>,
    ha_longlived_token: Option<String>,
    ha_url: Option<String>,
    ha_port: Option<u16>,
    ha_use_direct_api: bool,
    ha_dryer_entity: Option<String>,
    ha_washer_entity: Option<String>,
    ha_washer_start_entity: Option<String>,
    ha_washer_pause_entity: Option<String>,
    ha_dryer_start_entity: Option<String>,
    ha_dryer_pause_entity: Option<String>,
    ha_dishwasher_running_entity: Option<String>,
    ha_dishwasher_duration_entity: Option<String>,
    ha_ev_soc_entity: Option<String>,
    ha_ev_charging_entity: Option<String>,
    ha_ev_clamp_entity: Option<String>,
    // Live power tiles prefer Cerbo GX MQTT (system/vebus/shunt/acload/MPPT/PV/EV/water).
    // Daemon inverter/state still supplies: daily_stats, solar_forecast, booleans,
    // features, ess_mode, versions, dry_run, ui_config, HA connectivity flags.
    // HA entities cover washer/dryer/dishwasher (not merged from daemon).
    // Optional HA CT clamps if you prefer HA meters over Victron D-Bus:
    ha_consumption_clamps: Option<Vec<String>>,
    ha_generation_clamps: Option<Vec<String>>,
    color_scheme: Option<String>,
    // Home buttons retain the legacy ha_entities key in saved settings.
    ha_entities: Option<Vec<HomeButtonConfig>>,
    header_toggles_config: Option<Vec<HeaderToggle>>,
    portal_id: Option<String>,
    #[serde(default)]
    water_tank_instance: Option<u32>,
    water_pump_instance: Option<u32>,
    water_valve_instance: Option<u32>,
    #[serde(default = "default_evcharger_instance")]
    evcharger_instance: Option<u32>,
    #[serde(default = "default_ev_instance")]
    ev_instance: Option<u32>,
    camera_topic: Option<String>,
    /// Base URL for Frigate clips, e.g. http://192.168.151.21:5005 (no trailing slash required).
    #[serde(default)]
    frigate_base_url: Option<String>,
    /// HTTP(S) snapshot URL template for Ring-MQTT motion/ding events.
    /// Placeholders: `{device_id}`, `{location_id}`, `{event}`.
    /// Example: `http://ha:8123/api/camera_proxy/camera.front_door_snapshot`
    #[serde(default)]
    ring_snapshot_url_template: Option<String>,
    /// Retained legacy live-view mappings; interpreted only by installed-package migration.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    camera_live_urls: std::collections::BTreeMap<String, String>,
    camera_enabled: bool,
    show_advanced_settings: Option<bool>,
    #[serde(
        default = "legacy_section_visible",
        deserialize_with = "deserialize_section_visibility"
    )]
    show_batteries: Option<bool>,
    #[serde(
        default = "legacy_section_visible",
        deserialize_with = "deserialize_section_visibility"
    )]
    show_solar_production: Option<bool>,
    #[serde(
        default = "legacy_section_visible",
        deserialize_with = "deserialize_section_visibility"
    )]
    show_active_loads: Option<bool>,
    #[serde(
        default = "legacy_section_visible",
        deserialize_with = "deserialize_section_visibility"
    )]
    show_daily_stats: Option<bool>,
    #[serde(
        default = "legacy_section_visible",
        deserialize_with = "deserialize_section_visibility"
    )]
    show_ev: Option<bool>,
    show_washer: Option<bool>,
    show_dryer: Option<bool>,
    show_dishwasher: Option<bool>,
    #[serde(
        default = "legacy_section_visible",
        deserialize_with = "deserialize_section_visibility"
    )]
    show_home_section: Option<bool>,
    #[serde(
        default = "legacy_section_visible",
        deserialize_with = "deserialize_section_visibility"
    )]
    show_header_toggles: Option<bool>,
    show_ha_sensors: Option<bool>,
    show_ha_numbers: Option<bool>,
    show_ha_covers: Option<bool>,
    show_ha_media: Option<bool>,
    show_ha_scenes: Option<bool>,
    show_ha_weather: Option<bool>,
    ha_appliance_entities: Option<std::collections::HashMap<String, String>>,
    auto_start: Option<bool>,
    auth_enabled: Option<bool>,
    auth_username: Option<String>,
    auth_password: Option<String>,
    auth_biometric: Option<bool>,

    /// Enable remote inverter-gateway as a data source alongside configured MQTT.
    #[serde(default)]
    gateway_enabled: bool,
    /// Direct or Cloudflare-protected HTTPS base URL (no trailing slash required).
    gateway_url: Option<String>,
    /// Optional Cloudflare Access Service Token Client ID (CF-Access-Client-Id).
    gateway_access_client_id: Option<String>,
    /// Optional Cloudflare Access Service Token Client Secret (CF-Access-Client-Secret).
    gateway_access_client_secret: Option<String>,
    /// Gateway API bearer (Authorization: Bearer … / GATEWAY_API_TOKEN).
    gateway_api_token: Option<String>,

    /// First-run setup wizard completed. Missing in older configs → migrated in get_config.
    #[serde(default)]
    setup_completed: bool,
}

// A missing/null field in an existing store used to display the section. Preserve
// that layout while FullConfig::default applies the leaner first-run defaults.
fn legacy_section_visible() -> Option<bool> {
    Some(true)
}

fn deserialize_section_visibility<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(
        Option::<bool>::deserialize(deserializer)?.unwrap_or(true),
    ))
}

fn default_evcharger_instance() -> Option<u32> {
    Some(40)
}

fn default_ev_instance() -> Option<u32> {
    Some(22)
}

impl Default for FullConfig {
    fn default() -> Self {
        Self {
            modules: module_config::ModuleNamespaces::new(),
            desktop_plugins: Vec::new(),
            mqtt_host: DEFAULT_MQTT_HOST.to_string(),
            mqtt_port: DEFAULT_MQTT_PORT,
            mqtt_tls: false,
            mqtt_login: None,
            mqtt_password: None,
            mqtt_ha_host: Some(DEFAULT_MQTT_HOST.to_string()),
            mqtt_ha_port: Some(DEFAULT_MQTT_PORT),
            mqtt_ha_login: None,
            mqtt_ha_password: None,
            ha_longlived_token: None,
            ha_url: None,
            ha_port: None,
            ha_use_direct_api: false,
            ha_dryer_entity: None,
            ha_washer_entity: None,
            ha_washer_start_entity: None,
            ha_washer_pause_entity: None,
            ha_dryer_start_entity: None,
            ha_dryer_pause_entity: None,
            ha_dishwasher_running_entity: None,
            ha_dishwasher_duration_entity: None,
            ha_ev_soc_entity: None,
            ha_ev_charging_entity: None,
            ha_ev_clamp_entity: None,
            // Live tiles: Cerbo-first; daemon for stats/flags/config (see FullConfig note)
            ha_consumption_clamps: None,
            ha_generation_clamps: None,
            color_scheme: Some("dark".to_string()),
            ha_entities: None,
            header_toggles_config: None,
            portal_id: None,
            // dbus-pump defaults on the GX (see dbus-pump local_config.example.py)
            water_tank_instance: None,
            water_pump_instance: Some(1),
            water_valve_instance: Some(2),
            evcharger_instance: Some(40),
            ev_instance: Some(22),
            camera_topic: Some("kerberos/desktop/events".to_string()),
            frigate_base_url: None,
            ring_snapshot_url_template: None,
            camera_live_urls: Default::default(),
            camera_enabled: true,
            show_advanced_settings: Some(false),
            show_batteries: Some(true),
            show_solar_production: Some(true),
            show_active_loads: Some(false),
            show_daily_stats: Some(false),
            show_ev: Some(true),
            show_washer: Some(true),
            show_dryer: Some(true),
            show_dishwasher: Some(true),
            show_home_section: Some(false),
            show_header_toggles: Some(false),
            show_ha_sensors: Some(true),
            show_ha_numbers: Some(true),
            show_ha_covers: Some(true),
            show_ha_media: Some(true),
            show_ha_scenes: Some(true),
            show_ha_weather: Some(true),
            ha_appliance_entities: None,
            auto_start: Some(false),
            auth_enabled: Some(false),
            auth_username: None,
            auth_password: None,
            auth_biometric: Some(false),
            gateway_enabled: false,
            gateway_url: None,
            gateway_access_client_id: None,
            gateway_access_client_secret: None,
            gateway_api_token: None,
            setup_completed: false,
        }
    }
}

// Clear both owned inverter slots. Optional plugin workers have independent lifetimes.
fn stop_inverter_clients(
    mqtt: &MqttState,
    gateway: &GatewayState,
    lifecycle: &InverterLifecycle,
) -> Result<(), String> {
    let _lifecycle = lifecycle
        .0
        .lock()
        .map_err(|e| format!("Internal error: {}", e))?;
    let mut gateway_guard = gateway
        .0
        .lock()
        .map_err(|e| format!("Internal error: {}", e))?;
    let mut mqtt_guard = mqtt
        .0
        .lock()
        .map_err(|e| format!("Internal error: {}", e))?;
    if let Some(client) = gateway_guard.take() {
        client.stop();
    }
    if let Some(client) = mqtt_guard.take() {
        client.stop();
    }
    Ok(())
}

#[tauri::command]
fn disconnect_inverter(
    app: tauri::AppHandle,
    mqtt_client: State<MqttState>,
    gateway_client: State<GatewayState>,
    lifecycle: State<InverterLifecycle>,
) -> Result<(), String> {
    stop_inverter_clients(&mqtt_client, &gateway_client, &lifecycle)?;
    // stop() aborts/silences each poller, so its final status event is not
    // guaranteed. Explicit disconnect must immediately invalidate the editor.
    let _ = app.emit("setpoint-override-update", serde_json::Value::Null);
    Ok(())
}

#[tauri::command]
fn get_state(
    mqtt_client: State<MqttState>,
    gateway_client: State<GatewayState>,
) -> Result<InverterState, String> {
    if let Ok(g) = gateway_client.0.lock() {
        if let Some(ref client) = *g {
            return Ok(client.get_state());
        }
    }
    let client = mqtt_client
        .0
        .lock()
        .map_err(|e| format!("Internal error: {}", e))?;
    if let Some(ref client) = *client {
        Ok(client.get_state())
    } else {
        Err("MQTT client not connected".to_string())
    }
}

#[tauri::command]
async fn get_setpoint_override(
    mqtt_client: State<'_, MqttState>,
    gateway_client: State<'_, GatewayState>,
) -> Result<SetpointOverrideStatus, String> {
    if let Some(auth) = active_gateway_auth(&gateway_client)? {
        return gateway_actions::get_setpoint_override(&auth).await;
    }
    mqtt_setpoint_override(&mqtt_client)
}

fn mqtt_setpoint_override(mqtt_client: &MqttState) -> Result<SetpointOverrideStatus, String> {
    let guard = mqtt_client.0.lock().map_err(|e| e.to_string())?;
    guard
        .as_ref()
        .and_then(MqttClient::setpoint_override_status)
        .ok_or_else(|| {
            "Current Setpoint Override status is unavailable; wait for fresh MQTT telemetry".into()
        })
}

#[tauri::command]
async fn set_setpoint_override(
    value: Option<i32>,
    mqtt_client: State<'_, MqttState>,
    gateway_client: State<'_, GatewayState>,
) -> Result<SetpointOverrideStatus, String> {
    let request_id = uuid::Uuid::new_v4().to_string();
    if let Some(auth) = active_gateway_auth(&gateway_client)? {
        return gateway_actions::set_setpoint_override(&auth, value, &request_id).await;
    }
    let shared_state = {
        let guard = mqtt_client.0.lock().map_err(|e| e.to_string())?;
        let client = guard
            .as_ref()
            .ok_or("Connect to Cerbo MQTT to change the override")?;
        client.request_setpoint_override(value, &request_id)?;
        client.state.clone()
    };
    // Confirm daemon acceptance instead of presenting MQTT enqueue as success.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let status = shared_state
            .lock()
            .map_err(|e| e.to_string())?
            .setpoint_override
            .clone();
        if let Some(status) =
            status.filter(|s| s.request_id.as_deref() == Some(request_id.as_str()))
        {
            if let Some(error) = status.last_error.as_ref() {
                return Err(error.clone());
            }
            return Ok(status);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(
                "Cerbo has not confirmed the override. Check its connection and current status."
                    .into(),
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tauri::command]
async fn perform_action(
    action: String,
    payload: serde_json::Value,
    app: tauri::AppHandle,
    mqtt_client: State<'_, MqttState>,
    gateway_client: State<'_, GatewayState>,
) -> Result<(), String> {
    info!("perform_action: action={}", action);
    if action == "electricity_tariff" {
        inverter_control::validate_tariff_command(&payload)?;
    }

    // Water writes remain on the GX /Mode control plane through the active
    // transport. Validate before integer conversion; malformed input must not
    // silently turn into Auto or wrap to another mode.
    if action == "water_mode" {
        let (which, mode) = gateway_actions::water_payload(&payload)?;
        if let Some(auth) = active_gateway_auth(&gateway_client)? {
            let config = load_config(&app)?;
            return gateway_actions::perform(
                &auth,
                &action,
                payload,
                configured_gateway_instances(&config),
            )
            .await;
        }
        let client = mqtt_client
            .0
            .lock()
            .map_err(|e| format!("Internal error: {e}"))?;
        return match client.as_ref() {
            Some(client) => client.set_water_mode(which, mode),
            None => Err("Neither MQTT nor IGW is connected".into()),
        };
    }

    validate_core_action_target(&payload)?;

    perform_inverter_action(&action, payload, &mqtt_client, &gateway_client).await
}

fn validate_core_action_target(payload: &serde_json::Value) -> Result<(), String> {
    if let Some(entity) = payload.get("entity") {
        if !entity.as_str().is_some_and(inverter_control::is_flag) {
            return Err("This device control requires an installed plugin".into());
        }
    }
    Ok(())
}

fn active_gateway_auth(client: &GatewayState) -> Result<Option<gateway::GatewayHttpAuth>, String> {
    Ok(client
        .0
        .lock()
        .map_err(|e| format!("Internal error: {e}"))?
        .as_ref()
        .map(GatewayClient::http_auth))
}

fn configured_gateway_instances(config: &FullConfig) -> gateway::GatewayInstances {
    gateway::GatewayInstances {
        water_tank: config.water_tank_instance,
        water_pump: config.water_pump_instance,
        water_valve: config.water_valve_instance,
        ev: config.ev_instance,
        evcharger: config.evcharger_instance,
    }
}

async fn perform_inverter_action(
    action: &str,
    payload: serde_json::Value,
    mqtt: &MqttState,
    gateway: &GatewayState,
) -> Result<(), String> {
    // Active slots select the transport, not saved gateway_enabled (MQTT can be
    // preferred while IGW is configured for failover). Never retry a failed POST
    // on MQTT: the gateway may already have accepted the command.
    if let Some(auth) = active_gateway_auth(gateway)? {
        return gateway_actions::perform(&auth, action, payload, Default::default()).await;
    }
    let client = mqtt.0.lock().map_err(|e| format!("Internal error: {e}"))?;
    let client = client.as_ref().ok_or("Neither MQTT nor IGW is connected")?;
    client
        .publish_command(action, payload)
        .map_err(|e| e.to_string())
}

/// True when an existing install looks already configured (migrate setup_completed).
fn config_looks_previously_configured(config: &FullConfig) -> bool {
    let mqtt_configured = !config.mqtt_host.trim().is_empty();
    let gateway_configured = config.gateway_enabled
        && config
            .gateway_url
            .as_ref()
            .is_some_and(|u| !u.trim().is_empty());
    mqtt_configured || gateway_configured
}

/// Resolve whether setup is done. Existing persisted configs without the flag migrate to completed.
fn resolve_setup_completed(config: &FullConfig, had_saved_config: bool) -> bool {
    if config.setup_completed {
        return true;
    }
    had_saved_config && config_looks_previously_configured(config)
}

/// True when the first-run wizard should be shown.
#[cfg_attr(not(test), allow(dead_code))]
fn needs_setup(config: &FullConfig, had_saved_config: bool) -> bool {
    !resolve_setup_completed(config, had_saved_config)
}

#[tauri::command]
fn get_config(app: tauri::AppHandle) -> Result<FullConfig, String> {
    let _update = CONFIG_UPDATE_GATE
        .lock()
        .map_err(|_| "Config update lock failed")?;
    let mut config = load_config(&app)?;

    let store = app
        .store_builder("config.json")
        .build()
        .map_err(|e| format!("Failed to build store: {}", e))?;

    let had_saved_config = store.get("config").is_some();
    #[cfg(desktop)]
    let is_first_run = !had_saved_config;

    // Persist only real migrations / backfills for existing installs — never burn first-run.
    let mut persist = false;

    #[cfg(desktop)]
    if is_first_run {
        info!("Config: First run detected. Checking environment variables for seeding...");
        // Auto-fill from env ONLY on first run (in-memory; wizard must save).
        if let Ok(server) = std::env::var("HA_SERVER") {
            if !server.is_empty() {
                info!("Config: Found HA_SERVER={}", server);
                let url_base = if server.contains("://") {
                    server.clone()
                } else {
                    format!("http://{}", server)
                };

                let host_part = url_base
                    .trim_start_matches("http://")
                    .trim_start_matches("https://");
                let host_only = host_part
                    .split('/')
                    .next()
                    .unwrap_or(host_part)
                    .split(':')
                    .next()
                    .unwrap_or(host_part);

                config.ha_url = Some(format!("http://{}", host_only));
                config.mqtt_ha_host = Some(host_only.to_string());

                if let Some(port_str) = host_part.split(':').nth(1) {
                    if let Ok(port) = port_str
                        .split('/')
                        .next()
                        .unwrap_or(port_str)
                        .parse::<u16>()
                    {
                        config.ha_port = Some(port);
                        info!("Config: Parsed port {} from HA_SERVER", port);
                    }
                }
                info!(
                    "Config: Seeded ha_url={:?}, mqtt_ha_host={:?}",
                    config.ha_url, config.mqtt_ha_host
                );
            }
        }

        if let Ok(token) = std::env::var("HA_TOKEN") {
            if !token.is_empty() {
                info!("Config: Found HA_TOKEN (length={})", token.len());
                config.ha_longlived_token = Some(token);
            }
        }

        if let Ok(user) = std::env::var("HA_MQTT_USER") {
            if !user.is_empty() {
                info!("Config: Found HA_MQTT_USER={}", user);
                config.mqtt_ha_login = Some(user);
            }
        }

        if let Ok(pwd) = std::env::var("HA_MQTT_PWD") {
            if !pwd.is_empty() {
                info!("Config: Found HA_MQTT_PWD");
                config.mqtt_ha_password = Some(pwd);
            }
        }

        if config.ha_url.is_some() && config.ha_longlived_token.is_some() {
            info!("Config: Retaining legacy direct HA preference for optional migration");
            config.ha_use_direct_api = true;
        }
    }

    // Default values if missing (backward compatibility)
    #[cfg(desktop)]
    if config.ha_port.is_none() {
        config.ha_port = Some(DEFAULT_HA_PORT);
        if had_saved_config {
            persist = true;
        }
    }
    if config.mqtt_port == 0 {
        config.mqtt_port = DEFAULT_MQTT_PORT;
        if had_saved_config {
            persist = true;
        }
    }
    #[cfg(desktop)]
    if config.mqtt_ha_port.is_none() {
        config.mqtt_ha_port = Some(DEFAULT_MQTT_PORT);
        if had_saved_config {
            persist = true;
        }
    }

    // Migrate existing installs: do not show wizard for users who already have config.
    if resolve_setup_completed(&config, had_saved_config) && !config.setup_completed {
        info!("Config: Migrating setup_completed=true for existing install");
        config.setup_completed = true;
        persist = true;
    }

    if persist {
        save_config_encrypted(&app, &config)?;
    }

    Ok(public_core_config(config))
}

/// Core editors never receive retained module credentials or private live-view URLs.
fn public_core_config(mut config: FullConfig) -> FullConfig {
    config.modules = module_config::portable(&config.modules);
    config.camera_live_urls.clear();
    config
}

fn preserve_private_camera_config(
    config: &mut FullConfig,
    previous: &FullConfig,
) -> Result<(), String> {
    // Omission (including a core reset) keeps local mappings. This core IPC cannot
    // create, retarget or remove private mappings owned by package migration.
    if !config.camera_live_urls.is_empty() && config.camera_live_urls != previous.camera_live_urls {
        return Err("Private camera mappings cannot be changed through core settings".into());
    }
    config.camera_live_urls = previous.camera_live_urls.clone();
    Ok(())
}

#[tauri::command]
async fn save_config(
    app: tauri::AppHandle,
    #[allow(unused_variables)] window: tauri::WebviewWindow,
    mut config: FullConfig,
    desktop_plugins_expected: Option<Vec<plugin_config::DesktopPluginConfig>>,
) -> Result<(), String> {
    let _update = CONFIG_UPDATE_GATE
        .lock()
        .map_err(|_| "Config update lock failed")?;
    let previous = load_config(&app)?;
    plugin_config::reconcile_for_save(
        &mut config.desktop_plugins,
        &previous.desktop_plugins,
        desktop_plugins_expected.as_deref(),
    )?;
    preserve_private_camera_config(&mut config, &previous)?;
    config.modules = module_config::merge_for_save(config.modules, &previous.modules)?;
    auth::validate_policy(&config)?;
    // Any explicit save (wizard or Config UI) completes first-run setup.
    config.setup_completed = true;
    #[cfg(desktop)]
    plugins::bridge::save_configuration(&app, &window, &previous, &config)?;
    #[cfg(mobile)]
    save_config_encrypted(&app, &config)?;
    auth::revoke_if_policy_changed(&app, &previous, &config)?;
    #[cfg(desktop)]
    if previous.desktop_plugins != config.desktop_plugins {
        plugins::bridge::configuration_changed(&app);
    }
    Ok(())
}

#[tauri::command]
async fn backup_config(app: tauri::AppHandle) -> Result<bool, String> {
    use tauri_plugin_dialog::DialogExt;

    let file = app
        .dialog()
        .file()
        .set_file_name("config-backup.json")
        .add_filter("JSON", &["json"])
        .blocking_save_file();

    let file = match file {
        Some(file) => file,
        None => return Ok(false),
    };

    let mut source = load_config(&app)?;
    #[cfg(desktop)]
    source
        .modules
        .extend(plugins::bridge::portable_modules(&app).await?);
    let config = config_backup::redacted(&source)?;
    let json = serde_json::to_string_pretty(&config)
        .map_err(|e| format!("Failed to serialize config: {}", e))?;
    config_file_io::write(&app, file, &json)?;
    info!("Config backup saved");
    Ok(true)
}

#[tauri::command]
async fn restore_config(
    app: tauri::AppHandle,
    #[allow(unused_variables)] window: tauri::WebviewWindow,
) -> Result<bool, String> {
    use tauri_plugin_dialog::DialogExt;

    let file = app
        .dialog()
        .file()
        .add_filter("JSON", &["json"])
        .blocking_pick_file();

    let file = match file {
        Some(file) => file,
        None => return Ok(false),
    };

    let content = config_file_io::read(&app, file)?;
    #[cfg(desktop)]
    plugins::bridge::restore_configuration(&app, &window, content).await?;
    #[cfg(mobile)]
    {
        let _update = CONFIG_UPDATE_GATE
            .lock()
            .map_err(|_| "Config update lock failed")?;
        let previous = load_config(&app)?;
        let config = config_backup::restore(&content, &previous)?;
        save_config_encrypted(&app, &config)?;
    }
    info!("Config backup restored");
    Ok(true)
}

#[tauri::command]
async fn acknowledge_victron_banner(
    id: String,
    mqtt_client: State<'_, MqttState>,
    gateway_client: State<'_, GatewayState>,
) -> Result<(), String> {
    // id: victron-platform-<inst>-<slot>
    let rest = id
        .strip_prefix("victron-platform-")
        .ok_or_else(|| format!("Not a Victron platform banner id: {id}"))?;
    let mut parts = rest.splitn(2, '-');
    let platform_instance: u32 = parts
        .next()
        .ok_or("missing platform instance")?
        .parse()
        .map_err(|e| format!("bad platform instance: {e}"))?;
    let slot: u32 = parts
        .next()
        .ok_or("missing slot")?
        .parse()
        .map_err(|e| format!("bad slot: {e}"))?;

    // Prefer LAN MQTT (sets local user_dismissed + AcknowledgeAll).
    let mqtt_result = {
        let guard = mqtt_client
            .0
            .lock()
            .map_err(|e| format!("Internal error: {e}"))?;
        guard.as_ref().map(|client| {
            client
                .acknowledge_victron_notification(platform_instance, slot)
                .map_err(|e| e.to_string())
        })
    };
    match mqtt_result {
        Some(Ok(())) => return Ok(()),
        Some(Err(e)) => {
            warn!("LAN MQTT Victron ack failed ({e}); trying gateway");
        }
        None => {
            info!("LAN MQTT not connected; Victron ack via gateway");
        }
    }

    let auth = {
        let guard = gateway_client
            .0
            .lock()
            .map_err(|e| format!("Internal error: {e}"))?;
        guard.as_ref().map(|c| c.http_auth()).ok_or_else(|| {
            "Neither LAN MQTT nor gateway connected — cannot acknowledge on Cerbo".to_string()
        })?
    };
    gateway::acknowledge_all_notifications_http(&auth).await
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn connect_mqtt(
    host: String,
    port: u16,
    tls: Option<bool>,
    username: Option<String>,
    password: Option<String>,
    portal_id: Option<String>,
    water_tank_instance: Option<u32>,
    water_pump_instance: Option<u32>,
    water_valve_instance: Option<u32>,
    evcharger_instance: Option<u32>,
    ev_instance: Option<u32>,
    app: tauri::AppHandle,
    mqtt_client: State<'_, MqttState>,
    gateway_client: State<'_, GatewayState>,
    lifecycle: State<'_, InverterLifecycle>,
) -> Result<(), String> {
    connect_mqtt_impl(
        host,
        port,
        tls.unwrap_or(false),
        username,
        password,
        portal_id,
        water_tank_instance,
        water_pump_instance,
        water_valve_instance,
        evcharger_instance,
        ev_instance,
        app,
        mqtt_client,
        gateway_client,
        lifecycle,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn connect_mqtt_impl(
    host: String,
    port: u16,
    tls: bool,
    username: Option<String>,
    password: Option<String>,
    portal_id: Option<String>,
    water_tank_instance: Option<u32>,
    water_pump_instance: Option<u32>,
    water_valve_instance: Option<u32>,
    evcharger_instance: Option<u32>,
    ev_instance: Option<u32>,
    app: tauri::AppHandle,
    mqtt_client: State<'_, MqttState>,
    gateway_client: State<'_, GatewayState>,
    lifecycle: State<'_, InverterLifecycle>,
) -> Result<(), String> {
    let mut client = MqttClient::new(
        host,
        port,
        username,
        password,
        "inverter-dashboard-desktop".to_string(),
    );
    // Serialize shutdown, startup and slot installation across Tauri command threads.
    let _lifecycle = lifecycle
        .0
        .lock()
        .map_err(|e| format!("Internal error: {}", e))?;
    // Drop/stop any previous client first so its reconnect loop cannot keep
    // discovering the portal (xN) and racing the new connection.
    {
        let mut gw = gateway_client
            .0
            .lock()
            .map_err(|e| format!("Internal error: {}", e))?;
        if let Some(old) = gw.take() {
            old.stop();
        }
    }
    {
        let mut client_guard = mqtt_client
            .0
            .lock()
            .map_err(|e| format!("Internal error: {}", e))?;
        if let Some(old) = client_guard.take() {
            old.stop();
        }
    }
    // A newly saved config may be invalid. Stop the old source before rejecting
    // it so controls cannot target an old broker under the new endpoint's UI.
    let _ = app.emit("setpoint-override-update", serde_json::Value::Null);
    client.configure_transport(tls)?;
    client.set_app_handle(app);
    client.set_portal_id(portal_id);
    client.set_water_instances(Some((
        water_tank_instance,
        water_pump_instance,
        water_valve_instance,
    )));
    client.set_ev_instances(Some((ev_instance, evcharger_instance)));
    client.connect().map_err(|e| e.to_string())?;
    let mut client_guard = mqtt_client
        .0
        .lock()
        .map_err(|e| format!("Internal error: {}", e))?;
    *client_guard = Some(client);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
async fn connect_gateway(
    url: String,
    access_client_id: String,
    access_client_secret: String,
    api_token: Option<String>,
    water_tank_instance: Option<u32>,
    water_pump_instance: Option<u32>,
    water_valve_instance: Option<u32>,
    ev_instance: Option<u32>,
    evcharger_instance: Option<u32>,
    app: tauri::AppHandle,
    mqtt_client: State<'_, MqttState>,
    gateway_client: State<'_, GatewayState>,
) -> Result<(), String> {
    let url = gateway::validate_base_url(&url)?;
    gateway::validate_access_credentials(&access_client_id, &access_client_secret)?;
    let lifecycle = app.state::<InverterLifecycle>();
    // Serialize shutdown, startup and slot installation across Tauri command threads.
    let _lifecycle = lifecycle
        .0
        .lock()
        .map_err(|e| format!("Internal error: {}", e))?;
    let host_for_log = reqwest::Url::parse(&url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_else(|| "(invalid-url)".to_string());
    info!("connect_gateway: starting IGW host={host_for_log}");

    // Stop LAN MQTT so it cannot race remote updates.
    {
        let mut client_guard = mqtt_client
            .0
            .lock()
            .map_err(|e| format!("Internal error: {}", e))?;
        if let Some(old) = client_guard.take() {
            info!("connect_gateway: stopping prior LAN MQTT");
            old.stop();
        }
    }
    {
        let mut gw = gateway_client
            .0
            .lock()
            .map_err(|e| format!("Internal error: {}", e))?;
        if let Some(old) = gw.take() {
            info!("connect_gateway: stopping prior gateway client");
            old.stop();
        }
        let _ = app.emit("setpoint-override-update", serde_json::Value::Null);
        let client = gateway::start_gateway_client(
            app.clone(),
            url,
            access_client_id,
            access_client_secret,
            api_token.unwrap_or_default(),
            gateway::GatewayInstances {
                water_tank: water_tank_instance,
                water_pump: water_pump_instance,
                water_valve: water_valve_instance,
                ev: ev_instance,
                evcharger: evcharger_instance,
            },
        )?;
        *gw = Some(client);
        info!("connect_gateway: gateway client started host={host_for_log}");
    }
    Ok(())
}

/// Probe Cerbo MQTT (TCP + CONNACK) without leaving a permanent client.
#[tauri::command]
async fn test_mqtt_connection(
    host: String,
    port: u16,
    tls: Option<bool>,
    username: Option<String>,
    password: Option<String>,
) -> Result<(), String> {
    let user = username.clone();
    let pass = password.clone();
    tokio::task::spawn_blocking(move || {
        mqtt::test_mqtt_connection(
            &host,
            port,
            user.as_deref(),
            pass.as_deref(),
            tls.unwrap_or(false),
        )
    })
    .await
    .map_err(|e| format!("MQTT probe join error: {e}"))?
}

#[derive(Debug, Serialize, Deserialize)]
struct GatewayHealthResult {
    status: String,
    mqtt_connected: Option<bool>,
}

/// Probe remote inverter-gateway `/health` with optional Cloudflare Access and bearer.
#[tauri::command]
async fn test_gateway_connection(
    url: String,
    access_client_id: String,
    access_client_secret: String,
    api_token: Option<String>,
) -> Result<GatewayHealthResult, String> {
    let base = gateway::validate_base_url(&url)?;
    let health_url = format!("{}/health", base);
    let client = gateway::http_client()?;
    let req = gateway::authenticated_request(
        client
            .get(&health_url)
            .header("User-Agent", "inverter-desktop/gateway-test"),
        &access_client_id,
        &access_client_secret,
        api_token.as_deref().unwrap_or_default(),
    )?;
    let resp = req
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("Read body failed: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "HTTP {status}: {}",
            body.chars().take(200).collect::<String>()
        ));
    }
    let parsed: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("Invalid JSON: {e}; body={body}"))?;
    Ok(GatewayHealthResult {
        status: parsed
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("ok")
            .to_string(),
        mqtt_connected: parsed.get("mqtt_connected").and_then(|v| v.as_bool()),
    })
}

#[tauri::command]
async fn open_config_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Err(error) = auth::require_session(&app) {
        if let Some(main) = app.get_webview_window("main") {
            let _ = main.show();
            let _ = main.set_focus();
        }
        let _ = app.emit("auth-state-changed", ());
        return Err(error);
    }
    // Already open? Bring it to front instead of failing on duplicate label.
    // (unminimize/focused are desktop-only APIs)
    #[cfg(mobile)]
    {
        app.emit_to("main", "mobile-open-settings", ())
            .map_err(|e| e.to_string())
    }
    #[cfg(desktop)]
    {
        if let Some(window) = app.get_webview_window("config") {
            let _ = window.unminimize();
            let _ = window.show();
            let _ = window.set_focus();
            return Ok(());
        }
        let builder = tauri::WebviewWindowBuilder::new(
            &app,
            "config",
            tauri::WebviewUrl::App("config".into()),
        )
        .title("Configuration")
        .inner_size(CONFIG_WINDOW_W, CONFIG_WINDOW_H)
        .resizable(true)
        .focused(true);
        builder.build().map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(desktop)]
fn percent_encode_query(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(desktop)]
fn is_video_window_label(label: &str) -> bool {
    plugins::media_windows::is_plugin_video_label(label)
        || plugins::media_windows::is_plugin_preview_label(label)
}

/// Force an owned plugin media window to the default size, then stack top-right (see
/// [`position_camera_video_stacked`]). Call after create so window-state cannot stick.
#[cfg(desktop)]
fn apply_camera_video_window_defaults(app: &tauri::AppHandle, window: &tauri::WebviewWindow) {
    let _ = window.set_size(tauri::LogicalSize::new(
        CAMERA_VIDEO_WINDOW_W,
        CAMERA_VIDEO_WINDOW_H,
    ));
    position_camera_video_stacked(app, window);
}

/// Place a camera clip window at the top-right, packing into free slots.
///
/// Layout (physical pixels, same margin as edge inset = `CAMERA_VIDEO_WINDOW_MARGIN`):
/// Column-major from the top-right of the current (else primary) monitor's work area:
/// right column top→bottom, then the next column to the left, and so on.
/// Only **visible** peer plugin media windows count as occupied — closed or
/// closing windows that linger in `webview_windows()` are ignored so stacking
/// resets to `(right_x, top_y)` when none remain, and a gap left by a closed
/// middle clip can be filled by the next open.
///
/// Columns clamp so the left edge never goes past the monitor's left margin.
/// Does not focus the window.
#[cfg(desktop)]
fn position_camera_video_stacked(app: &tauri::AppHandle, window: &tauri::WebviewWindow) {
    let monitor = window
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| window.primary_monitor().ok().flatten());
    let Some(monitor) = monitor else {
        return;
    };

    let scale = monitor.scale_factor();
    let margin = (CAMERA_VIDEO_WINDOW_MARGIN * scale).round() as i32;
    // Exclude the menu bar/taskbar before assigning slots. Native clamping of
    // only the first window would otherwise make the next slot overlap it.
    let screen_pos = monitor.work_area().position;
    let screen_size = monitor.work_area().size;
    let window_size = window.outer_size().unwrap_or_else(|_| {
        tauri::PhysicalSize::new(
            (CAMERA_VIDEO_WINDOW_W * scale).round() as u32,
            (CAMERA_VIDEO_WINDOW_H * scale).round() as u32,
        )
    });
    let win_w = window_size.width as i32;
    let win_h = window_size.height as i32;

    let right_x = screen_pos.x + screen_size.width as i32 - win_w - margin;
    let top_y = screen_pos.y + margin;
    let bottom_limit = screen_pos.y + screen_size.height as i32 - win_h - margin;
    let left_limit = screen_pos.x + margin;
    let step_x = win_w + margin;
    let step_y = win_h + margin;
    let col_tolerance = (win_w / 2).max(1);
    let row_tolerance = (win_h / 2).max(1);

    let mut existing: Vec<(i32, i32)> = Vec::new();
    for (label, other) in app.webview_windows() {
        if label.as_str() == window.label() || !is_video_window_label(label.as_str()) {
            continue;
        }
        // Closed/closing windows can linger in webview_windows() with their last
        // outer_position; only count still-visible peers so stacking can reset.
        if !other.is_visible().unwrap_or(false) {
            continue;
        }
        let Ok(pos) = other.outer_position() else {
            continue;
        };
        existing.push((pos.x, pos.y));
    }

    let slot_occupied = |sx: i32, sy: i32| -> bool {
        existing
            .iter()
            .any(|(ex, ey)| (*ex - sx).abs() <= col_tolerance && (*ey - sy).abs() <= row_tolerance)
    };

    let (x, y) = if existing.is_empty() {
        (right_x, top_y)
    } else {
        // Scan free slots column-major: right→left, within each column top→bottom.
        let mut placed = None;
        let mut col = 0i32;
        'cols: loop {
            let sx = right_x - col * step_x;
            if sx < left_limit {
                break;
            }
            let mut sy = top_y;
            while sy <= bottom_limit {
                if !slot_occupied(sx, sy) {
                    placed = Some((sx, sy));
                    break 'cols;
                }
                sy += step_y;
            }
            col += 1;
            if col > 64 {
                // Safety: absurd number of columns — fall back to top-right.
                break;
            }
        }
        placed.unwrap_or((right_x, top_y))
    };

    let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
}

/// Regroup remaining visible camera clip windows into a solid column-major tile
/// from the top-right (same packing as [`position_camera_video_stacked`]).
/// Called after a camera window is destroyed so gaps from closed clips close up.
#[cfg(desktop)]
fn reflow_camera_video_windows(app: &tauri::AppHandle) {
    let mut windows: Vec<(tauri::WebviewWindow, i32, i32)> = Vec::new();
    for (label, win) in app.webview_windows() {
        if !is_video_window_label(label.as_str()) {
            continue;
        }
        if !win.is_visible().unwrap_or(false) {
            continue;
        }
        let Ok(pos) = win.outer_position() else {
            continue;
        };
        windows.push((win, pos.x, pos.y));
    }
    if windows.is_empty() {
        return;
    }

    // Visual stack reading order (column-major from top-right): rightmost, then topmost.
    windows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.cmp(&b.2)));

    let ref_win = &windows[0].0;
    let monitor = ref_win
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| ref_win.primary_monitor().ok().flatten());
    let Some(monitor) = monitor else {
        return;
    };

    let scale = monitor.scale_factor();
    let margin = (CAMERA_VIDEO_WINDOW_MARGIN * scale).round() as i32;
    let screen_pos = monitor.work_area().position;
    let screen_size = monitor.work_area().size;
    let window_size = ref_win.outer_size().unwrap_or_else(|_| {
        tauri::PhysicalSize::new(
            (CAMERA_VIDEO_WINDOW_W * scale).round() as u32,
            (CAMERA_VIDEO_WINDOW_H * scale).round() as u32,
        )
    });
    let win_w = window_size.width as i32;
    let win_h = window_size.height as i32;

    let right_x = screen_pos.x + screen_size.width as i32 - win_w - margin;
    let top_y = screen_pos.y + margin;
    let bottom_limit = screen_pos.y + screen_size.height as i32 - win_h - margin;
    let left_limit = screen_pos.x + margin;
    let step_x = win_w + margin;
    let step_y = win_h + margin;

    let mut slots: Vec<(i32, i32)> = Vec::with_capacity(windows.len());
    let mut col = 0i32;
    'cols: loop {
        let sx = right_x - col * step_x;
        if sx < left_limit {
            break;
        }
        let mut sy = top_y;
        while sy <= bottom_limit {
            slots.push((sx, sy));
            if slots.len() >= windows.len() {
                break 'cols;
            }
            sy += step_y;
        }
        col += 1;
        if col > 64 {
            break;
        }
    }

    for (i, (win, cur_x, cur_y)) in windows.into_iter().enumerate() {
        let Some(&(x, y)) = slots.get(i) else {
            break;
        };
        if cur_x == x && cur_y == y {
            continue;
        }
        let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
    }
}

#[tauri::command]
async fn close_config_window(window: tauri::Window) -> Result<(), String> {
    #[cfg(mobile)]
    {
        window
            .emit("mobile-close-settings", ())
            .map_err(|e| e.to_string())
    }
    #[cfg(desktop)]
    {
        window.close().map_err(|e| e.to_string())
    }
}

// === Auto-start management ===

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn get_autolaunch() -> Result<auto_launch::AutoLaunch, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_path = exe.to_string_lossy().to_string();
    auto_launch::AutoLaunchBuilder::new()
        .set_app_name("Inverter Desktop")
        .set_app_path(&exe_path)
        .set_macos_launch_mode(auto_launch::MacOSLaunchMode::LaunchAgent)
        .build()
        .map_err(|e| format!("Failed to create auto-launch: {}", e))
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tauri::command]
async fn set_auto_start(enable: bool) -> Result<(), String> {
    let auto = get_autolaunch()?;
    if enable {
        auto.enable()
            .map_err(|e| format!("Failed to enable auto-start: {}", e))?;
    } else {
        auto.disable()
            .map_err(|e| format!("Failed to disable auto-start: {}", e))?;
    }
    Ok(())
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tauri::command]
async fn get_auto_start() -> Result<bool, String> {
    let auto = get_autolaunch()?;
    auto.is_enabled()
        .map_err(|e| format!("Failed to check auto-start: {}", e))
}

// Mobile stubs - auto-start not supported on mobile
#[cfg(any(target_os = "android", target_os = "ios"))]
#[tauri::command]
async fn set_auto_start(_enable: bool) -> Result<(), String> {
    Ok(())
}

#[cfg(any(target_os = "android", target_os = "ios"))]
#[tauri::command]
async fn get_auto_start() -> Result<bool, String> {
    Ok(false)
}

#[tauri::command]
async fn send_notification(
    app: tauri::AppHandle,
    title: String,
    body: String,
) -> Result<(), String> {
    use tauri_plugin_notification::NotificationExt;
    app.notification()
        .builder()
        .title(&title)
        .body(&body)
        .show()
        .map_err(|e| format!("Notification error: {}", e))?;
    Ok(())
}

#[tauri::command]
fn set_window_hidden(hidden: bool, mqtt_client: State<'_, MqttState>) {
    app_visibility::WINDOW_HIDDEN.store(hidden, std::sync::atomic::Ordering::Relaxed);
    if !hidden {
        if let Ok(guard) = mqtt_client.0.lock() {
            if let Some(ref client) = *guard {
                client.emit_current_state(true);
            }
        }
    }
}

#[cfg(desktop)]
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mqtt_state = MqttState(Arc::new(Mutex::new(None)));
    let gateway_state = GatewayState(Arc::new(Mutex::new(None)));

    let builder = tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::new()
                // Keep frequent transport diagnostics from saturating the UI and disk.
                .level(log::LevelFilter::Info)
                .build(),
        )
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(mqtt_state)
        .manage(gateway_state)
        .manage(InverterLifecycle::default());

    #[cfg(any(target_os = "android", target_os = "ios"))]
    let builder = builder.plugin(mobile_credentials::init());

    #[cfg(target_os = "android")]
    let builder = builder.plugin(tauri_plugin_fs::init());

    #[cfg(desktop)]
    // Camera clip windows are ephemeral (fixed small size, stacked top-right); do not
    // persist/restore them or a prior huge size from .window-state.json will stick.
    let builder = builder.plugin(
        tauri_plugin_window_state::Builder::new()
            .with_filter(|label| {
                !is_video_window_label(label) && !plugins::live_view::is_live_window(label)
            })
            .build(),
    );

    #[cfg(desktop)]
    let builder = plugins::media_windows::register(builder);

    builder
        .invoke_handler(|invoke| {
            tauri::async_runtime::spawn_blocking(move || {
                let app = invoke.message.webview().app_handle().clone();
                #[cfg(desktop)]
                if plugins::live_view::is_live_window(invoke.message.webview().label()) {
                    invoke
                        .resolver
                        .reject("Remote plugin pages cannot invoke application commands");
                    return true;
                }
                #[cfg(desktop)]
                if plugins::media_windows::media_id_for_label(invoke.message.webview().label())
                    .is_some()
                    && !plugins::media_windows::preview_command_allowed(invoke.message.command())
                {
                    invoke
                        .resolver
                        .reject("Media windows can only access their own media controls");
                    return true;
                }
                if !auth::public_command(invoke.message.command()) {
                    if let Err(error) = auth::require_session(&app) {
                        let _ = app.emit("auth-state-changed", ());
                        invoke.resolver.reject(error);
                        return true;
                    }
                }
                #[cfg(desktop)]
                let handler: fn(tauri::ipc::Invoke) -> bool = tauri::generate_handler![
                    tariff_export::export_tariff,
                    release_info::get_release_info,
                    plugins::bridge::get_plugin_snapshot,
                    plugins::bridge::plugin_action,
                    plugins::bridge::get_live_preview_url,
                    plugins::bridge::reveal_plugin_video_window,
                    plugins::bridge::close_plugin_video_window,
                    plugins::bridge::drag_plugin_video_window,
                    plugins::bridge::get_plugin_manager_snapshot,
                    plugins::bridge::retry_configured_plugins,
                    plugins::bridge::get_plugin_settings,
                    plugins::bridge::get_plugin_settings_choices,
                    plugins::bridge::save_plugin_settings,
                    plugins::bridge::get_retained_plugin_data,
                    plugins::bridge::delete_retained_plugin_data,
                    plugins::bridge::preview_plugin_package,
                    plugins::bridge::install_plugin_package,
                    plugins::bridge::discard_plugin_package,
                    plugins::bridge::set_plugin_enabled,
                    plugins::bridge::get_plugin_groups,
                    plugins::bridge::set_plugin_group_enabled,
                    plugins::bridge::rollback_plugin_package,
                    plugins::bridge::uninstall_plugin_package,
                    plugins::bridge::remove_configured_plugin,
                    get_state,
                    get_setpoint_override,
                    set_setpoint_override,
                    disconnect_inverter,
                    perform_action,
                    connect_mqtt,
                    connect_gateway,
                    acknowledge_victron_banner,
                    get_config,
                    save_config,
                    backup_config,
                    restore_config,
                    test_mqtt_connection,
                    test_gateway_connection,
                    open_config_window,
                    close_config_window,
                    set_auto_start,
                    get_auto_start,
                    auth::auth_status,
                    auth::auth_logout,
                    auth::auth_login,
                    auth::auth_check,
                    auth::auth_biometric_available,
                    auth::auth_biometric,
                    send_notification,
                    set_window_hidden,
                ];
                #[cfg(mobile)]
                let handler: fn(tauri::ipc::Invoke) -> bool = tauri::generate_handler![
                    release_info::get_release_info,
                    get_state,
                    get_setpoint_override,
                    set_setpoint_override,
                    disconnect_inverter,
                    perform_action,
                    connect_mqtt,
                    connect_gateway,
                    acknowledge_victron_banner,
                    get_config,
                    save_config,
                    backup_config,
                    restore_config,
                    test_mqtt_connection,
                    test_gateway_connection,
                    open_config_window,
                    close_config_window,
                    set_auto_start,
                    get_auto_start,
                    auth::auth_status,
                    auth::auth_logout,
                    auth::auth_login,
                    auth::auth_check,
                    auth::auth_biometric_available,
                    auth::auth_biometric,
                    send_notification,
                    set_window_hidden,
                ];
                let resolver = invoke.resolver.clone();
                let handled = handler(invoke);
                if !handled {
                    resolver.reject("Unknown command");
                }
                handled
            });
            true
        })
        .setup(|app| {
            #[cfg(desktop)]
            plugins::bridge::install(app.handle());

            #[cfg(desktop)]
            {
                // Setup app menu with About, Edit and Window menus
                let about_item =
                    MenuItem::with_id(app, "about", "About Inverter Desktop", true, None::<&str>)?;
                let app_submenu = Submenu::with_items(
                    app,
                    "Inverter Desktop",
                    true,
                    &[
                        &about_item,
                        &PredefinedMenuItem::separator(app)?,
                        &PredefinedMenuItem::quit(app, Some("Quit"))?,
                    ],
                )?;

                let edit_submenu = Submenu::with_items(
                    app,
                    "Edit",
                    true,
                    &[
                        &PredefinedMenuItem::undo(app, None)?,
                        &PredefinedMenuItem::redo(app, None)?,
                        &PredefinedMenuItem::separator(app)?,
                        &PredefinedMenuItem::cut(app, None)?,
                        &PredefinedMenuItem::copy(app, None)?,
                        &PredefinedMenuItem::paste(app, None)?,
                        &PredefinedMenuItem::select_all(app, None)?,
                    ],
                )?;

                let window_submenu = Submenu::with_items(
                    app,
                    "Window",
                    true,
                    &[
                        &PredefinedMenuItem::minimize(app, None)?,
                        &PredefinedMenuItem::separator(app)?,
                        &PredefinedMenuItem::close_window(app, None)?,
                    ],
                )?;

                let menu = Menu::with_items(app, &[&app_submenu, &edit_submenu, &window_submenu])?;
                app.set_menu(menu)?;

                // Setup system tray with configuration menu
                info!("Building system tray...");
                TrayIconBuilder::with_id("main-tray")
                    .tooltip("Inverter Desktop")
                    .icon({
                        #[cfg(target_os = "macos")]
                        {
                            let (rgba, w, h) = tray_icon::render(None, None);
                            tauri::image::Image::new_owned(rgba, w, h)
                        }
                        #[cfg(not(target_os = "macos"))]
                        {
                            let img = image::load_from_memory(include_bytes!("../icons/icon.png"))
                                .expect("Failed to load tray icon")
                                .into_rgba8();
                            let (w, h) = img.dimensions();
                            tauri::image::Image::new_owned(img.into_raw(), w, h)
                        }
                    })
                    .menu(&tauri::menu::Menu::with_items(
                        app,
                        &[
                            &tauri::menu::MenuItem::with_id(
                                app,
                                "show",
                                "Show Dashboard",
                                true,
                                None::<&str>,
                            )?,
                            &tauri::menu::MenuItem::with_id(
                                app,
                                "config",
                                "Settings...",
                                true,
                                None::<&str>,
                            )?,
                            &tauri::menu::PredefinedMenuItem::separator(app)?,
                            &tauri::menu::MenuItem::with_id(
                                app,
                                "quit",
                                "Quit",
                                true,
                                None::<&str>,
                            )?,
                        ],
                    )?)
                    .on_menu_event(|app, event| match event.id.as_ref() {
                        "show" => {
                            if let Some(window) = app.get_webview_window("main") {
                                let _ = window.unminimize();
                                let _ = window.show();
                                let _ = window.set_focus();
                                let _ = app.emit("window-shown", ());
                            }
                        }
                        "config" => {
                            let app = app.clone();
                            tauri::async_runtime::spawn(async move {
                                let _ = open_config_window(app).await;
                            });
                        }
                        "quit" => {
                            app.exit(0);
                        }
                        _ => {}
                    })
                    .on_tray_icon_event(|tray, event| {
                        if let TrayIconEvent::Click {
                            button: MouseButton::Left,
                            ..
                        } = event
                        {
                            let app = tray.app_handle();
                            if let Some(window) = app.get_webview_window("main") {
                                let _ = window.unminimize();
                                let _ = window.show();
                                let _ = window.set_focus();
                                let _ = app.emit("window-shown", ());
                            }
                        }
                    })
                    .build(app)?;
                info!("Tray icon built successfully.");

                // Background task: update tray icon with live inverter state
                // macOS: renders custom bar-chart icon + tooltip
                // Other platforms: updates tooltip text only (no system font dependency)
                // Also monitors for critical alerts: low battery SoC, grid disconnection
                {
                    let mqtt_for_tray = app.state::<MqttState>().0.clone();
                    let gateway_for_tray = app.state::<GatewayState>().0.clone();
                    let app_for_tray = app.handle().clone();
                    tauri::async_runtime::spawn(async move {
                        let mut interval = tokio::time::interval(Duration::from_millis(1500));
                        // After sleep/wake, don't fire a burst of catch-up renders.
                        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                        // Track notification state to avoid spam
                        let mut low_battery_notified = false;
                        // One tray update. The loop below catches panics so a single
                        // bad tick can't silently kill the whole tray task.
                        let mut update_tray = || {
                            let state = {
                                // Same preference as get_state: IGW exclusive mode stops
                                // LAN MQTT, so the menu-bar sparkline must read GatewayState
                                // or solar/grid bars stay blank while the dashboard is live.
                                // Poisoned lock is not a broken client: keep going.
                                let gw = gateway_for_tray.lock().unwrap_or_else(|p| p.into_inner());
                                if let Some(ref client) = *gw {
                                    Some(client.get_state())
                                } else {
                                    drop(gw);
                                    let guard =
                                        mqtt_for_tray.lock().unwrap_or_else(|p| p.into_inner());
                                    guard.as_ref().map(|c| c.get_state())
                                }
                            };
                            if let Some(s) = state {
                                let solar = s.solar_total.unwrap_or(0.0) / 1000.0;
                                let batt = s.battery_soc.unwrap_or(0.0);
                                let grid_reading = s.gt.map(|v| v / 1000.0);
                                let grid = grid_reading.unwrap_or(0.0);
                                let tip = format!(
                                    "PV {:.1}kW  Battery {:.0}%  Grid {:+.1}kW",
                                    solar, batt, grid
                                );
                                if let Some(tray) = app_for_tray.tray_by_id("main-tray") {
                                    #[cfg(target_os = "macos")]
                                    {
                                        let (rgba, w, h) = tray_icon::render(s.solar_total, s.gt);
                                        let tauri_img = tauri::image::Image::new_owned(rgba, w, h);
                                        let _ = tray.set_title(None::<&str>);
                                        if let Err(e) = tray.set_icon(Some(tauri_img)) {
                                            log::warn!("Tray set_icon failed: {e}");
                                        }
                                    }
                                    if let Err(e) = tray.set_tooltip(Some(&tip)) {
                                        log::warn!("Tray set_tooltip failed: {e}");
                                    }
                                }

                                // Check for critical alerts
                                use tauri_plugin_notification::NotificationExt;

                                // Low battery alert (< 20%)
                                if batt > 0.0 && batt < 20.0 {
                                    if !low_battery_notified {
                                        let _ = app_for_tray
                                            .notification()
                                            .builder()
                                            .title("Inverter Desktop - Low Battery")
                                            .body(format!("Battery SoC dropped to {:.0}%!", batt))
                                            .show();
                                        low_battery_notified = true;
                                    }
                                } else {
                                    low_battery_notified = false;
                                }
                            }
                        };
                        loop {
                            interval.tick().await;
                            if let Err(p) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                                &mut update_tray,
                            )) {
                                let msg = p
                                    .downcast_ref::<String>()
                                    .map(String::as_str)
                                    .or_else(|| p.downcast_ref::<&str>().copied())
                                    .unwrap_or("unknown panic");
                                log::error!("Tray update tick panicked: {msg}");
                            }
                        }
                    });
                }
            }

            // Connection is owned by the frontend (`connectMqtt` /
            // `connect_gateway`) so we do not race a second Cerbo MQTT
            // client against remote gateway mode on startup.

            // Show window on startup
            info!("Showing main window...");
            let window = app.get_webview_window("main").unwrap();

            #[cfg(desktop)]
            {
                // Close / Minimize → hide (keep app running in menu bar) + notification
                let window_hide = window.clone();
                let app_handle_hide = app.handle().clone();
                window.on_window_event(move |event| match event {
                    WindowEvent::CloseRequested { api, .. } => {
                        api.prevent_close();
                        let _ = window_hide.hide();
                        let _ = app_handle_hide.emit("window-hidden", ());
                        use tauri_plugin_notification::NotificationExt;
                        let _ = app_handle_hide
                            .notification()
                            .builder()
                            .title("Inverter Desktop")
                            .body("Continuing to work in background, minimized to tray")
                            .show();
                    }
                    WindowEvent::Focused(false) => {
                        // macOS: Accessory mode has no dock icon, so minimize to dock is useless.
                        // Convert minimize to hide-to-tray instead.
                        #[cfg(target_os = "macos")]
                        if let Ok(true) = window_hide.is_minimized() {
                            let _ = window_hide.unminimize();
                            let _ = window_hide.hide();
                            let _ = app_handle_hide.emit("window-hidden", ());
                            use tauri_plugin_notification::NotificationExt;
                            let _ = app_handle_hide
                                .notification()
                                .builder()
                                .title("Inverter Desktop")
                                .body("Continuing to work in background, minimized to tray")
                                .show();
                        }
                        let _ = app_handle_hide.emit("window-blurred", ());
                    }
                    WindowEvent::Focused(true) => {
                        let _ = app_handle_hide.emit("window-focused", ());
                    }
                    _ => {}
                });
            }

            window.show().unwrap();

            // macOS: accessory mode keeps app in menu bar (tray icon visible) without dock icon
            #[cfg(target_os = "macos")]
            {
                info!("Setting activation policy to Accessory...");
                app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            }

            #[cfg(desktop)]
            {
                // Handle app menu events
                app.on_menu_event(move |app_handle, event| {
                    if event.id.as_ref() == "about" {
                        // Already open? Bring it to front instead of failing on duplicate label.
                        if let Some(window) = app_handle.get_webview_window("about") {
                            let _ = window.unminimize();
                            let _ = window.show();
                            let _ = window.set_focus();
                        } else {
                            let _ = tauri::WebviewWindowBuilder::new(
                                app_handle,
                                "about",
                                tauri::WebviewUrl::App("about".into()),
                            )
                            .title("About Inverter Desktop")
                            .inner_size(ABOUT_WINDOW_W, ABOUT_WINDOW_H)
                            .resizable(false)
                            .center()
                            .focused(true)
                            .build();
                        }
                    }
                });
            }

            info!("Setup block completed successfully.");
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, _event| {
            #[cfg(desktop)]
            plugins::bridge::on_run_event(_app, _event);
        });
}

#[cfg(test)]
mod dashboard_control_config_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn new_installation_visibility_defaults_differ_from_legacy_null_values() {
        let keys = [
            "show_batteries",
            "show_solar_production",
            "show_active_loads",
            "show_daily_stats",
            "show_ev",
            "show_home_section",
            "show_header_toggles",
        ];
        let defaults = serde_json::to_value(FullConfig::default()).unwrap();
        for key in keys {
            assert_eq!(
                defaults[key],
                matches!(key, "show_batteries" | "show_solar_production" | "show_ev")
            );
            for legacy in [serde_json::Value::Null, json!(false), json!(true)] {
                let mut saved = defaults.clone();
                saved[key] = legacy.clone();
                let decoded: FullConfig = serde_json::from_value(saved).unwrap();
                assert_eq!(
                    serde_json::to_value(decoded).unwrap()[key],
                    legacy != json!(false)
                );
            }
            let mut saved = defaults.clone();
            saved.as_object_mut().unwrap().remove(key);
            let decoded: FullConfig = serde_json::from_value(saved).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap()[key], true);
        }
    }

    fn roundtrip_controls(state_key: Option<&str>) -> serde_json::Value {
        let mut config = serde_json::to_value(FullConfig::default()).unwrap();
        let mut header =
            json!({"id": "custom_header", "label": "Custom", "entity": "switch.custom"});
        let mut home = json!({
            "id": "custom_home", "label": "Home", "entity": "switch.home",
            "domain": "switch", "enabled": true
        });
        if let Some(key) = state_key {
            header["state_key"] = json!(key);
            home["state_key"] = json!(key);
        }
        config["header_toggles_config"] = json!([header]);
        config["ha_entities"] = json!([home]);

        // Same typed boundary used by save_config/load_config: unknown fields
        // would silently disappear here and change which MQTT flag a UI uses.
        let decoded: FullConfig = serde_json::from_value(config.clone()).unwrap();
        let saved = serde_json::to_value(decoded).unwrap();
        assert_eq!(
            saved["header_toggles_config"],
            config["header_toggles_config"]
        );
        assert_eq!(saved["ha_entities"], config["ha_entities"]);
        saved
    }

    #[test]
    fn core_actions_reject_external_targets_without_echoing_them() {
        for key in inverter_control::FLAG_KEYS {
            assert!(validate_core_action_target(&serde_json::json!({"entity":key})).is_ok());
            assert!(validate_core_action_target(
                &serde_json::json!({"entity":format!("input_boolean.{key}")})
            )
            .is_ok());
        }
        for entity in [
            serde_json::json!("light.private_room"),
            serde_json::json!("switch.only_charging"),
            serde_json::Value::Null,
            serde_json::json!(["only_charging"]),
        ] {
            let error =
                validate_core_action_target(&serde_json::json!({"entity":entity})).unwrap_err();
            assert_eq!(error, "This device control requires an installed plugin");
        }
        assert!(validate_core_action_target(&serde_json::json!({"value":true})).is_ok());
    }

    #[test]
    fn legacy_camera_live_mappings_survive_core_configuration_roundtrip() {
        let mut value = serde_json::to_value(FullConfig::default()).unwrap();
        value["camera_live_urls"] = serde_json::json!({"front":"https://private.invalid/live"});
        let config: FullConfig = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(config).unwrap()["camera_live_urls"],
            value["camera_live_urls"]
        );
    }

    #[test]
    fn core_reads_and_resets_retain_private_camera_maps_without_exposing_or_retargeting_them() {
        let mut stored = FullConfig::default();
        stored.camera_live_urls.insert(
            "front".into(),
            "https://private.invalid/live?token=private-value".into(),
        );
        let mut public = public_core_config(stored.clone());
        let encoded = serde_json::to_string(&public).unwrap();
        assert!(!encoded.contains("private-value"));
        assert!(!encoded.contains("camera_live_urls"));
        public.mqtt_host = "edited-core-host".into();
        preserve_private_camera_config(&mut public, &stored).unwrap();
        assert_eq!(public.camera_live_urls, stored.camera_live_urls);
        let mut reset = FullConfig::default();
        preserve_private_camera_config(&mut reset, &stored).unwrap();
        assert_eq!(reset.camera_live_urls, stored.camera_live_urls);
        let mut forged = public_core_config(stored.clone());
        forged
            .camera_live_urls
            .insert("front".into(), "https://different.invalid".into());
        let error = preserve_private_camera_config(&mut forged, &stored).unwrap_err();
        assert_eq!(
            error,
            "Private camera mappings cannot be changed through core settings"
        );
        assert!(stored.camera_live_urls["front"].contains("private-value"));
    }

    #[test]
    fn legacy_desktop_settings_survive_core_config_roundtrip() {
        // Mobile excludes integration implementations, but encrypted settings
        // remain portable. Saving a core setting must not erase dormant data.
        let mut saved = serde_json::to_value(FullConfig::default()).unwrap();
        let legacy = json!({
            "ha_url": "https://ha.example.invalid",
            "ha_longlived_token": "test-only-token",
            "ha_use_direct_api": true,
            "mqtt_ha_host": "camera-broker.example.invalid",
            "mqtt_ha_port": 1884,
            "camera_enabled": true,
            "camera_topic": "frigate/events",
            "frigate_base_url": "https://clips.example.invalid",
            "ring_snapshot_url_template": "https://clips.example.invalid/{device_id}.jpg",
            "ha_appliance_entities": {"washer": "sensor.washer"},
            "ha_entities": [{"id": "guest", "label": "Guest", "entity": "switch.guest", "domain": "switch", "enabled": true}],
            "header_toggles_config": [{"id": "feed", "label": "No feed", "entity": "input_boolean.no_feed", "state_key": "no_feed"}]
        });
        for (key, value) in legacy.as_object().unwrap() {
            saved[key] = value.clone();
        }
        let mut config: FullConfig = serde_json::from_value(saved).unwrap();
        config.mqtt_host = "new-cerbo".into();
        let updated = serde_json::to_value(config).unwrap();
        assert_eq!(updated["mqtt_host"], "new-cerbo");
        for (key, value) in legacy.as_object().unwrap() {
            assert_eq!(&updated[key], value, "legacy key {key} was changed");
        }
    }

    #[test]
    fn saved_header_and_home_controls_preserve_their_state_key() {
        let saved = roundtrip_controls(Some("custom_status"));
        assert_eq!(
            saved["header_toggles_config"][0]["state_key"],
            "custom_status"
        );
        assert_eq!(saved["ha_entities"][0]["state_key"], "custom_status");
    }

    #[test]
    fn legacy_controls_without_state_key_keep_their_shape() {
        let saved = roundtrip_controls(None);
        assert!(saved["header_toggles_config"][0].get("state_key").is_none());
        assert!(saved["ha_entities"][0].get("state_key").is_none());
    }
}

#[cfg(test)]
mod setup_completed_tests {
    use super::*;

    fn base_config() -> FullConfig {
        FullConfig::default()
    }

    #[test]
    fn first_run_defaults_need_setup() {
        let config = base_config();
        assert!(needs_setup(&config, false));
        assert!(!resolve_setup_completed(&config, false));
    }

    #[test]
    fn explicit_setup_completed_skips_wizard() {
        let mut config = base_config();
        config.setup_completed = true;
        assert!(!needs_setup(&config, false));
        assert!(resolve_setup_completed(&config, true));
    }

    #[test]
    fn existing_saved_mqtt_config_migrates() {
        let mut config = base_config();
        config.setup_completed = false;
        config.mqtt_host = "Cerbo".into();
        assert!(config_looks_previously_configured(&config));
        assert!(resolve_setup_completed(&config, true));
        assert!(!needs_setup(&config, true));
    }

    #[test]
    fn existing_gateway_config_migrates() {
        let mut config = base_config();
        config.setup_completed = false;
        config.mqtt_host = String::new();
        config.gateway_enabled = true;
        config.gateway_url = Some("https://victron.example.com".into());
        assert!(config_looks_previously_configured(&config));
        assert!(resolve_setup_completed(&config, true));
    }

    #[test]
    fn empty_unsaved_config_needs_setup() {
        let mut config = base_config();
        config.mqtt_host = String::new();
        config.gateway_enabled = false;
        assert!(!config_looks_previously_configured(&config));
        assert!(needs_setup(&config, false));
        // Even if somehow persisted empty, do not migrate
        assert!(needs_setup(&config, true));
    }

    #[test]
    fn default_camera_enabled_is_true() {
        assert!(FullConfig::default().camera_enabled);
    }

    #[test]
    fn serde_default_setup_completed_is_false() {
        let json = r#"{"mqtt_host":"Cerbo","mqtt_port":1883,"ha_use_direct_api":false,"camera_enabled":false,"gateway_enabled":false}"#;
        let config: FullConfig = serde_json::from_str(json).expect("deserialize");
        assert!(!config.setup_completed);
        assert!(resolve_setup_completed(&config, true));
    }
}

#[cfg(test)]
mod inverter_disconnect_tests {
    use super::*;

    #[test]
    fn disconnect_waits_for_in_flight_startup_then_removes_its_client() {
        let mqtt = MqttState(Arc::new(Mutex::new(None)));
        let gateway = GatewayState(Arc::new(Mutex::new(None)));
        let lifecycle = InverterLifecycle::default();
        std::thread::scope(|scope| {
            let startup_guard = lifecycle.0.lock().unwrap();
            let (attempting, attempted) = std::sync::mpsc::channel();
            let (finished, done) = std::sync::mpsc::channel();
            let mqtt_ref = &mqtt;
            let gateway_ref = &gateway;
            let lifecycle_ref = &lifecycle;
            scope.spawn(move || {
                attempting.send(()).unwrap();
                stop_inverter_clients(mqtt_ref, gateway_ref, lifecycle_ref).unwrap();
                finished.send(()).unwrap();
            });
            attempted.recv().unwrap();
            assert!(done.try_recv().is_err());
            // The earlier startup owns the lifecycle lock until its client is installed.
            *mqtt.0.lock().unwrap() = Some(MqttClient::new(
                "localhost".into(),
                1883,
                None,
                None,
                "pending-start-test".into(),
            ));
            drop(startup_guard);
            done.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
        });
        assert!(mqtt.0.lock().unwrap().is_none());
        assert!(gateway.0.lock().unwrap().is_none());
    }

    #[test]
    fn disconnect_removes_both_owned_clients_and_can_repeat() {
        // Construct actual clients without starting a broker connection or HTTP task.
        let mqtt = MqttState(Arc::new(Mutex::new(Some(MqttClient::new(
            "localhost".into(),
            1883,
            None,
            None,
            "disconnect-test".into(),
        )))));
        let gateway = GatewayState(Arc::new(Mutex::new(Some(gateway::idle_test_client()))));
        stop_inverter_clients(&mqtt, &gateway, &InverterLifecycle::default()).unwrap();
        assert!(mqtt.0.lock().unwrap().is_none());
        assert!(gateway.0.lock().unwrap().is_none());
        stop_inverter_clients(&mqtt, &gateway, &InverterLifecycle::default()).unwrap();
    }
}

#[cfg(test)]
mod inverter_action_routing_tests {
    use super::*;
    use serde_json::json;

    fn mqtt_slot() -> MqttState {
        MqttState(Arc::new(Mutex::new(Some(MqttClient::new(
            "localhost".into(),
            1883,
            None,
            None,
            "route-test".into(),
        )))))
    }

    #[tokio::test]
    async fn gateway_slot_routes_header_actions_without_a_mqtt_client() {
        let gateway = GatewayState(Arc::new(Mutex::new(Some(gateway::idle_test_client()))));
        let mqtt = MqttState(Arc::new(Mutex::new(None)));
        // The idle fixture deliberately has no URL; reaching its HTTPS validation
        // proves routing without dispatching a request or touching real devices.
        for (action, body) in [
            ("dry_run", json!({"value":true})),
            ("ess_mode", json!({})),
            ("toggle", json!({"entity":"only_charging","state":"on"})),
        ] {
            let error = perform_inverter_action(action, body, &mqtt, &gateway)
                .await
                .unwrap_err();
            assert_eq!(error, "Gateway URL is required");
        }
    }

    #[tokio::test]
    async fn gateway_failure_does_not_fall_back_to_present_mqtt_slot() {
        let gateway = GatewayState(Arc::new(Mutex::new(Some(gateway::idle_test_client()))));
        let error = perform_inverter_action("ess_mode", json!({}), &mqtt_slot(), &gateway)
            .await
            .unwrap_err();
        assert_eq!(error, "Gateway URL is required");
    }

    #[tokio::test]
    async fn mqtt_selection_preserves_its_command_path_and_disconnected_error() {
        let gateway = GatewayState(Arc::new(Mutex::new(None)));
        let error =
            perform_inverter_action("dry_run", json!({"value":false}), &mqtt_slot(), &gateway)
                .await
                .unwrap_err();
        // Existing MQTT client exists but its broker slot has not connected.
        assert_eq!(error, "MQTT client not connected");
        let absent = MqttState(Arc::new(Mutex::new(None)));
        assert_eq!(
            perform_inverter_action("ess_mode", json!({}), &absent, &gateway)
                .await
                .unwrap_err(),
            "Neither MQTT nor IGW is connected"
        );
    }

    #[test]
    fn missing_mqtt_override_status_is_unknown_not_inactive() {
        let absent = MqttState(Arc::new(Mutex::new(None)));
        assert!(mqtt_setpoint_override(&absent).is_err());
        let mqtt = mqtt_slot();
        assert!(mqtt_setpoint_override(&mqtt).is_err());
        mqtt.0
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .setpoint_override = Some(SetpointOverrideStatus {
            value: None,
            request_id: Some("accepted-stop".into()),
            last_error: None,
        });
        let inactive = mqtt_setpoint_override(&mqtt).unwrap();
        assert_eq!(inactive.value, None);
        assert_eq!(inactive.request_id.as_deref(), Some("accepted-stop"));
    }

    #[test]
    fn gateway_selection_receives_the_same_explicit_instances_as_mqtt() {
        let config = FullConfig {
            water_tank_instance: Some(21),
            water_pump_instance: Some(7),
            water_valve_instance: Some(9),
            ev_instance: Some(22),
            evcharger_instance: Some(40),
            ..FullConfig::default()
        };
        let instances = configured_gateway_instances(&config);
        assert_eq!(instances.water_tank, Some(21));
        assert_eq!(instances.water_pump, Some(7));
        assert_eq!(instances.water_valve, Some(9));
        assert_eq!(instances.ev, Some(22));
        assert_eq!(instances.evcharger, Some(40));
    }
}
