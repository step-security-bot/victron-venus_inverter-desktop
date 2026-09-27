use super::*;
use crate::plugin_config::{DesktopPluginArtifact, DesktopPluginConfig};
use crate::plugins::package::{sha256_hex, PublisherTrust};
use crate::plugins::packaging::{build_package, build_pinned_package};
use crate::plugins::protocol::{PluginManifest, PluginPermission};
use crate::plugins::runtime::WorkerState;
use ed25519_dalek::SigningKey;
use serde_json::json;
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

fn declared_package(
    directory: &Path,
    version: &str,
    schema: Option<Value>,
) -> (DesktopPluginConfig, Vec<u8>) {
    declared_package_revision(directory, version, schema, None)
}

fn declared_package_revision(
    directory: &Path,
    version: &str,
    schema: Option<Value>,
    revision: Option<&str>,
) -> (DesktopPluginConfig, Vec<u8>) {
    let source = directory.join(format!("declared-payload-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&source).unwrap();
    let mode = if schema.is_some() {
        "configuration"
    } else {
        "worker"
    };
    let entrypoint = format!("{mode}{}", std::env::consts::EXE_SUFFIX);
    fs::copy(fixture(), source.join(&entrypoint)).unwrap();
    let mut manifest = PluginManifest {
        group: None,
        live_view: None,
        schema_version: 1,
        plugin_id: PLUGIN.into(),
        version: version.into(),
        host_api: "^1.0".into(),
        target: env!("INVERTER_DESKTOP_TARGET").into(),
        entrypoint,
        permissions: if schema.is_some() {
            vec![
                PluginPermission::DashboardContributions,
                PluginPermission::PluginConfiguration,
            ]
        } else {
            vec![PluginPermission::DashboardContributions]
        },
        config_schema: schema.unwrap_or_else(|| json!({"type":"object"})),
        http_video: None,
        inventory: Vec::new(),
        signature: None,
    };
    if let Some(revision) = revision {
        manifest.config_schema["description"] = json!(revision);
    }
    let bytes = build_pinned_package(manifest, &source).unwrap();
    let declaration = DesktopPluginConfig {
        plugin_id: PLUGIN.into(),
        version: version.into(),
        enabled: true,
        artifacts: BTreeMap::from([(
            env!("INVERTER_DESKTOP_TARGET").into(),
            DesktopPluginArtifact {
                url: format!("https://plugins.example.invalid/application-{version}.idplugin"),
                sha256: sha256_hex(&bytes),
                extra: BTreeMap::new(),
            },
        )]),
        extra: BTreeMap::new(),
    };
    (declaration, bytes)
}

async fn configured_application(
    directory: &Path,
    declarations: Vec<DesktopPluginConfig>,
    key: SettingsKeyProvider,
) -> (PackageApplication, PluginHost, u64) {
    let host = PluginHost::default();
    let service = PackageApplication::new(
        host.clone(),
        env!("INVERTER_DESKTOP_TARGET").into(),
        false,
        Arc::new(|| {}),
    );
    service
        .initialize_with_key(Ok(directory.join("store")), Ok(TrustStore::default()), key)
        .await;
    let epoch = service.session_configured(true, Ok(declarations)).unwrap();
    (service, host, epoch)
}

async fn reconcile_bytes(
    service: &PackageApplication,
    epoch: u64,
    bytes: &[u8],
    requests: &AtomicUsize,
) {
    service
        .reconcile_with(service.manager().unwrap(), epoch, |_| {
            requests.fetch_add(1, Ordering::SeqCst);
            let bytes = bytes.to_vec();
            async move { Ok(bytes) }
        })
        .await
        .unwrap();
}

async fn wait_configured_worker(host: &PluginHost) -> u64 {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(snapshot) = host.snapshots().into_iter().find(|snapshot| {
                snapshot.plugin_id == PLUGIN
                    && snapshot.state == WorkerState::Running
                    && !snapshot.contributions.is_empty()
            }) {
                return snapshot.generation;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

async fn configured_status(service: &PackageApplication, epoch: u64) -> Value {
    let snapshot = serde_json::to_value(service.snapshot(epoch).await.unwrap()).unwrap();
    assert!(snapshot["configuration_error"].is_null());
    assert_eq!(snapshot["configured"].as_array().unwrap().len(), 1);
    snapshot["configured"][0].clone()
}

#[tokio::test]
async fn video_window_preference_is_isolated_and_survives_store_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let key = settings_test_key();
    let (service, _, _) = configured_application(&root, vec![], key.clone()).await;
    assert!(!service.video_window_always_on_top(PLUGIN).unwrap());
    let mut data = SettingsData::default();
    data.values.insert(
        super::super::settings::VIDEO_ALWAYS_ON_TOP.into(),
        json!(true),
    );
    service
        .settings_store()
        .unwrap()
        .prepare_write(PLUGIN, &data)
        .unwrap()
        .commit()
        .unwrap();
    assert!(service.video_window_always_on_top(PLUGIN).unwrap());
    assert!(!service.video_window_always_on_top("other.camera").unwrap());
    service.close().await.unwrap();
    let (service, _, _) = configured_application(&root, vec![], key).await;
    assert!(service.video_window_always_on_top(PLUGIN).unwrap());
    assert!(!service.video_window_always_on_top("other.camera").unwrap());
    service.close().await.unwrap();
}

#[tokio::test]
async fn configured_reinstall_downloads_unsigned_package_once_and_reopens_offline() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (declaration, bytes) = declared_package(&root, "1.0.0", None);
    let key = settings_test_key();
    let (service, host, epoch) =
        configured_application(&root, vec![declaration.clone()], key.clone()).await;
    assert!(service.manager().unwrap().list().await.unwrap().is_empty());
    let requests = AtomicUsize::new(0);
    reconcile_bytes(&service, epoch, &bytes, &requests).await;
    let generation = wait_configured_worker(&host).await;
    let record = service.manager().unwrap().list().await.unwrap().remove(0);
    assert!(record.active.archive_pin);
    assert!(record.enabled);
    assert_eq!(configured_status(&service, epoch).await["state"], "ready");
    assert!(
        !service
            .snapshot(epoch)
            .await
            .unwrap()
            .installation_available
    );
    reconcile_bytes(&service, epoch, b"this must never be downloaded", &requests).await;
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(wait_configured_worker(&host).await, generation);
    // Helpers without a config persistence callback cannot override a pin.
    assert!(service.set_enabled(PLUGIN, false, epoch).await.is_err());
    assert!(service.rollback(PLUGIN, epoch).await.is_err());
    assert!(service.uninstall(PLUGIN, epoch).await.is_err());
    service.close().await.unwrap();

    let (service, host, epoch) = configured_application(&root, vec![declaration], key).await;
    assert!(host.snapshots().is_empty());
    service
        .reconcile_with(service.manager().unwrap(), epoch, |_| async {
            panic!("a verified cached package must not need network access")
        })
        .await
        .unwrap();
    wait_configured_worker(&host).await;
    assert_eq!(service.manager().unwrap().list().await.unwrap(), [record]);
    assert_eq!(configured_status(&service, epoch).await["state"], "ready");
    service.close().await.unwrap();
}

#[tokio::test]
async fn configured_disabled_intent_prevents_cached_start_and_installs_new_bytes_stopped() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (mut declaration, bytes) = declared_package(&root, "1.0.0", None);
    let key = settings_test_key();
    let (service, host, epoch) =
        configured_application(&root, vec![declaration.clone()], key.clone()).await;
    let requests = AtomicUsize::new(0);
    reconcile_bytes(&service, epoch, &bytes, &requests).await;
    wait_configured_worker(&host).await;
    service.close().await.unwrap();

    declaration.enabled = false;
    let (service, host, epoch) =
        configured_application(&root, vec![declaration.clone()], key).await;
    assert!(service.manager().unwrap().list().await.unwrap()[0].enabled);
    service
        .reconcile_with(service.manager().unwrap(), epoch, |_| async {
            panic!("disabling a cached plugin must not download")
        })
        .await
        .unwrap();
    assert!(host.snapshots().is_empty());
    assert!(!service.manager().unwrap().list().await.unwrap()[0].enabled);
    assert_eq!(
        configured_status(&service, epoch).await["state"],
        "disabled"
    );
    service.close().await.unwrap();

    let fresh = tempfile::tempdir().unwrap();
    let root = fresh.path().canonicalize().unwrap();
    let (service, host, epoch) =
        configured_application(&root, vec![declaration], settings_test_key()).await;
    let requests = AtomicUsize::new(0);
    reconcile_bytes(&service, epoch, &bytes, &requests).await;
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert!(host.snapshots().is_empty());
    assert!(!service.manager().unwrap().list().await.unwrap()[0].enabled);
    assert_eq!(
        configured_status(&service, epoch).await["state"],
        "disabled"
    );
    service.close().await.unwrap();
}

#[tokio::test]
async fn configured_missing_secret_keeps_a_settings_card_and_saved_settings_allow_restore() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (declaration, bytes) = declared_package(&root, "1.0.0", Some(settings_schema()));
    let (service, host, epoch) =
        configured_application(&root, vec![declaration], settings_test_key()).await;
    let requests = AtomicUsize::new(0);
    reconcile_bytes(&service, epoch, &bytes, &requests).await;
    let installed = service.manager().unwrap().list().await.unwrap().remove(0);
    assert!(!installed.enabled);
    assert!(host.snapshots().is_empty());
    let status = configured_status(&service, epoch).await;
    assert_eq!(status["state"], "failed");
    assert!(status["error"].is_string());
    assert_eq!(service.snapshot(epoch).await.unwrap().plugins.len(), 1);
    let settings = settings_view(&service, epoch).await;
    assert_eq!(settings["secret_present"]["token"], false);
    let saved = save_fixture_settings(&service, epoch, &settings["revision"]).await;
    assert!(saved.restart_error.is_none());
    assert!(host.snapshots().is_empty());
    // The bridge schedules this retry after settings save. A cached exact pin
    // does not invoke the production downloader, even with an offline endpoint.
    service.restore(epoch).await.unwrap();
    wait_configured_worker(&host).await;
    assert_eq!(configured_status(&service, epoch).await["state"], "ready");
    assert_eq!(
        service.manager().unwrap().list().await.unwrap()[0].active,
        installed.active
    );
    let result = host
        .action(PLUGIN, "echo", json!({}), Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(result["configuration_secret_matches"], true);
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    service.close().await.unwrap();
}

#[tokio::test]
async fn configured_failed_download_or_pin_preserves_working_version_and_update_is_transactional() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (first, first_bytes) = declared_package(&root, "1.0.0", None);
    let (second, second_bytes) = declared_package(&root, "1.1.0", None);
    let (service, host, mut epoch) =
        configured_application(&root, vec![first], settings_test_key()).await;
    let requests = AtomicUsize::new(0);
    reconcile_bytes(&service, epoch, &first_bytes, &requests).await;
    wait_configured_worker(&host).await;
    let original = service.manager().unwrap().list().await.unwrap().remove(0);
    for downloaded in [
        Err("Download is offline".to_owned()),
        Ok(first_bytes.clone()),
    ] {
        epoch = service
            .session_configured(true, Ok(vec![second.clone()]))
            .unwrap();
        service
            .reconcile_with(service.manager().unwrap(), epoch, |_| {
                let downloaded = downloaded.clone();
                async move { downloaded }
            })
            .await
            .unwrap();
        wait_configured_worker(&host).await;
        assert_eq!(
            service.manager().unwrap().list().await.unwrap().as_slice(),
            std::slice::from_ref(&original)
        );
        let status = configured_status(&service, epoch).await;
        assert_eq!(status["state"], "failed");
        assert!(status["error"].is_string());
        assert_eq!(
            host.action(PLUGIN, "echo", json!({}), Duration::from_secs(2))
                .await
                .unwrap()["ok"],
            true
        );
    }
    reconcile_bytes(&service, epoch, &second_bytes, &requests).await;
    wait_configured_worker(&host).await;
    let updated = service.manager().unwrap().list().await.unwrap().remove(0);
    assert_eq!(updated.active.version, "1.1.0");
    assert_eq!(updated.active.sha256, sha256_hex(&second_bytes));
    assert_eq!(updated.rollback, Some(original.active));
    assert!(updated.enabled);
    assert_eq!(configured_status(&service, epoch).await["state"], "ready");
    service.close().await.unwrap();
}

