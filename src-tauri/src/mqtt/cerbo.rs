//! Transport-independent Cerbo GX topic decoding, instance discovery and state
//! overlays. These helpers keep MQTT wire names and precedence rules unchanged.
use super::{
    inverter_state_name, voltage_soc, Battery, CerboDevices, DeviceIdentity, DiscoveredInstance,
    EvCache, EvField, InverterState, MpptCharger, MqttClient, NamedDevice, PvInverter,
    TrackedEntry,
};
use std::collections::{BTreeMap, HashMap};

impl MqttClient {
    /// Parse a dbus-pump water topic:
    /// N/<portal>/tank/<i>/Level|CustomName|ProductName,
    /// N/<portal>/pump/<i>/State|Mode|CustomName|ProductName
    /// -> Some((kind, instance, path)).
    pub(super) fn parse_water_topic(topic: &str) -> Option<(&str, u32, &str)> {
        let rest = topic.strip_prefix("N/")?;
        let mut it = rest.split('/');
        let _portal = it.next()?;
        let kind = it.next()?;
        let inst: u32 = it.next()?.parse().ok()?;
        let path = it.next()?;
        match (kind, path) {
            ("tank", "Level")
            | ("tank", "CustomName")
            | ("tank", "ProductName")
            | ("pump", "State")
            | ("pump", "Mode")
            | ("pump", "CustomName")
            | ("pump", "ProductName") => Some((kind, inst, path)),
            _ => None,
        }
    }

    /// Parse dbus-ev / dbus-evcharger topic:
    /// N/<portal>/ev/<i>/Soc, N/<portal>/ev/<i>/Ac/Power,
    /// N/<portal>/evcharger/<i>/Ac/Power -> Some((kind, instance, path)).
    pub(super) fn parse_ev_topic(topic: &str) -> Option<(&str, u32, &str)> {
        // "N/portal/ev/22/Soc"        -> 5 parts: portal, ev, 22, Soc
        // "N/portal/ev/22/Ac/Power"   -> 6 parts: portal, ev, 22, Ac, Power
        // (the slash inside "Ac/Power" is part of the path, not a separator).
        let rest = topic.strip_prefix("N/")?;
        let mut it = rest.splitn(6, '/');
        let _portal = it.next()?;
        let kind = it.next()?;
        let inst: u32 = it.next()?.parse().ok()?;
        let p1 = it.next()?;
        let path = match it.next() {
            Some(p2) if p1 == "Ac" && p2 == "Power" => "Ac/Power",
            Some(_) => return None,
            None => p1,
        };
        match (kind, path) {
            ("ev", "Soc")
            | ("ev", "Ac/Power")
            | ("ev", "CustomName")
            | ("ev", "ProductName")
            | ("evcharger", "Soc")
            | ("evcharger", "Ac/Power")
            | ("evcharger", "CustomName")
            | ("evcharger", "ProductName") => Some((kind, inst, path)),
            _ => None,
        }
    }

    /// Parse a Victron acload topic:
    /// N/<portal>/acload/<instance>/Ac/Power|CustomName|ProductName
    /// -> Some((instance, path)).
    pub(super) fn parse_acload_topic(topic: &str) -> Option<(u32, &str)> {
        let rest = topic.strip_prefix("N/")?;
        let mut it = rest.splitn(6, '/');
        let _portal = it.next()?;
        if it.next()? != "acload" {
            return None;
        }
        let inst: u32 = it.next()?.parse().ok()?;
        let p1 = it.next()?;
        let path = match it.next() {
            Some(p2) if p1 == "Ac" && p2 == "Power" => "Ac/Power",
            Some(_) => return None,
            None => p1,
        };
        match path {
            "Ac/Power" | "CustomName" | "ProductName" => Some((inst, path)),
            _ => None,
        }
    }

    /// Apply one acload MQTT message into CerboDevices.acloads.
    /// Returns true when the message mapped to a known path.
    pub(super) fn apply_acload_message(
        devices: &mut CerboDevices,
        inst: u32,
        path: &str,
        payload: &str,
    ) -> bool {
        let entry = devices.acloads.entry(inst).or_default();
        entry.touch();
        let a = &mut entry.data;
        match path {
            "Ac/Power" => {
                a.power = Self::parse_cerbo_value(payload);
                true
            }
            "CustomName" => {
                if let Some(n) = Self::parse_cerbo_name(payload) {
                    a.custom_name = Some(n);
                }
                true
            }
            "ProductName" => {
                // CustomName wins; only fill product when custom is empty.
                if let Some(n) = Self::parse_cerbo_name(payload) {
                    a.product_name = Some(n);
                }
                true
            }
            _ => false,
        }
    }

    /// Record tank/pump/ev/evcharger instance (+ optional name) in CerboDevices.
    pub(super) fn apply_named_discovery(
        devices: &mut CerboDevices,
        kind: &str,
        inst: u32,
        path: &str,
        payload: &str,
    ) {
        let map = match kind {
            "tank" => &mut devices.tanks,
            "pump" => &mut devices.pumps,
            "ev" => &mut devices.evs,
            "evcharger" => &mut devices.evchargers,
            _ => return,
        };
        let entry = map.entry(inst).or_default();
        entry.touch();
        match path {
            "CustomName" => {
                if let Some(n) = Self::parse_cerbo_name(payload) {
                    entry.data.custom_name = Some(n);
                }
            }
            "ProductName" => {
                if let Some(n) = Self::parse_cerbo_name(payload) {
                    entry.data.product_name = Some(n);
                }
            }
            // Level / State / Soc / Ac/Power — presence alone is enough.
            _ => {}
        }
    }

    /// Prefer `preferred` when discovery is empty or still contains it;
    /// otherwise the lowest discovered instance id.
    pub(super) fn resolve_active_instance(
        preferred: Option<u32>,
        discovered: &BTreeMap<u32, TrackedEntry<NamedDevice>>,
    ) -> Option<u32> {
        if let Some(p) = preferred {
            if discovered.is_empty() || discovered.contains_key(&p) {
                return Some(p);
            }
        }
        discovered.keys().next().copied()
    }

    /// Cerbo flashmq JSON envelope: {"value": <number>}.
    pub(super) fn parse_cerbo_value(payload: &str) -> Option<f64> {
        serde_json::from_str::<serde_json::Value>(payload)
            .ok()?
            .get("value")?
            .as_f64()
    }

    /// Missing/malformed envelopes are not source invalidation. An explicit
    /// {"value": null} is authoritative and must clear the previous phase.
    fn parse_cerbo_grid_sample(payload: &str) -> Option<Option<f64>> {
        let envelope = serde_json::from_str::<serde_json::Value>(payload).ok()?;
        match envelope.get("value")? {
            serde_json::Value::Null => Some(None),
            value => value.as_f64().map(Some),
        }
    }

    /// /TimeToGo arrives in seconds (null when idle -> parse_cerbo_value
    /// already yields None). Format like the inverter-control daemon does.
    pub(crate) fn format_time_to_go(secs: f64) -> Option<String> {
        let s = secs as u64;
        if s == 0 || s >= 86_400 * 14 {
            return None;
        }
        let h = s / 3600;
        let m = (s % 3600) / 60;
        Some(if h > 0 {
            format!("{h}h {m:02}m")
        } else {
            format!("{m}m")
        })
    }

    /// Charging/Discharging/Idle from current sign, ±0.5 A deadband —
    /// mirrors inverter_control's _battery_state.
    pub(crate) fn state_from_current(amps: f64) -> String {
        if amps > 0.5 {
            "Charging".to_string()
        } else if amps < -0.5 {
            "Discharging".to_string()
        } else {
            "Idle".to_string()
        }
    }

