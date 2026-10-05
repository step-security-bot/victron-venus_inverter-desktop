use crate::notification_session::{NotificationSession, SessionAppHandle};
use chrono::{TimeZone, Utc};
use rumqttc::{Client, ConnectReturnCode, MqttOptions, Packet, QoS, SubscribeFilter};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod cerbo;
mod lifecycle;
mod security;
use lifecycle::{CancelOnDrop, Shutdown, StateEmitter};

const MQTT_KEEP_ALIVE_SECS: u64 = 60;
const KEEPALIVE_INTERVAL_SECS: u64 = 45;
/// Must stay above the burst of control requests we enqueue from inside
/// `connection.iter()` handlers. rumqttc `Client::subscribe` uses a *blocking*
/// send on this channel; if the handler fills it while the event loop is
/// stuck in that same handler, the MQTT thread deadlocks permanently.
/// `subscribe_portal_topics` historically issued one subscribe per filter; with
/// acload (3512a15) that became 11 > 10 and froze the loop right after
/// "Discovered Cerbo portal ID" — UI stuck at all zeros, no state emits.
const MQTT_QUEUE_CAPACITY: usize = 64;
/// Calm MQTT reconnect delay (seconds): 5 → 10 → 20 → 40 → 60 cap.
pub fn mqtt_reconnect_delay_secs(attempt: u32) -> u64 {
    let shift = attempt.min(4);
    let delay = 5u64.saturating_mul(1u64 << shift);
    delay.min(60)
}

/// Short TCP + MQTT CONNACK probe. Does not leave a permanent client.
/// Used for startup reachability and dual-path MQTT recovery while on IGW.
pub fn test_mqtt_connection(
    host: &str,
    port: u16,
    username: Option<&str>,
    password: Option<&str>,
    tls: bool,
) -> Result<(), String> {
    let transport = security::transport(tls, username, password)?;
    let host = host.trim();
    if host.is_empty() {
        return Err("MQTT host is required".into());
    }

    let addr_str = format!("{host}:{port}");
    let mut addrs = addr_str
        .to_socket_addrs()
        .map_err(|e| format!("MQTT resolve {addr_str}: {e}"))?;
    let sock: SocketAddr = addrs
        .next()
        .ok_or_else(|| format!("MQTT resolve {addr_str}: no addresses"))?;

    // TCP reachability first (short timeout).
    TcpStream::connect_timeout(&sock, Duration::from_secs(3))
        .map_err(|e| format!("MQTT TCP {addr_str}: {e}"))?;

    // MQTT CONNECT / CONNACK on a helper thread so a stuck broker cannot block
    // the caller beyond `overall_timeout`.
    let host_owned = host.to_string();
    let user_owned = username.map(|s| s.to_string());
    let pass_owned = password.map(|s| s.to_string());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = probe_mqtt_connack(
            &host_owned,
            port,
            user_owned.as_deref(),
            pass_owned.as_deref(),
            transport,
        );
        let _ = tx.send(result);
    });
    match rx.recv_timeout(Duration::from_secs(8)) {
        Ok(r) => r,
        Err(_) => Err(format!("MQTT probe timed out for {addr_str}")),
    }
}

fn probe_mqtt_connack(
    host: &str,
    port: u16,
    username: Option<&str>,
    password: Option<&str>,
    transport: rumqttc::Transport,
) -> Result<(), String> {
    let client_id = format!(
        "inverter-desktop-probe-{:06x}",
        rand::random::<u32>() & 0xFF_FFFF
    );
    let mut opts = MqttOptions::new(&client_id, (host.to_string(), port));
    opts.set_transport(transport);
    opts.set_keep_alive(10u16);
    if let (Some(u), Some(p)) = (username, password) {
        if !u.is_empty() && !p.is_empty() {
            opts.set_credentials(u, p.to_string());
        }
    }
    let (client, mut connection) = Client::builder(opts).capacity(4).build();
    for event in connection.iter() {
        match event {
            Ok(rumqttc::Event::Incoming(Packet::ConnAck(ack))) => {
                let _ = client.disconnect();
                return if ack.code == ConnectReturnCode::Success {
                    Ok(())
                } else {
                    Err(format!("MQTT CONNACK refused: {:?}", ack.code))
                };
            }
            Ok(rumqttc::Event::Incoming(_)) | Ok(rumqttc::Event::Outgoing(_)) => {}
            Err(e) => {
                let _ = client.disconnect();
                return Err(format!("MQTT probe error: {e}"));
            }
        }
    }
    let _ = client.disconnect();
    Err("MQTT connection closed before ConnAck".into())
}

/// Rate-limited counter so we can confirm mqtt-state-update IPC is flowing
/// without flooding the log (one line every ~5s).
fn note_state_emit(kind: &str) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static EMITS: AtomicU64 = AtomicU64::new(0);
    static LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);
    let n = EMITS.fetch_add(1, Ordering::Relaxed) + 1;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let prev = LAST_LOG_MS.load(Ordering::Relaxed);
    if now_ms.saturating_sub(prev) >= 5000
        && LAST_LOG_MS
            .compare_exchange(prev, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        log::info!("mqtt-state-update emitted x{n} (last={kind})");
    }
}

/// Rate-limited confirmation that inverter/state MQTT messages are arriving
/// and parsing (one line every ~5s). Absence of this line with portal
/// discovery present means the daemon is not publishing or the MQTT loop
/// is stuck before handle_message.
/// Throttled recv counter for inverter/state. Live gt/tt/soc are Cerbo-only
/// (systemcalc + voltage_soc); never log raw daemon fields for those tiles.
fn note_inverter_state_recv(live: &InverterState) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static RECVS: AtomicU64 = AtomicU64::new(0);
    static LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);
    let n = RECVS.fetch_add(1, Ordering::Relaxed) + 1;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let prev = LAST_LOG_MS.load(Ordering::Relaxed);
    if now_ms.saturating_sub(prev) >= 5000
        && LAST_LOG_MS
            .compare_exchange(prev, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        log::info!(
            "inverter/state received x{n} (live gt={:?} tt={:?} soc={:?})",
            live.gt,
            live.tt,
            live.battery_soc
        );
    }
}

/// One Cerbo GX instance discovered under tank/pump/ev/evcharger portal topics.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveredInstance {
    pub instance: u32,
    /// "tank" | "pump" | "ev" | "evcharger"
    pub kind: String,
    /// CustomName, else ProductName, when published.
    pub name: Option<String>,
}

/// Authoritative override state acknowledged by inverter-control on Cerbo.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SetpointOverrideStatus {
    pub value: Option<i32>,
    pub last_error: Option<String>,
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GridBackupStatus {
    pub enabled: bool,
    pub available: bool,
    pub service: Option<String>,
    pub device_instance: Option<u32>,
    pub name: Option<String>,
    pub power: Option<f64>,
    pub measurement_time: Option<f64>,
    pub age_seconds: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InverterState {
    /// Full IGW update: clear absent IGW-owned fields, unlike partial MQTT events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_snapshot: Option<bool>,
    /// Backend failed-poll replay, not a fresh observation or an upstream field.
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub cached_snapshot: Option<bool>,
    pub grid_backup: Option<GridBackupStatus>,
    pub grid_using_backup: Option<bool>,
    /// Genuine daemon receipt time; unrelated Cerbo overlays never renew it.
    pub grid_backup_observed_at: Option<f64>,
    pub setpoint_override: Option<SetpointOverrideStatus>,
    pub gt: Option<f64>,
    pub g1: Option<f64>,
    pub g2: Option<f64>,
    /// Explicit source validity. None means this phase has not been observed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grid_l1_available: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grid_l2_available: Option<bool>,
    pub tt: Option<f64>,
    pub t1: Option<f64>,
    pub t2: Option<f64>,
    pub solar_total: Option<f64>,
    pub mppt_total: Option<f64>,
    pub battery_soc: Option<f64>,
    pub battery_power: Option<f64>,
    pub battery_voltage: Option<f64>,
    pub battery_current: Option<f64>,
    pub setpoint: Option<f64>,
    pub inverter_state: Option<String>,
    pub version: Option<String>,
    pub dashboard_version: Option<String>,
    pub uptime: Option<u64>,
    pub ha_connected: Option<bool>,
    pub ha_direct_connected: Option<bool>,
    pub dry_run: Option<bool>,
    pub ess_mode: Option<EssMode>,
    #[serde(default, skip_deserializing)]
    pub ess_mode_observed_at: Option<f64>,
    pub booleans: Option<std::collections::HashMap<String, bool>>,
    pub features: Option<std::collections::HashMap<String, bool>>,
    pub mppt_individual: Option<Vec<f64>>,
    pub mppt_chargers: Option<Vec<MpptCharger>>,
    pub pv_inverters: Option<Vec<PvInverter>>,
    pub pv_inverter_individual: Option<Vec<f64>>,
    pub batteries: Option<Vec<Battery>>,
    pub loads: Option<std::collections::HashMap<String, f64>>,
    /// Cerbo acload instance id → CustomName/ProductName. Loads stay keyed by
    /// stable instance id so power updates never flash raw ids once a name is known.
    pub load_names: Option<std::collections::HashMap<String, String>>,
    pub ui_config: Option<UiConfig>,
    pub daily_stats: Option<DailyStats>,
    pub solar_forecast: Option<SolarForecast>,
    pub ev_charging_kw: Option<f64>,
    pub ev_power: Option<f64>,
    pub car_soc: Option<f64>,
    pub ev_charging_power: Option<f64>,
    pub car_charging_power: Option<f64>,
    /// Cerbo MQTT has published at least one ev/<i>/... message for a
    /// configured ev_instance. Survives process_state_update clones/merges.
    pub ev_present: bool,
    /// Same for evcharger/<i>/... messages.
    pub evcharger_present: bool,
    /// Cerbo GX water (tank/pump) + EV (ev/evcharger) instances seen on MQTT.
    /// Populated at connect/keepalive via broad portal subscriptions; Config UI lists them.
    pub discovered_water_ev: Option<Vec<DiscoveredInstance>>,
    pub water_level: Option<f64>,
    pub water_valve: Option<bool>,
    pub pump_switch: Option<bool>,
    /// dbus-pump /Mode per device (0 auto, 1 always-on, 2 always-off);
    /// None until the retained Mode topic arrives.
    pub water_pump_mode: Option<u8>,
    pub water_valve_mode: Option<u8>,
    pub dishwasher_running: Option<bool>,
    pub dishwasher_duration: Option<u64>,
    pub washer_time: Option<u64>,
    pub washer_power: Option<bool>,
    pub dryer_time: Option<u64>,
    pub dryer_power: Option<bool>,
    pub latest_version: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawInverterState {
    #[serde(skip)]
    ess_mode_retained: bool,
    #[serde(default, deserialize_with = "deserialize_grid_backup")]
    grid_backup: Option<Option<GridBackupStatus>>,
    grid_using_backup: Option<bool>,
    // Deserialized for completeness / tests; never merged into live tiles.
    #[allow(dead_code)]
    gt: Option<f64>,
    #[allow(dead_code)]
    g1: Option<f64>,
    #[allow(dead_code)]
    g2: Option<f64>,
    #[allow(dead_code)]
    tt: Option<f64>,
    #[allow(dead_code)]
    t1: Option<f64>,
    #[allow(dead_code)]
    t2: Option<f64>,
    solar_total: Option<f64>,
    // Never merged — Cerbo voltage_soc owns the SoC tile.
    #[allow(dead_code)]
    battery_soc: Option<f64>,
    // Canonical + short battery keys. Do NOT use #[serde(alias = "bp")] etc:
    // the daemon publishes BOTH forms in one object, and serde aliases treat
    // them as the same field → "duplicate field `battery_power`" and the
    // entire inverter/state payload is rejected (Consumption/Setpoint stay 0).
    battery_power: Option<f64>,
    battery_voltage: Option<f64>,
    battery_current: Option<f64>,
    /// Short key historically published alongside/instead of battery_power.
    bp: Option<f64>,
    /// Short key historically published alongside/instead of battery_voltage.
    bv: Option<f64>,
    /// Short key historically published alongside/instead of battery_current.
    bc: Option<f64>,
    setpoint: Option<f64>,
    inverter_state: Option<String>,
    version: Option<String>,
    dashboard_version: Option<String>,
    uptime: Option<u64>,
    ha_connected: Option<bool>,
    ha_direct_connected: Option<bool>,
    dry_run: Option<serde_json::Value>,
    ess_mode: Option<EssMode>,
    booleans: Option<std::collections::HashMap<String, serde_json::Value>>,
    features: Option<std::collections::HashMap<String, bool>>,
    mppt_individual: Option<Vec<f64>>,
    mppt_chargers: Option<Vec<MpptCharger>>,
    pv_inverters: Option<Vec<PvInverter>>,
    pv_inverter_individual: Option<Vec<f64>>,
    batteries: Option<Vec<Battery>>,
    loads: Option<std::collections::HashMap<String, f64>>,
    ui_config: Option<UiConfig>,
    daily_stats: Option<DailyStats>,
    solar_forecast: Option<SolarForecast>,
    // ev_charging_kw, ev_power, car_soc are intentionally absent —
    // EV telemetry comes only from Cerbo MQTT via apply_ev_message + EvCache,
    // not from the daemon's inverter/state payload.
    // gt/g1/g2, tt/t1/t2, battery_soc: intentionally never merged — live
    // tiles come from Cerbo only (systemcalc g1+g2 / t1+t2, voltage_soc).
    // battery power/V/A, loads, setpoint/mode, MPPT/PV/batteries arrays, and
    // solar_total may still appear for fallback; process_state_update skips
    // merging them once Cerbo owns those tiles — same EV wipe-protection.
    // water_* / washer_* / dryer_* / dishwasher_*: intentionally absent —
    // water from Cerbo tank/pump handlers; appliances from HA entities only.
    // Ignoring them in JSON prevents daemon zeros from being tempting to merge.
    latest_version: Option<String>,
}

fn deserialize_grid_backup<'de, D>(
    deserializer: D,
) -> Result<Option<Option<GridBackupStatus>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<GridBackupStatus>::deserialize(deserializer).map(Some)
}

impl RawInverterState {
    /// Prefer canonical battery_* keys; fall back to short bp/bv/bc when the
    /// long form is absent. Safe when both are present (no serde duplicate).
    fn resolve_short_battery_keys(&mut self) {
        if self.battery_power.is_none() {
            self.battery_power = self.bp;
        }
        if self.battery_voltage.is_none() {
            self.battery_voltage = self.bv;
        }
        if self.battery_current.is_none() {
            self.battery_current = self.bc;
        }
    }
}

fn coerce_bool(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::String(s) => {
            let s_low = s.to_lowercase();
            s_low == "true" || s_low == "1" || s_low == "on" || s_low == "online"
        }
        serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
        _ => false,
    }
}

/// Bank % from pack voltage — same paradigm as the HA "Battery %" template
/// sensor: linear 40-54.4 V, clamped to 0-100, rounded. The shunt's own SoC
/// counter reads a bogus 100% while charging, so the UI never shows it.
pub(crate) fn voltage_soc(voltage: f64) -> f64 {
    const V_MIN: f64 = 40.0; // V -> 0%
    const V_MAX: f64 = 54.4; // V -> 100% (absorption)
    (((voltage - V_MIN) / (V_MAX - V_MIN)) * 100.0)
        .clamp(0.0, 100.0)
        .round()
}

