use super::*;
use serde_json::json;

fn manifest(schema: Value) -> PluginManifest {
    PluginManifest {
        group: None,
        live_view: None,
        schema_version: 1,
        plugin_id: "org.example.settings".into(),
        version: "1.0.0".into(),
        host_api: "^1.1".into(),
        target: "aarch64-apple-darwin".into(),
        entrypoint: "worker".into(),
        config_schema: schema,
        permissions: vec![PluginPermission::PluginConfiguration],
        http_video: None,
        inventory: vec![],
        signature: None,
    }
}

fn configured_manifest() -> PluginManifest {
    manifest(
        json!({"type":"object","additionalProperties":false,"properties":{
        "endpoint":{"type":"string","minLength":1,"maxLength":100,"default":"https://example.invalid"},
        "token":{"type":"string","writeOnly":true,"minLength":1},
        "port":{"type":"integer","minimum":1,"maximum":65535,"default":443},
        "enabled":{"type":"boolean","default":true},
        "mode":{"type":"string","enum":["local","remote"],"default":"remote"}
    },"required":["endpoint","token"]}),
    )
}

fn saved(schema: &SettingsSchema) -> SettingsData {
    schema
        .merge(
            "archive",
            &SettingsData::default(),
            "archive:0",
            BTreeMap::new(),
            BTreeMap::from([("token".into(), Some("private-token-value".into()))]),
        )
        .unwrap()
}

