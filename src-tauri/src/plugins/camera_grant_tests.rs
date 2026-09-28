//! Native authority tests independent of camera workers and external services.

use super::generation::GenerationLease;
use super::http_video::VideoError;
use super::media::{MediaError, MediaEvent, MediaService};
use super::package::{canonical_manifest_bytes, manifest_signing_payload};
use super::protocol::{
    parse_manifest, parse_worker_frame, HttpMediaKind, HttpVideoGrant, InventoryEntry,
    LiveViewGrant, PluginManifest, PluginPermission, WorkerConfiguration, WorkerMessage,
};
use super::runtime::QueuedHttpVideo;
use serde_json::json;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;

fn manifest(provider: &str) -> PluginManifest {
    let source = match provider {
        "kerberos" => include_str!("../../../scripts/plugins/kerberos-manifest.json"),
        "ring" => include_str!("../../../scripts/plugins/ring-manifest.json"),
        "frigate" => include_str!("../../../scripts/plugins/frigate-manifest.json"),
        _ => unreachable!(),
    };
    let mut manifest: PluginManifest = serde_json::from_str(source).unwrap();
    manifest.target = "aarch64-apple-darwin".into();
    manifest.inventory = vec![InventoryEntry {
        path: manifest.entrypoint.clone(),
        size: 1,
        sha256: "0".repeat(64),
    }];
    manifest.validate().unwrap();
    manifest
}

fn configuration(base: &str) -> WorkerConfiguration {
    WorkerConfiguration {
        revision: "native-grant-test".into(),
        values: json!({"snapshot_base_url":base}),
        secrets: BTreeMap::new(),
    }
}

#[test]
fn camera_manifests_require_explicit_secret_classification_and_permissions() {
    for (provider, field, permission) in [
        ("kerberos", "camera_live_urls", PluginPermission::LiveView),
        ("ring", "snapshot_bearer_token", PluginPermission::HttpVideo),
    ] {
        let original = manifest(provider);
        for replacement in [json!(false), json!(null)] {
            let mut changed = original.clone();
            changed.config_schema["properties"][field]["writeOnly"] = replacement;
            assert!(changed.validate().is_err());
        }
        let mut changed = original.clone();
        changed.config_schema["properties"][field]["type"] = json!("number");
        assert!(changed.validate().is_err());
        let mut changed = original;
        changed.permissions.retain(|value| *value != permission);
        assert!(changed.validate().is_err());
    }
    let mut ring = manifest("ring");
    ring.config_schema["properties"]["snapshot_base_url"]["writeOnly"] = json!(true);
    assert!(ring.validate().is_err());
    for seconds in [0, 19, 301, u16::MAX] {
        let mut ring = manifest("ring");
        ring.http_video.as_mut().unwrap().cooldown_seconds = Some(seconds);
        assert!(ring.validate().is_err());
    }
}

#[test]
fn media_credentials_are_explicit_bounded_and_never_part_of_diagnostics() {
    let manifest = manifest("ring");
    let secret = format!("fixture-{}", uuid::Uuid::new_v4());
    let mut configuration = configuration("https://camera.invalid/api/snapshot");
    configuration.values["snapshot_bearer_token"] = json!(secret);
    configuration
        .secrets
        .insert("ha_token".into(), secret.clone());
    let grant = HttpVideoGrant::from_manifest_configuration(&manifest, Some(&configuration))
        .unwrap()
        .unwrap();
    assert_eq!(
        grant.bearer_token(),
        None,
        "neither public values nor legacy HA tokens grant credentials"
    );
    configuration
        .secrets
        .insert("snapshot_bearer_token".into(), secret.clone());
    let grant = HttpVideoGrant::from_manifest_configuration(&manifest, Some(&configuration))
        .unwrap()
        .unwrap();
    assert_eq!(grant.bearer_token(), Some(secret.as_str()));
    assert_eq!(grant.cooldown(), Duration::from_secs(20));
    assert_eq!(format!("{grant:?}"), "HttpVideoGrant { base: [redacted] }");
    for secret in [
        "x".repeat(4097),
        "private\r\nX-Injected: value".into(),
        "private\0value".into(),
    ] {
        configuration
            .secrets
            .insert("snapshot_bearer_token".into(), secret);
        assert_eq!(
            HttpVideoGrant::from_manifest_configuration(&manifest, Some(&configuration))
                .unwrap_err(),
            "Invalid media credential"
        );
    }
}

