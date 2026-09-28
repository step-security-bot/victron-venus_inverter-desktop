//! Explicit acceptance tests for the separately built, signed Frigate package.
//!
//! Normal host tests do not build external crates or require a broker. CI runs
//! these ignored tests explicitly after building the actual worker executable.

use super::application::PackageApplication;
use super::package::{PublisherTrust, TrustStore};
use super::packaging::{build_package, write_package_atomic};
use super::presentation::Presentation;
use super::protocol::{DashboardContribution, PluginManifest};
use super::runtime::{PluginHost, WorkerState};
use ed25519_dalek::SigningKey;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

const PLUGIN: &str = "inverter-desktop.frigate";
const TOPIC: &str = "integration/frigate/events";
const NEXT_TOPIC: &str = "integration/frigate/updated";
const WAIT: Duration = Duration::from_secs(15);

pub(super) struct Broker {
    executable: PathBuf,
    config: PathBuf,
    log: PathBuf,
    pub(super) port: u16,
    child: Option<Child>,
}

impl Broker {
    pub(super) async fn new(root: &Path) -> Self {
        // These paths are test-only inputs. Neither is consulted by production
        // startup, installation, publisher policy, or webview IPC.
        let executable = required_file("MOSQUITTO_BIN");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let config = root.join("mosquitto.conf");
        fs::write(
            &config,
            format!(
                "listener {port} 127.0.0.1\nallow_anonymous true\npersistence false\nlog_type all\n"
            ),
        )
        .unwrap();
        let mut broker = Self {
            executable,
            config,
            log: root.join("mosquitto.log"),
            port,
            child: None,
        };
        broker.start().await;
        broker
    }

