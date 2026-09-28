use crate::config::Configuration;
use inverter_camera_common::{
    labels, title, valid_identity, Provider, MAX_CACHE_ENTRIES, MAX_PAYLOAD_BYTES,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    time::{Duration, Instant},
};

const SILENCE: Duration = Duration::from_secs(15);

pub struct Events {
    seen: HashMap<String, Instant>,
    labels: BTreeMap<String, String>,
    live_cameras: BTreeSet<String>,
    mqtt_urls: inverter_camera_common::live_urls::CameraLiveUrls,
    mqtt_seen: HashMap<String, Instant>,
    excluded_legacy_cameras: BTreeSet<String>,
}

struct Motion {
    camera: String,
    notification: Value,
}

impl Events {
    pub fn new(config: &Configuration) -> Result<Self, &'static str> {
        Ok(Self {
            seen: HashMap::new(),
            mqtt_seen: HashMap::new(),
            excluded_legacy_cameras: config.excluded_legacy_cameras()?,
            mqtt_urls: config.mqtt_live_urls()?,
            labels: labels(config.values.camera_labels.as_deref(), valid_identity)?,
            live_cameras: config.live_urls()?.into_keys().collect(),
        })
    }

    fn mqtt_live(
        &mut self,
        topic: &str,
        payload: &[u8],
        retained: bool,
        now: Instant,
    ) -> Option<Value> {
        if retained || payload.len() > 2048 {
            return None;
        }
        let camera = topic.strip_prefix(inverter_camera_common::live_urls::TOPIC_PREFIX)?;
        let value = std::str::from_utf8(payload).ok()?;
        let url = self.mqtt_urls.resolve(camera, value).ok()?;
        self.mqtt_seen
            .retain(|_, last| now.saturating_duration_since(*last) < SILENCE);
        if self.mqtt_seen.contains_key(camera) {
            return None;
        }
        self.mqtt_seen.insert(camera.into(), now);
        let name = self
            .labels
            .get(camera)
            .cloned()
            .unwrap_or_else(|| friendly_name(camera));
        Some(
            json!({"type":"mqtt_live", "id":format!("mqtt-{}", uuid::Uuid::new_v4()),
            "camera_id":camera, "title":title("HA", &name), "url":url.as_str()}),
        )
    }

    fn motion(
        &mut self,
        topic: &str,
        payload: &[u8],
        retained: bool,
        now: Instant,
        utc_now: u64,
    ) -> Option<Motion> {
        if retained || payload.len() > MAX_PAYLOAD_BYTES {
            return None;
        }
        let mut parts = topic.split('/');
        if parts.next()? != "kerberos" {
            return None;
        }
        let kind = parts.next()?;
        let identity = parts.next()?;
        if parts.next().is_some() || !valid_identity(identity) {
            return None;
        }
        let camera = match kind {
            "agent" if std::str::from_utf8(payload).ok()?.trim() == "motion" => identity.to_owned(),
            "hub" => {
                let message: Value = serde_json::from_slice(payload).ok()?;
                if ["hidden", "encrypted"].iter().any(|flag| {
                    message
                        .get(flag)
                        .is_some_and(|value| value.as_bool() != Some(false))
                }) {
                    return None;
                }
                let body = message.get("payload")?;
                if body.get("action")?.as_str()? != "motion" {
                    return None;
                }
                let camera = body.get("device_id")?.as_str()?;
                if !valid_identity(camera)
                    || message
                        .get("device_id")
                        .is_some_and(|v| v.as_str() != Some(camera))
                {
                    return None;
                }
                let timestamp = body.get("value")?.get("timestamp")?.as_u64()?;
                if timestamp == 0
                    || utc_now.saturating_sub(timestamp) > 60
                    || timestamp.saturating_sub(utc_now) > 10
                {
                    return None;
                }
                camera.to_owned()
            }
            _ => return None,
        };
        // Only legacy Agent/Hub motion is excluded. MQTT URL previews have
        // their own admission path and cooldown, even for the same camera ID.
        if self.excluded_legacy_cameras.contains(&camera) {
            return None;
        }
        self.seen
            .retain(|_, seen| now.saturating_duration_since(*seen) < SILENCE);
        if let Some(last) = self.seen.get_mut(&camera) {
            // Continuous movement extends the same episode, including suppressed repeats.
            *last = now;
            return None;
        }
        if self.seen.len() >= MAX_CACHE_ENTRIES {
            return None;
        }
        self.seen.insert(camera.clone(), now);
        let name = self
            .labels
            .get(&camera)
            .cloned()
            .unwrap_or_else(|| friendly_name(&camera));
        let notification = json!({"type":"notification","id":format!("kerberos-{}",uuid::Uuid::new_v4()),"title":title("Kerberos", &name),"body":"Motion started"});
        Some(Motion {
            camera,
            notification,
        })
    }
}

