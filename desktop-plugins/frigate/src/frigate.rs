use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};
use url::Url;

pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024;
const ID_TTL: Duration = Duration::from_secs(600);
const CAMERA_COOLDOWN: Duration = Duration::from_secs(45);
const MAX_CACHE_ENTRIES: usize = 512;

#[derive(Deserialize)]
struct FrigateEvent {
    #[serde(rename = "type")]
    kind: String,
    after: EventDetails,
}
#[derive(Deserialize)]
struct EventDetails {
    id: String,
    camera: String,
    start_time: Option<f64>,
    #[serde(default)]
    has_clip: serde_json::Value,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Motion {
    pub id: String,
    pub title: String,
    pub camera: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Clip {
    pub id: String,
    pub title: String,
    pub url: String,
}

#[derive(Default)]
pub struct ClipEvents {
    history: MotionEvents,
}

impl ClipEvents {
    pub fn excluding(cameras: BTreeSet<String>) -> Self {
        Self {
            history: MotionEvents::excluding(cameras),
        }
    }

    pub fn parse(
        &mut self,
        payload: &[u8],
        retained: bool,
        now: Instant,
        base: Option<&Url>,
    ) -> Option<Clip> {
        let base = base?;
        if retained || payload.len() > MAX_PAYLOAD_BYTES {
            return None;
        }
        let event: FrigateEvent = serde_json::from_slice(payload).ok()?;
        if event.kind != "end" || event.after.has_clip.as_bool() != Some(true) {
            return None;
        }
        // A completed long recording can have an old start_time. Clip admission
        // uses its own history and never applies the motion-start freshness test.
        let url = crate::media::clip_url(base, &event.after.id)?;
        let title = self.history.admit(&event.after, now)?;
        Some(Clip {
            id: format!(
                "frigate-clip-{:x}",
                Sha256::digest(event.after.id.as_bytes())
            ),
            title,
            url,
        })
    }
}

#[derive(Default)]
pub struct MotionEvents {
    excluded_cameras: BTreeSet<String>,
    ids: HashMap<String, Instant>,
    cameras: HashMap<String, Instant>,
}

impl MotionEvents {
    pub fn excluding(cameras: BTreeSet<String>) -> Self {
        Self {
            excluded_cameras: cameras,
            ..Self::default()
        }
    }

    pub fn parse(
        &mut self,
        payload: &[u8],
        retained: bool,
        now: Instant,
        unix_seconds: f64,
    ) -> Option<Motion> {
        if retained || payload.len() > MAX_PAYLOAD_BYTES {
            return None;
        }
        let event: FrigateEvent = serde_json::from_slice(payload).ok()?;
        let after = event.after;
        if event.kind != "new" {
            return None;
        }
        // Retained events are never live motion. Older timestamped replays are
        // also ignored; legacy publishers without start_time remain supported.
        if after.start_time.is_some_and(|start| {
            !start.is_finite() || start < unix_seconds - 600.0 || start > unix_seconds + 60.0
        }) {
            return None;
        }
        let title = self.admit(&after, now)?;
        Some(Motion {
            id: format!("frigate-{:x}", Sha256::digest(after.id.as_bytes())),
            title,
            camera: after.camera,
        })
    }

    fn admit(&mut self, after: &EventDetails, now: Instant) -> Option<String> {
        if self.excluded_cameras.contains(&after.camera)
            || after.id.is_empty()
            || after.id.len() > 128
            || after.camera.trim().is_empty()
            || after.camera.len() > 128
        {
            return None;
        }
        let camera: String = after.camera.chars().filter(|ch| !ch.is_control()).collect();
        let camera = camera.split_whitespace().collect::<Vec<_>>().join(" ");
        if camera.is_empty() {
            return None;
        }
        self.ids
            .retain(|_, seen| now.saturating_duration_since(*seen) < ID_TTL);
        self.cameras
            .retain(|_, seen| now.saturating_duration_since(*seen) < CAMERA_COOLDOWN);
        if self.ids.contains_key(&after.id)
            || self.cameras.contains_key(&after.camera)
            || self.ids.len() >= MAX_CACHE_ENTRIES
            || self.cameras.len() >= MAX_CACHE_ENTRIES
        {
            return None;
        }
        self.ids.insert(after.id.clone(), now);
        self.cameras.insert(after.camera.clone(), now);
        let mut chars = camera.chars();
        let capitalized = chars.next()?.to_uppercase().collect::<String>() + chars.as_str();
        let mut name = String::new();
        let max_name = 128 - "Frigate  camera motion detected".len();
        for ch in capitalized.chars() {
            if name.len() + ch.len_utf8() > max_name {
                break;
            }
            name.push(ch);
        }
        Some(format!("Frigate {name} camera motion detected"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn event(id: &str, camera: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({"type":"new","after":{"id":id,"camera":camera}})).unwrap()
    }

    fn completed(id: &str, camera: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({"type":"end","after":{
            "id":id,"camera":camera,"has_clip":true,"start_time":1.0
        }}))
        .unwrap()
    }

    #[test]
    fn excluded_camera_suppresses_motion_and_clips_without_consuming_history() {
        let excluded = BTreeSet::from(["front".into()]);
        let mut motions = MotionEvents::excluding(excluded.clone());
        let mut clips = ClipEvents::excluding(excluded);
        let now = Instant::now();
        let base = crate::media::base_url(Some("https://frigate.invalid"))
            .unwrap()
            .unwrap();
        assert!(motions
            .parse(&event("shared", "front"), false, now, 1000.0)
            .is_none());
        assert!(clips
            .parse(&completed("shared", "front"), false, now, Some(&base))
            .is_none());
        // Neither IDs nor camera cooldowns are spent by excluded deliveries.
        for camera in ["back", "Front", "front-east"] {
            let id = if camera == "back" { "shared" } else { camera };
            assert!(motions
                .parse(&event(id, camera), false, now, 1000.0)
                .is_some());
            assert!(clips
                .parse(&completed(id, camera), false, now, Some(&base))
                .is_some());
        }
    }

    #[test]
    fn start_and_completed_clip_have_independent_history_and_identifier_namespaces() {
        let now = Instant::now();
        let base = crate::media::base_url(Some("http://frigate.local:5000/prefix"))
            .unwrap()
            .unwrap();
        let mut motions = MotionEvents::default();
        let mut clips = ClipEvents::default();
        let motion = motions
            .parse(&event("123-a", "front"), false, now, 10000.0)
            .unwrap();
        // The same event's old start_time remains valid when its long recording ends.
        let clip = clips
            .parse(&completed("123-a", "front"), false, now, Some(&base))
            .unwrap();
        assert_ne!(motion.id, clip.id);
        assert!(motion.id.starts_with("frigate-"));
        assert!(clip.id.starts_with("frigate-clip-"));
        assert!(crate::config::valid_token(&clip.id));
        assert_eq!(clip.title, motion.title);
        assert_eq!(
            clip.url,
            "http://frigate.local:5000/prefix/api/events/123-a/clip.mp4"
        );
        assert!(clips
            .parse(&completed("123-a", "other"), false, now, Some(&base))
            .is_none());
        assert!(clips
            .parse(
                &completed("sibling", "front"),
                false,
                now + Duration::from_secs(44),
                Some(&base)
            )
            .is_none());
        assert!(clips
            .parse(&completed("different", "back"), false, now, Some(&base))
            .is_some());
        assert!(clips
            .parse(
                &completed("sibling", "front"),
                false,
                now + CAMERA_COOLDOWN,
                Some(&base)
            )
            .is_some());
        assert!(clips
            .parse(
                &completed("123-a", "front"),
                false,
                now + ID_TTL,
                Some(&base)
            )
            .is_some());
    }

    #[test]
    fn clips_require_a_base_a_completed_event_and_a_boolean_clip_flag() {
        let now = Instant::now();
        let base = crate::media::base_url(Some("https://frigate.local"))
            .unwrap()
            .unwrap();
        let mut clips = ClipEvents::default();
        assert!(clips
            .parse(&completed("a", "front"), false, now, None)
            .is_none());
        assert!(clips
            .parse(&completed("a", "front"), true, now, Some(&base))
            .is_none());
        for kind in ["new", "update"] {
            let payload = serde_json::to_vec(&json!({"type":kind,"after":{
                "id":"a","camera":"front","has_clip":true
            }}))
            .unwrap();
            assert!(clips.parse(&payload, false, now, Some(&base)).is_none());
        }
        for flag in [json!(false), json!("true"), json!(1), json!(null)] {
            let payload = serde_json::to_vec(&json!({"type":"end","after":{
                "id":"a","camera":"front","has_clip":flag
            }}))
            .unwrap();
            assert!(clips.parse(&payload, false, now, Some(&base)).is_none());
        }
        for payload in [
            b"invalid".to_vec(),
            vec![b' '; MAX_PAYLOAD_BYTES + 1],
            completed("a/b", "front"),
            completed("a%2fb", "front"),
            completed("..", "front"),
            completed("a", ""),
        ] {
            assert!(clips.parse(&payload, false, now, Some(&base)).is_none());
        }
        // Invalid/disabled events never consume the eventual valid clip's history.
        assert!(clips
            .parse(&completed("a", "front"), false, now, Some(&base))
            .is_some());
    }

    #[test]
    fn clip_titles_and_history_stay_bounded_independently_of_motion() {
        let now = Instant::now();
        let base = crate::media::base_url(Some("https://frigate.local"))
            .unwrap()
            .unwrap();
        let mut clips = ClipEvents::default();
        let clip = clips
            .parse(
                &completed("a", &("é".repeat(50) + "\n")),
                false,
                now,
                Some(&base),
            )
            .unwrap();
        assert!(clip.title.len() <= 128 && !clip.title.chars().any(char::is_control));
        for index in 0..MAX_CACHE_ENTRIES + 10 {
            let _ = clips.parse(
                &completed(&format!("id-{index}"), &format!("camera-{index}")),
                false,
                now,
                Some(&base),
            );
        }
        assert_eq!(clips.history.ids.len(), MAX_CACHE_ENTRIES);
        assert!(clips.history.cameras.len() <= MAX_CACHE_ENTRIES);
        assert!(clips
            .parse(
                &completed("fresh", "fresh"),
                false,
                now + ID_TTL,
                Some(&base)
            )
            .is_some());
    }
    #[test]
    fn notification_shape_and_legacy_cooldowns() {
        let now = Instant::now();
        let mut events = MotionEvents::default();
        let first = events
            .parse(&event("event-a", "front"), false, now, 1000.0)
            .unwrap();
        assert_eq!(first.title, "Frigate Front camera motion detected");
        assert!(crate::config::valid_token(&first.id));
        assert!(events
            .parse(&event("event-a", "other"), false, now, 1000.0)
            .is_none());
        assert!(events
            .parse(
                &event("event-b", "front"),
                false,
                now + Duration::from_secs(44),
                1000.0
            )
            .is_none());
        assert!(events
            .parse(&event("event-c", "back"), false, now, 1000.0)
            .is_some());
        assert!(events
            .parse(
                &event("event-b", "front"),
                false,
                now + CAMERA_COOLDOWN,
                1000.0
            )
            .is_some());
        assert!(events
            .parse(&event("event-a", "front"), false, now + ID_TTL, 1000.0)
            .is_some());
    }
    #[test]
    fn ignores_retained_old_future_non_start_and_malformed_events() {
        let now = Instant::now();
        let mut events = MotionEvents::default();
        assert!(events
            .parse(&event("a", "front"), true, now, 1000.0)
            .is_none());
        for kind in ["end", "update"] {
            let value = json!({"type":kind,"after":{"id":"a","camera":"front","has_clip":true}});
            assert!(events
                .parse(&serde_json::to_vec(&value).unwrap(), false, now, 1000.0)
                .is_none());
        }
        for start in [399.0, 1061.0] {
            let value =
                json!({"type":"new","after":{"id":"a","camera":"front","start_time":start}});
            assert!(events
                .parse(&serde_json::to_vec(&value).unwrap(), false, now, 1000.0)
                .is_none());
        }
        for value in [
            b"invalid".to_vec(),
            event("", "front"),
            event("a", ""),
            vec![b' '; MAX_PAYLOAD_BYTES + 1],
        ] {
            assert!(events.parse(&value, false, now, 1000.0).is_none());
        }
        assert!(events
            .parse(&event("a", "front"), false, now, 1000.0)
            .is_some());
    }
    #[test]
    fn sanitized_text_and_caches_remain_bounded() {
        let now = Instant::now();
        let mut events = MotionEvents::default();
        let motion = events
            .parse(&event("id\n", &("é".repeat(50) + "\n")), false, now, 1000.0)
            .unwrap();
        assert!(motion.title.len() <= 128 && !motion.title.chars().any(char::is_control));
        for index in 0..MAX_CACHE_ENTRIES + 10 {
            let _ = events.parse(
                &event(&format!("id-{index}"), &format!("camera-{index}")),
                false,
                now,
                1000.0,
            );
        }
        assert_eq!(events.ids.len(), MAX_CACHE_ENTRIES);
        assert!(events.cameras.len() <= MAX_CACHE_ENTRIES);
        assert!(events
            .parse(&event("fresh", "fresh"), false, now + ID_TTL, 1000.0)
            .is_some());
    }
}
