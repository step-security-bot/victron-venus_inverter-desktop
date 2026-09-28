//! Bounded, declarative protocol for desktop plugin worker processes.
//!
//! Validation here establishes a wire contract, not a process sandbox, publisher
//! trust, or verification of the files described by a manifest.

use std::collections::{BTreeMap, HashSet};

use reqwest::Url;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// Share pure endpoint validation without linking worker transport into the host.
#[path = "../../../desktop-plugins/camera-common/src/live_urls.rs"]
pub mod mqtt_live;

pub const PROTOCOL_VERSION: u32 = 1;
pub const HOST_API_VERSION: &str = "1.9.0";
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
/// Includes the newline terminating a frame.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_CONTRIBUTIONS: usize = 128;
pub const MAX_ACTION_PARAMS_BYTES: usize = 4 * 1024;
pub const MAX_ACTION_RESULT_BYTES: usize = 16 * 1024;
pub const MAX_ACTION_DEADLINE_MS: u64 = 60_000;
pub const MAX_SCALED_COEFFICIENT: i64 = 1_000_000_000_000_000;
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
pub const MAX_CONFIGURATION_BYTES: usize = 32 * 1024;
pub const MAX_NOTIFICATION_TITLE_BYTES: usize = 128;
pub const MAX_NOTIFICATION_BODY_BYTES: usize = 1024;
const MAX_JSON_DEPTH: usize = 8;
const MAX_JSON_NODES: usize = 512;
const MAX_INVENTORY_FILES: usize = 128;
const MAX_INVENTORY_BYTES: u64 = 512 * 1024 * 1024;

pub const DESKTOP_TARGETS: &[&str] = &[
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
    "aarch64-pc-windows-msvc",
    "x86_64-pc-windows-msvc",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostMessage {
    Hello {
        protocol_version: u32,
        host_api_version: String,
        plugin_id: String,
    },
    Configuration {
        configuration: WorkerConfiguration,
    },
    Action {
        request_id: String,
        action_id: String,
        params: Value,
        /// Relative deadline, in milliseconds, also enforced by the host.
        deadline_ms: u64,
    },
    Cancel {
        request_id: String,
    },
    Shutdown,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerMessage {
    Ready {
        protocol_version: u32,
        host_api_version: String,
        plugin_id: String,
    },
    ConfigurationReady {
        revision: String,
    },
    /// Replaces this worker's entire contribution snapshot.
    Contributions {
        items: Vec<DashboardContribution>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        presentation: Vec<super::presentation::Presentation>,
    },
    HttpVideo {
        id: String,
        url: String,
        title: String,
        #[serde(default, skip_serializing_if = "HttpMediaKind::is_video")]
        media_kind: HttpMediaKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cooldown_id: Option<String>,
    },
    HttpLive {
        id: String,
        url: String,
        title: String,
    },
    /// Fresh MQTT URL, scoped to a configured camera endpoint by the native host.
    MqttLive {
        id: String,
        title: String,
        camera_id: String,
        url: String,
    },
    /// Automatic preview of an exact, privately configured destination.
    LiveView {
        id: String,
        title: String,
        live_view_id: String,
    },
    Notification {
        id: String,
        title: String,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        live_view_id: Option<String>,
    },
    ActionResult {
        request_id: String,
        value: Value,
    },
    ActionError {
        request_id: String,
        code: String,
        message: String,
    },
    /// Worker-scoped data only; never a host command or a Tauri event name.
    Event {
        name: String,
        data: Value,
    },
}

/// The historical `http_video` frame also supports explicitly typed snapshots.
/// An omitted kind retains the original video wire representation.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HttpMediaKind {
    #[default]
    Video,
    Jpeg,
    Png,
    Webp,
}

impl HttpMediaKind {
    pub fn is_video(&self) -> bool {
        *self == Self::Video
    }
}

// Diagnostics never render worker data, including private media URLs/titles.
impl std::fmt::Debug for WorkerMessage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("WorkerMessage { contents: [redacted] }")
    }
}

/// Delivered only to the owning worker through its authenticated startup pipe.
/// Debug output deliberately omits both maps, including accidental secrets in values.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WorkerConfiguration {
    pub revision: String,
    pub values: Value,
    pub secrets: BTreeMap<String, String>,
}

impl std::fmt::Debug for WorkerConfiguration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("WorkerConfiguration { contents: [redacted] }")
    }
}