#[test]
fn video_window_preference_defaults_off_and_stays_out_of_worker_configuration() {
    for permission in [PluginPermission::HttpVideo, PluginPermission::LiveView] {
        let mut metadata = configured_manifest();
        metadata.permissions.push(permission);
        let schema = SettingsSchema::compile(&metadata).unwrap();
        let initial = saved(&schema);
        let view =
            serde_json::to_value(schema.view(&metadata, "archive", &initial).unwrap()).unwrap();
        assert_eq!(view["values"][VIDEO_ALWAYS_ON_TOP], false);
        assert!(view["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| { field["key"] == VIDEO_ALWAYS_ON_TOP && field["type"] == "boolean" }));
        let enabled = schema
            .merge(
                "archive",
                &initial,
                &format!("archive:{}", initial.revision),
                BTreeMap::from([(VIDEO_ALWAYS_ON_TOP.into(), json!(true))]),
                BTreeMap::new(),
            )
            .unwrap();
        assert!(video_always_on_top(&enabled).unwrap());
        assert_ne!(enabled.revision, initial.revision);
        assert_eq!(enabled.secrets, initial.secrets);
        assert_eq!(
            schema.configuration(&enabled).unwrap().values,
            schema.configuration(&initial).unwrap().values
        );
        let restored: SettingsData =
            serde_json::from_value(serde_json::to_value(&enabled).unwrap()).unwrap();
        assert!(video_always_on_top(&restored).unwrap());
        let legacy_save = schema
            .merge(
                "archive",
                &restored,
                &format!("archive:{}", restored.revision),
                BTreeMap::new(),
                BTreeMap::new(),
            )
            .unwrap();
        assert!(video_always_on_top(&legacy_save).unwrap());
        assert_eq!(legacy_save.revision, restored.revision);
        assert!(schema
            .merge(
                "archive",
                &enabled,
                &format!("archive:{}", initial.revision),
                BTreeMap::from([(VIDEO_ALWAYS_ON_TOP.into(), json!(false))]),
                BTreeMap::new(),
            )
            .is_err());
        let disabled = schema
            .merge(
                "archive",
                &enabled,
                &format!("archive:{}", enabled.revision),
                BTreeMap::from([(VIDEO_ALWAYS_ON_TOP.into(), json!(false))]),
                BTreeMap::new(),
            )
            .unwrap();
        assert!(!video_always_on_top(&disabled).unwrap());
    }
}

#[test]
fn video_window_preference_rejects_invalid_types_classification_and_non_video_plugins() {
    let metadata = configured_manifest();
    let schema = SettingsSchema::compile(&metadata).unwrap();
    let initial = saved(&schema);
    let view = serde_json::to_value(schema.view(&metadata, "archive", &initial).unwrap()).unwrap();
    assert!(view["values"].get(VIDEO_ALWAYS_ON_TOP).is_none());
    assert!(schema
        .merge(
            "archive",
            &initial,
            &format!("archive:{}", initial.revision),
            BTreeMap::from([(VIDEO_ALWAYS_ON_TOP.into(), json!(true))]),
            BTreeMap::new(),
        )
        .is_err());
    let mut metadata = metadata;
    metadata.permissions.push(PluginPermission::HttpVideo);
    let schema = SettingsSchema::compile(&metadata).unwrap();
    for invalid in [json!("true"), json!(1), Value::Null, json!({})] {
        assert!(schema
            .merge(
                "archive",
                &initial,
                &format!("archive:{}", initial.revision),
                BTreeMap::from([(VIDEO_ALWAYS_ON_TOP.into(), invalid.clone())]),
                BTreeMap::new(),
            )
            .is_err());
        let mut corrupted = initial.clone();
        corrupted.values.insert(VIDEO_ALWAYS_ON_TOP.into(), invalid);
        assert!(schema.view(&metadata, "archive", &corrupted).is_err());
        assert!(schema.seed_configuration(&corrupted).is_err());
    }
    let mut secret = initial;
    secret.secret_fields.insert(VIDEO_ALWAYS_ON_TOP.into());
    assert!(video_always_on_top(&secret).is_err());
    assert!(schema.view(&metadata, "archive", &secret).is_err());
    metadata.config_schema["properties"][VIDEO_ALWAYS_ON_TOP] = json!({"type":"boolean"});
    assert!(SettingsSchema::compile(&metadata).is_err());
}

#[test]
fn missing_settings_show_defaults_without_revealing_secrets_or_requiring_setup() {
    let metadata = configured_manifest();
    let schema = SettingsSchema::compile(&metadata).unwrap();
    let data = SettingsData::default();
    let view = serde_json::to_value(schema.view(&metadata, "archive", &data).unwrap()).unwrap();
    assert_eq!(view["values"]["endpoint"], "https://example.invalid");
    assert_eq!(view["values"]["port"], 443);
    assert_eq!(view["secret_present"]["token"], false);
    assert_eq!(view["revision"], "archive:0");
    assert!(view["values"].get("token").is_none());
    assert!(schema.configuration(&data).is_err());
}

#[test]
fn secret_replacement_keep_clear_and_required_validation() {
    let metadata = configured_manifest();
    let schema = SettingsSchema::compile(&metadata).unwrap();
    let initial = saved(&schema);
    let view =
        serde_json::to_string(&schema.view(&metadata, "archive", &initial).unwrap()).unwrap();
    assert!(!view.contains("private-token-value"));
    let config = schema.configuration(&initial).unwrap();
    assert_eq!(config.secrets["token"], "private-token-value");
    assert!(config.values.get("token").is_none());
    assert!(!format!("{config:?}").contains("private-token-value"));
    let unchanged = schema
        .merge(
            "archive",
            &initial,
            &format!("archive:{}", initial.revision),
            BTreeMap::new(),
            BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(unchanged.revision, initial.revision);
    assert_eq!(unchanged.secrets, initial.secrets);
    assert!(schema
        .merge(
            "archive",
            &initial,
            &format!("archive:{}", initial.revision),
            BTreeMap::new(),
            BTreeMap::from([("token".into(), None)])
        )
        .is_err());
    let optional = SettingsSchema::compile(&manifest(
        json!({"type":"object","properties":{"token":{"type":"string","writeOnly":true}}}),
    ))
    .unwrap();
    let cleared = optional
        .merge(
            "archive",
            &initial,
            &format!("archive:{}", initial.revision),
            BTreeMap::new(),
            BTreeMap::from([("token".into(), None)]),
        )
        .unwrap();
    assert!(cleared.secrets.is_empty());
    assert!(cleared.secret_fields.contains("token"));
    assert_ne!(cleared.revision, initial.revision);
}

#[test]
fn optimistic_revision_binds_settings_and_installed_archive() {
    let schema = SettingsSchema::compile(&configured_manifest()).unwrap();
    let current = saved(&schema);
    for revision in [
        "archive:0".to_owned(),
        format!("other:{}", current.revision),
    ] {
        assert!(schema
            .merge(
                "archive",
                &current,
                &revision,
                BTreeMap::new(),
                BTreeMap::new()
            )
            .is_err());
    }
}

#[test]
fn secret_classification_survives_clear_and_schema_downgrade() {
    let schema = SettingsSchema::compile(&configured_manifest()).unwrap();
    let mut data = saved(&schema);
    data.secrets.clear();
    let public_manifest =
        manifest(json!({"type":"object","properties":{"token":{"type":"string"}}}));
    let public = SettingsSchema::compile(&public_manifest).unwrap();
    assert!(public.view(&public_manifest, "archive", &data).is_err());
    assert!(public.configuration(&data).is_err());
    assert!(public
        .merge(
            "archive",
            &data,
            &format!("archive:{}", data.revision),
            BTreeMap::new(),
            BTreeMap::new()
        )
        .is_err());
}

#[test]
fn unknown_stored_fields_survive_but_are_never_sent_to_worker_or_ui() {
    let metadata = configured_manifest();
    let schema = SettingsSchema::compile(&metadata).unwrap();
    let mut current = saved(&schema);
    current
        .values
        .insert("future-setting".into(), json!({"nested":42}));
    current
        .secrets
        .insert("future-token".into(), "future-secret".into());
    current.secret_fields.insert("future-token".into());
    let next = schema
        .merge(
            "archive",
            &current,
            &format!("archive:{}", current.revision),
            BTreeMap::new(),
            BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(
        next.values["future-setting"],
        current.values["future-setting"]
    );
    assert_eq!(next.secrets["future-token"], "future-secret");
    assert!(schema
        .configuration(&next)
        .unwrap()
        .values
        .get("future-setting")
        .is_none());
    assert!(
        !serde_json::to_string(&schema.view(&metadata, "archive", &next).unwrap())
            .unwrap()
            .contains("future-")
    );
}

#[test]
fn rejects_cross_namespace_writes_and_never_quotes_secret_values_in_errors() {
    let schema = SettingsSchema::compile(&configured_manifest()).unwrap();
    let current = saved(&schema);
    let revision = format!("archive:{}", current.revision);
    for key in ["token", "unknown", "__proto__"] {
        assert!(schema
            .merge(
                "archive",
                &current,
                &revision,
                BTreeMap::from([(key.into(), json!("private"))]),
                BTreeMap::new()
            )
            .is_err());
    }
    for key in ["endpoint", "unknown"] {
        assert!(schema
            .merge(
                "archive",
                &current,
                &revision,
                BTreeMap::new(),
                BTreeMap::from([(key.into(), Some("private".into()))])
            )
            .is_err());
    }
    let secret = "unique-private".repeat(MAX_STRING / "unique-private".len() + 1);
    let error = schema
        .merge(
            "archive",
            &current,
            &revision,
            BTreeMap::new(),
            BTreeMap::from([("token".into(), Some(secret.clone()))]),
        )
        .err()
        .unwrap();
    assert!(!error.contains("unique-private"));
}

#[test]
fn primitive_types_enums_and_numeric_and_unicode_bounds_are_enforced() {
    let schema = SettingsSchema::compile(&configured_manifest()).unwrap();
    let current = saved(&schema);
    let revision = format!("archive:{}", current.revision);
    for (key, value) in [
        ("port", json!(1.5)),
        ("port", json!(0)),
        ("port", json!(65536)),
        ("enabled", json!("true")),
        ("mode", json!("unsupported")),
        ("endpoint", json!("")),
        ("endpoint", json!("x".repeat(101))),
    ] {
        assert!(schema
            .merge(
                "archive",
                &current,
                &revision,
                BTreeMap::from([(key.into(), value)]),
                BTreeMap::new()
            )
            .is_err());
    }
    let unicode = SettingsSchema::compile(&manifest(json!({"type":"object","properties":{"text":{"type":"string","minLength":2,"maxLength":2}}}))).unwrap();
    assert!(unicode
        .merge(
            "archive",
            &SettingsData::default(),
            "archive:0",
            BTreeMap::from([("text".into(), json!("ёж"))]),
            BTreeMap::new()
        )
        .is_ok());
}

#[test]
fn expanded_entity_lists_keep_scalar_and_complete_configuration_limits() {
    let schema = SettingsSchema::compile(&manifest(json!({
        "type":"object","properties": {
            "entities":{"type":"string","maxLength":8255},
            "notes":{"type":"string","maxLength":MAX_STRING}
        }
    })))
    .unwrap();
    let entities = (0..64)
        .map(|index| format!("sensor.{index:02}{}", "x".repeat(119)))
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(entities.len(), 8255);
    let data = schema
        .merge(
            "archive",
            &SettingsData::default(),
            "archive:0",
            BTreeMap::from([("entities".into(), json!(entities))]),
            BTreeMap::new(),
        )
        .unwrap();
    let configuration = schema.configuration(&data).unwrap();
    assert_eq!(
        configuration.values["entities"].as_str().unwrap().len(),
        8255
    );
    assert!(schema
        .merge(
            "archive",
            &SettingsData::default(),
            "archive:0",
            BTreeMap::from([("notes".into(), json!("x".repeat(MAX_STRING + 1)))]),
            BTreeMap::new()
        )
        .is_err());
    let escaped = schema
        .merge(
            "archive",
            &SettingsData::default(),
            "archive:0",
            BTreeMap::from([("notes".into(), json!("\n".repeat(MAX_STRING)))]),
            BTreeMap::new(),
        )
        .unwrap();
    assert!(
        schema.configuration(&escaped).is_err(),
        "escaping must not bypass the complete 32 KiB envelope"
    );
}

#[test]
fn unsupported_or_ambiguous_schemas_fail_closed() {
    let fields = [
        json!({"type":"string","writeOnly":true,"default":null}),
        json!({"type":"string","default":null}),
        json!({"type":"string","enum":["a"],"minLength":2}),
        json!({"type":"array"}),
        json!({"type":"object"}),
        json!({"type":"string","pattern":".*"}),
        json!({"type":"string","$ref":"remote"}),
        json!({"type":"string","writeOnly":true,"default":"never-echo-this"}),
        json!({"type":"string","writeOnly":true,"enum":["never-echo-this"]}),
        json!({"type":"boolean","writeOnly":true}),
        json!({"type":"boolean","minimum":1}),
        json!({"type":"integer","maxLength":10}),
        json!({"type":"string","minLength":9,"maxLength":2}),
        json!({"type":"integer","minimum":9,"maximum":2}),
        json!({"type":"string","enum":["a","a"]}),
        json!({"type":"integer","default":"bad"}),
        json!({"type":"string","maxLength":MAX_STRING + 1}),
    ];
    for field in fields {
        let result = SettingsSchema::compile(&manifest(
            json!({"type":"object","properties":{"test":field}}),
        ));
        assert!(result.is_err());
        assert!(!result.err().unwrap().contains("never-echo-this"));
    }
    for schema in [
        json!({"type":"object","additionalProperties":true}),
        json!({"type":"object","required":["missing"]}),
        json!({"type":"object","allOf":[]}),
        json!({"type":"object","properties":{"__proto__":{"type":"string"}}}),
        json!({"type":"object","properties":{"_token":{"type":"string","writeOnly":true}}}),
    ] {
        assert!(SettingsSchema::compile(&manifest(schema)).is_err());
    }
    let mut metadata = configured_manifest();
    metadata.permissions.clear();
    assert!(SettingsSchema::compile(&metadata).is_err());
}

#[test]
fn integer_values_outside_javascript_safe_range_are_rejected() {
    let schema = SettingsSchema::compile(&manifest(
        json!({"type":"object","properties":{"count":{"type":"integer"}}}),
    ))
    .unwrap();
    assert!(schema
        .merge(
            "archive",
            &SettingsData::default(),
            "archive:0",
            BTreeMap::from([("count".into(), json!(9_007_199_254_740_992u64))]),
            BTreeMap::new()
        )
        .is_err());
}

#[test]
fn omit_empty_requires_an_optional_public_string_with_an_explicit_empty_default() {
    for field in [
        json!({"type":"string","omitEmpty":true}),
        json!({"type":"string","default":null,"omitEmpty":true}),
        json!({"type":"string","default":"selected","omitEmpty":true}),
        json!({"type":"string","writeOnly":true,"default":"","omitEmpty":true}),
        json!({"type":"integer","default":0,"omitEmpty":true}),
        json!({"type":"number","default":0,"omitEmpty":true}),
        json!({"type":"boolean","default":false,"omitEmpty":true}),
        json!({"type":"string","default":"","omitEmpty":"true"}),
        json!({"type":"string","default":"","omitEmpty":null}),
    ] {
        assert!(SettingsSchema::compile(&manifest(json!({
            "type":"object","properties":{"selection":field}
        })))
        .is_err());
    }
    assert!(SettingsSchema::compile(&manifest(json!({
        "type":"object","required":["selection"],"properties":{
            "selection":{"type":"string","default":"","omitEmpty":true}
        }
    })))
    .is_err());
    for field in [
        json!({"type":"string","default":"","omitEmpty":false}),
        json!({"type":"string","default":"selected","omitEmpty":false}),
        json!({"type":"string","writeOnly":true,"omitEmpty":false}),
        json!({"type":"integer","default":0,"omitEmpty":false}),
        json!({"type":"boolean","default":false,"omitEmpty":false}),
    ] {
        assert!(SettingsSchema::compile(&manifest(json!({
            "type":"object","required":["selection"],"properties":{"selection":field}
        })))
        .is_ok());
    }
}

#[test]
fn omit_empty_preserves_effective_values_and_only_canonicalizes_opted_in_empty_strings() {
    let metadata = manifest(json!({"type":"object","properties":{
        "selection":{"type":"string","default":"","omitEmpty":true},
        "legacy":{"type":"string","default":""},
        "explicit_false":{"type":"string","default":"","omitEmpty":false}
    }}));
    let schema = SettingsSchema::compile(&metadata).unwrap();
    let mut initial = SettingsData::default();
    initial.values.insert("selection".into(), json!(""));
    initial
        .values
        .insert("unknown".into(), json!({"keep":true}));
    let view = serde_json::to_value(schema.view(&metadata, "archive", &initial).unwrap()).unwrap();
    assert_eq!(view["values"]["selection"], "");
    assert!(view["fields"].as_array().unwrap().iter().all(|field| {
        field.get("omitEmpty").is_none()
            && field.get("omit_empty").is_none()
            && field.get("default").is_none()
    }));
    assert_eq!(
        schema.configuration(&initial).unwrap().values,
        json!({
            "legacy":"","explicit_false":""
        })
    );
    assert_eq!(initial.values["selection"], "");
    let saved = schema
        .merge(
            "archive",
            &initial,
            "archive:0",
            BTreeMap::from([("selection".into(), json!(""))]),
            BTreeMap::new(),
        )
        .unwrap();
    assert!(!saved.values.contains_key("selection"));
    assert_eq!(saved.values["legacy"], "");
    assert_eq!(saved.values["explicit_false"], "");
    assert_eq!(saved.values["unknown"], initial.values["unknown"]);
    assert_eq!(
        schema.view(&metadata, "archive", &saved).unwrap().values["selection"],
        ""
    );
    for value in ["number.room", " "] {
        let updated = schema
            .merge(
                "archive",
                &saved,
                &format!("archive:{}", saved.revision),
                BTreeMap::from([("selection".into(), json!(value))]),
                BTreeMap::new(),
            )
            .unwrap();
        assert_eq!(updated.values["selection"], value);
        assert_eq!(
            schema.configuration(&updated).unwrap().values["selection"],
            value
        );
    }
}

const HA_DISHWASHER_ROLES: [&str; 2] = ["dishwasher_running_entity", "dishwasher_duration_entity"];

const HA_LAUNDRY_ROLES: [&str; 2] = ["washer_remaining_entity", "dryer_remaining_entity"];

const HA_APPLIANCE_ROLES: [&str; 4] = [
    "dishwasher_running_entity",
    "dishwasher_duration_entity",
    "washer_remaining_entity",
    "dryer_remaining_entity",
];

const HA_OMIT_EMPTY_SELECTIONS: [&str; 7] = [
    "number_entities",
    "cover_position_entities",
    "discovery_prefixes",
    "dishwasher_running_entity",
    "dishwasher_duration_entity",
    "washer_remaining_entity",
    "dryer_remaining_entity",
];

fn ha_optional_selection_upgrade(
    additions: &[&str],
) -> (PluginManifest, SettingsSchema, SettingsData) {
    let package: Value = serde_json::from_str(include_str!(
        "../../../scripts/plugins/home-assistant-manifest.json"
    ))
    .unwrap();
    let metadata = manifest(package["config_schema"].clone());
    let mut previous_metadata = metadata.clone();
    let properties = previous_metadata.config_schema["properties"]
        .as_object_mut()
        .unwrap();
    for &key in additions {
        assert_eq!(metadata.config_schema["properties"][key]["omitEmpty"], true);
        properties.remove(key).unwrap();
    }
    let previous = SettingsSchema::compile(&previous_metadata).unwrap();
    let mut data = SettingsData {
        legacy_migration_version: 0,
        revision: "12345678-1234-1234-1234-123456789abc".into(),
        values: serde_json::from_value(json!({
            "ha_base_url":"http://localhost","watch_entities":"\u{b}".repeat(4096),
            "action_entities":"\u{b}".repeat(1200),"media_player_entities":"",
            "binary_entities":"light.a","cover_entities":"cover.a"
        }))
        .unwrap(),
        secrets: BTreeMap::from([("ha_token".into(), "fixture-token".into())]),
        secret_fields: BTreeSet::from(["ha_token".into()]),
    };
    // Keep selections supported by each preceding schema active, including the
    // dishwasher profile when only the new laundry roles are absent.
    for (key, value) in [
        ("number_entities", "number.a"),
        ("cover_position_entities", "cover.a"),
        ("discovery_prefixes", "sensor."),
        ("dishwasher_running_entity", "binary_sensor.dishwasher"),
        ("dishwasher_duration_entity", "sensor.dishwasher_runtime"),
    ] {
        if !additions.contains(&key) {
            data.values.insert(key.into(), json!(value));
        }
    }
    (metadata, previous, data)
}

#[test]
fn ha_optional_selections_preserve_the_exact_previous_worker_configuration_byte_boundary() {
    for additions in [
        &HA_OMIT_EMPTY_SELECTIONS[..],
        &HA_APPLIANCE_ROLES[..],
        &HA_LAUNDRY_ROLES[..],
    ] {
        let (metadata, previous, mut data) = ha_optional_selection_upgrade(additions);
        let maximum = super::super::protocol::MAX_CONFIGURATION_BYTES;
        let padding = maximum
            - serde_json::to_vec(&previous.configuration(&data).unwrap())
                .unwrap()
                .len();
        data.secrets
            .get_mut("ha_token")
            .unwrap()
            .push_str(&"x".repeat(padding));
        let original = serde_json::to_vec(&previous.configuration(&data).unwrap()).unwrap();
        assert_eq!(original.len(), maximum);
        let upgraded = SettingsSchema::compile(&metadata).unwrap();
        for explicit_empty in [false, true] {
            if explicit_empty {
                for &key in additions {
                    data.values.insert(key.into(), json!(""));
                }
            }
            assert_eq!(
                serde_json::to_vec(&upgraded.configuration(&data).unwrap()).unwrap(),
                original
            );
        }
        data.secrets.get_mut("ha_token").unwrap().push('x');
        assert!(previous.configuration(&data).is_err());
        assert!(upgraded.configuration(&data).is_err());
    }
}

#[test]
fn ha_optional_selection_upgrade_can_resave_a_previous_settings_record_at_the_storage_limit() {
    use super::super::settings_store::{SettingsStore, MAX_SETTINGS_PLAINTEXT_BYTES};
    use std::sync::Arc;

    for additions in [
        &HA_OMIT_EMPTY_SELECTIONS[..],
        &HA_APPLIANCE_ROLES[..],
        &HA_LAUNDRY_ROLES[..],
    ] {
        let (metadata, previous, mut data) = ha_optional_selection_upgrade(additions);
        let padding = MAX_SETTINGS_PLAINTEXT_BYTES - serde_json::to_vec(&data).unwrap().len();
        data.secrets
            .get_mut("ha_token")
            .unwrap()
            .push_str(&"x".repeat(padding));
        assert_eq!(
            serde_json::to_vec(&data).unwrap().len(),
            MAX_SETTINGS_PLAINTEXT_BYTES
        );
        let original = serde_json::to_vec(&previous.configuration(&data).unwrap()).unwrap();

        let directory = tempfile::tempdir().unwrap();
        let package_root = directory.path().join("package-store");
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        builder.create(&package_root).unwrap();
        let store = SettingsStore::new(
            package_root.canonicalize().unwrap(),
            Arc::new(|| Ok(vec![7; 32])),
        );
        store
            .prepare_write(&metadata.plugin_id, &data)
            .unwrap()
            .commit()
            .unwrap();
        let stored = store.read(&metadata.plugin_id).unwrap();
        assert!(stored == data);

        let mut unfiltered_metadata = metadata.clone();
        for &key in additions {
            unfiltered_metadata.config_schema["properties"][key]["omitEmpty"] = json!(false);
        }
        assert!(SettingsSchema::compile(&unfiltered_metadata)
            .unwrap()
            .configuration(&stored)
            .is_err());
        let upgraded = SettingsSchema::compile(&metadata).unwrap();
        assert_eq!(
            serde_json::to_vec(&upgraded.configuration(&stored).unwrap()).unwrap(),
            original
        );
        let view = upgraded.view(&metadata, "upgraded", &stored).unwrap();
        for &key in additions {
            assert_eq!(view.values[key], "");
        }
        let saved = upgraded
            .merge(
                "upgraded",
                &stored,
                &view.revision,
                view.values,
                BTreeMap::new(),
            )
            .unwrap();
        assert!(saved == stored);
        store
            .prepare_write(&metadata.plugin_id, &saved)
            .unwrap()
            .commit()
            .unwrap();
        assert!(store.read(&metadata.plugin_id).unwrap() == saved);
        let mut oversized = saved;
        oversized.secrets.get_mut("ha_token").unwrap().push('x');
        assert!(store
            .prepare_write(&metadata.plugin_id, &oversized)
            .is_err());
    }
}

#[test]
fn ha_dishwasher_roles_are_opt_in_and_survive_schema_rollback() {
    ha_roles_are_opt_in_and_survive_schema_rollback(
        HA_DISHWASHER_ROLES,
        [
            "binary_sensor.dishwasher_running",
            "sensor.dishwasher_elapsed",
        ],
        &HA_APPLIANCE_ROLES,
        &[],
    );
}

#[test]
fn ha_laundry_roles_are_opt_in_and_survive_schema_rollback() {
    ha_roles_are_opt_in_and_survive_schema_rollback(
        HA_LAUNDRY_ROLES,
        ["sensor.washer_remaining", "sensor.dryer_remaining"],
        &HA_LAUNDRY_ROLES,
        &[
            ("dishwasher_running_entity", "binary_sensor.dishwasher"),
            ("dishwasher_duration_entity", "sensor.dishwasher_runtime"),
        ],
    );
}

fn ha_roles_are_opt_in_and_survive_schema_rollback(
    roles: [&str; 2],
    selections: [&str; 2],
    previous_additions: &[&str],
    retained_roles: &[(&str, &str)],
) {
    let (metadata, previous, _) = ha_optional_selection_upgrade(previous_additions);
    let schema = SettingsSchema::compile(&metadata).unwrap();
    let mut values = BTreeMap::from([
        ("ha_base_url".into(), json!("https://ha.example.invalid")),
        ("watch_entities".into(), json!("sensor.existing")),
        ("action_entities".into(), json!("button.explicit")),
    ]);
    for &(key, entity) in retained_roles {
        values.insert(key.into(), json!(entity));
    }
    let initial = schema
        .merge(
            "current",
            &SettingsData::default(),
            "current:0",
            values,
            BTreeMap::from([("ha_token".into(), Some("fixture-token".into()))]),
        )
        .unwrap();
    let mut view = schema.view(&metadata, "current", &initial).unwrap();
    let initial_startup = schema.configuration(&initial).unwrap();
    for key in roles {
        assert_eq!(view.values[key], "");
        assert!(!initial.values.contains_key(key));
        assert!(initial_startup.values.get(key).is_none());
    }
    for (key, entity) in roles.into_iter().zip(selections) {
        view.values.insert(key.into(), json!(entity));
    }
    let selected = schema
        .merge(
            "current",
            &initial,
            &view.revision,
            view.values,
            BTreeMap::new(),
        )
        .unwrap();
    let startup = schema.configuration(&selected).unwrap();
    for (key, entity) in roles.into_iter().zip(selections) {
        assert_eq!(selected.values[key], entity);
        assert_eq!(startup.values[key], entity);
    }
    assert_eq!(startup.values["watch_entities"], "sensor.existing");
    assert_eq!(startup.values["action_entities"], "button.explicit");
    assert_eq!(startup.secrets, initial_startup.secrets);
    for &(key, entity) in retained_roles {
        assert_eq!(startup.values[key], entity);
    }

    // A previous worker must not receive unsupported role fields. Its settings
    // editor still preserves the user's explicit selection for a later upgrade.
    let old_view = previous.view(&metadata, "previous", &selected).unwrap();
    let old_startup = previous.configuration(&selected).unwrap();
    for key in roles {
        assert!(!old_view.values.contains_key(key));
        assert!(old_startup.values.get(key).is_none());
    }
    for &(key, entity) in retained_roles {
        assert_eq!(old_view.values[key], entity);
        assert_eq!(old_startup.values[key], entity);
    }
    assert_eq!(old_startup.secrets, initial_startup.secrets);
    let resaved = previous
        .merge(
            "previous",
            &selected,
            &old_view.revision,
            old_view.values,
            BTreeMap::new(),
        )
        .unwrap();
    assert!(resaved == selected);
    let mut restored_view = schema.view(&metadata, "restored", &resaved).unwrap();
    for (key, entity) in roles.into_iter().zip(selections) {
        assert_eq!(restored_view.values[key], entity);
        assert_eq!(schema.configuration(&resaved).unwrap().values[key], entity);
        restored_view.values.insert(key.into(), json!(""));
    }
    let cleared = schema
        .merge(
            "restored",
            &resaved,
            &restored_view.revision,
            restored_view.values,
            BTreeMap::new(),
        )
        .unwrap();
    for key in roles {
        assert!(!cleared.values.contains_key(key));
        assert!(schema
            .configuration(&cleared)
            .unwrap()
            .values
            .get(key)
            .is_none());
        assert_eq!(
            schema.view(&metadata, "cleared", &cleared).unwrap().values[key],
            ""
        );
    }
    assert_eq!(cleared.values, initial.values);
    assert_eq!(cleared.secret_fields, initial.secret_fields);
    assert_ne!(cleared.revision, resaved.revision);
    assert_eq!(
        schema.configuration(&cleared).unwrap().values,
        initial_startup.values
    );
    assert_eq!(cleared.secrets, initial.secrets);
}

#[test]
fn structured_editor_metadata_is_bounded_and_values_are_validated_without_echoing_secrets() {
    let field = json!({"type":"string","writeOnly":true,"x-editor":{"kind":"json","schema":{
        "type":"object","maxProperties":2,"additionalProperties":{"type":"string","maxLength":128}
    }}});
    let metadata = manifest(json!({"type":"object","properties":{"private_map":field}}));
    let schema = SettingsSchema::compile(&metadata).unwrap();
    let secret = r#"{"front":"https://private.invalid/live"}"#;
    let data = schema
        .merge(
            "archive",
            &SettingsData::default(),
            "archive:0",
            BTreeMap::new(),
            BTreeMap::from([("private_map".into(), Some(secret.into()))]),
        )
        .unwrap();
    let view = serde_json::to_value(schema.view(&metadata, "archive", &data).unwrap()).unwrap();
    assert_eq!(view["fields"][0]["editor"]["kind"], "json");
    assert_eq!(view["secret_present"]["private_map"], true);
    assert!(view["values"].get("private_map").is_none());
    assert!(!view.to_string().contains("private.invalid"));
    assert_eq!(
        schema.configuration(&data).unwrap().secrets["private_map"],
        secret
    );
    for invalid in [
        r#"{"front":{"private":"never-echo-this"}}"#,
        r#"{"a":"1","b":"2","c":"3"}"#,
        "never-echo-this",
    ] {
        let result = schema.merge(
            "archive",
            &data,
            &format!("archive:{}", data.revision),
            BTreeMap::new(),
            BTreeMap::from([("private_map".into(), Some(invalid.into()))]),
        );
        assert!(result.is_err());
        assert!(!result.err().unwrap().contains("never-echo-this"));
        assert_eq!(data.secrets["private_map"], secret);
    }
    for editor in [
        json!({"kind":"html","schema":{"type":"string"}}),
        json!({"kind":"json","schema":{"type":"string","$ref":"https://invalid"}}),
        json!({"kind":"json","schema":{"type":"array","maxItems":129,"items":{"type":"string"}}}),
        json!({"kind":"json","schema":{"type":"object","properties":{"__proto__":{"type":"string"}}}}),
        json!({"kind":"json","schema":{"type":"string","x-options-multiple":true}}),
        json!({"kind":"json","schema":{"type":"string","enum":[true]}}),
        json!({"kind":"json","schema":{"type":"integer","enum":[1,1]}}),
    ] {
        let metadata = manifest(
            json!({"type":"object","properties":{"layout":{"type":"string","x-editor":editor}}}),
        );
        assert!(SettingsSchema::compile(&metadata).is_err());
    }
    let mut nested = json!({"type":"string"});
    for _ in 0..10 {
        nested = json!({"type":"array","items":nested});
    }
    assert!(SettingsSchema::compile(&manifest(json!({"type":"object","properties":{"layout":{"type":"string","x-editor":{"kind":"json","schema":nested}}}}))).is_err());
}

#[test]
fn installed_ha_editor_accepts_complete_layout_and_keeps_atomic_envelope_limits() {
    let package: Value = serde_json::from_str(include_str!(
        "../../../scripts/plugins/home-assistant-manifest.json"
    ))
    .unwrap();
    let metadata = manifest(package["config_schema"].clone());
    let schema = SettingsSchema::compile(&metadata).unwrap();
    let controls: Vec<_>=(0..64).map(|index| json!({"id":format!("control-{index}"),"surface":"home","order":index,"label":"L".repeat(128),"entity":format!("light.room_{index}{}", "x".repeat(70)),"icon":"light"})).collect();
    let layout = json!({"version":1,"controls":controls,"sections":{},"appliances":{}}).to_string();
    assert!(layout.len() > 16 * 1024);
    assert!(layout.len() < 24 * 1024);
    let data = schema
        .merge(
            "archive",
            &SettingsData::default(),
            "archive:0",
            BTreeMap::from([
                ("ha_base_url".into(), json!("https://ha.invalid")),
                ("dashboard_layout".into(), json!(layout)),
            ]),
            BTreeMap::from([("ha_token".into(), Some("private-token".into()))]),
        )
        .unwrap();
    schema.configuration(&data).unwrap();
    let view = serde_json::to_value(schema.view(&metadata, "archive", &data).unwrap()).unwrap();
    let fields = view["fields"].as_array().unwrap();
    let selection = fields
        .iter()
        .find(|field| field["key"] == "watch_entities")
        .unwrap();
    assert_eq!(selection["options_source"], "entities");
    assert_eq!(selection["options_multiple"], true);
    let layout_field = fields
        .iter()
        .find(|field| field["key"] == "dashboard_layout")
        .unwrap();
    assert_eq!(
        layout_field["editor"]["schema"]["properties"]["controls"]["items"]["properties"]["entity"]
            ["x-options-source"],
        "entities"
    );
    let mut values = data.values.clone();
    values.insert("dashboard_layout".into(), json!("x".repeat(MAX_STRING + 1)));
    assert!(schema
        .merge(
            "archive",
            &data,
            &format!("archive:{}", data.revision),
            values,
            BTreeMap::new()
        )
        .is_err());
    assert_eq!(data.values["dashboard_layout"], layout);
    // Each field fits its own scalar ceiling; the complete escaped worker envelope does not.
    let mut values = data.values.clone();
    values.insert("watch_entities".into(), json!("\n".repeat(8255)));
    let candidate = schema
        .merge(
            "archive",
            &data,
            &format!("archive:{}", data.revision),
            values,
            BTreeMap::new(),
        )
        .unwrap();
    assert!(schema.configuration(&candidate).is_err());
    assert!(schema.configuration(&data).is_ok());
}

#[test]
fn partial_restore_validates_supplied_values_and_never_classifies_secret_values_as_public() {
    let metadata = configured_manifest();
    let schema = SettingsSchema::compile(&metadata).unwrap();
    let mut data = SettingsData::default();
    data.values
        .insert("endpoint".into(), json!("https://restored.example.invalid"));
    schema.seed_configuration(&data).unwrap();
    assert!(schema.configuration(&data).is_err());
    data.values
        .insert("token".into(), json!("must-remain-private"));
    assert!(schema.seed_configuration(&data).is_err());
    assert!(schema.view(&metadata, "digest", &data).is_err());
    data.values.remove("token");
    data.values.insert("port".into(), json!(70000));
    assert!(schema.seed_configuration(&data).is_err());
}

#[test]
fn omitted_defaults_preserve_view_values_without_expanding_old_records_or_worker_frames() {
    let metadata = manifest(
        json!({"type":"object","additionalProperties":false,"properties":{
            "notify":{"type":"boolean","default":false,"omitDefault":true},
            "layout":{"type":"string","default":"empty-layout","omitDefault":true}
        }}),
    );
    let schema = SettingsSchema::compile(&metadata).unwrap();
    let original = SettingsData::default();
    let view = schema.view(&metadata, "archive", &original).unwrap();
    assert_eq!(view.values["notify"], false);
    assert_eq!(view.values["layout"], "empty-layout");
    let saved = schema
        .merge(
            "archive",
            &original,
            &view.revision,
            view.values,
            BTreeMap::new(),
        )
        .unwrap();
    assert!(saved == original);
    assert_eq!(schema.configuration(&saved).unwrap().values, json!({}));
    let mut changed = saved;
    changed.values.insert("notify".into(), json!(true));
    assert_eq!(
        schema.configuration(&changed).unwrap().values,
        json!({"notify":true})
    );
    for (key, field) in [
        (
            "required",
            json!({"type":"boolean","default":false,"omitDefault":true}),
        ),
        ("no-default", json!({"type":"string","omitDefault":true})),
        (
            "secret",
            json!({"type":"string","writeOnly":true,"omitDefault":true}),
        ),
    ] {
        let mut schema = json!({"type":"object","properties":{}});
        schema["properties"][key] = field;
        if key == "required" {
            schema["required"] = json!([key]);
        }
        assert!(SettingsSchema::compile(&manifest(schema)).is_err());
    }
}