#[tokio::test]
async fn configured_same_version_rebuild_replaces_exact_bytes_and_then_restores_offline() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (first, first_bytes) = declared_package(&root, "1.0.0", None);
    let (rebuilt, rebuilt_bytes) =
        declared_package_revision(&root, "1.0.0", None, Some("new desktop release build"));
    assert_ne!(sha256_hex(&first_bytes), sha256_hex(&rebuilt_bytes));
    let key = settings_test_key();
    let (service, host, epoch) = configured_application(&root, vec![first], key.clone()).await;
    let requests = AtomicUsize::new(0);
    reconcile_bytes(&service, epoch, &first_bytes, &requests).await;
    wait_configured_worker(&host).await;
    let old_instance = host.snapshots()[0].instance_id.clone().unwrap();
    let previous = service.manager().unwrap().list().await.unwrap().remove(0);

    let epoch = service
        .session_configured(true, Ok(vec![rebuilt.clone()]))
        .unwrap();
    let restored_instance = Mutex::new(None);
    service
        .reconcile_with(service.manager().unwrap(), epoch, |_| {
            requests.fetch_add(1, Ordering::SeqCst);
            // Reconciliation resumes the cached worker before downloading. Capture
            // that instance so the assertion proves activation replaced it too.
            *restored_instance.lock().unwrap() = host.snapshots()[0].instance_id.clone();
            let bytes = rebuilt_bytes.clone();
            async move { Ok(bytes) }
        })
        .await
        .unwrap();
    wait_configured_worker(&host).await;
    let new_instance = host.snapshots()[0].instance_id.clone().unwrap();
    assert_ne!(new_instance, old_instance);
    assert_ne!(
        new_instance,
        restored_instance.into_inner().unwrap().unwrap()
    );
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    let replaced = service.manager().unwrap().list().await.unwrap().remove(0);
    assert_eq!(replaced.active.version, previous.active.version);
    assert_eq!(replaced.active.sha256, sha256_hex(&rebuilt_bytes));
    assert_eq!(replaced.rollback, Some(previous.active));
    assert_eq!(configured_status(&service, epoch).await["state"], "ready");
    service
        .reconcile_with(service.manager().unwrap(), epoch, |_| async {
            panic!("a matching rebuilt archive must not be downloaded again")
        })
        .await
        .unwrap();
    assert_eq!(
        host.snapshots()[0].instance_id.as_deref(),
        Some(new_instance.as_str())
    );
    service.close().await.unwrap();

    let (service, host, epoch) = configured_application(&root, vec![rebuilt], key).await;
    service
        .reconcile_with(service.manager().unwrap(), epoch, |_| async {
            panic!("the configured rebuilt archive must restore offline after restart")
        })
        .await
        .unwrap();
    wait_configured_worker(&host).await;
    assert_eq!(service.manager().unwrap().list().await.unwrap(), [replaced]);
    assert_eq!(configured_status(&service, epoch).await["state"], "ready");
    service.close().await.unwrap();
}