#[test]
fn query_opt_in_preserves_exact_origin_and_path_scope() {
    let mut manifest = manifest("ring");
    let configuration = configuration("https://camera.invalid:9443/api/snapshot");
    let grant = HttpVideoGrant::from_manifest_configuration(&manifest, Some(&configuration))
        .unwrap()
        .unwrap();
    assert!(grant
        .validate_url("https://camera.invalid:9443/api/snapshot/front?private=value")
        .is_ok());
    for url in [
        "http://camera.invalid:9443/api/snapshot/front?private=value",
        "https://other.invalid:9443/api/snapshot/front?private=value",
        "https://camera.invalid/api/snapshot/front?private=value",
        "https://camera.invalid:9443/api/snapshot-other/front?private=value",
        "https://camera.invalid:9443/api/snapshot/../private?private=value",
        "https://camera.invalid:9443/api/snapshot/%252e%252e/private?private=value",
        "https://camera.invalid:9443/api/snapshot/front#private",
        "https://user:secret@camera.invalid:9443/api/snapshot/front",
    ] {
        let error = grant.validate_url(url).unwrap_err();
        assert!(!error.contains("private") && !error.contains("secret"));
    }
    manifest.http_video.as_mut().unwrap().allow_query = false;
    let grant = HttpVideoGrant::from_manifest_configuration(&manifest, Some(&configuration))
        .unwrap()
        .unwrap();
    assert!(grant
        .validate_url("https://camera.invalid:9443/api/snapshot/front?private=value")
        .is_err());
    assert!(grant
        .validate_url("https://camera.invalid:9443/api/snapshot/front")
        .is_ok());
}

fn live_configuration(value: String) -> WorkerConfiguration {
    WorkerConfiguration {
        revision: "live-test".into(),
        values: json!({}),
        secrets: BTreeMap::from([("camera_live_urls".into(), value)]),
    }
}

#[test]
fn live_destinations_are_bounded_exact_private_mappings() {
    let manifest = manifest("kerberos");
    let private = "https://camera.invalid/live?private=value#front";
    let configuration = live_configuration(json!({"front-camera": private}).to_string());
    let grant = LiveViewGrant::from_manifest_configuration(&manifest, Some(&configuration))
        .unwrap()
        .unwrap();
    assert_eq!(grant.resolve("front-camera").unwrap().as_str(), private);
    assert!(grant.resolve("front").is_none());
    assert_eq!(
        format!("{grant:?}"),
        "LiveViewGrant { destinations: [redacted] }"
    );
    let maps: BTreeMap<_, _> = (0..32)
        .map(|index| (format!("camera-{index}"), private))
        .collect();
    let configuration = live_configuration(serde_json::to_string(&maps).unwrap());
    assert!(LiveViewGrant::from_manifest_configuration(&manifest, Some(&configuration)).is_ok());
    let mut maps = maps;
    maps.insert("camera-33".into(), private);
    let mut invalid = vec![
        "not-json".into(),
        "[]".into(),
        "x".repeat(16 * 1024 + 1),
        serde_json::to_string(&maps).unwrap(),
    ];
    for (id, url) in [
        ("", private),
        ("camera/+", private),
        ("camera\n", private),
        ("front", "https://user:private@camera.invalid/live"),
        ("front", "https://@camera.invalid/live"),
        ("front", "file:///private/camera"),
        ("front", "https://camera.invalid/\nprivate"),
    ] {
        invalid.push(json!({id: url}).to_string());
    }
    invalid.push(
        json!({"front": "https://camera.invalid/".to_owned() + &"a".repeat(2048)}).to_string(),
    );
    for value in invalid {
        let configuration = live_configuration(value);
        let error = LiveViewGrant::from_manifest_configuration(&manifest, Some(&configuration))
            .unwrap_err();
        assert!(!error.contains("private"));
    }
}