fn friendly_name(identity: &str) -> String {
    match identity.to_ascii_lowercase().as_str() {
        "front_camera" => "Front".into(),
        "garage_camera" => "Garage".into(),
        "tenants_camera" => "Tenants".into(),
        "porch_camera" => "Porch".into(),
        _ => identity
            .split(['_', '-'])
            .filter(|s| !s.is_empty())
            .map(|s| {
                let mut chars = s.chars();
                chars
                    .next()
                    .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

impl Provider for Events {
    fn frames(
        &mut self,
        topic: &str,
        payload: &[u8],
        retained: bool,
        now: Instant,
        unix_seconds: u64,
    ) -> Vec<Value> {
        if topic.starts_with(inverter_camera_common::live_urls::TOPIC_PREFIX) {
            return self
                .mqtt_live(topic, payload, retained, now)
                .into_iter()
                .collect();
        }
        let Some(motion) = self.motion(topic, payload, retained, now, unix_seconds) else {
            return Vec::new();
        };
        if self.live_cameras.contains(&motion.camera) {
            // The host owns the URL and opens the preview automatically. Do not
            // cover that window with a second announcement of the same motion.
            vec![
                json!({"type":"live_view","id":motion.notification["id"],"title":motion.notification["title"],"live_view_id":motion.camera}),
            ]
        } else {
            vec![motion.notification]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn events() -> Events {
        Events::new(
            &serde_json::from_value(
                json!({"revision":"fixture","values":{"mqtt_host":"localhost"},"secrets":{}}),
            )
            .unwrap(),
        )
        .unwrap()
    }
    fn hub(camera: &str, timestamp: u64) -> Vec<u8> {
        serde_json::to_vec(&json!({"device_id":camera,"hidden":false,"encrypted":false,"payload":{"action":"motion","device_id":camera,"value":{"timestamp":timestamp}}})).unwrap()
    }

    #[test]
    fn legacy_exclusion_uses_device_identity_and_never_filters_mqtt_urls() {
        let endpoint = "https://ha.invalid/api/camera_proxy_stream/camera.front";
        for with_legacy_preview in [false, true] {
            let config: Configuration = serde_json::from_value(json!({
                "revision":"fixture", "values":{"mqtt_host":"localhost",
                    "excluded_legacy_cameras":r#"["front"]"#,
                    "mqtt_live_endpoints":json!({"front":endpoint}).to_string()},
                "secrets":if with_legacy_preview {
                    json!({"camera_live_urls":r#"{"front":"https://legacy.invalid/live"}"#})
                } else { json!({}) }
            }))
            .unwrap();
            let mut events = Events::new(&config).unwrap();
            let now = Instant::now();
            assert!(events
                .frames("kerberos/agent/front", b"motion", false, now, 1000)
                .is_empty());
            // A shared Hub topic identifies the camera in its payload.
            assert!(events
                .frames("kerberos/hub/shared", &hub("front", 1000), false, now, 1000)
                .is_empty());
            assert!(events.seen.is_empty());
            let preview = events.frames(
                "homelab/cameras/live/front",
                format!("{endpoint}?token=fixture").as_bytes(),
                false,
                now,
                1000,
            );
            assert_eq!(preview.len(), 1);
            assert_eq!(preview[0]["type"], "mqtt_live");
            for camera in ["back", "Front", "front-east"] {
                let frames =
                    events.frames("kerberos/hub/shared", &hub(camera, 1000), false, now, 1000);
                assert_eq!(frames.len(), 1);
                assert_eq!(frames[0]["type"], "notification");
            }
        }
    }

    #[test]
    fn agent_hub_share_silence_episodes_and_repeats_extend_quiet_boundary() {
        let mut events = events();
        let start = Instant::now();
        let first = events
            .motion("kerberos/agent/front_camera", b"motion", false, start, 1000)
            .unwrap()
            .notification;
        assert_eq!(first["title"], "Kerberos Front camera motion detected");
        assert!(first.get("live_view_id").is_none());
        assert!(events
            .motion(
                "kerberos/hub/hub",
                &hub("front_camera", 1000),
                false,
                start + Duration::from_secs(14),
                1000
            )
            .is_none());
        assert!(events
            .motion(
                "kerberos/agent/front_camera",
                b"motion",
                false,
                start + Duration::from_secs(28),
                1000
            )
            .is_none());
        assert!(events
            .motion(
                "kerberos/agent/front_camera",
                b"motion",
                false,
                start + Duration::from_secs(42),
                1000
            )
            .is_none());
        let second = events
            .motion(
                "kerberos/agent/front_camera",
                b"motion",
                false,
                start + Duration::from_secs(57),
                1000,
            )
            .unwrap()
            .notification;
        assert_ne!(
            first["id"], second["id"],
            "host ID cache must not suppress a later episode"
        );
    }

    #[test]
    fn retained_invalid_and_stale_hub_events_cannot_extend_silence() {
        let start = Instant::now();
        for timestamp in [0, 939, 1011] {
            let mut events = events();
            assert!(events
                .motion(
                    "kerberos/hub/hub",
                    &hub("front", timestamp),
                    false,
                    start,
                    1000
                )
                .is_none());
            assert!(events
                .motion("kerberos/agent/front", b"motion", false, start, 1000)
                .is_some());
        }
        for timestamp in [940, 1010] {
            assert!(events()
                .motion(
                    "kerberos/hub/hub",
                    &hub("front", timestamp),
                    false,
                    start,
                    1000
                )
                .is_some());
        }
        let mut events = events();
        events
            .motion("kerberos/agent/front", b"motion", false, start, 1000)
            .unwrap();
        assert!(events
            .motion(
                "kerberos/agent/front",
                b"motion",
                true,
                start + Duration::from_secs(14),
                1000
            )
            .is_none());
        assert!(events
            .motion(
                "kerberos/agent/front",
                b"motion",
                false,
                start + SILENCE,
                1000
            )
            .is_some());
    }

    #[test]
    fn native_namespaces_never_accept_clip_urls_or_conflicting_envelopes() {
        let start = Instant::now();
        for payload in [
            b"https://example.test/clip.mp4".as_slice(),
            br#"{"agent_name":"a","video_url":"https://example.test/a"}"#,
            b"Motion",
            b"",
            &[0xff],
        ] {
            assert!(events()
                .motion("kerberos/agent/front", payload, false, start, 1000)
                .is_none());
        }
        for topic in [
            "kerberos/agent/",
            "kerberos/agent/front/extra",
            "kerberos/agent/+",
            "frigate/events",
            "ring/site/camera/front/motion/state",
        ] {
            assert!(events()
                .motion(topic, b"motion", false, start, 1000)
                .is_none());
        }
        for (field, value) in [
            ("hidden", json!(true)),
            ("encrypted", json!("false")),
            ("device_id", json!("other")),
        ] {
            let mut payload: Value = serde_json::from_slice(&hub("front", 1000)).unwrap();
            payload[field] = value;
            assert!(events()
                .motion(
                    "kerberos/hub/hub",
                    &serde_json::to_vec(&payload).unwrap(),
                    false,
                    start,
                    1000
                )
                .is_none());
        }
        assert!(events()
            .motion(
                "kerberos/agent/front",
                &vec![b' '; MAX_PAYLOAD_BYTES + 1],
                false,
                start,
                1000
            )
            .is_none());
    }

    #[test]
    fn configured_exact_live_identity_opens_automatically_without_private_url() {
        let config: Configuration=serde_json::from_value(json!({"revision":"fixture","values":{"mqtt_host":"localhost","camera_labels":r#"{"front_camera":"Entrance"}"#},"secrets":{"camera_live_urls":r#"{"front_camera":"https://camera.test/live?token=private#view"}"#}})).unwrap();
        config.validate().unwrap();
        let mut events = Events::new(&config).unwrap();
        let now = Instant::now();
        let frames = events.frames("kerberos/agent/front_camera", b"motion", false, now, 1000);
        assert_eq!(frames.len(), 1, "preview must not emit a duplicate toast");
        assert_eq!(frames[0]["type"], "live_view");
        assert_eq!(frames[0]["live_view_id"], "front_camera");
        assert_eq!(
            frames[0]["title"],
            "Kerberos Entrance camera motion detected"
        );
        assert!(!serde_json::to_string(&frames).unwrap().contains("private"));
        let unmapped = events.frames("kerberos/agent/Front_camera", b"motion", false, now, 1000);
        assert_eq!(unmapped.len(), 1);
        assert_eq!(unmapped[0]["type"], "notification");
        assert!(unmapped[0].get("live_view_id").is_none());
        assert!(events
            .frames(
                "kerberos/agent/front_camera",
                b"motion",
                true,
                now + SILENCE,
                1000
            )
            .is_empty());
        let next = events.frames(
            "kerberos/agent/front_camera",
            b"motion",
            false,
            now + SILENCE,
            1000,
        );
        assert_eq!(next.len(), 1);
        assert_ne!(next[0]["id"], frames[0]["id"]);
    }

    #[test]
    fn equal_display_labels_keep_distinct_camera_preview_identities() {
        let config: Configuration = serde_json::from_value(json!({
            "revision":"fixture",
            "values":{"mqtt_host":"localhost","camera_labels":r#"{"front":"Entrance","back":"Entrance"}"#},
            "secrets":{"camera_live_urls":r#"{"front":"https://camera.test/front?private=1","back":"https://camera.test/back?private=2"}"#}
        })).unwrap();
        let mut events = Events::new(&config).unwrap();
        let now = Instant::now();
        let front = events.frames("kerberos/agent/front", b"motion", false, now, 1000);
        let back = events.frames("kerberos/agent/back", b"motion", false, now, 1000);
        assert_eq!(front.len(), 1);
        assert_eq!(back.len(), 1);
        assert_eq!(front[0]["title"], back[0]["title"]);
        assert_eq!(front[0]["live_view_id"], "front");
        assert_eq!(back[0]["live_view_id"], "back");
        assert_ne!(front[0]["id"], back[0]["id"]);
        assert!(!serde_json::to_string(&[front, back])
            .unwrap()
            .contains("private"));
    }

    #[test]
    fn mqtt_live_validates_before_cooldown_and_token_rotation_does_not_extend_preview() {
        let endpoint = "https://ha.invalid/api/camera_proxy_stream/camera.front";
        let config: Configuration = serde_json::from_value(json!({
            "revision":"fixture", "values":{"mqtt_host":"localhost", "camera_labels":r#"{"front":"Entrance"}"#,
            "mqtt_live_endpoints":json!({"front":endpoint}).to_string()}, "secrets":{}
        })).unwrap();
        config.validate().unwrap();
        let mut events = Events::new(&config).unwrap();
        let now = Instant::now();
        let topic = "homelab/cameras/live/front";
        let url = format!("{endpoint}?token=private_fixture");
        for payload in [
            b"motion".as_slice(),
            &[0xff],
            b"",
            b"https://evil.invalid/?token=private",
            &vec![b'a'; 2049],
        ] {
            assert!(events.frames(topic, payload, false, now, 0).is_empty());
        }
        assert!(events
            .frames(topic, url.as_bytes(), true, now, 0)
            .is_empty());
        assert!(events
            .frames(
                "homelab/cameras/live/front/extra",
                url.as_bytes(),
                false,
                now,
                0
            )
            .is_empty());
        let first = events.frames(topic, url.as_bytes(), false, now, 0);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0]["type"], "mqtt_live");
        assert_eq!(first[0]["title"], "HA Entrance camera motion detected");
        let rotated = format!("{endpoint}?token=rotated_fixture");
        assert!(events
            .frames(
                topic,
                rotated.as_bytes(),
                false,
                now + Duration::from_secs(14),
                0
            )
            .is_empty());
        let next = events.frames(topic, rotated.as_bytes(), false, now + SILENCE, 0);
        assert_eq!(next.len(), 1);
        assert_ne!(first[0]["id"], next[0]["id"]);
        assert_eq!(next[0]["url"], rotated);
        assert_eq!(events.mqtt_seen.len(), 1);
    }

    #[test]
    fn cache_is_bounded_and_old_entries_expire() {
        let mut events = events();
        let now = Instant::now();
        for i in 0..MAX_CACHE_ENTRIES {
            assert!(events
                .motion(&format!("kerberos/agent/{i}"), b"motion", false, now, 1000)
                .is_some());
        }
        assert!(events
            .motion("kerberos/agent/next", b"motion", false, now, 1000)
            .is_none());
        assert_eq!(events.seen.len(), MAX_CACHE_ENTRIES);
        assert!(events
            .motion("kerberos/agent/next", b"motion", false, now + SILENCE, 1000)
            .is_some());
    }
}