#[tokio::test]
async fn local_builds_verify_replace_restore_and_yield_to_new_release_pins() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (release, release_bytes) = declared_package(&root, "1.0.0", None);
    let (local, local_bytes) = declared_package_revision(&root, "1.0.0", None, Some("local build"));
    let (new_release, new_bytes) =
        declared_package_revision(&root, "1.0.0", None, Some("next release"));
    let (service, host, epoch) =
        configured_application(&root, vec![release.clone()], settings_test_key()).await;
    let requests = AtomicUsize::new(0);
    reconcile_bytes(&service, epoch, &release_bytes, &requests).await;
    wait_configured_worker(&host).await;
    let target = env!("INVERTER_DESKTOP_TARGET");
    let build = "a".repeat(32);
    let file = format!("{PLUGIN}-1.0.0-{target}.idplugin");
    let archives = root.join("local-plugin-builds").join(&build);
    fs::create_dir_all(&archives).unwrap();
    let selection = root.join("local-plugin-overrides.json");
    fs::write(
        &selection,
        serde_json::to_vec(&json!({
            "schema_version":1, "target":target, "build":build,
            "packages":[{"plugin_id":PLUGIN, "version":"1.0.0", "file":file,
                "sha256":local.artifacts[target].sha256,
                "baseline_sha256":release.artifacts[target].sha256}]
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(archives.join(&file), b"corrupt local archive").unwrap();
    reconcile_bytes(&service, epoch, b"no network", &requests).await;
    assert_eq!(configured_status(&service, epoch).await["state"], "failed");
    assert_eq!(
        service.manager().unwrap().list().await.unwrap()[0]
            .active
            .sha256,
        sha256_hex(&release_bytes)
    );
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    fs::write(archives.join(&file), &local_bytes).unwrap();
    reconcile_bytes(&service, epoch, b"no network", &requests).await;
    wait_configured_worker(&host).await;
    assert_eq!(configured_status(&service, epoch).await["local"], true);
    assert_eq!(
        service.manager().unwrap().list().await.unwrap()[0]
            .active
            .sha256,
        sha256_hex(&local_bytes)
    );
    assert_eq!(requests.load(Ordering::SeqCst), 1);

    // Explicit removal restores the saved release, without changing its declaration.
    let selected = fs::read(&selection).unwrap();
    fs::remove_file(&selection).unwrap();
    reconcile_bytes(&service, epoch, &release_bytes, &requests).await;
    assert_eq!(configured_status(&service, epoch).await["local"], false);
    assert_eq!(
        service.manager().unwrap().list().await.unwrap()[0]
            .active
            .sha256,
        sha256_hex(&release_bytes)
    );

    // A later published pin takes precedence even if the developer file remains.
    fs::write(&selection, &selected).unwrap();
    let epoch = service
        .session_configured(true, Ok(vec![new_release]))
        .unwrap();
    reconcile_bytes(&service, epoch, &new_bytes, &requests).await;
    assert_eq!(configured_status(&service, epoch).await["local"], false);
    assert_eq!(
        service.manager().unwrap().list().await.unwrap()[0]
            .active
            .sha256,
        sha256_hex(&new_bytes)
    );

    // Local installation retains disabled intent and never starts a disabled worker.
    let mut disabled = release;
    disabled.enabled = false;
    let epoch = service
        .session_configured(true, Ok(vec![disabled]))
        .unwrap();
    reconcile_bytes(&service, epoch, b"no network", &requests).await;
    let installed = service.manager().unwrap().list().await.unwrap().remove(0);
    assert!(!installed.enabled);
    assert_eq!(installed.active.sha256, sha256_hex(&local_bytes));
    assert!(!host
        .snapshots()
        .iter()
        .any(|snapshot| snapshot.state == WorkerState::Running));
    service.close().await.unwrap();
}

#[tokio::test]
async fn configured_downloads_are_dropped_on_logout_replacement_and_shutdown() {
    struct DownloadGuard(Arc<AtomicBool>);
    impl Drop for DownloadGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    for boundary in ["logout", "replacement", "shutdown"] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (declaration, _) = declared_package(&root, "1.0.0", None);
        let (service, host, epoch) =
            configured_application(&root, vec![declaration], settings_test_key()).await;
        let manager = service.manager().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started = Arc::new(Mutex::new(Some(started_tx)));
        let dropped = Arc::new(AtomicBool::new(false));
        let task_service = service.clone();
        let task_dropped = dropped.clone();
        let task = tokio::spawn(async move {
            task_service
                .reconcile_with(manager, epoch, move |_| {
                    let started = started.lock().unwrap().take().unwrap();
                    let guard = DownloadGuard(task_dropped.clone());
                    async move {
                        let _guard = guard;
                        started.send(()).unwrap();
                        std::future::pending::<Result<Vec<u8>, String>>().await
                    }
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        let replacement = match boundary {
            "replacement" => {
                let (mut declaration, bytes) = declared_package(&root, "1.1.0", None);
                declaration.enabled = false;
                let expected_url = declaration.artifacts[env!("INVERTER_DESKTOP_TARGET")]
                    .url
                    .clone();
                let next = service
                    .session_configured(true, Ok(vec![declaration]))
                    .unwrap();
                assert_ne!(next, epoch);
                assert!(!host.is_authorized_epoch(epoch));
                Some((next, bytes, expected_url))
            }
            "logout" => {
                assert!(service.session_changed(false).is_none());
                None
            }
            "shutdown" => {
                service.begin_shutdown();
                None
            }
            _ => unreachable!(),
        };
        assert!(
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .is_err(),
            "{boundary}"
        );
        assert!(dropped.load(Ordering::SeqCst), "{boundary}");
        assert!(service.manager().unwrap().list().await.unwrap().is_empty());
        assert!(host.snapshots().is_empty());
        if let Some((next, bytes, expected_url)) = replacement {
            service
                .reconcile_with(service.manager().unwrap(), next, |url| {
                    assert_eq!(url, expected_url);
                    let bytes = bytes.clone();
                    async move { Ok(bytes) }
                })
                .await
                .unwrap();
            let installed = service.manager().unwrap().list().await.unwrap();
            assert_eq!(installed.len(), 1);
            assert_eq!(installed[0].active.version, "1.1.0");
            assert!(!installed[0].enabled);
            let desired = configured_status(&service, next).await;
            assert_eq!(desired["version"], "1.1.0");
            assert_eq!(desired["state"], "disabled");
        }
        service.close().await.unwrap();
    }
}

#[tokio::test]
async fn invalid_configured_declarations_report_errors_without_downloading_or_installing() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (mut declaration, _) = declared_package(&root, "1.0.0", None);
    declaration.version = "latest".into();
    let (service, host, epoch) =
        configured_application(&root, vec![declaration], settings_test_key()).await;
    assert!(service
        .reconcile_with(service.manager().unwrap(), epoch, |_| async {
            panic!("invalid declarations must not start downloads")
        })
        .await
        .is_err());
    let snapshot = service.snapshot(epoch).await.unwrap();
    assert!(snapshot.configuration_error.is_some());
    assert!(snapshot.configured.is_empty());
    assert!(snapshot.plugins.is_empty());
    assert!(host.snapshots().is_empty());
    service.close().await.unwrap();
}

const PLUGIN: &str = "test.application";
const PUBLISHER: &str = "application-tests-only";

fn trust() -> TrustStore {
    TrustStore::new(vec![PublisherTrust::new(
        PUBLISHER.into(),
        SigningKey::from_bytes(&[29; 32]).verifying_key().to_bytes(),
        vec![PLUGIN.into()],
    )
    .unwrap()])
    .unwrap()
}

fn fixture() -> PathBuf {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY
        .get_or_init(|| {
            let directory = std::env::temp_dir().join(format!(
                "inverter-application-worker-{}",
                std::process::id()
            ));
            fs::create_dir_all(&directory).unwrap();
            let binary = directory.join(format!("worker{}", std::env::consts::EXE_SUFFIX));
            let status = std::process::Command::new(
                std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()),
            )
            .arg("--edition=2021")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugin_worker.rs"))
            .arg("-o")
            .arg(&binary)
            .output()
            .unwrap();
            assert!(
                status.status.success(),
                "{}",
                String::from_utf8_lossy(&status.stderr)
            );
            binary
        })
        .clone()
}

fn archive(directory: &Path, version: &str) -> PathBuf {
    configured_archive(directory, version, None, "worker")
}

fn configured_archive(
    directory: &Path,
    version: &str,
    schema: Option<Value>,
    mode: &str,
) -> PathBuf {
    let source = directory.join(format!("payload-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&source).unwrap();
    let entrypoint = format!("{mode}{}", std::env::consts::EXE_SUFFIX);
    fs::copy(fixture(), source.join(&entrypoint)).unwrap();
    let metadata = PluginManifest {
        group: None,
        live_view: None,
        http_video: None,
        schema_version: 1,
        plugin_id: PLUGIN.into(),
        version: version.into(),
        host_api: "^1.0".into(),
        target: env!("INVERTER_DESKTOP_TARGET").into(),
        entrypoint,
        permissions: if schema.is_some() {
            vec![
                PluginPermission::DashboardContributions,
                PluginPermission::PluginConfiguration,
            ]
        } else {
            vec![PluginPermission::DashboardContributions]
        },
        config_schema: schema.unwrap_or_else(|| json!({"type":"object"})),
        inventory: Vec::new(),
        signature: None,
    };
    let bytes = build_package(
        metadata,
        &source,
        PUBLISHER,
        &SigningKey::from_bytes(&[29; 32]),
    )
    .unwrap();
    let path = directory.join(format!("{}.idplugin", uuid::Uuid::new_v4()));
    fs::write(&path, bytes).unwrap();
    path
}

async fn application(directory: &Path) -> (PackageApplication, PluginHost, u64) {
    let host = PluginHost::default();
    let service = PackageApplication::new(
        host.clone(),
        env!("INVERTER_DESKTOP_TARGET").into(),
        true,
        Arc::new(|| {}),
    );
    service
        .initialize(Ok(directory.join("store")), Ok(trust()))
        .await;
    let epoch = service.session_changed(true).unwrap();
    (service, host, epoch)
}

async fn review(service: &PackageApplication, path: PathBuf, epoch: u64) -> PackagePreview {
    let token = service.begin_selection("config", epoch).unwrap();
    service
        .finish_selection(&token, "config", epoch, path)
        .await
        .unwrap()
}

async fn install(service: &PackageApplication, path: PathBuf, epoch: u64, enabled: bool) {
    let preview = review(service, path, epoch).await;
    service
        .install_review(&preview.token, "config", epoch, enabled)
        .await
        .unwrap();
}

#[tokio::test]
async fn cancelled_caller_keeps_session_expiry_active_until_transaction_finishes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (service, host, epoch) = application(&root).await;
    let manager = service.manager().unwrap();
    let package = manager
        .inspect_archive(archive(&root, "1.0.0"), epoch)
        .await
        .unwrap();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
    let caller_service = service.clone();
    let caller = tokio::spawn(async move {
        let activity = caller_service.begin_activity();
        caller_service
            .run_operation(PLUGIN.into(), epoch, activity, async move {
                started_tx.send(()).unwrap();
                release_rx.await.unwrap();
                let result = manager
                    .install_verified_in_epoch(package, false, epoch)
                    .await
                    .map(|_| ());
                finished_tx.send(result.clone()).unwrap();
                result
            })
            .await
    });
    started_rx.await.unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    service.window_closed("config");
    assert!(host.snapshots().is_empty());
    assert!(service.0.pending.lock().unwrap().is_none());
    assert!(service.has_session_work());
    // The bridge watchdog continues checking auth even with no preview or worker.
    service.session_changed(false);
    release_tx.send(()).unwrap();
    assert!(finished_rx.await.unwrap().is_err());
    tokio::time::timeout(Duration::from_secs(2), async {
        while service.has_session_work() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let next = service.session_changed(true).unwrap();
    assert!(service.snapshot(next).await.unwrap().plugins.is_empty());
    service.close().await.unwrap();
}

#[tokio::test]
async fn reviewed_bytes_survive_file_replacement_and_consent_is_single_use() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (service, host, epoch) = application(&root).await;
    let original = archive(&root, "1.0.0");
    let preview = review(&service, original.clone(), epoch).await;
    assert_eq!(preview.version, "1.0.0");
    assert_eq!(preview.publisher_key_id, PUBLISHER);
    fs::copy(archive(&root, "2.0.0"), original).unwrap();
    let (first, second) = tokio::join!(
        service.install_review(&preview.token, "config", epoch, true),
        service.install_review(&preview.token, "config", epoch, true),
    );
    assert_ne!(first.is_ok(), second.is_ok());
    let snapshot = service.snapshot(epoch).await.unwrap();
    assert_eq!(snapshot.plugins[0].version, "1.0.0");
    assert_eq!(
        snapshot.plugins[0].runtime.as_ref().unwrap().state,
        WorkerState::Running
    );
    assert!(!host.snapshots().is_empty());
    service.uninstall(PLUGIN, epoch).await.unwrap();
    service.close().await.unwrap();
}

#[tokio::test]
async fn delayed_dialog_and_review_cannot_cross_logout_or_closed_window() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (service, _, epoch) = application(&root).await;
    let path = archive(&root, "1.0.0");
    let token = service.begin_selection("config", epoch).unwrap();
    assert!(service.begin_selection("config", epoch).is_err());
    service.session_changed(false);
    let next = service.session_changed(true).unwrap();
    assert!(service
        .finish_selection(&token, "config", epoch, path.clone())
        .await
        .is_err());
    let current = review(&service, path.clone(), next).await;
    assert!(service
        .install_review(&current.token, "main", next, true)
        .await
        .is_err());
    service.window_closed("config");
    assert!(service
        .install_review(&current.token, "config", next, true)
        .await
        .is_err());
    let pending = service.begin_selection("config", next).unwrap();
    service.window_closed("config");
    assert!(service
        .finish_selection(&pending, "config", next, path)
        .await
        .is_err());
    assert!(service.snapshot(next).await.unwrap().plugins.is_empty());
    service.close().await.unwrap();
}

#[tokio::test]
async fn preview_replacement_expiry_and_cancel_never_reuse_old_consent() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (service, _, epoch) = application(&root).await;
    let path = archive(&root, "1.0.0");
    let first = review(&service, path.clone(), epoch).await;
    let next = review(&service, path.clone(), epoch).await;
    service.discard(&first.token, "config", epoch);
    assert!(service
        .install_review(&first.token, "config", epoch, false)
        .await
        .is_err());
    assert!(service
        .finish_selection(&next.token, "config", epoch, path)
        .await
        .is_err());
    service.0.pending.lock().unwrap().as_mut().unwrap().created = Instant::now() - PREVIEW_TTL;
    assert!(service
        .install_review(&next.token, "config", epoch, false)
        .await
        .is_err());
    service.expire_preview();
    assert!(service.0.pending.lock().unwrap().is_none());
    service.close().await.unwrap();
}

#[tokio::test]
async fn startup_recovery_restores_cached_worker_once_without_reusing_old_authority() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (declaration, bytes) = declared_package(&root, "1.0.0", None);
    let key = settings_test_key();
    let (service, _, epoch) =
        configured_application(&root, vec![declaration.clone()], key.clone()).await;
    reconcile_bytes(&service, epoch, &bytes, &AtomicUsize::new(0)).await;
    service.close().await.unwrap();

    // Reopen the existing package inventory with a failed startup config read.
    let (service, host, old_epoch) = configured_application(&root, Vec::new(), key).await;
    service.session_configured(false, Err("Configuration is unavailable".into()));
    let config_gate = Mutex::new(());
    let gate = Mutex::new(());
    let exiting = AtomicBool::new(false);
    let epoch = super::super::bridge::recover_session(
        &host,
        &service,
        &config_gate,
        &gate,
        &exiting,
        || Ok((vec![declaration], ())),
    )
    .unwrap();
    assert!(service.snapshot(old_epoch).await.is_err());
    assert!(service.begin_selection("config", old_epoch).is_err());
    service.restore(epoch).await.unwrap();
    let generation = wait_configured_worker(&host).await;
    assert_eq!(configured_status(&service, epoch).await["state"], "ready");

    assert!(super::super::bridge::recover_session::<()>(
        &host,
        &service,
        &config_gate,
        &gate,
        &exiting,
        || panic!("a healthy poll must not reload or schedule another restore"),
    )
    .is_none());
    assert_eq!(host.authority_epoch(), epoch);
    assert_eq!(wait_configured_worker(&host).await, generation);

    // A logout after recovery invalidates even an already queued restoration.
    service.session_configured(false, Ok(Vec::new()));
    assert!(service.restore(epoch).await.is_err());
    assert!(!host.is_authorized_epoch(epoch));
    service.close().await.unwrap();
}

#[tokio::test]
async fn authenticated_restart_restores_enabled_only_and_shutdown_releases_lease() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (service, host, epoch) = application(&root).await;
    install(&service, archive(&root, "1.0.0"), epoch, true).await;
    service.session_changed(false);
    assert!(host
        .snapshots()
        .iter()
        .all(|worker| worker.contributions.is_empty()));
    let next = service.session_changed(true).unwrap();
    service.restore(next).await.unwrap();
    assert_eq!(
        service.snapshot(next).await.unwrap().plugins[0]
            .runtime
            .as_ref()
            .unwrap()
            .state,
        WorkerState::Running
    );
    service.set_enabled(PLUGIN, false, next).await.unwrap();
    service.close().await.unwrap();
    let (reopened, second_host, second_epoch) = application(&root).await;
    reopened.restore(second_epoch).await.unwrap();
    assert!(second_host.snapshots().is_empty());
    assert!(!reopened.snapshot(second_epoch).await.unwrap().plugins[0].enabled);
    reopened.uninstall(PLUGIN, second_epoch).await.unwrap();
    let (left, right) = tokio::join!(reopened.close(), reopened.close());
    left.unwrap();
    right.unwrap();
}

#[tokio::test]
async fn package_mutations_invalidate_review_and_refresh_metadata_without_core_config() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let legacy = root.join("legacy-settings.json");
    fs::write(
        &legacy,
        br#"{"ha_url":"https://example.invalid","camera_enabled":true}"#,
    )
    .unwrap();
    let before = fs::read(&legacy).unwrap();
    let (service, _, epoch) = application(&root).await;
    service.restore(epoch).await.unwrap();
    assert!(service.snapshot(epoch).await.unwrap().plugins.is_empty());
    install(&service, archive(&root, "1.0.0"), epoch, false).await;
    assert_eq!(
        service.snapshot(epoch).await.unwrap().plugins[0].version,
        "1.0.0"
    );
    let preview = review(&service, archive(&root, "2.0.0"), epoch).await;
    service.set_enabled(PLUGIN, true, epoch).await.unwrap();
    assert!(service
        .install_review(&preview.token, "config", epoch, true)
        .await
        .is_err());
    install(&service, archive(&root, "2.0.0"), epoch, true).await;
    assert_eq!(
        service.snapshot(epoch).await.unwrap().plugins[0]
            .rollback_version
            .as_deref(),
        Some("1.0.0")
    );
    service.rollback(PLUGIN, epoch).await.unwrap();
    assert_eq!(
        service.snapshot(epoch).await.unwrap().plugins[0].version,
        "1.0.0"
    );
    service.uninstall(PLUGIN, epoch).await.unwrap();
    assert!(service.snapshot(epoch).await.unwrap().plugins.is_empty());
    assert_eq!(fs::read(legacy).unwrap(), before);
    service.close().await.unwrap();
}

#[tokio::test]
async fn initial_auth_restore_and_quit_wait_for_store_initialization() {
    let directory = tempfile::tempdir().unwrap();
    let host = PluginHost::default();
    let service = PackageApplication::new(
        host.clone(),
        env!("INVERTER_DESKTOP_TARGET").into(),
        false,
        Arc::new(|| {}),
    );
    let epoch = service.session_changed(true).unwrap();
    let restore = tokio::spawn({
        let service = service.clone();
        async move { service.restore(epoch).await }
    });
    let close = tokio::spawn({
        let service = service.clone();
        async move { service.close().await }
    });
    tokio::task::yield_now().await;
    assert!(!close.is_finished());
    service
        .initialize(
            Ok(directory.path().join("store")),
            Ok(TrustStore::default()),
        )
        .await;
    close.await.unwrap().unwrap();
    assert!(restore.await.unwrap().is_err());
    assert!(host.snapshots().is_empty());
}

#[tokio::test]
async fn empty_policy_and_store_failure_are_visible_without_installation_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let host = PluginHost::default();
    let service = PackageApplication::new(
        host,
        env!("INVERTER_DESKTOP_TARGET").into(),
        false,
        Arc::new(|| {}),
    );
    service
        .initialize(
            Ok(directory.path().join("store")),
            Ok(TrustStore::default()),
        )
        .await;
    let epoch = service.session_changed(true).unwrap();
    let snapshot = service.snapshot(epoch).await.unwrap();
    assert!(snapshot.ready);
    assert!(!snapshot.installation_available);
    assert!(service.begin_selection("config", epoch).is_err());
    service.close().await.unwrap();
    let failed = PackageApplication::new(
        PluginHost::default(),
        env!("INVERTER_DESKTOP_TARGET").into(),
        false,
        Arc::new(|| {}),
    );
    failed
        .initialize(
            Err("Store location unavailable".into()),
            Ok(TrustStore::default()),
        )
        .await;
    let epoch = failed.session_changed(true).unwrap();
    let snapshot = failed.snapshot(epoch).await.unwrap();
    assert!(!snapshot.ready);
    assert_eq!(
        snapshot.error.as_deref(),
        Some("Store location unavailable")
    );
    failed.close().await.unwrap();
}