impl WorkerConfiguration {
    pub fn validate(&self) -> Result<(), String> {
        token(&self.revision, "configuration revision")?;
        if !self.values.is_object() {
            return Err("worker configuration values must be an object".into());
        }
        bounded_json(&self.values, MAX_CONFIGURATION_BYTES)?;
        for key in self.secrets.keys() {
            token(key, "secret key")?;
            if self.values.get(key).is_some() {
                return Err("worker values and secrets must have separate keys".into());
            }
        }
        if self.secrets.len() > MAX_JSON_NODES
            || serde_json::to_vec(self)
                .map_err(|_| "cannot encode worker configuration")?
                .len()
                > MAX_CONFIGURATION_BYTES
        {
            return Err("worker configuration exceeds byte or field limit".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DashboardContribution {
    Text {
        id: String,
        title: String,
        text: String,
    },
    Metric {
        id: String,
        title: String,
        value: f64,
        unit: Option<String>,
    },
    Status {
        id: String,
        title: String,
        value: String,
        tone: StatusTone,
    },
    Action {
        id: String,
        title: String,
        /// Presentation-only reference to a read-only item in this snapshot.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state_id: Option<String>,
        action_id: String,
        label: String,
        params: Value,
    },
    NumberInput {
        id: String,
        title: String,
        /// Presentation-only reference; never part of a numeric grant.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state_id: Option<String>,
        action_id: String,
        label: String,
        unit: Option<String>,
        input_revision: String,
        value_scaled: i64,
        min_scaled: i64,
        max_scaled: i64,
        step_scaled: i64,
        decimal_places: u8,
    },
}

/// Numeric authority excludes observed value and presentation-only name/label/group.
/// Comparing every constraint also protects against a worker reusing a revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NumberInputGrant {
    pub id: String,
    pub action_id: String,
    pub unit: Option<String>,
    pub input_revision: String,
    pub min_scaled: i64,
    pub max_scaled: i64,
    pub step_scaled: i64,
    pub decimal_places: u8,
}

fn scaled_coefficient(value: i64) -> bool {
    (-MAX_SCALED_COEFFICIENT..=MAX_SCALED_COEFFICIENT).contains(&value)
}

fn scaled_value(value: i64, minimum: i64, maximum: i64, step: i64) -> bool {
    scaled_coefficient(value)
        && step > 0
        && value >= minimum
        && value <= maximum
        && value
            .checked_sub(minimum)
            .is_some_and(|delta| delta % step == 0)
}

impl NumberInputGrant {
    pub fn accepts(&self, params: &Value) -> bool {
        params.as_object().is_some_and(|params| {
            params.len() == 2
                && params.get("input_revision").and_then(Value::as_str)
                    == Some(self.input_revision.as_str())
                && params
                    .get("value_scaled")
                    .and_then(Value::as_i64)
                    .is_some_and(|value| {
                        scaled_value(value, self.min_scaled, self.max_scaled, self.step_scaled)
                    })
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StatusTone {
    Neutral,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub schema_version: u32,
    pub plugin_id: String,
    pub version: String,
    /// Semantic version requirement for the host API, independent of app version.
    pub host_api: String,
    pub target: String,
    pub entrypoint: String,
    /// Bounded JSON Schema metadata. No schema is fetched or executed here.
    pub config_schema: Value,
    pub permissions: Vec<PluginPermission>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_video: Option<HttpVideoDeclaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_view: Option<LiveViewDeclaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<PluginGroup>,
    pub inventory: Vec<InventoryEntry>,
    /// Optional for local development. Presence does not establish authenticity.
    pub signature: Option<SignatureMetadata>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PluginPermission {
    DashboardContributions,
    PluginConfiguration,
    DesktopNotifications,
    HttpVideo,
    LiveView,
    NetworkHttp,
    NetworkMqtt,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HttpVideoDeclaration {
    pub base_url_setting: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_preview: Option<LivePreviewDeclaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token_setting: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_seconds: Option<u16>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_query: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LivePreviewDeclaration {
    pub query: String,
    pub max_duration_seconds: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LiveViewDeclaration {
    pub urls_setting: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mqtt_urls_setting: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_duration_seconds: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PluginGroup {
    pub id: String,
    pub title: String,
    pub icon: String,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Default)]
pub struct LiveViewGrant {
    urls: BTreeMap<String, reqwest::Url>,
    mqtt_urls: mqtt_live::CameraLiveUrls,
    preview_duration_seconds: Option<u16>,
}

impl std::fmt::Debug for LiveViewGrant {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LiveViewGrant { destinations: [redacted] }")
    }
}

pub(crate) fn validate_live_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || value.trim() != value
        || value
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '+' | '#'))
    {
        return Err("Invalid live view identity".into());
    }
    Ok(())
}

impl LiveViewGrant {
    pub(crate) fn from_manifest_configuration(
        manifest: &PluginManifest,
        configuration: Option<&WorkerConfiguration>,
    ) -> Result<Option<Self>, String> {
        manifest.validate_http_video()?;
        let Some(declaration) = &manifest.live_view else {
            return Ok(None);
        };
        let configuration = configuration.ok_or("Live view startup configuration missing")?;
        let mqtt_urls = mqtt_live::CameraLiveUrls::parse(
            declaration
                .mqtt_urls_setting
                .as_ref()
                .and_then(|key| configuration.values.get(key))
                .map(|value| {
                    value
                        .as_str()
                        .ok_or("Invalid MQTT live camera configuration")
                })
                .transpose()?,
        )?;
        let Some(value) = configuration
            .secrets
            .get(&declaration.urls_setting)
            .filter(|value| !value.trim().is_empty())
        else {
            return Ok(Some(Self {
                mqtt_urls,
                preview_duration_seconds: declaration.preview_duration_seconds,
                ..Self::default()
            }));
        };
        if value.len() > 16 * 1024 {
            return Err("Live view configuration exceeds byte limit".into());
        }
        let values: BTreeMap<String, String> =
            serde_json::from_str(value).map_err(|_| "Invalid live view configuration")?;
        if values.len() > 32 {
            return Err("Too many live view destinations".into());
        }
        let mut urls = BTreeMap::new();
        for (id, value) in values {
            validate_live_id(&id)?;
            if value.len() > 2048
                || value.trim() != value
                || value.chars().any(char::is_control)
                || value.contains('\\')
            {
                return Err("Invalid live view destination".into());
            }
            let authority = value
                .split_once("://")
                .map(|(_, tail)| tail.split(['/', '?', '#']).next().unwrap_or_default())
                .ok_or("Invalid live view destination")?;
            if authority.contains('@') {
                return Err("Invalid live view destination".into());
            }
            let url = reqwest::Url::parse(&value).map_err(|_| "Invalid live view destination")?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err("Invalid live view destination".into());
            }
            urls.insert(id, url);
        }
        Ok(Some(Self {
            urls,
            mqtt_urls,
            preview_duration_seconds: declaration.preview_duration_seconds,
        }))
    }

    pub(crate) fn resolve(&self, id: &str) -> Option<reqwest::Url> {
        self.urls.get(id).cloned()
    }

    pub(crate) fn preview(
        &self,
        id: &str,
    ) -> Result<Option<(reqwest::Url, HttpVideoGrant)>, String> {
        let duration = self
            .preview_duration_seconds
            .ok_or("Automatic live view is not authorized")?;
        Ok(self
            .resolve(id)
            .map(|url| Self::exact_preview(url, duration)))
    }

    pub(crate) fn mqtt_preview(
        &self,
        id: &str,
        value: &str,
    ) -> Result<(reqwest::Url, HttpVideoGrant), String> {
        let duration = self
            .preview_duration_seconds
            .filter(|seconds| (1..=15).contains(seconds))
            .ok_or("MQTT live preview is not authorized")?;
        let url = self.mqtt_urls.resolve(id, value)?;
        Ok(Self::exact_preview(url, duration))
    }

    fn exact_preview(url: reqwest::Url, duration: u16) -> (reqwest::Url, HttpVideoGrant) {
        let grant = HttpVideoGrant {
            base: url.clone(),
            live_preview: None,
            exact_preview: Some((url.clone(), duration)),
            bearer_token: None,
            cooldown_seconds: 15,
            allow_query: false,
        };
        (url, grant)
    }
}

/// Only native installation may derive a grant from verified startup settings.
#[derive(Clone)]
pub struct HttpVideoGrant {
    base: reqwest::Url,
    live_preview: Option<LivePreviewDeclaration>,
    /// Derived only from the private native map; never grants downloads or a prefix.
    exact_preview: Option<(reqwest::Url, u16)>,
    bearer_token: Option<String>,
    cooldown_seconds: u16,
    allow_query: bool,
}
impl std::fmt::Debug for HttpVideoGrant {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("HttpVideoGrant { base: [redacted] }")
    }
}

fn validated_video_url(value: &str) -> Result<reqwest::Url, String> {
    validated_media_url(value, false)
}

fn validated_media_url(value: &str, allow_query: bool) -> Result<reqwest::Url, String> {
    if value.is_empty()
        || value.len() > 2048
        || value.trim() != value
        || value.chars().any(char::is_control)
        || value.contains('\\')
    {
        return Err("Invalid HTTP video URL".into());
    }
    if !value.contains("://") {
        return Err("Invalid HTTP video URL".into());
    }
    let url = reqwest::Url::parse(value).map_err(|_| "Invalid HTTP video URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || (!allow_query && url.query().is_some())
        || url.fragment().is_some()
    {
        return Err("Invalid HTTP video URL".into());
    }
    // Reject alternate encodings of path separators and traversal before any
    // server-specific decoding can reinterpret the allowed prefix.
    let raw_path = value
        .split_once("://")
        .and_then(|(_, tail)| tail.find('/').map(|i| &tail[i..]))
        .unwrap_or("")
        .split(['?', '#'])
        .next()
        .unwrap_or("");
    for segment in raw_path.split('/') {
        let lower = segment.to_ascii_lowercase();
        if matches!(segment, "." | "..")
            || lower.contains("%2e")
            || lower.contains("%2f")
            || lower.contains("%5c")
            || lower.contains("%25")
        {
            return Err("Invalid HTTP video URL path".into());
        }
        let mut decoded = Vec::with_capacity(segment.len());
        let mut bytes = segment.bytes();
        while let Some(byte) = bytes.next() {
            if byte == b'%' {
                let high = bytes.next().and_then(|b| (b as char).to_digit(16));
                let low = bytes.next().and_then(|b| (b as char).to_digit(16));
                let (Some(high), Some(low)) = (high, low) else {
                    return Err("Invalid HTTP video URL path".into());
                };
                decoded.push((high * 16 + low) as u8);
            } else {
                decoded.push(byte);
            }
        }
        if std::str::from_utf8(&decoded).map_or(true, |text| text.chars().any(char::is_control)) {
            return Err("Invalid HTTP video URL path".into());
        }
    }
    Ok(url)
}

impl HttpVideoGrant {
    pub(crate) fn from_manifest_configuration(
        manifest: &PluginManifest,
        configuration: Option<&WorkerConfiguration>,
    ) -> Result<Option<Self>, String> {
        manifest.validate_http_video()?;
        let Some(declaration) = &manifest.http_video else {
            return Ok(None);
        };
        let configuration = configuration.ok_or("HTTP video startup configuration missing")?;
        if configuration
            .secrets
            .contains_key(&declaration.base_url_setting)
        {
            return Err("HTTP video base cannot be secret".into());
        }
        let Some(value) = configuration
            .values
            .get(&declaration.base_url_setting)
            .filter(|value| !value.is_null())
        else {
            return Ok(None);
        };
        let base = value.as_str().ok_or("HTTP video base must be a string")?;
        if base.trim().is_empty() {
            return Ok(None);
        }
        let bearer_token = declaration
            .bearer_token_setting
            .as_ref()
            .and_then(|key| configuration.secrets.get(key))
            .filter(|value| !value.is_empty())
            .cloned();
        if bearer_token.as_ref().is_some_and(|value| {
            value.len() > 4096
                || reqwest::header::HeaderValue::from_str(&format!("Bearer {value}")).is_err()
        }) {
            return Err("Invalid media credential".into());
        }
        Ok(Some(Self {
            base: validated_video_url(base)?,
            live_preview: declaration.live_preview.clone(),
            exact_preview: None,
            bearer_token,
            cooldown_seconds: declaration.cooldown_seconds.unwrap_or(45),
            allow_query: declaration.allow_query,
        }))
    }

    pub fn validate_url(&self, value: &str) -> Result<reqwest::Url, String> {
        if self.exact_preview.is_some() {
            return Err("Mapped live view does not authorize downloads".into());
        }
        let url = validated_media_url(value, self.allow_query)?;
        let prefix = self.base.path().trim_end_matches('/');
        if url.origin() != self.base.origin() || !url.path().starts_with(&format!("{prefix}/")) {
            return Err("HTTP video URL is outside its configured scope".into());
        }
        Ok(url)
    }

    pub(crate) fn preview_duration(&self) -> Option<std::time::Duration> {
        if let Some((_, seconds)) = &self.exact_preview {
            return Some(std::time::Duration::from_secs((*seconds).into()));
        }
        self.live_preview
            .as_ref()
            .map(|policy| std::time::Duration::from_secs(policy.max_duration_seconds.into()))
    }

    pub(crate) fn validate_preview_url(&self, value: &str) -> Result<reqwest::Url, String> {
        if let Some((url, _)) = &self.exact_preview {
            return if value == url.as_str() {
                Ok(url.clone())
            } else {
                Err("Live preview URL is outside its configured scope".into())
            };
        }
        let policy = self
            .live_preview
            .as_ref()
            .ok_or("Live preview is not authorized")?;
        let url = validated_media_url(value, true)?;
        let prefix = self.base.path().trim_end_matches('/');
        if self.bearer_token.is_some()
            || url.origin() != self.base.origin()
            || !url.path().starts_with(&format!("{prefix}/"))
            || url.query() != Some(policy.query.as_str())
        {
            return Err("Live preview URL is outside its configured scope".into());
        }
        Ok(url)
    }

    pub(crate) fn bearer_token(&self) -> Option<&str> {
        self.bearer_token.as_deref()
    }
    pub(crate) fn cooldown(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.cooldown_seconds.into())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InventoryEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SignatureMetadata {
    pub algorithm: String,
    pub key_id: String,
    /// A lowercase hexadecimal Ed25519 signature. It is not verified here.
    pub signature: String,
}

fn token(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value.as_bytes()[0].is_ascii_alphanumeric()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
    {
        return Err(format!("invalid {label}"));
    }
    Ok(())
}

pub fn validate_plugin_id(value: &str) -> Result<(), String> {
    if value.len() > 128 || !value.contains('.') {
        return Err("plugin_id must be a namespaced lowercase identifier".into());
    }
    for part in value.split('.') {
        if part.is_empty()
            || !part.as_bytes()[0].is_ascii_lowercase()
            || part.ends_with('-')
            || !part
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err("invalid plugin_id component".into());
        }
    }
    Ok(())
}

fn label(value: &str, name: &str, limit: usize) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > limit || value.chars().any(char::is_control) {
        return Err(format!("invalid {name}"));
    }
    Ok(())
}

fn text(value: &str, name: &str, limit: usize) -> Result<(), String> {
    if value.len() > limit
        || value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(format!("invalid {name}"));
    }
    Ok(())
}

fn visit_json(
    value: &Value,
    depth: usize,
    nodes: &mut usize,
    maximum_depth: usize,
) -> Result<(), String> {
    *nodes += 1;
    if depth > maximum_depth || *nodes > MAX_JSON_NODES {
        return Err("JSON value exceeds depth or node limit".into());
    }
    match value {
        Value::Array(values) => {
            for child in values {
                visit_json(child, depth + 1, nodes, maximum_depth)?;
            }
        }
        Value::Object(values) => {
            for (key, child) in values {
                if key.len() > 128 || key.chars().any(char::is_control) {
                    return Err("invalid JSON object key".into());
                }
                visit_json(child, depth + 1, nodes, maximum_depth)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn bounded_json(value: &Value, limit: usize) -> Result<(), String> {
    bounded_json_depth(value, limit, MAX_JSON_DEPTH)
}

fn bounded_json_depth(value: &Value, limit: usize, maximum_depth: usize) -> Result<(), String> {
    visit_json(value, 0, &mut 0, maximum_depth)?;
    let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    if bytes.len() > limit {
        return Err("JSON value exceeds byte limit".into());
    }
    Ok(())
}

pub fn validate_action_params(params: &Value) -> Result<(), String> {
    if !params.is_object() {
        return Err("action params must be an object".into());
    }
    bounded_json(params, MAX_ACTION_PARAMS_BYTES)
}

fn validate_versions(protocol_version: u32, host_api_version: &str) -> Result<(), String> {
    if protocol_version != PROTOCOL_VERSION {
        return Err("unsupported worker protocol version".into());
    }
    // This version is the negotiated API version, not the worker's full supported
    // range. The manifest expresses that range before launching the worker.
    if host_api_version != HOST_API_VERSION {
        return Err("worker did not acknowledge the negotiated host API version".into());
    }
    Ok(())
}

fn bounded_serialized_frame<T: Serialize>(message: &T) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec(message).map_err(|error| error.to_string())?;
    if bytes.len() >= MAX_FRAME_BYTES {
        return Err("worker frame exceeds byte limit".into());
    }
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn validate_host_message(message: &HostMessage) -> Result<(), String> {
    match message {
        HostMessage::Hello {
            protocol_version,
            host_api_version,
            plugin_id,
        } => {
            validate_versions(*protocol_version, host_api_version)?;
            validate_plugin_id(plugin_id)?;
        }
        HostMessage::Configuration { configuration } => configuration.validate()?,
        HostMessage::Action {
            request_id,
            action_id,
            params,
            deadline_ms,
        } => {
            token(request_id, "request_id")?;
            token(action_id, "action_id")?;
            validate_action_params(params)?;
            if !(1..=MAX_ACTION_DEADLINE_MS).contains(deadline_ms) {
                return Err("action deadline is outside allowed range".into());
            }
        }
        HostMessage::Cancel { request_id } => token(request_id, "request_id")?,
        HostMessage::Shutdown => {}
    }
    bounded_serialized_frame(message)?;
    Ok(())
}

impl DashboardContribution {
    pub fn id(&self) -> &str {
        match self {
            Self::Text { id, .. }
            | Self::Metric { id, .. }
            | Self::Status { id, .. }
            | Self::Action { id, .. }
            | Self::NumberInput { id, .. } => id,
        }
    }

    pub(crate) fn number_input_grant(&self) -> Option<NumberInputGrant> {
        match self {
            Self::NumberInput {
                id,
                action_id,
                unit,
                input_revision,
                min_scaled,
                max_scaled,
                step_scaled,
                decimal_places,
                ..
            } => Some(NumberInputGrant {
                id: id.clone(),
                action_id: action_id.clone(),
                unit: unit.clone(),
                input_revision: input_revision.clone(),
                min_scaled: *min_scaled,
                max_scaled: *max_scaled,
                step_scaled: *step_scaled,
                decimal_places: *decimal_places,
            }),
            _ => None,
        }
    }

    fn state_id(&self) -> Option<&str> {
        match self {
            Self::Action { state_id, .. } | Self::NumberInput { state_id, .. } => {
                state_id.as_deref()
            }
            _ => None,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        token(self.id(), "contribution id")?;
        if let Some(state_id) = self.state_id() {
            token(state_id, "contribution state id")?;
        }
        let title = match self {
            Self::Text {
                title, text: body, ..
            } => {
                text(body, "contribution text", 4096)?;
                title
            }
            Self::Metric {
                title, value, unit, ..
            } => {
                if !value.is_finite() {
                    return Err("metric value must be finite".into());
                }
                if let Some(unit) = unit {
                    label(unit, "metric unit", 32)?;
                }
                title
            }
            Self::Status { title, value, .. } => {
                label(value, "status value", 4096)?;
                title
            }
            Self::Action {
                title,
                action_id,
                label: action_label,
                params,
                ..
            } => {
                token(action_id, "action_id")?;
                label(action_label, "action label", 128)?;
                validate_action_params(params)?;
                title
            }
            Self::NumberInput {
                title,
                action_id,
                label: action_label,
                unit,
                input_revision,
                value_scaled,
                min_scaled,
                max_scaled,
                step_scaled,
                decimal_places,
                ..
            } => {
                token(action_id, "action_id")?;
                token(input_revision, "input revision")?;
                label(action_label, "action label", 128)?;
                if let Some(unit) = unit {
                    label(unit, "numeric unit", 32)?;
                }
                if *decimal_places > 6
                    || !scaled_coefficient(*min_scaled)
                    || !scaled_coefficient(*max_scaled)
                    || !scaled_coefficient(*step_scaled)
                    || min_scaled > max_scaled
                    || !scaled_value(*value_scaled, *min_scaled, *max_scaled, *step_scaled)
                {
                    return Err("invalid numeric input bounds or observed value".into());
                }
                title
            }
        };
        label(title, "contribution title", 128)
    }
}

pub fn validate_contributions(items: &[DashboardContribution]) -> Result<(), String> {
    if items.len() > MAX_CONTRIBUTIONS {
        return Err("too many dashboard contributions".into());
    }
    let mut ids = HashSet::new();
    let mut actionable = HashSet::new();
    let mut numeric = HashSet::new();
    // Resolve references from the complete replacement snapshot, independent of
    // order. Read-only anchors cannot form nested groups or grant actions.
    let states: HashSet<_> = items
        .iter()
        .filter(|item| {
            matches!(
                item,
                DashboardContribution::Text { .. }
                    | DashboardContribution::Metric { .. }
                    | DashboardContribution::Status { .. }
            )
        })
        .map(DashboardContribution::id)
        .collect();
    for item in items {
        item.validate()?;
        if !ids.insert(item.id()) {
            return Err("duplicate dashboard contribution id".into());
        }
        if item.state_id().is_some_and(|id| !states.contains(id)) {
            return Err("control state id must reference a read-only contribution".into());
        }
        match item {
            DashboardContribution::NumberInput { action_id, .. } => {
                if !actionable.insert(action_id) {
                    return Err("numeric action ids must be unique".into());
                }
                numeric.insert(action_id);
            }
            DashboardContribution::Action { action_id, .. } => {
                if numeric.contains(action_id) {
                    return Err("numeric action ids must be unique".into());
                }
                // Existing static aliases may intentionally share one action id.
                actionable.insert(action_id);
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn validate_worker_message(message: &WorkerMessage) -> Result<(), String> {
    match message {
        WorkerMessage::Ready {
            protocol_version,
            host_api_version,
            plugin_id,
        } => {
            validate_versions(*protocol_version, host_api_version)?;
            validate_plugin_id(plugin_id)?;
        }
        WorkerMessage::ConfigurationReady { revision } => {
            token(revision, "configuration revision")?
        }
        WorkerMessage::Contributions {
            items,
            presentation,
        } => {
            validate_contributions(items)?;
            super::presentation::validate(presentation, items)?;
        }
        WorkerMessage::HttpVideo {
            id,
            url,
            title,
            cooldown_id,
            ..
        } => {
            token(id, "HTTP video id")?;
            if let Some(id) = cooldown_id {
                token(id, "HTTP media cooldown id")?;
            }
            label(title, "HTTP video title", 128)?;
            validated_media_url(url, true)?;
        }
        WorkerMessage::HttpLive { id, url, title } => {
            token(id, "live preview id")?;
            label(title, "live preview title", 128)?;
            validated_media_url(url, true)?;
        }
        WorkerMessage::MqttLive {
            id,
            title,
            camera_id,
            url,
        } => {
            token(id, "MQTT live preview id")?;
            label(title, "MQTT live preview title", 128)?;
            if !mqtt_live::camera_id(camera_id) {
                return Err("Invalid MQTT live camera identity".into());
            }
            validated_media_url(url, true)?;
        }
        WorkerMessage::LiveView {
            id,
            title,
            live_view_id,
        } => {
            token(id, "live preview id")?;
            label(title, "live preview title", 128)?;
            validate_live_id(live_view_id)?;
        }
        WorkerMessage::Notification {
            id,
            title,
            body,
            live_view_id,
        } => {
            token(id, "notification id")?;
            label(title, "notification title", MAX_NOTIFICATION_TITLE_BYTES)?;
            label(body, "notification body", MAX_NOTIFICATION_BODY_BYTES)?;
            if let Some(id) = live_view_id {
                validate_live_id(id)?;
            }
        }
        WorkerMessage::ActionResult { request_id, value } => {
            token(request_id, "request_id")?;
            bounded_json(value, MAX_ACTION_RESULT_BYTES)?;
        }
        WorkerMessage::ActionError {
            request_id,
            code,
            message,
        } => {
            token(request_id, "request_id")?;
            token(code, "error code")?;
            text(message, "error message", 1024)?;
        }
        WorkerMessage::Event { name, data } => {
            token(name, "worker event name")?;
            bounded_json(data, MAX_ACTION_RESULT_BYTES)?;
        }
    }
    bounded_serialized_frame(message)?;
    Ok(())
}

pub fn validate_handshake(expected_plugin_id: &str, message: &WorkerMessage) -> Result<(), String> {
    validate_plugin_id(expected_plugin_id)?;
    validate_worker_message(message)?;
    match message {
        WorkerMessage::Ready { plugin_id, .. } if plugin_id == expected_plugin_id => Ok(()),
        WorkerMessage::Ready { .. } => Err("worker plugin identity mismatch".into()),
        _ => Err("worker must acknowledge the handshake before sending data".into()),
    }
}

fn frame_payload(bytes: &[u8]) -> Result<&[u8], String> {
    if bytes.is_empty() || bytes.len() > MAX_FRAME_BYTES {
        return Err("empty or oversized worker frame".into());
    }
    let payload = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let payload = payload.strip_suffix(b"\r").unwrap_or(payload);
    // Input from a line reader can omit the delimiter; it must still leave room
    // for the newline on the wire, and cannot smuggle additional frames.
    if payload.is_empty()
        || payload.len() >= MAX_FRAME_BYTES
        || payload.iter().any(|byte| matches!(byte, b'\r' | b'\n'))
    {
        return Err("invalid newline-delimited worker frame".into());
    }
    Ok(payload)
}

pub fn parse_worker_frame(bytes: &[u8]) -> Result<WorkerMessage, String> {
    let message = serde_json::from_slice(frame_payload(bytes)?)
        .map_err(|error| format!("invalid worker message: {error}"))?;
    validate_worker_message(&message)?;
    Ok(message)
}

pub fn encode_host_frame(message: &HostMessage) -> Result<Vec<u8>, String> {
    validate_host_message(message)?;
    bounded_serialized_frame(message)
}

pub fn parse_manifest(bytes: &[u8]) -> Result<PluginManifest, String> {
    if bytes.is_empty() || bytes.len() > MAX_MANIFEST_BYTES {
        return Err("empty or oversized plugin manifest".into());
    }
    let manifest: PluginManifest = serde_json::from_slice(bytes)
        .map_err(|error| format!("invalid plugin manifest: {error}"))?;
    manifest.validate()?;
    Ok(manifest)
}

pub fn validate_package_path(path: &str) -> Result<(), String> {
    if path.is_empty() || path.len() > 240 || path.contains('\\') || path.contains(':') {
        return Err("entrypoint and inventory paths must be portable relative paths".into());
    }
    for component in path.split('/') {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.starts_with('.')
            || component.ends_with('.')
            || component.ends_with(' ')
            || component
                .bytes()
                .any(|byte| !byte.is_ascii_alphanumeric() && !b"-_.".contains(&byte))
        {
            return Err("invalid entrypoint or inventory path component".into());
        }
        let stem = component.split('.').next().unwrap_or("");
        let uppercase = stem.to_ascii_uppercase();
        if matches!(uppercase.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ((uppercase.starts_with("COM") || uppercase.starts_with("LPT"))
                && uppercase.len() == 4
                && matches!(uppercase.as_bytes()[3], b'1'..=b'9'))
        {
            return Err("reserved platform filename in plugin inventory".into());
        }
    }
    Ok(())
}

fn lowercase_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn reject_schema_references(value: &Value) -> Result<(), String> {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if matches!(key.as_str(), "$ref" | "$dynamicRef" | "$recursiveRef") {
                    return Err("configuration schema references are not supported".into());
                }
                reject_schema_references(value)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                reject_schema_references(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

impl PluginManifest {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err("unsupported plugin manifest schema".into());
        }
        validate_plugin_id(&self.plugin_id)?;
        label(&self.version, "plugin version", 128)?;
        Version::parse(&self.version).map_err(|_| "invalid plugin semantic version".to_string())?;
        label(&self.host_api, "host API requirement", 128)?;
        let requirement = VersionReq::parse(&self.host_api)
            .map_err(|_| "invalid host API semantic version requirement".to_string())?;
        let current = Version::parse(HOST_API_VERSION).map_err(|error| error.to_string())?;
        if !requirement.matches(&current) {
            return Err("plugin does not support this host API version".into());
        }
        if !DESKTOP_TARGETS.contains(&self.target.as_str()) {
            return Err("plugin target must be a supported desktop target".into());
        }
        validate_package_path(&self.entrypoint)?;
        // Editor schema nesting includes its metadata envelope. Runtime action
        // and event JSON retains the independent eight-level bound.
        bounded_json_depth(&self.config_schema, MAX_ACTION_RESULT_BYTES, 16)?;
        if !self.config_schema.is_object()
            || self.config_schema.get("type").and_then(Value::as_str) != Some("object")
        {
            return Err("config_schema must describe a JSON object".into());
        }
        reject_schema_references(&self.config_schema)?;
        let permissions: HashSet<_> = self.permissions.iter().collect();
        if permissions.len() != self.permissions.len() {
            return Err("duplicate plugin permission".into());
        }
        self.validate_http_video()?;
        self.validate_inventory()?;
        if let Some(signature) = &self.signature {
            if signature.algorithm != "ed25519" || !lowercase_hex(&signature.signature, 128) {
                return Err("invalid signature metadata encoding".into());
            }
            token(&signature.key_id, "signature key_id")?;
        }
        if serde_json::to_vec(self)
            .map_err(|error| error.to_string())?
            .len()
            > MAX_MANIFEST_BYTES
        {
            return Err("plugin manifest exceeds byte limit".into());
        }
        Ok(())
    }

    fn validate_http_video(&self) -> Result<(), String> {
        let live_permitted = self.permissions.contains(&PluginPermission::LiveView);
        if live_permitted != self.live_view.is_some() {
            return Err("Live view permission requires its scoped declaration".into());
        }
        if let Some(declaration) = &self.live_view {
            self.validate_secret_setting(&declaration.urls_setting)?;
            if let Some(key) = &declaration.mqtt_urls_setting {
                self.validate_public_string_setting(key)?;
                if declaration
                    .preview_duration_seconds
                    .is_none_or(|seconds| !(1..=15).contains(&seconds))
                {
                    return Err("Invalid MQTT live preview duration".into());
                }
            }
            if declaration
                .preview_duration_seconds
                .is_some_and(|seconds| !(1..=30).contains(&seconds))
            {
                return Err("Invalid mapped live preview duration".into());
            }
        }
        if let Some(group) = &self.group {
            token(&group.id, "plugin group")?;
            label(&group.title, "plugin group title", 64)?;
            if !matches!(group.icon.as_str(), "camera" | "home" | "plug") {
                return Err("Invalid plugin group icon".into());
            }
        }
        let permitted = self.permissions.contains(&PluginPermission::HttpVideo);
        if permitted != self.http_video.is_some() {
            return Err("HTTP video permission requires its scoped declaration".into());
        }
        if let Some(declaration) = &self.http_video {
            if let Some(policy) = &declaration.live_preview {
                let parsed =
                    reqwest::Url::parse(&format!("https://preview.invalid/?{}", policy.query))
                        .map_err(|_| "Invalid live preview query")?;
                if policy.query.is_empty()
                    || policy.query.len() > 128
                    || parsed.query() != Some(policy.query.as_str())
                    || parsed.fragment().is_some()
                    || !(1..=30).contains(&policy.max_duration_seconds)
                    || declaration.bearer_token_setting.is_some()
                {
                    return Err("Invalid live preview policy".into());
                }
            }
            if declaration
                .cooldown_seconds
                .is_some_and(|value| !(20..=300).contains(&value))
            {
                return Err("Invalid media cooldown".into());
            }
            if let Some(key) = &declaration.bearer_token_setting {
                self.validate_secret_setting(key)?;
            }
            token(&declaration.base_url_setting, "HTTP video setting")?;
            let field = self
                .config_schema
                .get("properties")
                .and_then(|fields| fields.get(&declaration.base_url_setting));
            if !self
                .permissions
                .contains(&PluginPermission::PluginConfiguration)
                || field
                    .and_then(|field| field.get("type"))
                    .and_then(Value::as_str)
                    != Some("string")
                || field
                    .and_then(|field| field.get("writeOnly"))
                    .is_some_and(|value| value.as_bool() != Some(false))
            {
                return Err(
                    "HTTP video base must name a nonsecret string configuration setting".into(),
                );
            }
        }
        Ok(())
    }

    fn validate_public_string_setting(&self, key: &str) -> Result<(), String> {
        token(key, "scoped public setting")?;
        let field = self
            .config_schema
            .get("properties")
            .and_then(|fields| fields.get(key));
        if !self
            .permissions
            .contains(&PluginPermission::PluginConfiguration)
            || field.and_then(|f| f.get("type")).and_then(Value::as_str) != Some("string")
            || field
                .and_then(|f| f.get("writeOnly"))
                .is_some_and(|v| v.as_bool() != Some(false))
        {
            return Err("Scoped endpoints must name a nonsecret string setting".into());
        }
        Ok(())
    }

    fn validate_secret_setting(&self, key: &str) -> Result<(), String> {
        token(key, "scoped secret setting")?;
        let field = self
            .config_schema
            .get("properties")
            .and_then(|fields| fields.get(key));
        if !self
            .permissions
            .contains(&PluginPermission::PluginConfiguration)
            || field
                .and_then(|field| field.get("type"))
                .and_then(Value::as_str)
                != Some("string")
            || field
                .and_then(|field| field.get("writeOnly"))
                .and_then(Value::as_bool)
                != Some(true)
        {
            return Err("Scoped credentials must name a secret string setting".into());
        }
        Ok(())
    }

    fn validate_inventory(&self) -> Result<(), String> {
        if self.inventory.is_empty() || self.inventory.len() > MAX_INVENTORY_FILES {
            return Err("invalid plugin inventory file count".into());
        }
        let mut paths = HashSet::new();
        let mut total_size = 0_u64;
        let mut has_entrypoint = false;
        for item in &self.inventory {
            validate_package_path(&item.path)?;
            // Case-insensitive collisions cannot be portable across our targets.
            if !paths.insert(item.path.to_ascii_lowercase()) {
                return Err("duplicate plugin inventory path".into());
            }
            if !lowercase_hex(&item.sha256, 64) {
                return Err("invalid inventory sha256 encoding".into());
            }
            total_size = total_size
                .checked_add(item.size)
                .ok_or("plugin inventory size overflow")?;
            if total_size > MAX_INVENTORY_BYTES {
                return Err("plugin inventory exceeds size limit".into());
            }
            has_entrypoint |= item.path == self.entrypoint && item.size > 0;
        }
        if !has_entrypoint {
            return Err("entrypoint must name a nonempty file in the inventory".into());
        }
        Ok(())
    }

    pub fn validate_for_target(&self, target: &str) -> Result<(), String> {
        self.validate()?;
        if self.target != target {
            return Err("plugin package target does not match this host".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ready() -> WorkerMessage {
        WorkerMessage::Ready {
            protocol_version: PROTOCOL_VERSION,
            host_api_version: HOST_API_VERSION.into(),
            plugin_id: "org.example.demo".into(),
        }
    }

    fn video_manifest() -> PluginManifest {
        let mut value = manifest();
        value.permissions.extend([
            PluginPermission::PluginConfiguration,
            PluginPermission::HttpVideo,
        ]);
        value.http_video = Some(HttpVideoDeclaration {
            live_preview: None,
            allow_query: false,
            bearer_token_setting: None,
            cooldown_seconds: None,
            base_url_setting: "base".into(),
        });
        value.config_schema =
            serde_json::json!({"type":"object","properties":{"base":{"type":"string"}}});
        value
    }

    #[test]
    fn http_video_grant_is_scoped_to_the_verified_nonsecret_startup_setting() {
        let mut manifest = video_manifest();
        manifest.validate().unwrap();
        let mut configuration = WorkerConfiguration {
            revision: "test".into(),
            values: serde_json::json!({"base":"https://camera.test:9443/frigate/"}),
            secrets: BTreeMap::new(),
        };
        let grant = HttpVideoGrant::from_manifest_configuration(&manifest, Some(&configuration))
            .unwrap()
            .unwrap();
        assert!(grant
            .validate_url("https://camera.test:9443/frigate/api/events/id/clip.mp4")
            .is_ok());
        for url in [
            "http://camera.test:9443/frigate/clip.mp4",
            "https://camera.test/frigate/clip.mp4",
            "https://other.test:9443/frigate/clip.mp4",
            "https://camera.test:9443/frigate-other/clip.mp4",
            "https://camera.test:9443/frigate/../outside",
            "https://camera.test:9443/frigate/%2e%2e/outside",
            "https://camera.test:9443/frigate/a%2fb",
            "https://camera.test:9443/frigate/%0a",
            "https://camera.test:9443/frigate/%C2%85",
            "https://camera.test:9443/frigate/%zz",
            "https://u:p@camera.test:9443/frigate/clip.mp4",
            "https://camera.test:9443/frigate/clip.mp4?token=x",
            "https://camera.test:9443/frigate/clip.mp4#fragment",
            "file:///tmp/clip.mp4",
        ] {
            assert!(grant.validate_url(url).is_err(), "{url}");
        }
        assert!(grant
            .validate_url("https://camera.test:9443/frigate/a%20b%C3%A9%3F%23/clip.mp4")
            .is_ok());
        for value in [
            serde_json::json!({}),
            serde_json::json!({"base":null}),
            serde_json::json!({"base":" "}),
        ] {
            configuration.values = value;
            assert!(
                HttpVideoGrant::from_manifest_configuration(&manifest, Some(&configuration))
                    .unwrap()
                    .is_none()
            );
        }
        manifest.config_schema["properties"]["base"]["writeOnly"] = serde_json::json!(true);
        assert!(manifest.validate().is_err());
        manifest.config_schema["properties"]["base"]["writeOnly"] = serde_json::json!(false);
        manifest
            .permissions
            .retain(|permission| *permission != PluginPermission::HttpVideo);
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn live_preview_requires_explicit_bounded_policy_and_exact_scoped_query() {
        let mut manifest = video_manifest();
        let configuration = WorkerConfiguration {
            revision: "test".into(),
            values: json!({"base":"https://camera.test:9443/frigate/"}),
            secrets: BTreeMap::new(),
        };
        let target = "https://camera.test:9443/frigate/api/front?fps=2&height=360";
        let grant = HttpVideoGrant::from_manifest_configuration(&manifest, Some(&configuration))
            .unwrap()
            .unwrap();
        assert!(grant.validate_preview_url(target).is_err());
        assert!(grant.preview_duration().is_none());
        manifest.http_video.as_mut().unwrap().live_preview = Some(LivePreviewDeclaration {
            query: "fps=2&height=360".into(),
            max_duration_seconds: 15,
        });
        manifest.validate().unwrap();
        let grant = HttpVideoGrant::from_manifest_configuration(&manifest, Some(&configuration))
            .unwrap()
            .unwrap();
        assert!(grant.validate_preview_url(target).is_ok());
        assert_eq!(
            grant.preview_duration(),
            Some(std::time::Duration::from_secs(15))
        );
        assert!(grant.validate_url(target).is_err());
        for url in [
            "https://other.test:9443/frigate/api/front?fps=2&height=360",
            "https://camera.test:9443/frigate-other/api/front?fps=2&height=360",
            "https://camera.test:9443/frigate/api/front?height=360&fps=2",
            "https://camera.test:9443/frigate/api/front?fps=2&height=360&token=private",
            "https://camera.test:9443/frigate/api/front?fps=2&height=360#fragment",
            "https://camera.test:9443/frigate/api/front",
            "https://u:p@camera.test:9443/frigate/api/front?fps=2&height=360",
            "https://camera.test:9443/frigate/%2e%2e/api/front?fps=2&height=360",
        ] {
            assert!(grant.validate_preview_url(url).is_err(), "{url}");
        }
        for (query, duration) in [
            ("", 15),
            ("fps=2#fragment", 15),
            ("fps=2\n", 15),
            ("fps=2", 0),
            ("fps=2", 31),
        ] {
            let mut invalid = manifest.clone();
            invalid.http_video.as_mut().unwrap().live_preview = Some(LivePreviewDeclaration {
                query: query.into(),
                max_duration_seconds: duration,
            });
            assert!(invalid.validate().is_err());
        }
        manifest.http_video.as_mut().unwrap().bearer_token_setting = Some("token".into());
        assert!(manifest.validate().is_err());
        let wire = WorkerMessage::HttpLive {
            id: "motion-1".into(),
            url: target.into(),
            title: "Front".into(),
        };
        validate_worker_message(&wire).unwrap();
        assert!(!format!("{wire:?}").contains("camera.test"));
    }

    #[test]
    fn http_video_wire_fields_are_bounded_plain_data_and_debug_is_redacted() {
        let valid = WorkerMessage::HttpVideo {
            cooldown_id: None,
            id: "clip-1".into(),
            url: "https://private-camera.test/base/clip.mp4".into(),
            title: "Private camera".into(),
            media_kind: HttpMediaKind::Video,
        };
        validate_worker_message(&valid).unwrap();
        assert!(!format!("{valid:?}").contains("private-camera"));
        for (id, url, title) in [
            (
                "x".repeat(129),
                "https://camera.test/x".into(),
                "Camera".into(),
            ),
            (
                "x".into(),
                format!("https://camera.test/{}", "x".repeat(2048)),
                "Camera".into(),
            ),
            ("x".into(), "https://camera.test/x".into(), "x".repeat(129)),
            (
                "x".into(),
                "https://camera.test/x".into(),
                "Camera\n".into(),
            ),
        ] {
            assert!(validate_worker_message(&WorkerMessage::HttpVideo {
                cooldown_id: None,
                id,
                url,
                title,
                media_kind: HttpMediaKind::Video,
            })
            .is_err());
        }
        assert!(parse_worker_frame(br#"{"type":"http_video","id":"x","url":"https://camera.test/x","title":"Camera","headers":{}}"#).is_err());
    }

    #[test]
    fn typed_snapshots_preserve_legacy_video_frames_and_reject_unrecognized_kinds() {
        let legacy = json!({"type":"http_video","id":"media-1","url":"https://camera.test/snapshot","title":"Camera"});
        let parsed = parse_worker_frame(&serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert!(matches!(
            parsed,
            WorkerMessage::HttpVideo {
                media_kind: HttpMediaKind::Video,
                ..
            }
        ));
        assert_eq!(serde_json::to_value(parsed).unwrap(), legacy);
        for kind in ["jpeg", "png", "webp"] {
            let mut snapshot = legacy.clone();
            snapshot["media_kind"] = json!(kind);
            let message = parse_worker_frame(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
            assert_eq!(serde_json::to_value(message).unwrap(), snapshot);
        }
        for kind in [
            json!("svg"),
            json!("html"),
            json!("image"),
            json!(null),
            json!(1),
        ] {
            let mut invalid = legacy.clone();
            invalid["media_kind"] = kind;
            assert!(parse_worker_frame(&serde_json::to_vec(&invalid).unwrap()).is_err());
        }
    }

    fn manifest() -> PluginManifest {
        PluginManifest {
            group: None,
            live_view: None,
            schema_version: MANIFEST_SCHEMA_VERSION,
            plugin_id: "org.example.demo".into(),
            version: "0.1.0".into(),
            host_api: ">=1.0.0, <2.0.0".into(),
            target: "aarch64-apple-darwin".into(),
            entrypoint: "bin/demo-worker".into(),
            config_schema: json!({"type":"object","properties":{}}),
            permissions: vec![PluginPermission::DashboardContributions],
            http_video: None,
            inventory: vec![InventoryEntry {
                path: "bin/demo-worker".into(),
                size: 100,
                sha256: "a".repeat(64),
            }],
            signature: None,
        }
    }

    fn text_card(id: &str) -> DashboardContribution {
        DashboardContribution::Text {
            id: id.into(),
            title: "Plain text".into(),
            text: "<script>is text, never HTML</script>".into(),
        }
    }

    #[test]
    fn handshake_round_trip_requires_expected_identity_and_negotiated_versions() {
        let bytes = bounded_serialized_frame(&ready()).unwrap();
        let decoded = parse_worker_frame(&bytes).unwrap();
        assert_eq!(decoded, ready());
        assert!(validate_handshake("org.example.demo", &decoded).is_ok());
        assert!(validate_handshake("org.example.other", &decoded).is_err());
        let mut wrong_protocol = ready();
        if let WorkerMessage::Ready {
            protocol_version, ..
        } = &mut wrong_protocol
        {
            *protocol_version = 2;
        }
        assert!(validate_worker_message(&wrong_protocol).is_err());
        let mut wrong_api = ready();
        if let WorkerMessage::Ready {
            host_api_version, ..
        } = &mut wrong_api
        {
            *host_api_version = "2.0.0".into();
        }
        assert!(validate_worker_message(&wrong_api).is_err());
        assert!(validate_handshake(
            "org.example.demo",
            &WorkerMessage::Contributions {
                items: Vec::new(),
                presentation: Vec::new()
            }
        )
        .is_err());
    }

    #[test]
    fn rejects_unknown_messages_fields_and_multiple_lines() {
        for bytes in [
            br#"{"type":"mqtt_publish","topic":"write"}"#.as_slice(),
            br#"{"type":"ready","protocol_version":1,"host_api_version":"1.0.0","plugin_id":"org.example.demo","extra":true}"#,
            br#"{"type":"action_error","request_id":"1","code":"bad","message":"x","retry":true}"#,
            b"{}\n{}\n",
            b"{\n\"type\":\"shutdown\"\n}",
            b"",
            &[0xff],
        ] {
            assert!(parse_worker_frame(bytes).is_err());
        }
        assert!(parse_worker_frame(&vec![b' '; MAX_FRAME_BYTES + 1]).is_err());
    }

    #[test]
    fn actions_are_bounded_objects_with_a_correlated_deadline() {
        let mut action = HostMessage::Action {
            request_id: "request-1".into(),
            action_id: "refresh".into(),
            params: json!({"force":true}),
            deadline_ms: 5000,
        };
        let bytes = encode_host_frame(&action).unwrap();
        assert!(bytes.ends_with(b"\n"));
        assert_eq!(
            serde_json::from_slice::<HostMessage>(&bytes).unwrap(),
            action
        );
        if let HostMessage::Action { deadline_ms, .. } = &mut action {
            *deadline_ms = 0;
        }
        assert!(encode_host_frame(&action).is_err());
        assert!(validate_action_params(&json!([1, 2])).is_err());
        assert!(
            validate_action_params(&json!({"data":"x".repeat(MAX_ACTION_PARAMS_BYTES)})).is_err()
        );
        let mut nested = json!(true);
        for _ in 0..=MAX_JSON_DEPTH {
            nested = json!({"child":nested});
        }
        assert!(validate_action_params(&nested).is_err());
        assert!(validate_worker_message(&WorkerMessage::ActionResult {
            request_id: "../request".into(),
            value: Value::Null,
        })
        .is_err());
        assert!(validate_worker_message(&WorkerMessage::ActionResult {
            request_id: "request-1".into(),
            value: json!("x".repeat(MAX_ACTION_RESULT_BYTES)),
        })
        .is_err());
    }

    #[test]
    fn contributions_are_bounded_unique_and_strictly_declarative() {
        assert!(validate_contributions(&[text_card("first")]).is_ok());
        assert!(validate_contributions(&[text_card("first"), text_card("first")]).is_err());
        let items: Vec<_> = (0..=MAX_CONTRIBUTIONS)
            .map(|index| text_card(&format!("item-{index}")))
            .collect();
        assert!(validate_contributions(&items).is_err());
        assert!(serde_json::from_value::<DashboardContribution>(json!({
            "kind":"text","id":"card","title":"Title","text":"Hello","html":"<b>html</b>"
        }))
        .is_err());
        assert!(DashboardContribution::Metric {
            id: "metric".into(),
            title: "Title".into(),
            value: f64::NAN,
            unit: None,
        }
        .validate()
        .is_err());
        assert!(DashboardContribution::Action {
            id: "action".into(),
            title: "Title".into(),
            state_id: None,
            action_id: "do-work".into(),
            label: "Run".into(),
            params: json!(["unsupported"]),
        }
        .validate()
        .is_err());
        let items = (0..20)
            .map(|index| DashboardContribution::Text {
                id: format!("item-{index}"),
                title: "Title".into(),
                text: "x".repeat(4096),
            })
            .collect();
        assert!(validate_worker_message(&WorkerMessage::Contributions {
            items,
            presentation: Vec::new()
        })
        .is_err());
    }

    fn numeric_card() -> DashboardContribution {
        serde_json::from_value(json!({
            "kind":"number_input","id":"temperature-input","title":"Temperature",
            "action_id":"set-temperature","label":"Set temperature","unit":"°C",
            "input_revision":"input-1","value_scaled":-15,"min_scaled":-25,
            "max_scaled":25,"step_scaled":5,"decimal_places":1
        }))
        .unwrap()
    }

    fn static_card() -> DashboardContribution {
        serde_json::from_value(json!({
            "kind":"action","id":"turn-on","title":"Switch",
            "action_id":"switch-on","label":"Turn on","params":{}
        }))
        .unwrap()
    }

    fn with_state_id(item: DashboardContribution, state_id: Value) -> DashboardContribution {
        let mut value = serde_json::to_value(item).unwrap();
        value["state_id"] = state_id;
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn entity_card_references_round_trip_without_changing_legacy_wire_shape() {
        let anchors = [
            text_card("state"),
            serde_json::from_value(json!({
                "kind":"metric","id":"state","title":"Temperature","value":21.5
            }))
            .unwrap(),
            serde_json::from_value(json!({
                "kind":"status","id":"state","title":"Switch","value":"On","tone":"success"
            }))
            .unwrap(),
        ];
        for control in [static_card(), numeric_card()] {
            assert!(serde_json::to_value(&control)
                .unwrap()
                .get("state_id")
                .is_none());
            let absent = with_state_id(control.clone(), Value::Null);
            assert_eq!(absent, control);
            assert!(serde_json::to_value(absent)
                .unwrap()
                .get("state_id")
                .is_none());
            let grouped = with_state_id(control, json!("state"));
            for anchor in &anchors {
                // References can precede their anchor on the wire.
                let frame = WorkerMessage::Contributions {
                    presentation: Vec::new(),
                    items: vec![grouped.clone(), anchor.clone()],
                };
                let bytes = serde_json::to_vec(&frame).unwrap();
                assert_eq!(parse_worker_frame(&bytes).unwrap(), frame);
            }
        }
    }

    #[test]
    fn entity_cards_reject_dangling_recursive_and_malformed_references() {
        for control in [static_card(), numeric_card()] {
            for state_id in [
                "".to_owned(),
                "../state".to_owned(),
                "state name".to_owned(),
                "x".repeat(129),
                "missing".to_owned(),
                control.id().to_owned(),
            ] {
                let grouped = with_state_id(control.clone(), json!(state_id));
                assert!(validate_contributions(&[text_card("state"), grouped]).is_err());
            }
            for target in [static_card(), numeric_card()] {
                if target.id() == control.id() {
                    continue;
                }
                let grouped = with_state_id(control.clone(), json!(target.id()));
                assert!(validate_contributions(&[target, grouped]).is_err());
            }
            for invalid in [json!(true), json!(1), json!([]), json!({})] {
                let mut value = serde_json::to_value(&control).unwrap();
                value["state_id"] = invalid;
                assert!(serde_json::from_value::<DashboardContribution>(value).is_err());
            }
        }
        // Read-only items cannot themselves acquire parent links or controls.
        let mut readonly = serde_json::to_value(text_card("state")).unwrap();
        readonly["state_id"] = json!("other");
        assert!(serde_json::from_value::<DashboardContribution>(readonly).is_err());
        // An anchor from a previous snapshot cannot satisfy a new reference.
        validate_contributions(&[text_card("state")]).unwrap();
        let grouped = with_state_id(static_card(), json!("state"));
        assert!(validate_contributions(&[grouped]).is_err());
    }

    #[test]
    fn entity_card_members_keep_snapshot_count_and_frame_bounds() {
        let anchor = "s".repeat(128);
        let mut items = vec![text_card(&anchor)];
        for index in 1..MAX_CONTRIBUTIONS {
            let mut value = serde_json::to_value(static_card()).unwrap();
            value["id"] = json!(format!("action-{index}"));
            value["state_id"] = json!(anchor);
            items.push(serde_json::from_value(value).unwrap());
        }
        validate_worker_message(&WorkerMessage::Contributions {
            presentation: Vec::new(),
            items: items.clone(),
        })
        .unwrap();
        items.push(text_card("another-state"));
        assert!(validate_contributions(&items).is_err());
        items.pop();
        for item in &mut items {
            if let DashboardContribution::Action { params, .. } = item {
                *params = json!({"value":"x".repeat(2048)});
            }
        }
        validate_contributions(&items).unwrap();
        assert!(validate_worker_message(&WorkerMessage::Contributions {
            items,
            presentation: Vec::new()
        })
        .is_err());
    }

    #[test]
    fn numeric_inputs_validate_integer_bounds_grid_metadata_and_wire_shape() {
        let card = numeric_card();
        card.validate().unwrap();
        let message = WorkerMessage::Contributions {
            presentation: Vec::new(),
            items: vec![card.clone()],
        };
        let encoded = serde_json::to_vec(&message).unwrap();
        assert_eq!(parse_worker_frame(&encoded).unwrap(), message);
        for (field, invalid) in [
            ("value_scaled", json!(-26)),
            ("value_scaled", json!(-24)),
            ("value_scaled", json!(30)),
            ("value_scaled", json!(1.0)),
            ("value_scaled", json!("5")),
            ("value_scaled", json!(true)),
            ("value_scaled", json!(i64::MIN)),
            ("min_scaled", json!(i64::MIN)),
            ("max_scaled", json!(i64::MAX)),
            ("min_scaled", json!(30)),
            ("step_scaled", json!(0)),
            ("step_scaled", json!(-5)),
            ("step_scaled", json!(MAX_SCALED_COEFFICIENT + 1)),
            ("decimal_places", json!(7)),
            ("decimal_places", json!(-1)),
            ("input_revision", json!("../input")),
            ("input_revision", json!("")),
            ("unit", json!("x".repeat(33))),
            ("label", json!("x".repeat(129))),
            ("title", json!("x".repeat(129))),
            ("params", json!({})),
            ("url", json!("https://example.test")),
        ] {
            let mut value = serde_json::to_value(&card).unwrap();
            value[field] = invalid;
            assert!(
                !serde_json::from_value::<DashboardContribution>(value)
                    .is_ok_and(|item| item.validate().is_ok()),
                "accepted {field}"
            );
        }
        for places in 0..=6 {
            let mut value = serde_json::to_value(&card).unwrap();
            value["decimal_places"] = json!(places);
            value["min_scaled"] = json!(-MAX_SCALED_COEFFICIENT);
            value["max_scaled"] = json!(MAX_SCALED_COEFFICIENT);
            value["step_scaled"] = json!(MAX_SCALED_COEFFICIENT);
            value["value_scaled"] = json!(0);
            serde_json::from_value::<DashboardContribution>(value)
                .unwrap()
                .validate()
                .unwrap();
        }
    }

    #[test]
    fn numeric_requests_allow_only_matching_revision_and_integer_grid_values() {
        let grant = numeric_card().number_input_grant().unwrap();
        for value in [-25, -20, -15, 0, 25] {
            assert!(grant.accepts(&json!({"input_revision":"input-1","value_scaled":value})));
        }
        for params in [
            json!({}),
            json!({"input_revision":"input-1"}),
            json!({"input_revision":"stale","value_scaled":0}),
            json!({"input_revision":"input-1","value_scaled":0,"entity_id":"number.other"}),
            json!({"input_revision":"input-1","value_scaled":0,"service":"set_value"}),
            json!({"input_revision":"input-1","value_scaled":0.0}),
            json!({"input_revision":"input-1","value_scaled":"0"}),
            json!({"input_revision":"input-1","value_scaled":true}),
            json!({"input_revision":"input-1","value_scaled":-24}),
            json!({"input_revision":"input-1","value_scaled":30}),
            json!({"input_revision":"input-1","value_scaled":i64::MIN}),
            json!([]),
            Value::Null,
        ] {
            assert!(!grant.accepts(&params));
        }
    }

    #[test]
    fn numeric_action_ids_cannot_alias_any_action_but_static_aliases_remain_valid() {
        let numeric = numeric_card();
        let mut second = numeric.clone();
        if let DashboardContribution::NumberInput { id, .. } = &mut second {
            *id = "other-input".into();
        }
        let static_action = DashboardContribution::Action {
            id: "preset".into(),
            title: "Preset".into(),
            state_id: None,
            action_id: "set-temperature".into(),
            label: "Set".into(),
            params: json!({"input_revision":"input-1","value_scaled":0}),
        };
        for items in [
            vec![numeric.clone(), second],
            vec![numeric.clone(), static_action.clone()],
            vec![static_action.clone(), numeric],
        ] {
            assert!(validate_contributions(&items).is_err());
        }
        let mut alias = static_action.clone();
        if let DashboardContribution::Action { id, params, .. } = &mut alias {
            *id = "another-preset".into();
            *params = json!({"value":42});
        }
        validate_contributions(&[static_action, alias]).unwrap();
    }

    #[test]
    fn host_api_19_accepts_earlier_worker_ranges_and_requires_negotiated_acknowledgement() {
        assert_eq!(HOST_API_VERSION, "1.9.0");
        for requirement in [
            "^1.0", "^1.3", "^1.4", "^1.5", "^1.6", "^1.7", "^1.8", "^1.9",
        ] {
            let mut manifest = manifest();
            manifest.host_api = requirement.into();
            manifest.validate().unwrap();
        }
        assert!(validate_versions(1, "1.9.0").is_ok());
        assert!(validate_versions(1, "1.8.0").is_err());
        assert!(validate_versions(2, "1.8.0").is_err());
    }

    #[test]
    fn packaged_editor_schemas_fit_manifest_bounds_without_relaxing_runtime_json() {
        for template in [
            include_str!("../../../scripts/plugins/home-assistant-manifest.json"),
            include_str!("../../../scripts/plugins/frigate-manifest.json"),
            include_str!("../../../scripts/plugins/kerberos-manifest.json"),
            include_str!("../../../scripts/plugins/ring-manifest.json"),
        ] {
            let mut value: Value = serde_json::from_str(template).unwrap();
            value["target"] = json!("aarch64-apple-darwin");
            value["inventory"] =
                json!([{"path":value["entrypoint"],"sha256":"0".repeat(64),"size":1}]);
            let manifest: PluginManifest = serde_json::from_value(value).unwrap();
            manifest.validate().unwrap();
        }
        let mut nested = json!(true);
        for _ in 0..10 {
            nested = json!({"child":nested});
        }
        assert!(bounded_json(&nested, MAX_ACTION_RESULT_BYTES).is_err());
        assert!(bounded_json_depth(&nested, MAX_ACTION_RESULT_BYTES, 16).is_ok());
        for _ in 0..7 {
            nested = json!({"child":nested});
        }
        assert!(bounded_json_depth(&nested, MAX_ACTION_RESULT_BYTES, 16).is_err());
    }

    #[test]
    fn manifest_accepts_compatible_desktop_metadata_without_claiming_authenticity() {
        let mut manifest = manifest();
        manifest.signature = Some(SignatureMetadata {
            algorithm: "ed25519".into(),
            key_id: "example-development".into(),
            signature: "0".repeat(128),
        });
        // A syntactically valid all-zero signature is deliberately not a trust check.
        assert!(manifest.validate().is_ok());
        let encoded = serde_json::to_vec(&manifest).unwrap();
        assert_eq!(parse_manifest(&encoded).unwrap(), manifest);
        assert!(manifest.validate_for_target("aarch64-apple-darwin").is_ok());
        assert!(manifest.validate_for_target("x86_64-apple-darwin").is_err());
        assert!(parse_manifest(&vec![b' '; MAX_MANIFEST_BYTES + 1]).is_err());
    }

    #[test]
    fn manifest_rejects_incompatible_api_mobile_targets_and_unknown_permissions() {
        let mut value = serde_json::to_value(manifest()).unwrap();
        for (field, invalid) in [
            ("schema_version", json!(2)),
            ("plugin_id", json!("../escape")),
            ("version", json!("latest")),
            ("host_api", json!(">=2.0.0")),
            ("target", json!("aarch64-linux-android")),
            ("config_schema", json!({"type":"array"})),
            ("permissions", json!(["mqtt_publish_core"])),
        ] {
            let old = value[field].clone();
            value[field] = invalid;
            assert!(
                parse_manifest(&serde_json::to_vec(&value).unwrap()).is_err(),
                "{field}"
            );
            value[field] = old;
        }
        value["unexpected"] = json!(true);
        assert!(parse_manifest(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn manifest_inventory_paths_cannot_escape_or_collide_across_desktop_targets() {
        for path in [
            "../worker",
            "/worker",
            "C:/worker",
            "bin\\worker",
            "bin//worker",
            "bin/./worker",
            "bin/CON.exe",
            "bin/LPT1",
            "bin/worker.",
            "bin/.hidden",
            "bin/worker ",
        ] {
            let mut manifest = manifest();
            manifest.entrypoint = path.into();
            manifest.inventory[0].path = path.into();
            assert!(manifest.validate().is_err(), "{path}");
        }
        let mut manifest = manifest();
        let mut collision = manifest.inventory[0].clone();
        collision.path = "bin/DEMO-worker".into();
        manifest.inventory.push(collision);
        assert!(manifest.validate().is_err());
        manifest.inventory.pop();
        manifest.inventory[0].sha256 = "not-a-digest".into();
        assert!(manifest.validate().is_err());
        manifest.inventory[0].sha256 = "a".repeat(64);
        manifest.inventory[0].size = u64::MAX;
        assert!(manifest.validate().is_err());
        manifest.inventory[0].size = 0;
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn configuration_schema_references_and_duplicate_permissions_are_rejected() {
        let mut manifest = manifest();
        manifest.config_schema = json!({"type":"object","properties":{"host":{"$ref":"https://example.invalid/schema"}}});
        assert!(manifest.validate().is_err());
        manifest.config_schema = json!({"type":"object"});
        manifest
            .permissions
            .push(PluginPermission::DashboardContributions);
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn notifications_are_bounded_plain_text_without_additional_host_operations() {
        let valid = json!({"type":"notification","id":"motion-1:front","title":"Motion","body":"Person detected"});
        assert!(parse_worker_frame(&serde_json::to_vec(&valid).unwrap()).is_ok());
        for (field, value) in [
            ("id", "../outside".to_owned()),
            ("id", "x".repeat(129)),
            ("title", "x".repeat(MAX_NOTIFICATION_TITLE_BYTES + 1)),
            ("title", "é".repeat(65)),
            ("title", " ".to_owned()),
            ("body", "x".repeat(MAX_NOTIFICATION_BODY_BYTES + 1)),
            ("body", "line\nbreak".to_owned()),
            ("body", "\0".to_owned()),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value.into();
            assert!(parse_worker_frame(&serde_json::to_vec(&invalid).unwrap()).is_err());
        }
        for field in ["url", "html", "command", "icon", "actions"] {
            let mut invalid = valid.clone();
            invalid[field] = "untrusted host operation".into();
            assert!(parse_worker_frame(&serde_json::to_vec(&invalid).unwrap()).is_err());
        }
        assert_eq!(
            serde_json::to_string(&PluginPermission::DesktopNotifications).unwrap(),
            "\"desktop_notifications\""
        );
    }

    #[test]
    fn worker_configuration_is_bounded_strict_and_redacted() {
        let mut configuration = WorkerConfiguration {
            revision: "revision-1".into(),
            values: json!({"server":"https://plugin.example"}),
            secrets: [("token".into(), "sensitive-fixture-value".into())].into(),
        };
        let message = HostMessage::Configuration {
            configuration: configuration.clone(),
        };
        assert!(validate_host_message(&message).is_ok());
        let encoded = encode_host_frame(&message).unwrap();
        assert!(String::from_utf8(encoded)
            .unwrap()
            .contains("sensitive-fixture-value"));
        assert!(!format!("{message:?}").contains("sensitive-fixture-value"));
        assert!(!format!("{message:?}").contains("plugin.example"));
        assert!(
            parse_worker_frame(br#"{"type":"configuration_ready","revision":"revision-1"}"#)
                .is_ok()
        );
        assert!(parse_worker_frame(
            br#"{"type":"configuration_ready","revision":"revision-1","secret":"not-allowed"}"#
        )
        .is_err());
        configuration.values = json!({"token":"public-reclassification"});
        assert!(configuration.validate().is_err());
        configuration.values = json!({});
        configuration
            .secrets
            .insert("token".into(), "x".repeat(MAX_CONFIGURATION_BYTES));
        assert!(configuration.validate().is_err());
        configuration.secrets.clear();
        configuration.revision = "bad\nrevision".into();
        assert!(configuration.validate().is_err());
        configuration.revision = "valid".into();
        configuration.values = json!([]);
        assert!(configuration.validate().is_err());
    }
}