#[test]
fn old_live_view_manifest_keeps_its_canonical_and_signing_bytes() {
    // This literal is the pre-preview-policy wire format, not generated by the
    // current serializer. Existing archive signatures depend on these bytes.
    const ORIGINAL: &str = concat!(
        r#"{"schema_version":1,"plugin_id":"fixture.camera","version":"0.1.0","host_api":"^1.8","target":"aarch64-apple-darwin","entrypoint":"bin/worker","config_schema":{"additionalProperties":false,"properties":{"camera_live_urls":{"maxLength":16384,"title":"Private destinations","type":"string","writeOnly":true}},"type":"object"},"permissions":["plugin_configuration","live_view"],"live_view":{"urls_setting":"camera_live_urls"},"inventory":[{"path":"bin/worker","size":1,"sha256":""#,
        "0000000000000000000000000000000000000000000000000000000000000000",
        r#""}],"signature":null}"#,
    );
    let manifest = parse_manifest(ORIGINAL.as_bytes()).unwrap();
    assert_eq!(
        manifest
            .live_view
            .as_ref()
            .unwrap()
            .preview_duration_seconds,
        None
    );
    assert_eq!(
        canonical_manifest_bytes(&manifest).unwrap(),
        ORIGINAL.as_bytes()
    );
    let mut signed_bytes = b"inverter-desktop:idplugin:manifest:v1\0".to_vec();
    signed_bytes.extend_from_slice(ORIGINAL.as_bytes());
    assert_eq!(manifest_signing_payload(&manifest).unwrap(), signed_bytes);

    let config = live_configuration(
        json!({"front": "https://private-camera.invalid/live?token=private"}).to_string(),
    );
    let grant = LiveViewGrant::from_manifest_configuration(&manifest, Some(&config))
        .unwrap()
        .unwrap();
    assert!(
        grant.resolve("front").is_some(),
        "legacy click authority remains"
    );
    assert_eq!(
        grant.preview("front").unwrap_err(),
        "Automatic live view is not authorized"
    );
}

#[test]
fn mapped_preview_policy_requires_explicit_permission_and_bounded_duration() {
    let config = live_configuration(json!({"front": "https://camera.invalid/live"}).to_string());
    for seconds in [1, 15, 30] {
        let mut manifest = manifest("kerberos");
        manifest.live_view.as_mut().unwrap().mqtt_urls_setting = None;
        manifest
            .live_view
            .as_mut()
            .unwrap()
            .preview_duration_seconds = Some(seconds);
        manifest.validate().unwrap();
        let grant = LiveViewGrant::from_manifest_configuration(&manifest, Some(&config))
            .unwrap()
            .unwrap();
        let (_, preview) = grant.preview("front").unwrap().unwrap();
        assert_eq!(
            preview.preview_duration(),
            Some(Duration::from_secs(seconds.into()))
        );
        assert_eq!(preview.cooldown(), Duration::from_secs(15));
    }
    for seconds in [0, 31, u16::MAX] {
        let mut manifest = manifest("kerberos");
        manifest.live_view.as_mut().unwrap().mqtt_urls_setting = None;
        manifest
            .live_view
            .as_mut()
            .unwrap()
            .preview_duration_seconds = Some(seconds);
        assert!(manifest.validate().is_err());
        assert_eq!(
            LiveViewGrant::from_manifest_configuration(&manifest, Some(&config)).unwrap_err(),
            "Invalid mapped live preview duration"
        );
    }
    let mut no_permission = manifest("kerberos");
    no_permission
        .permissions
        .retain(|value| *value != PluginPermission::LiveView);
    assert!(no_permission.validate().is_err());
    assert!(LiveViewGrant::from_manifest_configuration(&no_permission, Some(&config)).is_err());
    let mut no_declaration = manifest("kerberos");
    no_declaration.live_view = None;
    assert!(no_declaration.validate().is_err());
    assert!(LiveViewGrant::from_manifest_configuration(&no_declaration, Some(&config)).is_err());
    no_declaration
        .permissions
        .retain(|value| *value != PluginPermission::LiveView);
    assert!(
        LiveViewGrant::from_manifest_configuration(&no_declaration, Some(&config))
            .unwrap()
            .is_none()
    );
}