fn settings_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"properties":{
        "endpoint":{"type":"string","default":"https://example.invalid"},
        "token":{"type":"string","writeOnly":true,"minLength":1}
    },"required":["token"]})
}

fn settings_test_key() -> SettingsKeyProvider {
    let key = rand::random::<[u8; 32]>();
    Arc::new(move || Ok(key.to_vec()))
}

async fn settings_application(
    directory: &Path,
    key: SettingsKeyProvider,
) -> (PackageApplication, PluginHost, u64) {
    let host = PluginHost::default();
    let service = PackageApplication::new(
        host.clone(),
        env!("INVERTER_DESKTOP_TARGET").into(),
        true,
        Arc::new(|| {}),
    );
    service
        .initialize_with_key(Ok(directory.join("store")), Ok(trust()), key)
        .await;
    let epoch = service.session_changed(true).unwrap();
    (service, host, epoch)
}

async fn settings_view(service: &PackageApplication, epoch: u64) -> Value {
    serde_json::to_value(service.get_settings(PLUGIN, epoch).await.unwrap()).unwrap()
}

async fn save_fixture_settings(
    service: &PackageApplication,
    epoch: u64,
    revision: &Value,
) -> SettingsSaveResult {
    service
        .save_settings(
            PLUGIN,
            epoch,
            revision.as_str().unwrap().into(),
            BTreeMap::new(),
            BTreeMap::from([("token".into(), Some("fixture-secret".into()))]),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn settings_save_keeps_disabled_workers_stopped_and_restarts_enabled_workers() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (service, host, epoch) = settings_application(&root, settings_test_key()).await;
    install(
        &service,
        configured_archive(&root, "1.0.0", Some(settings_schema()), "configuration"),
        epoch,
        false,
    )
    .await;
    let initial = settings_view(&service, epoch).await;
    assert_eq!(initial["secret_present"]["token"], false);
    let saved = save_fixture_settings(&service, epoch, &initial["revision"]).await;
    assert!(saved.restart_error.is_none());
    assert!(host.snapshots().is_empty());
    let public = serde_json::to_value(saved).unwrap();
    assert_eq!(public["settings"]["secret_present"]["token"], true);
    assert!(!public.to_string().contains("fixture-secret"));
    service.set_enabled(PLUGIN, true, epoch).await.unwrap();
    let before = host
        .action_in_epoch(
            PLUGIN,
            host.snapshots()[0].instance_id.as_deref().unwrap(),
            "echo",
            json!({}),
            Duration::from_secs(2),
            epoch,
        )
        .await
        .unwrap();
    assert_eq!(before["configuration_secret_matches"], true);
    let revision = &public["settings"]["revision"];
    let saved = service
        .save_settings(
            PLUGIN,
            epoch,
            revision.as_str().unwrap().into(),
            BTreeMap::from([("endpoint".into(), json!("https://updated.invalid"))]),
            BTreeMap::new(),
        )
        .await
        .unwrap();
    assert!(saved.restart_error.is_none());
    let after = host
        .action_in_epoch(
            PLUGIN,
            host.snapshots()[0].instance_id.as_deref().unwrap(),
            "echo",
            json!({}),
            Duration::from_secs(2),
            epoch,
        )
        .await
        .unwrap();
    assert_ne!(before["pid"], after["pid"]);
    assert_ne!(
        before["configuration_revision"],
        after["configuration_revision"]
    );
    assert_eq!(after["configuration_secret_matches"], true);
    service.session_changed(false);
    assert!(service.get_settings(PLUGIN, epoch).await.is_err());
    let next_epoch = service.session_changed(true).unwrap();
    service.restore(next_epoch).await.unwrap();
    let restored = host
        .action_in_epoch(
            PLUGIN,
            host.snapshots()[0].instance_id.as_deref().unwrap(),
            "echo",
            json!({}),
            Duration::from_secs(2),
            next_epoch,
        )
        .await
        .unwrap();
    assert_eq!(
        restored["configuration_revision"],
        after["configuration_revision"]
    );
    service.close().await.unwrap();
}

#[tokio::test]
async fn stale_settings_and_package_revisions_cannot_overwrite_current_values() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (service, _, epoch) = settings_application(&root, settings_test_key()).await;
    install(
        &service,
        configured_archive(&root, "1.0.0", Some(settings_schema()), "configuration"),
        epoch,
        false,
    )
    .await;
    let initial = settings_view(&service, epoch).await;
    save_fixture_settings(&service, epoch, &initial["revision"]).await;
    assert!(service
        .save_settings(
            PLUGIN,
            epoch,
            initial["revision"].as_str().unwrap().into(),
            BTreeMap::new(),
            BTreeMap::new()
        )
        .await
        .is_err());
    let current = settings_view(&service, epoch).await;
    install(
        &service,
        configured_archive(&root, "1.1.0", Some(settings_schema()), "configuration"),
        epoch,
        false,
    )
    .await;
    assert!(service
        .save_settings(
            PLUGIN,
            epoch,
            current["revision"].as_str().unwrap().into(),
            BTreeMap::new(),
            BTreeMap::new()
        )
        .await
        .is_err());
    let updated = settings_view(&service, epoch).await;
    assert_eq!(updated["version"], "1.1.0");
    assert_eq!(updated["secret_present"]["token"], true);
    assert_ne!(updated["revision"], current["revision"]);
    service.close().await.unwrap();
}

