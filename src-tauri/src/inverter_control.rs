//! Commands owned by the inverter-control daemon on Cerbo.
//!
//! Flags are read from `inverter/state.booleans` and written through
//! `inverter/cmd/toggle`. Home Assistant may expose the same controls as MQTT
//! switches, but is not required to read or operate them from this application.

use serde_json::Value;

pub(crate) const ESS_MODES: &[&str] = &[
    "off",
    "on",
    "optimized_with_battery_life",
    "optimized_without_battery_life",
    "keep_batteries_charged",
    "external_control",
];

pub(crate) fn validate_ess_selection(body: &Value) -> Result<(), String> {
    let valid = body.as_object().is_some_and(|obj| obj.len() == 2)
        && body
            .get("mode")
            .and_then(Value::as_str)
            .is_some_and(|mode| ESS_MODES.contains(&mode))
        && body
            .get("request_id")
            .and_then(Value::as_str)
            .is_some_and(|id| {
                !id.is_empty()
                    && id.len() <= 128
                    && id.bytes().all(|b| {
                        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-')
                    })
            });
    if valid {
        Ok(())
    } else {
        Err("Invalid ESS mode selection".into())
    }
}

/// Keep aligned with inverter-control's `control_flags.CONTROL_FLAG_KEYS`.
pub(crate) const FLAG_KEYS: &[&str] = &[
    "only_charging",
    "no_feed",
    "house_support",
    "charge_battery",
    "do_not_supply_charger",
    "set_limit_to_ev_charger",
    "minimize_charging",
];

/// Bare keys are canonical. Accept the historical configuration prefix only;
/// e.g. `switch.only_charging` remains a genuine Home Assistant entity.
pub(crate) fn flag_key(entity_or_key: &str) -> Option<&str> {
    let entity_or_key = entity_or_key.trim();
    let key = entity_or_key
        .strip_prefix("input_boolean.")
        .unwrap_or(entity_or_key);
    FLAG_KEYS.contains(&key).then_some(key)
}

pub(crate) fn is_flag(entity_or_key: &str) -> bool {
    flag_key(entity_or_key).is_some()
}

/// The controller owns complete tariff validation; transports enforce a bounded,
/// correlated command envelope and never accept a path or an arbitrary MQTT topic.
pub(crate) fn validate_tariff_command(body: &Value) -> Result<(), String> {
    let valid = body.as_object().is_some_and(|object| object.len() == 3)
        && body
            .get("request_id")
            .and_then(Value::as_str)
            .is_some_and(|id| {
                !id.is_empty()
                    && id.len() <= 128
                    && id.bytes().all(|b| {
                        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-')
                    })
            })
        && body
            .get("revision")
            .and_then(Value::as_str)
            .is_some_and(|revision| {
                revision.len() == 64
                    && revision
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            })
        && body
            .get("plan")
            .is_some_and(|plan| plan.is_null() || plan.is_object())
        && body.to_string().len() <= 100_000;
    if valid {
        Ok(())
    } else {
        Err("Invalid controller tariff command".into())
    }
}

