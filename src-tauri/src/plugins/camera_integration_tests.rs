//! Explicit installed-package acceptance with real workers and private fixtures.
//! No OS notification/window delivery or household endpoint is exercised here.

use super::application::PackageApplication;
use super::frigate_integration_tests::{required_file, Broker};
use super::media::{MediaEvent, MediaService, ReadyMedia};
use super::package::{PublisherTrust, TrustStore};
use super::packaging::{build_package, write_package_atomic};
use super::presentation::Presentation;
use super::protocol::{DashboardContribution, HttpMediaKind, PluginManifest};
use super::runtime::{LiveViewRequest, PluginHost, QueuedHttpVideo, WorkerState};
use ed25519_dalek::SigningKey;
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, path::Path, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{sleep, timeout};

const WAIT: Duration = Duration::from_secs(15);
const KERBEROS: &str = "inverter-desktop.kerberos";
const RING: &str = "inverter-desktop.ring";

async fn installed(
    root: &Path,
    provider: &str,
    media: Option<MediaService>,
) -> (PackageApplication, PluginHost, u64) {
    let (manifest_text, executable) = match provider {
        "kerberos" => (
            include_str!("../../../scripts/plugins/kerberos-manifest.json"),
            "INVERTER_KERBEROS_WORKER",
        ),
        "ring" => (
            include_str!("../../../scripts/plugins/ring-manifest.json"),
            "INVERTER_RING_WORKER",
        ),
        _ => panic!("unsupported fixture provider"),
    };
    let mut manifest: PluginManifest = serde_json::from_str(manifest_text).unwrap();
    let plugin = manifest.plugin_id.clone();
    let key = SigningKey::from_bytes(&rand::random::<[u8; 32]>());
    let publisher = "disposable-camera-integration";
    let trust = TrustStore::new(vec![PublisherTrust::new(
        publisher.into(),
        key.verifying_key().to_bytes(),
        vec![plugin.clone()],
    )
    .unwrap()])
    .unwrap();
    let host = PluginHost::default();
    let service = PackageApplication::new_with_media(
        host.clone(),
        env!("INVERTER_DESKTOP_TARGET").into(),
        true,
        Arc::new(|| {}),
        media,
    );
    let settings_key = rand::random::<[u8; 32]>();
    service
        .initialize_with_key(
            Ok(root.join("store")),
            Ok(trust),
            Arc::new(move || Ok(settings_key.to_vec())),
        )
        .await;
    let epoch = service.session_changed(true).unwrap();
    let payload = root.join("payload");
    fs::create_dir_all(payload.join("bin")).unwrap();
    manifest.target = env!("INVERTER_DESKTOP_TARGET").into();
    manifest.entrypoint = format!(
        "bin/inverter-{provider}-worker{}",
        std::env::consts::EXE_SUFFIX
    );
    fs::copy(
        required_file(executable),
        payload.join(&manifest.entrypoint),
    )
    .unwrap();
    let bytes = build_package(manifest, &payload, publisher, &key).unwrap();
    let path = root.join(format!("{provider}.idplugin"));
    write_package_atomic(&path, &bytes).unwrap();
    let pending = service.begin_selection("config", epoch).unwrap();
    let preview = service
        .finish_selection(&pending, "config", epoch, path)
        .await
        .unwrap();
    assert_eq!(preview.plugin_id, plugin);
    service
        .install_review(&preview.token, "config", epoch, false)
        .await
        .unwrap();
    assert!(host.snapshots().is_empty());
    (service, host, epoch)
}

async fn configure(
    service: &PackageApplication,
    plugin: &str,
    epoch: u64,
    port: u16,
    values: BTreeMap<String, Value>,
    secrets: BTreeMap<String, String>,
) {
    let view = serde_json::to_value(service.get_settings(plugin, epoch).await.unwrap()).unwrap();
    let mut settings = BTreeMap::from([
        ("mqtt_host".into(), json!("127.0.0.1")),
        ("mqtt_port".into(), json!(port)),
    ]);
    settings.extend(values);
    let saved = service
        .save_settings(
            plugin,
            epoch,
            view["revision"].as_str().unwrap().into(),
            settings,
            secrets
                .into_iter()
                .map(|(key, value)| (key, Some(value)))
                .collect(),
        )
        .await
        .unwrap();
    assert!(serde_json::to_value(saved).unwrap()["restart_error"].is_null());
}