#[tokio::test]
async fn uninstall_retains_settings_unless_explicitly_deleted() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (service, _, epoch) = settings_application(&root, settings_test_key()).await;
    let path = configured_archive(&root, "1.0.0", Some(settings_schema()), "configuration");
    install(&service, path.clone(), epoch, false).await;
    let initial = settings_view(&service, epoch).await;
    save_fixture_settings(&service, epoch, &initial["revision"]).await;
    service
        .uninstall_with_settings(PLUGIN, false, epoch)
        .await
        .unwrap();
    assert_eq!(
        service
            .settings_store()
            .unwrap()
            .read(PLUGIN)
            .unwrap()
            .secrets["token"],
        "fixture-secret"
    );
    assert!(service.get_settings(PLUGIN, epoch).await.is_err());
    install(&service, path.clone(), epoch, false).await;
    assert_eq!(
        settings_view(&service, epoch).await["secret_present"]["token"],
        true
    );
    service
        .uninstall_with_settings(PLUGIN, true, epoch)
        .await
        .unwrap();
    assert!(service
        .settings_store()
        .unwrap()
        .read(PLUGIN)
        .unwrap()
        .secrets
        .is_empty());
    install(&service, path, epoch, false).await;
    assert_eq!(
        settings_view(&service, epoch).await["secret_present"]["token"],
        false
    );
    service.close().await.unwrap();
}