/// Victron VE.Bus /State codes — mirrors inverter-control INVERTER_STATES.
pub(crate) fn inverter_state_name(code: u32) -> String {
    match code {
        0 => "Off".into(),
        1 => "Low Power".into(),
        2 => "Fault".into(),
        3 => "Bulk".into(),
        4 => "Absorption".into(),
        5 => "Float".into(),
        6 => "Storage".into(),
        7 => "Equalize".into(),
        8 => "Passthru".into(),
        9 => "Inverting".into(),
        10 => "Power assist".into(),
        11 => "Power supply".into(),
        252 => "External control".into(),
        other => format!("? ({other})"),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EssMode {
    pub selected: Option<String>,
    pub vebus_mode: Option<u8>,
    pub selection_supported: Option<bool>,
    pub request_id: Option<String>,
    pub error: Option<String>,
    pub mode_name: Option<String>,
    pub is_external: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MpptCharger {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pv_voltage: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub power: Option<f64>,
}

/// AC PV inverter of any vendor (Tasmota plug, ESPHome, Fronius, ...):
/// published on the GX broker by dbus services as N/<portal>/pvinverter/<id>.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PvInverter {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voltage: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub power: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Vebus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l1_power: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l2_power: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ac_power: Option<f64>,
    /// Hub4/L1/AcPowerSetpoint (W).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setpoint: Option<f64>,
    /// VE.Bus /State label (Bulk, Absorption, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inverter_state: Option<String>,
}

/// systemcalc aggregates on com.victronenergy.system (MQTT N/.../system/0/...).
#[derive(Debug, Clone, Default)]
struct SystemTotals {
    g1: Option<f64>,
    g2: Option<f64>,
    // Distinguish an explicit MQTT null from an unrelated partial update.
    g1_seen: bool,
    g2_seen: bool,
    t1: Option<f64>,
    t2: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Battery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub soc: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voltage: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub power: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_to_go: Option<String>,
    /// From battery MQTT System/MaxCellVoltage -- not in platform Notifications.
    #[serde(skip)]
    pub max_cell_voltage: Option<f64>,
    /// From battery MQTT System/MaxVoltageCellId.
    #[serde(skip)]
    pub max_voltage_cell_id: Option<String>,
    /// From battery MQTT System/MinCellVoltage.
    #[serde(skip)]
    pub min_cell_voltage: Option<f64>,
    /// From battery MQTT System/MinVoltageCellId.
    #[serde(skip)]
    pub min_voltage_cell_id: Option<String>,
}

/// Wrapper that tracks when a device was last seen via MQTT, enabling
/// per-device TTL eviction instead of a global map wipe.
struct TrackedEntry<T> {
    data: T,
    last_seen: Instant,
}

impl<T: Default> Default for TrackedEntry<T> {
    fn default() -> Self {
        Self {
            data: T::default(),
            last_seen: Instant::now(),
        }
    }
}

impl<T> TrackedEntry<T> {
    fn touch(&mut self) {
        self.last_seen = Instant::now();
    }
}

/// Devices discovered directly on the Cerbo GX MQTT broker
/// (N/<portal>/battery/..., N/<portal>/solarcharger/...,
/// N/<portal>/pvinverter/...), independent of the inverter-control daemon's
/// inverter/state payload. BTreeMap keeps a stable instance-ordered list for
/// the UI.
/// Tile identity for duplicate-name disambiguation: identical units ship one
/// shared ProductName ("SmartSolar Charger MPPT 100/20 48V" x N), which reads
/// as the same tile repeated.
trait DeviceIdentity {
    fn display_name(&self) -> Option<&str>;
    fn name_slot(&mut self) -> &mut Option<String>;
    fn serial(&self) -> Option<&str>;
}

impl DeviceIdentity for MpptCharger {
    fn display_name(&self) -> Option<&str> {
        self.name.as_deref()
    }
    fn name_slot(&mut self) -> &mut Option<String> {
        &mut self.name
    }
    fn serial(&self) -> Option<&str> {
        self.serial.as_deref()
    }
}

impl DeviceIdentity for PvInverter {
    fn display_name(&self) -> Option<&str> {
        self.name.as_deref()
    }
    fn name_slot(&mut self) -> &mut Option<String> {
        &mut self.name
    }
    fn serial(&self) -> Option<&str> {
        self.serial.as_deref()
    }
}

impl DeviceIdentity for Battery {
    fn display_name(&self) -> Option<&str> {
        self.name.as_deref()
    }
    fn name_slot(&mut self) -> &mut Option<String> {
        &mut self.name
    }
    fn serial(&self) -> Option<&str> {
        self.serial.as_deref()
    }
}

/// One Victron acload service (dbus-emporia-vue circuit, etc.) discovered on
/// the Cerbo GX MQTT broker. Watts stay keyed by instance; display name is
/// cached separately so UI never flickers back to bare ids.
#[derive(Debug, Clone, Default)]
struct AcLoad {
    power: Option<f64>,
    /// CustomName when published (preferred).
    custom_name: Option<String>,
    /// ProductName fallback when CustomName has not arrived yet.
    product_name: Option<String>,
}

impl AcLoad {
    fn display_name(&self) -> Option<&str> {
        self.custom_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| self.product_name.as_deref().filter(|s| !s.is_empty()))
    }
}

/// Named Cerbo service (tank/pump/ev/evcharger) tracked for Config discovery.
#[derive(Debug, Clone, Default)]
struct NamedDevice {
    custom_name: Option<String>,
    product_name: Option<String>,
}

impl NamedDevice {
    fn display_name(&self) -> Option<&str> {
        self.custom_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| self.product_name.as_deref().filter(|s| !s.is_empty()))
    }
}

/// Devices discovered directly on the Cerbo GX MQTT broker
/// (N/<portal>/battery/..., N/<portal>/solarcharger/...,
/// N/<portal>/pvinverter/..., N/<portal>/acload/...,
/// N/<portal>/tank|pump|ev|evcharger/...), independent of the
/// inverter-control daemon's inverter/state payload. BTreeMap keeps a stable
/// instance-ordered list for the UI.
#[derive(Default)]
struct CerboDevices {
    batteries: BTreeMap<u32, TrackedEntry<Battery>>,
    chargers: BTreeMap<u32, TrackedEntry<MpptCharger>>,
    pv_inverters: BTreeMap<u32, TrackedEntry<PvInverter>>,
    vebus: BTreeMap<u32, TrackedEntry<Vebus>>,
    /// system/0 Ac/Grid + Ac/Consumption (preferred source for gt/tt).
    system: BTreeMap<u32, TrackedEntry<SystemTotals>>,
    acloads: BTreeMap<u32, TrackedEntry<AcLoad>>,
    /// dbus-pump tank Level (+ names)
    tanks: BTreeMap<u32, TrackedEntry<NamedDevice>>,
    /// dbus-pump startstop (pump + valve share this service type)
    pumps: BTreeMap<u32, TrackedEntry<NamedDevice>>,
    /// dbus-ev vehicle instances
    evs: BTreeMap<u32, TrackedEntry<NamedDevice>>,
    /// dbus-evcharger wallbox instances
    evchargers: BTreeMap<u32, TrackedEntry<NamedDevice>>,
}

impl CerboDevices {
    /// How long a discovered entry survives without any fresh publish.
    const DEVICE_TTL_SECS: u64 = 120;

    /// Evict entries whose `last_seen` is older than the TTL. Unlike the old
    /// global map wipe, this preserves active entries when other devices are
    /// updated — only truly stale (disconnected) devices are removed.
    fn sweep_stale(&mut self) {
        let ttl = Duration::from_secs(Self::DEVICE_TTL_SECS);
        self.batteries
            .retain(|_, entry| entry.last_seen.elapsed() < ttl);
        self.chargers
            .retain(|_, entry| entry.last_seen.elapsed() < ttl);
        self.pv_inverters
            .retain(|_, entry| entry.last_seen.elapsed() < ttl);
        self.vebus
            .retain(|_, entry| entry.last_seen.elapsed() < ttl);
        self.system
            .retain(|_, entry| entry.last_seen.elapsed() < ttl);
        self.acloads
            .retain(|_, entry| entry.last_seen.elapsed() < ttl);
        self.tanks
            .retain(|_, entry| entry.last_seen.elapsed() < ttl);
        self.pumps
            .retain(|_, entry| entry.last_seen.elapsed() < ttl);
        self.evs.retain(|_, entry| entry.last_seen.elapsed() < ttl);
        self.evchargers
            .retain(|_, entry| entry.last_seen.elapsed() < ttl);
    }

    fn has_shunt(&self) -> bool {
        self.batteries.values().any(|e| {
            e.data
                .name
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .contains("shunt")
        })
    }

    /// systemcalc Ac/Grid seen — Cerbo owns gt/g1/g2 (vebus is fallback only).
    #[allow(dead_code)] // overlay uses system fields directly; kept for owns_* API
    fn owns_grid(&self) -> bool {
        self.system
            .values()
            .any(|e| e.data.g1_seen || e.data.g2_seen)
            || self.vebus.values().any(|e| {
                e.data.l1_power.is_some() || e.data.l2_power.is_some() || e.data.ac_power.is_some()
            })
    }

    /// systemcalc Ac/Consumption seen — Cerbo owns tt/t1/t2.
    #[allow(dead_code)] // overlay uses system fields directly; kept for owns_* API
    fn owns_consumption(&self) -> bool {
        self.system
            .values()
            .any(|e| e.data.t1.is_some() || e.data.t2.is_some())
    }

    /// VE.Bus discovered — Cerbo owns Hub4 setpoint + /State label.
    fn owns_vebus_mode(&self) -> bool {
        self.vebus
            .values()
            .any(|e| e.data.setpoint.is_some() || e.data.inverter_state.is_some())
    }

    fn owns_chargers(&self) -> bool {
        !self.chargers.is_empty()
    }

    fn owns_pv(&self) -> bool {
        !self.pv_inverters.is_empty()
    }

    fn owns_batteries(&self) -> bool {
        !self.batteries.is_empty()
    }