/// Shared dashboard v1: an invalid observation is unknown, never confirmed off.
pub(crate) fn control_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(v) => Some(*v),
        Value::Number(v) => match v.as_f64() {
            Some(0.0) => Some(false),
            Some(1.0) => Some(true),
            _ => None,
        },
        Value::String(v) => match v.trim().to_ascii_lowercase().as_str() {
            "true" | "on" | "1" => Some(true),
            "false" | "off" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Canonicalize legacy toggle IDs at the MQTT boundary. An explicit state is
/// authoritative; otherwise preserve Desktop's absolute on/off command based
/// on the last received flag value (initially off when not yet published).
/// This keeps repeated delivery from toggling a flag twice in inverter-control.
pub(crate) fn prepare_command(
    action: &str,
    payload: &mut Value,
    current_flag_state: impl FnOnce(&str) -> Option<bool>,
) {
    if action != "toggle" {
        return;
    }
    let Some(key) = payload
        .get("entity")
        .and_then(Value::as_str)
        .and_then(flag_key)
        .map(str::to_owned)
    else {
        return;
    };
    let Some(object) = payload.as_object_mut() else {
        return;
    };
    object.insert("entity".to_string(), Value::String(key.clone()));
    if !object.contains_key("state") {
        let next = if current_flag_state(&key).unwrap_or(false) {
            "off"
        } else {
            "on"
        };
        object.insert("state".to_string(), Value::String(next.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tariff_envelopes_and_ui_fields_survive_native_transports() {
        let valid = json!({"request_id":"abc-123", "revision":"a".repeat(64), "plan":null});
        assert!(validate_tariff_command(&valid).is_ok());
        for invalid in [
            json!({}),
            json!({"request_id":"../topic", "revision":"a".repeat(64), "plan":null}),
            json!({"request_id":"id", "revision":"a".repeat(64), "plan":[], "path":"/tmp/file"}),
            json!({"request_id":"id", "revision":"bad", "plan":null}),
        ] {
            assert!(validate_tariff_command(&invalid).is_err());
        }
        let ui = json!({"electricity_tariff":{"version":2,"name":"Seasonal tariff"},
            "electricity_tariff_status":{"writable":true,"revision":"a".repeat(64)}});
        let native: crate::mqtt::UiConfig = serde_json::from_value(ui.clone()).unwrap();
        let serialized = serde_json::to_value(native).unwrap();
        assert_eq!(serialized["electricity_tariff"], ui["electricity_tariff"]);
        assert_eq!(
            serialized["electricity_tariff_status"],
            ui["electricity_tariff_status"]
        );
    }

    #[test]
    fn shared_dashboard_contract_v1() {
        use sha2::{Digest, Sha256};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../contracts/dashboard");
        let lock: Value =
            serde_json::from_slice(&std::fs::read(root.join("contract-lock.json")).unwrap())
                .unwrap();
        for (name, digest) in lock["sha256"].as_object().unwrap() {
            let bytes = std::fs::read(root.join("v1").join(name)).unwrap();
            assert_eq!(
                Sha256::digest(bytes)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>(),
                digest.as_str().unwrap()
            );
        }
        let fixtures: Value =
            serde_json::from_slice(&std::fs::read(root.join("v1/fixtures.json")).unwrap()).unwrap();
        for case in fixtures["key_cases"].as_array().unwrap() {
            assert_eq!(
                flag_key(case["input"].as_str().unwrap()),
                case["expected"].as_str()
            );
        }
        for case in fixtures["boolean_cases"].as_array().unwrap() {
            assert_eq!(control_bool(&case["input"]), case["expected"].as_bool());
        }
        for case in fixtures["commands"].as_array().unwrap() {
            let mut payload = case["payload"].clone();
            prepare_command("toggle", &mut payload, |_| {
                panic!("absolute commands never read stale state")
            });
            assert_eq!(payload, case["payload"]);
        }
    }

    #[test]
    fn known_flags_accept_only_canonical_keys_and_legacy_aliases() {
        for key in FLAG_KEYS {
            assert_eq!(flag_key(key), Some(*key));
            assert_eq!(flag_key(&format!("input_boolean.{key}")), Some(*key));
            assert_eq!(flag_key(&format!("switch.{key}")), None);
            assert_eq!(flag_key(&format!("sensor.{key}")), None);
        }
        assert_eq!(flag_key("input_boolean.guest_mode"), None);
        assert_eq!(flag_key("input_boolean.input_boolean.no_feed"), None);
        assert_eq!(flag_key(" no_feed "), Some("no_feed"));
        assert_eq!(flag_key(" input_boolean.no_feed "), Some("no_feed"));
    }

    #[test]
    fn legacy_toggle_is_normalized_and_uses_mqtt_flag_state() {
        let mut payload = json!({"entity": "input_boolean.no_feed"});
        prepare_command("toggle", &mut payload, |key| {
            assert_eq!(key, "no_feed");
            Some(true)
        });
        assert_eq!(payload, json!({"entity": "no_feed", "state": "off"}));
    }

    #[test]
    fn disabled_or_unknown_flags_preserve_initial_on_behavior() {
        for current in [Some(false), None] {
            let mut payload = json!({"entity": "house_support"});
            prepare_command("toggle", &mut payload, |_| current);
            assert_eq!(payload, json!({"entity": "house_support", "state": "on"}));
        }
    }

    #[test]
    fn explicit_state_is_never_replaced_by_a_cached_toggle() {
        for state in [
            json!("on"),
            json!("off"),
            json!(true),
            json!(false),
            json!(1),
            json!(0),
        ] {
            let mut payload = json!({"entity": "input_boolean.only_charging", "state": state});
            prepare_command("toggle", &mut payload, |_| {
                panic!("state lookup not needed")
            });
            assert_eq!(payload, json!({"entity": "only_charging", "state": state}));
        }
    }

    #[test]
    fn invalid_explicit_state_is_left_for_the_daemon_to_reject() {
        let mut payload = json!({"entity": "no_feed", "state": null});
        prepare_command("toggle", &mut payload, |_| {
            panic!("state lookup not needed")
        });
        assert_eq!(payload, json!({"entity": "no_feed", "state": null}));
    }

    #[test]
    fn home_entities_and_non_toggle_actions_are_unchanged() {
        for (action, mut payload) in [
            ("toggle", json!({"entity": "switch.only_charging"})),
            ("toggle", json!({"entity": "input_boolean.guest_mode"})),
            ("toggle", json!({"entity": "switch.garage"})),
            ("press", json!({"entity": "input_boolean.only_charging"})),
            ("water_mode", json!({"which": "pump", "mode": 1})),
            ("toggle", Value::Null),
        ] {
            let original = payload.clone();
            prepare_command(action, &mut payload, |_| panic!("state lookup not needed"));
            assert_eq!(payload, original);
        }
    }
}
