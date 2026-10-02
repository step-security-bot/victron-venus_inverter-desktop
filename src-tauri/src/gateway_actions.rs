//! Allowlisted UI actions for the active HTTPS gateway transport.
//!
//! A POST acknowledges queueing only. Never retry it or fall back to another
//! transport: an interrupted response does not mean the command was rejected.

use crate::gateway::{GatewayHttpAuth, GatewayInstances, GatewaySnapshot};
use crate::inverter_control;
use crate::mqtt::SetpointOverrideStatus;
use serde_json::{json, Value};
use std::future::Future;
use std::time::Duration;
use tokio::time::Instant;

#[derive(Debug, PartialEq)]
enum Action {
    Toggle {
        entity: String,
        state: Option<bool>,
    },
    DryRun(bool),
    EssMode,
    SelectEssMode(Value),
    ElectricityTariff(Value),
    WaterMode {
        instance: Option<u32>,
        valve: bool,
        mode: u8,
    },
}

pub(crate) fn water_payload(payload: &Value) -> Result<(&str, u8), String> {
    let invalid =
        || "Water control requires pump or valve and an integer mode (0, 1 or 2)".to_string();
    let object = payload
        .as_object()
        .filter(|obj| obj.len() == 2)
        .ok_or_else(invalid)?;
    let which = object
        .get("which")
        .and_then(Value::as_str)
        .filter(|which| matches!(*which, "pump" | "valve"))
        .ok_or_else(invalid)?;
    let mode = object
        .get("mode")
        .and_then(Value::as_u64)
        .filter(|mode| *mode <= 2)
        .ok_or_else(invalid)?;
    Ok((which, mode as u8))
}

fn parse(action: &str, payload: Value, water: GatewayInstances) -> Result<Action, String> {
    let invalid = || "Invalid gateway control payload".to_string();
    let object = payload.as_object().ok_or_else(invalid)?;
    match action {
        "toggle" => {
            if object.len() != 1 + usize::from(object.contains_key("state")) {
                return Err(invalid());
            }
            let entity = object
                .get("entity")
                .and_then(Value::as_str)
                .and_then(inverter_control::flag_key)
                .ok_or("Only inverter-control flags can be changed through IGW")?
                .to_string();
            let state = object
                .get("state")
                .map(|value| inverter_control::control_bool(value).ok_or_else(invalid))
                .transpose()?;
            Ok(Action::Toggle { entity, state })
        }
        "dry_run" if object.len() == 1 => object
            .get("value")
            .and_then(Value::as_bool)
            .map(Action::DryRun)
            .ok_or_else(invalid),
        "ess_mode" if object.is_empty() => Ok(Action::EssMode),
        "set_ess_mode" => {
            inverter_control::validate_ess_selection(&payload)?;
            Ok(Action::SelectEssMode(payload))
        }
        "electricity_tariff" => {
            inverter_control::validate_tariff_command(&payload)?;
            Ok(Action::ElectricityTariff(payload))
        }
        "water_mode" => {
            let (which, mode) = water_payload(&payload)?;
            let instance = if which == "valve" {
                water.water_valve
            } else {
                water.water_pump
            };
            Ok(Action::WaterMode {
                instance,
                valve: which == "valve",
                mode,
            })
        }
        "dry_run" | "ess_mode" => Err(invalid()),
        _ => Err(
            "This control is not supported through IGW; use its configured device connection"
                .into(),
        ),
    }
}