    fn owns_solar(&self) -> bool {
        self.owns_chargers() || self.owns_pv()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiConfig {
    pub electricity_tariff: Option<serde_json::Value>,
    pub electricity_tariff_status: Option<serde_json::Value>,
    pub loads: Option<LoadsConfig>,
    pub home_buttons: Option<Vec<HomeButton>>,
    pub header_toggles: Option<Vec<HeaderToggle>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoadsConfig {
    pub hidden: Option<Vec<String>>,
    pub min_watts: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HomeButton {
    pub id: String,
    pub label: String,
    pub entity: String,
    pub state_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeaderToggle {
    pub id: String,
    pub label: String,
    pub entity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_key: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DailyStats {
    pub produced_today: Option<f64>,
    pub produced_yesterday: Option<f64>,
    pub produced_dollars: Option<f64>,
    pub grid_kwh: Option<f64>,
    // Keep this optional telemetry envelope independent of the other daily statistics.
    // The frontend validates its date, coverage and freshness before displaying counters.
    pub grid_energy: Option<serde_json::Value>,
    pub battery_in: Option<f64>,
    pub battery_out: Option<f64>,
    pub battery_in_yesterday: Option<f64>,
    pub battery_out_yesterday: Option<f64>,
    pub pv_inverter_daily: Option<Vec<f64>>,
    pub mppt_daily: Option<Vec<f64>>,
    pub pv_total_daily: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SolarForecast {
    pub date: Option<String>,
    pub generated_at: Option<String>,
    pub today_kwh: Option<f64>,
    pub tomorrow_kwh: Option<f64>,
}

struct AlertState {
    triggered: bool,
    last_alert: Option<std::time::Instant>,
    last_notified_value: Option<f64>,
}

impl AlertState {
    fn new() -> Self {
        Self {
            triggered: false,
            last_alert: None,
            last_notified_value: None,
        }
    }

    fn should_alert(&mut self) -> bool {
        match self.last_alert {
            None => {
                self.triggered = true;
                self.last_alert = Some(std::time::Instant::now());
                true
            }
            Some(last) => {
                if last.elapsed() > std::time::Duration::from_secs(NOTIFICATION_COOLDOWN_SECS) {
                    self.last_alert = Some(std::time::Instant::now());
                    true
                } else {
                    false
                }
            }
        }
    }

    fn should_alert_value(&mut self, value: f64) -> bool {
        match self.last_notified_value {
            None => {
                self.triggered = true;
                self.last_notified_value = Some(value);
                true
            }
            Some(prev) => {
                if (prev - value).abs() > f64::EPSILON {
                    self.triggered = true;
                    self.last_notified_value = Some(value);
                    true
                } else {
                    false
                }
            }
        }
    }

    fn check_resolved(&mut self) {
        if self.triggered {
            self.triggered = false;
            self.last_alert = None;
            self.last_notified_value = None;
        }
    }
}

struct NotificationState {
    high_consumption: AlertState,
    low_water: AlertState,
    high_solar: AlertState,
    high_load: std::collections::HashMap<String, AlertState>,
}

/// Last-good Cerbo GX EV sample per field with a throttle window.
///
/// dbus-ev / dbus-evcharger publish at the inverter-control poll cadence
/// (every 2 s). The desktop EV tile flickered because process_state_update
/// ran on every inverter/state message and, while building its merged
/// snapshot, cloned the state from *before* apply_ev_message landed. The
/// resulting write-back wiped the freshly-populated EV fields and the tile
/// toggled on/off in a 2 s loop. SoC never showed because inverter/state
/// published car_soc=0 when no car was connected, and merge_opt!(car_soc)
/// happily overwrote the real 0 with the daemon's 0 — wait, 0 is a
/// perfectly cromulent value. The real issue is that *missing* SoC
/// (inverter publishes 0 as "not connected") is indistinguishable from a
/// legitimate 0, so we treat 0 as no-data for SoC and refuse to clobber a
/// cached real value.
///
/// Throttle: per-field, ignore a new sample if the cached sample is younger
/// than 8 s. The cache survives process_state_update so the tile keeps
/// showing the last good value between Cerbo publishes.
#[derive(Default)]
struct EvCache {
    car_soc: Option<(f64, Instant)>,
    car_charging_power: Option<(f64, Instant)>,
    ev_charging_power: Option<(f64, Instant)>,
    /// Presence bits survive process_state_update so the EV tile never
    /// disappears on a daemon merge that wipes the EV metrics.
    ev_present: bool,
    evcharger_present: bool,
}

const EV_CACHE_TTL: Duration = Duration::from_secs(8);

impl EvCache {
    /// Apply a new Cerbo sample; returns true if the cache was updated.
    /// - car_soc: 0 is treated as no-data (refused if cache already populated).
    /// - power: 0 is a legitimate idle value, accepted.
    /// - throttle: reject a sample if the existing cache is younger than TTL.
    fn update(&mut self, field: EvField, value: f64) -> bool {
        let now = Instant::now();
        let slot = match field {
            EvField::CarSoc => {
                if value <= 0.0 && self.car_soc.is_some() {
                    return false;
                }
                &mut self.car_soc
            }
            EvField::CarChargingPower => &mut self.car_charging_power,
            EvField::EvChargingPower => &mut self.ev_charging_power,
        };
        if let Some((_, prev_ts)) = slot {
            if now.duration_since(*prev_ts) < EV_CACHE_TTL {
                return false;
            }
        }
        *slot = Some((value, now));
        true
    }

    /// Mark presence for ev/evcharger when a matching MQTT message arrives
    /// (including value 0). Presence survives process_state_update merges
    /// because daemon never publishes these fields.
    fn set_presence(&mut self, kind: &str) {
        match kind {
            "ev" => self.ev_present = true,
            "evcharger" => self.evcharger_present = true,
            _ => {}
        }
    }

    /// Re-apply the cached values AND presence bits to `st` after a
    /// daemon merge wiped them. Presence bits ensure the EV tile stays
    /// visible even when SOC/power are 0 or absent.
    ///
    /// Always overwrite: if the cache has a value (even 0), copy it onto st.
    /// Presence bits are sticky — never cleared to false.
    fn restore_into(&self, st: &mut InverterState) {
        st.ev_present = self.ev_present || st.ev_present;
        st.evcharger_present = self.evcharger_present || st.evcharger_present;
        // Always overwrite: cache value (even 0) takes precedence over None.
        if let Some((v, _)) = self.car_soc {
            st.car_soc = Some(v);
        }
        if let Some((v, _)) = self.car_charging_power {
            st.car_charging_power = Some(v);
        }
        if let Some((v, _)) = self.ev_charging_power {
            st.ev_charging_power = Some(v);
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
enum EvField {
    CarSoc,
    CarChargingPower,
    EvChargingPower,
}

pub struct MqttClient {
    client: Arc<Mutex<Option<Client>>>,
    client_id: String,
    pub(crate) state: Arc<Mutex<InverterState>>,
    host: String,
    port: u16,
    username: Option<String>,
    password: Option<String>,
    transport: rumqttc::Transport,
    app_handle: Option<SessionAppHandle>,
    /// Shared so runtime inverter/portal discovery updates W/ ack topics.
    portal_id: Arc<Mutex<Option<String>>>,
    /// Cerbo GX water instances: (tank, pump, valve). Any side may be None;
    /// discovery fills gaps (saved id kept if still present, else first found).
    water_instances: Option<(Option<u32>, Option<u32>, Option<u32>)>,
    /// Cerbo GX EV (vehicle) and evcharger instance pair for EV topics.
    /// Either side may be None — dbus-ev and dbus-evcharger are independent
    /// services, so the EV tile must populate if just one is configured.
    ev_instances: Option<(Option<u32>, Option<u32>)>,
    notifications: Arc<Mutex<NotificationState>>,
    alarms: Arc<Mutex<HashMap<String, u8>>>,
    /// Venus-platform notification slots (GUIv2 Notifications/[0-19]).
    platform_notifs: Arc<Mutex<HashMap<u32, PlatformNotifSlot>>>,
    /// Once any platform Notifications/* message arrives, raw Alarms/*
    /// banners are suppressed to avoid duplicate/generic "Dvcc alarm" text.
    platform_notifs_seen: Arc<std::sync::atomic::AtomicBool>,
    status_event: String,
    /// Throttled last-good EV sample cache (see EvCache docs). Wrapped in
    /// Mutex so the run_mqtt_loop closure can hold an Arc clone.
    ev_cache: Arc<Mutex<EvCache>>,
    /// Cleared by [`Self::stop`] so leaked reconnect loops from a replaced
    /// client exit instead of discovering the portal N more times.
    shutdown: Arc<Shutdown>,
    emitter: Arc<StateEmitter>,
}

/// Notification pushed by inverter-control on {prefix}/notifications.
#[derive(serde::Deserialize, serde::Serialize, Clone, Debug, PartialEq)]
pub struct MqttNotification {
    pub id: String,
    pub level: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub ts: String,
}

/// One Venus-platform notification slot (GUIv2 source of truth).
/// MQTT: N/<portal>/platform/<inst>/Notifications/<slot>/<field>
///
/// Venus publishes: Description, DeviceName, Service, DateTime, Type,
/// Active, Acknowledged, Silenced. No cell id / voltage / threshold —
/// those live on separate battery paths (System/MaxCellVoltage,
/// System/MaxVoltageCellId, Cell/*/Voltage) and are not in this payload.
#[derive(Debug, Clone, Default)]
pub(crate) struct PlatformNotifSlot {
    platform_instance: u32,
    slot: u32,
    description: Option<String>,
    device_name: Option<String>,
    service: Option<String>,
    /// Unix seconds when the notification was generated (GUIv2 DateTime).
    date_time: Option<i64>,
    /// 0=Warning, 1=Alarm, 2=Info
    notif_type: Option<i64>,
    active: Option<bool>,
    acknowledged: Option<bool>,
    silenced: Option<bool>,
    /// User tapped X in our UI. Sticky until Active goes false so MQTT
    /// re-publishes / cell-detail re-emits cannot resurrect the banner when
    /// Cerbo rejects or never receives the W/ Acknowledged write.
    user_dismissed: bool,
}

impl PlatformNotifSlot {
    fn apply_field(&mut self, field: &str, json: &serde_json::Value) {
        match field {
            "Description" => self.description = MqttClient::json_value_string(json),
            "DeviceName" => self.device_name = MqttClient::json_value_string(json),
            "Service" => self.service = MqttClient::json_value_string(json),
            "DateTime" => {
                let next = MqttClient::json_value_i64(json);
                // New event in a recycled slot — allow the banner again.
                if next.is_some() && next != self.date_time {
                    self.user_dismissed = false;
                }
                self.date_time = next;
            }
            "Type" => self.notif_type = MqttClient::json_value_i64(json),
            "Active" => {
                self.active = MqttClient::json_value_bool(json);
                // Condition cleared — next Active=true is a fresh alarm.
                if self.active == Some(false) {
                    self.user_dismissed = false;
                }
            }
            "Acknowledged" => self.acknowledged = MqttClient::json_value_bool(json),
            "Silenced" => self.silenced = MqttClient::json_value_bool(json),
            _ => {}
        }
    }

    /// A complete HTTP snapshot can arrive in any field order. Assemble the slot
    /// before rendering, so Acknowledged=true never flashes a historical alarm.
    pub(crate) fn from_gateway_fields(
        platform_instance: u32,
        slot: u32,
        fields: &HashMap<String, serde_json::Value>,
    ) -> Self {
        let mut result = Self {
            platform_instance,
            slot,
            ..Default::default()
        };
        for (field, value) in fields {
            result.apply_field(field, &serde_json::json!({ "value": value }));
        }
        result
    }

    fn banner_id(&self) -> String {
        format!("victron-platform-{}-{}", self.platform_instance, self.slot)
    }

    fn level(&self) -> &'static str {
        match self.notif_type.unwrap_or(1) {
            0 => "warning",
            2 => "info",
            _ => "alarm",
        }
    }

    fn should_show(&self) -> bool {
        // Hide after user dismiss or Cerbo ack; need a description to render.
        if self.user_dismissed || self.acknowledged.unwrap_or(false) {
            return false;
        }
        let desc = self.description.as_deref().unwrap_or("").trim();
        !desc.is_empty()
    }

    pub(crate) fn to_notification(&self) -> Option<MqttNotification> {
        if !self.should_show() {
            return None;
        }
        let title = self
            .description
            .as_deref()
            .unwrap_or("Alarm")
            .trim()
            .to_string();
        let body = self
            .device_name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| self.service.as_deref().filter(|s| !s.trim().is_empty()))
            .unwrap_or("")
            .trim()
            .to_string();
        let ts = self
            .date_time
            .and_then(|secs| Utc.timestamp_opt(secs, 0).single())
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| Utc::now().to_rfc3339());
        Some(MqttNotification {
            id: self.banner_id(),
            level: self.level().to_string(),
            title,
            body,
            source: "victron".to_string(),
            ts,
        })
    }
}

/// True when platform Description is a high/low (cell) voltage alarm we can
/// enrich from battery System/Max*|Min* MQTT paths.
fn voltage_cell_alarm_kind(description: &str) -> Option<&'static str> {
    let d = description.to_lowercase();
    let has_voltage = d.contains("voltage") || d.contains("cell");
    if !has_voltage {
        return None;
    }
    if d.contains("high") {
        Some("high")
    } else if d.contains("low") {
        Some("low")
    } else {
        None
    }
}

fn format_cell_detail(cell_id: Option<&str>, voltage: Option<f64>) -> Option<String> {
    match (cell_id.map(str::trim).filter(|s| !s.is_empty()), voltage) {
        (Some(id), Some(v)) => Some(format!("cell {id} · {v:.2}V")),
        (None, Some(v)) => Some(format!("{v:.2}V")),
        (Some(id), None) => Some(format!("cell {id}")),
        _ => None,
    }
}

fn battery_cell_detail_for_alarm(battery: &Battery, kind: &str) -> Option<String> {
    match kind {
        "high" => format_cell_detail(
            battery.max_voltage_cell_id.as_deref(),
            battery.max_cell_voltage,
        ),
        "low" => format_cell_detail(
            battery.min_voltage_cell_id.as_deref(),
            battery.min_cell_voltage,
        ),
        _ => None,
    }
}

fn find_battery_for_platform_notif<'a>(
    devices: &'a CerboDevices,
    slot: &PlatformNotifSlot,
) -> Option<&'a Battery> {
    if let Some(dn) = slot
        .device_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let dn_l = dn.to_lowercase();
        if let Some(b) = devices.batteries.values().find_map(|e| {
            e.data
                .name
                .as_deref()
                .filter(|n| n.to_lowercase() == dn_l)
                .map(|_| &e.data)
        }) {
            return Some(b);
        }
    }
    // Service may embed the MQTT instance id.
    if let Some(svc) = slot.service.as_deref() {
        for (inst, e) in &devices.batteries {
            if svc.contains(&inst.to_string()) {
                return Some(&e.data);
            }
        }
    }
    let with_cells: Vec<_> = devices
        .batteries
        .values()
        .filter(|e| {
            e.data.max_cell_voltage.is_some()
                || e.data.min_cell_voltage.is_some()
                || e.data.max_voltage_cell_id.is_some()
                || e.data.min_voltage_cell_id.is_some()
        })
        .collect();
    if with_cells.len() == 1 {
        return Some(&with_cells[0].data);
    }
    if devices.batteries.len() == 1 {
        return Some(&devices.batteries.values().next()?.data);
    }
    None
}

fn enrich_platform_notification_body(
    slot: &PlatformNotifSlot,
    mut n: MqttNotification,
    devices: &CerboDevices,
) -> MqttNotification {
    let Some(kind) = slot
        .description
        .as_deref()
        .and_then(voltage_cell_alarm_kind)
    else {
        return n;
    };
    let Some(battery) = find_battery_for_platform_notif(devices, slot) else {
        return n;
    };
    let Some(detail) = battery_cell_detail_for_alarm(battery, kind) else {
        return n;
    };
    if n.body.trim().is_empty() {
        n.body = detail;
    } else if !n.body.contains(&detail) {
        n.body = format!("{} · {}", n.body.trim(), detail);
    }
    n
}

/// Split CamelCase into words: "HighCellVoltage" -> ["High", "Cell", "Voltage"]
fn split_camel(s: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in s.chars() {
        if ch.is_uppercase() && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// "HighVoltage" -> "High voltage alarm"
fn pretty_alarm_name(alarm: &str) -> String {
    let mut words = split_camel(alarm);
    let mut out = words.first().cloned().unwrap_or_default();
    for w in words.drain(1..) {
        out.push(' ');
        out.push_str(&w.to_lowercase());
    }
    out.push_str(" alarm");
    out
}

use tauri_plugin_notification::NotificationExt;

const THRESHOLD_LOAD_W: f64 = 1500.0;
const THRESHOLD_CONSUMPTION_W: f64 = 1500.0;
const THRESHOLD_WATER_CM: f64 = 23.0;
const THRESHOLD_SOLAR_W: f64 = 3000.0;
const NOTIFICATION_COOLDOWN_SECS: u64 = 300;

fn fmt_watts(v: f64) -> String {
    if v >= 1000.0 {
        format!("{:.1}kW", v / 1000.0)
    } else {
        format!("{:.0}W", v)
    }
}

impl Drop for MqttClient {
    fn drop(&mut self) {
        self.stop();
    }
}

impl MqttClient {
    pub fn new(
        host: String,
        port: u16,
        username: Option<String>,
        password: Option<String>,
        client_id: String,
    ) -> Self {
        Self {
            client: Arc::new(Mutex::new(None)),
            client_id,
            state: Arc::new(Mutex::new(InverterState::default())),
            host,
            port,
            username,
            password,
            transport: rumqttc::Transport::tcp(),
            app_handle: None,
            portal_id: Arc::new(Mutex::new(None)),
            water_instances: None,
            ev_instances: None,
            notifications: Arc::new(Mutex::new(NotificationState {
                high_consumption: AlertState::new(),
                low_water: AlertState::new(),
                high_solar: AlertState::new(),
                high_load: std::collections::HashMap::new(),
            })),
            alarms: Arc::new(Mutex::new(HashMap::new())),
            platform_notifs: Arc::new(Mutex::new(HashMap::new())),
            platform_notifs_seen: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            status_event: "mqtt-connection-status".to_string(),
            ev_cache: Arc::new(Mutex::new(EvCache::default())),
            shutdown: Arc::new(Shutdown::new()),
            emitter: Arc::new(StateEmitter::new(true)),
        }
    }

    /// Stop the background reconnect loop and disconnect the broker client.
    /// Call before replacing this client in `connect_mqtt` so orphaned loops
    /// do not keep discovering the portal and fighting for messages.
    pub fn stop(&self) {
        self.shutdown.stop();
        self.emitter.stop();
        if let Ok(mut slot) = self.client.lock() {
            if let Some(client) = slot.take() {
                let _ = client.try_disconnect_now();
            }
        }
    }

    pub(crate) fn set_app_handle(
        &mut self,
        handle: tauri::AppHandle,
        notification_session: NotificationSession,
    ) {
        self.app_handle = Some(SessionAppHandle::new(handle, notification_session));
    }

    pub fn set_portal_id(&mut self, id: Option<String>) {
        if let Ok(mut g) = self.portal_id.lock() {
            *g = id;
        }
    }

    pub fn set_water_instances(
        &mut self,
        instances: Option<(Option<u32>, Option<u32>, Option<u32>)>,
    ) {
        self.water_instances = instances;
    }

    pub fn set_ev_instances(&mut self, instances: Option<(Option<u32>, Option<u32>)>) {
        self.ev_instances = instances;
    }

    pub fn get_state(&self) -> InverterState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// IPC hydration must identify the actual native producer, including the
    /// initial empty snapshot while its connection is still being established.
    pub(crate) fn get_scoped_state(&self) -> Result<serde_json::Value, String> {
        self.app_handle
            .as_ref()
            .ok_or_else(|| "MQTT notification session is not configured".to_string())?
            .bind(self.get_state())
            .map_err(|error| error.to_string())
    }

    pub fn emit_current_state(&self, force: bool) {
        self.emitter
            .emit(&self.app_handle, &self.get_state(), force);
    }

    /// Apply the inverter MQTT policy before starting any network operation.
    pub fn configure_transport(&mut self, tls: bool) -> Result<(), String> {
        self.transport =
            security::transport(tls, self.username.as_deref(), self.password.as_deref())?;
        Ok(())
    }

    pub fn connect(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.stop();
        self.shutdown = Arc::new(Shutdown::new());
        self.emitter = Arc::new(StateEmitter::new(
            self.status_event == "mqtt-connection-status",
        ));
        let host = self.host.clone();
        let port = self.port;
        let username = self.username.clone();
        let password = self.password.clone();
        let transport = self.transport.clone();
        let client_id = self.client_id.clone();

        let state = self.state.clone();
        let app_handle = self.app_handle.clone();
        let portal_id = self.portal_id.clone();
        let water_instances_owned = self.water_instances;
        let ev_instances_owned = self.ev_instances;
        let notifications = self.notifications.clone();
        let alarms = self.alarms.clone();
        let platform_notifs = self.platform_notifs.clone();
        let platform_notifs_seen = self.platform_notifs_seen.clone();
        let status_event = self.status_event.clone();
        let client_slot = self.client.clone();
        let ev_cache = self.ev_cache.clone();
        let shutdown = self.shutdown.clone();
        let emitter = self.emitter.clone();

        tauri::async_runtime::spawn(async move {
            let mut reconnect_attempt: u32 = 0;
            loop {
                if shutdown.is_stopped() {
                    log::info!("MQTT client stopped, exiting reconnect loop");
                    break;
                }
                // Log error separately so `result` drops before the await
                {
                    let loop_result = Self::run_mqtt_loop(
                        &host,
                        port,
                        &username,
                        &password,
                        transport.clone(),
                        &client_id,
                        state.clone(),
                        app_handle.clone(),
                        portal_id.clone(),
                        water_instances_owned,
                        ev_instances_owned,
                        notifications.clone(),
                        alarms.clone(),
                        platform_notifs.clone(),
                        platform_notifs_seen.clone(),
                        &status_event,
                        client_slot.clone(),
                        ev_cache.clone(),
                        shutdown.clone(),
                        emitter.clone(),
                    )
                    .await;
                    let is_err = loop_result.is_err();
                    let ever_connected = loop_result.ok() == Some(true);
                    if shutdown.is_stopped() {
                        log::info!("MQTT client stopped after disconnect");
                        break;
                    }
                    if ever_connected {
                        reconnect_attempt = 0;
                    }
                    if is_err {
                        log::error!("MQTT loop ended (err), will reconnect with backoff...");
                    } else {
                        log::info!("MQTT disconnected, will reconnect with backoff...");
                    }
                    // Connection lost or failed — clear the publish slot so
                    // publish_command reports the disconnect, then wait.
                    if let Ok(mut slot) = client_slot.lock() {
                        *slot = None;
                    }
                    if let Ok(mut state) = state.lock() {
                        state.ess_mode_observed_at = None;
                    }
                    if let Some(ref handle) = app_handle {
                        shutdown.while_running(|| {
                            let _ = handle.emit(&status_event, false);
                        });
                    }
                }
                let delay = mqtt_reconnect_delay_secs(reconnect_attempt);
                reconnect_attempt = reconnect_attempt.saturating_add(1);
                log::info!("MQTT reconnect in {delay}s (attempt backoff)");
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(delay)) => {},
                    _ = shutdown.cancelled() => break,
                }
            }
        });

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_mqtt_loop(
        host: &str,
        port: u16,
        username: &Option<String>,
        password: &Option<String>,
        transport: rumqttc::Transport,
        client_id: &str,
        state: Arc<Mutex<InverterState>>,
        app_handle: Option<SessionAppHandle>,
        portal_id: Arc<Mutex<Option<String>>>,
        water_instances: Option<(Option<u32>, Option<u32>, Option<u32>)>,
        ev_instances: Option<(Option<u32>, Option<u32>)>,
        notifications: Arc<Mutex<NotificationState>>,
        alarms: Arc<Mutex<HashMap<String, u8>>>,
        platform_notifs: Arc<Mutex<HashMap<u32, PlatformNotifSlot>>>,
        platform_notifs_seen: Arc<std::sync::atomic::AtomicBool>,
        status_event: &str,
        client_slot: Arc<Mutex<Option<Client>>>,
        ev_cache: Arc<Mutex<EvCache>>,
        shutdown: Arc<Shutdown>,
        emitter: Arc<StateEmitter>,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let connection_shutdown = Arc::new(Shutdown::new());
        let _connection_guard = CancelOnDrop(connection_shutdown.clone());
        let keepalive_secs = MQTT_KEEP_ALIVE_SECS;
        let queue_cap = MQTT_QUEUE_CAPACITY;

        // Random suffix keeps the client ID unique so a second instance or a
        // stale broker session cannot kick this client off the broker.
        let client_id = format!("{}-{:06x}", client_id, rand::random::<u32>() & 0xFF_FFFF);
        let mut mqttoptions = MqttOptions::new(&client_id, (host.to_string(), port));
        // A complete seasonal tariff can approach 100 KB, in addition to telemetry.
        mqttoptions.set_max_packet_size(1024 * 1024, 128 * 1024);
        mqttoptions.set_transport(transport);
        mqttoptions.set_keep_alive(keepalive_secs as u16);

        if let (Some(u), Some(p)) = (username, password) {
            if !u.is_empty() && !p.is_empty() {
                mqttoptions.set_credentials(u, p.clone());
            }
        }

        let (client, mut connection) = Client::builder(mqttoptions).capacity(queue_cap).build();

        // Store the connected client so publish_command can use it.
        if let Ok(mut slot) = client_slot.lock() {
            if shutdown.is_stopped() {
                return Ok(false);
            }
            *slot = Some(client.clone());
        }

        // Subscribe to topics using QoS 1 (AtLeastOnce)
        client.subscribe("inverter/state", QoS::AtLeastOnce)?;
        client.subscribe("inverter/setpoint_override", QoS::AtLeastOnce)?;
        client.subscribe("inverter/notifications", QoS::AtLeastOnce)?;
        // Portal ID advertised by inverter-control (retained) - lets the app
        // find the N/<portal>/... water/alarms topics with no manual config.
        client.subscribe("inverter/portal", QoS::AtLeastOnce)?;

        // Victron alarms + dbus-pump water topics for a configured portal
        let mut active_portal: Option<String> = None;
        if let Ok(guard) = portal_id.lock() {
            if let Some(id) = guard.as_deref().filter(|s| !s.is_empty()) {
                Self::subscribe_portal_topics(&client, id);
                Self::spawn_keepalive(
                    client.clone(),
                    id.to_string(),
                    shutdown.clone(),
                    connection_shutdown.clone(),
                );
                active_portal = Some(id.to_string());
            }
        }

        // NOTE: use tokio net (async) instead of blocking rumqttc sync iter.
        // Since rumqttc's AsyncClient/disconnection requires refactor, keep
        // spawn_blocking for backward compat but treat EOF as reconnect signal.
        let state_c = state.clone();
        let app_c = app_handle.clone();
        let water_c = water_instances;
        let ev_c = ev_instances;
        let notif_c = notifications.clone();
        let alarms_c = alarms.clone();
        let platform_notifs_c = platform_notifs.clone();
        let platform_notifs_seen_c = platform_notifs_seen.clone();
        let portal_id_c = portal_id.clone();
        let cerbo_devices: Arc<Mutex<CerboDevices>> = Arc::new(Mutex::new(CerboDevices::default()));
        let cerbo_c = cerbo_devices.clone();
        let ev_cache_c = ev_cache.clone();
        let se = status_event.to_string();
        let outer_shutdown = shutdown.clone();
        let con_result = tokio::task::spawn_blocking(move || {
            // Portal discovered at runtime via the retained inverter/portal
            // topic (inverter-control publishes it when no ID is configured).
            let mut ever_connected = false;
            for event in connection.iter() {
                if shutdown.is_stopped() {
                    break;
                }
                match event {
                    Ok(rumqttc::Event::Incoming(rumqttc::Packet::Publish(publish))) => {
                        // Use closures instead of closures capturing vars below
                        let topic = String::from_utf8_lossy(&publish.topic).to_string();
                        let payload = String::from_utf8(publish.payload.to_vec())
                            .unwrap_or_else(|_| String::new());

                        if topic == "inverter/portal" {
                            let id = payload.trim().to_string();
                            if !id.is_empty() && active_portal.as_deref() != Some(id.as_str()) {
                                log::info!("Discovered Cerbo portal ID {}", id);
                                active_portal = Some(id.clone());
                                if let Ok(mut g) = portal_id_c.lock() {
                                    *g = Some(id.clone());
                                }
                                Self::subscribe_portal_topics(&client, &id);
                                Self::spawn_keepalive(
                                    client.clone(),
                                    id,
                                    shutdown.clone(),
                                    connection_shutdown.clone(),
                                );
                            }
                            continue;
                        }

                        shutdown.while_running(|| {
                            Self::handle_message(
                                &topic,
                                &payload,
                                publish.retain,
                                &state_c,
                                &app_c,
                                &water_c,
                                &ev_c,
                                &notif_c,
                                &alarms_c,
                                &platform_notifs_c,
                                &platform_notifs_seen_c,
                                &cerbo_c,
                                &ev_cache_c,
                                &emitter,
                            )
                        });
                    }
                    Ok(rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(_))) => {
                        ever_connected = true;
                        if let Some(ref handle) = app_c {
                            shutdown.while_running(|| {
                                let _ = handle.emit(&se, true);
                            });
                        }
                    }
                    Ok(rumqttc::Event::Incoming(_)) => {}
                    Err(e) => {
                        log::error!("MQTT error: {:?}", e);
                        // Emit disconnect and return (exit for reconnect)
                        if let Some(ref handle) = app_c {
                            shutdown.while_running(|| {
                                let _ = handle.emit(&se, false);
                            });
                        }
                        return Err(e.into());
                    }
                    _ => {}
                }
            }
            Ok::<bool, Box<dyn std::error::Error + Send + Sync>>(ever_connected)
        })
        .await;

        let ever_connected = match con_result {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(e.into()),
        };

        // Connection ended cleanly (EOF) — signal reconnect
        if !outer_shutdown.is_stopped() {
            if let Some(ref handle) = app_handle {
                outer_shutdown.while_running(|| {
                    let _ = handle.emit(status_event, false);
                });
            }
        }
        Ok(ever_connected)
    }

    /// Subscribe the GX portal topics (alarms + dbus-pump water + active loads).
    ///
    /// Uses a single `subscribe_many` request so we never fill rumqttc's
    /// bounded request channel from inside `connection.iter()` (that deadlocks
    /// the event loop — see MQTT_QUEUE_CAPACITY).
    fn subscribe_portal_topics(client: &Client, id: &str) {
        let filters = [
            // Legacy dbus-mqtt: N/<portal>/<service>_<inst>/Alarms/<Name>
            format!("N/{}/+/Alarms/#", id),
            // Modern dbus-flashmq: N/<portal>/<service>/<inst>/Alarms/<Name>
            format!("N/{}/+/+/Alarms/#", id),
            // GUIv2 source of truth for alarm text/time/ack
            format!("N/{}/platform/+/Notifications/#", id),
            // Water + EV: wildcards cover Level/State/Mode/Soc/Ac/Power + CustomName/
            // ProductName so Config can list every instance the GX publishes.
            format!("N/{}/tank/+/#", id),
            format!("N/{}/pump/+/#", id),
            // dbus-ev uses bus name com.victronenergy.evcharger.<N> on some
            // installs, so Soc/Power may land under evcharger/<instance>/.
            format!("N/{}/ev/+/#", id),
            format!("N/{}/evcharger/+/#", id),
            // Active loads: Victron acload services (dbus-emporia-vue etc.).
            // Wildcard covers Ac/Power + CustomName + ProductName so names can
            // arrive after watts without a second subscribe burst.
            format!("N/{}/acload/+/#", id),
            // Directly discovered GX devices: battery bank(s) + MPPT chargers
            // + AC PV inverters of any vendor, so the app finds them even
            // when inverter-control is down.
            format!("N/{}/battery/+/#", id),
            format!("N/{}/solarcharger/+/#", id),
            format!("N/{}/pvinverter/+/#", id),
            // VE.Bus: ActiveIn grid fallback, Hub4 setpoint, /State.
            format!("N/{}/vebus/+/#", id),
            // systemcalc: Ac/Grid + Ac/Consumption (tt/t1/t2) without daemon.
            format!("N/{}/system/+/#", id),
        ];
        let n = filters.len();
        let topics: Vec<SubscribeFilter> = filters
            .into_iter()
            .map(|path| SubscribeFilter::new(path, QoS::AtLeastOnce))
            .collect();
        if let Err(e) = client.subscribe_many(topics) {
            log::warn!("Failed to subscribe portal topics for {}: {:?}", id, e);
        } else {
            log::info!("Subscribed to {n} Cerbo portal topic filters for {id}");
        }
    }

    /// Periodic R/<portal>/keepalive publisher so the Cerbo GX MQTT broker
    /// keeps accepting this client.
    fn spawn_keepalive(
        client: Client,
        id: String,
        session: Arc<Shutdown>,
        connection: Arc<Shutdown>,
    ) -> tauri::async_runtime::JoinHandle<()> {
        let topic = format!("R/{}/keepalive", id);
        tauri::async_runtime::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(KEEPALIVE_INTERVAL_SECS));
            loop {
                tokio::select! {
                    biased;
                    _ = session.cancelled() => break,
                    _ = connection.cancelled() => break,
                    _ = interval.tick() => {
                        // Never block a Tokio worker on a full sync-client queue.
                        if client.try_publish(&topic, QoS::AtMostOnce, false, "").is_err() {
                            break;
                        }
                    }
                }
            }
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_message(
        topic: &str,
        payload: &str,
        retained: bool,
        state: &Arc<Mutex<InverterState>>,
        app_handle: &Option<SessionAppHandle>,
        water_instances: &Option<(Option<u32>, Option<u32>, Option<u32>)>,
        ev_instances: &Option<(Option<u32>, Option<u32>)>,
        notifications: &Arc<Mutex<NotificationState>>,
        alarms: &Arc<Mutex<HashMap<String, u8>>>,
        platform_notifs: &Arc<Mutex<HashMap<u32, PlatformNotifSlot>>>,
        platform_notifs_seen: &Arc<std::sync::atomic::AtomicBool>,
        cerbo_devices: &Arc<Mutex<CerboDevices>>,
        ev_cache: &Arc<Mutex<EvCache>>,
        emitter: &Arc<StateEmitter>,
    ) {
        if topic == "inverter/state" {
            match serde_json::from_str::<RawInverterState>(payload) {
                Ok(mut raw) => {
                    raw.ess_mode_retained = retained;
                    raw.resolve_short_battery_keys();
                    Self::process_state_update(
                        raw,
                        state.clone(),
                        app_handle.clone(),
                        notifications.clone(),
                        Some(cerbo_devices.clone()),
                        ev_cache.clone(),
                        emitter,
                    );
                    // Log Cerbo-derived live tiles after merge, not raw daemon gt/tt/soc.
                    if let Ok(guard) = state.lock() {
                        note_inverter_state_recv(&guard);
                    }
                }
                Err(e) => {
                    log::warn!(
                        "Bad inverter/state payload ({} bytes): {}",
                        payload.len(),
                        e
                    );
                }
            }
        } else if topic == "inverter/setpoint_override" {
            match serde_json::from_str::<SetpointOverrideStatus>(payload) {
                Ok(status) => {
                    if let Ok(mut guard) = state.lock() {
                        guard.setpoint_override = Some(status.clone());
                    }
                    if let Some(handle) = app_handle {
                        let _ = handle.emit("setpoint-override-update", &status);
                    }
                }
                Err(error) => log::warn!("Invalid inverter-control override status: {error}"),
            }
        } else if topic == "inverter/notifications" {
            match serde_json::from_str::<MqttNotification>(payload) {
                Ok(mut notification) => {
                    // Ensure timestamp is present (add if missing)
                    if notification.ts.is_empty() {
                        notification.ts = Utc::now().to_rfc3339();
                    }
                    if let Some(ref handle) = app_handle {
                        let _ = handle.emit("mqtt-notification", &notification);
                        // Mirror to OS notification like local alerts
                        let _ = handle
                            .notification()
                            .builder()
                            .title(&notification.title)
                            .body(&notification.body)
                            .show();
                    }
                }
                Err(e) => log::warn!("Bad notification payload on {}: {}", topic, e),
            }
        } else if topic.starts_with("N/")
            && topic.contains("/platform/")
            && topic.contains("/Notifications/")
        {
            Self::handle_platform_notification_message(
                topic,
                payload,
                platform_notifs,
                platform_notifs_seen,
                cerbo_devices,
                app_handle,
            );
        } else if topic.starts_with("N/") && topic.contains("/Alarms/") {
            Self::handle_alarm_message(
                topic,
                payload,
                alarms,
                platform_notifs_seen,
                cerbo_devices,
                app_handle,
            );
        } else if topic.starts_with("N/") && Self::parse_device_topic(topic).is_some() {
            // Directly discovered GX device value (battery/solarcharger).
            if let Some((kind, inst, path)) = Self::parse_device_topic(topic) {
                let applied = cerbo_devices
                    .lock()
                    .ok()
                    .map(|mut d| {
                        d.sweep_stale();
                        Self::apply_device_message(&mut d, kind, inst, path, payload)
                    })
                    .unwrap_or(false);
                if applied {
                    let snapshot = {
                        let mut guard = match state.lock() {
                            Ok(g) => g,
                            Err(_) => return,
                        };
                        if let Ok(d) = cerbo_devices.lock() {
                            Self::apply_cerbo_to_state(&d, &mut guard);
                        }
                        guard.clone()
                    };
                    emitter.emit(app_handle, &snapshot, false);
                    if kind == "battery" && Self::is_battery_cell_path(path) {
                        Self::reemit_platform_notifications_with_cell_detail(
                            platform_notifs,
                            cerbo_devices,
                            app_handle,
                        );
                        Self::reemit_alarm_notifications_with_cell_detail(
                            alarms,
                            platform_notifs_seen,
                            cerbo_devices,
                            app_handle,
                        );
                    }
                }
            }
        } else if topic.starts_with("N/") && Self::parse_water_topic(topic).is_some() {
            // dbus-pump on the GX: N/<portal>/tank/<i>/Level|CustomName|ProductName,
            // N/<portal>/pump/<i>/State|Mode|CustomName|ProductName.
            // Always discover every instance; apply Level/State/Mode only for the
            // active (preferred-if-still-present, else first-found) selection.
            let Some((kind, inst, path)) = Self::parse_water_topic(topic) else {
                return;
            };
            let discovered = cerbo_devices
                .lock()
                .ok()
                .map(|mut d| {
                    d.sweep_stale();
                    Self::apply_named_discovery(&mut d, kind, inst, path, payload);
                    true
                })
                .unwrap_or(false);
            let mut applied_value = false;
            if matches!(path, "Level" | "State" | "Mode") {
                if let Some(value) = Self::parse_cerbo_value(payload) {
                    let snapshot_prep = {
                        let d = cerbo_devices.lock().ok();
                        let (pref_tank, pref_pump, pref_valve) = match water_instances {
                            Some((t, p, v)) => (*t, *p, *v),
                            None => (None, None, None),
                        };
                        let active_tank = d
                            .as_ref()
                            .map(|dev| Self::resolve_active_instance(pref_tank, &dev.tanks))
                            .unwrap_or(pref_tank);
                        let active_pump = d
                            .as_ref()
                            .map(|dev| Self::resolve_active_instance(pref_pump, &dev.pumps))
                            .unwrap_or(pref_pump);
                        let active_valve = d
                            .as_ref()
                            .map(|dev| {
                                // Prefer configured valve; else first pump that is not the active pump.
                                if let Some(v) = pref_valve {
                                    if dev.pumps.is_empty() || dev.pumps.contains_key(&v) {
                                        return Some(v);
                                    }
                                }
                                dev.pumps.keys().copied().find(|i| Some(*i) != active_pump)
                            })
                            .unwrap_or(pref_valve);
                        (active_tank, active_pump, active_valve)
                    };
                    let (active_tank, active_pump, active_valve) = snapshot_prep;
                    let mut guard = match state.lock() {
                        Ok(g) => g,
                        Err(_) => return,
                    };
                    match (kind, path, inst) {
                        ("tank", "Level", i) if Some(i) == active_tank => {
                            guard.water_level = Some(value);
                            applied_value = true;
                        }
                        ("pump", "State", i) if Some(i) == active_pump => {
                            guard.pump_switch = Some(value >= 0.5);
                            applied_value = true;
                        }
                        ("pump", "State", i) if Some(i) == active_valve => {
                            guard.water_valve = Some(value >= 0.5);
                            applied_value = true;
                        }
                        ("pump", "Mode", i) if Some(i) == active_pump => {
                            guard.water_pump_mode = Some(value as u8);
                            applied_value = true;
                        }
                        ("pump", "Mode", i) if Some(i) == active_valve => {
                            guard.water_valve_mode = Some(value as u8);
                            applied_value = true;
                        }
                        _ => {}
                    }
                }
            }
            if discovered || applied_value {
                let snapshot = {
                    let mut guard = match state.lock() {
                        Ok(g) => g,
                        Err(_) => return,
                    };
                    if let Ok(d) = cerbo_devices.lock() {
                        Self::apply_cerbo_to_state(&d, &mut guard);
                    }
                    guard.clone()
                };
                emitter.emit(app_handle, &snapshot, false);
            }
        } else if topic.starts_with("N/") && Self::parse_ev_topic(topic).is_some() {
            // dbus-ev / dbus-evcharger on the GX. Discover every instance;
            // apply Soc/Power only for the active selection (saved config if
            // still present among discovered, else first found).
            let Some((kind, inst, path)) = Self::parse_ev_topic(topic) else {
                return;
            };
            let _ = cerbo_devices.lock().ok().map(|mut d| {
                d.sweep_stale();
                Self::apply_named_discovery(&mut d, kind, inst, path, payload);
            });
            let mut applied = false;
            if matches!(path, "Soc" | "Ac/Power") {
                if let Some(value) = Self::parse_cerbo_value(payload) {
                    let effective = {
                        let d = cerbo_devices.lock().ok();
                        let ev_pref = ev_instances.as_ref().and_then(|(e, _)| *e);
                        let evc_pref = ev_instances.as_ref().and_then(|(_, c)| *c);
                        let active_ev = d
                            .as_ref()
                            .map(|dev| Self::resolve_active_instance(ev_pref, &dev.evs))
                            .unwrap_or(ev_pref);
                        let active_evc = d
                            .as_ref()
                            .map(|dev| Self::resolve_active_instance(evc_pref, &dev.evchargers))
                            .unwrap_or(evc_pref);
                        Some((active_ev, active_evc))
                    };
                    let (mut guard, mut cache) = match (state.lock(), ev_cache.lock()) {
                        (Ok(g), Ok(c)) => (g, c),
                        _ => return,
                    };
                    applied = Self::apply_ev_message(
                        &mut guard, &mut cache, kind, inst, path, value, &effective,
                    )
                    .is_some();
                }
            }
            // Always refresh discovered list onto state when we saw a topic.
            let snapshot = {
                let mut guard = match state.lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                if let Ok(d) = cerbo_devices.lock() {
                    Self::apply_cerbo_to_state(&d, &mut guard);
                }
                guard.clone()
            };
            if applied
                || snapshot
                    .discovered_water_ev
                    .as_ref()
                    .map(|v| !v.is_empty())
                    .unwrap_or(false)
            {
                emitter.emit(app_handle, &snapshot, false);
            }
        } else if topic.starts_with("N/") && Self::parse_acload_topic(topic).is_some() {
            // dbus-emporia-vue / Victron acload on the GX:
            // N/<portal>/acload/<i>/Ac/Power | CustomName | ProductName.
            // Cache under CerboDevices (stable instance key); overlay onto
            // state.loads / state.load_names so daemon merges cannot rename.
            let Some((inst, path)) = Self::parse_acload_topic(topic) else {
                return;
            };
            let applied = cerbo_devices
                .lock()
                .ok()
                .map(|mut d| {
                    d.sweep_stale();
                    Self::apply_acload_message(&mut d, inst, path, payload)
                })
                .unwrap_or(false);
            if applied {
                let snapshot = {
                    let mut guard = match state.lock() {
                        Ok(g) => g,
                        Err(_) => return,
                    };
                    if let Ok(d) = cerbo_devices.lock() {
                        Self::apply_cerbo_to_state(&d, &mut guard);
                    }
                    guard.clone()
                };
                emitter.emit(app_handle, &snapshot, false);
            }
        }
    }

    /// Parse N/<portal>/platform/<inst>/Notifications/<slot>/<Field>
    fn parse_platform_notif_topic(topic: &str) -> Option<(u32, u32, &str)> {
        let parts: Vec<&str> = topic.split('/').collect();
        // N / portal / platform / inst / Notifications / slot / Field
        if parts.len() < 7 {
            return None;
        }
        if parts.first() != Some(&"N") || parts.get(2) != Some(&"platform") {
            return None;
        }
        if parts.get(4) != Some(&"Notifications") {
            return None;
        }
        let inst: u32 = parts.get(3)?.parse().ok()?;
        let slot: u32 = parts.get(5)?.parse().ok()?;
        if slot > 20 {
            return None;
        }
        let field = *parts.get(6)?;
        Some((inst, slot, field))
    }

    fn json_value_bool(v: &serde_json::Value) -> Option<bool> {
        v.get("value").and_then(|x| {
            x.as_bool()
                .or_else(|| x.as_u64().map(|n| n != 0))
                .or_else(|| x.as_i64().map(|n| n != 0))
                .or_else(|| {
                    x.as_str().map(|s| {
                        let t = s.trim();
                        t == "1" || t.eq_ignore_ascii_case("true")
                    })
                })
        })
    }

    fn json_value_i64(v: &serde_json::Value) -> Option<i64> {
        v.get("value").and_then(|x| {
            x.as_i64()
                .or_else(|| x.as_u64().map(|n| n as i64))
                .or_else(|| x.as_f64().map(|n| n as i64))
                .or_else(|| x.as_str().and_then(|s| s.trim().parse().ok()))
        })
    }

    fn json_value_string(v: &serde_json::Value) -> Option<String> {
        v.get("value").and_then(|x| {
            if x.is_null() {
                return None;
            }
            if let Some(s) = x.as_str() {
                let t = s.trim();
                if t.is_empty() {
                    return None;
                }
                return Some(t.to_string());
            }
            Some(x.to_string().trim_matches('"').to_string())
        })
    }

    /// Venus-platform Notifications — same data GUIv2 uses for alarm text/time.
    fn handle_platform_notification_message(
        topic: &str,
        payload: &str,
        platform_notifs: &Arc<Mutex<HashMap<u32, PlatformNotifSlot>>>,
        platform_notifs_seen: &Arc<std::sync::atomic::AtomicBool>,
        cerbo_devices: &Arc<Mutex<CerboDevices>>,
        app_handle: &Option<SessionAppHandle>,
    ) {
        let Some((inst, slot, field)) = Self::parse_platform_notif_topic(topic) else {
            return;
        };
        platform_notifs_seen.store(true, std::sync::atomic::Ordering::Relaxed);

        let Ok(json) = serde_json::from_str::<serde_json::Value>(payload) else {
            return;
        };

        let notification = {
            let mut map = match platform_notifs.lock() {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
            let entry = map.entry(slot).or_insert_with(|| PlatformNotifSlot {
                platform_instance: inst,
                slot,
                ..Default::default()
            });
            entry.platform_instance = inst;
            entry.slot = slot;
            entry.apply_field(field, &json);
            entry.clone()
        };

        let Some(ref handle) = app_handle else {
            return;
        };
        if let Some(n) = notification.to_notification() {
            let n = if let Ok(devices) = cerbo_devices.lock() {
                enrich_platform_notification_body(&notification, n, &devices)
            } else {
                n
            };
            let _ = handle.emit("mqtt-notification", n);
        } else {
            let _ = handle.emit(
                "mqtt-notification-clear",
                serde_json::json!({ "id": notification.banner_id() }),
            );
        }
    }

    fn is_battery_cell_path(path: &str) -> bool {
        matches!(
            path,
            "System/MaxCellVoltage"
                | "System/MinCellVoltage"
                | "System/MaxVoltageCellId"
                | "System/MinVoltageCellId"
        )
    }

    /// Re-emit visible platform banners so High/Low voltage bodies pick up
    /// freshly arrived Max/Min cell fields from battery MQTT.
    fn reemit_platform_notifications_with_cell_detail(
        platform_notifs: &Arc<Mutex<HashMap<u32, PlatformNotifSlot>>>,
        cerbo_devices: &Arc<Mutex<CerboDevices>>,
        app_handle: &Option<SessionAppHandle>,
    ) {
        let Some(ref handle) = app_handle else {
            return;
        };
        let slots: Vec<PlatformNotifSlot> = match platform_notifs.lock() {
            Ok(g) => g.values().cloned().collect(),
            Err(e) => e.into_inner().values().cloned().collect(),
        };
        let devices = match cerbo_devices.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        for slot in slots {
            if let Some(n) = slot.to_notification() {
                let n = enrich_platform_notification_body(&slot, n, &devices);
                let _ = handle.emit("mqtt-notification", n);
            }
        }
    }

    /// Re-emit active Victron Alarms banners (fallback path) after cell fields update.
    fn reemit_alarm_notifications_with_cell_detail(
        alarms: &Arc<Mutex<HashMap<String, u8>>>,
        platform_notifs_seen: &Arc<std::sync::atomic::AtomicBool>,
        cerbo_devices: &Arc<Mutex<CerboDevices>>,
        app_handle: &Option<SessionAppHandle>,
    ) {
        if platform_notifs_seen.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let Some(ref handle) = app_handle else {
            return;
        };
        let active: Vec<(String, u8)> = match alarms.lock() {
            Ok(g) => g
                .iter()
                .filter(|(_, v)| **v == 1 || **v == 2)
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            Err(e) => e
                .into_inner()
                .iter()
                .filter(|(_, v)| **v == 1 || **v == 2)
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
        };
        for (topic, value) in active {
            let Some((service, instance, alarm_name)) = Self::parse_alarm_topic(&topic) else {
                continue;
            };
            if service != "battery" {
                continue;
            }
            let pretty = pretty_alarm_name(alarm_name);
            let title = pretty
                .strip_suffix(" alarm")
                .unwrap_or(pretty.as_str())
                .to_string();
            if voltage_cell_alarm_kind(&title).is_none() {
                continue;
            }
            let level = if value == 2 { "alarm" } else { "warning" };
            let device = Self::resolve_alarm_device_name(service, instance, cerbo_devices)
                .unwrap_or_else(|| match instance {
                    Some(i) => format!("{service} {i}"),
                    None => service.to_string(),
                });
            let body = Self::enrich_alarm_body_with_cell_detail(
                service,
                instance,
                &title,
                device,
                cerbo_devices,
            );
            let _ = handle.emit(
                "mqtt-notification",
                MqttNotification {
                    id: format!("victron-{topic}"),
                    level: level.to_string(),
                    title,
                    body,
                    source: "victron".to_string(),
                    ts: Utc::now().to_rfc3339(),
                },
            );
        }
    }

    /// Parse alarm topic in either legacy or modern form.
    /// Returns (service_type, instance_opt, alarm_name).
    fn parse_alarm_topic(topic: &str) -> Option<(&str, Option<u32>, &str)> {
        let parts: Vec<&str> = topic.split('/').collect();
        // Modern: N/<portal>/<service>/<inst>/Alarms/<Name>
        // Legacy: N/<portal>/<service>_<inst>/Alarms/<Name>
        if parts.len() < 5 || parts.first() != Some(&"N") {
            return None;
        }
        if parts.get(3) == Some(&"Alarms") {
            let service_inst = parts.get(2)?;
            let alarm_name = *parts.get(4)?;
            if let Some((svc, inst_s)) = service_inst.rsplit_once('_') {
                if let Ok(inst) = inst_s.parse::<u32>() {
                    return Some((svc, Some(inst), alarm_name));
                }
            }
            return Some((service_inst, None, alarm_name));
        }
        if parts.get(4) == Some(&"Alarms") {
            let service = *parts.get(2)?;
            let inst = parts.get(3)?.parse::<u32>().ok();
            let alarm_name = *parts.get(5)?;
            return Some((service, inst, alarm_name));
        }
        if let Some(pos) = parts.iter().position(|p| *p == "Alarms") {
            let alarm_name = *parts.get(pos + 1)?;
            let before = *parts.get(pos.checked_sub(1)?)?;
            if let Ok(inst) = before.parse::<u32>() {
                let service = *parts.get(pos.checked_sub(2)?)?;
                return Some((service, Some(inst), alarm_name));
            }
            return Some((before, None, alarm_name));
        }
        None
    }

    fn enrich_alarm_body_with_cell_detail(
        service: &str,
        instance: Option<u32>,
        title: &str,
        device: String,
        cerbo_devices: &Arc<Mutex<CerboDevices>>,
    ) -> String {
        if service != "battery" {
            return device;
        }
        let Some(kind) = voltage_cell_alarm_kind(title) else {
            return device;
        };
        let Some(inst) = instance else {
            return device;
        };
        let Ok(devices) = cerbo_devices.lock() else {
            return device;
        };
        let Some(detail) = devices
            .batteries
            .get(&inst)
            .and_then(|e| battery_cell_detail_for_alarm(&e.data, kind))
        else {
            return device;
        };
        if device.trim().is_empty() {
            detail
        } else {
            format!("{} · {}", device.trim(), detail)
        }
    }

    fn resolve_alarm_device_name(
        service: &str,
        instance: Option<u32>,
        cerbo_devices: &Arc<Mutex<CerboDevices>>,
    ) -> Option<String> {
        let Ok(devices) = cerbo_devices.lock() else {
            return None;
        };
        let inst = instance?;
        match service {
            "battery" => devices
                .batteries
                .get(&inst)
                .and_then(|e| e.data.display_name().map(|s| s.to_string())),
            "solarcharger" => devices
                .chargers
                .get(&inst)
                .and_then(|e| e.data.display_name().map(|s| s.to_string())),
            "pvinverter" => devices
                .pv_inverters
                .get(&inst)
                .and_then(|e| e.data.display_name().map(|s| s.to_string())),
            "acload" => devices
                .acloads
                .get(&inst)
                .and_then(|e| e.data.display_name().map(|s| s.to_string())),
            "vebus" => Some(format!("VE.Bus {inst}")),
            other => Some(format!("{other} {inst}")),
        }
    }

    /// Track a Victron alarm topic (fallback when platform Notifications unavailable).
    /// Value 0 clears the banner. Suppressed once platform notifications are seen.
    fn handle_alarm_message(
        topic: &str,
        payload: &str,
        alarms: &Arc<Mutex<HashMap<String, u8>>>,
        platform_notifs_seen: &Arc<std::sync::atomic::AtomicBool>,
        cerbo_devices: &Arc<Mutex<CerboDevices>>,
        app_handle: &Option<SessionAppHandle>,
    ) {
        if platform_notifs_seen.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }

        let value = serde_json::from_str::<serde_json::Value>(payload)
            .ok()
            .and_then(|v| v.get("value").and_then(|x| x.as_u64()))
            .unwrap_or(0) as u8;

        let prev = {
            let mut map = match alarms.lock() {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
            let entry = map.entry(topic.to_string()).or_insert(0);
            let prev = *entry;
            *entry = value;
            prev
        };

        if prev == value {
            return;
        }

        let (service, instance, alarm_name) = match Self::parse_alarm_topic(topic) {
            Some(t) => t,
            None => return,
        };
        let id = format!("victron-{}", topic);

        if let Some(ref handle) = app_handle {
            if value == 1 || value == 2 {
                let level = if value == 2 { "alarm" } else { "warning" };
                let pretty = pretty_alarm_name(alarm_name);
                let title = pretty
                    .strip_suffix(" alarm")
                    .unwrap_or(pretty.as_str())
                    .to_string();
                let device = Self::resolve_alarm_device_name(service, instance, cerbo_devices)
                    .unwrap_or_else(|| match instance {
                        Some(i) => format!("{service} {i}"),
                        None => service.to_string(),
                    });
                let body = Self::enrich_alarm_body_with_cell_detail(
                    service,
                    instance,
                    &title,
                    device,
                    cerbo_devices,
                );
                let _ = handle.emit(
                    "mqtt-notification",
                    MqttNotification {
                        id,
                        level: level.to_string(),
                        title,
                        body,
                        source: "victron".to_string(),
                        ts: Utc::now().to_rfc3339(),
                    },
                );
            } else {
                let _ = handle.emit("mqtt-notification-clear", serde_json::json!({ "id": id }));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn process_state_update(
        raw: RawInverterState,
        state: Arc<Mutex<InverterState>>,
        app_handle: Option<SessionAppHandle>,
        notifications: Arc<Mutex<NotificationState>>,
        cerbo_devices: Option<Arc<Mutex<CerboDevices>>>,
        ev_cache: Arc<Mutex<EvCache>>,
        emitter: &Arc<StateEmitter>,
    ) {
        // Non-destructive merge: start from existing state, update only fields
        // present in the incoming payload. Prevents transient `None` values
        // from wiping valid data during partial MQTT messages.
        let mut new_state = {
            let guard = state.lock().unwrap_or_else(|e| e.into_inner());
            guard.clone()
        };

        // Merge numeric scalars — keep existing if incoming is None
        macro_rules! merge_opt {
            ($field:ident, $val:expr) => {
                if $val.is_some() {
                    new_state.$field = $val;
                }
            };
        }

        // Snapshot which Cerbo maps already own live tiles so daemon
        // zeros/partials cannot clobber them (same pattern as EV / shunt).
        // gt/tt/soc are never taken from daemon — Cerbo apply_cerbo_to_state
        // owns those tiles (systemcalc + voltage_soc) whether or not owns_* yet.
        let cerbo_flags = cerbo_devices.as_ref().and_then(|c| {
            c.lock().ok().map(|d| {
                (
                    d.has_shunt(),
                    d.owns_vebus_mode(),
                    d.owns_chargers(),
                    d.owns_pv(),
                    d.owns_batteries(),
                    d.owns_solar(),
                    !d.acloads.is_empty(),
                )
            })
        });
        let (
            cerbo_has_shunt,
            cerbo_owns_vebus_mode,
            cerbo_owns_chargers,
            cerbo_owns_pv,
            cerbo_owns_batteries,
            cerbo_owns_solar,
            cerbo_has_acloads,
        ) = cerbo_flags.unwrap_or((false, false, false, false, false, false, false));

        // Never merge gt/g1/g2, tt/t1/t2, or battery_soc from daemon —
        // treat inverter-control as not providing them (Cerbo only).
        if !cerbo_owns_solar {
            merge_opt!(solar_total, raw.solar_total);
        }
        // Battery power/V/A: shunt-gated daemon fallback. SoC never from daemon.
        if !cerbo_has_shunt {
            merge_opt!(battery_power, raw.battery_power);
            merge_opt!(battery_voltage, raw.battery_voltage);
            merge_opt!(battery_current, raw.battery_current);
        }
        if !cerbo_owns_vebus_mode {
            merge_opt!(setpoint, raw.setpoint);
            merge_opt!(inverter_state, raw.inverter_state);
        }
        // The daemon identifies the explicitly selected backup. Ordinary
        // AC loads must never be inferred to be whole-house grid meters.
        if let Some(backup) = raw.grid_backup {
            if backup.is_none() {
                new_state.grid_using_backup = Some(false);
            }
            new_state.grid_backup = backup;
            new_state.grid_backup_observed_at = Some(Utc::now().timestamp_millis() as f64 / 1000.0);
        }
        merge_opt!(grid_using_backup, raw.grid_using_backup);
        merge_opt!(version, raw.version);
        merge_opt!(dashboard_version, raw.dashboard_version);
        merge_opt!(uptime, raw.uptime);
        merge_opt!(ha_connected, raw.ha_connected);
        merge_opt!(ha_direct_connected, raw.ha_direct_connected);
        if let Some(mode) = raw.ess_mode {
            new_state.ess_mode = Some(mode);
            new_state.ess_mode_observed_at =
                (!raw.ess_mode_retained).then(|| Utc::now().timestamp_millis() as f64 / 1000.0);
        }
        if !cerbo_owns_chargers {
            merge_opt!(mppt_individual, raw.mppt_individual);
        }
        merge_opt!(ui_config, raw.ui_config);
        merge_opt!(daily_stats, raw.daily_stats);
        merge_opt!(solar_forecast, raw.solar_forecast);
        // EV + water come ONLY from Cerbo MQTT handlers (apply_ev_message /
        // tank+pump). Do NOT merge daemon values — they overwrite live tiles.
        // Washer/dryer/dishwasher: UI reads HA entities only — skip daemon.
        merge_opt!(latest_version, raw.latest_version);

        // Bool coercions — keep existing if incoming is None
        if let Some(ref v) = raw.dry_run {
            new_state.dry_run = Some(coerce_bool(v));
        }

        // Map coercions
        if let Some(map) = raw.booleans {
            new_state.booleans = Some(
                map.into_iter()
                    .filter_map(|(key, value)| {
                        crate::inverter_control::control_bool(&value).map(|flag| {
                            let canonical = crate::inverter_control::flag_key(&key)
                                .unwrap_or(&key)
                                .to_owned();
                            (canonical, flag)
                        })
                    })
                    .collect(),
            );
        }
        merge_opt!(features, raw.features);

        // Collection fields — Cerbo device maps win when discovered.
        if !cerbo_owns_chargers {
            merge_opt!(mppt_chargers, raw.mppt_chargers);
        }
        if !cerbo_owns_pv {
            merge_opt!(pv_inverters, raw.pv_inverters);
            merge_opt!(pv_inverter_individual, raw.pv_inverter_individual);
        }
        if !cerbo_owns_batteries {
            merge_opt!(batteries, raw.batteries);
        }
        // Active loads: Cerbo acload services own the map (instance-keyed +
        // load_names). Daemon loads are often name-keyed and would replace the
        // Cerbo map on every inverter/state → UI flickers ids ↔ names.
        if !cerbo_has_acloads {
            merge_opt!(loads, raw.loads);
        }

        // mppt_total from daemon mppt_individual only when Cerbo has no chargers.
        // Otherwise apply_cerbo_to_state owns mppt_total / solar_total.
        if !cerbo_owns_chargers {
            new_state.mppt_total = new_state.mppt_individual.as_ref().map(|v| v.iter().sum());
        }

        // GX-discovered devices win over daemon arrays (see
        // apply_cerbo_to_state) so batteries/MPPTs survive daemon outages.
        if let Some(cerbo) = cerbo_devices.as_ref() {
            if let Ok(d) = cerbo.lock() {
                Self::apply_cerbo_to_state(&d, &mut new_state);
            }
        }

        // Skip alert/notification processing when window hidden (CPU/battery optimization)
        let hidden =
            crate::app_visibility::WINDOW_HIDDEN.load(std::sync::atomic::Ordering::Relaxed);

        if !hidden {
            let mut alert_notifications: Vec<(String, String)> = Vec::new();
            if let Ok(mut alert_state) = notifications.lock() {
                let mut active_loads = std::collections::HashSet::new();
                if let Some(ref loads) = new_state.loads {
                    for (name, power) in loads {
                        if *power > THRESHOLD_LOAD_W {
                            active_loads.insert(name.clone());
                            let alert = alert_state
                                .high_load
                                .entry(name.clone())
                                .or_insert_with(AlertState::new);
                            if alert.should_alert() {
                                let display_name = new_state
                                    .load_names
                                    .as_ref()
                                    .and_then(|m| m.get(name).cloned())
                                    .unwrap_or_else(|| name.clone());
                                let title = "High Load".to_string();
                                let body = format!("{}: {}", display_name, fmt_watts(*power));
                                alert_notifications.push((title.clone(), body.clone()));
                                // Also send as persistent banner
                                if let Some(ref handle) = app_handle {
                                    let alert_id = "high-load".to_string();
                                    let _ = handle.emit(
                                        "mqtt-notification",
                                        MqttNotification {
                                            id: alert_id,
                                            level: "alarm".to_string(),
                                            title: title.clone(),
                                            body: body.clone(),
                                            source: "system".to_string(),
                                            ts: Utc::now().to_rfc3339(),
                                        },
                                    );
                                }
                            }
                        }
                    }
                    alert_state
                        .high_load
                        .retain(|name, _| active_loads.contains(name));
                }

                if let Some(tt) = new_state.tt {
                    if tt > THRESHOLD_CONSUMPTION_W {
                        if alert_state.high_consumption.should_alert() {
                            let title = "High Consumption".to_string();
                            let body = format!("Consumption: {}", fmt_watts(tt));
                            alert_notifications.push((title.clone(), body.clone()));
                            // Also send as persistent banner
                            if let Some(ref handle) = app_handle {
                                let alert_id = "high-consumption".to_string();
                                let _ = handle.emit(
                                    "mqtt-notification",
                                    MqttNotification {
                                        id: alert_id,
                                        level: "alarm".to_string(),
                                        title: title.clone(),
                                        body: body.clone(),
                                        source: "system".to_string(),
                                        ts: Utc::now().to_rfc3339(),
                                    },
                                );
                            }
                        }
                    } else {
                        alert_state.high_consumption.check_resolved();
                    }
                }
                if let Some(wl) = new_state.water_level {
                    if wl < THRESHOLD_WATER_CM {
                        if alert_state.low_water.should_alert_value(wl) {
                            let title = "Low Water".to_string();
                            let body = format!("Water level: {} cm", wl);
                            alert_notifications.push((title.clone(), body.clone()));
                            // Also send as persistent banner
                            if let Some(ref handle) = app_handle {
                                let alert_id = "low-water".to_string();
                                let _ = handle.emit(
                                    "mqtt-notification",
                                    MqttNotification {
                                        id: alert_id,
                                        level: "alarm".to_string(),
                                        title: title.clone(),
                                        body: body.clone(),
                                        source: "system".to_string(),
                                        ts: Utc::now().to_rfc3339(),
                                    },
                                );
                            }
                        }
                    } else {
                        alert_state.low_water.check_resolved();
                    }
                }
                if let Some(st) = new_state.solar_total {
                    if st > THRESHOLD_SOLAR_W {
                        if alert_state.high_solar.should_alert() {
                            let title = "High Solar".to_string();
                            let body = format!("Solar: {}", fmt_watts(st));
                            alert_notifications.push((title.clone(), body.clone()));
                            // Also send as persistent banner
                            if let Some(ref handle) = app_handle {
                                let alert_id = "high-solar".to_string();
                                let _ = handle.emit(
                                    "mqtt-notification",
                                    MqttNotification {
                                        id: alert_id,
                                        level: "alarm".to_string(),
                                        title: title.clone(),
                                        body: body.clone(),
                                        source: "system".to_string(),
                                        ts: Utc::now().to_rfc3339(),
                                    },
                                );
                            }
                        }
                    } else {
                        alert_state.high_solar.check_resolved();
                    }
                }
            }

            if let Some(ref handle) = app_handle {
                for (title, body) in &alert_notifications {
                    let _ = handle
                        .notification()
                        .builder()
                        .title(title)
                        .body(body)
                        .show();
                    let _ = handle.emit(
                        "notification",
                        serde_json::json!({ "title": title, "body": body }),
                    );
                }
            }
        }

        // Restore EV fields/presence BEFORE emitting so the UI never sees
        // the pre-restore state (no flash/blink on HA poll interval).
        if let Ok(cache) = ev_cache.lock() {
            cache.restore_into(&mut new_state);
        }
        // restore_into ran above BEFORE emit so the emitted snapshot already
        // carries cached EV values (prevents the blink where the clone from
        // before apply_ev_message lands sees null EV numbers).
        // Persist under the mutex *before* emit so concurrent Cerbo handlers
        // (acload/device/EV) merge onto the latest daemon state.
        if let Ok(mut guard) = state.lock() {
            *guard = new_state.clone();
        }
        emitter.emit(&app_handle, &new_state, false);
    }

    /// Returns the current value of an inverter-control flag (true=on, false=off),
    /// or None when the flag has not been published yet.
    pub fn flag_state(&self, key: &str) -> Option<bool> {
        let state = self.state.lock().ok()?;
        let bools = state.booleans.as_ref()?;
        bools.get(key).copied()
    }

    /// Acknowledge + silence Venus-platform notifications on Cerbo (GUIv2 ecosystem).
    /// Locally sets `user_dismissed` so MQTT re-emits cannot resurrect the banner,
    /// then publishes per-slot Silenced/Acknowledged plus
    /// W/<portal>/platform/<inst>/Notifications/AcknowledgeAll (the MQTT write Venus
    /// actually honours — per-slot W/ alone is often ignored by dbus-flashmq).
    pub fn acknowledge_victron_notification(
        &self,
        platform_instance: u32,
        slot: u32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Local suppress first — UI must stay clear even if MQTT write fails
        // (disconnected client, Cerbo ignoring W/, gateway-only mode).
        if let Ok(mut map) = self.platform_notifs.lock() {
            let entry = map.entry(slot).or_insert_with(|| PlatformNotifSlot {
                platform_instance,
                slot,
                ..Default::default()
            });
            entry.platform_instance = platform_instance;
            entry.slot = slot;
            entry.user_dismissed = true;
            entry.acknowledged = Some(true);
            entry.silenced = Some(true);
        }
        if let Some(ref handle) = self.app_handle {
            let id = format!("victron-platform-{}-{}", platform_instance, slot);
            let _ = handle.emit("mqtt-notification-clear", serde_json::json!({ "id": id }));
        }

        let portal = {
            let guard = self
                .portal_id
                .lock()
                .map_err(|e| format!("Internal error: {e}"))?;
            guard.clone().filter(|s| !s.is_empty())
        };
        let Some(portal) = portal else {
            return Err("Cerbo portal ID not configured — cannot acknowledge on Venus".into());
        };
        let guard = self
            .client
            .lock()
            .map_err(|e| format!("Internal error: {}", e))?;
        let Some(client) = guard.as_ref() else {
            return Err("MQTT client not connected — cannot acknowledge on Venus".into());
        };
        let payload = r#"{"value":1}"#;
        // Per-slot Silenced/Acknowledged matches GUIv2 NotificationSlot::acknowledge,
        // but dbus-flashmq on current Venus often ignores those W/ paths. The path that
        // reliably updates Cerbo (and GUIv2) is AcknowledgeAll — same as Node-RED /
        // GUIv2 silence-all (verified live: UnAcknowledgedAlarms → 0).
        for field in ["Silenced", "Acknowledged"] {
            let topic = format!(
                "W/{}/platform/{}/Notifications/{}/{}",
                portal, platform_instance, slot, field
            );
            if let Err(e) = client.publish(&topic, QoS::AtLeastOnce, false, payload) {
                log::warn!("Cerbo per-slot ack publish {field} failed: {e}");
            }
        }
        let ack_all = format!(
            "W/{}/platform/{}/Notifications/AcknowledgeAll",
            portal, platform_instance
        );
        client
            .publish(ack_all, QoS::AtLeastOnce, false, payload)
            .map_err(|e| format!("Cerbo AcknowledgeAll publish failed: {e}"))?;
        Ok(())
    }

    /// Resolve pump/valve instance for a water Mode write.
    /// Config defaults: pump=1, valve=2 when instances are unset.
    fn resolve_water_mode_instance(&self, which: &str) -> Result<u32, String> {
        if which != "pump" && which != "valve" {
            return Err(format!("unknown water device '{which}'"));
        }
        let (pump_i, valve_i) = match self.water_instances {
            Some((_, p, v)) => (p.unwrap_or(1), v.unwrap_or(2)),
            None => (1, 2),
        };
        Ok(if which == "valve" { valve_i } else { pump_i })
    }

    /// Build GX MQTT-API write topic for dbus-pump /Mode.
    fn water_mode_write_topic(portal: &str, instance: u32) -> String {
        format!("W/{portal}/pump/{instance}/Mode")
    }

    /// Manual pump/valve override via GX MQTT-API:
    /// `W/<portal>/pump/<n>/Mode` with `{"value": <mode>}`
    /// (0 auto, 1 always-on, 2 always-off). Uses the live rumqttc client slot.
    pub fn set_water_mode(&self, which: &str, mode: u8) -> Result<(), String> {
        if mode > 2 {
            return Err(format!("invalid mode {mode}"));
        }
        let instance = self.resolve_water_mode_instance(which)?;
        let portal = {
            let guard = self
                .portal_id
                .lock()
                .map_err(|e| format!("Internal error: {e}"))?;
            guard.clone().filter(|s| !s.is_empty())
        };
        let Some(portal) = portal else {
            return Err("Cerbo portal ID not configured — cannot set water mode".into());
        };
        let guard = self
            .client
            .lock()
            .map_err(|e| format!("Internal error: {}", e))?;
        let Some(client) = guard.as_ref() else {
            return Err("MQTT client not connected — cannot set water mode".into());
        };
        let topic = Self::water_mode_write_topic(&portal, instance);
        let payload = serde_json::json!({ "value": mode }).to_string();
        client
            .publish(topic, QoS::AtLeastOnce, false, payload)
            .map_err(|e| format!("Cerbo water Mode publish failed: {e}"))?;
        Ok(())
    }

    pub fn setpoint_override_status(&self) -> Option<SetpointOverrideStatus> {
        self.state.lock().ok()?.setpoint_override.clone()
    }

    /// A single explicit command; inverter-control owns all periodic writes.
    pub fn request_setpoint_override(
        &self,
        value: Option<i32>,
        request_id: &str,
    ) -> Result<(), String> {
        if self.setpoint_override_status().is_none() {
            return Err("Waiting for inverter-control override support on Cerbo".into());
        }
        let slot = self.client.lock().map_err(|e| e.to_string())?;
        let client = slot.as_ref().ok_or("Cerbo MQTT is not connected")?;
        client
            .try_publish(
                "inverter/cmd/setpoint_override",
                QoS::AtMostOnce,
                false,
                serde_json::json!({"value": value, "request_id": request_id}).to_string(),
            )
            .map_err(|e| format!("Could not send setpoint override: {e}"))
    }

    pub fn publish_command(
        &self,
        action: &str,
        mut payload: serde_json::Value,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if action == "set_ess_mode" {
            crate::inverter_control::validate_ess_selection(&payload)?;
            let state = self
                .state
                .lock()
                .map_err(|e| format!("Internal error: {e}"))?;
            let now = Utc::now().timestamp_millis() as f64 / 1000.0;
            if state
                .ess_mode
                .as_ref()
                .and_then(|mode| mode.selection_supported)
                != Some(true)
                || !state
                    .ess_mode_observed_at
                    .is_some_and(|at| (0.0..=30.0).contains(&(now - at)))
            {
                return Err("Update inverter-control or wait for fresh ESS telemetry".into());
            }
        }
        crate::inverter_control::prepare_command(action, &mut payload, |key| self.flag_state(key));
        let guard = self
            .client
            .lock()
            .map_err(|e| format!("Internal error: {}", e))?;
        let client = guard.as_ref().ok_or("MQTT client not connected")?;
        let topic = format!("inverter/cmd/{}", action);
        let payload_str = if payload.is_null() {
            String::new()
        } else {
            serde_json::to_string(&payload)?
        };
        let qos = if action == "set_ess_mode" {
            QoS::AtMostOnce
        } else {
            QoS::AtLeastOnce
        };
        if action == "set_ess_mode" {
            client.try_publish(topic, qos, false, payload_str)?;
        } else {
            client.publish(topic, qos, false, payload_str)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ess_selection_rejects_unsupported_stale_and_forged_telemetry() {
        let client = MqttClient::new("localhost".into(), 1883, None, None, "ess-test".into());
        let body = serde_json::json!({"mode":"off","request_id":"a"});
        for age in [None, Some(31.0), Some(-5.0)] {
            let mut state = client.state.lock().unwrap();
            state.ess_mode = Some(EssMode {
                selection_supported: Some(true),
                ..Default::default()
            });
            state.ess_mode_observed_at =
                age.map(|age| Utc::now().timestamp_millis() as f64 / 1000.0 - age);
            drop(state);
            assert!(client
                .publish_command("set_ess_mode", body.clone())
                .unwrap_err()
                .to_string()
                .contains("fresh ESS"));
        }
        let mut forged_json = serde_json::to_value(InverterState::default()).unwrap();
        forged_json["ess_mode_observed_at"] = serde_json::json!(99999999999.0);
        let forged: InverterState = serde_json::from_value(forged_json).unwrap();
        assert!(forged.ess_mode_observed_at.is_none());
        assert!(client
            .publish_command("set_ess_mode", serde_json::json!({}))
            .unwrap_err()
            .to_string()
            .contains("Invalid ESS"));
    }

    #[test]
    fn retained_ess_status_is_display_only_and_cannot_enable_commands() {
        let client = MqttClient::new("localhost".into(), 1883, None, None, "ess-test".into());
        for retained in [false, true] {
            MqttClient::process_state_update(
                RawInverterState {
                    ess_mode: Some(EssMode {
                        selected: Some("external_control".into()),
                        selection_supported: Some(true),
                        ..Default::default()
                    }),
                    ess_mode_retained: retained,
                    ..Default::default()
                },
                client.state.clone(),
                None,
                empty_notifications(),
                None,
                Arc::new(Mutex::new(EvCache::default())),
                &Arc::new(StateEmitter::new(true)),
            );
            let state = client.state.lock().unwrap();
            assert_eq!(state.ess_mode_observed_at.is_some(), !retained);
            assert_eq!(
                state.ess_mode.as_ref().unwrap().selected.as_deref(),
                Some("external_control")
            );
        }
    }

    #[test]
    fn grid_daily_energy_survives_mqtt_and_gateway_and_clears_with_old_snapshots() {
        let client = MqttClient::new("localhost".into(), 1883, None, None, "grid-day-test".into());
        let energy = serde_json::json!({
            "date":"2026-09-29", "time_zone":"America/Los_Angeles",
            "import_kwh":12.34, "export_kwh":0.0,
            "observed_at":1790697600.0, "started_at":1790665200.0,
            "complete":true, "status":"complete", "reason":null,
            "source":{"service":"com.victronenergy.grid.example",
                "device_instance":40, "serial":"TEST-METER"}
        });
        // Invalid optional telemetry is assessed by the frontend; it must not
        // discard unrelated solar/battery statistics during native decoding.
        for value in [Some(energy), Some(serde_json::json!("invalid")), None] {
            let mut daily = serde_json::json!({"produced_today":7.5, "grid_kwh":null});
            if let Some(value) = &value {
                daily["grid_energy"] = value.clone();
            }
            MqttClient::process_state_update(
                serde_json::from_value(serde_json::json!({"daily_stats":daily})).unwrap(),
                client.state.clone(),
                None,
                Arc::new(Mutex::new(NotificationState {
                    high_consumption: AlertState::new(),
                    low_water: AlertState::new(),
                    high_solar: AlertState::new(),
                    high_load: HashMap::new(),
                })),
                None,
                Arc::new(Mutex::new(EvCache::default())),
                &Arc::new(StateEmitter::new(false)),
            );
            let snapshot = serde_json::from_value(serde_json::json!({
                "inverter":{"daily_stats":daily}
            }))
            .unwrap();
            let gateway = crate::gateway::snapshot_to_state(&snapshot);
            for state in [client.get_state(), gateway] {
                let stats = state.daily_stats.unwrap();
                assert_eq!(stats.produced_today, Some(7.5));
                assert_eq!(stats.grid_energy, value);
                let ipc = serde_json::to_value(stats).unwrap();
                assert_eq!(ipc["grid_energy"], value.clone().unwrap_or_default());
            }
        }
    }

    #[test]
    fn selected_backup_survives_cerbo_overlay_and_clears_unavailable_power() {
        let client = MqttClient::new("localhost".into(), 1883, None, None, "backup-test".into());
        client.state.lock().unwrap().gt = Some(120.0);
        let apply = |payload: serde_json::Value| {
            MqttClient::process_state_update(
                serde_json::from_value(payload).unwrap(),
                client.state.clone(),
                None,
                Arc::new(Mutex::new(NotificationState {
                    high_consumption: AlertState::new(),
                    low_water: AlertState::new(),
                    high_solar: AlertState::new(),
                    high_load: HashMap::new(),
                })),
                None,
                Arc::new(Mutex::new(EvCache::default())),
                &Arc::new(StateEmitter::new(false)),
            );
        };
        let mut backup = serde_json::json!({"enabled":true,"available":true,
            "service":"com.victronenergy.acload.example","device_instance":78,
            "name":"Home","power":-750.0,"measurement_time":1000.0,"age_seconds":1.0});
        apply(serde_json::json!({"grid_backup":backup,"grid_using_backup":false}));
        assert_eq!(client.get_state().grid_backup.unwrap().power, Some(-750.0));
        assert_eq!(client.get_state().gt, Some(120.0));
        let observed = client.get_state().grid_backup_observed_at;
        apply(serde_json::json!({"uptime":20}));
        assert_eq!(client.get_state().grid_backup_observed_at, observed);
        assert_eq!(
            client.get_state().grid_backup.unwrap().name.as_deref(),
            Some("Home")
        );
        backup["power"] = serde_json::Value::Null;
        backup["available"] = false.into();
        apply(serde_json::json!({"grid_backup":backup,"grid_using_backup":false}));
        assert_eq!(client.get_state().grid_backup.unwrap().power, None);
        backup["service"] = serde_json::Value::Null;
        apply(serde_json::json!({"grid_backup":backup}));
        assert_eq!(client.get_state().grid_backup.unwrap().service, None);
        apply(serde_json::json!({"grid_backup":null}));
        assert!(client.get_state().grid_backup.is_none());
        assert_eq!(client.get_state().grid_using_backup, Some(false));
    }

    #[test]
    fn override_requires_daemon_support_and_connected_mqtt() {
        let client = MqttClient::new("localhost".into(), 1883, None, None, "override-test".into());
        assert!(client
            .request_setpoint_override(Some(-10), "id")
            .unwrap_err()
            .contains("support"));
        client.state.lock().unwrap().setpoint_override = Some(SetpointOverrideStatus::default());
        assert!(client
            .request_setpoint_override(Some(-10), "id")
            .unwrap_err()
            .contains("not connected"));
    }

    #[test]
    fn raw_inverter_state_accepts_short_battery_aliases() {
        let mut raw: RawInverterState =
            serde_json::from_str(r#"{"gt":1.0,"bp":1200.0,"bv":52.4,"bc":-23.1}"#).expect("parse");
        raw.resolve_short_battery_keys();
        assert_eq!(raw.battery_power, Some(1200.0));
        assert_eq!(raw.battery_voltage, Some(52.4));
        assert_eq!(raw.battery_current, Some(-23.1));
    }

    /// Regression: daemon JSON includes BOTH canonical and short battery keys.
    /// serde `alias` treated them as one field and rejected the whole payload
    /// ("duplicate field `battery_power`"), zeroing Consumption/Setpoint.
    #[test]
    fn raw_inverter_state_accepts_canonical_and_short_battery_keys_together() {
        let json = r#"{
            "gt": 100.0,
            "tt": 2500.0,
            "setpoint": -500.0,
            "battery_power": 1800.5,
            "bp": 999.0,
            "battery_voltage": 53.2,
            "bv": 40.0,
            "battery_current": -12.5,
            "bc": 0.0,
            "battery_soc": 88.0
        }"#;
        let mut raw: RawInverterState = serde_json::from_str(json).expect("parse both keys");
        raw.resolve_short_battery_keys();
        // Prefer canonical when both present.
        assert_eq!(raw.battery_power, Some(1800.5));
        assert_eq!(raw.battery_voltage, Some(53.2));
        assert_eq!(raw.battery_current, Some(-12.5));
        assert_eq!(raw.gt, Some(100.0));
        assert_eq!(raw.tt, Some(2500.0));
        assert_eq!(raw.setpoint, Some(-500.0));
        assert_eq!(raw.battery_soc, Some(88.0));
    }

    /// Guardrail: portal subscribe burst must fit the rumqttc request channel
    /// even if someone reverts subscribe_many back to per-filter subscribe.
    /// Regression: 3512a15 added acload as the 11th filter while capacity was
    /// 10, deadlocking the MQTT thread inside the portal discovery handler.
    #[test]
    fn portal_topic_filter_count_fits_mqtt_queue_capacity() {
        // Keep in sync with subscribe_portal_topics filter list.
        const PORTAL_FILTER_COUNT: usize = 15;
        const {
            assert!(PORTAL_FILTER_COUNT < MQTT_QUEUE_CAPACITY);
        }
    }

    #[test]
    fn parse_platform_notif_topic_extracts_slot_and_field() {
        assert_eq!(
            MqttClient::parse_platform_notif_topic(
                "N/b827eb123/platform/0/Notifications/3/Description"
            ),
            Some((0, 3, "Description"))
        );
        assert_eq!(
            MqttClient::parse_platform_notif_topic(
                "N/b827eb123/platform/0/Notifications/AcknowledgeAll"
            ),
            None
        );
    }

    #[test]
    fn parse_alarm_topic_modern_and_legacy() {
        assert_eq!(
            MqttClient::parse_alarm_topic("N/portal/battery/288/Alarms/HighVoltage"),
            Some(("battery", Some(288), "HighVoltage"))
        );
        assert_eq!(
            MqttClient::parse_alarm_topic("N/portal/battery_288/Alarms/HighVoltage"),
            Some(("battery", Some(288), "HighVoltage"))
        );
        assert_eq!(
            MqttClient::parse_alarm_topic("N/portal/system/0/Alarms/Dvcc"),
            Some(("system", Some(0), "Dvcc"))
        );
    }

    #[test]
    fn platform_slot_banner_uses_description_device_and_datetime() {
        let slot = PlatformNotifSlot {
            platform_instance: 0,
            slot: 2,
            description: Some("High voltage".into()),
            device_name: Some("JBD Battery Chain 1".into()),
            service: Some("com.victronenergy.battery.ttyUSB0".into()),
            date_time: Some(1_700_000_000),
            notif_type: Some(1),
            active: Some(true),
            acknowledged: Some(false),
            silenced: Some(false),
            user_dismissed: false,
        };
        let n = slot.to_notification().expect("show");
        assert_eq!(n.title, "High voltage");
        assert_eq!(n.body, "JBD Battery Chain 1");
        assert_eq!(n.level, "alarm");
        assert!(n.ts.contains("2023-"), "unexpected ts {}", n.ts);
        assert_eq!(n.id, "victron-platform-0-2");
    }

    #[test]
    fn platform_slot_hides_when_acknowledged() {
        let mut slot = PlatformNotifSlot {
            platform_instance: 0,
            slot: 1,
            description: Some("High voltage".into()),
            device_name: Some("Batt".into()),
            notif_type: Some(1),
            active: Some(true),
            acknowledged: Some(true),
            ..Default::default()
        };
        assert!(slot.to_notification().is_none());
        slot.acknowledged = Some(false);
        assert!(slot.to_notification().is_some());
    }

    #[test]
    fn platform_slot_hides_when_user_dismissed_even_if_unacked() {
        let slot = PlatformNotifSlot {
            platform_instance: 0,
            slot: 1,
            description: Some("High voltage".into()),
            acknowledged: Some(false),
            user_dismissed: true,
            ..Default::default()
        };
        assert!(slot.to_notification().is_none());
    }

    #[test]
    fn platform_banner_appends_cell_detail_from_battery_mqtt() {
        let mut d = CerboDevices::default();
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            288,
            "ProductName",
            "{\"value\": \"JBD Battery Chain 1\"}",
        ));
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            288,
            "System/MaxCellVoltage",
            "{\"value\": 3.62}",
        ));
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            288,
            "System/MaxVoltageCellId",
            "{\"value\": \"7\"}",
        ));
        let slot = PlatformNotifSlot {
            platform_instance: 0,
            slot: 2,
            description: Some("High voltage".into()),
            device_name: Some("JBD Battery Chain 1".into()),
            service: Some("com.victronenergy.battery.ttyUSB0".into()),
            date_time: Some(1_700_000_000),
            notif_type: Some(1),
            active: Some(true),
            acknowledged: Some(false),
            silenced: Some(false),
            user_dismissed: false,
        };
        let n = slot.to_notification().expect("show");
        let n = enrich_platform_notification_body(&slot, n, &d);
        assert_eq!(n.title, "High voltage");
        assert_eq!(n.body, "JBD Battery Chain 1 · cell 7 · 3.62V");
    }

    #[test]
    fn platform_banner_low_voltage_uses_min_cell_fields() {
        let mut d = CerboDevices::default();
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            1,
            "ProductName",
            "{\"value\": \"Pack\"}",
        ));
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            1,
            "System/MinCellVoltage",
            "{\"value\": 2.91}",
        ));
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            1,
            "System/MinVoltageCellId",
            "{\"value\": 3}",
        ));
        let slot = PlatformNotifSlot {
            platform_instance: 0,
            slot: 4,
            description: Some("Low cell voltage".into()),
            device_name: Some("Pack".into()),
            notif_type: Some(1),
            acknowledged: Some(false),
            ..Default::default()
        };
        let n = enrich_platform_notification_body(&slot, slot.to_notification().expect("show"), &d);
        assert_eq!(n.body, "Pack · cell 3 · 2.91V");
    }

    #[test]
    fn platform_banner_high_cell_voltage_description_enriches() {
        let mut d = CerboDevices::default();
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            288,
            "CustomName",
            "{\"value\": \"JBD Battery Chain 1\"}",
        ));
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            288,
            "System/MaxCellVoltage",
            "{\"value\": 3.62}",
        ));
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            288,
            "System/MaxVoltageCellId",
            "{\"value\": 7}",
        ));
        let slot = PlatformNotifSlot {
            platform_instance: 0,
            slot: 2,
            description: Some("HighCellVoltage".into()),
            device_name: Some("JBD Battery Chain 1".into()),
            notif_type: Some(1),
            acknowledged: Some(false),
            ..Default::default()
        };
        let n = enrich_platform_notification_body(&slot, slot.to_notification().expect("show"), &d);
        assert_eq!(n.body, "JBD Battery Chain 1 · cell 7 · 3.62V");
    }

    #[test]
    fn enrich_alarm_body_appends_max_cell_detail() {
        let mut d = CerboDevices::default();
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            288,
            "ProductName",
            "{\"value\": \"JBD Battery Chain 1\"}",
        ));
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            288,
            "System/MaxCellVoltage",
            "{\"value\": 3.62}",
        ));
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            288,
            "System/MaxVoltageCellId",
            "{\"value\": 7}",
        ));
        let devices = Arc::new(Mutex::new(d));
        let body = MqttClient::enrich_alarm_body_with_cell_detail(
            "battery",
            Some(288),
            "High voltage",
            "JBD Battery Chain 1".into(),
            &devices,
        );
        assert_eq!(body, "JBD Battery Chain 1 · cell 7 · 3.62V");
    }

    #[test]
    fn pretty_alarm_name_high_voltage() {
        assert_eq!(pretty_alarm_name("HighVoltage"), "High voltage alarm");
        assert_eq!(pretty_alarm_name("Dvcc"), "Dvcc alarm");
    }

    #[test]
    fn water_mode_write_topic_matches_gx_mqtt_api() {
        assert_eq!(
            MqttClient::water_mode_write_topic("portal42", 1),
            "W/portal42/pump/1/Mode"
        );
        assert_eq!(
            MqttClient::water_mode_write_topic("abc", 2),
            "W/abc/pump/2/Mode"
        );
    }

    #[test]
    fn resolve_water_mode_instance_uses_config_defaults() {
        let mut client = MqttClient::new("localhost".into(), 1883, None, None, "test".into());
        assert_eq!(client.resolve_water_mode_instance("pump").unwrap(), 1);
        assert_eq!(client.resolve_water_mode_instance("valve").unwrap(), 2);
        client.set_water_instances(Some((Some(21), Some(7), Some(9))));
        assert_eq!(client.resolve_water_mode_instance("pump").unwrap(), 7);
        assert_eq!(client.resolve_water_mode_instance("valve").unwrap(), 9);
        assert!(client.resolve_water_mode_instance("tank").is_err());
    }

    #[test]
    fn voltage_soc_matches_ha_battery_percent() {
        // Parity with the HA template: linear 40-54.4 V, clamp 0-100, round.
        assert_eq!(voltage_soc(40.0), 0.0);
        assert_eq!(voltage_soc(54.4), 100.0);
        assert_eq!(voltage_soc(47.2), 50.0);
        assert_eq!(voltage_soc(30.0), 0.0); // below range clamps to 0
        assert_eq!(voltage_soc(60.0), 100.0); // above range clamps to 100
        assert_eq!(voltage_soc(51.2), 78.0); // whole numbers only
    }

    #[test]
    fn partial_device_serializes_without_nulls() {
        // The UI guards optional battery fields with `!== undefined`; a JSON
        // `null` passes that check and crashes rendering (null.toFixed).
        // Partially-discovered GX devices must omit the fields instead.
        assert_eq!(serde_json::to_string(&Battery::default()).unwrap(), "{}");
        assert_eq!(
            serde_json::to_string(&MpptCharger::default()).unwrap(),
            "{}"
        );
        assert_eq!(serde_json::to_string(&PvInverter::default()).unwrap(), "{}");
        // Fully-populated values still serialize.
        let b = Battery {
            soc: Some(87.5),
            ..Default::default()
        };
        assert_eq!(serde_json::to_string(&b).unwrap(), r#"{"soc":87.5}"#);
    }

    #[test]
    fn daemon_header_control_preserves_state_key_in_frontend_payload() {
        let control = serde_json::json!({
            "id": "charging", "label": "Charging", "entity": "only_charging",
            "state_key": "only_charging"
        });
        let raw: RawInverterState = serde_json::from_value(serde_json::json!({
            "ui_config": {"header_toggles": [control]}
        }))
        .unwrap();
        let header = raw.ui_config.unwrap().header_toggles.unwrap();
        assert_eq!(header[0].state_key.as_deref(), Some("only_charging"));
        assert_eq!(serde_json::to_value(&header[0]).unwrap(), control);
    }

    #[test]
    fn flag_state_returns_current_value() {
        let client = MqttClient::new("localhost".into(), 1883, None, None, "test".into());
        // No booleans yet → None.
        assert_eq!(client.flag_state("only_charging"), None);
        {
            let mut st = client.state.lock().unwrap();
            st.booleans = Some(HashMap::from([("only_charging".into(), true)]));
        }
        assert_eq!(client.flag_state("only_charging"), Some(true));
        {
            let mut st = client.state.lock().unwrap();
            st.booleans = Some(HashMap::from([("only_charging".into(), false)]));
        }
        assert_eq!(client.flag_state("only_charging"), Some(false));
        assert_eq!(client.flag_state("never_set"), None);
    }

    #[test]
    fn publish_command_errors_when_client_is_none() {
        // Regression: clicks used to silently return Ok(()) when the client
        // was dropped during reconnect, leaving the UI with no feedback.
        let client = MqttClient::new("localhost".into(), 1883, None, None, "test".into());
        // self.client is None — connect() is the only thing that fills it.
        let err = client
            .publish_command("toggle", serde_json::json!({"entity": "only_charging"}))
            .unwrap_err();
        assert!(err.to_string().contains("not connected"));
    }

    #[test]
    fn publish_command_does_not_error_when_slot_is_some() {
        // After connect() builds the rumqtt Client it stores a clone in the Arc slot.
        // publish_command must be able to use it.  Test the slot-populated path by
        // directly filling the Arc slot with a dummy Client (no broker needed).
        let client = MqttClient::new("localhost".into(), 1883, None, None, "test".into());

        // Build a real Client so we exercise the Arc slot path (not the None path).
        let mqttoptions = rumqttc::MqttOptions::new("test-publish", ("localhost", 1883));
        let (dummy, _connection) = rumqttc::Client::builder(mqttoptions).build();
        {
            let mut slot = client.client.lock().unwrap();
            *slot = Some(dummy);
        }
        // Slot is Some — publish_command must not return "not connected".
        // (try_publish on a disconnected client queues the request in the channel
        // and returns Ok; the real broker connection is the EventLoop's job. We
        // only care that publish_command did not bail out before reaching the
        // client because the slot was None.)
        let result =
            client.publish_command("toggle", serde_json::json!({"entity": "only_charging"}));
        match result {
            Ok(()) => {}
            Err(e) => assert!(
                !e.to_string().contains("not connected"),
                "expected a real publish error, not 'not connected': {e}"
            ),
        }
    }

    // -------------------------------------------------------------------------
    // EV cache integration tests
    // -------------------------------------------------------------------------

    #[test]
    fn ev_cache_throttles_same_field_within_window() {
        // Apply once → cache populated. Second call within 8 s → rejected by TTL.
        // But apply_ev_message still copies the cached value onto st and returns
        // Some (so process_state_update sees consistent state).
        let mut cache = EvCache::default();
        let instances = Some((Some(22), Some(40)));

        let mut st = InverterState::default();
        assert!(MqttClient::apply_ev_message(
            &mut st, &mut cache, "ev", 22, "Soc", 66.0, &instances
        )
        .is_some());
        assert_eq!(st.car_soc, Some(66.0));

        // Second call within the 8 s throttle window → cache not updated, but
        // cached value is re-applied to st and Some is returned.
        let mut st2 = InverterState::default();
        assert!(MqttClient::apply_ev_message(
            &mut st2, &mut cache, "ev", 22, "Soc", 70.0, &instances
        )
        .is_some());
        assert_eq!(st2.car_soc, Some(66.0)); // cached value preserved
    }

    #[test]
    fn ev_cache_restores_after_process_state_update() {
        // Simulate the race: apply_ev_message sets EV fields, then
        // process_state_update clones from before that and overwrites them.
        // The cache must restore the EV values after write-back.
        let state = Arc::new(Mutex::new(InverterState::default()));
        let ev_cache = Arc::new(Mutex::new(EvCache::default()));
        let cerbo_devices: Arc<Mutex<CerboDevices>> = Arc::new(Mutex::new(CerboDevices::default()));

        // Apply EV sample — populates both state and cache.
        {
            let mut guard = state.lock().unwrap();
            let mut cache = ev_cache.lock().unwrap();
            let instances = Some((Some(22), Some(40)));
            MqttClient::apply_ev_message(&mut guard, &mut cache, "ev", 22, "Soc", 66.0, &instances);
            MqttClient::apply_ev_message(
                &mut guard, &mut cache, "ev", 22, "Ac/Power", 3200.0, &instances,
            );
            MqttClient::apply_ev_message(
                &mut guard,
                &mut cache,
                "evcharger",
                40,
                "Ac/Power",
                7400.0,
                &instances,
            );
        }

        // process_state_update with a RawInverterState that has no EV fields
        // (simulates the inverter/state payload missing EV data). The clone-
        // and-merge starts from the pre-apply snapshot, so EV fields would be
        // wiped without cache restoration.
        let raw = RawInverterState {
            gt: Some(500.0),
            ..Default::default()
        };
        MqttClient::process_state_update(
            raw,
            state.clone(),
            None,
            Arc::new(Mutex::new(NotificationState {
                high_consumption: AlertState::new(),
                low_water: AlertState::new(),
                high_solar: AlertState::new(),
                high_load: std::collections::HashMap::new(),
            })),
            Some(cerbo_devices),
            ev_cache.clone(),
            &Arc::new(StateEmitter::new(true)),
        );

        // EV fields AND presence bits must survive the daemon's merge.
        let guard = state.lock().unwrap();
        assert_eq!(guard.car_soc, Some(66.0));
        assert_eq!(guard.car_charging_power, Some(3200.0));
        assert_eq!(guard.ev_charging_power, Some(7400.0));
        assert!(guard.ev_present, "ev_present must survive daemon merge");
        assert!(
            guard.evcharger_present,
            "evcharger_present must survive daemon merge"
        );
    }

    #[test]
    fn ev_cache_zero_power_still_shows_section() {
        // Zero power is a legitimate idle value. The section should stay visible
        // via presence bits (set on cache, restored into state after daemon merge).
        let mut st = InverterState::default();
        let mut cache = EvCache::default();
        let instances = Some((Some(22), Some(40)));

        MqttClient::apply_ev_message(&mut st, &mut cache, "ev", 22, "Ac/Power", 0.0, &instances);
        assert!(cache.ev_present, "section visible via cache.ev_present");
        assert_eq!(st.car_charging_power, Some(0.0));

        MqttClient::apply_ev_message(
            &mut st,
            &mut cache,
            "evcharger",
            40,
            "Ac/Power",
            0.0,
            &instances,
        );
        assert!(
            cache.evcharger_present,
            "section visible via cache.evcharger_present"
        );
        assert_eq!(st.ev_charging_power, Some(0.0));

        // restore_into propagates presence to state
        let mut st2 = InverterState::default();
        cache.restore_into(&mut st2);
        assert!(st2.ev_present);
        assert!(st2.evcharger_present);
    }

    #[test]
    fn ev_cache_zero_soc_does_not_clobber_real_soc() {
        // inverter-control publishes car_soc=0 when no car is connected.
        // The cache must refuse to overwrite a real SoC with 0.
        let mut cache = EvCache::default();
        let instances = Some((Some(22), Some(40)));

        // First: real car SoC from Cerbo.
        let mut st = InverterState::default();
        assert!(MqttClient::apply_ev_message(
            &mut st, &mut cache, "ev", 22, "Soc", 66.0, &instances,
        )
        .is_some());
        assert_eq!(st.car_soc, Some(66.0));

        // Second: daemon publishes 0 → cache refuses update (preserves real).
        // apply_ev_message still copies cached value onto st2 and returns Some.
        let mut st2 = InverterState::default();
        assert_eq!(
            MqttClient::apply_ev_message(&mut st2, &mut cache, "ev", 22, "Soc", 0.0, &instances,),
            Some(EvField::CarSoc),
        );
        assert_eq!(st2.car_soc, Some(66.0)); // cached value preserved, not clobbered
        assert_eq!(st.car_soc, Some(66.0));
    }

    // -------------------------------------------------------------------------
    // Active loads (Cerbo acload) — stable instance keys + name cache
    // -------------------------------------------------------------------------

    #[test]
    fn process_state_update_does_not_replace_cerbo_loads_with_daemon_map() {
        let state = Arc::new(Mutex::new(InverterState::default()));
        let ev_cache = Arc::new(Mutex::new(EvCache::default()));
        let cerbo_devices: Arc<Mutex<CerboDevices>> = Arc::new(Mutex::new(CerboDevices::default()));

        {
            let mut d = cerbo_devices.lock().unwrap();
            MqttClient::apply_acload_message(&mut d, 81, "Ac/Power", "{\"value\": 100}");
            MqttClient::apply_acload_message(&mut d, 81, "CustomName", "{\"value\": \"Oven\"}");
            let mut st = state.lock().unwrap();
            MqttClient::apply_cerbo_to_state(&d, &mut st);
        }

        // Daemon publishes name-keyed loads — classic flicker source.
        let mut daemon_loads = std::collections::HashMap::new();
        daemon_loads.insert("Oven".to_string(), 999.0);
        daemon_loads.insert("Dryer".to_string(), 50.0);
        let raw = RawInverterState {
            gt: Some(1.0),
            loads: Some(daemon_loads),
            ..Default::default()
        };
        MqttClient::process_state_update(
            raw,
            state.clone(),
            None,
            Arc::new(Mutex::new(NotificationState {
                high_consumption: AlertState::new(),
                low_water: AlertState::new(),
                high_solar: AlertState::new(),
                high_load: std::collections::HashMap::new(),
            })),
            Some(cerbo_devices),
            ev_cache,
            &Arc::new(StateEmitter::new(true)),
        );

        let guard = state.lock().unwrap();
        assert_eq!(
            guard.loads.as_ref().unwrap().get("81"),
            Some(&100.0),
            "Cerbo instance-keyed watts must survive daemon loads merge"
        );
        assert!(
            guard.loads.as_ref().unwrap().get("Oven").is_none(),
            "daemon name-keyed map must not replace Cerbo loads"
        );
        assert_eq!(
            guard
                .load_names
                .as_ref()
                .unwrap()
                .get("81")
                .map(String::as_str),
            Some("Oven")
        );
    }

    #[test]
    fn process_state_update_skips_daemon_battery_when_cerbo_shunt_present() {
        let state = Arc::new(Mutex::new(InverterState::default()));
        let ev_cache = Arc::new(Mutex::new(EvCache::default()));
        let cerbo_devices: Arc<Mutex<CerboDevices>> = Arc::new(Mutex::new(CerboDevices::default()));

        {
            let mut d = cerbo_devices.lock().unwrap();
            MqttClient::apply_device_message(
                &mut d,
                "battery",
                2,
                "ProductName",
                "\"SmartShunt 500A/50mV\"",
            );
            MqttClient::apply_device_message(
                &mut d,
                "battery",
                2,
                "Dc/0/Voltage",
                "{\"value\": 52.0}",
            );
            MqttClient::apply_device_message(
                &mut d,
                "battery",
                2,
                "Dc/0/Power",
                "{\"value\": -265.0}",
            );
            MqttClient::apply_device_message(
                &mut d,
                "battery",
                2,
                "Dc/0/Current",
                "{\"value\": -5.1}",
            );
            let mut st = state.lock().unwrap();
            MqttClient::apply_cerbo_to_state(&d, &mut st);
        }

        let raw = RawInverterState {
            battery_power: Some(9999.0),
            battery_voltage: Some(40.0),
            battery_current: Some(0.0),
            battery_soc: Some(11.0),
            gt: Some(10.0),
            ..Default::default()
        };
        MqttClient::process_state_update(
            raw,
            state.clone(),
            None,
            Arc::new(Mutex::new(NotificationState {
                high_consumption: AlertState::new(),
                low_water: AlertState::new(),
                high_solar: AlertState::new(),
                high_load: std::collections::HashMap::new(),
            })),
            Some(cerbo_devices),
            ev_cache,
            &Arc::new(StateEmitter::new(true)),
        );

        let guard = state.lock().unwrap();
        assert_eq!(guard.battery_power, Some(-265.0));
        assert_eq!(guard.battery_voltage, Some(52.0));
        assert_eq!(guard.battery_current, Some(-5.1));
        // Voltage-derived SoC, not daemon 11%.
        assert_eq!(guard.battery_soc, Some(voltage_soc(52.0)));
        // Daemon gt must never merge — Cerbo systemcalc is the only source.
        assert_eq!(guard.gt, None);
    }

    fn empty_notifications() -> Arc<Mutex<NotificationState>> {
        Arc::new(Mutex::new(NotificationState {
            high_consumption: AlertState::new(),
            low_water: AlertState::new(),
            high_solar: AlertState::new(),
            high_load: std::collections::HashMap::new(),
        }))
    }

    #[test]
    fn process_state_update_never_merges_daemon_gt_tt_soc() {
        // Even with no Cerbo discovery yet, gt/tt/soc stay Cerbo-only
        // (None / prior values) — daemon inverter/state is ignored for them.
        let state = Arc::new(Mutex::new(InverterState {
            gt: Some(123.0),
            tt: Some(456.0),
            battery_soc: Some(77.0),
            ..Default::default()
        }));
        let ev_cache = Arc::new(Mutex::new(EvCache::default()));

        let raw = RawInverterState {
            gt: Some(0.0),
            g1: Some(1.0),
            g2: Some(2.0),
            tt: Some(0.0),
            t1: Some(3.0),
            t2: Some(4.0),
            battery_soc: Some(11.0),
            battery_power: Some(50.0),
            version: Some("daemon".into()),
            ..Default::default()
        };
        MqttClient::process_state_update(
            raw,
            state.clone(),
            None,
            empty_notifications(),
            None,
            ev_cache,
            &Arc::new(StateEmitter::new(true)),
        );

        let guard = state.lock().unwrap();
        assert_eq!(guard.gt, Some(123.0));
        assert_eq!(guard.g1, None);
        assert_eq!(guard.g2, None);
        assert_eq!(guard.tt, Some(456.0));
        assert_eq!(guard.t1, None);
        assert_eq!(guard.t2, None);
        assert_eq!(guard.battery_soc, Some(77.0));
        // Power still has shunt-gated daemon fallback when no Cerbo shunt.
        assert_eq!(guard.battery_power, Some(50.0));
        assert_eq!(guard.version.as_deref(), Some("daemon"));
    }

    #[test]
    fn process_state_update_skips_daemon_grid_when_cerbo_system_present() {
        let state = Arc::new(Mutex::new(InverterState::default()));
        let ev_cache = Arc::new(Mutex::new(EvCache::default()));
        let cerbo_devices: Arc<Mutex<CerboDevices>> = Arc::new(Mutex::new(CerboDevices::default()));

        {
            let mut d = cerbo_devices.lock().unwrap();
            assert!(MqttClient::apply_device_message(
                &mut d,
                "system",
                0,
                "Ac/Grid/L1/Power",
                "{\"value\": 120.0}",
            ));
            assert!(MqttClient::apply_device_message(
                &mut d,
                "system",
                0,
                "Ac/Grid/L2/Power",
                "{\"value\": 80.0}",
            ));
            assert!(MqttClient::apply_device_message(
                &mut d,
                "system",
                0,
                "Ac/Consumption/L1/Power",
                "{\"value\": 400.0}",
            ));
            assert!(MqttClient::apply_device_message(
                &mut d,
                "system",
                0,
                "Ac/Consumption/L2/Power",
                "{\"value\": 100.0}",
            ));
            let mut st = state.lock().unwrap();
            MqttClient::apply_cerbo_to_state(&d, &mut st);
        }

        let raw = RawInverterState {
            gt: Some(0.0),
            g1: Some(0.0),
            g2: Some(0.0),
            tt: Some(0.0),
            t1: Some(0.0),
            t2: Some(0.0),
            version: Some("daemon".into()),
            ..Default::default()
        };
        MqttClient::process_state_update(
            raw,
            state.clone(),
            None,
            empty_notifications(),
            Some(cerbo_devices),
            ev_cache,
            &Arc::new(StateEmitter::new(true)),
        );

        let guard = state.lock().unwrap();
        assert_eq!(guard.g1, Some(120.0));
        assert_eq!(guard.g2, Some(80.0));
        assert_eq!(guard.gt, Some(200.0));
        assert_eq!(guard.t1, Some(400.0));
        assert_eq!(guard.t2, Some(100.0));
        assert_eq!(guard.tt, Some(500.0));
        assert_eq!(guard.version.as_deref(), Some("daemon"));
    }

    #[test]
    fn process_state_update_skips_daemon_setpoint_when_cerbo_vebus_present() {
        let state = Arc::new(Mutex::new(InverterState::default()));
        let ev_cache = Arc::new(Mutex::new(EvCache::default()));
        let cerbo_devices: Arc<Mutex<CerboDevices>> = Arc::new(Mutex::new(CerboDevices::default()));

        {
            let mut d = cerbo_devices.lock().unwrap();
            assert!(MqttClient::apply_device_message(
                &mut d,
                "vebus",
                276,
                "Hub4/L1/AcPowerSetpoint",
                "{\"value\": -1500.0}",
            ));
            assert!(MqttClient::apply_device_message(
                &mut d,
                "vebus",
                276,
                "State",
                "{\"value\": 9}",
            ));
            let mut st = state.lock().unwrap();
            MqttClient::apply_cerbo_to_state(&d, &mut st);
        }

        let raw = RawInverterState {
            setpoint: Some(0.0),
            inverter_state: Some("Off".into()),
            ess_mode: Some(EssMode {
                mode_name: Some("Optimized".into()),
                is_external: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        MqttClient::process_state_update(
            raw,
            state.clone(),
            None,
            empty_notifications(),
            Some(cerbo_devices),
            ev_cache,
            &Arc::new(StateEmitter::new(true)),
        );

        let guard = state.lock().unwrap();
        assert_eq!(guard.setpoint, Some(-1500.0));
        assert_eq!(guard.inverter_state.as_deref(), Some("Inverting"));
        // ess_mode remains daemon-only
        assert_eq!(
            guard.ess_mode.as_ref().and_then(|m| m.mode_name.as_deref()),
            Some("Optimized")
        );
    }

    #[test]
    fn process_state_update_skips_daemon_solar_when_cerbo_chargers_present() {
        let state = Arc::new(Mutex::new(InverterState::default()));
        let ev_cache = Arc::new(Mutex::new(EvCache::default()));
        let cerbo_devices: Arc<Mutex<CerboDevices>> = Arc::new(Mutex::new(CerboDevices::default()));

        {
            let mut d = cerbo_devices.lock().unwrap();
            assert!(MqttClient::apply_device_message(
                &mut d,
                "solarcharger",
                1,
                "Yield/Power",
                "{\"value\": 700.0}",
            ));
            assert!(MqttClient::apply_device_message(
                &mut d,
                "solarcharger",
                1,
                "ProductName",
                "{\"value\": \"SmartSolar\"}",
            ));
            assert!(MqttClient::apply_device_message(
                &mut d,
                "pvinverter",
                20,
                "Ac/Power",
                "{\"value\": 300.0}",
            ));
            let mut st = state.lock().unwrap();
            MqttClient::apply_cerbo_to_state(&d, &mut st);
        }

        let raw = RawInverterState {
            solar_total: Some(1.0),
            mppt_individual: Some(vec![1.0, 2.0]),
            mppt_chargers: Some(vec![MpptCharger {
                name: Some("daemon-mppt".into()),
                power: Some(1.0),
                ..Default::default()
            }]),
            pv_inverters: Some(vec![PvInverter {
                name: Some("daemon-pv".into()),
                power: Some(1.0),
                ..Default::default()
            }]),
            pv_inverter_individual: Some(vec![9.0]),
            ..Default::default()
        };
        MqttClient::process_state_update(
            raw,
            state.clone(),
            None,
            empty_notifications(),
            Some(cerbo_devices),
            ev_cache,
            &Arc::new(StateEmitter::new(true)),
        );

        let guard = state.lock().unwrap();
        assert_eq!(guard.mppt_total, Some(700.0));
        assert_eq!(guard.solar_total, Some(1000.0)); // 700 + 300
        assert_eq!(
            guard
                .mppt_chargers
                .as_ref()
                .unwrap()
                .first()
                .and_then(|m| m.name.as_deref()),
            Some("SmartSolar")
        );
        assert!(guard
            .pv_inverters
            .as_ref()
            .unwrap()
            .iter()
            .all(|p| p.name.as_deref() != Some("daemon-pv")));
        assert!(guard.mppt_individual.is_none());
        assert!(guard.pv_inverter_individual.is_none());
    }

    #[test]
    fn process_state_update_does_not_merge_water_or_appliances_from_daemon() {
        let state = Arc::new(Mutex::new(InverterState {
            water_level: Some(77.0),
            water_valve: Some(true),
            pump_switch: Some(false),
            dishwasher_running: Some(false),
            dishwasher_duration: Some(0),
            washer_time: Some(0),
            dryer_time: Some(0),
            ..Default::default()
        }));
        let ev_cache = Arc::new(Mutex::new(EvCache::default()));

        // Daemon JSON still may contain water/appliance keys — they must be
        // ignored (fields removed from RawInverterState) so Cerbo/HA values stay.
        let raw: RawInverterState = serde_json::from_str(
            r#"{
                "water_level": 0,
                "water_valve": false,
                "pump_switch": true,
                "dishwasher_running": true,
                "dishwasher_duration": 99,
                "washer_time": 88,
                "dryer_time": 77,
                "washer_power": true,
                "dryer_power": true,
                "dry_run": true
            }"#,
        )
        .expect("extra appliance keys must not fail deserialize");
        MqttClient::process_state_update(
            raw,
            state.clone(),
            None,
            empty_notifications(),
            None,
            ev_cache,
            &Arc::new(StateEmitter::new(true)),
        );

        let guard = state.lock().unwrap();
        // Cerbo/HA-owned: daemon keys must not overwrite
        assert_eq!(guard.water_level, Some(77.0));
        assert_eq!(guard.water_valve, Some(true));
        assert_eq!(guard.pump_switch, Some(false));
        assert_eq!(guard.dishwasher_running, Some(false));
        assert_eq!(guard.dishwasher_duration, Some(0));
        assert_eq!(guard.washer_time, Some(0));
        assert_eq!(guard.dryer_time, Some(0));
        assert!(guard.washer_power.is_none());
        assert!(guard.dryer_power.is_none());
        // Daemon-only flag still merges
        assert_eq!(guard.dry_run, Some(true));
    }
}

#[cfg(test)]
mod reconnect_policy_tests {
    use super::mqtt_reconnect_delay_secs;

    #[test]
    fn mqtt_reconnect_delay_secs_caps() {
        assert_eq!(mqtt_reconnect_delay_secs(0), 5);
        assert_eq!(mqtt_reconnect_delay_secs(1), 10);
        assert_eq!(mqtt_reconnect_delay_secs(2), 20);
        assert_eq!(mqtt_reconnect_delay_secs(3), 40);
        assert_eq!(mqtt_reconnect_delay_secs(4), 60);
        assert_eq!(mqtt_reconnect_delay_secs(20), 60);
    }
}

#[cfg(test)]
mod keepalive_lifecycle_tests {
    use super::*;
    #[test]
    fn stopping_session_or_connection_finishes_actual_keepalive_task() {
        for stop_session in [false, true] {
            let options = MqttOptions::new("offline-test", ("unused.invalid".to_string(), 1883));
            // Merely construct the channel; never poll or connect to a broker.
            let (client, _connection) = Client::builder(options).capacity(4).build();
            let session = Arc::new(Shutdown::new());
            let connection = Arc::new(Shutdown::new());
            let task = MqttClient::spawn_keepalive(
                client,
                "test".into(),
                session.clone(),
                connection.clone(),
            );
            if stop_session {
                session.stop();
            } else {
                connection.stop();
            }
            tauri::async_runtime::block_on(async {
                tokio::time::timeout(Duration::from_secs(1), task)
                    .await
                    .unwrap()
                    .unwrap();
            });
        }
    }
}