#[tokio::test]
async fn saved_settings_survive_restart_failure_and_enabled_intent_is_preserved() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let fail_at = Arc::new(AtomicUsize::new(usize::MAX));
    let key_calls = calls.clone();
    let key_failure = fail_at.clone();
    let test_key = rand::random::<[u8; 32]>();
    let (service, host, epoch) = settings_application(
        &root,
        Arc::new(move || {
            if key_calls.fetch_add(1, Ordering::SeqCst) >= key_failure.load(Ordering::SeqCst) {
                Err("provider unavailable".into())
            } else {
                Ok(test_key.to_vec())
            }
        }),
    )
    .await;
    install(
        &service,
        configured_archive(&root, "1.0.0", Some(settings_schema()), "configuration"),
        epoch,
        false,
    )
    .await;
    let initial = settings_view(&service, epoch).await;
    save_fixture_settings(&service, epoch, &initial["revision"]).await;
    service.set_enabled(PLUGIN, true, epoch).await.unwrap();
    let current = settings_view(&service, epoch).await;
    // Reading the current record and preparing ciphertext succeed. Restart's
    // credential lookup fails after the authorized rename has committed.
    fail_at.store(calls.load(Ordering::SeqCst) + 2, Ordering::SeqCst);
    let saved = service
        .save_settings(
            PLUGIN,
            epoch,
            current["revision"].as_str().unwrap().into(),
            BTreeMap::from([("endpoint".into(), json!("https://persisted.invalid"))]),
            BTreeMap::new(),
        )
        .await
        .unwrap();
    assert!(saved.restart_error.is_some());
    assert!(host.snapshots().is_empty());
    assert!(service.manager().unwrap().list().await.unwrap()[0].enabled);
    fail_at.store(usize::MAX, Ordering::SeqCst);
    assert_eq!(
        settings_view(&service, epoch).await["values"]["endpoint"],
        "https://persisted.invalid"
    );
    service.set_enabled(PLUGIN, true, epoch).await.unwrap();
    assert_eq!(host.snapshots()[0].state, WorkerState::Running);
    service.close().await.unwrap();
}

#[tokio::test]
async fn retained_inventory_is_lazy_and_bound_to_the_current_session() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let key_calls = calls.clone();
    let (service, _, epoch) = settings_application(
        &root,
        Arc::new(move || {
            key_calls.fetch_add(1, Ordering::SeqCst);
            Err("credentials are unavailable".into())
        }),
    )
    .await;
    let data = service.retained_data(epoch).await.unwrap();
    assert!(data.records.is_empty());
    assert_eq!(data.total_bytes, 0);
    assert_eq!(data.max_records, 64);
    assert_eq!(data.max_bytes, 8 * 1024 * 1024);
    let revision = service.snapshot(epoch).await.unwrap().data_revision;
    service.retained_data(epoch).await.unwrap();
    assert_eq!(
        service.snapshot(epoch).await.unwrap().data_revision,
        revision
    );
    assert!(!root.join("store/settings").exists());
    service.session_changed(false);
    assert!(service.retained_data(epoch).await.is_err());
    let next = service.session_changed(true).unwrap();
    assert!(service.retained_data(epoch).await.is_err());
    assert!(service
        .retained_data(next)
        .await
        .unwrap()
        .records
        .is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    service.close().await.unwrap();
    assert!(service.retained_data(next).await.is_err());
    assert_eq!(service.0.authorized_work.load(Ordering::Acquire), 0);
}

async fn retained_fixture(root: &Path) -> (PackageApplication, u64, PathBuf) {
    let (service, _, epoch) = settings_application(root, settings_test_key()).await;
    let package = configured_archive(root, "1.0.0", Some(settings_schema()), "configuration");
    install(&service, package.clone(), epoch, false).await;
    let initial = settings_view(&service, epoch).await;
    save_fixture_settings(&service, epoch, &initial["revision"]).await;
    (service, epoch, package)
}