async fn execute_with<S, P, SF, PF>(request: Action, snapshot: S, post: P) -> Result<(), String>
where
    S: FnOnce() -> SF,
    SF: Future<Output = Result<GatewaySnapshot, String>>,
    P: FnOnce(&'static str, Value) -> PF,
    PF: Future<Output = Result<(), String>>,
{
    let (name, body) = match request {
        Action::Toggle { entity, state } => {
            let enabled = match state {
                Some(value) => value,
                None => {
                    let snap = snapshot().await?;
                    let state = crate::gateway::snapshot_to_state(&snap);
                    !state.booleans.as_ref().and_then(|flags| flags.get(&entity)).copied()
                        .ok_or("Current inverter-control flag state is unavailable; wait for fresh IGW telemetry")?
                }
            };
            (
                "toggle",
                json!({"entity":entity,"state":if enabled { "on" } else { "off" }}),
            )
        }
        Action::DryRun(value) => ("dry_run", json!({"value":value})),
        Action::EssMode => ("ess_mode", json!({})),
        Action::SelectEssMode(body) => {
            let snap = snapshot().await?;
            if snap
                .capabilities
                .get("set_ess_mode")
                .and_then(Value::as_bool)
                != Some(true)
            {
                return Err("Update inverter-gateway to select an ESS mode".into());
            }
            if snap
                .inverter
                .as_ref()
                .and_then(|state| state.get("ess_mode"))
                .and_then(|mode| mode.get("selection_supported"))
                .and_then(Value::as_bool)
                != Some(true)
            {
                return Err("Update inverter-control or wait for fresh ESS telemetry".into());
            }
            ("set_ess_mode", body)
        }
        Action::ElectricityTariff(body) => {
            let snap = snapshot().await?;
            if snap
                .capabilities
                .get("electricity_tariff")
                .and_then(Value::as_bool)
                != Some(true)
            {
                return Err("Update inverter-gateway to edit the controller tariff".into());
            }
            if snap
                .inverter
                .as_ref()
                .and_then(|inverter| {
                    inverter
                        .get("ui_config")
                        .and_then(|ui| ui.pointer("/electricity_tariff_status/writable"))
                })
                .and_then(Value::as_bool)
                != Some(true)
            {
                return Err(
                    "The controller does not support tariff editing or is unavailable".into(),
                );
            }
            ("electricity_tariff", body)
        }
        Action::WaterMode {
            instance,
            valve,
            mode,
        } => {
            let snap = snapshot().await?;
            if snap.capabilities.get("water_mode").and_then(Value::as_bool) != Some(true) {
                return Err("This IGW does not advertise Water control support".into());
            }
            let selected = crate::gateway::snapshot_instances(
                &snap,
                GatewayInstances {
                    water_pump: if valve { None } else { instance },
                    water_valve: if valve { instance } else { None },
                    ..Default::default()
                },
            );
            let instance = if valve {
                selected.water_valve
            } else {
                selected.water_pump
            }
            .ok_or("Water instance is unavailable")?;
            // Match IGW's native compatibility: absent Connected is allowed,
            // while a present value must be the numeric integer 1.
            let connected = snap
                .pump
                .get(&format!("{instance}/Connected"))
                .is_none_or(|value| value.as_u64() == Some(1));
            let current_mode = snap
                .pump
                .get(&format!("{instance}/Mode"))
                .and_then(Value::as_u64);
            if !connected || current_mode.is_none_or(|mode| mode > 2) {
                return Err(
                    "Selected Water device is unavailable; wait for fresh IGW telemetry".into(),
                );
            }
            ("water_mode", json!({"instance":instance,"mode":mode}))
        }
    };
    post(name, body).await
}

pub(crate) async fn perform(
    auth: &GatewayHttpAuth,
    action: &str,
    payload: Value,
    water: GatewayInstances,
) -> Result<(), String> {
    let request = parse(action, payload, water)?;
    execute_with(
        request,
        || crate::gateway::command_snapshot(auth),
        |name, body| crate::gateway::send_command(auth, name, body),
    )
    .await
}

const OVERRIDE_DEADLINE: Duration = Duration::from_secs(5);
const OVERRIDE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const OVERRIDE_UNCONFIRMED: &str =
    "Cerbo has not confirmed the override. Check its connection and current status before trying again.";

async fn before_deadline<T>(
    deadline: Instant,
    future: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    if Instant::now() >= deadline {
        return Err(OVERRIDE_UNCONFIRMED.into());
    }
    tokio::time::timeout_at(deadline, future)
        .await
        .map_err(|_| OVERRIDE_UNCONFIRMED.to_string())?
}

pub(crate) async fn get_setpoint_override(
    auth: &GatewayHttpAuth,
) -> Result<SetpointOverrideStatus, String> {
    auth.ensure_active()?;
    let snap = before_deadline(
        Instant::now() + OVERRIDE_DEADLINE,
        crate::gateway::command_snapshot(auth),
    )
    .await?;
    auth.ensure_active()?;
    crate::gateway::snapshot_override_status(&snap)
}

async fn set_override_with<S, P, A, SF, PF>(
    value: Option<i32>,
    request_id: &str,
    mut snapshot: S,
    post: P,
    active: A,
) -> Result<SetpointOverrideStatus, String>
where
    S: FnMut() -> SF,
    SF: Future<Output = Result<GatewaySnapshot, String>>,
    P: FnOnce(Value) -> PF,
    PF: Future<Output = Result<(), String>>,
    A: Fn() -> Result<(), String>,
{
    let deadline = Instant::now() + OVERRIDE_DEADLINE;
    active()?;
    let initial = before_deadline(deadline, snapshot()).await?;
    active()?;
    crate::gateway::snapshot_override_status(&initial)?;
    // One queue request, never repeated even if the response or acknowledgement
    // is lost. Reads and the POST all share one deadline, including DNS/TLS.
    before_deadline(
        deadline,
        post(json!({"value":value,"request_id":request_id})),
    )
    .await?;
    active()?;
    loop {
        let snap = before_deadline(deadline, snapshot()).await?;
        active()?;
        let status = crate::gateway::snapshot_override_status(&snap)?;
        if status.request_id.as_deref() == Some(request_id) {
            if let Some(error) = status.last_error.as_ref() {
                return Err(error.clone());
            }
            if status.value != value {
                return Err(
                    "Cerbo acknowledged a different override value; check its current status"
                        .into(),
                );
            }
            return Ok(status);
        }
        before_deadline(deadline, async {
            tokio::time::sleep(OVERRIDE_POLL_INTERVAL).await;
            Ok(())
        })
        .await?;
        active()?;
    }
}

pub(crate) async fn set_setpoint_override(
    auth: &GatewayHttpAuth,
    value: Option<i32>,
    request_id: &str,
) -> Result<SetpointOverrideStatus, String> {
    set_override_with(
        value,
        request_id,
        || crate::gateway::command_snapshot(auth),
        |body| crate::gateway::send_command(auth, "setpoint_override", body),
        || auth.ensure_active(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn controller(flags: Value) -> GatewaySnapshot {
        serde_json::from_value(json!({"inverter":{"booleans":flags}})).unwrap()
    }

    async fn request(
        action: &str,
        payload: Value,
        snapshot: Result<GatewaySnapshot, String>,
    ) -> (Result<(), String>, Vec<(&'static str, Value)>) {
        let calls = RefCell::new(Vec::new());
        let result = execute_with(
            parse(action, payload, GatewayInstances::default()).unwrap(),
            || async { snapshot },
            |name, body| {
                calls.borrow_mut().push((name, body));
                async { Ok(()) }
            },
        )
        .await;
        (result, calls.into_inner())
    }

    #[tokio::test]
    async fn ess_selection_requires_both_capabilities_and_posts_exactly_once() {
        for mode in crate::inverter_control::ESS_MODES {
            let body = json!({"mode":mode,"request_id":"ess-1"});
            for (gateway, controller) in
                [(false, false), (true, false), (false, true), (true, true)]
            {
                let snapshot =
                    serde_json::from_value(json!({"capabilities":{"set_ess_mode":gateway},
                    "inverter":{"ess_mode":{"selection_supported":controller}}}))
                    .unwrap();
                let (result, posts) = request("set_ess_mode", body.clone(), Ok(snapshot)).await;
                if gateway && controller {
                    result.unwrap();
                    assert_eq!(posts, vec![("set_ess_mode", body.clone())]);
                } else {
                    assert!(result.is_err());
                    assert!(posts.is_empty());
                }
            }
        }
        for body in [
            json!({}),
            json!({"mode":"off","request_id":""}),
            json!({"mode":"toggle","request_id":"a"}),
            json!({"mode":"off","request_id":"a","extra":1}),
        ] {
            assert!(parse("set_ess_mode", body, GatewayInstances::default()).is_err());
        }
    }

    #[tokio::test]
    async fn implicit_flags_use_fresh_gateway_values_and_canonical_entities() {
        for key in inverter_control::FLAG_KEYS {
            for current in [false, true] {
                let (result, posts) = request(
                    "toggle",
                    json!({"entity":format!("input_boolean.{key}")}),
                    Ok(controller(json!({(*key):current}))),
                )
                .await;
                result.unwrap();
                assert_eq!(
                    posts,
                    vec![(
                        "toggle",
                        json!({"entity":key,"state":if current {"off"} else {"on"}})
                    )]
                );
            }
        }
    }

    #[tokio::test]
    async fn unknown_or_failed_controller_read_never_posts() {
        for snap in [
            Ok(GatewaySnapshot::default()),
            Ok(controller(json!({}))),
            Err("snapshot HTTP 503".into()),
        ] {
            let (result, posts) = request("toggle", json!({"entity":"no_feed"}), snap).await;
            assert!(result.is_err());
            assert!(posts.is_empty());
        }
    }

    #[tokio::test]
    async fn explicit_flags_and_stateless_header_actions_skip_snapshot() {
        for (action, payload, expected) in [
            ("dry_run", json!({"value":true}), json!({"value":true})),
            ("dry_run", json!({"value":false}), json!({"value":false})),
            ("ess_mode", json!({}), json!({})),
            (
                "toggle",
                json!({"entity":"input_boolean.no_feed","state":false}),
                json!({"entity":"no_feed","state":"off"}),
            ),
            (
                "toggle",
                json!({"entity":"no_feed","state":" ON "}),
                json!({"entity":"no_feed","state":"on"}),
            ),
        ] {
            let (result, posts) =
                request(action, payload, Err("snapshot must not be called".into())).await;
            result.unwrap();
            assert_eq!(posts, vec![(action, expected)]);
        }
    }

    #[test]
    fn arbitrary_entities_topics_and_invalid_bodies_are_rejected_locally() {
        for (action, payload) in [
            ("toggle", json!({"entity":"switch.no_feed"})),
            ("toggle", json!({"entity":"input_boolean.guest"})),
            ("toggle", json!({"entity":"no_feed","topic":"arbitrary"})),
            ("toggle", json!({"entity":"no_feed","state":null})),
            ("toggle", json!({"entity":"no_feed","state":"toggle"})),
            ("dry_run", json!({"value":1})),
            ("dry_run", json!({"value":true,"extra":0})),
            ("ess_mode", json!({"value":true})),
            ("ess_mode", Value::Null),
            ("setpoint_override", json!({"value":100})),
            ("../toggle", json!({})),
        ] {
            assert!(parse(action, payload, GatewayInstances::default()).is_err());
        }
    }

    #[tokio::test]
    async fn post_failure_is_returned_once_without_retry() {
        let count = RefCell::new(0);
        let result = execute_with(
            Action::EssMode,
            || async { panic!("no snapshot") },
            |_, _| {
                *count.borrow_mut() += 1;
                async { Err("response interrupted after enqueue".into()) }
            },
        )
        .await;
        assert_eq!(result.unwrap_err(), "response interrupted after enqueue");
        assert_eq!(count.into_inner(), 1);
    }

    #[tokio::test]
    async fn source_switch_during_snapshot_rejects_the_command_before_http_dispatch() {
        for (action, payload, snap) in [
            (
                "toggle",
                json!({"entity":"no_feed"}),
                controller(json!({"no_feed":false})),
            ),
            (
                "water_mode",
                json!({"which":"pump","mode":1}),
                serde_json::from_value(json!({
                    "capabilities":{"water_mode":true},"pump":{"1/Mode":0}
                }))
                .unwrap(),
            ),
        ] {
            let client = crate::gateway::idle_test_client();
            let auth = client.http_auth();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            let pending = execute_with(
                parse(action, payload, GatewayInstances::default()).unwrap(),
                || async {
                    started_tx.send(()).unwrap();
                    release_rx.await.unwrap();
                    Ok(snap)
                },
                |name, body| crate::gateway::send_command(&auth, name, body),
            );
            let switch = async {
                started_rx.await.unwrap();
                client.stop();
                release_tx.send(()).unwrap();
            };
            let (result, ()) = tokio::join!(pending, switch);
            // The real HTTP boundary checks the stopped client before even URL
            // validation; no request or fallback can be dispatched.
            assert_eq!(
                result.unwrap_err(),
                "Gateway connection changed; retry the action on the current connection"
            );
        }
    }

    #[test]
    fn water_rejects_truncation_or_defaults_from_bad_input() {
        for payload in [
            json!({"which":"pump"}),
            json!({"which":"tank","mode":0}),
            json!({"which":"pump","mode":256}),
            json!({"which":"pump","mode":-1}),
            json!({"which":"pump","mode":1.5}),
            json!({"which":"pump","mode":"1"}),
            json!({"which":"pump","mode":1,"instance":99}),
        ] {
            assert!(water_payload(&payload).is_err());
        }
    }

    #[tokio::test]
    async fn water_selection_matches_display_and_never_substitutes_an_unavailable_target() {
        let snap = serde_json::from_value::<GatewaySnapshot>(json!({
            "inverter":{"ui_config":{"water":{"pump_instance":7,"valve_instance":9}}},
            "capabilities":{"water_mode":true},
            "pump":{"1/Mode":0,"2/Mode":0,"7/Mode":0,"9/Mode":0,"11/Mode":0,"13/Mode":0}
        }))
        .unwrap();
        for (which, configured, expected) in [
            ("pump", GatewayInstances::default(), 7),
            ("valve", GatewayInstances::default(), 9),
            (
                "pump",
                GatewayInstances {
                    water_pump: Some(11),
                    ..Default::default()
                },
                11,
            ),
            (
                "valve",
                GatewayInstances {
                    water_valve: Some(13),
                    ..Default::default()
                },
                13,
            ),
        ] {
            execute_with(
                parse("water_mode", json!({"which":which,"mode":0}), configured).unwrap(),
                || async { Ok(snap.clone()) },
                |name, body| async move {
                    assert_eq!(name, "water_mode");
                    assert_eq!(body, json!({"instance":expected,"mode":0}));
                    Ok(())
                },
            )
            .await
            .unwrap();
        }
        let configured = GatewayInstances {
            water_pump: Some(99),
            ..Default::default()
        };
        let result = execute_with(
            parse("water_mode", json!({"which":"pump","mode":0}), configured).unwrap(),
            || async { Ok(snap) },
            |_, _| async { panic!("must not use another pump") },
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn water_requires_support_and_selected_connected_device_before_post() {
        let valid = json!({"capabilities":{"water_mode":true},"pump":{"1/Connected":1,"1/Mode":0,"2/Connected":1,"2/Mode":2}});
        for which in ["pump", "valve"] {
            let (result, posts) = request(
                "water_mode",
                json!({"which":which,"mode":1}),
                Ok(serde_json::from_value(valid.clone()).unwrap()),
            )
            .await;
            result.unwrap();
            assert_eq!(
                posts,
                vec![(
                    "water_mode",
                    json!({"instance":if which == "pump" {1} else {2},"mode":1})
                )]
            );
        }
        for invalid in [
            json!({}),
            json!({"capabilities":{"water_mode":false},"pump":{"1/Connected":1,"1/Mode":0}}),
            json!({"capabilities":{"water_mode":true},"pump":{"2/Connected":1,"2/Mode":0}}),
            json!({"capabilities":{"water_mode":true},"pump":{"1/Connected":0,"1/Mode":0}}),
            json!({"capabilities":{"water_mode":true},"pump":{"1/Connected":null,"1/Mode":0}}),
            json!({"capabilities":{"water_mode":true},"pump":{"1/Connected":true,"1/Mode":0}}),
            json!({"capabilities":{"water_mode":true},"pump":{"1/Connected":"1","1/Mode":0}}),
            json!({"capabilities":{"water_mode":true},"pump":{"1/Connected":1,"1/Mode":null}}),
        ] {
            let (result, posts) = request(
                "water_mode",
                json!({"which":"pump","mode":1}),
                Ok(serde_json::from_value(invalid).unwrap()),
            )
            .await;
            assert!(result.is_err());
            assert!(posts.is_empty());
        }
    }
}

#[cfg(test)]
mod override_tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    fn status(
        value: Option<i32>,
        request_id: Option<&str>,
        error: Option<&str>,
    ) -> GatewaySnapshot {
        serde_json::from_value(json!({
            "capabilities":{"setpoint_override":true},
            "inverter":{"setpoint_override":{"value":value,"request_id":request_id,"last_error":error}}
        })).unwrap()
    }

    #[tokio::test]
    async fn override_success_requires_matching_acknowledgement_including_stop() {
        for value in [Some(-500), Some(i32::MIN), Some(i32::MAX), None] {
            let reads = RefCell::new(VecDeque::from([
                status(None, None, None),
                status(value, Some("old-request"), None),
                status(value, Some("current-request"), None),
            ]));
            let posts = Cell::new(0);
            let result = set_override_with(
                value,
                "current-request",
                || {
                    let next = reads.borrow_mut().pop_front().expect("bounded reads");
                    async { Ok(next) }
                },
                |body| {
                    posts.set(posts.get() + 1);
                    assert_eq!(body, json!({"value":value,"request_id":"current-request"}));
                    async { Ok(()) }
                },
                || Ok(()),
            )
            .await
            .unwrap();
            assert_eq!(result.value, value);
            assert_eq!(result.request_id.as_deref(), Some("current-request"));
            assert_eq!(posts.get(), 1);
            assert!(reads.borrow().is_empty());
        }
    }

    #[tokio::test]
    async fn daemon_error_or_mismatched_value_is_not_success() {
        for (ack, expected) in [
            (
                status(
                    Some(5),
                    Some("request"),
                    Some("Device rejected requested override"),
                ),
                "Device rejected requested override",
            ),
            (
                status(Some(6), Some("request"), None),
                "Cerbo acknowledged a different override value; check its current status",
            ),
        ] {
            let reads = RefCell::new(VecDeque::from([status(None, None, None), ack]));
            let posts = Cell::new(0);
            let error = set_override_with(
                Some(5),
                "request",
                || {
                    let snap = reads.borrow_mut().pop_front().unwrap();
                    async { Ok(snap) }
                },
                |_| {
                    posts.set(posts.get() + 1);
                    async { Ok(()) }
                },
                || Ok(()),
            )
            .await
            .unwrap_err();
            assert_eq!(error, expected);
            assert_eq!(posts.get(), 1);
        }
    }

    #[tokio::test]
    async fn missing_capability_or_stale_controller_prevents_post() {
        for snap in [
            GatewaySnapshot::default(),
            serde_json::from_value(json!({"capabilities":{"setpoint_override":true},"inverter":{"setpoint_override":null}})).unwrap(),
            serde_json::from_value(
                json!({"capabilities":{"setpoint_override":true},"inverter":null}),
            )
            .unwrap(),
        ] {
            let error = set_override_with(
                Some(1),
                "request",
                || async { Ok(snap.clone()) },
                |_| async { panic!("unsupported control must not post") },
                || Ok(()),
            )
            .await
            .unwrap_err();
            assert!(error.contains("gateway") || error.contains("unavailable"));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn queue_acceptance_and_an_old_ack_never_count_as_confirmation() {
        let posts = Cell::new(0);
        let started = Instant::now();
        let result = set_override_with(
            Some(1),
            "new",
            || async { Ok(status(None, Some("old"), None)) },
            |_| {
                posts.set(posts.get() + 1);
                async { Ok(()) }
            },
            || Ok(()),
        )
        .await;
        assert_eq!(result.unwrap_err(), OVERRIDE_UNCONFIRMED);
        assert_eq!(posts.get(), 1);
        assert_eq!(Instant::now() - started, OVERRIDE_DEADLINE);
    }

    #[tokio::test(start_paused = true)]
    async fn total_deadline_bounds_preflight_post_and_each_acknowledgement_read() {
        let reads = Cell::new(0);
        let posts = Cell::new(0);
        let started = Instant::now();
        let result = set_override_with(
            Some(1),
            "request",
            || {
                reads.set(reads.get() + 1);
                let first = reads.get() == 1;
                async move {
                    tokio::time::sleep(Duration::from_secs(if first { 1 } else { 4 })).await;
                    Ok(status(None, None, None))
                }
            },
            |_| {
                posts.set(posts.get() + 1);
                async {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    Ok(())
                }
            },
            || Ok(()),
        )
        .await;
        assert_eq!(result.unwrap_err(), OVERRIDE_UNCONFIRMED);
        assert_eq!(posts.get(), 1);
        assert_eq!(reads.get(), 2);
        assert_eq!(Instant::now() - started, OVERRIDE_DEADLINE);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_preflight_sends_nothing_and_a_hung_post_is_not_repeated() {
        let started = Instant::now();
        let result = set_override_with(
            Some(1),
            "request",
            || async { std::future::pending::<Result<GatewaySnapshot, String>>().await },
            |_| async { panic!("preflight did not complete") },
            || Ok(()),
        )
        .await;
        assert_eq!(result.unwrap_err(), OVERRIDE_UNCONFIRMED);
        assert_eq!(Instant::now() - started, OVERRIDE_DEADLINE);
        let posts = Cell::new(0);
        let result = set_override_with(
            Some(1),
            "request",
            || async { Ok(status(None, None, None)) },
            |_| {
                posts.set(posts.get() + 1);
                async { std::future::pending::<Result<(), String>>().await }
            },
            || Ok(()),
        )
        .await;
        assert_eq!(result.unwrap_err(), OVERRIDE_UNCONFIRMED);
        assert_eq!(posts.get(), 1);
    }

    #[tokio::test]
    async fn stopped_gateway_during_preflight_or_ack_cannot_post_or_claim_success() {
        for stop_at in [1, 2] {
            let client = crate::gateway::idle_test_client();
            let auth = client.http_auth();
            let reads = Cell::new(0);
            let posts = Cell::new(0);
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
            let mut signal = Some((started_tx, resume_rx));
            let operation = set_override_with(
                Some(1),
                "request",
                || {
                    reads.set(reads.get() + 1);
                    let pause = if reads.get() == stop_at {
                        signal.take()
                    } else {
                        None
                    };
                    let ack = reads.get() == 2;
                    async move {
                        if let Some((started, resume)) = pause {
                            started.send(()).unwrap();
                            resume.await.unwrap();
                        }
                        Ok(status(
                            if ack { Some(1) } else { None },
                            if ack { Some("request") } else { None },
                            None,
                        ))
                    }
                },
                |_| {
                    posts.set(posts.get() + 1);
                    async { Ok(()) }
                },
                || auth.ensure_active(),
            );
            let stop = async {
                started_rx.await.unwrap();
                client.stop();
                resume_tx.send(()).unwrap();
            };
            let (result, ()) = tokio::join!(operation, stop);
            assert!(result.unwrap_err().contains("connection changed"));
            assert_eq!(posts.get(), stop_at - 1);
        }
    }

    #[tokio::test]
    async fn network_failure_or_controller_invalidation_after_post_never_reposts() {
        for failure in [
            Err("snapshot HTTP 503".to_string()),
            Ok(GatewaySnapshot::default()),
            Ok(serde_json::from_value(json!({"capabilities":{"setpoint_override":true},"inverter":{"setpoint_override":null}})).unwrap()),
        ] {
            let reads = RefCell::new(VecDeque::from([Ok(status(None, None, None)), failure]));
            let posts = Cell::new(0);
            let result = set_override_with(
                Some(1),
                "request",
                || {
                    let snap = reads.borrow_mut().pop_front().unwrap();
                    async { snap }
                },
                |_| {
                    posts.set(posts.get() + 1);
                    async { Ok(()) }
                },
                || Ok(()),
            )
            .await;
            assert!(result.is_err());
            assert_eq!(posts.get(), 1);
        }
        let posts = Cell::new(0);
        let result = set_override_with(
            Some(1),
            "request",
            || async { Ok(status(None, None, None)) },
            |_| {
                posts.set(posts.get() + 1);
                async { Err("POST response lost".into()) }
            },
            || Ok(()),
        )
        .await;
        assert_eq!(result.unwrap_err(), "POST response lost");
        assert_eq!(posts.get(), 1);
    }
}
