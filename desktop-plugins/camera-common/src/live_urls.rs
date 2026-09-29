//! Exact HA camera endpoints shared by the worker and native host.
//! The parent supplies url::Url (also re-exported by reqwest in the host).

use super::Url;
use std::collections::BTreeMap;

pub const TOPIC_PREFIX: &str = "homelab/cameras/live/";
const ERROR: &str = "Invalid MQTT live camera destination";

#[derive(Clone, Default)]
pub struct CameraLiveUrls {
    endpoints: BTreeMap<String, Url>,
}

impl std::fmt::Debug for CameraLiveUrls {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CameraLiveUrls { endpoints: [redacted] }")
    }
}

pub fn camera_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}

impl CameraLiveUrls {
    /// Settings contain token-free endpoints, keyed by the exact MQTT camera ID.
    pub fn parse(value: Option<&str>) -> Result<Self, &'static str> {
        if value.is_some_and(|v| v.len() > 16384) {
            return Err(ERROR);
        }
        let Some(value) = value.filter(|v| !v.trim().is_empty()) else {
            return Ok(Self::default());
        };
        let mapping: BTreeMap<String, String> = serde_json::from_str(value).map_err(|_| ERROR)?;
        if mapping.len() > 32 {
            return Err(ERROR);
        }
        let mut endpoints = BTreeMap::new();
        for (id, value) in mapping {
            let url = Url::parse(&value).map_err(|_| ERROR)?;
            let entity = url
                .path()
                .strip_prefix("/api/camera_proxy_stream/camera.")
                .ok_or(ERROR)?;
            if !camera_id(&id)
                || value.len() > 1536
                || url.as_str() != value
                || url.scheme() != "https"
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || value.contains('@')
                || url.query().is_some()
                || url.fragment().is_some()
                || entity.is_empty()
                || entity.len() > 128
                || !entity
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
            {
                return Err(ERROR);
            }
            endpoints.insert(id, url);
        }
        Ok(Self { endpoints })
    }

    pub fn topics(&self) -> impl Iterator<Item = String> + '_ {
        self.endpoints
            .keys()
            .map(|id| format!("{TOPIC_PREFIX}{id}"))
    }

    /// No normalization or query decoding can widen a configured destination.
    /// Only the camera-scoped URL-safe token may vary between live events.
    pub fn resolve(&self, id: &str, value: &str) -> Result<Url, &'static str> {
        if value.len() > 2048 {
            return Err(ERROR);
        }
        let endpoint = self.endpoints.get(id).ok_or(ERROR)?;
        let token = value
            .strip_prefix(endpoint.as_str())
            .and_then(|suffix| suffix.strip_prefix("?token="))
            .ok_or(ERROR)?;
        if token.is_empty()
            || token.len() > 512
            || !token
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        {
            return Err(ERROR);
        }
        Url::parse(value).map_err(|_| ERROR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const ENDPOINT: &str = "https://ha.invalid/api/camera_proxy_stream/camera.front";
    fn urls() -> CameraLiveUrls {
        CameraLiveUrls::parse(Some(&json!({"front": ENDPOINT}).to_string())).unwrap()
    }

    #[test]
    fn exact_camera_and_only_fresh_token_are_allowed() {
        let urls = urls();
        assert_eq!(
            urls.topics().collect::<Vec<_>>(),
            ["homelab/cameras/live/front"]
        );
        for token in ["current-camera-token_1", "rotated-camera-token_2"] {
            let value = format!("{ENDPOINT}?token={token}");
            assert_eq!(urls.resolve("front", &value).unwrap().as_str(), value);
            assert!(urls.resolve("back", &value).is_err());
        }
        for suffix in [
            "",
            "?token=",
            "?token=x&token=y",
            "?token=x&auth=y",
            "?token=x#private",
            "?token=x%26y",
            "?token=x+y",
            "?TOKEN=x",
            "?token=x\n",
            "?token=x ",
        ] {
            assert_eq!(
                urls.resolve("front", &format!("{ENDPOINT}{suffix}"))
                    .unwrap_err(),
                ERROR
            );
        }
        for value in [
            "http://ha.invalid/api/camera_proxy_stream/camera.front?token=private",
            "https://ha.invalid.evil/api/camera_proxy_stream/camera.front?token=private",
            "https://user:private@ha.invalid/api/camera_proxy_stream/camera.front?token=private",
            "https://ha.invalid:444/api/camera_proxy_stream/camera.front?token=private",
            "https://ha.invalid/api/camera_proxy_stream/camera.back?token=private",
            "https://ha.invalid/api/camera_proxy_stream/../camera_proxy_stream/camera.front?token=private",
            "https://ha.invalid/api/camera_proxy_stream/camera.%66ront?token=private",
            " https://ha.invalid/api/camera_proxy_stream/camera.front?token=private",
        ] {
            assert_eq!(urls.resolve("front", value).unwrap_err(), ERROR);
        }
        assert!(urls
            .resolve("front", &format!("{ENDPOINT}?token={}", "x".repeat(513)))
            .is_err());
        assert!(!format!("{urls:?}").contains("ha.invalid"));
    }

    #[test]
    fn configuration_rejects_tokens_paths_normalization_and_unbounded_maps() {
        for value in [
            format!("{ENDPOINT}?token=private"),
            format!("{ENDPOINT}#private"),
            ENDPOINT.replace("https", "http"),
            ENDPOINT.replace("ha.invalid", "user:private@ha.invalid"),
            ENDPOINT.replace("ha.invalid", "@ha.invalid"),
            ENDPOINT.replace("camera.front", "camera.%66ront"),
            ENDPOINT.replace("camera.front", "../camera.front"),
            ENDPOINT.replace("camera.front", "camera."),
            ENDPOINT.replace("camera.front", "camera.front/other"),
            format!(" {ENDPOINT}"),
        ] {
            assert_eq!(
                CameraLiveUrls::parse(Some(&json!({"front":value}).to_string())).unwrap_err(),
                ERROR
            );
        }
        for id in ["", "+", "#", "front/other", " front", "front\n"] {
            assert!(CameraLiveUrls::parse(Some(&json!({id:ENDPOINT}).to_string())).is_err());
        }
        let many: BTreeMap<_, _> = (0..33).map(|i| (format!("camera{i}"), ENDPOINT)).collect();
        assert!(CameraLiveUrls::parse(Some(&serde_json::to_string(&many).unwrap())).is_err());
        assert!(CameraLiveUrls::parse(Some(&" ".repeat(16385))).is_err());
        assert!(CameraLiveUrls::parse(Some("not-json-private")).is_err());
        assert!(CameraLiveUrls::parse(None)
            .unwrap()
            .resolve("front", ENDPOINT)
            .is_err());
    }
}