    /// Cerbo ProductName arrives as {"value": "<name>"}.
    pub(super) fn parse_cerbo_name(payload: &str) -> Option<String> {
        let s = serde_json::from_str::<serde_json::Value>(payload)
            .ok()
            .and_then(|v| v.get("value").and_then(|x| x.as_str()).map(String::from))
            .unwrap_or_else(|| payload.trim().to_string());
        (!s.is_empty()).then_some(s)
    }
    /// Cell id may be a string ("7") or number (7) in {"value": ...}.
    /// Do not reuse parse_cerbo_name: numeric values fall back to the raw
    /// JSON payload there.
    pub(super) fn parse_cerbo_cell_id(payload: &str) -> Option<String> {
        let v = serde_json::from_str::<serde_json::Value>(payload).ok()?;
        let x = v.get("value")?;
        if x.is_null() {
            return None;
        }
        if let Some(s) = x.as_str() {
            let t = s.trim();
            return (!t.is_empty()).then(|| t.to_string());
        }
        if let Some(n) = x.as_i64().or_else(|| x.as_u64().map(|n| n as i64)) {
            return Some(n.to_string());
        }
        if let Some(n) = x.as_f64() {
            if n.fract().abs() < f64::EPSILON {
                return Some(format!("{}", n as i64));
            }
            return Some(format!("{n}"));
        }
        None
    }

    /// Parse N/<portal>/<kind>/<instance>/<path> for GX devices we discover
    /// ourselves (battery, solarcharger, pvinverter, vebus). Other kinds are left to
    /// their own handlers (tank/pump) or ignored.
    pub(super) fn parse_device_topic(topic: &str) -> Option<(&str, u32, &str)> {
        let rest = topic.strip_prefix("N/")?;
        let (portal, rest) = rest.split_once('/')?;
        if portal.is_empty() {
            return None;
        }
        let (kind, rest) = rest.split_once('/')?;
        if kind != "battery"
            && kind != "solarcharger"
            && kind != "pvinverter"
            && kind != "vebus"
            && kind != "system"
        {
            return None;
        }
        let (inst, path) = rest.split_once('/')?;
        Some((kind, inst.parse().ok()?, path))
    }

    /// Apply one GX device message to the discovered-device maps.
    /// Returns true when the message mapped to a known value path.
    pub(super) fn apply_device_message(
        devices: &mut CerboDevices,
        kind: &str,
        inst: u32,
        path: &str,
        payload: &str,
    ) -> bool {
        let val = Self::parse_cerbo_value(payload);
        match kind {
            "battery" => {
                let entry = devices.batteries.entry(inst).or_default();
                entry.touch();
                let b = &mut entry.data;
                if b.instance.is_none() {
                    b.instance = Some(inst);
                }
                match path {
                    "Soc" => b.soc = val,
                    "Dc/0/Voltage" => b.voltage = val,
                    // The GX MQTT bridge publishes no battery /State — derive
                    // it from current sign (±0.5 A, same as the daemon).
                    "Dc/0/Current" => {
                        b.current = val;
                        if let Some(a) = val {
                            b.state = Some(Self::state_from_current(a));
                        }
                    }
                    "Dc/0/Power" => b.power = val,
                    "ProductName" => b.name = Self::parse_cerbo_name(payload),
                    "CustomName" => {
                        // Prefer custom label when present (matches DeviceName).
                        if let Some(n) = Self::parse_cerbo_name(payload) {
                            b.name = Some(n);
                        }
                    }
                    "Serial" => b.serial = Self::parse_cerbo_name(payload),
                    "TimeToGo" => b.time_to_go = val.and_then(Self::format_time_to_go),
                    // Cell extremes for High/Low (cell) voltage banner enrichment.
                    // Platform Notifications do not carry these — only battery MQTT.
                    "System/MaxCellVoltage" => b.max_cell_voltage = val,
                    "System/MinCellVoltage" => b.min_cell_voltage = val,
                    "System/MaxVoltageCellId" => {
                        b.max_voltage_cell_id = Self::parse_cerbo_cell_id(payload)
                    }
                    "System/MinVoltageCellId" => {
                        b.min_voltage_cell_id = Self::parse_cerbo_cell_id(payload)
                    }
                    _ => return false,
                }
                true
            }
            "solarcharger" => {
                let entry = devices.chargers.entry(inst).or_default();
                entry.touch();
                let m = &mut entry.data;
                match path {
                    "Pv/V" => m.pv_voltage = val,
                    "Dc/0/Current" => m.current = val,
                    "Yield/Power" => m.power = val,
                    "ProductName" => m.name = Self::parse_cerbo_name(payload),
                    "Serial" => m.serial = Self::parse_cerbo_name(payload),
                    _ => return false,
                }
                true
            }
            "pvinverter" => {
                let entry = devices.pv_inverters.entry(inst).or_default();
                entry.touch();
                let p = &mut entry.data;
                // Store instance on first discovery so the UI can distinguish
                // devices with identical names across different Cerbo instances.
                if p.instance.is_none() {
                    p.instance = Some(inst);
                }
                match path {
                    // Ac/Power is the device total; L1 Power equals it on
                    // single-phase units but is accepted as a fallback.
                    "Ac/Power" | "Ac/L1/Power" | "Ac/L2/Power" => p.power = val,
                    "Ac/L1/Voltage" | "Ac/L2/Voltage" => p.voltage = val,
                    "Ac/L1/Current" | "Ac/L2/Current" => p.current = val,
                    "ProductName" => p.name = Self::parse_cerbo_name(payload),
                    "Serial" => p.serial = Self::parse_cerbo_name(payload),
                    _ => return false,
                }
                true
            }
            "vebus" => {
                let entry = devices.vebus.entry(inst).or_default();
                entry.touch();
                let v = &mut entry.data;
                match path {
                    "Ac/L1/Power" => v.l1_power = val,
                    "Ac/L2/Power" => v.l2_power = val,
                    "Ac/ActiveIn/L1/Power" => v.l1_power = val, // fallback grid-in
                    "Ac/ActiveIn/L2/Power" => v.l2_power = val,
                    "Ac/Out/P" | "Ac/Power" => v.ac_power = val,
                    "Hub4/L1/AcPowerSetpoint" => v.setpoint = val,
                    "State" => {
                        if let Some(code) = val {
                            v.inverter_state = Some(inverter_state_name(code as u32));
                        }
                    }
                    _ => return false,
                }
                true
            }
            "system" => {
                let entry = devices.system.entry(inst).or_default();
                entry.touch();
                let s = &mut entry.data;
                match path {
                    "Ac/Grid/L1/Power" => {
                        let Some(sample) = Self::parse_cerbo_grid_sample(payload) else {
                            return false;
                        };
                        s.g1 = sample;
                        s.g1_seen = true;
                    }
                    "Ac/Grid/L2/Power" => {
                        let Some(sample) = Self::parse_cerbo_grid_sample(payload) else {
                            return false;
                        };
                        s.g2 = sample;
                        s.g2_seen = true;
                    }
                    "Ac/Consumption/L1/Power" => s.t1 = val,
                    "Ac/Consumption/L2/Power" => s.t2 = val,
                    _ => return false,
                }
                true
            }
            _ => false,
        }
    }

