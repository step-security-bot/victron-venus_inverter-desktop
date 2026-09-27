//! Bounded declarative settings, independent of the webview and core configuration.

use super::protocol::{PluginManifest, PluginPermission, WorkerConfiguration};
use super::settings_store::SettingsData;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const MAX_FIELDS: usize = 32;
// Selected-entity lists may exceed 4 KiB; the complete settings envelope still
// has the independent 32 KiB configuration limit, including JSON escaping.
const MAX_STRING: usize = 24 * 1024;

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SettingType {
    String,
    Boolean,
    Number,
    Integer,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSchema {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    properties: BTreeMap<String, RawField>,
    #[serde(default)]
    required: Vec<String>,
    #[serde(rename = "additionalProperties")]
    additional_properties: Option<bool>,
    title: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawField {
    #[serde(rename = "type")]
    kind: SettingType,
    title: Option<String>,
    description: Option<String>,
    #[serde(default, rename = "writeOnly")]
    secret: bool,
    #[serde(default, rename = "omitEmpty")]
    omit_empty: bool,
    #[serde(default, rename = "omitDefault")]
    omit_default: bool,
    #[serde(rename = "enum")]
    choices: Option<Vec<String>>,
    minimum: Option<f64>,
    maximum: Option<f64>,
    #[serde(rename = "minLength")]
    min_length: Option<usize>,
    #[serde(rename = "maxLength")]
    max_length: Option<usize>,
    #[serde(default, deserialize_with = "present_default")]
    default: Option<Value>,
    #[serde(rename = "x-editor")]
    editor: Option<JsonEditor>,
    #[serde(rename = "x-options-source")]
    options_source: Option<String>,
    #[serde(default, rename = "x-options-prefixes")]
    options_prefixes: Vec<String>,
    #[serde(default, rename = "x-options-multiple")]
    options_multiple: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JsonEditor {
    kind: String,
    schema: Value,
}

fn valid_options(source: Option<&str>, prefixes: &[String], multiple: bool) -> bool {
    if source.is_none() {
        return prefixes.is_empty() && !multiple;
    }
    source.is_some_and(|source| {
        !source.is_empty()
            && source.len() <= 64
            && source
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-.:".contains(&byte))
    }) && prefixes.len() <= 16
        && prefixes.iter().all(|prefix| {
            !prefix.is_empty() && prefix.len() <= 128 && !prefix.chars().any(char::is_control)
        })
}

fn valid_editor(editor: &JsonEditor) -> bool {
    let mut remaining = 256;
    editor.kind == "json"
        && editor.schema.is_object()
        && editor_schema(&editor.schema, 0, &mut remaining)
}

fn editor_schema(schema: &Value, depth: usize, remaining: &mut usize) -> bool {
    if depth > 8 || *remaining == 0 {
        return false;
    }
    *remaining -= 1;
    let Some(object) = schema.as_object() else {
        return false;
    };
    let allowed = [
        "type",
        "title",
        "description",
        "properties",
        "required",
        "additionalProperties",
        "items",
        "minItems",
        "maxItems",
        "maxProperties",
        "minLength",
        "maxLength",
        "minimum",
        "maximum",
        "enum",
        "default",
        "const",
        "x-options-source",
        "x-options-prefixes",
        "x-options-multiple",
    ];
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return false;
    }
    let kind = schema["type"].as_str().unwrap_or("");
    if !["object", "array", "string", "boolean", "number", "integer"].contains(&kind) {
        return false;
    }
    for (key, bound) in [("title", 128), ("description", 512)] {
        if object
            .get(key)
            .is_some_and(|value| value.as_str().is_none_or(|text| text.len() > bound))
        {
            return false;
        }
    }
    for (key, bound) in [
        ("minItems", 128),
        ("maxItems", 128),
        ("maxProperties", 128),
        ("minLength", MAX_STRING),
        ("maxLength", MAX_STRING),
    ] {
        if object
            .get(key)
            .is_some_and(|value| value.as_u64().is_none_or(|n| n > bound as u64))
        {
            return false;
        }
    }
    for (min, max) in [
        ("minItems", "maxItems"),
        ("minLength", "maxLength"),
        ("minimum", "maximum"),
    ] {
        if object.get(min).is_some_and(|v| v.as_f64().is_none())
            || object.get(max).is_some_and(|v| v.as_f64().is_none())
        {
            return false;
        }
        if let (Some(min), Some(max)) = (schema[min].as_f64(), schema[max].as_f64()) {
            if min > max {
                return false;
            }
        }
    }
    let source = object.get("x-options-source").and_then(Value::as_str);
    if object.contains_key("x-options-source") && source.is_none() {
        return false;
    }
    let prefixes: Vec<String> = match object.get("x-options-prefixes") {
        None => Vec::new(),
        Some(value) => match serde_json::from_value(value.clone()) {
            Ok(value) => value,
            Err(_) => return false,
        },
    };
    let multiple = match object.get("x-options-multiple") {
        None => false,
        Some(Value::Bool(value)) => *value,
        _ => return false,
    };
    if !valid_options(source, &prefixes, multiple)
        || (source.is_some() && (kind != "string" || object.contains_key("enum")))
    {
        return false;
    }
    if let Some(values) = object.get("enum") {
        let Some(values) = values.as_array() else {
            return false;
        };
        let mut scalar_schema = schema.clone();
        if let Some(scalar) = scalar_schema.as_object_mut() {
            scalar.remove("enum");
            scalar.remove("default");
            scalar.remove("const");
        }
        if values.is_empty()
            || values.len() > 128
            || values.iter().enumerate().any(|(index, value)| {
                (!value.is_string() && !value.is_number() && !value.is_boolean())
                    || values[..index].contains(value)
                    || !json_value_matches(value, &scalar_schema)
            })
        {
            return false;
        }
    }
    for (allowed_kind, keys) in [
        (
            "object",
            &[
                "properties",
                "required",
                "additionalProperties",
                "maxProperties",
            ][..],
        ),
        ("array", &["items", "minItems", "maxItems"][..]),
        ("string", &["minLength", "maxLength"][..]),
    ] {
        if kind != allowed_kind && keys.iter().any(|key| object.contains_key(*key)) {
            return false;
        }
    }
    if !["number", "integer"].contains(&kind)
        && ["minimum", "maximum"]
            .iter()
            .any(|key| object.contains_key(*key))
    {
        return false;
    }
    if kind == "object" {
        let properties = match object.get("properties") {
            None => None,
            Some(Value::Object(value)) => Some(value),
            _ => return false,
        };
        if properties.is_some_and(|values| {
            values.len() > 128
                || values.iter().any(|(key, value)| {
                    key.is_empty()
                        || key.len() > 64
                        || matches!(key.as_str(), "__proto__" | "constructor" | "prototype")
                        || !editor_schema(value, depth + 1, remaining)
                })
        }) {
            return false;
        }
        if let Some(required) = object.get("required") {
            let Some(required) = required.as_array() else {
                return false;
            };
            if required.len() > 128
                || required.iter().any(|key| {
                    key.as_str().is_none_or(|key| {
                        !properties.is_some_and(|properties| properties.contains_key(key))
                    })
                })
            {
                return false;
            }
        }
        if let Some(additional) = object.get("additionalProperties") {
            if !additional.is_boolean() && !editor_schema(additional, depth + 1, remaining) {
                return false;
            }
        }
    } else if kind == "array" {
        if !editor_schema(&schema["items"], depth + 1, remaining) {
            return false;
        }
    } else if object.contains_key("properties")
        || object.contains_key("items")
        || object.contains_key("additionalProperties")
        || object.contains_key("required")
    {
        return false;
    }
    for key in ["default", "const"] {
        if object
            .get(key)
            .is_some_and(|value| !json_value_matches(value, schema))
        {
            return false;
        }
    }
    true
}

fn json_value_matches(value: &Value, schema: &Value) -> bool {
    if schema
        .get("const")
        .is_some_and(|expected| value != expected)
    {
        return false;
    }
    if schema
        .get("enum")
        .and_then(Value::as_array)
        .is_some_and(|items| !items.contains(value))
    {
        return false;
    }
    match schema["type"].as_str() {
        Some("object") => value.as_object().is_some_and(|object| {
            if object.len() > 128
                || object.keys().any(|key| {
                    key.is_empty()
                        || key.len() > 128
                        || matches!(key.as_str(), "__proto__" | "constructor" | "prototype")
                })
            {
                return false;
            }
            if schema["maxProperties"]
                .as_u64()
                .is_some_and(|max| object.len() as u64 > max)
            {
                return false;
            }
            if schema["required"].as_array().is_some_and(|required| {
                required
                    .iter()
                    .any(|key| key.as_str().is_none_or(|key| !object.contains_key(key)))
            }) {
                return false;
            }
            object.iter().all(|(key, value)| {
                if let Some(field) = schema["properties"].get(key) {
                    json_value_matches(value, field)
                } else if schema["additionalProperties"] == Value::Bool(false) {
                    false
                } else if schema["additionalProperties"].is_object() {
                    json_value_matches(value, &schema["additionalProperties"])
                } else {
                    true
                }
            })
        }),
        Some("array") => value.as_array().is_some_and(|items| {
            schema["minItems"]
                .as_u64()
                .is_none_or(|min| items.len() as u64 >= min)
                && schema["maxItems"]
                    .as_u64()
                    .is_none_or(|max| items.len() as u64 <= max)
                && items.len() <= 128
                && items
                    .iter()
                    .all(|item| json_value_matches(item, &schema["items"]))
        }),
        Some("string") => value.as_str().is_some_and(|text| {
            text.len() <= MAX_STRING
                && schema["minLength"]
                    .as_u64()
                    .is_none_or(|min| text.chars().count() as u64 >= min)
                && schema["maxLength"]
                    .as_u64()
                    .is_none_or(|max| text.chars().count() as u64 <= max)
        }),
        Some("boolean") => value.is_boolean(),
        Some("number" | "integer") => value.as_f64().is_some_and(|number| {
            number.is_finite()
                && (schema["type"] != "integer"
                    || (number.fract() == 0.0 && number.abs() <= 9_007_199_254_740_991.0))
                && schema["minimum"].as_f64().is_none_or(|min| number >= min)
                && schema["maximum"].as_f64().is_none_or(|max| number <= max)
        }),
        _ => false,
    }
}

fn present_default<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

#[derive(Clone, Serialize)]
pub(crate) struct SettingField {
    key: String,
    title: String,
    description: Option<String>,
    #[serde(rename = "type")]
    kind: SettingType,
    required: bool,
    secret: bool,
    #[serde(rename = "enum")]
    choices: Option<Vec<String>>,
    minimum: Option<f64>,
    maximum: Option<f64>,
    min_length: Option<usize>,
    max_length: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    editor: Option<JsonEditor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options_source: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    options_prefixes: Vec<String>,
    options_multiple: bool,
    #[serde(skip)]
    default: Option<Value>,
    #[serde(skip)]
    omit_empty: bool,
    #[serde(skip)]
    omit_default: bool,
}

#[derive(Serialize)]
pub(crate) struct PluginSettingsView {
    plugin_id: String,
    version: String,
    revision: String,
    fields: Vec<SettingField>,
    values: BTreeMap<String, Value>,
    secret_present: BTreeMap<String, bool>,
}

pub(crate) struct SettingsSchema {
    fields: Vec<SettingField>,
    video_windows: bool,
}

// Package schemas cannot declare keys beginning with '_'. This host preference
// shares the encrypted settings transaction but never enters worker configuration.
pub(crate) const VIDEO_ALWAYS_ON_TOP: &str = "_host_video_always_on_top";

pub(crate) fn video_always_on_top(data: &SettingsData) -> Result<bool, String> {
    if data.secret_fields.contains(VIDEO_ALWAYS_ON_TOP)
        || data.secrets.contains_key(VIDEO_ALWAYS_ON_TOP)
    {
        return Err("Invalid video window preference classification".into());
    }
    match data.values.get(VIDEO_ALWAYS_ON_TOP) {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err("Invalid video window preference".into()),
    }
}

fn video_window_field() -> SettingField {
    SettingField {
        key: VIDEO_ALWAYS_ON_TOP.into(),
        title: "Always on top".into(),
        description: Some("Keep this plugin's video windows above other windows.".into()),
        kind: SettingType::Boolean,
        required: false,
        secret: false,
        choices: None,
        minimum: None,
        maximum: None,
        min_length: None,
        max_length: None,
        editor: None,
        options_source: None,
        options_prefixes: Vec::new(),
        options_multiple: false,
        default: Some(Value::Bool(false)),
        omit_empty: false,
        omit_default: false,
    }
}

fn bounded_text(text: &Option<String>, maximum: usize) -> bool {
    text.as_ref().is_none_or(|text| text.len() <= maximum)
}

impl SettingsSchema {
    pub(crate) fn compile(manifest: &PluginManifest) -> Result<Self, String> {
        if !manifest
            .permissions
            .contains(&PluginPermission::PluginConfiguration)
        {
            return Err("Plugin does not declare configuration permission".into());
        }
        // Never return serde errors: malformed schema defaults could contain secrets.
        let raw: RawSchema = serde_json::from_value(manifest.config_schema.clone())
            .map_err(|_| "Unsupported plugin settings schema")?;
        if raw.kind != "object"
            || raw.additional_properties == Some(true)
            || raw.properties.len() > MAX_FIELDS
            || raw.required.len() > MAX_FIELDS
            || !bounded_text(&raw.title, 128)
            || !bounded_text(&raw.description, 512)
        {
            return Err("Unsupported plugin settings schema".into());
        }
        let required: BTreeSet<_> = raw.required.iter().collect();
        if required.len() != raw.required.len()
            || required
                .iter()
                .any(|key| !raw.properties.contains_key(*key))
        {
            return Err("Invalid required plugin settings fields".into());
        }
        let mut fields = Vec::new();
        for (key, raw) in raw.properties {
            if key.is_empty()
                || key.len() > 64
                || !key.as_bytes()[0].is_ascii_alphanumeric()
                || !key
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-.".contains(&c))
                || matches!(key.as_str(), "__proto__" | "constructor" | "prototype")
                || !bounded_text(&raw.title, 128)
                || !bounded_text(&raw.description, 512)
                || (raw.secret
                    && (raw.kind != SettingType::String
                        || raw.default.is_some()
                        || raw.choices.is_some()))
                || (raw.omit_empty
                    && (required.contains(&key)
                        || raw.secret
                        || raw.kind != SettingType::String
                        || raw.default.as_ref().and_then(Value::as_str) != Some("")))
                || (raw.omit_default
                    && (required.contains(&key) || raw.secret || raw.default.is_none()))
                || (raw.editor.is_some()
                    && (raw.kind != SettingType::String
                        || raw.choices.is_some()
                        || raw.options_source.is_some()))
                || raw
                    .editor
                    .as_ref()
                    .is_some_and(|editor| !valid_editor(editor))
                || !valid_options(
                    raw.options_source.as_deref(),
                    &raw.options_prefixes,
                    raw.options_multiple,
                )
                || (raw.options_source.is_some()
                    && (raw.kind != SettingType::String || raw.secret || raw.choices.is_some()))
                || (raw.kind != SettingType::String
                    && (raw.choices.is_some()
                        || raw.min_length.is_some()
                        || raw.max_length.is_some()))
                || (!matches!(raw.kind, SettingType::Number | SettingType::Integer)
                    && (raw.minimum.is_some() || raw.maximum.is_some()))
                || raw.min_length.is_some_and(|n| n > MAX_STRING)
                || raw.max_length.is_some_and(|n| n > MAX_STRING)
                || raw
                    .min_length
                    .zip(raw.max_length)
                    .is_some_and(|(min, max)| min > max)
                || raw
                    .minimum
                    .zip(raw.maximum)
                    .is_some_and(|(min, max)| min > max)
                || raw.minimum.is_some_and(|n| !n.is_finite())
                || raw.maximum.is_some_and(|n| !n.is_finite())
                || raw.choices.as_ref().is_some_and(|items| {
                    items.is_empty()
                        || items.len() > 32
                        || items.iter().any(|item| item.len() > MAX_STRING)
                        || items.iter().collect::<BTreeSet<_>>().len() != items.len()
                })
            {
                return Err("Unsupported plugin settings field".into());
            }
            let field = SettingField {
                required: required.contains(&key),
                title: raw.title.unwrap_or_else(|| key.clone()),
                key,
                description: raw.description,
                kind: raw.kind,
                secret: raw.secret,
                choices: raw.choices,
                minimum: raw.minimum,
                maximum: raw.maximum,
                min_length: raw.min_length,
                max_length: raw.max_length,
                default: raw.default,
                omit_empty: raw.omit_empty,
                omit_default: raw.omit_default,
                editor: raw.editor,
                options_source: raw.options_source,
                options_prefixes: raw.options_prefixes,
                options_multiple: raw.options_multiple,
            };
            if let Some(choices) = &field.choices {
                for choice in choices {
                    field.validate(&Value::String(choice.clone()))?;
                }
            }
            if let Some(default) = &field.default {
                field.validate(default)?;
            }
            fields.push(field);
        }
        Ok(Self {
            fields,
            video_windows: manifest.permissions.iter().any(|permission| {
                matches!(
                    permission,
                    PluginPermission::HttpVideo | PluginPermission::LiveView
                )
            }),
        })
    }

    fn check_classification(&self, data: &SettingsData) -> Result<(), String> {
        if self.fields.iter().any(|field| {
            (field.secret && data.values.contains_key(&field.key))
                || (!field.secret
                    && (data.secret_fields.contains(&field.key)
                        || data.secrets.contains_key(&field.key)))
        }) {
            return Err("Plugin settings contain incompatible secret classifications".into());
        }
        Ok(())
    }

    fn public_values(&self, data: &SettingsData) -> BTreeMap<String, Value> {
        self.fields
            .iter()
            .filter(|field| !field.secret)
            .filter_map(|field| {
                data.values
                    .get(&field.key)
                    .or(field.default.as_ref())
                    .map(|value| (field.key.clone(), value.clone()))
            })
            .collect()
    }

    pub(crate) fn view(
        &self,
        manifest: &PluginManifest,
        digest: &str,
        data: &SettingsData,
    ) -> Result<PluginSettingsView, String> {
        self.check_classification(data)?;
        let mut fields = self.fields.clone();
        let mut values = self.public_values(data);
        if self.video_windows {
            fields.push(video_window_field());
            values.insert(
                VIDEO_ALWAYS_ON_TOP.into(),
                Value::Bool(video_always_on_top(data)?),
            );
        }
        Ok(PluginSettingsView {
            plugin_id: manifest.plugin_id.clone(),
            version: manifest.version.clone(),
            revision: format!("{digest}:{}", data.revision),
            fields,
            values,
            secret_present: self
                .fields
                .iter()
                .filter(|field| field.secret)
                .map(|field| (field.key.clone(), data.secrets.contains_key(&field.key)))
                .collect(),
        })
    }

    pub(crate) fn merge(
        &self,
        digest: &str,
        current: &SettingsData,
        revision: &str,
        mut values: BTreeMap<String, Value>,
        changes: BTreeMap<String, Option<String>>,
    ) -> Result<SettingsData, String> {
        self.check_classification(current)?;
        if revision != format!("{digest}:{}", current.revision) {
            return Err("Plugin or settings changed; reopen settings before saving".into());
        }
        let video_preference = values.remove(VIDEO_ALWAYS_ON_TOP);
        if video_preference
            .as_ref()
            .is_some_and(|value| !self.video_windows || !value.is_boolean())
        {
            return Err("Invalid video window preference".into());
        }
        if values.len() > MAX_FIELDS
            || changes.len() > MAX_FIELDS
            || values.keys().any(|key| {
                !self
                    .fields
                    .iter()
                    .any(|field| &field.key == key && !field.secret)
            })
            || changes.keys().any(|key| {
                !self
                    .fields
                    .iter()
                    .any(|field| &field.key == key && field.secret)
            })
        {
            return Err("Unknown or incorrectly classified plugin setting".into());
        }
        let mut next = current.clone();
        // An older editor omits host preferences; keep the saved choice intact.
        if let Some(value) = video_preference {
            next.values.insert(VIDEO_ALWAYS_ON_TOP.into(), value);
        }
        for field in &self.fields {
            // Preserve fields absent from this schema for rollback and future migration.
            next.values.remove(&field.key);
            if field.secret {
                next.secret_fields.insert(field.key.clone());
                if let Some(change) = changes.get(&field.key) {
                    match change {
                        Some(value) => {
                            next.secrets.insert(field.key.clone(), value.clone());
                        }
                        None => {
                            next.secrets.remove(&field.key);
                        }
                    }
                }
            } else if let Some(value) = values
                .get(&field.key)
                .or(field.default.as_ref())
                .filter(|value| !field.omit_empty || value.as_str() != Some(""))
                .filter(|value| !field.omit_default || Some(*value) != field.default.as_ref())
            {
                next.values.insert(field.key.clone(), value.clone());
            }
        }
        self.validate_data(&next)?;
        if next.values != current.values
            || next.secrets != current.secrets
            || next.secret_fields != current.secret_fields
        {
            next.revision = uuid::Uuid::new_v4().to_string();
        }
        Ok(next)
    }

    fn validate_data(&self, data: &SettingsData) -> Result<(), String> {
        self.validate_fields(data, true)
    }

    fn validate_fields(&self, data: &SettingsData, require_complete: bool) -> Result<(), String> {
        self.check_classification(data)?;
        video_always_on_top(data)?;
        for field in &self.fields {
            let secret;
            let value = if field.secret {
                secret = data
                    .secrets
                    .get(&field.key)
                    .map(|value| Value::String(value.clone()));
                secret.as_ref()
            } else {
                data.values.get(&field.key).or(field.default.as_ref())
            };
            match value {
                Some(value) => field.validate(value)?,
                None if field.required && require_complete => {
                    return Err(format!("Required plugin setting is missing: {}", field.key))
                }
                None => {}
            }
        }
        Ok(())
    }

    /// Portable restores intentionally omit secrets but still validate supplied
    /// values, field classification, and the complete encoded envelope budget.
    pub(crate) fn seed_configuration(
        &self,
        data: &SettingsData,
    ) -> Result<WorkerConfiguration, String> {
        self.configuration_impl(data, false)
    }

    pub(crate) fn configuration(&self, data: &SettingsData) -> Result<WorkerConfiguration, String> {
        self.configuration_impl(data, true)
    }

    fn configuration_impl(
        &self,
        data: &SettingsData,
        require_complete: bool,
    ) -> Result<WorkerConfiguration, String> {
        self.validate_fields(data, require_complete)?;
        let mut values = self.public_values(data);
        for field in &self.fields {
            if (field.omit_empty && values.get(&field.key).and_then(Value::as_str) == Some(""))
                || (field.omit_default && values.get(&field.key) == field.default.as_ref())
            {
                // Opted-in defaults stay visible without consuming startup bytes.
                values.remove(&field.key);
            }
        }
        let configuration = WorkerConfiguration {
            revision: data.revision.clone(),
            values: serde_json::to_value(values).map_err(|_| "Cannot encode plugin settings")?,
            secrets: self
                .fields
                .iter()
                .filter(|field| field.secret)
                .filter_map(|field| {
                    data.secrets
                        .get(&field.key)
                        .map(|value| (field.key.clone(), value.clone()))
                })
                .collect(),
        };
        configuration.validate()?;
        Ok(configuration)
    }
}

impl SettingField {
    fn validate(&self, value: &Value) -> Result<(), String> {
        let valid = match self.kind {
            SettingType::String => value.as_str().is_some_and(|value| {
                let length = value.chars().count();
                value.len() <= MAX_STRING
                    && self.editor.as_ref().is_none_or(|editor| {
                        // Empty optional settings retain their existing omit-empty semantics.
                        (value.is_empty() && !self.required)
                            || serde_json::from_str::<Value>(value)
                                .is_ok_and(|value| json_value_matches(&value, &editor.schema))
                    })
                    && self.min_length.is_none_or(|min| length >= min)
                    && self.max_length.is_none_or(|max| length <= max)
                    && self
                        .choices
                        .as_ref()
                        .is_none_or(|items| items.iter().any(|item| item == value))
            }),
            SettingType::Boolean => value.is_boolean(),
            SettingType::Number | SettingType::Integer => value.as_f64().is_some_and(|value| {
                value.is_finite()
                    && (self.kind != SettingType::Integer
                        || (value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0))
                    && self.minimum.is_none_or(|min| value >= min)
                    && self.maximum.is_none_or(|max| value <= max)
            }),
        };
        if valid {
            Ok(())
        } else {
            Err(format!("Invalid plugin setting: {}", self.key))
        }
    }
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;