    pub(super) async fn start(&mut self) {
        assert!(self.child.is_none());
        let output = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .unwrap();
        self.child = Some(
            Command::new(&self.executable)
                .arg("-c")
                .arg(&self.config)
                .stdin(Stdio::null())
                .stdout(output.try_clone().unwrap())
                .stderr(output)
                .spawn()
                .expect("start the explicitly selected local Mosquitto binary"),
        );
        timeout(WAIT, async {
            loop {
                assert!(self.child.as_mut().unwrap().try_wait().unwrap().is_none());
                if TcpStream::connect(("127.0.0.1", self.port)).await.is_ok() {
                    break;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("private loopback MQTT broker must become ready");
    }

    pub(super) fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            child.wait().unwrap();
        }
    }

    pub(super) async fn publish(&self, topic: &str, payload: Value) {
        self.publish_bytes(topic, &serde_json::to_vec(&payload).unwrap(), false)
            .await;
    }

    pub(super) async fn publish_bytes(&self, topic: &str, payload: &[u8], retained: bool) {
        timeout(WAIT, async {
            let mut stream = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
            // A tiny MQTT 3.1.1 producer keeps this acceptance fixture independent
            // of the worker's MQTT client implementation and dependency graph.
            let mut connect = vec![0, 4, b'M', b'Q', b'T', b'T', 4, 2, 0, 10];
            let client = format!("acceptance-{}", uuid::Uuid::new_v4());
            mqtt_string(&mut connect, &client);
            stream.write_all(&packet(0x10, &connect)).await.unwrap();
            let mut ack = [0_u8; 4];
            stream.read_exact(&mut ack).await.unwrap();
            assert_eq!(ack, [0x20, 2, 0, 0], "local producer CONNACK");
            let mut publish = Vec::new();
            mqtt_string(&mut publish, topic);
            publish.extend_from_slice(payload);
            stream
                .write_all(&packet(if retained { 0x31 } else { 0x30 }, &publish))
                .await
                .unwrap();
            stream.write_all(&[0xe0, 0]).await.unwrap();
            stream.shutdown().await.unwrap();
        })
        .await
        .expect("local publisher must not block");
        sleep(Duration::from_millis(50)).await;
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(super) fn required_file(name: &str) -> PathBuf {
    let value = std::env::var_os(name).unwrap_or_else(|| {
        panic!("{name} must name an explicit fixture executable; this test never silently skips")
    });
    let path = PathBuf::from(value).canonicalize().unwrap();
    assert!(path.is_file(), "{name} must be a regular fixture file");
    path
}

fn mqtt_string(output: &mut Vec<u8>, value: &str) {
    output.extend_from_slice(&u16::try_from(value.len()).unwrap().to_be_bytes());
    output.extend_from_slice(value.as_bytes());
}

fn packet(header: u8, payload: &[u8]) -> Vec<u8> {
    let mut result = vec![header];
    let mut remaining = payload.len();
    loop {
        let mut byte = (remaining % 128) as u8;
        remaining /= 128;
        if remaining != 0 {
            byte |= 128;
        }
        result.push(byte);
        if remaining == 0 {
            break;
        }
    }
    result.extend_from_slice(payload);
    result
}

fn motion(id: &str, camera: &str) -> Value {
    json!({"type":"new","after":{"id":id,"camera":camera}})
}

async fn wait_connection(host: &PluginHost, expected: &str) {
    timeout(WAIT, async {
        loop {
            let found = host.snapshots().iter().any(|snapshot| {
                snapshot.plugin_id == PLUGIN
                    && snapshot.state == WorkerState::Running
                    && snapshot.contributions.iter().any(|item| {
                        matches!(item, DashboardContribution::Status { id, value, .. }
                            if id == "connection" && value == expected)
                    })
                    && snapshot.presentation.iter().any(|item| {
                        matches!(item, Presentation::Connection { id, title, connected }
                            if id == "frigate-mqtt" && title == "Frigate MQTT"
                                && *connected == (expected == "Connected"))
                    })
            });
            if found {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("real Frigate worker must report MQTT {expected}"));
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Notice {
    id: String,
    title: String,
    body: String,
}

fn drain(host: &PluginHost, notices: &mut Vec<Notice>) {
    host.dispatch_notifications(|notification| {
        // Never reenter host/auth from this synchronous guarded delivery callback.
        assert_eq!(notification.plugin_id, PLUGIN);
        notices.push(Notice {
            id: notification.id.clone(),
            title: notification.title.clone(),
            body: notification.body.clone(),
        });
    });
}

async fn expect_count(host: &PluginHost, notices: &mut Vec<Notice>, expected: usize) {
    timeout(WAIT, async {
        loop {
            drain(host, notices);
            assert!(
                notices.len() <= expected,
                "unexpected duplicate notification"
            );
            if notices.len() == expected {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("real motion event must reach the native notification adapter");
}

async fn expect_quiet(host: &PluginHost, notices: &mut Vec<Notice>, expected: usize) {
    // Positive events before/after suppression checks prove the real MQTT worker
    // is processing traffic; this interval additionally catches delayed output.
    sleep(Duration::from_millis(500)).await;
    drain(host, notices);
    assert_eq!(notices.len(), expected);
}

async fn configure(service: &PackageApplication, epoch: u64, port: u16, topic: &str) {
    let view = serde_json::to_value(service.get_settings(PLUGIN, epoch).await.unwrap()).unwrap();
    let saved = service
        .save_settings(
            PLUGIN,
            epoch,
            view["revision"].as_str().unwrap().into(),
            BTreeMap::from([
                ("mqtt_host".into(), json!("127.0.0.1")),
                ("mqtt_port".into(), json!(port)),
                ("mqtt_tls".into(), json!(false)),
                ("mqtt_topic".into(), json!(topic)),
            ]),
            BTreeMap::new(),
        )
        .await
        .unwrap();
    let result = serde_json::to_value(saved).unwrap();
    assert!(result["restart_error"].is_null());
}

async fn installed_application(root: &Path) -> (PackageApplication, PluginHost, u64) {
    installed_application_with_media(root, None).await
}

async fn installed_application_with_media(
    root: &Path,
    media: Option<super::media::MediaService>,
) -> (PackageApplication, PluginHost, u64) {
    let key = SigningKey::from_bytes(&rand::random::<[u8; 32]>());
    let publisher = "disposable-frigate-integration";
    let trust = TrustStore::new(vec![PublisherTrust::new(
        publisher.into(),
        key.verifying_key().to_bytes(),
        vec![PLUGIN.into()],
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
    let source = root.join("payload");
    fs::create_dir_all(source.join("bin")).unwrap();
    let mut manifest: PluginManifest = serde_json::from_str(include_str!(
        "../../../scripts/plugins/frigate-manifest.json"
    ))
    .unwrap();
    manifest.target = env!("INVERTER_DESKTOP_TARGET").into();
    manifest.entrypoint = format!(
        "bin/inverter-frigate-worker{}",
        std::env::consts::EXE_SUFFIX
    );
    fs::copy(
        required_file("INVERTER_FRIGATE_WORKER"),
        source.join(&manifest.entrypoint),
    )
    .unwrap();
    let bytes = build_package(manifest, &source, publisher, &key).unwrap();
    let path = root.join("frigate.idplugin");
    write_package_atomic(&path, &bytes).unwrap();
    let pending = service.begin_selection("config", epoch).unwrap();
    let preview = service
        .finish_selection(&pending, "config", epoch, path)
        .await
        .unwrap();
    assert_eq!(preview.plugin_id, PLUGIN);
    service
        .install_review(&preview.token, "config", epoch, false)
        .await
        .unwrap();
    assert!(host.snapshots().is_empty());
    (service, host, epoch)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly built INVERTER_FRIGATE_WORKER and local MOSQUITTO_BIN; CI runs this acceptance test"]
async fn signed_frigate_package_real_mqtt_lifecycle() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let mut broker = Broker::new(&root).await;
    let (service, host, epoch) = installed_application(&root).await;
    configure(&service, epoch, broker.port, TOPIC).await;
    broker
        .publish_bytes(
            TOPIC,
            &serde_json::to_vec(&motion("retained", "retained")).unwrap(),
            true,
        )
        .await;
    service.set_enabled(PLUGIN, true, epoch).await.unwrap();
    wait_connection(&host, "Connected").await;
    let mut notices = Vec::new();
    expect_quiet(&host, &mut notices, 0).await;
    broker.publish_bytes(TOPIC, b"", true).await;
    broker
        .publish("frigate/events", motion("wrong-topic", "other"))
        .await;
    broker
        .publish(
            TOPIC,
            json!({"type":"end","after":{"id":"ended","camera":"front"}}),
        )
        .await;
    broker.publish(TOPIC, motion("first", "front")).await;
    expect_count(&host, &mut notices, 1).await;
    assert!(notices[0].title.contains("Front"));
    assert_eq!(notices[0].body, "Motion started");
    broker.publish(TOPIC, motion("first", "front")).await;
    broker.publish(TOPIC, motion("same-camera", "front")).await;
    broker.publish(TOPIC, motion("second", "garden")).await;
    expect_count(&host, &mut notices, 2).await;
    expect_quiet(&host, &mut notices, 2).await;
    assert_ne!(notices[0].id, notices[1].id);

    broker.stop();
    wait_connection(&host, "Disconnected").await;
    broker.start().await;
    wait_connection(&host, "Connected").await;
    broker.publish(TOPIC, motion("first", "front")).await;
    broker.publish(TOPIC, motion("third", "garage")).await;
    expect_count(&host, &mut notices, 3).await;
    expect_quiet(&host, &mut notices, 3).await;

    configure(&service, epoch, broker.port, NEXT_TOPIC).await;
    wait_connection(&host, "Connected").await;
    broker.publish(TOPIC, motion("old-settings", "side")).await;
    broker.publish(NEXT_TOPIC, motion("fourth", "side")).await;
    expect_count(&host, &mut notices, 4).await;
    expect_quiet(&host, &mut notices, 4).await;

    service.set_enabled(PLUGIN, false, epoch).await.unwrap();
    broker
        .publish(NEXT_TOPIC, motion("disabled", "driveway"))
        .await;
    expect_quiet(&host, &mut notices, 4).await;
    service.set_enabled(PLUGIN, true, epoch).await.unwrap();
    wait_connection(&host, "Connected").await;
    broker
        .publish(NEXT_TOPIC, motion("fifth", "driveway"))
        .await;
    expect_count(&host, &mut notices, 5).await;
    // Queue real motion but revoke before dispatch: auth must discard pending
    // notifications in addition to stopping subsequent MQTT activity.
    broker
        .publish(NEXT_TOPIC, motion("queued-at-logout", "porch"))
        .await;
    timeout(WAIT, async {
        while !host.has_pending_notifications() {
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("a real MQTT notification must be queued before revocation");
    assert!(service.session_changed(false).is_none());
    broker
        .publish(NEXT_TOPIC, motion("logged-out", "porch"))
        .await;
    expect_quiet(&host, &mut notices, 5).await;
    let next_epoch = service.session_changed(true).unwrap();
    service.restore(next_epoch).await.unwrap();
    wait_connection(&host, "Connected").await;
    broker.publish(NEXT_TOPIC, motion("sixth", "porch")).await;
    expect_count(&host, &mut notices, 6).await;
    service
        .uninstall_with_settings(PLUGIN, true, next_epoch)
        .await
        .unwrap();
    broker
        .publish(NEXT_TOPIC, motion("uninstalled", "entry"))
        .await;
    expect_quiet(&host, &mut notices, 6).await;
    assert!(host.snapshots().is_empty());
    assert!(service
        .snapshot(next_epoch)
        .await
        .unwrap()
        .plugins
        .is_empty());
    service.close().await.unwrap();
}

/// This collector acknowledges simulated windows only. Native window creation,
/// stacking, playback and presentation require their separate platform checks.
struct PreviewWindows {
    ready: tokio::sync::mpsc::Receiver<super::media::ReadyMedia>,
    closed: tokio::sync::mpsc::Receiver<(String, tokio::sync::oneshot::Sender<()>)>,
    task: tokio::task::JoinHandle<()>,
}

impl PreviewWindows {
    fn new(
        media: super::media::MediaService,
        mut events: tokio::sync::mpsc::Receiver<super::media::MediaEvent>,
    ) -> Self {
        let (ready, received) = tokio::sync::mpsc::channel(8);
        let (closed, destroyed) = tokio::sync::mpsc::channel(8);
        let task = tokio::spawn(async move {
            let mut windows = std::collections::BTreeSet::new();
            while let Some(event) = events.recv().await {
                match event {
                    super::media::MediaEvent::Ready(item) => {
                        if media.window_ready(&item.media_id, &item.window_label) {
                            windows.insert(item.window_label.clone());
                            assert!(ready.send(item).await.is_ok());
                        } else {
                            // No simulated window was created, so absence is known.
                            media.window_failed(&item.media_id);
                        }
                    }
                    super::media::MediaEvent::Close { window_label } => {
                        let (acknowledge, acknowledged) = tokio::sync::oneshot::channel();
                        assert!(closed
                            .send((window_label.clone(), acknowledge))
                            .await
                            .is_ok());
                        // Keep the simulated window present until the test has
                        // checked that access is already revoked.
                        acknowledged.await.unwrap();
                        assert!(windows.remove(&window_label));
                        media.window_destroyed(&window_label);
                    }
                }
            }
        });
        Self {
            ready: received,
            closed: destroyed,
            task,
        }
    }

    async fn next_ready(&mut self) -> super::media::ReadyMedia {
        let ready = timeout(WAIT, self.ready.recv())
            .await
            .unwrap()
            .expect("owned live preview");
        assert!(
            ready.error.is_none(),
            "live preview must not enter the download path"
        );
        ready
    }

    async fn expect_closed(&mut self, label: &str) {
        let (closed, acknowledge) = timeout(WAIT, self.closed.recv())
            .await
            .unwrap()
            .expect("simulated native Close");
        assert_eq!(closed, label);
        acknowledge.send(()).unwrap();
    }
}

impl Drop for PreviewWindows {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Independent MQTT telemetry connection; plugin lifecycle must not disconnect it.
struct TelemetryProbe(TcpStream);

impl TelemetryProbe {
    async fn new(broker: &Broker) -> Self {
        timeout(WAIT, async {
            let mut stream = TcpStream::connect(("127.0.0.1", broker.port))
                .await
                .unwrap();
            let mut connect = vec![0, 4, b'M', b'Q', b'T', b'T', 4, 2, 0, 60];
            mqtt_string(
                &mut connect,
                &format!("telemetry-probe-{}", uuid::Uuid::new_v4()),
            );
            stream.write_all(&packet(0x10, &connect)).await.unwrap();
            assert_eq!(read_mqtt_packet(&mut stream).await, (0x20, vec![0, 0]));
            let mut subscribe = vec![0, 1];
            mqtt_string(&mut subscribe, "acceptance/core/telemetry");
            subscribe.push(0);
            stream.write_all(&packet(0x82, &subscribe)).await.unwrap();
            assert_eq!(read_mqtt_packet(&mut stream).await, (0x90, vec![0, 1, 0]));
            Self(stream)
        })
        .await
        .unwrap()
    }

    async fn assert_live(&mut self, broker: &Broker, sequence: u32) {
        let value = json!({"sequence":sequence});
        broker
            .publish("acceptance/core/telemetry", value.clone())
            .await;
        let (header, bytes) = timeout(WAIT, read_mqtt_packet(&mut self.0)).await.unwrap();
        assert_eq!(header, 0x30);
        let topic_bytes = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
        assert_eq!(&bytes[2..2 + topic_bytes], b"acceptance/core/telemetry");
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes[2 + topic_bytes..]).unwrap(),
            value
        );
    }
}

async fn read_mqtt_packet(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0];
    stream.read_exact(&mut header).await.unwrap();
    let mut length = 0;
    let mut scale = 1;
    loop {
        let mut byte = [0];
        stream.read_exact(&mut byte).await.unwrap();
        length += (byte[0] & 127) as usize * scale;
        assert!(length <= 4096 && scale <= 128 * 128 * 128);
        if byte[0] & 128 == 0 {
            break;
        }
        scale *= 128;
    }
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).await.unwrap();
    (header[0], payload)
}

fn completed_motion(id: &str, camera: &str) -> Value {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    json!({"type":"end","after":{"id":id,"camera":camera,"has_clip":true,"start_time":now - 3600.0,"end_time":now}})
}

async fn configure_preview(
    service: &PackageApplication,
    epoch: u64,
    broker: &Broker,
    topic: &str,
    base: &str,
    excluded: &[&str],
) {
    let view = serde_json::to_value(service.get_settings(PLUGIN, epoch).await.unwrap()).unwrap();
    let saved = service
        .save_settings(
            PLUGIN,
            epoch,
            view["revision"].as_str().unwrap().into(),
            BTreeMap::from([
                ("mqtt_host".into(), json!("127.0.0.1")),
                ("mqtt_port".into(), json!(broker.port)),
                ("mqtt_tls".into(), json!(false)),
                ("mqtt_topic".into(), json!(topic)),
                ("frigate_base_url".into(), json!(base)),
                (
                    "excluded_cameras".into(),
                    json!(serde_json::to_string(excluded).unwrap()),
                ),
            ]),
            BTreeMap::new(),
        )
        .await
        .unwrap();
    assert!(serde_json::to_value(saved).unwrap()["restart_error"].is_null());
}

async fn submit_live(
    host: &PluginHost,
    media: &super::media::MediaService,
) -> super::generation::GenerationLease {
    let mut requests = timeout(WAIT, async {
        loop {
            let requests = host.take_http_video_requests();
            if !requests.is_empty() {
                break requests;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("real worker must emit an authorized HTTP live request");
    assert_eq!(requests.len(), 1);
    let request = requests.pop().unwrap();
    assert!(request.live_preview);
    let lease = request.lease.clone();
    media.try_submit(request).unwrap();
    lease
}

async fn expect_media_empty(media: &super::media::MediaService, root: &Path) {
    timeout(WAIT, async {
        while media.has_owned_work() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("revoked media must drain its file owners");
    assert!(fs::read_dir(root.join("desktop-plugin-media"))
        .unwrap()
        .next()
        .is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly built INVERTER_FRIGATE_WORKER and local MOSQUITTO_BIN; CI runs this live acceptance test"]
async fn signed_frigate_package_real_mqtt_live_lifecycle() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let broker = Broker::new(&root).await;
    let mut telemetry = TelemetryProbe::new(&broker).await;
    let base = "http://127.0.0.1:1/frigate-prefix/";
    let (media, events) = super::media::MediaService::new();
    let mut windows = PreviewWindows::new(media.clone(), events);
    let (service, host, epoch) = installed_application_with_media(&root, Some(media.clone())).await;
    configure_preview(&service, epoch, &broker, TOPIC, base, &["excluded"]).await;
    service.set_enabled(PLUGIN, true, epoch).await.unwrap();
    wait_connection(&host, "Connected").await;
    telemetry.assert_live(&broker, 0).await;

    let mut notices = Vec::new();
    broker
        .publish(TOPIC, motion("excluded live", "excluded"))
        .await;
    broker
        .publish(TOPIC, completed_motion("excluded clip", "excluded"))
        .await;
    expect_quiet(&host, &mut notices, 0).await;
    assert!(host.take_http_video_requests().is_empty());
    broker.publish(TOPIC, motion("first live", "front")).await;
    let first_lease = submit_live(&host, &media).await;
    expect_quiet(&host, &mut notices, 0).await;
    let first = windows.next_ready().await;
    assert_eq!(
        first.live_url.as_ref().unwrap().as_str(),
        "http://127.0.0.1:1/frigate-prefix/api/front?fps=2&height=360"
    );
    assert!(first.window_label.starts_with("plugin-preview-"));
    assert!(media
        .read_range(&first.media_id, &first.window_label, None, false)
        .await
        .is_err());
    broker
        .publish(TOPIC, completed_motion("first live", "front"))
        .await;
    expect_quiet(&host, &mut notices, 0).await;
    assert!(
        host.take_http_video_requests().is_empty(),
        "end event cannot download or open a second clip on host 1.8"
    );

    // Settings replacement revokes the original instance before native absence.
    configure_preview(&service, epoch, &broker, NEXT_TOPIC, base, &[]).await;
    assert!(!first_lease.is_active());
    assert!(!media.is_window_active(&first.media_id, &first.window_label));
    windows.expect_closed(&first.window_label).await;
    expect_media_empty(&media, &root).await;
    wait_connection(&host, "Connected").await;
    telemetry.assert_live(&broker, 1).await;
    // Removing the setting restores admission for the previously excluded camera.
    broker
        .publish(NEXT_TOPIC, motion("second", "excluded"))
        .await;
    let second_lease = submit_live(&host, &media).await;
    assert_ne!(first_lease.instance_id(), second_lease.instance_id());
    let second = windows.next_ready().await;
    expect_quiet(&host, &mut notices, 0).await;
    service.set_enabled(PLUGIN, false, epoch).await.unwrap();
    assert!(!second_lease.is_active());
    assert!(!media.is_window_active(&second.media_id, &second.window_label));
    windows.expect_closed(&second.window_label).await;
    expect_media_empty(&media, &root).await;
    telemetry.assert_live(&broker, 2).await;

    service.set_enabled(PLUGIN, true, epoch).await.unwrap();
    wait_connection(&host, "Connected").await;
    broker.publish(NEXT_TOPIC, motion("logout", "garage")).await;
    let logout_lease = submit_live(&host, &media).await;
    let logout_preview = windows.next_ready().await;
    expect_quiet(&host, &mut notices, 0).await;
    assert!(service.session_changed(false).is_none());
    assert!(!logout_lease.is_active());
    assert!(!media.is_window_active(&logout_preview.media_id, &logout_preview.window_label));
    windows.expect_closed(&logout_preview.window_label).await;
    expect_media_empty(&media, &root).await;
    telemetry.assert_live(&broker, 3).await;

    let epoch = service.session_changed(true).unwrap();
    service.restore(epoch).await.unwrap();
    wait_connection(&host, "Connected").await;
    broker.publish(NEXT_TOPIC, motion("final", "side")).await;
    let final_lease = submit_live(&host, &media).await;
    let final_preview = windows.next_ready().await;
    expect_quiet(&host, &mut notices, 0).await;
    service
        .uninstall_with_settings(PLUGIN, true, epoch)
        .await
        .unwrap();
    assert!(!final_lease.is_active());
    assert!(!media.is_window_active(&final_preview.media_id, &final_preview.window_label));
    windows.expect_closed(&final_preview.window_label).await;
    expect_media_empty(&media, &root).await;
    assert!(host.snapshots().is_empty());
    assert!(service.snapshot(epoch).await.unwrap().plugins.is_empty());
    telemetry.assert_live(&broker, 4).await;
    service.close().await.unwrap();
    assert!(
        !windows.task.is_finished(),
        "simulated adapter must survive through cleanup"
    );
}