#[tokio::test]
async fn retained_inventory_identifies_installed_owners_and_protects_reinstalled_data() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (service, epoch, package) = retained_fixture(&root).await;
    let installed = service.retained_data(epoch).await.unwrap();
    assert_eq!(installed.records.len(), 1);
    let record = &installed.records[0];
    assert_eq!(record.plugin_id.as_deref(), Some(PLUGIN));
    assert_eq!(record.record_id, SettingsStore::record_id(PLUGIN).unwrap());
    assert_eq!(record.bytes, installed.total_bytes);
    let public = serde_json::to_string(&installed).unwrap();
    assert!(!public.contains("fixture-secret"));
    assert!(!public.contains("endpoint"));
    assert!(service
        .delete_retained_data(&record.record_id, &record.revision, epoch)
        .await
        .is_err());
    service.uninstall(PLUGIN, epoch).await.unwrap();
    let retained = service.retained_data(epoch).await.unwrap();
    assert_eq!(retained.records.len(), 1);
    assert!(retained.records[0].plugin_id.is_none());
    assert_eq!(retained.records[0].revision, record.revision);
    install(&service, package, epoch, false).await;
    assert!(service
        .delete_retained_data(&record.record_id, &record.revision, epoch)
        .await
        .is_err());
    assert_eq!(
        settings_view(&service, epoch).await["secret_present"]["token"],
        true
    );
    service.uninstall(PLUGIN, epoch).await.unwrap();
    let revision_before_delete = service.snapshot(epoch).await.unwrap().data_revision;
    service
        .delete_retained_data(&record.record_id, &record.revision, epoch)
        .await
        .unwrap();
    let empty = service.retained_data(epoch).await.unwrap();
    assert!(empty.records.is_empty());
    assert_eq!(empty.total_bytes, 0);
    assert_ne!(
        service.snapshot(epoch).await.unwrap().data_revision,
        revision_before_delete
    );
    service.close().await.unwrap();
}

