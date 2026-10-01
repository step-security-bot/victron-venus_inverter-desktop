//! Remote inverter-gateway client (bearer + optional Cloudflare Access).
//! When `gateway_enabled`, the desktop polls `/v1/snapshot` and maps Cerbo
//! leaf paths into `InverterState` for the same UI events as LAN MQTT.

use crate::mqtt::{
    inverter_state_name, voltage_soc, Battery, DiscoveredInstance, InverterState, MpptCharger,
    PvInverter, SetpointOverrideStatus,
};
use log::{info, warn};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter};

const POLL_INTERVAL_SECS: u64 = 2;

/// Gateway credentials require HTTPS, including when connecting directly to the origin.
pub(crate) fn validate_base_url(input: &str) -> Result<String, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("Gateway URL is required".into());
    }
    let url = reqwest::Url::parse(input).map_err(|_| {
        "Enter the gateway's full HTTPS URL, for example https://gateway.example.com"
    })?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err(
            "Remote Gateway requires HTTPS to protect access credentials. Use its HTTPS endpoint (for example a Cloudflare Tunnel hostname), not the HTTP origin."
                .into(),
        );
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(
            "Gateway URL must not contain a username or password; use the credential fields".into(),
        );
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(
            "Gateway URL must be a base HTTPS URL without a query string or fragment".into(),
        );
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn http_client_builder() -> Result<reqwest::ClientBuilder, String> {
    Ok(reqwest::Client::builder()
        .use_preconfigured_tls(crate::tls::client_config()?)
        .https_only(true)
        // Reqwest strips standard Authorization on redirects, but not CF Access headers.
        // Even same-origin redirects should be fixed in the configured base URL.
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(Duration::from_secs(25)))
}

pub(crate) fn http_client() -> Result<reqwest::Client, String> {
    http_client_builder()?
        .build()
        .map_err(|e| format!("gateway http client: {e}"))
}

pub(crate) fn validate_access_credentials(
    access_id: &str,
    access_secret: &str,
) -> Result<(), String> {
    if access_id.trim().is_empty() != access_secret.trim().is_empty() {
        return Err(
            "Provide both Cloudflare Access Client ID and Secret, or leave both empty for direct HTTPS"
                .into(),
        );
    }
    Ok(())
}