#[test]
fn mapped_preview_frames_accept_only_bounded_identity_and_title() {
    fn parse(value: &serde_json::Value) -> Result<WorkerMessage, String> {
        let mut bytes = serde_json::to_vec(value).unwrap();
        bytes.push(b'\n');
        parse_worker_frame(&bytes)
    }
    let valid = json!({
        "type": "live_view", "id": "a".repeat(128),
        "title": "é".repeat(64), "live_view_id": "c".repeat(128)
    });
    let parsed = parse(&valid).unwrap();
    assert!(matches!(parsed, WorkerMessage::LiveView { .. }));
    assert_eq!(serde_json::to_value(&parsed).unwrap(), valid);
    assert_eq!(
        format!("{parsed:?}"),
        "WorkerMessage { contents: [redacted] }"
    );
    for (field, value) in [
        ("id", json!("")),
        ("id", json!("a".repeat(129))),
        ("id", json!("episode/other")),
        ("title", json!(" ")),
        ("title", json!("é".repeat(65))),
        ("title", json!("private\nname")),
        ("live_view_id", json!("")),
        ("live_view_id", json!("c".repeat(129))),
        ("live_view_id", json!(" front")),
        ("live_view_id", json!("front/other")),
        ("live_view_id", json!("front+")),
        ("live_view_id", json!("front#")),
        ("live_view_id", json!("private\nname")),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        let error = parse(&invalid).unwrap_err();
        assert!(!error.contains("private"));
    }
    let mut url_injection = valid;
    url_injection["url"] = json!("https://private-camera.invalid/?token=private-secret");
    let error = parse(&url_injection).unwrap_err();
    assert!(!error.contains("private-camera") && !error.contains("private-secret"));
}

#[test]
fn mapped_preview_grants_only_the_exact_private_serialized_url_and_never_downloads() {
    let private = "https://private-camera.invalid:9443/live/Front?token=private%2Fsecret&quality=2#private-fragment";
    let mut manifest = manifest("kerberos");
    manifest
        .live_view
        .as_mut()
        .unwrap()
        .preview_duration_seconds = Some(15);
    let config = live_configuration(json!({"Front_camera": private}).to_string());
    let grant = LiveViewGrant::from_manifest_configuration(&manifest, Some(&config))
        .unwrap()
        .unwrap();
    let (url, preview) = grant.preview("Front_camera").unwrap().unwrap();
    assert_eq!(url.as_str(), private);
    assert_eq!(preview.validate_preview_url(private).unwrap(), url);
    assert_eq!(preview.preview_duration(), Some(Duration::from_secs(15)));
    assert_eq!(preview.cooldown(), Duration::from_secs(15));
    assert_eq!(preview.bearer_token(), None);
    assert_eq!(
        format!("{grant:?}"),
        "LiveViewGrant { destinations: [redacted] }"
    );
    assert_eq!(
        format!("{preview:?}"),
        "HttpVideoGrant { base: [redacted] }"
    );

    let altered = [
        private.replace("https:", "http:"),
        private.replace("private-camera.invalid", "other.invalid"),
        private.replace(":9443", ":9444"),
        private.replace("/live/Front", "/live/Other"),
        private.replace("/live/Front", "/live/Front/child"),
        private.replace("private%2Fsecret", "different-secret"),
        private.replace(
            "token=private%2Fsecret&quality=2",
            "quality=2&token=private%2Fsecret",
        ),
        private.replace("#private-fragment", "#other-fragment"),
        private.replace("#private-fragment", ""),
        // URL parsing could normalize these spellings; the grant is stricter.
        private.replace("private-camera.invalid", "PRIVATE-CAMERA.INVALID"),
        private.replace("/live/Front", "/live/./Front"),
        format!(" {private}"),
    ];
    for value in altered
        .iter()
        .map(String::as_str)
        .chain(std::iter::once("file:///private"))
    {
        assert_eq!(
            preview.validate_preview_url(value).unwrap_err(),
            "Live preview URL is outside its configured scope"
        );
    }
    for value in altered
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(private))
    {
        assert_eq!(
            preview.validate_url(value).unwrap_err(),
            "Mapped live view does not authorize downloads"
        );
    }
    assert!(grant.preview("front_camera").unwrap().is_none());
    assert!(grant.preview("missing").unwrap().is_none());
}