async fn connection(host: &PluginHost, plugin: &str, expected: &str) {
    let provider = plugin.strip_prefix("inverter-desktop.").unwrap();
    let connection_id = format!("{provider}-mqtt");
    timeout(WAIT, async {
        loop {
            if host.snapshots().iter().any(|snapshot| {
                snapshot.plugin_id == plugin
                    && snapshot.state == WorkerState::Running
                    && snapshot.contributions.iter().any(|item| {
                        matches!(item, DashboardContribution::Status { id, value, .. }
                            if id == "connection" && value == expected)
                    })
                    && snapshot.presentation.iter().any(|item| {
                        matches!(item, Presentation::Connection { id, connected, .. }
                            if id == &connection_id && *connected == (expected == "Connected"))
                    })
            }) {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("installed camera worker must report broker state and connection presentation");
}

struct Notice {
    id: String,
    live: Option<LiveViewRequest>,
}
fn drain(host: &PluginHost, plugin: &str, notices: &mut Vec<Notice>) {
    host.dispatch_notifications(|notice| {
        assert_eq!(notice.plugin_id, plugin);
        notices.push(Notice {
            id: notice.id.clone(),
            live: notice.live_view.clone(),
        });
    });
}
async fn count(host: &PluginHost, plugin: &str, notices: &mut Vec<Notice>, expected: usize) {
    timeout(WAIT, async {
        loop {
            drain(host, plugin, notices);
            assert!(notices.len() <= expected, "duplicate camera notification");
            if notices.len() == expected {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("installed worker notification must arrive");
}
async fn quiet(host: &PluginHost, plugin: &str, notices: &mut Vec<Notice>, expected: usize) {
    sleep(Duration::from_millis(300)).await;
    drain(host, plugin, notices);
    assert_eq!(notices.len(), expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit INVERTER_KERBEROS_WORKER and private MOSQUITTO_BIN; CI runs this test"]
async fn signed_kerberos_package_real_mqtt_lifecycle() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let mut broker = Broker::new(&root).await;
    let (media, mut events) = MediaService::new();
    let (service, host, epoch) = installed(&root, "kerberos", Some(media.clone())).await;
    // No HTTP endpoint exists here: an automatic page preview must reach Ready
    // without downloading its private configured destination in the host.
    let live = "http://127.0.0.1:1/live?token=private-fixture#view";
    configure(
        &service,
        KERBEROS,
        epoch,
        broker.port,
        BTreeMap::new(),
        BTreeMap::from([(
            "camera_live_urls".into(),
            json!({"front_camera":live}).to_string(),
        )]),
    )
    .await;
    broker
        .publish_bytes("kerberos/agent/front_camera", b"motion", true)
        .await;
    service.set_enabled(KERBEROS, true, epoch).await.unwrap();
    connection(&host, KERBEROS, "Connected").await;
    let mut notices = Vec::new();
    quiet(&host, KERBEROS, &mut notices, 0).await;
    assert!(host.take_http_video_requests().is_empty());
    broker
        .publish_bytes("kerberos/agent/front_camera", b"", true)
        .await;
    broker
        .publish_bytes("kerberos/agent/front_camera", b"motion", false)
        .await;
    let request = media_request(&host).await;
    quiet(&host, KERBEROS, &mut notices, 0).await;
    assert!(request.live_preview);
    let preview_id = request.id.clone();
    assert_eq!(request.url, live);
    assert_eq!(
        request.grant.preview_duration(),
        Some(Duration::from_secs(15))
    );
    let original = request.lease.clone();
    assert!(original.is_active());
    media.try_submit(request).unwrap();
    let first = ready(&media, &mut events).await;
    assert_eq!(first.live_url.as_ref().unwrap().as_str(), live);
    assert!(first.window_label.starts_with("plugin-preview-"));
    assert!(media
        .read_range(&first.media_id, &first.window_label, None, false)
        .await
        .is_err());
    let utc = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    broker.publish("kerberos/hub/test",json!({"payload":{"action":"motion","device_id":"front_camera","value":{"timestamp":utc}}})).await;
    broker
        .publish_bytes("kerberos/agent/back_camera", b"motion", false)
        .await;
    count(&host, KERBEROS, &mut notices, 1).await;
    quiet(&host, KERBEROS, &mut notices, 1).await;
    assert!(
        host.take_http_video_requests().is_empty(),
        "same episode and unmapped cameras cannot open extra previews"
    );
    assert!(notices[0].live.is_none());
    assert_ne!(preview_id, notices[0].id);
    assert!(!serde_json::to_string(&host.snapshots())
        .unwrap()
        .contains("private-fixture"));
    assert!(
        !serde_json::to_string(&service.get_settings(KERBEROS, epoch).await.unwrap())
            .unwrap()
            .contains("private-fixture")
    );
    broker.stop();
    connection(&host, KERBEROS, "Disconnected").await;
    broker.start().await;
    connection(&host, KERBEROS, "Connected").await;
    broker
        .publish_bytes("kerberos/agent/front_camera", b"motion", false)
        .await;
    quiet(&host, KERBEROS, &mut notices, 1).await;
    assert!(host.take_http_video_requests().is_empty());
    service.set_enabled(KERBEROS, false, epoch).await.unwrap();
    assert!(!original.is_active());
    closed(&media, &mut events, &first).await;
    broker
        .publish_bytes("kerberos/agent/disabled", b"motion", false)
        .await;
    quiet(&host, KERBEROS, &mut notices, 1).await;
    service.set_enabled(KERBEROS, true, epoch).await.unwrap();
    connection(&host, KERBEROS, "Connected").await;
    broker
        .publish_bytes("kerberos/agent/front_camera", b"motion", false)
        .await;
    let request = media_request(&host).await;
    quiet(&host, KERBEROS, &mut notices, 1).await;
    let resumed = request.lease.clone();
    assert_ne!(resumed.instance_id(), original.instance_id());
    media.try_submit(request).unwrap();
    let second = ready(&media, &mut events).await;
    broker
        .publish_bytes("kerberos/agent/logout", b"motion", false)
        .await;
    timeout(WAIT, async {
        while !host.has_pending_notifications() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(service.session_changed(false).is_none());
    assert!(!resumed.is_active());
    closed(&media, &mut events, &second).await;
    quiet(&host, KERBEROS, &mut notices, 1).await;
    let epoch = service.session_changed(true).unwrap();
    service.restore(epoch).await.unwrap();
    connection(&host, KERBEROS, "Connected").await;
    broker
        .publish_bytes("kerberos/agent/front_camera", b"motion", false)
        .await;
    let request = media_request(&host).await;
    quiet(&host, KERBEROS, &mut notices, 1).await;
    let restored = request.lease.clone();
    media.try_submit(request).unwrap();
    let third = ready(&media, &mut events).await;
    service
        .uninstall_with_settings(KERBEROS, true, epoch)
        .await
        .unwrap();
    assert!(!restored.is_active());
    closed(&media, &mut events, &third).await;
    broker
        .publish_bytes("kerberos/agent/uninstalled", b"motion", false)
        .await;
    quiet(&host, KERBEROS, &mut notices, 1).await;
    assert!(host.snapshots().is_empty());
    assert!(service.snapshot(epoch).await.unwrap().plugins.is_empty());
    service.close().await.unwrap();
}

async fn media_request(host: &PluginHost) -> QueuedHttpVideo {
    timeout(WAIT, async {
        loop {
            let mut requests = host.take_http_video_requests();
            if !requests.is_empty() {
                assert_eq!(requests.len(), 1);
                break requests.pop().unwrap();
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("installed camera worker must request owned media")
}

async fn ready(
    media: &MediaService,
    events: &mut tokio::sync::mpsc::Receiver<MediaEvent>,
) -> ReadyMedia {
    let MediaEvent::Ready(item) = timeout(WAIT, events.recv()).await.unwrap().unwrap() else {
        panic!("expected owned media window");
    };
    assert!(item.error.is_none());
    assert!(media.window_ready(&item.media_id, &item.window_label));
    item
}

async fn closed(
    media: &MediaService,
    events: &mut tokio::sync::mpsc::Receiver<MediaEvent>,
    item: &ReadyMedia,
) {
    assert!(!media.is_window_active(&item.media_id, &item.window_label));
    let MediaEvent::Close { window_label } = timeout(WAIT, events.recv()).await.unwrap().unwrap()
    else {
        panic!("expected revoked media window");
    };
    assert_eq!(window_label, item.window_label);
    media.window_destroyed(&window_label);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit INVERTER_RING_WORKER and private MOSQUITTO_BIN; CI runs this test"]
async fn signed_ring_package_real_mqtt_snapshot_lifecycle() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let broker = Broker::new(&root).await;
    let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/camera", origin.local_addr().unwrap());
    let bearer = format!("fixture-{}", uuid::Uuid::new_v4());
    let expected_bearer = bearer.clone();
    let (sent, mut requests) = tokio::sync::mpsc::channel(3);
    let bytes = b"\x89PNG\r\n\x1a\nprivate-fixture-image";
    let server = tokio::spawn(async move {
        for _ in 0..3 {
            let (mut stream, _) = origin.accept().await.unwrap();
            let mut headers = Vec::new();
            timeout(WAIT, async {
                while !headers.ends_with(b"\r\n\r\n") {
                    assert!(headers.len() < 8192);
                    let mut byte = [0];
                    stream.read_exact(&mut byte).await.unwrap();
                    headers.push(byte[0]);
                }
            })
            .await
            .unwrap();
            let headers = String::from_utf8(headers).unwrap();
            assert!(headers.to_ascii_lowercase().contains(&format!(
                "\r\nauthorization: bearer {}\r\n",
                expected_bearer.to_ascii_lowercase()
            )));
            assert!(!headers.to_ascii_lowercase().contains("\r\ncookie:"));
            sent.send(headers.lines().next().unwrap().to_owned())
                .await
                .unwrap();
            let response=format!("HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len());
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(bytes).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    });
    let (media, mut events) = MediaService::new();
    let (service, host, epoch) = installed(&root, "ring", Some(media.clone())).await;
    configure(
        &service,
        RING,
        epoch,
        broker.port,
        BTreeMap::from([
            ("snapshot_base_url".into(), json!(base)),
            ("snapshot_media_kind".into(), json!("png")),
            (
                "camera_labels".into(),
                json!(r#"{"home/front":"Entrance","home/back":"Entrance"}"#),
            ),
        ]),
        BTreeMap::from([
            (
                "snapshot_url_template".into(),
                format!("{base}/{{device_id}}?event={{event}}"),
            ),
            ("snapshot_bearer_token".into(), bearer.clone()),
        ]),
    )
    .await;
    broker
        .publish_bytes("ring/home/camera/front/motion/state", b"ON", true)
        .await;
    service.set_enabled(RING, true, epoch).await.unwrap();
    connection(&host, RING, "Connected").await;
    let mut notices = Vec::new();
    let first = media_request(&host).await;
    count(&host, RING, &mut notices, 1).await;
    assert_eq!(first.media_kind, HttpMediaKind::Png);
    assert_eq!(first.grant.cooldown(), Duration::from_secs(20));
    assert_eq!(first.grant.bearer_token(), Some(bearer.as_str()));
    let lease = first.lease.clone();
    media.try_submit(first).unwrap();
    assert_eq!(
        timeout(WAIT, requests.recv()).await.unwrap().unwrap(),
        "GET /camera/front?event=motion HTTP/1.1"
    );
    let image = ready(&media, &mut events).await;
    let response = media
        .read_range(
            &image.media_id,
            &image.window_label,
            Some("bytes=0-"),
            false,
        )
        .await
        .unwrap();
    assert_eq!(response.content_type, "image/png");
    assert_eq!(response.bytes.as_slice(), bytes);
    drop(response);
    assert!(!serde_json::to_string(&host.snapshots())
        .unwrap()
        .contains(&bearer));
    assert!(
        !serde_json::to_string(&service.get_settings(RING, epoch).await.unwrap())
            .unwrap()
            .contains(&bearer)
    );
    broker
        .publish_bytes("ring/home/camera/front/motion/state", b"ON", false)
        .await;
    quiet(&host, RING, &mut notices, 1).await;
    assert!(host.take_http_video_requests().is_empty());
    // Both events intentionally have the same display label. Their explicit
    // event buckets must remain independent under the host's media cooldown.
    broker
        .publish_bytes("ring/home/camera/front/ding/state", b"ON", false)
        .await;
    let ding = media_request(&host).await;
    count(&host, RING, &mut notices, 2).await;
    media.try_submit(ding).unwrap();
    assert_eq!(
        timeout(WAIT, requests.recv()).await.unwrap().unwrap(),
        "GET /camera/front?event=ding HTTP/1.1"
    );
    let ding = ready(&media, &mut events).await;
    // Simulate the user closing this second image before revoking the plugin.
    media.window_destroyed(&ding.window_label);
    service.set_enabled(RING, false, epoch).await.unwrap();
    assert!(!lease.is_active());
    closed(&media, &mut events, &image).await;
    broker
        .publish_bytes("ring/home/camera/front/motion/state", b"", true)
        .await;
    service.set_enabled(RING, true, epoch).await.unwrap();
    connection(&host, RING, "Connected").await;
    broker
        .publish_bytes("ring/home/camera/back/ding/state", b"1", false)
        .await;
    let second = media_request(&host).await;
    count(&host, RING, &mut notices, 3).await;
    let final_lease = second.lease.clone();
    assert_ne!(lease.instance_id(), final_lease.instance_id());
    media.try_submit(second).unwrap();
    assert_eq!(
        timeout(WAIT, requests.recv()).await.unwrap().unwrap(),
        "GET /camera/back?event=ding HTTP/1.1"
    );
    let image = ready(&media, &mut events).await;
    service
        .uninstall_with_settings(RING, true, epoch)
        .await
        .unwrap();
    assert!(!final_lease.is_active());
    closed(&media, &mut events, &image).await;
    assert!(host.snapshots().is_empty());
    assert!(service.snapshot(epoch).await.unwrap().plugins.is_empty());
    timeout(WAIT, server).await.unwrap().unwrap();
    service.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit INVERTER_KERBEROS_WORKER and private MOSQUITTO_BIN; CI runs this test"]
async fn signed_kerberos_package_mqtt_url_preview_lifecycle() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let mut broker = Broker::new(&root).await;
    let (media, mut events) = MediaService::new();
    let (service, host, epoch) = installed(&root, "kerberos", Some(media.clone())).await;
    let endpoint = "https://ha.invalid/api/camera_proxy_stream/camera.front";
    let topic = "homelab/cameras/live/front";
    let initial = format!("{endpoint}?token=private_fixture");
    let rotated = format!("{endpoint}?token=rotated_fixture");
    configure(
        &service,
        KERBEROS,
        epoch,
        broker.port,
        BTreeMap::from([
            (
                "mqtt_live_endpoints".into(),
                json!(json!({"front":endpoint}).to_string()),
            ),
            ("excluded_legacy_cameras".into(), json!(r#"["front"]"#)),
        ]),
        BTreeMap::from([(
            "camera_live_urls".into(),
            r#"{"front":"https://legacy.invalid/live"}"#.into(),
        )]),
    )
    .await;
    broker.publish_bytes(topic, initial.as_bytes(), true).await;
    service.set_enabled(KERBEROS, true, epoch).await.unwrap();
    connection(&host, KERBEROS, "Connected").await;
    let mut notices = Vec::new();
    broker
        .publish_bytes("kerberos/agent/front", b"motion", false)
        .await;
    let legacy = json!({"device_id":"front","payload":{"action":"motion","device_id":"front","value":{"timestamp":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()}}});
    broker
        .publish_bytes("kerberos/hub/shared", legacy.to_string().as_bytes(), false)
        .await;
    quiet(&host, KERBEROS, &mut notices, 0).await;
    assert!(host.take_http_video_requests().is_empty());
    broker.publish_bytes(topic, b"", true).await;
    broker
        .publish_bytes(
            topic,
            b"https://other.invalid/api/camera_proxy_stream/camera.front?token=private_fixture",
            false,
        )
        .await;
    quiet(&host, KERBEROS, &mut notices, 0).await;
    assert!(host.take_http_video_requests().is_empty());
    broker.publish_bytes(topic, initial.as_bytes(), false).await;
    let request = media_request(&host).await;
    assert_eq!(request.url, initial);
    assert_eq!(
        request.grant.preview_duration(),
        Some(Duration::from_secs(15))
    );
    media.try_submit(request).unwrap();
    let first = ready(&media, &mut events).await;
    assert_eq!(
        media
            .live_preview_url(&first.media_id, &first.window_label)
            .unwrap()
            .as_str(),
        initial
    );
    assert!(media.live_preview_url(&first.media_id, "main").is_none());
    broker.stop();
    connection(&host, KERBEROS, "Disconnected").await;
    broker.start().await;
    connection(&host, KERBEROS, "Connected").await;
    broker.publish_bytes(topic, rotated.as_bytes(), false).await;
    quiet(&host, KERBEROS, &mut notices, 0).await;
    assert!(host.take_http_video_requests().is_empty());
    let public = serde_json::to_string(&host.snapshots()).unwrap();
    let settings =
        serde_json::to_string(&service.get_settings(KERBEROS, epoch).await.unwrap()).unwrap();
    assert!(!public.contains("private_fixture") && !settings.contains("private_fixture"));
    let MediaEvent::Close { window_label } = timeout(Duration::from_secs(17), events.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("preview must expire without another MQTT event");
    };
    assert_eq!(window_label, first.window_label);
    assert!(!media.is_window_active(&first.media_id, &first.window_label));
    media.window_destroyed(&window_label);
    broker.publish_bytes(topic, rotated.as_bytes(), false).await;
    let request = media_request(&host).await;
    assert_eq!(request.url, rotated, "fresh event uses its new token");
    let lease = request.lease.clone();
    media.try_submit(request).unwrap();
    let next = ready(&media, &mut events).await;
    service.set_enabled(KERBEROS, false, epoch).await.unwrap();
    assert!(!lease.is_active());
    closed(&media, &mut events, &next).await;
    quiet(&host, KERBEROS, &mut notices, 0).await;
    service.close().await.unwrap();
}