#[tokio::test]
async fn corrupt_orphan_data_can_be_removed_after_reopening_without_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let core = root.join("core-config.json");
    fs::write(&core, b"preserved core configuration").unwrap();
    let (service, epoch, _) = retained_fixture(&root).await;
    service.uninstall(PLUGIN, epoch).await.unwrap();
    let record_id = SettingsStore::record_id(PLUGIN).unwrap();
    service.close().await.unwrap();
    let record_path = root.join("store/settings").join(format!("{record_id}.enc"));
    fs::write(&record_path, b"corrupt ciphertext").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let key_calls = calls.clone();
    let (reopened, _, epoch) = settings_application(
        &root,
        Arc::new(move || {
            key_calls.fetch_add(1, Ordering::SeqCst);
            Err("credentials are unavailable".into())
        }),
    )
    .await;
    let data = reopened.retained_data(epoch).await.unwrap();
    assert_eq!(data.records.len(), 1);
    let record = &data.records[0];
    assert!(record.plugin_id.is_none());
    assert_eq!(record.bytes, b"corrupt ciphertext".len() as u64);
    reopened
        .delete_retained_data(&record.record_id, &record.revision, epoch)
        .await
        .unwrap();
    assert!(!record_path.exists());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(fs::read(core).unwrap(), b"preserved core configuration");
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn stale_retained_deletion_preserves_replacement_and_invalidates_package_consent() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (service, epoch, package) = retained_fixture(&root).await;
    service.uninstall(PLUGIN, epoch).await.unwrap();
    let data = service.retained_data(epoch).await.unwrap();
    let record = &data.records[0];
    let path = root
        .join("store/settings")
        .join(format!("{}.enc", record.record_id));
    fs::write(&path, b"changed after review").unwrap();
    let preview = review(&service, package, epoch).await;
    assert!(service
        .delete_retained_data(&record.record_id, &record.revision, epoch)
        .await
        .is_err());
    assert_eq!(fs::read(&path).unwrap(), b"changed after review");
    assert!(service
        .install_review(&preview.token, "config", epoch, false)
        .await
        .is_err());
    let refreshed = service.retained_data(epoch).await.unwrap();
    let current = &refreshed.records[0];
    assert_ne!(current.revision, record.revision);
    service.session_changed(false);
    let next = service.session_changed(true).unwrap();
    assert!(service
        .delete_retained_data(&current.record_id, &current.revision, epoch)
        .await
        .is_err());
    assert!(path.exists());
    service
        .delete_retained_data(&current.record_id, &current.revision, next)
        .await
        .unwrap();
    service.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_retained_deletion_keeps_expiry_active_and_cannot_commit_after_logout() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (service, epoch, _) = retained_fixture(&root).await;
    service.uninstall(PLUGIN, epoch).await.unwrap();
    let mut data = service.retained_data(epoch).await.unwrap();
    let record = data.records.pop().unwrap();
    let manager = service.manager().unwrap();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocker = tokio::spawn(async move {
        manager
            .read_retained_data_in_epoch(epoch, move |_| {
                let _ = started_tx.send(());
                release_rx
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(|_| "test operation was not released".to_string())
            })
            .await
    });
    started_rx.await.unwrap();
    let caller_service = service.clone();
    let caller = tokio::spawn(async move {
        caller_service
            .delete_retained_data(&record.record_id, &record.revision, epoch)
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while service.0.authorized_work.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    assert!(service.has_session_work());
    service.session_changed(false);
    release_tx.send(()).unwrap();
    assert!(blocker.await.unwrap().is_err());
    tokio::time::timeout(Duration::from_secs(2), async {
        while service.has_session_work() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let next = service.session_changed(true).unwrap();
    assert_eq!(service.retained_data(next).await.unwrap().records.len(), 1);
    service.close().await.unwrap();
}

fn grouped_declaration(
    directory: &Path,
    id: &str,
    grouped: bool,
) -> (DesktopPluginConfig, Vec<u8>) {
    grouped_declaration_with_settings(directory, id, grouped, None)
}

fn grouped_declaration_with_settings(
    directory: &Path,
    id: &str,
    grouped: bool,
    schema: Option<Value>,
) -> (DesktopPluginConfig, Vec<u8>) {
    let source = directory.join(format!("group-payload-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&source).unwrap();
    let mode = if schema.is_some() {
        "configuration"
    } else {
        "worker"
    };
    let entrypoint = format!("{mode}{}", std::env::consts::EXE_SUFFIX);
    let mut permissions = vec![PluginPermission::DashboardContributions];
    if schema.is_some() {
        permissions.push(PluginPermission::PluginConfiguration);
    }
    fs::copy(fixture(), source.join(&entrypoint)).unwrap();
    let bytes = build_pinned_package(
        PluginManifest {
            schema_version: 1,
            plugin_id: id.into(),
            version: "1.0.0".into(),
            host_api: "^1.8".into(),
            target: env!("INVERTER_DESKTOP_TARGET").into(),
            entrypoint,
            permissions,
            config_schema: schema.unwrap_or_else(|| json!({"type":"object"})),
            group: grouped.then(|| crate::plugins::protocol::PluginGroup {
                id: "cameras".into(),
                title: "Cameras".into(),
                icon: "camera".into(),
            }),
            live_view: None,
            http_video: None,
            inventory: Vec::new(),
            signature: None,
        },
        &source,
    )
    .unwrap();
    let declaration = DesktopPluginConfig {
        plugin_id: id.into(),
        version: "1.0.0".into(),
        enabled: true,
        artifacts: BTreeMap::from([(
            env!("INVERTER_DESKTOP_TARGET").into(),
            DesktopPluginArtifact {
                url: format!("https://plugins.example.invalid/{id}.idplugin"),
                sha256: sha256_hex(&bytes),
                extra: BTreeMap::new(),
            },
        )]),
        extra: BTreeMap::new(),
    };
    (declaration, bytes)
}

async fn install_group_fixture(
    service: &PackageApplication,
    epoch: u64,
    declaration: &DesktopPluginConfig,
    bytes: &[u8],
) {
    let package = crate::plugins::package::verify_pinned_archive_bytes(
        bytes.to_vec(),
        &declaration.plugin_id,
        &declaration.version,
        env!("INVERTER_DESKTOP_TARGET"),
        &sha256_hex(bytes),
    )
    .unwrap();
    service
        .manager()
        .unwrap()
        .install_verified_in_epoch(package, true, epoch)
        .await
        .unwrap();
    service.0.metadata_revision.fetch_add(1, Ordering::AcqRel);
}

#[tokio::test]
async fn group_toggle_persists_pinned_intent_and_keeps_unrelated_worker_instance() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let first = grouped_declaration(&root, "test.camera-one", true);
    let second = grouped_declaration(&root, "test.camera-two", true);
    let unrelated = grouped_declaration(&root, "test.other", false);
    let declarations = vec![first.0.clone(), second.0.clone(), unrelated.0.clone()];
    let (service, host, epoch) =
        configured_application(&root, declarations.clone(), settings_test_key()).await;
    for (declaration, bytes) in [&first, &second, &unrelated] {
        install_group_fixture(&service, epoch, declaration, bytes).await;
    }
    let original = host
        .snapshots()
        .into_iter()
        .find(|worker| worker.plugin_id == unrelated.0.plugin_id)
        .unwrap()
        .instance_id;
    let groups = service.groups(epoch).await.unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(
        groups[0].plugin_ids,
        [first.0.plugin_id.clone(), second.0.plugin_id.clone()]
    );
    assert!(groups[0].enabled);
    let persisted = Arc::new(Mutex::new(declarations));
    for enabled in [false, true] {
        let saved = persisted.clone();
        let authority = host.clone();
        service
            .set_group_enabled(
                "cameras".into(),
                enabled,
                epoch,
                move |members, packages| {
                    authority.commit_in_epoch(epoch, || {
                        for declaration in saved.lock().unwrap().iter_mut() {
                            if members.contains(&declaration.plugin_id) {
                                declaration.enabled = enabled;
                            }
                        }
                        packages.group_desired_changed(members, enabled);
                        Ok(())
                    })
                },
            )
            .await
            .unwrap();
        // Startup reconciliation must use the new desired state without a download.
        service
            .reconcile_with(service.manager().unwrap(), epoch, |_| async {
                panic!("cached group toggle must not download")
            })
            .await
            .unwrap();
        assert_eq!(service.groups(epoch).await.unwrap()[0].enabled, enabled);
        assert_eq!(host.authority_epoch(), epoch);
        assert_eq!(
            host.snapshots()
                .into_iter()
                .find(|worker| worker.plugin_id == unrelated.0.plugin_id)
                .unwrap()
                .instance_id,
            original
        );
        for record in service.manager().unwrap().list().await.unwrap() {
            assert_eq!(
                record.enabled,
                if record.plugin_id == unrelated.0.plugin_id {
                    true
                } else {
                    enabled
                }
            );
        }
        assert!(persisted
            .lock()
            .unwrap()
            .iter()
            .all(|entry| entry.enabled == (enabled || entry.plugin_id == unrelated.0.plugin_id)));
    }
    service.close().await.unwrap();
}

#[tokio::test]
async fn group_activation_failure_reports_supported_failed_status_and_keeps_other_worker() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let camera =
        grouped_declaration_with_settings(&root, "test.camera", true, Some(settings_schema()));
    let unrelated = grouped_declaration(&root, "test.other", false);
    let declarations = vec![camera.0.clone(), unrelated.0.clone()];
    let (service, host, epoch) =
        configured_application(&root, declarations.clone(), settings_test_key()).await;
    let package = crate::plugins::package::verify_pinned_archive_bytes(
        camera.1.clone(),
        &camera.0.plugin_id,
        &camera.0.version,
        env!("INVERTER_DESKTOP_TARGET"),
        &sha256_hex(&camera.1),
    )
    .unwrap();
    service
        .manager()
        .unwrap()
        .install_verified_in_epoch(package, false, epoch)
        .await
        .unwrap();
    install_group_fixture(&service, epoch, &unrelated.0, &unrelated.1).await;
    let original = host.snapshots()[0].instance_id.clone();
    let persisted = Arc::new(Mutex::new(declarations));
    let saved = persisted.clone();
    let authority = host.clone();
    let error = service
        .set_group_enabled("cameras".into(), true, epoch, move |members, packages| {
            authority.commit_in_epoch(epoch, || {
                for declaration in saved.lock().unwrap().iter_mut() {
                    if members.contains(&declaration.plugin_id) {
                        declaration.enabled = true;
                    }
                }
                packages.group_desired_changed(members, true);
                Ok(())
            })
        })
        .await
        .unwrap_err();
    let snapshot = serde_json::to_value(service.snapshot(epoch).await.unwrap()).unwrap();
    let status = snapshot["configured"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["plugin_id"] == camera.0.plugin_id)
        .unwrap();
    assert_eq!(status["state"], "failed");
    assert_eq!(status["error"], error);
    assert_eq!(status["enabled"], true);
    assert_eq!(persisted.lock().unwrap()[1], unrelated.0);
    let records = service.manager().unwrap().list().await.unwrap();
    assert!(
        !records
            .iter()
            .find(|entry| entry.plugin_id == camera.0.plugin_id)
            .unwrap()
            .enabled
    );
    assert_eq!(host.snapshots().len(), 1);
    assert_eq!(host.snapshots()[0].plugin_id, unrelated.0.plugin_id);
    assert_eq!(host.snapshots()[0].instance_id, original);
    service.close().await.unwrap();
}

#[tokio::test]
async fn group_failed_persistence_and_unknown_group_never_change_inventory() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let package = grouped_declaration(&root, "test.camera", true);
    let (service, host, epoch) =
        configured_application(&root, vec![package.0.clone()], settings_test_key()).await;
    install_group_fixture(&service, epoch, &package.0, &package.1).await;
    let original = host.snapshots()[0].instance_id.clone();
    assert!(service
        .set_group_enabled("missing".into(), false, epoch, |_, _| panic!(
            "unverified group must not persist"
        ))
        .await
        .is_err());
    assert_eq!(
        service
            .set_group_enabled("cameras".into(), false, epoch, |_, _| Err(
                "save failed".into()
            ))
            .await
            .unwrap_err(),
        "save failed"
    );
    assert!(service.manager().unwrap().list().await.unwrap()[0].enabled);
    assert_eq!(host.snapshots()[0].instance_id, original);
    service.close().await.unwrap();
}

#[tokio::test]
async fn group_owned_toggle_survives_caller_cancellation_and_revoked_waiter_cannot_persist() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let package = grouped_declaration(&root, "test.camera", true);
    let (service, host, epoch) =
        configured_application(&root, vec![package.0.clone()], settings_test_key()).await;
    install_group_fixture(&service, epoch, &package.0, &package.1).await;
    for revoke in [false, true] {
        let gate = service.0.reconciliation.operation.lock().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let operation = service.clone();
        let authority = host.clone();
        let task = tokio::spawn(async move {
            operation
                .set_group_enabled("cameras".into(), false, epoch, move |members, packages| {
                    authority.commit_in_epoch(epoch, || {
                        counted.fetch_add(1, Ordering::SeqCst);
                        packages.group_desired_changed(members, false);
                        Ok(())
                    })
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            while !service.has_session_work() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if revoke {
            service.session_configured(false, Ok(vec![package.0.clone()]));
        }
        task.abort();
        drop(gate);
        tokio::time::timeout(Duration::from_secs(5), async {
            while service.has_session_work() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), usize::from(!revoke));
        assert!(!service.manager().unwrap().list().await.unwrap()[0].enabled);
    }
    service.close().await.unwrap();
}

#[path = "application_migration_tests.rs"]
mod migration_tests;

#[path = "application_management_tests.rs"]
mod management_tests;

#[path = "application_configured_removal_tests.rs"]
mod configured_removal_tests;