#[test]
fn mapped_preview_missing_configuration_cannot_gain_authority_from_public_values() {
    let mut manifest = manifest("kerberos");
    manifest
        .live_view
        .as_mut()
        .unwrap()
        .preview_duration_seconds = Some(15);
    assert_eq!(
        LiveViewGrant::from_manifest_configuration(&manifest, None).unwrap_err(),
        "Live view startup configuration missing"
    );
    for secret in [None, Some(""), Some(" "), Some("{}")] {
        let mut config = WorkerConfiguration {
            revision: "missing-private-map".into(),
            values: json!({"camera_live_urls": "{\"front\":\"https://private.invalid/live\"}"}),
            secrets: BTreeMap::new(),
        };
        if let Some(secret) = secret {
            config
                .secrets
                .insert("camera_live_urls".into(), secret.into());
        }
        let grant = LiveViewGrant::from_manifest_configuration(&manifest, Some(&config))
            .unwrap()
            .unwrap();
        assert!(grant.resolve("front").is_none());
        assert!(grant.preview("front").unwrap().is_none());
    }
}

async fn ready(receiver: &mut tokio::sync::mpsc::Receiver<MediaEvent>) -> super::media::ReadyMedia {
    match timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap()
        .unwrap()
    {
        MediaEvent::Ready(ready) => ready,
        MediaEvent::Close { .. } => panic!("unexpected close before transfer completion"),
    }
}