/// Cloudflare service credentials belong to clients using an Access-protected URL.
/// With the Access fields empty, only the configured IGW bearer token is added.
pub(crate) fn authenticated_request(
    mut request: reqwest::RequestBuilder,
    access_id: &str,
    access_secret: &str,
    api_token: &str,
) -> Result<reqwest::RequestBuilder, String> {
    validate_access_credentials(access_id, access_secret)?;
    if !access_id.trim().is_empty() {
        request = request
            .header("CF-Access-Client-Id", access_id.trim())
            .header("CF-Access-Client-Secret", access_secret.trim());
    }
    if !api_token.trim().is_empty() {
        request = request.bearer_auth(api_token.trim());
    }
    Ok(request)
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GatewaySnapshot {
    #[serde(default)]
    pub inverter: Option<serde_json::Map<String, Value>>,
    #[serde(default)]
    pub capabilities: HashMap<String, Value>,
    #[serde(default)]
    pub system: HashMap<String, Value>,
    #[serde(default)]
    pub vebus: HashMap<String, Value>,
    #[serde(default)]
    pub battery: HashMap<String, Value>,
    #[serde(default)]
    pub solarcharger: HashMap<String, Value>,
    #[serde(default)]
    pub pvinverter: HashMap<String, Value>,
    #[serde(default)]
    pub tank: HashMap<String, Value>,
    #[serde(default)]
    pub pump: HashMap<String, Value>,
    #[serde(default)]
    pub ev: HashMap<String, Value>,
    #[serde(default)]
    pub evcharger: HashMap<String, Value>,
    #[serde(default)]
    pub acload: HashMap<String, Value>,
    #[serde(default)]
    #[allow(dead_code)]
    pub settings: HashMap<String, Value>,
}

pub struct GatewayClient {
    state: Arc<Mutex<InverterState>>,
    stop: Arc<AtomicBool>,
    handle: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    base: String,
    access_client_id: String,
    access_client_secret: String,
    api_token: String,
}

impl GatewayClient {
    pub fn get_state(&self) -> InverterState {
        self.state.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut h) = self.handle.lock() {
            if let Some(join) = h.take() {
                join.abort();
            }
        }
    }

    pub fn http_auth(&self) -> GatewayHttpAuth {
        GatewayHttpAuth {
            base: self.base.clone(),
            access_client_id: self.access_client_id.clone(),
            access_client_secret: self.access_client_secret.clone(),
            api_token: self.api_token.clone(),
            stopped: self.stop.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct GatewayHttpAuth {
    pub base: String,
    pub access_client_id: String,
    pub access_client_secret: String,
    pub api_token: String,
    stopped: Arc<AtomicBool>,
}

impl GatewayHttpAuth {
    pub(crate) fn ensure_active(&self) -> Result<(), String> {
        if self.stopped.load(Ordering::SeqCst) {
            Err("Gateway connection changed; retry the action on the current connection".into())
        } else {
            Ok(())
        }
    }
}

pub async fn acknowledge_all_notifications_http(auth: &GatewayHttpAuth) -> Result<(), String> {
    send_command(auth, "acknowledge_all_notifications", json!({})).await
}

/// One fresh authenticated snapshot for command validation; never retries a write.
pub(crate) async fn command_snapshot(auth: &GatewayHttpAuth) -> Result<GatewaySnapshot, String> {
    auth.ensure_active()?;
    fetch_snapshot(
        &http_client()?,
        &auth.base,
        &auth.access_client_id,
        &auth.access_client_secret,
        &auth.api_token,
    )
    .await
}

pub(crate) async fn send_command(
    auth: &GatewayHttpAuth,
    name: &str,
    body: Value,
) -> Result<(), String> {
    auth.ensure_active()?;
    post_command(auth, name, body).await
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GatewayInstances {
    pub water_tank: Option<u32>,
    pub water_pump: Option<u32>,
    pub water_valve: Option<u32>,
    pub ev: Option<u32>,
    pub evcharger: Option<u32>,
}

/// Explicit local choices win over controller presentation defaults. A missing
/// selected device stays unknown: silently selecting a different pump is unsafe.
pub(crate) fn snapshot_instances(
    snap: &GatewaySnapshot,
    configured: GatewayInstances,
) -> GatewayInstances {
    let ui = snap.inverter.as_ref().and_then(|v| v.get("ui_config"));
    let choice = |section: &str, key: &str| {
        ui.and_then(|v| v.get(section))
            .and_then(|v| v.get(key))
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
    };
    GatewayInstances {
        water_tank: configured
            .water_tank
            .or_else(|| choice("water", "tank_instance"))
            .or_else(|| device_instances(&snap.tank).into_iter().next()),
        water_pump: configured
            .water_pump
            .or_else(|| choice("water", "pump_instance"))
            .or(Some(1)),
        water_valve: configured
            .water_valve
            .or_else(|| choice("water", "valve_instance"))
            .or(Some(2)),
        ev: configured
            .ev
            .or_else(|| choice("ev", "instance"))
            .or(Some(22)),
        evcharger: configured
            .evcharger
            .or_else(|| choice("ev", "evcharger_instance"))
            .or(Some(40)),
    }
}

fn device_instances(map: &HashMap<String, Value>) -> Vec<u32> {
    let mut ids: Vec<u32> = map
        .iter()
        .filter(|(_, value)| !value.is_null())
        .filter_map(|(path, _)| path.split('/').next()?.parse().ok())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn device_connected(map: &HashMap<String, Value>, instance: u32) -> bool {
    match map.get(&format!("{instance}/Connected")) {
        None => true, // Older native producers did not publish Connected.
        Some(value) => num(value) == Some(1.0),
    }
}

fn water_device_connected(map: &HashMap<String, Value>, instance: u32) -> bool {
    match map.get(&format!("{instance}/Connected")) {
        None => true,
        Some(value) => value.as_u64() == Some(1),
    }
}

fn mode(map: &HashMap<String, Value>, instance: u32) -> Option<u8> {
    map.get(&format!("{instance}/Mode"))?
        .as_u64()
        .filter(|v| *v <= 2)
        .map(|v| v as u8)
}

fn map_water_ev(snap: &GatewaySnapshot, configured: GatewayInstances, st: &mut InverterState) {
    let selected = snapshot_instances(snap, configured);
    let mut discovery = Vec::new();
    for (kind, map) in [
        ("tank", &snap.tank),
        ("pump", &snap.pump),
        ("ev", &snap.ev),
        ("evcharger", &snap.evcharger),
    ] {
        for instance in device_instances(map) {
            let name = ["CustomName", "ProductName"].iter().find_map(|leaf| {
                map.get(&format!("{instance}/{leaf}"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
            });
            discovery.push(DiscoveredInstance {
                instance,
                kind: kind.into(),
                name,
            });
        }
    }
    st.discovered_water_ev = Some(discovery);
    if let Some(i) = selected
        .water_tank
        .filter(|i| water_device_connected(&snap.tank, *i))
    {
        // dbus-pump and Cerbo tank Level are percentages, including 0.5%.
        st.water_level = path_num(&snap.tank, &format!("{i}/Level"));
    }
    for (instance, valve) in [(selected.water_pump, false), (selected.water_valve, true)] {
        if let Some(i) = instance.filter(|i| water_device_connected(&snap.pump, *i)) {
            // The existing water status buttons are controls. Match IGW's
            // target guards before exposing them; do not guess an unknown Mode.
            let Some(current_mode) = mode(&snap.pump, i) else {
                continue;
            };
            if snap.capabilities.get("water_mode").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let value = path_num(&snap.pump, &format!("{i}/State"))
                .filter(|v| *v == 0.0 || *v == 1.0)
                .map(|v| v == 1.0);
            if valve {
                st.water_valve = value;
                st.water_valve_mode = Some(current_mode);
            } else {
                st.pump_switch = value;
                st.water_pump_mode = Some(current_mode);
            }
        }
    }
    if let Some(i) = selected.ev.filter(|i| device_connected(&snap.ev, *i)) {
        st.ev_present = device_instances(&snap.ev).contains(&i);
        st.car_soc = path_num(&snap.ev, &format!("{i}/Soc"));
        st.car_charging_power = path_num(&snap.ev, &format!("{i}/Ac/Power"));
    }
    if let Some(i) = selected
        .evcharger
        .filter(|i| device_connected(&snap.evcharger, *i))
    {
        st.evcharger_present = device_instances(&snap.evcharger).contains(&i);
        st.ev_charging_power = path_num(&snap.evcharger, &format!("{i}/Ac/Power"));
        st.ev_power = st.ev_charging_power;
        // Compatibility with older dbus-ev producers using evcharger/Soc.
        // Explicit disconnection/nulls from a selected EV are authoritative;
        // another device's SOC must not make that vehicle look connected.
        if st.car_soc.is_none() && snap.ev.is_empty() {
            st.car_soc = path_num(&snap.evcharger, &format!("{i}/Soc"));
        }
    }
}

fn bool_value(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(v) => Some(*v),
        Value::Number(v) if v.as_i64() == Some(0) => Some(false),
        Value::Number(v) if v.as_i64() == Some(1) => Some(true),
        Value::String(v) => match v.to_ascii_lowercase().as_str() {
            "true" | "1" | "on" | "online" => Some(true),
            "false" | "0" | "off" | "offline" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// A missing capability/controller/value is unknown, never an inactive override.
/// The same parser serves display, fresh reads, and request-id acknowledgement.
pub(crate) fn snapshot_override_status(
    snap: &GatewaySnapshot,
) -> Result<SetpointOverrideStatus, String> {
    if snap
        .capabilities
        .get("setpoint_override")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Err("This IGW does not support Setpoint Override; update the gateway first".into());
    }
    let unavailable = || {
        "Current Setpoint Override status is unavailable; wait for fresh IGW telemetry".to_string()
    };
    let status = snap
        .inverter
        .as_ref()
        .and_then(|v| v.get("setpoint_override"))
        .and_then(Value::as_object)
        .ok_or_else(unavailable)?;
    let value = match status.get("value") {
        Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_i64()
                .and_then(|v| i32::try_from(v).ok())
                .ok_or_else(unavailable)?,
        ),
        None => return Err(unavailable()),
    };
    let text = |key: &str| -> Result<Option<String>, String> {
        match status.get(key) {
            Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.clone())),
            _ => Err(unavailable()),
        }
    };
    Ok(SetpointOverrideStatus {
        value,
        last_error: text("last_error")?,
        request_id: text("request_id")?,
    })
}

fn map_controller(snap: &GatewaySnapshot, st: &mut InverterState) {
    // IGW expires the controller separately from native Cerbo devices. None
    // must clear controls even if the rest of the gateway remains connected.
    st.booleans = Some(HashMap::new());
    st.grid_using_backup = Some(false);
    let Some(controller) = snap.inverter.as_ref() else {
        return;
    };
    let field = |name: &str| controller.get(name).cloned().unwrap_or(Value::Null);
    st.booleans = Some(
        controller
            .get("booleans")
            .and_then(Value::as_object)
            .map(|flags| {
                flags
                    .iter()
                    .filter_map(|(k, v)| bool_value(v).map(|v| (k.clone(), v)))
                    .collect()
            })
            .unwrap_or_default(),
    );
    st.dry_run = controller.get("dry_run").and_then(bool_value);
    st.ess_mode = serde_json::from_value(field("ess_mode")).ok();
    st.ui_config = serde_json::from_value(field("ui_config")).ok();
    st.features = serde_json::from_value(field("features")).ok();
    st.version = serde_json::from_value(field("version")).ok();
    st.uptime = serde_json::from_value(field("uptime")).ok();
    st.ha_connected = controller.get("ha_connected").and_then(bool_value);
    st.daily_stats = serde_json::from_value(field("daily_stats")).ok();
    st.solar_forecast = serde_json::from_value(field("solar_forecast")).ok();
    st.setpoint_override = snapshot_override_status(snap).ok();
    st.grid_backup = serde_json::from_value(field("grid_backup")).ok();
    st.grid_using_backup = Some(
        controller
            .get("grid_using_backup")
            .and_then(bool_value)
            .unwrap_or(false),
    );
    // Polling a cached controller every two seconds must not renew a stale
    // submeter. The measurement timestamp is stable across repeated snapshots.
    st.grid_backup_observed_at = st
        .grid_backup
        .as_ref()
        .and_then(|v| v.measurement_time)
        .filter(|v| v.is_finite());
}

fn invalidate_gateway_controls(st: &mut InverterState) {
    let empty = snapshot_to_state(&GatewaySnapshot::default());
    st.gateway_snapshot = Some(true);
    st.cached_snapshot = Some(true);
    st.booleans = empty.booleans;
    st.dry_run = None;
    st.ess_mode = None;
    st.ui_config = None;
    st.features = None;
    st.grid_backup = None;
    st.grid_using_backup = Some(false);
    st.grid_backup_observed_at = None;
    st.setpoint_override = None;
    st.water_level = None;
    st.pump_switch = None;
    st.water_valve = None;
    st.water_pump_mode = None;
    st.water_valve_mode = None;
    st.car_soc = None;
    st.car_charging_power = None;
    st.ev_charging_power = None;
    st.ev_power = None;
    st.ev_present = false;
    st.evcharger_present = false;
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok().filter(|v| v.is_finite()),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

fn path_num(map: &HashMap<String, Value>, path: &str) -> Option<f64> {
    map.get(path).and_then(num)
}

/// Map gateway snapshot leaf maps into dashboard InverterState.
pub fn snapshot_to_state(snap: &GatewaySnapshot) -> InverterState {
    snapshot_to_state_with_instances(snap, GatewayInstances::default())
}

fn snapshot_to_state_with_instances(
    snap: &GatewaySnapshot,
    instances: GatewayInstances,
) -> InverterState {
    let mut st = InverterState {
        gateway_snapshot: Some(true),
        cached_snapshot: Some(false),
        ..InverterState::default()
    };

    let g1 = path_num(&snap.system, "0/Ac/Grid/L1/Power");
    let g2 = path_num(&snap.system, "0/Ac/Grid/L2/Power");
    st.g1 = g1;
    st.g2 = g2;
    // Present null values explicitly invalidate a phase; absent paths remain
    // partial snapshots and must not invalidate unrelated dashboard fields.
    st.grid_l1_available = snap
        .system
        .get("0/Ac/Grid/L1/Power")
        .filter(|value| value.is_null() || num(value).is_some())
        .map(|_| g1.is_some());
    st.grid_l2_available = snap
        .system
        .get("0/Ac/Grid/L2/Power")
        .filter(|value| value.is_null() || num(value).is_some())
        .map(|_| g2.is_some());
    st.gt = match (g1, g2) {
        (Some(a), Some(b)) => Some(a + b),
        (Some(a), None) | (None, Some(a)) => Some(a),
        _ => None,
    };

    let t1 = path_num(&snap.system, "0/Ac/Consumption/L1/Power");
    let t2 = path_num(&snap.system, "0/Ac/Consumption/L2/Power");
    st.t1 = t1;
    st.t2 = t2;
    st.tt = match (t1, t2) {
        (Some(a), Some(b)) => Some(a + b),
        (Some(a), None) | (None, Some(a)) => Some(a),
        _ => None,
    };

    // Bank V/I/P/SoC: prefer SmartShunt (same as LAN `apply_cerbo_to_state`).
    // system/0/Dc/Battery/* is a parallel aggregate and often disagrees with the shunt
    // (e.g. 25.2 A system vs 23.5 A shunt) — that was the IGW vs MQTT mismatch.
    // Filled after the battery device list below.

    // VE.Bus: first instance with Hub4 setpoint / State.
    let mut setpoint = None;
    let mut inv_state = None;
    let mut vebus_insts: Vec<u32> = snap
        .vebus
        .keys()
        .filter_map(|k| k.split('/').next()?.parse().ok())
        .collect();
    vebus_insts.sort_unstable();
    vebus_insts.dedup();
    for inst in vebus_insts {
        let sp = path_num(&snap.vebus, &format!("{inst}/Hub4/L1/AcPowerSetpoint"));
        let state_code = path_num(&snap.vebus, &format!("{inst}/State"));
        if setpoint.is_none() {
            setpoint = sp;
        }
        if inv_state.is_none() {
            if let Some(code) = state_code {
                inv_state = Some(inverter_state_name(code as u32));
            }
        }
    }
    st.setpoint = setpoint;
    st.inverter_state = inv_state;

    // MPPT chargers
    let mut charger_insts: Vec<u32> = snap
        .solarcharger
        .keys()
        .filter_map(|k| k.split('/').next()?.parse().ok())
        .collect();
    charger_insts.sort_unstable();
    charger_insts.dedup();
    let mut mppts = Vec::new();
    let mut mppt_powers = Vec::new();
    for inst in charger_insts {
        let power = path_num(&snap.solarcharger, &format!("{inst}/Yield/Power"))
            .or_else(|| path_num(&snap.solarcharger, &format!("{inst}/Dc/0/Power")));
        let current = path_num(&snap.solarcharger, &format!("{inst}/Dc/0/Current"));
        let pv_v = path_num(&snap.solarcharger, &format!("{inst}/Pv/V"));
        let name = snap
            .solarcharger
            .get(&format!("{inst}/CustomName"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                snap.solarcharger
                    .get(&format!("{inst}/ProductName"))
                    .and_then(|v| v.as_str())
            })
            .map(|s| s.to_string());
        if power.is_some() || current.is_some() || name.is_some() {
            if let Some(p) = power {
                mppt_powers.push(p);
            }
            let serial = snap
                .solarcharger
                .get(&format!("{inst}/Serial"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            mppts.push(MpptCharger {
                name,
                serial,
                instance: Some(inst),
                pv_voltage: pv_v,
                current,
                power,
            });
        }
    }
    let mppt_total: f64 = mppt_powers.iter().sum();
    let has_mppt = !mppts.is_empty();
    if has_mppt {
        st.mppt_chargers = Some(mppts);
        st.mppt_individual = Some(mppt_powers);
        st.mppt_total = Some(mppt_total);
    }

    // AC PV inverters
    let mut pv_insts: Vec<u32> = snap
        .pvinverter
        .keys()
        .filter_map(|k| k.split('/').next()?.parse().ok())
        .collect();
    pv_insts.sort_unstable();
    pv_insts.dedup();
    let mut pvs = Vec::new();
    let mut pv_powers = Vec::new();
    for inst in pv_insts {
        let power = path_num(&snap.pvinverter, &format!("{inst}/Ac/Power"))
            .or_else(|| path_num(&snap.pvinverter, &format!("{inst}/Ac/L1/Power")));
        let name = snap
            .pvinverter
            .get(&format!("{inst}/CustomName"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                snap.pvinverter
                    .get(&format!("{inst}/ProductName"))
                    .and_then(|v| v.as_str())
            })
            .map(|s| s.to_string());
        if let Some(p) = power {
            pv_powers.push(p);
            pvs.push(PvInverter {
                name,
                serial: None,
                instance: Some(inst),
                voltage: path_num(&snap.pvinverter, &format!("{inst}/Ac/L1/Voltage")),
                current: path_num(&snap.pvinverter, &format!("{inst}/Ac/L1/Current")),
                power: Some(p),
            });
        }
    }
    let pv_total: f64 = pv_powers.iter().sum();
    let has_pv = !pvs.is_empty();
    if has_pv {
        st.pv_inverters = Some(pvs);
        st.pv_inverter_individual = Some(pv_powers.clone());
    }

    // Only set when we saw at least one producer — Some(0) would overwrite a
    // richer LAN MQTT value if both sources briefly race.
    if has_mppt || has_pv {
        st.solar_total = Some(mppt_total + pv_total);
    }

    // Battery devices (for multi-battery UI)
    let mut bat_insts: Vec<u32> = snap
        .battery
        .keys()
        .filter_map(|k| k.split('/').next()?.parse().ok())
        .collect();
    bat_insts.sort_unstable();
    bat_insts.dedup();
    let mut bats = Vec::new();
    for inst in bat_insts {
        let voltage = path_num(&snap.battery, &format!("{inst}/Dc/0/Voltage"));
        let current = path_num(&snap.battery, &format!("{inst}/Dc/0/Current"));
        let power = path_num(&snap.battery, &format!("{inst}/Dc/0/Power"));
        let soc = path_num(&snap.battery, &format!("{inst}/Soc"));
        let name = snap
            .battery
            .get(&format!("{inst}/CustomName"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                snap.battery
                    .get(&format!("{inst}/ProductName"))
                    .and_then(|v| v.as_str())
            })
            .map(|s| s.to_string());
        let serial = snap
            .battery
            .get(&format!("{inst}/Serial"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        if voltage.is_some() || power.is_some() || current.is_some() || name.is_some() {
            let bat_state = current.map(crate::mqtt::MqttClient::state_from_current);
            // Cerbo publishes TimeToGo in seconds (null when idle). Same formatter as LAN MQTT.
            let mut time_to_go = path_num(&snap.battery, &format!("{inst}/TimeToGo"))
                .and_then(crate::mqtt::MqttClient::format_time_to_go);
            // Only meaningful while charging/discharging (mirrors apply_cerbo_to_state).
            if !matches!(bat_state.as_deref(), Some("Charging") | Some("Discharging")) {
                time_to_go = None;
            }
            bats.push(Battery {
                name,
                serial,
                instance: Some(inst),
                soc,
                voltage,
                current,
                power,
                state: bat_state,
                time_to_go,
                max_cell_voltage: path_num(&snap.battery, &format!("{inst}/System/MaxCellVoltage")),
                max_voltage_cell_id: snap
                    .battery
                    .get(&format!("{inst}/System/MaxVoltageCellId"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                min_cell_voltage: path_num(&snap.battery, &format!("{inst}/System/MinCellVoltage")),
                min_voltage_cell_id: snap
                    .battery
                    .get(&format!("{inst}/System/MinVoltageCellId"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
            });
        }
    }
    if !bats.is_empty() {
        // Bank totals from SmartShunt only (never sum chains — double-counts).
        if let Some(shunt) = bats.iter().find(|b| {
            b.name
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .contains("shunt")
        }) {
            st.battery_voltage = shunt.voltage;
            st.battery_current = Some(shunt.current.unwrap_or(0.0));
            st.battery_power = Some(shunt.power.unwrap_or(0.0));
            st.battery_soc = shunt.voltage.map(voltage_soc);
        } else {
            let batt_v = path_num(&snap.system, "0/Dc/Battery/Voltage");
            st.battery_voltage = batt_v;
            st.battery_current = path_num(&snap.system, "0/Dc/Battery/Current");
            st.battery_power = path_num(&snap.system, "0/Dc/Battery/Power");
            st.battery_soc = batt_v.map(voltage_soc);
        }
        st.batteries = Some(bats);
    } else {
        let batt_v = path_num(&snap.system, "0/Dc/Battery/Voltage");
        st.battery_voltage = batt_v;
        st.battery_current = path_num(&snap.system, "0/Dc/Battery/Current");
        st.battery_power = path_num(&snap.system, "0/Dc/Battery/Power");
        st.battery_soc = batt_v.map(voltage_soc);
    }

    // Active loads from acload
    let mut load_insts: Vec<u32> = snap
        .acload
        .keys()
        .filter_map(|k| k.split('/').next()?.parse().ok())
        .collect();
    load_insts.sort_unstable();
    load_insts.dedup();
    let mut loads = HashMap::new();
    let mut load_names = HashMap::new();
    for inst in load_insts {
        let power = path_num(&snap.acload, &format!("{inst}/Ac/Power"))
            .or_else(|| path_num(&snap.acload, &format!("{inst}/Ac/L1/Power")));
        if let Some(p) = power {
            let key = inst.to_string();
            loads.insert(key.clone(), p);
            if let Some(name) = snap
                .acload
                .get(&format!("{inst}/CustomName"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    snap.acload
                        .get(&format!("{inst}/ProductName"))
                        .and_then(|v| v.as_str())
                })
            {
                load_names.insert(key, name.to_string());
            }
        }
    }
    if !loads.is_empty() {
        st.loads = Some(loads);
        if !load_names.is_empty() {
            st.load_names = Some(load_names);
        }
    }

    map_water_ev(snap, instances, &mut st);
    map_controller(snap, &mut st);

    st
}

async fn fetch_snapshot(
    client: &reqwest::Client,
    base: &str,
    access_id: &str,
    access_secret: &str,
    api_token: &str,
) -> Result<GatewaySnapshot, String> {
    let base = validate_base_url(base)?;
    let url = format!("{base}/v1/snapshot");
    let req = authenticated_request(
        client
            .get(&url)
            .header("User-Agent", "inverter-desktop/gateway"),
        access_id,
        access_secret,
        api_token,
    )?;
    let resp = req
        .send()
        .await
        .map_err(|e| format!("gateway snapshot request failed: {e}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("gateway snapshot body: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "gateway snapshot HTTP {status}: {}",
            body.chars().take(160).collect::<String>()
        ));
    }
    serde_json::from_str(&body).map_err(|e| format!("gateway snapshot JSON: {e}"))
}

async fn post_command(auth: &GatewayHttpAuth, name: &str, body: Value) -> Result<(), String> {
    let base = validate_base_url(&auth.base)?;
    let url = format!("{}/v1/commands/{}", base, name.trim_matches('/'));
    let client = http_client()?;
    let req = authenticated_request(
        client
            .post(&url)
            .header("User-Agent", "inverter-desktop/gateway")
            .json(&body),
        &auth.access_client_id,
        &auth.access_client_secret,
        &auth.api_token,
    )?;
    auth.ensure_active()?;
    let resp = req
        .send()
        .await
        .map_err(|e| format!("gateway command request failed: {e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("gateway command body: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "gateway command HTTP {status}: {}",
            text.chars().take(160).collect::<String>()
        ));
    }
    Ok(())
}

/// Keep last time_to_go when this poll has no TimeToGo leaf (Cerbo often nulls it)
/// and the battery is not Idle.
fn preserve_battery_time_to_go(prev: &InverterState, next: &mut InverterState) {
    let Some(next_bats) = next.batteries.as_mut() else {
        return;
    };
    let Some(prev_bats) = prev.batteries.as_ref() else {
        return;
    };
    for bat in next_bats.iter_mut() {
        if bat.time_to_go.is_some() {
            continue;
        }
        if bat.state.as_deref() == Some("Idle") {
            continue;
        }
        let prev_bat = prev_bats.iter().find(|p| {
            (p.serial.is_some() && p.serial == bat.serial)
                || (p.instance.is_some() && p.instance == bat.instance)
                || (p.name.is_some()
                    && !p.name.as_deref().unwrap_or("").is_empty()
                    && p.name == bat.name)
        });
        if let Some(p) = prev_bat {
            if p.time_to_go.is_some() {
                bat.time_to_go = p.time_to_go.clone();
            }
        }
    }
}

pub(crate) fn start_gateway_client(
    app: AppHandle,
    url: String,
    access_client_id: String,
    access_client_secret: String,
    api_token: String,
    instances: GatewayInstances,
) -> Result<GatewayClient, String> {
    let base = validate_base_url(&url)?;
    validate_access_credentials(&access_client_id, &access_client_secret)?;

    let state = Arc::new(Mutex::new(InverterState::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let state_c = state.clone();
    let stop_c = stop.clone();
    let access_id = access_client_id.trim().to_string();
    let access_secret = access_client_secret.trim().to_string();
    let token = api_token.trim().to_string();
    let access_id_poll = access_id.clone();
    let access_secret_poll = access_secret.clone();
    let token_poll = token.clone();
    let base_poll = base.clone();

    let handle = tauri::async_runtime::spawn(async move {
        // A new connection has no confirmed override status until its first
        // successful snapshot, even if the previous transport had one.
        let _ = app.emit(
            "setpoint-override-update",
            Option::<SetpointOverrideStatus>::None,
        );
        let client = match http_client() {
            Ok(c) => c,
            Err(e) => {
                warn!("gateway http client: {e}");
                let _ = app.emit("mqtt-connection-status", false);
                return;
            }
        };

        let mut connected_emitted = false;
        let mut interval = tokio::time::interval(Duration::from_secs(POLL_INTERVAL_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        while !stop_c.load(Ordering::SeqCst) {
            interval.tick().await;
            if stop_c.load(Ordering::SeqCst) {
                break;
            }
            match fetch_snapshot(
                &client,
                &base_poll,
                &access_id_poll,
                &access_secret_poll,
                &token_poll,
            )
            .await
            {
                Ok(snap) => {
                    let mut mapped = snapshot_to_state_with_instances(&snap, instances);
                    if let Ok(mut g) = state_c.lock() {
                        preserve_battery_time_to_go(&g, &mut mapped);
                        *g = mapped.clone();
                    }
                    if !connected_emitted {
                        connected_emitted = true;
                        info!("gateway remote connected to {base_poll}");
                        let _ = app.emit("mqtt-connection-status", true);
                    }
                    let _ = app.emit("setpoint-override-update", &mapped.setpoint_override);
                    let _ = app.emit("mqtt-state-update", mapped);
                }
                Err(e) => {
                    warn!("gateway poll failed: {e}");
                    let _ = app.emit(
                        "setpoint-override-update",
                        Option::<SetpointOverrideStatus>::None,
                    );
                    if let Ok(mut cached) = state_c.lock() {
                        invalidate_gateway_controls(&mut cached);
                        let _ = app.emit("mqtt-state-update", cached.clone());
                    }
                    if connected_emitted {
                        connected_emitted = false;
                        let _ = app.emit("mqtt-connection-status", false);
                    }
                }
            }
        }
        let _ = app.emit(
            "setpoint-override-update",
            Option::<SetpointOverrideStatus>::None,
        );
        let _ = app.emit("mqtt-connection-status", false);
        info!("gateway remote poller stopped");
    });

    Ok(GatewayClient {
        state,
        stop,
        handle: Mutex::new(Some(handle)),
        base: base.clone(),
        access_client_id: access_id,
        access_client_secret: access_secret,
        api_token: token,
    })
}

#[cfg(test)]
pub(crate) fn idle_test_client() -> GatewayClient {
    GatewayClient {
        state: Arc::new(Mutex::new(InverterState::default())),
        stop: Arc::new(AtomicBool::new(false)),
        handle: Mutex::new(None),
        base: String::new(),
        access_client_id: String::new(),
        access_client_secret: String::new(),
        api_token: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_gateway_requests_omit_cloudflare_headers() {
        let client = http_client().unwrap();
        for (method, path) in [
            (reqwest::Method::GET, "/health"),
            (reqwest::Method::GET, "/v1/snapshot"),
            (
                reqwest::Method::POST,
                "/v1/commands/acknowledge_all_notifications",
            ),
        ] {
            let request = authenticated_request(
                client.request(method.clone(), format!("https://gateway.example.com{path}")),
                " ",
                "\t",
                " read-token ",
            )
            .unwrap()
            .build()
            .unwrap();
            assert_eq!(request.method(), method);
            assert_eq!(request.headers()["Authorization"], "Bearer read-token");
            assert!(!request.headers().contains_key("CF-Access-Client-Id"));
            assert!(!request.headers().contains_key("CF-Access-Client-Secret"));
        }
    }

    #[test]
    fn cloudflare_gateway_requests_preserve_complete_service_credentials() {
        let client = http_client().unwrap();
        let request = authenticated_request(
            client.get("https://gateway.example.com/v1/snapshot"),
            " access-id ",
            " access-secret ",
            " api-token ",
        )
        .unwrap()
        .build()
        .unwrap();
        assert_eq!(request.headers()["CF-Access-Client-Id"], "access-id");
        assert_eq!(
            request.headers()["CF-Access-Client-Secret"],
            "access-secret"
        );
        assert_eq!(request.headers()["Authorization"], "Bearer api-token");
        let without_bearer = authenticated_request(
            client.get("https://gateway.example.com/health"),
            "access-id",
            "access-secret",
            " ",
        )
        .unwrap()
        .build()
        .unwrap();
        assert!(!without_bearer.headers().contains_key("Authorization"));
    }

    #[tokio::test]
    async fn all_gateway_operations_reject_incomplete_cloudflare_credentials() {
        let client = http_client().unwrap();
        let base = "https://127.0.0.1:1";
        for (id, secret) in [("id", " "), ("", "secret")] {
            assert!(fetch_snapshot(&client, base, id, secret, "token")
                .await
                .unwrap_err()
                .contains("Provide both Cloudflare"));
            assert!(send_command(
                &GatewayHttpAuth {
                    base: base.into(),
                    access_client_id: id.into(),
                    access_client_secret: secret.into(),
                    api_token: "token".into(),
                    stopped: Arc::new(AtomicBool::new(false))
                },
                "test",
                json!({})
            )
            .await
            .unwrap_err()
            .contains("Provide both Cloudflare"));
            assert!(crate::test_gateway_connection(
                base.into(),
                id.into(),
                secret.into(),
                Some("token".into())
            )
            .await
            .unwrap_err()
            .contains("Provide both Cloudflare"));
        }
    }

    #[test]
    fn gateway_url_requires_https_and_separate_credentials() {
        assert_eq!(
            validate_base_url("  https://gateway.example.com:8443/inverter///  ").unwrap(),
            "https://gateway.example.com:8443/inverter"
        );
        assert_eq!(
            validate_base_url("https://gateway.example.com").unwrap(),
            "https://gateway.example.com"
        );
        for input in [
            "",
            "gateway.example.com",
            "http://gateway.example.com",
            "http://localhost:8080",
            "file:///tmp/gateway",
            "https://user:secret@gateway.example.com",
            "https://gateway.example.com?token=secret",
            "https://gateway.example.com#fragment",
        ] {
            assert!(
                validate_base_url(input).is_err(),
                "accepted unsafe gateway URL"
            );
        }
    }

    #[tokio::test]
    async fn all_gateway_operations_reject_http_before_sending_credentials() {
        let client = http_client().unwrap();
        let url = "http://127.0.0.1:1";
        assert!(fetch_snapshot(&client, url, "id", "secret", "token")
            .await
            .unwrap_err()
            .contains("requires HTTPS"));
        assert!(send_command(
            &GatewayHttpAuth {
                base: url.into(),
                access_client_id: "id".into(),
                access_client_secret: "secret".into(),
                api_token: "token".into(),
                stopped: Arc::new(AtomicBool::new(false))
            },
            "test",
            json!({})
        )
        .await
        .unwrap_err()
        .contains("requires HTTPS"));
        assert!(crate::test_gateway_connection(
            url.into(),
            "id".into(),
            "secret".into(),
            Some("token".into())
        )
        .await
        .err()
        .unwrap()
        .contains("requires HTTPS"));
        // The client also refuses an HTTP URL if a future caller misses URL validation.
        assert!(client.get(url).send().await.unwrap_err().is_builder());
    }

    #[tokio::test]
    async fn redirects_never_forward_gateway_credentials_or_commands() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        // Only this test disables HTTPS, to inspect real redirect behaviour with local
        // HTTP fixtures. It retains the exact production redirect policy.
        let client = http_client_builder()
            .unwrap()
            .https_only(false)
            .no_proxy()
            .build()
            .unwrap();
        for (method, status) in [
            (reqwest::Method::GET, 302),
            (reqwest::Method::POST, 307),
            (reqwest::Method::POST, 308),
        ] {
            let destination = TcpListener::bind("127.0.0.1:0").unwrap();
            destination.set_nonblocking(true).unwrap();
            let origin = TcpListener::bind("127.0.0.1:0").unwrap();
            origin.set_nonblocking(true).unwrap();
            let origin_url = format!("http://{}/v1/snapshot", origin.local_addr().unwrap());
            let redirect_url = format!("http://{}/credentials", destination.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match origin.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "gateway test request timed out"
                            );
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("gateway test accept failed: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let size = stream.read(&mut buf).unwrap();
                    assert_ne!(size, 0);
                    request.extend_from_slice(&buf[..size]);
                }
                write!(stream, "HTTP/1.1 {status} Redirect\r\nLocation: {redirect_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                String::from_utf8(request).unwrap()
            });
            let response = client
                .request(method, &origin_url)
                .header("CF-Access-Client-Id", "test-client")
                .header("CF-Access-Client-Secret", "test-secret")
                .bearer_auth("test-token")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), status);
            let request = server.join().unwrap().to_ascii_lowercase();
            assert!(request.contains("cf-access-client-secret: test-secret"));
            assert!(request.contains("authorization: bearer test-token"));
            assert_eq!(
                destination.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
    }

    #[test]
    fn stopping_idle_client_sets_shutdown_and_is_idempotent() {
        let client = idle_test_client();
        client.stop();
        client.stop();
        assert!(client.stop.load(Ordering::SeqCst));
        assert!(client.handle.lock().unwrap().is_none());
    }

    #[test]
    fn maps_core_tiles() {
        let mut snap = GatewaySnapshot::default();
        snap.system.insert("0/Ac/Grid/L1/Power".into(), json!(10.0));
        snap.system.insert("0/Ac/Grid/L2/Power".into(), json!(5.0));
        snap.system
            .insert("0/Ac/Consumption/L1/Power".into(), json!(100.0));
        snap.system
            .insert("0/Ac/Consumption/L2/Power".into(), json!(50.0));
        snap.system
            .insert("0/Dc/Battery/Voltage".into(), json!(52.15));
        snap.system
            .insert("0/Dc/Battery/Power".into(), json!(-1800.0));
        snap.vebus
            .insert("290/Hub4/L1/AcPowerSetpoint".into(), json!(-2100.0));
        snap.vebus.insert("290/State".into(), json!(3));

        let st = snapshot_to_state(&snap);
        assert_eq!(st.gt, Some(15.0));
        assert_eq!(st.tt, Some(150.0));
        assert_eq!(st.battery_power, Some(-1800.0));
        assert_eq!(st.setpoint, Some(-2100.0));
        assert_eq!(st.inverter_state.as_deref(), Some("Bulk"));
        assert!(st.battery_soc.is_some());
    }

    #[test]
    fn bank_totals_prefer_smartshunt_over_system() {
        let mut snap = GatewaySnapshot::default();
        // Divergent system aggregate (what Cerbo systemcalc publishes).
        snap.system
            .insert("0/Dc/Battery/Voltage".into(), json!(53.56));
        snap.system
            .insert("0/Dc/Battery/Current".into(), json!(25.2));
        snap.system
            .insert("0/Dc/Battery/Power".into(), json!(1349.7));
        // Per-device batteries — bank must follow the shunt, not the sum of chains.
        snap.battery
            .insert("289/ProductName".into(), json!("SmartShunt 500A/50mV"));
        snap.battery.insert("289/Dc/0/Voltage".into(), json!(53.55));
        snap.battery.insert("289/Dc/0/Current".into(), json!(23.5));
        snap.battery.insert("289/Dc/0/Power".into(), json!(1258.4));
        snap.battery
            .insert("512/ProductName".into(), json!("JBD Battery Chain 1"));
        snap.battery.insert("512/Dc/0/Current".into(), json!(7.03));
        snap.battery.insert("512/Dc/0/Power".into(), json!(376.0));

        let st = snapshot_to_state(&snap);
        assert_eq!(st.battery_current, Some(23.5));
        assert_eq!(st.battery_power, Some(1258.4));
        assert_eq!(st.battery_voltage, Some(53.55));
        assert_eq!(st.batteries.as_ref().map(|b| b.len()), Some(2));
    }

    #[test]
    fn maps_battery_time_to_go_while_charging() {
        let mut snap = GatewaySnapshot::default();
        snap.battery
            .insert("289/ProductName".into(), json!("SmartShunt 500A/50mV"));
        snap.battery.insert("289/Dc/0/Voltage".into(), json!(53.55));
        snap.battery.insert("289/Dc/0/Current".into(), json!(23.5));
        snap.battery.insert("289/Dc/0/Power".into(), json!(1258.4));
        // ~40h 48m = 146880 seconds
        snap.battery.insert("289/TimeToGo".into(), json!(146880.0));

        let st = snapshot_to_state(&snap);
        let b = &st.batteries.as_ref().unwrap()[0];
        assert_eq!(b.state.as_deref(), Some("Charging"));
        assert_eq!(b.time_to_go.as_deref(), Some("40h 48m"));
    }

    #[test]
    fn hides_battery_time_to_go_when_idle() {
        let mut snap = GatewaySnapshot::default();
        snap.battery
            .insert("289/ProductName".into(), json!("SmartShunt 500A/50mV"));
        snap.battery.insert("289/Dc/0/Current".into(), json!(0.1));
        snap.battery.insert("289/TimeToGo".into(), json!(146880.0));

        let st = snapshot_to_state(&snap);
        let b = &st.batteries.as_ref().unwrap()[0];
        assert_eq!(b.state.as_deref(), Some("Idle"));
        assert_eq!(b.time_to_go, None);
    }

    #[test]
    fn solar_total_sums_mppt_yield_and_ac_pv() {
        let mut snap = GatewaySnapshot::default();
        snap.solarcharger
            .insert("290/Yield/Power".into(), json!(403.57));
        snap.solarcharger
            .insert("291/Yield/Power".into(), json!(594.33));
        snap.solarcharger
            .insert("292/Yield/Power".into(), json!(455.39));
        snap.pvinverter.insert("369/Ac/Power".into(), json!(235.0));
        snap.pvinverter.insert("9895/Ac/Power".into(), json!(278.0));

        let st = snapshot_to_state(&snap);
        assert_eq!(st.mppt_total, Some(403.57 + 594.33 + 455.39));
        assert_eq!(
            st.solar_total,
            Some(403.57 + 594.33 + 455.39 + 235.0 + 278.0)
        );
        assert_eq!(st.mppt_chargers.as_ref().map(|c| c.len()), Some(3));
        assert_eq!(st.pv_inverters.as_ref().map(|c| c.len()), Some(2));
    }

    /// Tray sparkline reads `solar_total` + `gt` from the same mapped state.
    #[test]
    fn maps_tray_sparkline_metrics_from_live_like_snapshot() {
        let mut snap = GatewaySnapshot::default();
        snap.system
            .insert("0/Ac/Grid/L1/Power".into(), json!(-32.0));
        snap.system.insert("0/Ac/Grid/L2/Power".into(), json!(17.0));
        snap.system
            .insert("0/Ac/Consumption/L1/Power".into(), json!(411.0));
        snap.system
            .insert("0/Ac/Consumption/L2/Power".into(), json!(17.0));
        snap.system
            .insert("0/Dc/Battery/Voltage".into(), json!(53.21));
        snap.system
            .insert("0/Dc/Battery/Current".into(), json!(-9.3));
        snap.system
            .insert("0/Dc/Battery/Power".into(), json!(-494.85));
        snap.vebus
            .insert("290/Hub4/L1/AcPowerSetpoint".into(), json!(-394));
        snap.vebus.insert("290/State".into(), json!(3));
        // Cerbo often publishes null on unused phases — must not break mapping.
        snap.vebus
            .insert("290/Ac/ActiveIn/L2/P".into(), json!(null));
        snap.solarcharger
            .insert("290/Yield/Power".into(), json!(1250.0));
        snap.solarcharger
            .insert("290/ProductName".into(), json!("SmartSolar"));
        snap.battery
            .insert("289/ProductName".into(), json!("SmartShunt 500A/50mV"));
        snap.battery.insert("289/Dc/0/Voltage".into(), json!(53.21));
        snap.battery.insert("289/Dc/0/Current".into(), json!(-9.3));
        snap.battery.insert("289/Dc/0/Power".into(), json!(-494.85));

        let st = snapshot_to_state(&snap);
        assert_eq!(st.gt, Some(-15.0));
        assert_eq!(st.tt, Some(428.0));
        assert_eq!(st.solar_total, Some(1250.0));
        assert_eq!(st.setpoint, Some(-394.0));
        assert_eq!(st.battery_power, Some(-494.85));
        // IPC must include these so the menu-bar painter (and UI) see them.
        let json = serde_json::to_value(&st).unwrap();
        assert_eq!(json.get("gt").and_then(|v| v.as_f64()), Some(-15.0));
        assert_eq!(
            json.get("solar_total").and_then(|v| v.as_f64()),
            Some(1250.0)
        );
    }
    fn complete_snapshot() -> GatewaySnapshot {
        // Synthetic values only; no captured names, VINs or household telemetry.
        serde_json::from_value(json!({
            "inverter": {
                "booleans": {"only_charging": true, "no_feed": false, "house_support": 1,
                    "charge_battery": "on", "do_not_supply_charger": "off",
                    "set_limit_to_ev_charger": false, "minimize_charging": true},
                "dry_run": false, "ess_mode": {"mode_name": "External control", "is_external": true},
                "ui_config": {"water": {"tank_instance": 21, "pump_instance": 7, "valve_instance": 8},
                    "ev": {"instance": 23, "evcharger_instance": 41},
                    "header_toggles": [{"id":"no_feed","label":"No feed","entity":"no_feed"}]},
                "grid_backup": {"enabled": true, "available": true, "service": "test.grid",
                    "name": "Test submeter", "power": 123.0, "measurement_time": 1700000000.0, "age_seconds": 7.0},
                "grid_using_backup": false,
                "setpoint_override": {"value": 100, "last_error": null, "request_id": null},
                "daily_stats": {"grid_kwh": 2.0}, "solar_forecast": {"today_kwh": 3.0}
            },
            "capabilities": {"water_mode": true, "setpoint_override": true},
            "tank": {"21/Level": 0.5, "21/Connected": 1, "21/CustomName": "Test tank"},
            "pump": {"7/State": 1, "7/Mode": 0, "7/Connected": 1,
                "8/State": 0, "8/Mode": 2, "8/Connected": 1,
                "1/State": 0, "1/Mode": 1, "2/State": 1, "2/Mode": 1},
            "ev": {"23/Soc": 0, "23/Ac/Power": 0, "23/Connected": 1},
            "evcharger": {"41/Ac/Power": 12, "41/Connected": 1}
        })).unwrap()
    }

    #[test]
    fn maps_controller_and_native_ev_water_from_one_snapshot() {
        let state = snapshot_to_state(&complete_snapshot());
        assert_eq!(state.gateway_snapshot, Some(true));
        assert_eq!(state.booleans.as_ref().unwrap().len(), 7);
        assert!(state.booleans.as_ref().unwrap()["only_charging"]);
        assert!(!state.booleans.as_ref().unwrap()["do_not_supply_charger"]);
        assert_eq!(state.dry_run, Some(false));
        assert_eq!(state.ess_mode.as_ref().unwrap().is_external, Some(true));
        assert_eq!(
            state
                .ui_config
                .as_ref()
                .unwrap()
                .header_toggles
                .as_ref()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(state.water_level, Some(0.5));
        assert_eq!(state.pump_switch, Some(true));
        assert_eq!(state.water_valve, Some(false));
        assert_eq!(state.water_pump_mode, Some(0));
        assert_eq!(state.water_valve_mode, Some(2));
        assert_eq!(state.car_soc, Some(0.0));
        assert_eq!(state.car_charging_power, Some(0.0));
        assert_eq!(state.ev_charging_power, Some(12.0));
        assert!(state.ev_present && state.evcharger_present);
        assert_eq!(state.discovered_water_ev.as_ref().unwrap().len(), 7);
        assert_eq!(state.grid_backup.as_ref().unwrap().power, Some(123.0));
        assert_eq!(state.grid_using_backup, Some(false));
        assert_eq!(state.setpoint_override.as_ref().unwrap().value, Some(100));
    }

    #[test]
    fn configured_instances_win_over_controller_and_missing_choices_do_not_fallback() {
        let snap = complete_snapshot();
        let state = snapshot_to_state_with_instances(
            &snap,
            GatewayInstances {
                water_tank: Some(99),
                water_pump: Some(1),
                water_valve: Some(2),
                ev: Some(99),
                evcharger: Some(99),
            },
        );
        assert_eq!(state.water_level, None);
        assert_eq!(state.pump_switch, Some(false));
        assert_eq!(state.water_valve, Some(true));
        assert_eq!(state.water_pump_mode, Some(1));
        assert_eq!(state.car_soc, None);
        assert_eq!(state.car_charging_power, None);
        assert_eq!(state.ev_charging_power, None);
        assert!(!state.ev_present && !state.evcharger_present);
    }

    #[test]
    fn disconnected_or_null_native_devices_clear_previous_values() {
        for connected in [json!(0), Value::Null] {
            let mut snap = complete_snapshot();
            for (map, instance) in [
                (&mut snap.tank, 21),
                (&mut snap.pump, 7),
                (&mut snap.ev, 23),
                (&mut snap.evcharger, 41),
            ] {
                map.insert(format!("{instance}/Connected"), connected.clone());
            }
            let state = snapshot_to_state(&snap);
            assert_eq!(state.water_level, None);
            assert_eq!(state.pump_switch, None);
            assert_eq!(state.water_pump_mode, None);
            assert_eq!(state.car_soc, None);
            assert_eq!(state.car_charging_power, None);
            assert_eq!(state.ev_charging_power, None);
            assert!(!state.ev_present && !state.evcharger_present);
        }
    }

    #[test]
    fn null_native_leaves_remain_unknown_and_zero_is_preserved() {
        let mut snap = complete_snapshot();
        snap.pump.insert("7/State".into(), Value::Null);
        snap.pump.insert("7/Mode".into(), json!(1.5));
        snap.ev.insert("23/Soc".into(), Value::Null);
        let state = snapshot_to_state(&snap);
        assert_eq!(state.pump_switch, None);
        assert_eq!(state.water_pump_mode, None);
        assert_eq!(state.car_soc, None);
        assert_eq!(state.car_charging_power, Some(0.0));
    }

    #[test]
    fn expired_controller_clears_flags_and_submeter_but_keeps_native_devices() {
        let mut snap = complete_snapshot();
        snap.inverter = None;
        let state = snapshot_to_state_with_instances(
            &snap,
            GatewayInstances {
                ev: Some(23),
                water_pump: Some(7),
                ..GatewayInstances::default()
            },
        );
        assert!(state.booleans.as_ref().unwrap().is_empty());
        assert_eq!(state.dry_run, None);
        assert!(state.ess_mode.is_none());
        assert!(state.grid_backup.is_none());
        assert_eq!(state.grid_backup_observed_at, None);
        assert_eq!(state.grid_using_backup, Some(false));
        assert_eq!(state.car_soc, Some(0.0));
        assert_eq!(state.pump_switch, Some(true));
    }

    #[test]
    fn repeated_backup_snapshots_never_renew_measurement_timestamp() {
        let snap = complete_snapshot();
        let first = snapshot_to_state(&snap);
        let repeated = snapshot_to_state(&snap);
        assert_eq!(first.grid_backup_observed_at, Some(1700000000.0));
        assert_eq!(
            first.grid_backup_observed_at,
            repeated.grid_backup_observed_at
        );
        let mut absent_time = snap;
        absent_time.inverter.as_mut().unwrap()["grid_backup"]["measurement_time"] = Value::Null;
        assert_eq!(
            snapshot_to_state(&absent_time).grid_backup_observed_at,
            None
        );
    }

    #[test]
    fn controller_never_overwrites_cerbo_owned_power_soc_or_ev() {
        let mut snap = complete_snapshot();
        let controller = snap.inverter.as_mut().unwrap();
        for key in [
            "gt",
            "g1",
            "battery_soc",
            "battery_power",
            "car_soc",
            "water_level",
        ] {
            controller.insert(key.into(), json!(9999));
        }
        snap.system.insert("0/Ac/Grid/L1/Power".into(), json!(45));
        let state = snapshot_to_state(&snap);
        assert_eq!(state.gt, Some(45.0));
        assert_eq!(state.battery_soc, None);
        assert_eq!(state.battery_power, None);
        assert_eq!(state.car_soc, Some(0.0));
        assert_eq!(state.water_level, Some(0.5));
    }

    #[test]
    fn poll_failure_invalidates_controls_without_erasing_unrelated_state() {
        let mut state = snapshot_to_state(&complete_snapshot());
        state.ha_direct_connected = Some(true);
        state.washer_power = Some(true);
        state.gt = Some(42.0);
        invalidate_gateway_controls(&mut state);
        assert!(state.booleans.as_ref().unwrap().is_empty());
        assert!(state.grid_backup.is_none());
        assert_eq!(state.car_soc, None);
        assert_eq!(state.pump_switch, None);
        assert_eq!(state.ha_direct_connected, Some(true));
        assert_eq!(state.washer_power, Some(true));
        assert_eq!(state.gt, Some(42.0));
    }

    #[test]
    fn gateway_poll_provenance_distinguishes_cached_repeats_from_recovery() {
        let snapshot = complete_snapshot();
        let mut state = snapshot_to_state(&snapshot);
        assert_eq!(
            serde_json::to_value(&state).unwrap()["cached_snapshot"],
            false
        );
        state.gt = Some(42.0);
        invalidate_gateway_controls(&mut state);
        let cached = serde_json::to_value(&state).unwrap();
        assert_eq!(cached["gateway_snapshot"], true);
        assert_eq!(cached["cached_snapshot"], true);
        assert_eq!(cached["gt"], 42.0);
        assert_eq!(cached["pump_switch"], Value::Null);
        invalidate_gateway_controls(&mut state);
        assert_eq!(serde_json::to_value(&state).unwrap(), cached);

        let recovered = snapshot_to_state(&snapshot);
        assert_eq!(
            serde_json::to_value(recovered).unwrap()["cached_snapshot"],
            false
        );
        // This marker describes the backend's event, never upstream JSON input.
        let upstream: InverterState = serde_json::from_value(cached).unwrap();
        assert!(serde_json::to_value(upstream)
            .unwrap()
            .get("cached_snapshot")
            .is_none());
        assert!(serde_json::to_value(InverterState::default())
            .unwrap()
            .get("cached_snapshot")
            .is_none());
    }

    #[tokio::test]
    async fn stopped_gateway_auth_rejects_snapshot_and_commands_before_network() {
        let client = idle_test_client();
        let auth = client.http_auth();
        assert!(auth.ensure_active().is_ok());
        client.stop();
        assert!(command_snapshot(&auth)
            .await
            .unwrap_err()
            .contains("connection changed"));
        assert!(send_command(&auth, "ess_mode", json!({}))
            .await
            .unwrap_err()
            .contains("connection changed"));
    }
    #[test]
    fn legacy_charger_soc_only_applies_when_native_ev_namespace_is_absent() {
        let mut snap = complete_snapshot();
        snap.evcharger.insert("41/Soc".into(), json!(55));
        let missing_selected = snapshot_to_state_with_instances(
            &snap,
            GatewayInstances {
                ev: Some(99),
                ..Default::default()
            },
        );
        assert_eq!(missing_selected.car_soc, None);
        assert!(!missing_selected.ev_present);
        for connected in [json!(0), Value::Null] {
            snap.ev.insert("23/Connected".into(), connected);
            let state = snapshot_to_state(&snap);
            assert_eq!(state.car_soc, None);
            assert!(!state.ev_present);
            assert!(state.evcharger_present);
        }
        // An explicit EV tombstone also must not resurrect another SOC.
        snap.ev = HashMap::from([("23/Soc".into(), Value::Null)]);
        assert_eq!(snapshot_to_state(&snap).car_soc, None);
        // Older producers have no ev/<instance> namespace at all.
        snap.ev.clear();
        assert_eq!(snapshot_to_state(&snap).car_soc, Some(55.0));
    }
    #[test]
    fn water_display_matches_strict_gateway_command_target_guards() {
        for connected in [json!(true), json!("1"), json!(1.0), Value::Null, json!(0)] {
            let mut snap = complete_snapshot();
            snap.pump.insert("7/Connected".into(), connected);
            let state = snapshot_to_state(&snap);
            assert_eq!(state.pump_switch, None);
            assert_eq!(state.water_pump_mode, None);
        }
        for mode in [
            json!(true),
            json!("1"),
            json!(1.0),
            Value::Null,
            json!(3),
            json!(-1),
        ] {
            let mut snap = complete_snapshot();
            snap.pump.insert("7/Mode".into(), mode);
            let state = snapshot_to_state(&snap);
            assert_eq!(state.pump_switch, None);
            assert_eq!(state.water_pump_mode, None);
        }
        let mut snap = complete_snapshot();
        snap.pump.remove("7/Connected");
        assert_eq!(snapshot_to_state(&snap).pump_switch, Some(true));
        snap.capabilities.clear();
        let state = snapshot_to_state(&snap);
        assert_eq!(state.pump_switch, None);
        assert_eq!(state.water_pump_mode, None);
        assert_eq!(state.water_level, Some(0.5));
    }
    #[test]
    fn override_status_distinguishes_unknown_from_confirmed_inactive_and_keeps_ack() {
        let mut snap = complete_snapshot();
        assert_eq!(snapshot_override_status(&snap).unwrap().value, Some(100));
        snap.inverter.as_mut().unwrap()["setpoint_override"] = json!({
            "value": null, "last_error": null, "request_id": "synthetic-stop-ack"
        });
        let status = snapshot_override_status(&snap).unwrap();
        assert_eq!(status.value, None);
        assert_eq!(status.request_id.as_deref(), Some("synthetic-stop-ack"));
        assert!(snapshot_to_state(&snap).setpoint_override.is_some());
        for invalid in [
            Value::Null,
            json!({}),
            json!({"value":true}),
            json!({"value":1.0}),
            json!({"value":"100"}),
            json!({"value":2147483648_u64}),
            json!({"value":null,"request_id":3}),
            json!({"value":null,"last_error":null}),
            json!({"value":null,"request_id":null}),
        ] {
            snap.inverter.as_mut().unwrap()["setpoint_override"] = invalid;
            assert!(snapshot_override_status(&snap).is_err());
            assert!(snapshot_to_state(&snap).setpoint_override.is_none());
        }
        snap = complete_snapshot();
        snap.capabilities.remove("setpoint_override");
        assert!(snapshot_override_status(&snap).is_err());
        snap = complete_snapshot();
        snap.inverter = None;
        assert!(snapshot_override_status(&snap).is_err());
    }
}