    /// Apply a dbus-ev / dbus-evcharger message to state.
    /// Returns true if the state was modified.
    /// Each side matches its own configured instance; either may be None.
    /// Apply a dbus-ev / dbus-evcharger message to state.
    /// Updates `ev_cache` (throttled, 8 s per field) and state simultaneously.
    /// Returns the field that was updated, or None if no state change occurred.
    pub(super) fn apply_ev_message(
        st: &mut InverterState,
        cache: &mut EvCache,
        kind: &str,
        inst: u32,
        path: &str,
        value: f64,
        ev_instances: &Option<(Option<u32>, Option<u32>)>,
    ) -> Option<EvField> {
        let ev_i = ev_instances.as_ref().and_then(|(e, _)| *e);
        let evc_i = ev_instances.as_ref().and_then(|(_, c)| *c);
        let matched = match kind {
            "ev" => ev_i == Some(inst),
            "evcharger" => evc_i == Some(inst),
            _ => false,
        };
        if !matched {
            return None;
        }
        // Mark presence so the EV tile stays visible even when SOC/power are 0.
        cache.set_presence(kind);
        // When cache.update returns false (TTL or negative SoC), still re-apply the
        // cached value to st so process_state_update's clone doesn't see None.
        match (kind, path) {
            ("ev", "Soc") => {
                if cache.update(EvField::CarSoc, value) {
                    st.car_soc = Some(value);
                    Some(EvField::CarSoc)
                } else if let Some((v, _)) = cache.car_soc {
                    st.car_soc = Some(v);
                    Some(EvField::CarSoc)
                } else {
                    None
                }
            }
            ("ev", "Ac/Power") => {
                if cache.update(EvField::CarChargingPower, value) {
                    st.car_charging_power = Some(value);
                    Some(EvField::CarChargingPower)
                } else if let Some((v, _)) = cache.car_charging_power {
                    st.car_charging_power = Some(v);
                    Some(EvField::CarChargingPower)
                } else {
                    None
                }
            }
            ("evcharger", "Ac/Power") => {
                if cache.update(EvField::EvChargingPower, value) {
                    st.ev_charging_power = Some(value);
                    Some(EvField::EvChargingPower)
                } else if let Some((v, _)) = cache.ev_charging_power {
                    st.ev_charging_power = Some(v);
                    Some(EvField::EvChargingPower)
                } else {
                    None
                }
            }
            ("evcharger", "Soc") => {
                // dbus-ev originally published Soc under com.victronenergy.evcharger;
                // some GX installs still use that name after the .ev rename.
                if cache.update(EvField::CarSoc, value) {
                    st.car_soc = Some(value);
                    Some(EvField::CarSoc)
                } else if let Some((v, _)) = cache.car_soc {
                    st.car_soc = Some(v);
                    Some(EvField::CarSoc)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// The SmartShunt is the ground-truth meter for the whole battery bank:
    /// the mqtt chains report per-string BMS views and virtual_chain is
    /// derived from the shunt itself (shunt - chain1 - chain2), so summing
    /// every battery service double-counts. The shunt's D-Bus/MQTT instance
    /// can change across GX reboots, so match by product name, not instance.
    pub(super) fn find_shunt(batteries: &[Battery]) -> Option<&Battery> {
        batteries.iter().find(|b| {
            b.name
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .contains("shunt")
        })
    }

    /// Identical units ship one shared ProductName ("SmartSolar Charger MPPT
    /// 100/20 48V" x N), which reads as the same tile repeated. Suffix every
    /// duplicate with its serial tail (or broker instance when no serial is
    /// known) so each tile stays distinguishable; unique names pass through.
    pub(super) fn disambiguate_names<T: DeviceIdentity>(items: &mut [T], instances: &[u32]) {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for it in items.iter() {
            if let Some(n) = it.display_name() {
                *counts.entry(n.to_string()).or_insert(0) += 1;
            }
        }
        for (it, inst) in items.iter_mut().zip(instances) {
            let Some(name) = it.display_name().map(String::from) else {
                continue;
            };
            if counts.get(&name).copied().unwrap_or(1) <= 1 {
                continue;
            }
            let tail = match it.serial() {
                Some(s) if s.len() >= 4 && s.is_char_boundary(s.len() - 4) => {
                    s[s.len() - 4..].to_string()
                }
                _ => format!("#{}", inst),
            };
            *it.name_slot() = Some(format!("{} · {}", name, tail));
        }
    }

    /// Overlay discovered GX devices onto a state snapshot. When anything was
    /// found on the broker it wins over the daemon-provided arrays so the UI
    /// stays correct even with inverter-control down; empty maps leave the
    /// daemon data untouched.
    pub(super) fn apply_cerbo_to_state(devices: &CerboDevices, st: &mut InverterState) {
        if !devices.batteries.is_empty() {
            let mut batteries: Vec<Battery> =
                devices.batteries.values().map(|e| e.data.clone()).collect();
            // Time-to-go is only meaningful while charging/discharging; hide
            // the stale value otherwise (daemon does the same gating).
            for b in &mut batteries {
                if !matches!(b.state.as_deref(), Some("Charging") | Some("Discharging")) {
                    b.time_to_go = None;
                }
            }
            // Bank totals come from the shunt alone; without it, leave the
            // daemon's system-aggregate values untouched rather than summing
            // overlapping battery services.
            if let Some(shunt) = Self::find_shunt(&batteries) {
                // Bank % is computed from pack voltage (HA "Battery %" paradigm):
                // the shunt's own SoC counter reads a bogus 100% while charging.
                // Computed here, not in the daemon, so it works with inverter-control down.
                st.battery_soc = shunt.voltage.map(voltage_soc).or(st.battery_soc);
                st.battery_voltage = shunt.voltage.or(st.battery_voltage);
                st.battery_current = Some(shunt.current.unwrap_or(0.0));
                st.battery_power = Some(shunt.power.unwrap_or(0.0));
            }
            Self::disambiguate_names(
                &mut batteries,
                &devices.batteries.keys().copied().collect::<Vec<_>>(),
            );
            st.batteries = Some(batteries);
        }
        if !devices.chargers.is_empty() {
            let mut chargers: Vec<MpptCharger> =
                devices.chargers.values().map(|e| e.data.clone()).collect();
            Self::disambiguate_names(
                &mut chargers,
                &devices.chargers.keys().copied().collect::<Vec<_>>(),
            );
            st.mppt_total = Some(devices.chargers.values().filter_map(|e| e.data.power).sum());
            st.mppt_chargers = Some(chargers);
        }
        if !devices.pv_inverters.is_empty() {
            let mut pv_inverters: Vec<PvInverter> = devices
                .pv_inverters
                .values()
                .map(|e| e.data.clone())
                .collect();
            Self::disambiguate_names(
                &mut pv_inverters,
                &devices.pv_inverters.keys().copied().collect::<Vec<_>>(),
            );
            st.pv_inverters = Some(pv_inverters);
        }
        // Chart/stat "solar total": prefer Cerbo maps; if only one side is on
        // Cerbo, keep the other side from existing state (daemon fallback).
        if devices.owns_solar() {
            let mppt = if devices.owns_chargers() {
                devices.chargers.values().filter_map(|e| e.data.power).sum()
            } else {
                st.mppt_total
                    .or_else(|| st.mppt_individual.as_ref().map(|v| v.iter().sum()))
                    .unwrap_or(0.0)
            };
            let pv = if devices.owns_pv() {
                devices
                    .pv_inverters
                    .values()
                    .filter_map(|e| e.data.power)
                    .sum()
            } else if let Some(ref invs) = st.pv_inverters {
                invs.iter().filter_map(|p| p.power).sum()
            } else {
                st.pv_inverter_individual
                    .as_ref()
                    .map(|v| v.iter().sum())
                    .unwrap_or(0.0)
            };
            st.solar_total = Some(mppt + pv);
        }
        // Prefer systemcalc grid/consumption (same paths as inverter-control).
        if let Some(entry) = devices.system.values().next() {
            let s = &entry.data;
            if s.g1_seen {
                st.g1 = s.g1;
                st.grid_l1_available = Some(s.g1.is_some());
            }
            if s.g2_seen {
                st.g2 = s.g2;
                st.grid_l2_available = Some(s.g2.is_some());
            }
            if let Some(t1) = s.t1 {
                st.t1 = Some(t1);
            }
            if let Some(t2) = s.t2 {
                st.t2 = Some(t2);
            }
            match (st.t1, st.t2) {
                (Some(t1), Some(t2)) => st.tt = Some(t1 + t2),
                (Some(t1), None) => st.tt = Some(t1),
                (None, Some(t2)) => st.tt = Some(t2),
                _ => {}
            }
        }

        if let Some(entry) = devices.vebus.values().next() {
            let v = &entry.data;
            // Grid from vebus only when systemcalc has not filled it.
            if st.g1.is_none() && st.grid_l1_available != Some(false) {
                if let Some(l1) = v.l1_power {
                    st.g1 = Some(l1);
                    st.grid_l1_available = Some(true);
                }
            }
            if st.g2.is_none() && st.grid_l2_available != Some(false) {
                if let Some(l2) = v.l2_power {
                    st.g2 = Some(l2);
                    st.grid_l2_available = Some(true);
                }
            }
            if st.gt.is_none() {
                if let (Some(g1), Some(g2)) = (st.g1, st.g2) {
                    st.gt = Some(g1 + g2);
                } else if let Some(ac_power) = v.ac_power {
                    st.gt = Some(ac_power);
                }
            }
            // Live ESS setpoint + charger mode — chart/tile even without daemon.
            if let Some(sp) = v.setpoint {
                st.setpoint = Some(sp);
            }
            if let Some(ref mode) = v.inverter_state {
                st.inverter_state = Some(mode.clone());
            }
        }
        if st.grid_l1_available.is_some() || st.grid_l2_available.is_some() {
            // Sum only presently available phases. In particular, a current
            // single-phase fallback must not include a cached meter L2 value.
            st.gt = match (st.g1, st.g2) {
                (Some(a), Some(b)) => Some(a + b),
                (Some(a), None) | (None, Some(a)) => Some(a),
                (None, None) => None,
            };
        }
        if !devices.acloads.is_empty() {
            // Stable instance-id keys for watts; names live in load_names so
            // power updates never rekey the map back to bare ids.
            let mut loads = std::collections::HashMap::new();
            let mut names = std::collections::HashMap::new();
            for (inst, entry) in &devices.acloads {
                let id = inst.to_string();
                if let Some(p) = entry.data.power {
                    loads.insert(id.clone(), p);
                }
                if let Some(n) = entry.data.display_name() {
                    names.insert(id, n.to_string());
                }
            }
            st.loads = Some(loads);
            // Preserve previously known names for instances that briefly lose
            // CustomName publishes; only overwrite/extend, never wipe known names
            // when the overlay has a partial name set.
            let dest = st.load_names.get_or_insert_with(Default::default);
            for (id, n) in names {
                dest.insert(id, n);
            }
        }
        // Water & EV instance inventory for Config (stable instance order).
        let mut discovered = Vec::new();
        for (inst, entry) in &devices.tanks {
            discovered.push(DiscoveredInstance {
                instance: *inst,
                kind: "tank".to_string(),
                name: entry.data.display_name().map(str::to_string),
            });
        }
        for (inst, entry) in &devices.pumps {
            discovered.push(DiscoveredInstance {
                instance: *inst,
                kind: "pump".to_string(),
                name: entry.data.display_name().map(str::to_string),
            });
        }
        for (inst, entry) in &devices.evs {
            discovered.push(DiscoveredInstance {
                instance: *inst,
                kind: "ev".to_string(),
                name: entry.data.display_name().map(str::to_string),
            });
        }
        for (inst, entry) in &devices.evchargers {
            discovered.push(DiscoveredInstance {
                instance: *inst,
                kind: "evcharger".to_string(),
                name: entry.data.display_name().map(str::to_string),
            });
        }
        if !discovered.is_empty() {
            st.discovered_water_ev = Some(discovered);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_grid_null_clears_cached_phase_but_malformed_samples_do_not() {
        let mut devices = CerboDevices::default();
        let mut state = InverterState::default();
        for (path, payload) in [
            ("Ac/Grid/L1/Power", r#"{"value":-743}"#),
            ("Ac/Grid/L2/Power", r#"{"value":-7}"#),
        ] {
            assert!(MqttClient::apply_device_message(
                &mut devices,
                "system",
                0,
                path,
                payload
            ));
        }
        MqttClient::apply_cerbo_to_state(&devices, &mut state);
        assert_eq!(state.gt, Some(-750.0));
        for payload in ["broken", r#"{}"#, r#"{"value":"invalid"}"#] {
            assert!(!MqttClient::apply_device_message(
                &mut devices,
                "system",
                0,
                "Ac/Grid/L2/Power",
                payload
            ));
        }
        MqttClient::apply_cerbo_to_state(&devices, &mut state);
        assert_eq!(state.g2, Some(-7.0));
        assert!(MqttClient::apply_device_message(
            &mut devices,
            "system",
            0,
            "Ac/Grid/L2/Power",
            r#"{"value":null}"#
        ));
        MqttClient::apply_cerbo_to_state(&devices, &mut state);
        assert_eq!(state.g2, None);
        assert_eq!(state.grid_l2_available, Some(false));
        assert_eq!(state.gt, Some(-743.0));
        assert!(MqttClient::apply_device_message(
            &mut devices,
            "system",
            0,
            "Ac/Grid/L1/Power",
            r#"{"value":null}"#
        ));
        MqttClient::apply_cerbo_to_state(&devices, &mut state);
        assert_eq!(state.gt, None);
    }

    fn bat(name: &str, amps: f64) -> Battery {
        Battery {
            name: Some(name.to_string()),
            current: Some(amps),
            ..Default::default()
        }
    }

    #[test]
    fn system_consumption_and_vebus_setpoint_overlay_state() {
        let mut d = CerboDevices::default();
        assert!(MqttClient::apply_device_message(
            &mut d,
            "system",
            0,
            "Ac/Consumption/L1/Power",
            r#"{"value": 1200.0}"#
        ));
        assert!(MqttClient::apply_device_message(
            &mut d,
            "system",
            0,
            "Ac/Consumption/L2/Power",
            r#"{"value": 800.0}"#
        ));
        assert!(MqttClient::apply_device_message(
            &mut d,
            "system",
            0,
            "Ac/Grid/L1/Power",
            r#"{"value": -50.0}"#
        ));
        assert!(MqttClient::apply_device_message(
            &mut d,
            "system",
            0,
            "Ac/Grid/L2/Power",
            r#"{"value": 20.0}"#
        ));
        assert!(MqttClient::apply_device_message(
            &mut d,
            "vebus",
            276,
            "Hub4/L1/AcPowerSetpoint",
            r#"{"value": -615.0}"#
        ));
        assert!(MqttClient::apply_device_message(
            &mut d,
            "vebus",
            276,
            "State",
            r#"{"value": 3}"#
        ));

        // Daemon zeros must not win over Cerbo overlay.
        let mut st = InverterState {
            tt: Some(0.0),
            setpoint: Some(0.0),
            ..InverterState::default()
        };
        MqttClient::apply_cerbo_to_state(&d, &mut st);
        assert_eq!(st.t1, Some(1200.0));
        assert_eq!(st.t2, Some(800.0));
        assert_eq!(st.tt, Some(2000.0));
        assert_eq!(st.g1, Some(-50.0));
        assert_eq!(st.g2, Some(20.0));
        assert_eq!(st.gt, Some(-30.0));
        assert_eq!(st.setpoint, Some(-615.0));
        assert_eq!(st.inverter_state.as_deref(), Some("Bulk"));
    }

    #[test]
    fn apply_device_message_tracks_max_cell_voltage_fields() {
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
        let b = &d.batteries.get(&288).unwrap().data;
        assert_eq!(b.instance, Some(288));
        assert_eq!(b.max_cell_voltage, Some(3.62));
        assert_eq!(b.max_voltage_cell_id.as_deref(), Some("7"));
    }

    #[test]
    fn parse_cerbo_cell_id_accepts_number_and_string() {
        assert_eq!(
            MqttClient::parse_cerbo_cell_id("{\"value\": 7}").as_deref(),
            Some("7")
        );
        assert_eq!(
            MqttClient::parse_cerbo_cell_id("{\"value\": \"12\"}").as_deref(),
            Some("12")
        );
    }

    #[test]
    fn parses_tank_level_topic() {
        assert_eq!(
            MqttClient::parse_water_topic("N/abc123/tank/21/Level"),
            Some(("tank", 21, "Level"))
        );
    }

    #[test]
    fn parses_pump_state_topic() {
        assert_eq!(
            MqttClient::parse_water_topic("N/abc123/pump/2/State"),
            Some(("pump", 2, "State"))
        );
    }

    #[test]
    fn parses_pump_mode_topic() {
        assert_eq!(
            MqttClient::parse_water_topic("N/abc123/pump/1/Mode"),
            Some(("pump", 1, "Mode"))
        );
        assert_eq!(
            MqttClient::parse_water_topic("N/abc123/pump/2/Mode"),
            Some(("pump", 2, "Mode"))
        );
        assert_eq!(
            MqttClient::parse_water_topic("N/abc123/pump/1/Pressure"),
            None
        );
    }

    #[test]
    fn parses_tank_custom_name_topic() {
        assert_eq!(
            MqttClient::parse_water_topic("N/abc123/tank/21/CustomName"),
            Some(("tank", 21, "CustomName"))
        );
    }

    #[test]
    fn rejects_other_paths_and_services() {
        assert_eq!(MqttClient::parse_water_topic("N/abc/tank/21/Voltage"), None);
        assert_eq!(
            MqttClient::parse_water_topic("N/abc/solarcharger/0/State"),
            None
        );
        assert_eq!(MqttClient::parse_water_topic("N/abc/pump/x/State"), None);
    }

    #[test]
    fn parses_ev_soc_topic() {
        assert_eq!(
            MqttClient::parse_ev_topic("N/portal/ev/22/Soc"),
            Some(("ev", 22, "Soc"))
        );
    }

    #[test]
    fn parses_ev_power_topic() {
        assert_eq!(
            MqttClient::parse_ev_topic("N/portal/ev/22/Ac/Power"),
            Some(("ev", 22, "Ac/Power"))
        );
    }

    #[test]
    fn parses_evcharger_power_topic() {
        assert_eq!(
            MqttClient::parse_ev_topic("N/portal/evcharger/40/Ac/Power"),
            Some(("evcharger", 40, "Ac/Power"))
        );
    }

    #[test]
    fn apply_ev_message_pops_car_soc() {
        let mut st = InverterState::default();
        let mut cache = EvCache::default();
        let instances = Some((Some(22), Some(40)));
        assert!(MqttClient::apply_ev_message(
            &mut st, &mut cache, "ev", 22, "Soc", 66.0, &instances
        )
        .is_some());
        assert_eq!(st.car_soc, Some(66.0));
    }

    #[test]
    fn apply_ev_message_pops_evcharger_power() {
        let mut st = InverterState::default();
        let mut cache = EvCache::default();
        let instances = Some((Some(22), Some(40)));
        assert!(MqttClient::apply_ev_message(
            &mut st,
            &mut cache,
            "evcharger",
            40,
            "Ac/Power",
            7400.0,
            &instances
        )
        .is_some());
        assert_eq!(st.ev_charging_power, Some(7400.0));
    }

    #[test]
    fn apply_ev_message_ignores_wrong_instance() {
        let mut st = InverterState::default();
        let mut cache = EvCache::default();
        let instances = Some((Some(22), Some(40)));
        assert_eq!(
            MqttClient::apply_ev_message(&mut st, &mut cache, "ev", 99, "Soc", 66.0, &instances),
            None
        );
        assert!(st.car_soc.is_none());
        assert_eq!(
            MqttClient::apply_ev_message(
                &mut st,
                &mut cache,
                "evcharger",
                99,
                "Ac/Power",
                7400.0,
                &instances
            ),
            None
        );
        assert!(st.ev_charging_power.is_none());
    }

    #[test]
    fn apply_ev_message_partial_instances_ev_only() {
        let mut st = InverterState::default();
        let mut cache = EvCache::default();
        // ev_instance set, evcharger_instance absent
        let instances = Some((Some(22), None));
        assert!(MqttClient::apply_ev_message(
            &mut st, &mut cache, "ev", 22, "Soc", 55.0, &instances
        )
        .is_some());
        assert_eq!(st.car_soc, Some(55.0));
        // evcharger message must be ignored
        assert_eq!(
            MqttClient::apply_ev_message(
                &mut st,
                &mut cache,
                "evcharger",
                40,
                "Ac/Power",
                7400.0,
                &instances
            ),
            None
        );
        assert!(st.ev_charging_power.is_none());
    }

    #[test]
    fn apply_ev_message_sets_presence_for_ev() {
        let mut st = InverterState::default();
        let mut cache = EvCache::default();
        let instances = Some((Some(22), Some(40)));
        // Presence is set on the cache after apply_ev_message
        MqttClient::apply_ev_message(&mut st, &mut cache, "ev", 22, "Soc", 0.0, &instances);
        assert!(
            cache.ev_present,
            "cache.ev_present should be true after apply_ev_message with 0"
        );
        assert_eq!(st.car_soc, Some(0.0));
        // After restore_into, st.ev_present mirrors cache
        cache.restore_into(&mut st);
        assert!(
            st.ev_present,
            "st.ev_present should be true after restore_into"
        );
    }

    #[test]
    fn apply_ev_message_sets_presence_for_evcharger() {
        let mut st = InverterState::default();
        let mut cache = EvCache::default();
        let instances = Some((Some(22), Some(40)));
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
            "cache.evcharger_present should be true after apply_ev_message with 0"
        );
        assert_eq!(st.ev_charging_power, Some(0.0));
        // After restore_into, st.evcharger_present mirrors cache
        cache.restore_into(&mut st);
        assert!(
            st.evcharger_present,
            "st.evcharger_present should be true after restore_into"
        );
    }

    #[test]
    fn apply_ev_message_partial_instances_evcharger_only() {
        let mut st = InverterState::default();
        let mut cache = EvCache::default();
        // evcharger_instance set, ev_instance absent
        let instances = Some((None, Some(40)));
        assert!(MqttClient::apply_ev_message(
            &mut st,
            &mut cache,
            "evcharger",
            40,
            "Ac/Power",
            5500.0,
            &instances
        )
        .is_some());
        assert_eq!(st.ev_charging_power, Some(5500.0));
        // ev message must be ignored
        assert_eq!(
            MqttClient::apply_ev_message(&mut st, &mut cache, "ev", 22, "Soc", 55.0, &instances),
            None
        );
        assert!(st.car_soc.is_none());
    }

    #[test]
    fn apply_ev_message_no_instances_ignores_all() {
        let mut st = InverterState::default();
        let mut cache = EvCache::default();
        let instances: Option<(Option<u32>, Option<u32>)> = None;
        assert_eq!(
            MqttClient::apply_ev_message(&mut st, &mut cache, "ev", 22, "Soc", 66.0, &instances),
            None
        );
        assert_eq!(
            MqttClient::apply_ev_message(
                &mut st,
                &mut cache,
                "evcharger",
                40,
                "Ac/Power",
                7400.0,
                &instances
            ),
            None
        );
        assert!(st.car_soc.is_none());
        assert!(st.ev_charging_power.is_none());
    }

    #[test]
    fn apply_ev_message_evcharger_soc_populates_car_soc() {
        let mut st = InverterState::default();
        let mut cache = EvCache::default();
        let instances = Some((Some(22), Some(40)));
        // dbus-ev may publish Soc under the evcharger bus name on installs
        // where the .ev rename hasn't reached the GX yet.
        assert!(MqttClient::apply_ev_message(
            &mut st,
            &mut cache,
            "evcharger",
            40,
            "Soc",
            72.5,
            &instances
        )
        .is_some());
        assert_eq!(st.car_soc, Some(72.5));
    }

    #[test]
    fn apply_ev_message_partial_default_when_serialized_old_config() {
        // Simulates the #302 regression for users whose saved config predates
        // the EV instance fields: serde defaults populate them to (22, 40).
        let mut st = InverterState::default();
        let mut cache = EvCache::default();
        let instances = Some((Some(22), Some(40)));
        assert!(MqttClient::apply_ev_message(
            &mut st, &mut cache, "ev", 22, "Soc", 66.0, &instances
        )
        .is_some());
        assert_eq!(st.car_soc, Some(66.0));
        assert!(MqttClient::apply_ev_message(
            &mut st,
            &mut cache,
            "evcharger",
            40,
            "Ac/Power",
            7400.0,
            &instances
        )
        .is_some());
        assert_eq!(st.ev_charging_power, Some(7400.0));
    }

    #[test]
    fn apply_ev_message_both_none_drops_all_messages() {
        // Belt-and-braces: if the auto-connect path ever regresses to pass
        // Some((None, None)) again, EVERY ev and evcharger message must drop.
        let mut st = InverterState::default();
        let mut cache = EvCache::default();
        let instances: Option<(Option<u32>, Option<u32>)> = Some((None, None));
        assert_eq!(
            MqttClient::apply_ev_message(&mut st, &mut cache, "ev", 22, "Soc", 66.0, &instances),
            None
        );
        assert_eq!(
            MqttClient::apply_ev_message(
                &mut st,
                &mut cache,
                "evcharger",
                40,
                "Ac/Power",
                7400.0,
                &instances
            ),
            None
        );
        assert!(st.car_soc.is_none());
        assert!(st.ev_charging_power.is_none());
    }

    #[test]
    fn rejects_ev_other_paths() {
        assert_eq!(MqttClient::parse_ev_topic("N/portal/ev/22/Status"), None);
        assert_eq!(MqttClient::parse_ev_topic("N/portal/ev/22/Current"), None);
        assert_eq!(MqttClient::parse_ev_topic("N/portal/ev/22/Energy"), None);
        assert_eq!(MqttClient::parse_ev_topic("N/portal/ev/x/Ac/Power"), None);
        // dbus-ev publishes Soc under the evcharger bus name, so this is valid.
        assert_eq!(
            MqttClient::parse_ev_topic("N/portal/evcharger/40/Soc"),
            Some(("evcharger", 40, "Soc"))
        );
        assert_eq!(
            MqttClient::parse_ev_topic("N/portal/evcharger/40/Status"),
            None
        );
    }

    #[test]
    fn parses_cerbo_envelope() {
        assert_eq!(
            MqttClient::parse_cerbo_value("{\"value\": 66.0}"),
            Some(66.0)
        );
        assert_eq!(MqttClient::parse_cerbo_value("{\"value\": null}"), None);
        assert_eq!(MqttClient::parse_cerbo_value("not json"), None);
    }

    #[test]
    fn parses_device_topics() {
        assert_eq!(
            MqttClient::parse_device_topic("N/abc/battery/512/Soc"),
            Some(("battery", 512, "Soc"))
        );
        assert_eq!(
            MqttClient::parse_device_topic("N/abc/solarcharger/1/Dc/0/Current"),
            Some(("solarcharger", 1, "Dc/0/Current"))
        );
        assert_eq!(
            MqttClient::parse_device_topic("N/abc/pvinverter/369/Ac/L1/Voltage"),
            Some(("pvinverter", 369, "Ac/L1/Voltage"))
        );
        // Other kinds route elsewhere or are ignored.
        assert_eq!(MqttClient::parse_device_topic("N/abc/tank/21/Level"), None);
        assert_eq!(
            MqttClient::parse_device_topic("N/abc/vebus/256/Soc"),
            Some(("vebus", 256, "Soc"))
        );
        assert_eq!(MqttClient::parse_device_topic("N/abc/battery/x/Soc"), None);
    }

    #[test]
    fn discovers_battery_and_charger_from_gx() {
        let mut d = CerboDevices::default();
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            512,
            "Soc",
            "{\"value\": 87.5}"
        ));
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            512,
            "Dc/0/Voltage",
            "{\"value\": 51.2}"
        ));
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            512,
            "ProductName",
            "{\"value\": \"SmartShunt 500A\"}"
        ));
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            512,
            "Dc/0/Current",
            "{\"value\": 12.5}"
        ));
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            512,
            "TimeToGo",
            "{\"value\": 108110.0}"
        ));
        // Idle battery (0 A within deadband): stale time-to-go must be hidden
        // even if a value was seen earlier.
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            513,
            "Dc/0/Current",
            "{\"value\": 0.1}"
        ));
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            513,
            "TimeToGo",
            "{\"value\": 7200.0}"
        ));
        // Discharging derivation from negative current.
        assert_eq!(MqttClient::state_from_current(-3.5), "Discharging");
        assert!(MqttClient::apply_device_message(
            &mut d,
            "solarcharger",
            2,
            "Yield/Power",
            "{\"value\": 1450}"
        ));
        // AC PV inverter of any vendor: V/I/P from the GX bridge paths.
        assert!(MqttClient::apply_device_message(
            &mut d,
            "pvinverter",
            369,
            "Ac/L1/Voltage",
            "{\"value\": 126.0}"
        ));
        assert!(MqttClient::apply_device_message(
            &mut d,
            "pvinverter",
            369,
            "Ac/L1/Current",
            "{\"value\": 1.29}"
        ));
        assert!(MqttClient::apply_device_message(
            &mut d,
            "pvinverter",
            369,
            "Ac/Power",
            "{\"value\": 163}"
        ));
        assert!(!MqttClient::apply_device_message(
            &mut d,
            "pvinverter",
            369,
            "StatusCode",
            "{\"value\": 0}"
        ));
        // Unknown path ignored, alarms payload not a number.
        assert!(!MqttClient::apply_device_message(
            &mut d,
            "battery",
            512,
            "Alarms/HighVoltage",
            "{\"value\": 1}"
        ));

        let b = &d.batteries[&512].data;
        assert_eq!(b.soc, Some(87.5));
        assert_eq!(b.voltage, Some(51.2));
        assert_eq!(b.name.as_deref(), Some("SmartShunt 500A"));
        assert_eq!(b.state.as_deref(), Some("Charging"));
        assert_eq!(b.time_to_go.as_deref(), Some("30h 01m"));
        assert_eq!(d.batteries[&513].data.state.as_deref(), Some("Idle"));
        // Raw value still stored; the Idle gate happens at overlay time.
        assert_eq!(d.batteries[&513].data.time_to_go.as_deref(), Some("2h 00m"));

        let mut st = InverterState::default();
        MqttClient::apply_cerbo_to_state(&d, &mut st);
        // Bank % is voltage-derived (HA paradigm), NOT the shunt's SoC counter
        // which reads bogus 100% while charging: ((51.2-40)/14.4)*100 -> 78.
        assert_eq!(st.battery_soc, Some(voltage_soc(51.2)));
        assert_eq!(st.battery_soc, Some(78.0));
        assert_eq!(st.batteries.as_ref().unwrap().len(), 2);
        assert_eq!(st.mppt_chargers.as_ref().unwrap().len(), 1);
        assert_eq!(st.mppt_total, Some(1450.0));
        // Discovered PV inverter surfaces with V/I/P; the legacy aggregate
        // mirrors its power so older UIs keep working.
        assert_eq!(st.pv_inverters.as_ref().unwrap().len(), 1);
        let inv = &st.pv_inverters.as_ref().unwrap()[0];
        assert_eq!(inv.voltage, Some(126.0));
        assert_eq!(inv.current, Some(1.29));
        assert_eq!(inv.power, Some(163.0));

        // Shunt SoC counter itself is untouched in the per-device tile list.
        let bats = st.batteries.clone().unwrap();
        assert_eq!(bats[0].soc, Some(87.5));
        // Idle battery keeps its state but loses the stale time-to-go.
        assert_eq!(bats[1].state.as_deref(), Some("Idle"));
        assert_eq!(bats[1].time_to_go, None);
    }

    /// Per-device TTL: entries survive sweep when recently seen, are evicted
    /// after the TTL window, and updating one entry doesn't affect others.
    #[test]
    fn cerbo_sweep_evicts_ghost_devices() {
        let mut d = CerboDevices::default();
        assert!(MqttClient::apply_device_message(
            &mut d,
            "solarcharger",
            9,
            "Yield/Power",
            "{\"value\": 5}"
        ));
        // Entry just created — survives sweep (last_seen < TTL).
        d.sweep_stale();
        assert_eq!(d.chargers.len(), 1);

        // Second entry: updating one device doesn't evict the other.
        assert!(MqttClient::apply_device_message(
            &mut d,
            "solarcharger",
            10,
            "Yield/Power",
            "{\"value\": 8}"
        ));
        d.sweep_stale();
        assert_eq!(d.chargers.len(), 2);

        // Both entries are fresh — neither is evicted.
        assert!(d.chargers.contains_key(&9));
        assert!(d.chargers.contains_key(&10));
    }

    #[test]
    fn cerbo_devices_override_daemon_but_empty_map_leaves_daemon_data() {
        let mut st = InverterState {
            batteries: Some(vec![Battery {
                name: Some("daemon".into()),
                soc: Some(10.0),
                ..Default::default()
            }]),
            mppt_chargers: None,
            battery_soc: Some(10.0),
            ..Default::default()
        };

        // Empty discovery → daemon data untouched.
        let empty = CerboDevices::default();
        MqttClient::apply_cerbo_to_state(&empty, &mut st);
        assert_eq!(
            st.batteries.as_ref().unwrap()[0].name.as_deref(),
            Some("daemon")
        );
        assert_eq!(st.battery_soc, Some(10.0));

        // Discovered non-shunt devices refresh the battery list but must NOT
        // become bank totals (summing overlapping services double-counts);
        // daemon totals stay untouched until a shunt shows up.
        let mut d = CerboDevices::default();
        MqttClient::apply_device_message(&mut d, "battery", 1, "Soc", "{\"value\": 90}");
        MqttClient::apply_device_message(&mut d, "battery", 1, "Dc/0/Current", "{\"value\": -3.5}");
        MqttClient::apply_cerbo_to_state(&d, &mut st);
        let bats = st.batteries.clone().unwrap();
        assert_eq!(bats.len(), 1);
        assert_eq!(bats[0].soc, Some(90.0));
        assert_eq!(st.battery_soc, Some(10.0));
        assert_eq!(st.battery_current, None);

        // Shunt-named device wins all bank totals.
        MqttClient::apply_device_message(
            &mut d,
            "battery",
            2,
            "ProductName",
            "\"SmartShunt 500A/50mV\"",
        );
        MqttClient::apply_device_message(&mut d, "battery", 2, "Soc", "{\"value\": 87}");
        MqttClient::apply_device_message(&mut d, "battery", 2, "Dc/0/Voltage", "{\"value\": 53.2}");
        MqttClient::apply_device_message(&mut d, "battery", 2, "Dc/0/Power", "{\"value\": 672}");
        MqttClient::apply_cerbo_to_state(&d, &mut st);
        // Voltage-derived % wins over the shunt's SoC counter (87 here).
        assert_eq!(st.battery_soc, Some(voltage_soc(53.2)));
        assert_eq!(st.battery_soc, Some(92.0));
        assert_eq!(st.battery_power, Some(672.0));

        // Missing voltage keeps the previous bank % instead of blanking it.
        let mut d_no_v = CerboDevices::default();
        MqttClient::apply_device_message(
            &mut d_no_v,
            "battery",
            3,
            "ProductName",
            "\"SmartShunt 500A/50mV\"",
        );
        MqttClient::apply_device_message(&mut d_no_v, "battery", 3, "Soc", "{\"value\": 100}");
        MqttClient::apply_cerbo_to_state(&d_no_v, &mut st);
        assert_eq!(st.battery_soc, Some(92.0));
    }

    #[test]
    fn finds_shunt_by_product_name_regardless_of_instance() {
        // Instance numbers change across GX reboots; the product name is stable.
        let batteries = vec![
            bat("JBD Chain 1", 4.0),
            bat("Virtual Battery", 4.4),
            bat("SmartShunt 500A/50mV", 12.9),
        ];
        assert_eq!(
            MqttClient::find_shunt(&batteries).unwrap().current,
            Some(12.9)
        );
    }

    #[test]
    fn no_shunt_when_only_chains_present() {
        let batteries = vec![bat("JBD Chain 1", 4.0), bat("JBD Chain 2", 4.3)];
        assert!(MqttClient::find_shunt(&batteries).is_none());
    }

    /// Partial topic streams: only some fields arrive per message. Existing
    /// fields must be preserved when a new message updates a different field.
    #[test]
    fn partial_device_messages_preserve_existing_fields() {
        let mut d = CerboDevices::default();
        // First message: only SoC arrives.
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            512,
            "Soc",
            "{\"value\": 85.0}"
        ));
        assert_eq!(d.batteries[&512].data.soc, Some(85.0));
        assert_eq!(d.batteries[&512].data.voltage, None);

        // Second message: only voltage arrives — SoC must survive.
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            512,
            "Dc/0/Voltage",
            "{\"value\": 52.1}"
        ));
        assert_eq!(d.batteries[&512].data.soc, Some(85.0));
        assert_eq!(d.batteries[&512].data.voltage, Some(52.1));

        // Third message: power arrives — both SoC and voltage survive.
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            512,
            "Dc/0/Power",
            "{\"value\": -1200}"
        ));
        assert_eq!(d.batteries[&512].data.soc, Some(85.0));
        assert_eq!(d.batteries[&512].data.voltage, Some(52.1));
        assert_eq!(d.batteries[&512].data.power, Some(-1200.0));

        // Name arrives later — all numeric fields still intact.
        assert!(MqttClient::apply_device_message(
            &mut d,
            "battery",
            512,
            "ProductName",
            "{\"value\": \"SmartShunt 500A\"}"
        ));
        assert_eq!(
            d.batteries[&512].data.name.as_deref(),
            Some("SmartShunt 500A")
        );
        assert_eq!(d.batteries[&512].data.soc, Some(85.0));
        assert_eq!(d.batteries[&512].data.voltage, Some(52.1));
        assert_eq!(d.batteries[&512].data.power, Some(-1200.0));
    }

    /// Shunt bank totals survive when the shunt message is delayed.
    /// The cerbo overlay should keep existing bank values when the shunt
    /// is momentarily absent from the device map.
    #[test]
    fn shunt_bank_totals_retained_when_shunt_delayed() {
        let mut st = InverterState::default();

        // Phase 1: shunt present — sets bank totals.
        let mut d = CerboDevices::default();
        MqttClient::apply_device_message(
            &mut d,
            "battery",
            1,
            "ProductName",
            "\"SmartShunt 500A/50mV\"",
        );
        MqttClient::apply_device_message(&mut d, "battery", 1, "Dc/0/Voltage", "{\"value\": 53.0}");
        MqttClient::apply_device_message(&mut d, "battery", 1, "Dc/0/Current", "{\"value\": -5.0}");
        MqttClient::apply_device_message(&mut d, "battery", 1, "Dc/0/Power", "{\"value\": -265}");
        MqttClient::apply_cerbo_to_state(&d, &mut st);
        assert_eq!(st.battery_soc, Some(voltage_soc(53.0)));
        assert_eq!(st.battery_voltage, Some(53.0));
        assert_eq!(st.battery_current, Some(-5.0));
        assert_eq!(st.battery_power, Some(-265.0));

        // Phase 2: shunt missing (delayed / MQTT gap) — only a non-shunt battery.
        let mut d2 = CerboDevices::default();
        MqttClient::apply_device_message(&mut d2, "battery", 2, "Soc", "{\"value\": 90}");
        MqttClient::apply_cerbo_to_state(&d2, &mut st);
        // Bank totals from the shunt must survive.
        assert_eq!(st.battery_soc, Some(voltage_soc(53.0)));
        assert_eq!(st.battery_voltage, Some(53.0));
        assert_eq!(st.battery_power, Some(-265.0));

        // Phase 3: shunt returns with updated values.
        let mut d3 = CerboDevices::default();
        MqttClient::apply_device_message(
            &mut d3,
            "battery",
            1,
            "ProductName",
            "\"SmartShunt 500A/50mV\"",
        );
        MqttClient::apply_device_message(
            &mut d3,
            "battery",
            1,
            "Dc/0/Voltage",
            "{\"value\": 52.5}",
        );
        MqttClient::apply_device_message(&mut d3, "battery", 1, "Dc/0/Power", "{\"value\": -300}");
        MqttClient::apply_cerbo_to_state(&d3, &mut st);
        // Updated shunt values replace the old ones.
        assert_eq!(st.battery_soc, Some(voltage_soc(52.5)));
        assert_eq!(st.battery_voltage, Some(52.5));
        assert_eq!(st.battery_power, Some(-300.0));
    }

    /// Per-device TTL: updating one device must not evict other active entries.
    #[test]
    fn per_device_ttl_does_not_evict_active_peers() {
        let mut d = CerboDevices::default();
        // Create two devices.
        MqttClient::apply_device_message(&mut d, "battery", 1, "Soc", "{\"value\": 80}");
        MqttClient::apply_device_message(&mut d, "battery", 2, "Soc", "{\"value\": 90}");
        assert_eq!(d.batteries.len(), 2);

        // Update only device 1 — device 2 must survive the sweep.
        MqttClient::apply_device_message(&mut d, "battery", 1, "Soc", "{\"value\": 81}");
        d.sweep_stale();
        assert_eq!(d.batteries.len(), 2);
        assert_eq!(d.batteries[&1].data.soc, Some(81.0));
        assert_eq!(d.batteries[&2].data.soc, Some(90.0));
    }

    #[test]
    fn parses_acload_power_and_name_topics() {
        assert_eq!(
            MqttClient::parse_acload_topic("N/portal/acload/81/Ac/Power"),
            Some((81, "Ac/Power"))
        );
        assert_eq!(
            MqttClient::parse_acload_topic("N/portal/acload/88/CustomName"),
            Some((88, "CustomName"))
        );
        assert_eq!(
            MqttClient::parse_acload_topic("N/portal/acload/88/ProductName"),
            Some((88, "ProductName"))
        );
        assert_eq!(
            MqttClient::parse_acload_topic("N/portal/acload/88/Ac/Energy/Forward"),
            None
        );
    }

    #[test]
    fn acload_custom_name_preferred_over_product_and_survives_power_update() {
        let mut d = CerboDevices::default();
        assert!(MqttClient::apply_acload_message(
            &mut d,
            81,
            "Ac/Power",
            "{\"value\": 420}"
        ));
        assert!(MqttClient::apply_acload_message(
            &mut d,
            81,
            "ProductName",
            "{\"value\": \"AC Load\"}"
        ));
        assert!(MqttClient::apply_acload_message(
            &mut d,
            81,
            "CustomName",
            "{\"value\": \"Kitchen\"}"
        ));

        let mut st = InverterState::default();
        MqttClient::apply_cerbo_to_state(&d, &mut st);
        assert_eq!(st.loads.as_ref().unwrap().get("81"), Some(&420.0));
        assert_eq!(
            st.load_names
                .as_ref()
                .unwrap()
                .get("81")
                .map(String::as_str),
            Some("Kitchen")
        );

        // Later power tick must NOT rekey loads or drop the cached name.
        assert!(MqttClient::apply_acload_message(
            &mut d,
            81,
            "Ac/Power",
            "{\"value\": 455}"
        ));
        MqttClient::apply_cerbo_to_state(&d, &mut st);
        assert_eq!(st.loads.as_ref().unwrap().get("81"), Some(&455.0));
        assert_eq!(
            st.load_names
                .as_ref()
                .unwrap()
                .get("81")
                .map(String::as_str),
            Some("Kitchen"),
            "name must survive power-only updates (no id flicker)"
        );
        // Map must stay instance-keyed — never rename the watts key to "Kitchen".
        assert!(st.loads.as_ref().unwrap().get("Kitchen").is_none());
    }

    #[test]
    fn resolve_active_keeps_preferred_when_present() {
        let mut map = BTreeMap::new();
        map.insert(1, TrackedEntry::default());
        map.insert(2, TrackedEntry::default());
        assert_eq!(MqttClient::resolve_active_instance(Some(2), &map), Some(2));
    }

    #[test]
    fn resolve_active_falls_back_to_first_when_preferred_missing() {
        let mut map = BTreeMap::new();
        map.insert(5, TrackedEntry::default());
        map.insert(9, TrackedEntry::default());
        assert_eq!(MqttClient::resolve_active_instance(Some(40), &map), Some(5));
    }

    #[test]
    fn resolve_active_keeps_preferred_while_discovery_empty() {
        let map: BTreeMap<u32, TrackedEntry<NamedDevice>> = BTreeMap::new();
        assert_eq!(
            MqttClient::resolve_active_instance(Some(22), &map),
            Some(22)
        );
    }

    #[test]
    fn apply_named_discovery_tracks_tank_and_name() {
        let mut d = CerboDevices::default();
        MqttClient::apply_named_discovery(&mut d, "tank", 21, "Level", "{\"value\": 66.0}");
        MqttClient::apply_named_discovery(
            &mut d,
            "tank",
            21,
            "CustomName",
            "{\"value\": \"Cistern\"}",
        );
        MqttClient::apply_named_discovery(&mut d, "pump", 1, "State", "{\"value\": 1}");
        MqttClient::apply_named_discovery(
            &mut d,
            "evcharger",
            40,
            "Ac/Power",
            "{\"value\": 1200.0}",
        );
        MqttClient::apply_named_discovery(
            &mut d,
            "ev",
            22,
            "ProductName",
            "{\"value\": \"EV Vehicle\"}",
        );
        let mut st = InverterState::default();
        MqttClient::apply_cerbo_to_state(&d, &mut st);
        let list = st.discovered_water_ev.expect("discovered list");
        assert!(list
            .iter()
            .any(|i| i.kind == "tank" && i.instance == 21 && i.name.as_deref() == Some("Cistern")));
        assert!(list.iter().any(|i| i.kind == "pump" && i.instance == 1));
        assert!(list.iter().any(|i| i.kind == "ev"
            && i.instance == 22
            && i.name.as_deref() == Some("EV Vehicle")));
        assert!(list
            .iter()
            .any(|i| i.kind == "evcharger" && i.instance == 40));
    }

    #[test]
    fn parses_ev_custom_name() {
        assert_eq!(
            MqttClient::parse_ev_topic("N/portal/ev/22/CustomName"),
            Some(("ev", 22, "CustomName"))
        );
        assert_eq!(
            MqttClient::parse_ev_topic("N/portal/evcharger/40/ProductName"),
            Some(("evcharger", 40, "ProductName"))
        );
    }
}