#[tokio::test]
async fn bearer_is_not_forwarded_by_redirects_or_reused_by_another_plugin() {
    let source = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let redirect = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", source.local_addr().unwrap());
    let destination = format!("http://{}/private", redirect.local_addr().unwrap());
    let secret = format!("fixture-{}", uuid::Uuid::new_v4());
    let expected = secret.clone();
    let server = tokio::spawn(async move {
        for index in 0..2 {
            let (mut socket, _) = timeout(Duration::from_secs(5), source.accept())
                .await
                .unwrap()
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                timeout(Duration::from_secs(3), socket.read_exact(&mut byte))
                    .await
                    .unwrap()
                    .unwrap();
                request.push(byte[0]);
                assert!(request.len() <= 8192);
            }
            let headers = String::from_utf8(request).unwrap().to_ascii_lowercase();
            if index == 0 {
                assert!(headers.contains(&format!("\r\nauthorization: bearer {expected}\r\n")));
                socket.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {destination}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            } else {
                assert!(!headers.contains("authorization:") && !headers.contains("cookie:"));
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\n\x89PNG\r\n\x1a\nDATA").await.unwrap();
            }
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let (service, mut receiver) = MediaService::new();
    service
        .initialize(directory.path().canonicalize().unwrap().join("media"))
        .await
        .unwrap();
    let mut configuration = configuration(&base);
    configuration
        .secrets
        .insert("snapshot_bearer_token".into(), secret);
    let ring = HttpVideoGrant::from_manifest_configuration(&manifest("ring"), Some(&configuration))
        .unwrap()
        .unwrap();
    service
        .try_submit(QueuedHttpVideo {
            camera_id: None,
            live_preview: false,
            lease: GenerationLease::new("inverter-desktop.ring".into(), 1, 1),
            grant: ring,
            id: uuid::Uuid::new_v4().to_string(),
            url: format!("{base}/snapshot/front?private=value"),
            title: "Front".into(),
            media_kind: HttpMediaKind::Png,
        })
        .unwrap();
    let first = ready(&mut receiver).await;
    assert_eq!(first.error, Some(MediaError::Transfer(VideoError::Http)));
    assert!(timeout(Duration::from_millis(50), redirect.accept())
        .await
        .is_err());
    service.window_failed(&first.media_id);
    let frigate = HttpVideoGrant::from_manifest_configuration(
        &manifest("frigate"),
        Some(&WorkerConfiguration {
            revision: "frigate-test".into(),
            values: json!({"frigate_base_url":base}),
            secrets: BTreeMap::new(),
        }),
    )
    .unwrap()
    .unwrap();
    service
        .try_submit(QueuedHttpVideo {
            camera_id: None,
            live_preview: false,
            lease: GenerationLease::new("inverter-desktop.frigate".into(), 1, 1),
            grant: frigate,
            id: uuid::Uuid::new_v4().to_string(),
            url: format!("{base}/snapshot/front"),
            title: "Other provider".into(),
            media_kind: HttpMediaKind::Png,
        })
        .unwrap();
    let second = ready(&mut receiver).await;
    assert_eq!(second.error, None);
    service.window_failed(&second.media_id);
    server.await.unwrap();
    service.shutdown().await.unwrap();
}

#[test]
fn mqtt_urls_require_native_camera_authority_and_never_grant_downloads_or_legacy_clicks() {
    let manifest = manifest("kerberos");
    let endpoint = "https://ha.invalid/api/camera_proxy_stream/camera.front";
    let mut config = live_configuration("{}".into());
    config.values["mqtt_live_endpoints"] = json!(json!({"front":endpoint}).to_string());
    let grant = LiveViewGrant::from_manifest_configuration(&manifest, Some(&config))
        .unwrap()
        .unwrap();
    let value = format!("{endpoint}?token=private_fixture");
    let (url, media) = grant.mqtt_preview("front", &value).unwrap();
    assert_eq!(url.as_str(), value);
    assert_eq!(media.preview_duration(), Some(Duration::from_secs(15)));
    assert_eq!(media.cooldown(), Duration::from_secs(15));
    assert!(media.validate_preview_url(&value).is_ok());
    assert!(media.validate_url(&value).is_err());
    assert!(media
        .validate_preview_url(&format!("{endpoint}?token=another_fixture"))
        .is_err());
    assert!(grant.resolve("front").is_none());
    assert!(grant.preview("front").unwrap().is_none());
    assert!(grant.mqtt_preview("back", &value).is_err());
    let frame = json!({"type":"mqtt_live","id":"fresh-event","title":"Camera", "camera_id":"front","url":value});
    let bytes = format!("{frame}\n");
    let message = parse_worker_frame(bytes.as_bytes()).unwrap();
    for debug in [
        format!("{message:?}"),
        format!("{media:?}"),
        format!("{grant:?}"),
    ] {
        assert!(!debug.contains("private_fixture") && !debug.contains("ha.invalid"));
    }
    let mut legacy = manifest.clone();
    legacy.live_view.as_mut().unwrap().mqtt_urls_setting = None;
    let legacy = LiveViewGrant::from_manifest_configuration(&legacy, Some(&config))
        .unwrap()
        .unwrap();
    assert!(legacy.mqtt_preview("front", &value).is_err());
    for seconds in [None, Some(0), Some(16), Some(30)] {
        let mut changed = manifest.clone();
        changed.live_view.as_mut().unwrap().preview_duration_seconds = seconds;
        assert!(changed.validate().is_err());
    }
    for replacement in [
        json!({"type":"string","writeOnly":true}),
        json!({"type":"number"}),
        json!(null),
    ] {
        let mut changed = manifest.clone();
        changed.config_schema["properties"]["mqtt_live_endpoints"] = replacement;
        assert!(changed.validate().is_err());
    }
    config.values["mqtt_live_endpoints"] = json!(json!({"front":value}).to_string());
    let error = LiveViewGrant::from_manifest_configuration(&manifest, Some(&config)).unwrap_err();
    assert!(!error.contains("private_fixture") && !error.contains("ha.invalid"));
}
