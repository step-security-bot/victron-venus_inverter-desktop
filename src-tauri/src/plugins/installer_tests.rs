use super::super::package::{canonical_manifest_bytes, sha256_hex, PublisherTrust};
use super::super::packaging::build_package;
use super::super::protocol::{PluginManifest, PluginPermission};
use super::*;
use ed25519_dalek::SigningKey;
use serde_json::json;
use std::collections::HashMap;
use std::sync::OnceLock;

const PLUGIN: &str = "test.installer";
const KEY_ID: &str = "installer-fixture-only";

struct TestDirectory(PathBuf);
impl TestDirectory {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("inverter-package-test-{}", uuid::Uuid::new_v4()));
        create_private_directory(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }
}
impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = remove_owned_tree(&self.0);
    }
}

fn host_target() -> String {
    let arch = std::env::consts::ARCH;
    let suffix = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu",
        "windows" => "pc-windows-msvc",
        other => panic!("unsupported fixture host: {other}"),
    };
    format!("{arch}-{suffix}")
}

fn key() -> SigningKey {
    SigningKey::from_bytes(&[97_u8; 32])
}

fn trust() -> TrustStore {
    trust_for(&[PLUGIN])
}

fn trust_for(ids: &[&str]) -> TrustStore {
    TrustStore::new(vec![PublisherTrust::new(
        KEY_ID.into(),
        key().verifying_key().to_bytes(),
        ids.iter().map(|id| (*id).into()).collect(),
    )
    .unwrap()])
    .unwrap()
}

fn fixture(mode: &str) -> PathBuf {
    static FIXTURES: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();
    let mut fixtures = FIXTURES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    if let Some(path) = fixtures.get(mode) {
        return path.clone();
    }
    let directory = std::env::temp_dir().join(format!(
        "inverter-installer-fixture-{}-{mode}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    let executable = directory.join(format!("worker{}", std::env::consts::EXE_SUFFIX));
    let manifest_source =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugin_worker.rs");
    let source_path = if manifest_source.exists() {
        manifest_source
    } else {
        Path::new(file!())
            .ancestors()
            .nth(3)
            .unwrap()
            .join("tests/fixtures/plugin_worker.rs")
    };
    let source = fs::read_to_string(source_path).unwrap().replacen(
        "\"normal\".into()",
        &format!("\"{mode}\".into()"),
        1,
    );
    let source_file = directory.join("worker.rs");
    fs::write(&source_file, source).unwrap();
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let result = std::process::Command::new(rustc)
        .arg("--edition=2021")
        .arg(source_file)
        .arg("-o")
        .arg(&executable)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    fixtures.insert(mode.into(), executable.clone());
    executable
}

fn package(directory: &Path, version: &str, mode: &str) -> PathBuf {
    package_for(directory, PLUGIN, version, mode)
}

fn package_for(directory: &Path, id: &str, version: &str, mode: &str) -> PathBuf {
    let payload = directory.join(format!("source-{}", uuid::Uuid::new_v4()));
    create_private_directory(&payload).unwrap();
    let entrypoint = format!("worker{}", std::env::consts::EXE_SUFFIX);
    fs::copy(fixture(mode), payload.join(&entrypoint)).unwrap();
    let mut manifest = PluginManifest {
        group: None,
        live_view: None,
        schema_version: 1,
        plugin_id: id.into(),
        version: version.into(),
        host_api: "^1.0".into(),
        target: host_target(),
        entrypoint,
        config_schema: json!({"type":"object","properties":{}}),
        permissions: vec![PluginPermission::DashboardContributions],
        http_video: None,
        inventory: Vec::new(),
        signature: None,
    };
    if mode.starts_with("configuration") {
        manifest
            .permissions
            .push(PluginPermission::PluginConfiguration);
        manifest.config_schema = json!({"type":"object","properties":{
            "server":{"type":"string"}, "token":{"type":"string","writeOnly":true}
        }});
    }
    if mode == "configuration_video_authorized" {
        manifest.permissions.push(PluginPermission::HttpVideo);
        manifest.http_video = Some(super::super::protocol::HttpVideoDeclaration {
            live_preview: None,
            allow_query: false,
            bearer_token_setting: None,
            cooldown_seconds: None,
            base_url_setting: "server".into(),
        });
    }
    if mode == "notifications_authorized" {
        manifest
            .permissions
            .push(PluginPermission::DesktopNotifications);
    }
    let bytes = build_package(manifest, &payload, KEY_ID, &key()).unwrap();
    let archive = directory.join(format!("archive-{}.idplugin", uuid::Uuid::new_v4()));
    fs::write(&archive, bytes).unwrap();
    archive
}

fn pinned_package(directory: &Path, version: &str, mode: &str) -> VerifiedPackage {
    pinned_package_revision(directory, version, mode, None)
}

fn pinned_package_revision(
    directory: &Path,
    version: &str,
    mode: &str,
    revision: Option<&str>,
) -> VerifiedPackage {
    let original = fs::read(package(directory, version, mode)).unwrap();
    let signed = verify_archive_bytes(original, &trust(), &host_target()).unwrap();
    let mut manifest = signed.manifest().clone();
    manifest.signature = None;
    if let Some(revision) = revision {
        manifest.config_schema["description"] = json!(revision);
    }
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o755);
    writer.start_file("manifest.json", options).unwrap();
    writer
        .write_all(&canonical_manifest_bytes(&manifest).unwrap())
        .unwrap();
    for (path, bytes) in signed.files() {
        writer.start_file(path, options).unwrap();
        writer.write_all(bytes).unwrap();
    }
    let bytes = writer.finish().unwrap().into_inner();
    let digest = sha256_hex(&bytes);
    verify_pinned_archive_bytes(bytes, PLUGIN, version, &host_target(), &digest).unwrap()
}

async fn manager(directory: &TestDirectory) -> (PackageManager, PluginHost) {
    let host = PluginHost::default();
    let manager = PackageManager::open(
        directory.0.join("store"),
        host_target(),
        trust(),
        host.clone(),
    )
    .await
    .unwrap();
    (manager, host)
}

async fn wait_running(host: &PluginHost) {
    time::timeout(Duration::from_secs(5), async {
        loop {
            if host.snapshots().iter().any(|snapshot| {
                snapshot.plugin_id == PLUGIN
                    && snapshot.state == WorkerState::Running
                    && !snapshot.contributions.is_empty()
            }) {
                break;
            }
            time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

fn worker_configuration(revision: &str) -> WorkerConfiguration {
    WorkerConfiguration {
        revision: revision.into(),
        values: json!({"server":"https://plugin.example"}),
        secrets: [("token".into(), "fixture-secret".into())].into(),
    }
}

#[tokio::test]
async fn notifications_require_the_freshly_verified_package_permission() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    manager
        .install(
            package(&directory.0, "1.0.0", "notifications_actions"),
            true,
        )
        .await
        .unwrap();
    wait_running(&host).await;
    assert!(host
        .action(PLUGIN, "echo", json!({}), Duration::from_secs(2))
        .await
        .is_err());
    time::timeout(Duration::from_secs(5), async {
        while !host
            .snapshots()
            .iter()
            .any(|snapshot| snapshot.state == WorkerState::Failed)
        {
            time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        host.snapshots()[0].last_error.as_deref(),
        Some("worker_notification_unauthorized")
    );
    assert_eq!(
        host.dispatch_notifications(|_| panic!("ungranted package notification")),
        0
    );
    manager.disable(PLUGIN).await.unwrap();
    manager
        .install(
            package(&directory.0, "1.1.0", "notifications_authorized"),
            true,
        )
        .await
        .unwrap();
    wait_running(&host).await;
    host.action(PLUGIN, "echo", json!({}), Duration::from_secs(2))
        .await
        .unwrap();
    let mut delivered = Vec::new();
    assert_eq!(
        host.dispatch_notifications(|item| delivered.push(item.plugin_id.clone())),
        16
    );
    assert!(delivered.iter().all(|id| id == PLUGIN));
    host.action(PLUGIN, "echo", json!({}), Duration::from_secs(2))
        .await
        .unwrap();
    manager.remove(PLUGIN).await.unwrap();
    assert_eq!(
        host.dispatch_notifications(|_| panic!("uninstalled package notification")),
        0
    );
    manager.close().await.unwrap();
}

// Poll the public operation once so its owned task exists, then let that task
// reach the held operation lock on these tests' single-thread Tokio runtime.
async fn queue_operation<F: std::future::Future>(mut operation: std::pin::Pin<&mut F>) {
    std::future::poll_fn(|context| {
        assert!(operation.as_mut().poll(context).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    tokio::task::yield_now().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coherent_settings_snapshot_consumer_excludes_concurrent_settings_writes() {
    use std::sync::atomic::AtomicUsize;
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    manager
        .install(package(&directory.0, "1.0.0", "configuration"), false)
        .await
        .unwrap();
    let epoch = host.authority_epoch();
    let value = Arc::new(AtomicUsize::new(1));
    let (entered, observed) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let snapshot_value = value.clone();
    let mut snapshot = Box::pin(manager.read_all_settings_in_epoch(epoch, move |packages| {
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].0.plugin_id, PLUGIN);
        let before = snapshot_value.load(Ordering::SeqCst);
        let _ = entered.send(before);
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            snapshot_value.load(Ordering::SeqCst),
            before,
            "the restore consumer must retain its settings authority through commit"
        );
        Ok(before)
    }));
    queue_operation(snapshot.as_mut()).await;
    assert_eq!(observed.await.unwrap(), 1);
    let (prepared, mut write_started) = tokio::sync::oneshot::channel();
    let write_value = value.clone();
    let mut write = Box::pin(manager.apply_settings_in_epoch(PLUGIN, epoch, move |_, _| {
        let _ = prepared.send(());
        Ok((
            (),
            Box::new(move || {
                write_value.store(2, Ordering::SeqCst);
                Ok(())
            }) as SettingsCommit,
        ))
    }));
    queue_operation(write.as_mut()).await;
    assert!(
        time::timeout(Duration::from_millis(100), &mut write_started)
            .await
            .is_err()
    );
    release.send(()).unwrap();
    assert_eq!(snapshot.await.unwrap(), 1);
    write.await.unwrap();
    write_started.await.unwrap();
    assert_eq!(value.load(Ordering::SeqCst), 2);
    manager.close().await.unwrap();
}

#[tokio::test]
async fn queued_coherent_settings_snapshot_cannot_cross_authentication_epoch() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    let epoch = host.authority_epoch();
    let lock = manager.0.operation.lock().await;
    let (called, observed) = tokio::sync::oneshot::channel();
    let mut snapshot = Box::pin(manager.read_all_settings_in_epoch(epoch, move |_| {
        let _ = called.send(());
        Ok(())
    }));
    queue_operation(snapshot.as_mut()).await;
    host.revoke();
    host.resume();
    drop(lock);
    assert!(snapshot.await.is_err());
    assert!(observed.await.is_err());
    manager.close().await.unwrap();
}

#[tokio::test]
async fn retained_data_protects_running_disabled_and_broken_installed_packages() {
    const DISABLED: &str = "test.disabled";
    const BROKEN: &str = "test.broken";
    let directory = TestDirectory::new();
    let host = PluginHost::default();
    let manager = PackageManager::open(
        directory.0.join("store"),
        host_target(),
        trust_for(&[PLUGIN, DISABLED, BROKEN]),
        host.clone(),
    )
    .await
    .unwrap();
    manager
        .install(package(&directory.0, "1.0.0", "normal"), true)
        .await
        .unwrap();
    manager
        .install(
            package_for(&directory.0, DISABLED, "1.0.0", "normal"),
            false,
        )
        .await
        .unwrap();
    let broken = manager
        .install(package_for(&directory.0, BROKEN, "1.0.0", "normal"), false)
        .await
        .unwrap();
    let archive = manager
        .version_path(BROKEN, &broken.active)
        .join("archive.idplugin");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&archive, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::write(archive, b"broken installed archive").unwrap();
    assert!(manager.verify_installed(BROKEN, &broken.active).is_err());
    wait_running(&host).await;
    let generation = host.snapshots()[0].generation;
    let before = fs::read(manager.0.root.join("state.json")).unwrap();
    let epoch = host.authority_epoch();
    let ids = manager
        .read_retained_data_in_epoch(epoch, |ids| Ok(ids.clone()))
        .await
        .unwrap();
    assert_eq!(
        ids,
        BTreeSet::from([PLUGIN.to_owned(), DISABLED.to_owned(), BROKEN.to_owned()])
    );
    let cleaned = Arc::new(AtomicBool::new(false));
    for id in &ids {
        let cleaned = cleaned.clone();
        let error = manager
            .remove_retained_data_in_epoch(
                &SettingsStore::record_id(id).unwrap(),
                epoch,
                Box::new(move || {
                    cleaned.store(true, Ordering::SeqCst);
                    Ok(())
                }),
            )
            .await
            .unwrap_err();
        assert_eq!(error, "plugin data belongs to an installed package");
    }
    assert!(!cleaned.load(Ordering::SeqCst));
    let flag = cleaned.clone();
    manager
        .remove_retained_data_in_epoch(
            &SettingsStore::record_id("test.uninstalled").unwrap(),
            epoch,
            Box::new(move || {
                flag.store(true, Ordering::SeqCst);
                Ok(())
            }),
        )
        .await
        .unwrap();
    assert!(cleaned.load(Ordering::SeqCst));
    assert_eq!(fs::read(manager.0.root.join("state.json")).unwrap(), before);
    assert_eq!(host.snapshots().len(), 1);
    assert_eq!(host.snapshots()[0].generation, generation);
    assert_eq!(
        host.action(PLUGIN, "echo", json!({}), Duration::from_secs(2))
            .await
            .unwrap()["ok"],
        true
    );
    manager.close().await.unwrap();
}

#[tokio::test]
async fn retained_data_serializes_with_installation_and_uninstall() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    let archive = package(&directory.0, "1.0.0", "normal");
    let epoch = host.authority_epoch();
    let record_id = SettingsStore::record_id(PLUGIN).unwrap();
    let cleaned = Arc::new(AtomicBool::new(false));
    let lock = manager.0.operation.lock().await;
    let mut install = Box::pin(manager.install(archive, false));
    queue_operation(install.as_mut()).await;
    let mut read = Box::pin(manager.read_retained_data_in_epoch(epoch, |ids| Ok(ids.clone())));
    queue_operation(read.as_mut()).await;
    let flag = cleaned.clone();
    let mut cleanup = Box::pin(manager.remove_retained_data_in_epoch(
        &record_id,
        epoch,
        Box::new(move || {
            flag.store(true, Ordering::SeqCst);
            Ok(())
        }),
    ));
    queue_operation(cleanup.as_mut()).await;
    drop(lock);
    install.await.unwrap();
    assert_eq!(read.await.unwrap(), BTreeSet::from([PLUGIN.to_owned()]));
    assert_eq!(
        cleanup.await.unwrap_err(),
        "plugin data belongs to an installed package"
    );
    assert!(!cleaned.load(Ordering::SeqCst));

    let lock = manager.0.operation.lock().await;
    let mut uninstall = Box::pin(manager.remove_in_epoch(PLUGIN, epoch));
    queue_operation(uninstall.as_mut()).await;
    let mut read = Box::pin(manager.read_retained_data_in_epoch(epoch, |ids| Ok(ids.clone())));
    queue_operation(read.as_mut()).await;
    let flag = cleaned.clone();
    let mut cleanup = Box::pin(manager.remove_retained_data_in_epoch(
        &record_id,
        epoch,
        Box::new(move || {
            flag.store(true, Ordering::SeqCst);
            Ok(())
        }),
    ));
    queue_operation(cleanup.as_mut()).await;
    drop(lock);
    uninstall.await.unwrap();
    assert!(read.await.unwrap().is_empty());
    cleanup.await.unwrap();
    assert!(cleaned.load(Ordering::SeqCst));
    assert!(manager.list().await.unwrap().is_empty());
    assert!(host.snapshots().is_empty());
    manager.close().await.unwrap();
}

#[tokio::test]
async fn retained_data_rejects_stale_queued_operations_and_revoked_read_results() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    let epoch = host.authority_epoch();
    let record_id = SettingsStore::record_id(PLUGIN).unwrap();
    let lock = manager.0.operation.lock().await;
    let (read_sent, read_called) = tokio::sync::oneshot::channel();
    let mut read = Box::pin(manager.read_retained_data_in_epoch(epoch, move |_| {
        let _ = read_sent.send(());
        Ok(())
    }));
    queue_operation(read.as_mut()).await;
    let (cleanup_sent, cleanup_called) = tokio::sync::oneshot::channel();
    let mut cleanup = Box::pin(manager.remove_retained_data_in_epoch(
        &record_id,
        epoch,
        Box::new(move || {
            let _ = cleanup_sent.send(());
            Ok(())
        }),
    ));
    queue_operation(cleanup.as_mut()).await;
    host.revoke();
    host.resume();
    drop(lock);
    assert!(read.await.unwrap_err().contains("authorization"));
    assert!(cleanup.await.unwrap_err().contains("authorization"));
    assert!(read_called.await.is_err());
    assert!(cleanup_called.await.is_err());

    let revoker = host.clone();
    let error = manager
        .read_retained_data_in_epoch(host.authority_epoch(), move |_| {
            revoker.revoke();
            revoker.resume();
            Ok("result from an expired session")
        })
        .await
        .unwrap_err();
    assert!(error.contains("authorization"));
    manager.close().await.unwrap();
}

#[tokio::test]
async fn retained_data_owned_operations_survive_caller_cancellation_but_not_revocation() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    let record_id = SettingsStore::record_id(PLUGIN).unwrap();
    for revoke in [false, true] {
        let epoch = host.authority_epoch();
        let lock = manager.0.operation.lock().await;
        let (read_sent, read_called) = tokio::sync::oneshot::channel();
        let mut read = Box::pin(manager.read_retained_data_in_epoch(epoch, move |_| {
            let _ = read_sent.send(());
            Ok(())
        }));
        queue_operation(read.as_mut()).await;
        let (cleanup_sent, cleanup_called) = tokio::sync::oneshot::channel();
        let mut cleanup = Box::pin(manager.remove_retained_data_in_epoch(
            &record_id,
            epoch,
            Box::new(move || {
                let _ = cleanup_sent.send(());
                Ok(())
            }),
        ));
        queue_operation(cleanup.as_mut()).await;
        drop(read);
        drop(cleanup);
        if revoke {
            host.revoke();
            host.resume();
        }
        drop(lock);
        let read = time::timeout(Duration::from_secs(2), read_called)
            .await
            .unwrap();
        let cleanup = time::timeout(Duration::from_secs(2), cleanup_called)
            .await
            .unwrap();
        assert_eq!(read.is_err(), revoke);
        assert_eq!(cleanup.is_err(), revoke);
    }
    assert!(manager.list().await.unwrap().is_empty());
    manager.close().await.unwrap();
}

#[tokio::test]
async fn retained_data_rejects_noncanonical_ids_and_preserves_cleanup_errors() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    let epoch = host.authority_epoch();
    for id in [
        String::new(),
        "A".repeat(64),
        "g".repeat(64),
        "a".repeat(63),
        "a".repeat(65),
        format!("{}.enc", "a".repeat(64)),
        format!("../{}", "a".repeat(64)),
    ] {
        let (sent, called) = tokio::sync::oneshot::channel();
        let error = manager
            .remove_retained_data_in_epoch(
                &id,
                epoch,
                Box::new(move || {
                    let _ = sent.send(());
                    Ok(())
                }),
            )
            .await
            .unwrap_err();
        assert_eq!(error, "invalid retained plugin data identity");
        assert!(called.await.is_err());
    }
    let record_id = SettingsStore::record_id(PLUGIN).unwrap();
    assert_eq!(
        manager
            .remove_retained_data_in_epoch(
                &record_id,
                epoch,
                Box::new(|| Err("reviewed ciphertext changed".into())),
            )
            .await
            .unwrap_err(),
        "reviewed ciphertext changed"
    );
    manager.close().await.unwrap();
    assert_eq!(
        manager
            .read_retained_data_in_epoch(epoch, |_| Ok(()))
            .await
            .unwrap_err(),
        "package store is closed"
    );
    let (sent, called) = tokio::sync::oneshot::channel();
    assert_eq!(
        manager
            .remove_retained_data_in_epoch(
                &record_id,
                epoch,
                Box::new(move || {
                    let _ = sent.send(());
                    Ok(())
                }),
            )
            .await
            .unwrap_err(),
        "package store is closed"
    );
    assert!(called.await.is_err());
}

#[tokio::test]
async fn settings_are_scoped_to_verified_permission_and_restart_only_enabled_workers() {
    let directory = TestDirectory::new();
    let host = PluginHost::default();
    let current = Arc::new(Mutex::new(worker_configuration("first")));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let manager = PackageManager::open_with_configuration(
        directory.0.join("store"),
        host_target(),
        trust(),
        host.clone(),
        Arc::new({
            let current = current.clone();
            let calls = calls.clone();
            move |manifest, _epoch| {
                assert_eq!(manifest.plugin_id, PLUGIN);
                assert!(manifest
                    .permissions
                    .contains(&PluginPermission::PluginConfiguration));
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(current.lock().unwrap().clone())
            }
        }),
    )
    .await
    .unwrap();
    manager
        .install(package(&directory.0, "1.0.0", "normal"), true)
        .await
        .unwrap();
    let epoch = host.authority_epoch();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(manager
        .read_settings_in_epoch(PLUGIN, epoch, |_, _| -> Result<(), String> {
            panic!("unpermitted read")
        })
        .await
        .is_err());
    assert!(manager
        .apply_settings_in_epoch(
            PLUGIN,
            epoch,
            |_, _| -> Result<((), SettingsCommit), String> { panic!("unpermitted write") }
        )
        .await
        .is_err());
    manager
        .install(package(&directory.0, "2.0.0", "configuration"), true)
        .await
        .unwrap();
    wait_running(&host).await;
    let first = host
        .action(PLUGIN, "echo", json!({}), Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(first["configuration_revision"], "first");
    assert_eq!(first["configuration_secret_matches"], true);
    let record = manager.list().await.unwrap().remove(0);
    let observed = manager
        .read_settings_in_epoch(PLUGIN, epoch, |manifest, digest| {
            Ok((manifest.plugin_id.clone(), digest.to_owned()))
        })
        .await
        .unwrap();
    assert_eq!(observed, (PLUGIN.into(), record.active.sha256));

    let saved = current.clone();
    let (revision, outcome) = manager
        .apply_settings_in_epoch(PLUGIN, epoch, move |_, _| {
            Ok((
                "second",
                Box::new(move || {
                    *saved.lock().unwrap() = worker_configuration("second");
                    Ok(())
                }) as SettingsCommit,
            ))
        })
        .await
        .unwrap();
    assert_eq!(revision, "second");
    assert!(outcome.restart_error.is_none());
    wait_running(&host).await;
    let second = host
        .action(PLUGIN, "echo", json!({}), Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(second["configuration_revision"], "second");
    assert_ne!(first["pid"], second["pid"]);
    manager.disable_in_epoch(PLUGIN, epoch).await.unwrap();
    let loads_before = calls.load(Ordering::SeqCst);
    let saved = current.clone();
    let (_, outcome) = manager
        .apply_settings_in_epoch(PLUGIN, epoch, move |_, _| {
            Ok((
                (),
                Box::new(move || {
                    *saved.lock().unwrap() = worker_configuration("third");
                    Ok(())
                }) as SettingsCommit,
            ))
        })
        .await
        .unwrap();
    assert!(outcome.restart_error.is_none());
    assert!(host.snapshots().is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), loads_before);
    manager.enable_in_epoch(PLUGIN, epoch).await.unwrap();
    wait_running(&host).await;
    assert_eq!(
        host.action(PLUGIN, "echo", json!({}), Duration::from_secs(2))
            .await
            .unwrap()["configuration_revision"],
        "third"
    );
    manager.close().await.unwrap();
}

#[tokio::test]
async fn settings_prepare_cannot_commit_after_revocation_and_reads_recheck_epoch() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    manager
        .install(package(&directory.0, "1.0.0", "configuration"), false)
        .await
        .unwrap();
    let epoch = host.authority_epoch();
    let committed = Arc::new(AtomicBool::new(false));
    let saved = committed.clone();
    let revoker = host.clone();
    assert!(manager
        .apply_settings_in_epoch(PLUGIN, epoch, move |_, _| {
            revoker.revoke();
            revoker.resume();
            Ok((
                (),
                Box::new(move || {
                    saved.store(true, Ordering::SeqCst);
                    Ok(())
                }) as SettingsCommit,
            ))
        })
        .await
        .is_err());
    assert!(!committed.load(Ordering::SeqCst));
    let epoch = host.authority_epoch();
    let revoker = host.clone();
    assert!(manager
        .read_settings_in_epoch(PLUGIN, epoch, move |_, _| {
            revoker.revoke();
            revoker.resume();
            Ok("must not cross sessions")
        })
        .await
        .is_err());
    assert!(host.snapshots().is_empty());
    manager.close().await.unwrap();
}

#[tokio::test]
async fn saved_settings_survive_worker_restart_failure_without_reverting_secrets() {
    let directory = TestDirectory::new();
    let host = PluginHost::default();
    let current = Arc::new(Mutex::new(worker_configuration("first")));
    let manager = PackageManager::open_with_configuration(
        directory.0.join("store"),
        host_target(),
        trust(),
        host.clone(),
        Arc::new({
            let current = current.clone();
            move |_, _epoch| {
                let configuration = current.lock().unwrap().clone();
                if configuration.revision == "changed" {
                    Err("configuration unavailable".into())
                } else {
                    Ok(configuration)
                }
            }
        }),
    )
    .await
    .unwrap();
    manager
        .install(package(&directory.0, "1.0.0", "configuration"), true)
        .await
        .unwrap();
    let saved = current.clone();
    let (_, outcome) = manager
        .apply_settings_in_epoch(PLUGIN, host.authority_epoch(), move |_, _| {
            Ok((
                (),
                Box::new(move || {
                    let mut configuration = saved.lock().unwrap();
                    configuration.revision = "changed".into();
                    configuration.secrets.clear();
                    Ok(())
                }) as SettingsCommit,
            ))
        })
        .await
        .unwrap();
    assert!(outcome.restart_error.is_some());
    assert!(current.lock().unwrap().secrets.is_empty());
    assert!(manager.list().await.unwrap()[0].enabled);
    assert!(host.snapshots().is_empty());
    manager.close().await.unwrap();
}

#[tokio::test]
async fn candidate_settings_failure_preserves_the_previous_running_worker() {
    let directory = TestDirectory::new();
    let host = PluginHost::default();
    let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let manager = PackageManager::open_with_configuration(
        directory.0.join("store"),
        host_target(),
        trust(),
        host.clone(),
        Arc::new({
            let loads = loads.clone();
            move |manifest, _epoch| {
                loads.fetch_add(1, Ordering::SeqCst);
                if manifest.version == "2.0.0" {
                    Err("Required plugin setting is missing".into())
                } else {
                    Ok(worker_configuration("first"))
                }
            }
        }),
    )
    .await
    .unwrap();
    let original = manager
        .install(package(&directory.0, "1.0.0", "configuration"), true)
        .await
        .unwrap();
    wait_running(&host).await;
    let pid = host
        .action(PLUGIN, "echo", json!({}), Duration::from_secs(2))
        .await
        .unwrap()["pid"]
        .clone();
    assert!(manager
        .install(package(&directory.0, "2.0.0", "configuration"), true)
        .await
        .is_err());
    assert_eq!(
        host.action(PLUGIN, "echo", json!({}), Duration::from_secs(2))
            .await
            .unwrap()["pid"],
        pid
    );
    assert_eq!(manager.list().await.unwrap(), vec![original]);
    assert_eq!(loads.load(Ordering::SeqCst), 2);
    manager.close().await.unwrap();
}

#[tokio::test]
async fn explicit_settings_cleanup_runs_only_after_package_worker_is_reaped() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    let retained = Arc::new(AtomicBool::new(true));
    manager
        .install(package(&directory.0, "1.0.0", "normal"), true)
        .await
        .unwrap();
    manager
        .remove_in_epoch(PLUGIN, host.authority_epoch())
        .await
        .unwrap();
    assert!(retained.load(Ordering::SeqCst));
    let record = manager
        .install(package(&directory.0, "1.0.0", "normal"), true)
        .await
        .unwrap();
    let content = manager.version_path(PLUGIN, &record.active);
    let cleanup_content = content.clone();
    let observing_host = host.clone();
    let removed = retained.clone();
    manager
        .remove_with_settings_in_epoch(
            PLUGIN,
            host.authority_epoch(),
            Some(Box::new(move || {
                assert!(observing_host.snapshots().is_empty());
                assert!(cleanup_content.exists());
                removed.store(false, Ordering::SeqCst);
                Ok(())
            })),
        )
        .await
        .unwrap();
    assert!(!retained.load(Ordering::SeqCst));
    assert!(!content.exists());
    manager.close().await.unwrap();
}

#[tokio::test]
async fn failed_settings_cleanup_preserves_the_installed_record_for_retry() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    let record = manager
        .install(package(&directory.0, "1.0.0", "normal"), true)
        .await
        .unwrap();
    let content = manager.version_path(PLUGIN, &record.active);
    assert!(manager
        .remove_with_settings_in_epoch(
            PLUGIN,
            host.authority_epoch(),
            Some(Box::new(|| Err("settings deletion failed".into())))
        )
        .await
        .is_err());
    assert_eq!(manager.list().await.unwrap(), vec![record]);
    assert!(content.exists());
    assert!(host.snapshots().is_empty());
    manager
        .remove_with_settings_in_epoch(PLUGIN, host.authority_epoch(), Some(Box::new(|| Ok(()))))
        .await
        .unwrap();
    assert!(manager.list().await.unwrap().is_empty());
    assert!(!content.exists());
    manager.close().await.unwrap();
}

#[tokio::test]
async fn aggregate_store_quota_rejects_settings_before_prepare_creates_files() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    manager
        .install(package(&directory.0, "1.0.0", "configuration"), false)
        .await
        .unwrap();
    let (bytes, _) = tree_usage(&manager.0.root).unwrap();
    let filler = manager.0.root.join("quota-fixture");
    File::create(&filler)
        .unwrap()
        .set_len(MAX_STORE_BYTES - bytes - MAX_SETTINGS_FILE_BYTES as u64 + 1)
        .unwrap();
    assert!(manager
        .apply_settings_in_epoch(
            PLUGIN,
            host.authority_epoch(),
            |_, _| -> Result<((), SettingsCommit), String> {
                panic!("settings prepare must not run beyond the aggregate quota");
            }
        )
        .await
        .is_err());
    assert!(!manager.0.root.join("settings").exists());
    fs::remove_file(filler).unwrap();
    manager.close().await.unwrap();
}

#[tokio::test]
async fn signed_install_update_rollback_disable_enable_and_remove_use_real_worker() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    let first = manager
        .install(package(&directory.0, "1.0.0", "normal"), true)
        .await
        .unwrap();
    assert!(first.enabled);
    wait_running(&host).await;
    assert_eq!(
        host.action(PLUGIN, "echo", json!({}), Duration::from_secs(2))
            .await
            .unwrap()["ok"],
        true
    );
    let second = manager
        .install(package(&directory.0, "1.1.0", "normal"), true)
        .await
        .unwrap();
    assert_eq!(second.rollback.as_ref(), Some(&first.active));
    for rejected in [
        package(&directory.0, "1.1.0", "notifications_authorized"),
        package(&directory.0, "1.0.0", "normal"),
    ] {
        assert!(manager
            .install(rejected, true)
            .await
            .unwrap_err()
            .contains("newer immutable version"));
        assert_eq!(
            manager.list().await.unwrap().as_slice(),
            std::slice::from_ref(&second)
        );
    }
    assert_eq!(manager.rollback(PLUGIN).await.unwrap().active, first.active);
    assert_eq!(
        manager.rollback(PLUGIN).await.unwrap().active,
        second.active
    );
    let disabled = manager.disable(PLUGIN).await.unwrap();
    assert!(!disabled.enabled);
    assert!(host.snapshots().is_empty());
    manager.enable(PLUGIN).await.unwrap();
    wait_running(&host).await;
    let third = manager
        .install(package(&directory.0, "1.2.0", "normal"), true)
        .await
        .unwrap();
    assert_eq!(third.rollback, Some(second.active));
    assert_eq!(
        fs::read_dir(manager.0.root.join("content").join(PLUGIN))
            .unwrap()
            .count(),
        2
    );
    assert!(!manager.version_path(PLUGIN, &first.active).exists());
    manager.remove(PLUGIN).await.unwrap();
    assert!(manager.list().await.unwrap().is_empty());
    assert!(host.snapshots().is_empty());
    assert!(!manager.0.root.join("content").join(PLUGIN).exists());
    manager.close().await.unwrap();
}

#[test]
fn legacy_package_versions_remain_signature_authorized_and_pins_are_explicit() {
    let legacy = json!({"version":"1.0.0","sha256":"a".repeat(64)});
    let version: PackageVersion = serde_json::from_value(legacy.clone()).unwrap();
    assert!(!version.archive_pin);
    assert_eq!(serde_json::to_value(&version).unwrap(), legacy);
    let pinned = PackageVersion {
        archive_pin: true,
        ..version.clone()
    };
    assert_eq!(serde_json::to_value(&pinned).unwrap()["archive_pin"], true);
    assert_eq!(
        serde_json::from_value::<PackageVersion>(serde_json::to_value(&pinned).unwrap()).unwrap(),
        pinned
    );
    assert!(pinned.same_content(&version));
    for invalid in [json!(null), json!("true"), json!(1)] {
        let mut record = legacy.clone();
        record["archive_pin"] = invalid;
        assert!(serde_json::from_value::<PackageVersion>(record).is_err());
    }
}

#[tokio::test]
async fn pinned_install_reopen_update_and_rollback_keep_exact_archive_authorization() {
    let directory = TestDirectory::new();
    let host = PluginHost::default();
    let manager = PackageManager::open(
        directory.0.join("store"),
        host_target(),
        TrustStore::default(),
        host.clone(),
    )
    .await
    .unwrap();
    let initial = pinned_package(&directory.0, "1.0.0", "normal");
    assert!(initial.manifest().signature.is_none());
    let first = manager
        .install_verified_in_epoch(initial, true, host.authority_epoch())
        .await
        .unwrap();
    assert!(first.active.archive_pin);
    wait_running(&host).await;
    manager.close().await.unwrap();
    assert!(host.snapshots().is_empty());

    let host = PluginHost::default();
    let manager = PackageManager::open(
        directory.0.join("store"),
        host_target(),
        TrustStore::default(),
        host.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        manager.list().await.unwrap().as_slice(),
        std::slice::from_ref(&first)
    );
    assert!(host.snapshots().is_empty());
    let restored = manager
        .restore_enabled_in_epoch(host.authority_epoch())
        .await
        .unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].plugin_id, PLUGIN);
    assert!(restored[0].error.is_none());
    wait_running(&host).await;
    let second = manager
        .install_verified_in_epoch(
            pinned_package(&directory.0, "1.1.0", "normal"),
            true,
            host.authority_epoch(),
        )
        .await
        .unwrap();
    assert!(second.active.archive_pin);
    assert_eq!(second.rollback.as_ref(), Some(&first.active));
    let same = manager.verify_installed(PLUGIN, &second.active).unwrap();
    assert_eq!(
        manager
            .install_verified_in_epoch(same, true, host.authority_epoch())
            .await
            .unwrap(),
        second
    );
    let older = manager.verify_installed(PLUGIN, &first.active).unwrap();
    let downgraded = manager
        .install_verified_in_epoch(older, true, host.authority_epoch())
        .await
        .unwrap();
    assert_eq!(downgraded.active, first.active);
    assert_eq!(downgraded.rollback.as_ref(), Some(&second.active));
    assert_eq!(
        manager.rollback(PLUGIN).await.unwrap().active,
        second.active
    );
    assert_eq!(
        manager.list().await.unwrap().as_slice(),
        std::slice::from_ref(&second)
    );
    manager.close().await.unwrap();

    let host = PluginHost::default();
    let manager = PackageManager::open(
        directory.0.join("store"),
        host_target(),
        TrustStore::default(),
        host.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        manager.list().await.unwrap().as_slice(),
        std::slice::from_ref(&second)
    );
    assert_eq!(manager.rollback(PLUGIN).await.unwrap().active, first.active);
    wait_running(&host).await;
    assert_eq!(
        manager.rollback(PLUGIN).await.unwrap().active,
        second.active
    );
    wait_running(&host).await;
    let rebuilt = manager
        .install_verified_in_epoch(
            pinned_package_revision(&directory.0, "1.1.0", "normal", Some("rebuilt artifact")),
            true,
            host.authority_epoch(),
        )
        .await
        .unwrap();
    assert_eq!(rebuilt.active.version, second.active.version);
    assert_ne!(rebuilt.active.sha256, second.active.sha256);
    assert_eq!(rebuilt.rollback.as_ref(), Some(&second.active));
    assert_eq!(
        manager.rollback(PLUGIN).await.unwrap().active,
        second.active
    );
    assert_eq!(
        manager.rollback(PLUGIN).await.unwrap().active,
        rebuilt.active
    );
    wait_running(&host).await;
    assert_eq!(
        host.action(PLUGIN, "echo", json!({}), Duration::from_secs(2))
            .await
            .unwrap()["ok"],
        true
    );
    manager.close().await.unwrap();
}

#[tokio::test]
async fn pinned_failed_activation_restores_the_prior_worker_and_tampering_still_blocks_launch() {
    let directory = TestDirectory::new();
    let host = PluginHost::default();
    let manager = PackageManager::open(
        directory.0.join("store"),
        host_target(),
        TrustStore::default(),
        host.clone(),
    )
    .await
    .unwrap();
    let first = manager
        .install_verified_in_epoch(
            pinned_package(&directory.0, "1.0.0", "normal"),
            true,
            host.authority_epoch(),
        )
        .await
        .unwrap();
    wait_running(&host).await;
    assert!(manager
        .install_verified_in_epoch(
            pinned_package(&directory.0, "1.1.0", "bad_identity"),
            true,
            host.authority_epoch(),
        )
        .await
        .is_err());
    assert_eq!(
        manager.list().await.unwrap().as_slice(),
        std::slice::from_ref(&first)
    );
    wait_running(&host).await;
    assert_eq!(
        host.action(PLUGIN, "echo", json!({}), Duration::from_secs(2))
            .await
            .unwrap()["ok"],
        true
    );
    manager.disable(PLUGIN).await.unwrap();
    let executable = manager
        .version_path(PLUGIN, &first.active)
        .join("payload")
        .join(format!("worker{}", std::env::consts::EXE_SUFFIX));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::write(executable, b"changed unsigned worker").unwrap();
    assert!(manager.enable(PLUGIN).await.is_err());
    assert!(host.snapshots().is_empty());
    assert_eq!(manager.list().await.unwrap()[0].active, first.active);
    manager.close().await.unwrap();
}

#[tokio::test]
async fn failed_signature_or_handshake_preserves_previous_active_version_and_worker() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    let first = manager
        .install(package(&directory.0, "1.0.0", "normal"), true)
        .await
        .unwrap();
    let corrupt = package(&directory.0, "1.1.0", "normal");
    let mut bytes = fs::read(&corrupt).unwrap();
    bytes[90] ^= 1;
    fs::write(&corrupt, bytes).unwrap();
    assert!(manager.install(corrupt, true).await.is_err());
    assert_eq!(manager.list().await.unwrap(), vec![first.clone()]);
    assert!(manager
        .install(package(&directory.0, "1.1.0", "bad_identity"), true)
        .await
        .is_err());
    assert_eq!(manager.list().await.unwrap(), vec![first]);
    wait_running(&host).await;
    assert_eq!(
        host.action(PLUGIN, "echo", json!({}), Duration::from_secs(2))
            .await
            .unwrap()["ok"],
        true
    );
    manager.close().await.unwrap();
}

#[tokio::test]
async fn reopened_enabled_inventory_never_autoexecutes_and_rechecks_payload_bytes() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    let first = manager
        .install(package(&directory.0, "1.0.0", "normal"), true)
        .await
        .unwrap();
    manager.close().await.unwrap();
    assert!(host.snapshots().is_empty());
    let reopened = PackageManager::open(
        directory.0.join("store"),
        host_target(),
        trust(),
        host.clone(),
    )
    .await
    .unwrap();
    assert!(reopened.list().await.unwrap()[0].enabled);
    assert!(host.snapshots().is_empty());
    let archive = reopened
        .version_path(PLUGIN, &first.active)
        .join("archive.idplugin");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&archive, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::write(&archive, b"corrupt").unwrap();
    assert!(reopened.enable(PLUGIN).await.is_err());
    assert!(host.snapshots().is_empty());
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn extracted_payload_and_extra_inventory_are_checked_before_launch() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    let record = manager
        .install(package(&directory.0, "1.0.0", "normal"), false)
        .await
        .unwrap();
    let payload = manager.version_path(PLUGIN, &record.active).join("payload");
    let executable = payload.join(format!("worker{}", std::env::consts::EXE_SUFFIX));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::write(&executable, b"changed executable").unwrap();
    assert!(manager.enable(PLUGIN).await.is_err());
    assert!(host.snapshots().is_empty());
    manager.close().await.unwrap();
}

#[tokio::test]
async fn store_lease_rejects_other_instances_and_closed_manager_cannot_operate() {
    let directory = TestDirectory::new();
    let (manager, _) = manager(&directory).await;
    assert!(PackageManager::open(
        directory.0.join("store"),
        host_target(),
        trust(),
        PluginHost::default()
    )
    .await
    .is_err());
    manager.close().await.unwrap();
    assert!(manager.list().await.is_err());
    let second = PackageManager::open(
        directory.0.join("store"),
        host_target(),
        trust(),
        PluginHost::default(),
    )
    .await
    .unwrap();
    second.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_close_releases_store_lock_while_a_duplicate_descriptor_remains_open() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    manager
        .install(package(&directory.0, "1.0.0", "normal"), true)
        .await
        .unwrap();
    wait_running(&host).await;
    // Models a descriptor inherited by an unrelated concurrent fork before exec.
    let inherited = manager
        .0
        .lease
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .try_clone()
        .unwrap();
    manager.close().await.unwrap();
    assert!(host.snapshots().is_empty());
    assert!(manager.list().await.is_err());
    let reopened = PackageManager::open(
        directory.0.join("store"),
        host_target(),
        trust(),
        PluginHost::default(),
    )
    .await
    .unwrap();
    assert!(inherited.metadata().is_ok());
    reopened.close().await.unwrap();
    drop(inherited);
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_idle_manager_releases_store_lock_while_a_duplicate_descriptor_remains_open() {
    let directory = TestDirectory::new();
    let (manager, _) = manager(&directory).await;
    let inherited = manager
        .0
        .lease
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .try_clone()
        .unwrap();
    drop(manager);
    let reopened = PackageManager::open(
        directory.0.join("store"),
        host_target(),
        trust(),
        PluginHost::default(),
    )
    .await
    .unwrap();
    assert!(inherited.metadata().is_ok());
    reopened.close().await.unwrap();
    drop(inherited);
}

#[tokio::test]
async fn interrupted_staging_and_unreferenced_versions_are_recovered_without_starting() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    let first = manager
        .install(package(&directory.0, "1.0.0", "normal"), false)
        .await
        .unwrap();
    let staging = manager
        .0
        .root
        .join("staging")
        .join(format!("stage-{}", uuid::Uuid::new_v4()));
    create_private_directory(&staging).unwrap();
    fs::write(staging.join("partial"), b"interrupted extraction").unwrap();
    let pending = manager
        .0
        .root
        .join(format!("state-pending-{}", uuid::Uuid::new_v4()));
    fs::write(&pending, b"incomplete inventory").unwrap();
    let orphan = manager
        .0
        .root
        .join("content")
        .join(PLUGIN)
        .join("b".repeat(64));
    create_private_directory(&orphan).unwrap();
    fs::write(orphan.join("partial"), b"uncommitted package").unwrap();
    manager.close().await.unwrap();
    let reopened = PackageManager::open(
        directory.0.join("store"),
        host_target(),
        trust(),
        host.clone(),
    )
    .await
    .unwrap();
    assert_eq!(reopened.list().await.unwrap(), vec![first]);
    assert!(!staging.exists());
    assert!(!pending.exists());
    assert!(!orphan.exists());
    assert!(host.snapshots().is_empty());
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn revocation_during_handshake_cannot_activate_under_a_later_login() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    let archive = package(&directory.0, "1.0.0", "no_hello");
    let installer = manager.clone();
    let installing = tokio::spawn(async move { installer.install(archive, true).await });
    time::timeout(Duration::from_secs(5), async {
        loop {
            if host
                .snapshots()
                .iter()
                .any(|snapshot| snapshot.plugin_id == PLUGIN)
            {
                break;
            }
            time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    host.revoke();
    host.resume();
    assert!(installing.await.unwrap().is_err());
    assert!(manager.list().await.unwrap().is_empty());
    assert!(host.snapshots().is_empty());
    manager.close().await.unwrap();
}

#[tokio::test]
async fn cancelled_caller_does_not_interrupt_transaction_or_drop_store_lease() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    let archive = package(&directory.0, "1.0.0", "no_hello");
    let installer = manager.clone();
    let installing = tokio::spawn(async move { installer.install(archive, true).await });
    time::timeout(Duration::from_secs(5), async {
        while host.snapshots().is_empty() {
            time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    installing.abort();
    let _ = installing.await;
    // list waits for the detached transaction to finish restoring inventory.
    assert!(manager.list().await.unwrap().is_empty());
    assert!(host.snapshots().is_empty());
    manager.close().await.unwrap();
}

#[tokio::test]
async fn rejects_mobile_targets_unowned_directories_and_unsigned_or_unscoped_keys() {
    let directory = TestDirectory::new();
    assert!(PackageManager::open(
        directory.0.join("mobile"),
        "aarch64-linux-android".into(),
        trust(),
        PluginHost::default()
    )
    .await
    .is_err());
    let unrelated = directory.0.join("unrelated");
    create_private_directory(&unrelated).unwrap();
    fs::write(unrelated.join("keep"), b"private user content").unwrap();
    assert!(PackageManager::open(
        unrelated.clone(),
        host_target(),
        trust(),
        PluginHost::default()
    )
    .await
    .is_err());
    assert_eq!(
        fs::read(unrelated.join("keep")).unwrap(),
        b"private user content"
    );
    let manager = PackageManager::open(
        directory.0.join("empty-trust"),
        host_target(),
        TrustStore::default(),
        PluginHost::default(),
    )
    .await
    .unwrap();
    assert!(manager
        .install(package(&directory.0, "1.0.0", "normal"), false)
        .await
        .is_err());
    assert!(manager.list().await.unwrap().is_empty());
    manager.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_store_paths_are_rejected_without_deleting_external_files() {
    use std::os::unix::fs::symlink;
    let directory = TestDirectory::new();
    let (manager, _) = manager(&directory).await;
    let record = manager
        .install(package(&directory.0, "1.0.0", "normal"), false)
        .await
        .unwrap();
    let external = directory.0.join("external");
    create_private_directory(&external).unwrap();
    fs::write(external.join("keep"), b"not package data").unwrap();
    let package_path = manager.version_path(PLUGIN, &record.active);
    remove_owned_tree(&package_path).unwrap();
    symlink(&external, &package_path).unwrap();
    assert!(manager.enable(PLUGIN).await.is_err());
    assert!(manager.remove(PLUGIN).await.is_err());
    assert_eq!(
        fs::read(external.join("keep")).unwrap(),
        b"not package data"
    );
    fs::remove_file(&package_path).unwrap();
    manager.close().await.unwrap();
}

#[tokio::test]
async fn initialization_resumes_after_each_owned_empty_creation_step() {
    for step in 0..4 {
        let directory = TestDirectory::new();
        let root = directory.0.join("store");
        create_private_directory(&root).unwrap();
        fs::write(root.join("store.lock"), []).unwrap();
        if step >= 1 {
            create_private_directory(&root.join(STORE_SENTINEL)).unwrap();
        }
        if step >= 2 {
            create_private_directory(&root.join("content")).unwrap();
        }
        if step >= 3 {
            create_private_directory(&root.join("staging")).unwrap();
        }
        let manager = PackageManager::open(root, host_target(), trust(), PluginHost::default())
            .await
            .unwrap();
        assert!(manager.list().await.unwrap().is_empty());
        manager.close().await.unwrap();
    }
}

#[tokio::test]
async fn missing_inventory_never_erases_existing_package_content() {
    let directory = TestDirectory::new();
    let (manager, _) = manager(&directory).await;
    let record = manager
        .install(package(&directory.0, "1.0.0", "normal"), false)
        .await
        .unwrap();
    let archive = manager
        .version_path(PLUGIN, &record.active)
        .join("archive.idplugin");
    manager.close().await.unwrap();
    fs::remove_file(directory.0.join("store/state.json")).unwrap();
    assert!(PackageManager::open(
        directory.0.join("store"),
        host_target(),
        trust(),
        PluginHost::default()
    )
    .await
    .is_err());
    assert!(archive.exists());
}

#[tokio::test]
async fn queued_lifecycle_operations_cannot_cross_logout_and_relogin() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    let original = manager
        .install(package(&directory.0, "1.0.0", "normal"), false)
        .await
        .unwrap();
    for remove in [false, true] {
        let operation = manager.0.operation.lock().await;
        let caller = manager.clone();
        let pending = tokio::spawn(async move {
            if remove {
                caller.remove(PLUGIN).await
            } else {
                caller.disable(PLUGIN).await.map(|_| ())
            }
        });
        // Poll the public method so it captures the current epoch and then waits
        // behind the deliberately held transaction lock.
        tokio::task::yield_now().await;
        host.revoke();
        host.resume();
        drop(operation);
        assert!(pending.await.unwrap().is_err());
        assert_eq!(manager.list().await.unwrap(), vec![original.clone()]);
    }
    manager.close().await.unwrap();
}

#[tokio::test]
async fn reviewed_archive_replacement_cannot_change_installed_bytes() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    let epoch = host.authority_epoch();
    let selected = package(&directory.0, "1.0.0", "normal");
    let reviewed = manager
        .inspect_archive(selected.clone(), epoch)
        .await
        .unwrap();
    let reviewed_digest = reviewed.archive_sha256().to_owned();
    let replacement = package(&directory.0, "2.0.0", "bad_identity");
    fs::write(&selected, fs::read(replacement).unwrap()).unwrap();
    let installed = manager
        .install_verified_in_epoch(reviewed, true, epoch)
        .await
        .unwrap();
    assert_eq!(installed.active.version, "1.0.0");
    assert_eq!(installed.active.sha256, reviewed_digest);
    wait_running(&host).await;
    assert_eq!(
        host.action(PLUGIN, "echo", json!({}), Duration::from_secs(2))
            .await
            .unwrap()["ok"],
        true
    );
    manager.close().await.unwrap();
}

#[tokio::test]
async fn verified_package_from_foreign_trust_cannot_bypass_manager_policy() {
    let directory = TestDirectory::new();
    let (reviewer, reviewer_host) = manager(&directory).await;
    let reviewed = reviewer
        .inspect_archive(
            package(&directory.0, "1.0.0", "normal"),
            reviewer_host.authority_epoch(),
        )
        .await
        .unwrap();
    let receiving_host = PluginHost::default();
    let receiving_trust = TrustStore::new(vec![PublisherTrust::new(
        KEY_ID.into(),
        SigningKey::from_bytes(&[98; 32]).verifying_key().to_bytes(),
        vec![PLUGIN.into()],
    )
    .unwrap()])
    .unwrap();
    let receiver = PackageManager::open(
        directory.0.join("receiving-store"),
        host_target(),
        receiving_trust,
        receiving_host.clone(),
    )
    .await
    .unwrap();
    assert!(receiver
        .install_verified_in_epoch(reviewed, true, receiving_host.authority_epoch())
        .await
        .is_err());
    assert!(receiver.list().await.unwrap().is_empty());
    assert_eq!(
        fs::read_dir(receiver.0.root.join("staging"))
            .unwrap()
            .count(),
        0
    );
    assert!(receiving_host.snapshots().is_empty());
    receiver.close().await.unwrap();
    reviewer.close().await.unwrap();
}

#[tokio::test]
async fn installed_details_keep_corrupt_and_untrusted_records_visible_and_removable() {
    const OTHER: &str = "test.other";
    let directory = TestDirectory::new();
    let host = PluginHost::default();
    let manager = PackageManager::open(
        directory.0.join("store"),
        host_target(),
        trust_for(&[PLUGIN, OTHER]),
        host.clone(),
    )
    .await
    .unwrap();
    let corrupt = manager
        .install(package(&directory.0, "1.0.0", "normal"), false)
        .await
        .unwrap();
    manager
        .install(package_for(&directory.0, OTHER, "1.0.0", "normal"), false)
        .await
        .unwrap();
    let archive = manager
        .version_path(PLUGIN, &corrupt.active)
        .join("archive.idplugin");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&archive, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::write(archive, b"corrupt").unwrap();
    let epoch = host.authority_epoch();
    manager.0.verifications.store(0, Ordering::Relaxed);
    let valid = manager
        .details_in_epoch(OTHER, epoch)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(valid.manifest.as_ref().unwrap().plugin_id, OTHER);
    assert!(valid.error.is_none());
    assert_eq!(manager.0.verifications.load(Ordering::Relaxed), 1);
    let damaged = manager
        .details_in_epoch(PLUGIN, epoch)
        .await
        .unwrap()
        .unwrap();
    assert!(damaged.manifest.is_none());
    assert!(damaged.error.is_some());
    assert_eq!(manager.0.verifications.load(Ordering::Relaxed), 2);
    assert!(manager
        .details_in_epoch("test.absent", epoch)
        .await
        .unwrap()
        .is_none());
    assert!(manager.details_in_epoch("../invalid", epoch).await.is_err());
    assert_eq!(manager.0.verifications.load(Ordering::Relaxed), 2);
    let details = manager
        .list_details_in_epoch(host.authority_epoch())
        .await
        .unwrap();
    assert_eq!(details.len(), 2);
    assert_eq!(manager.0.verifications.load(Ordering::Relaxed), 4);
    let damaged = details
        .iter()
        .find(|item| item.record.plugin_id == PLUGIN)
        .unwrap();
    assert!(damaged.manifest.is_none());
    assert!(damaged.error.is_some());
    let valid = details
        .iter()
        .find(|item| item.record.plugin_id == OTHER)
        .unwrap();
    assert_eq!(valid.manifest.as_ref().unwrap().plugin_id, OTHER);
    assert!(valid.error.is_none());
    manager
        .remove_in_epoch(PLUGIN, host.authority_epoch())
        .await
        .unwrap();
    manager.close().await.unwrap();

    let untrusted = PackageManager::open(
        directory.0.join("store"),
        host_target(),
        TrustStore::new(Vec::new()).unwrap(),
        host.clone(),
    )
    .await
    .unwrap();
    let details = untrusted
        .list_details_in_epoch(host.authority_epoch())
        .await
        .unwrap();
    assert_eq!(details.len(), 1);
    assert_eq!(details[0].record.plugin_id, OTHER);
    assert!(details[0].manifest.is_none());
    assert!(details[0].error.is_some());
    untrusted
        .remove_in_epoch(OTHER, host.authority_epoch())
        .await
        .unwrap();
    assert!(untrusted.list().await.unwrap().is_empty());
    untrusted.close().await.unwrap();
}

#[tokio::test]
async fn explicit_epoch_operations_reject_a_review_from_before_logout_and_relogin() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    let original = manager
        .install(package(&directory.0, "1.0.0", "normal"), false)
        .await
        .unwrap();
    let epoch = host.authority_epoch();
    let selected = package(&directory.0, "2.0.0", "normal");
    let reviewed = manager
        .inspect_archive(selected.clone(), epoch)
        .await
        .unwrap();
    host.revoke();
    host.resume();
    assert!(manager.inspect_archive(selected, epoch).await.is_err());
    assert!(manager
        .install_verified_in_epoch(reviewed, true, epoch)
        .await
        .is_err());
    assert!(manager.enable_in_epoch(PLUGIN, epoch).await.is_err());
    assert!(manager.disable_in_epoch(PLUGIN, epoch).await.is_err());
    assert!(manager.rollback_in_epoch(PLUGIN, epoch).await.is_err());
    assert!(manager.remove_in_epoch(PLUGIN, epoch).await.is_err());
    assert!(manager.list_details_in_epoch(epoch).await.is_err());
    assert!(manager.details_in_epoch(PLUGIN, epoch).await.is_err());
    assert!(manager.restore_enabled_in_epoch(epoch).await.is_err());
    assert_eq!(manager.list().await.unwrap(), vec![original]);
    assert!(host.snapshots().is_empty());
    manager.close().await.unwrap();
}

#[tokio::test]
async fn restoration_is_explicit_skips_disabled_and_isolates_failed_handshakes() {
    const BAD: &str = "test.bad";
    const DISABLED: &str = "test.disabled";
    let directory = TestDirectory::new();
    let host = PluginHost::default();
    let manager = PackageManager::open(
        directory.0.join("store"),
        host_target(),
        trust_for(&[PLUGIN, BAD, DISABLED]),
        host.clone(),
    )
    .await
    .unwrap();
    manager
        .install(package(&directory.0, "1.0.0", "normal"), true)
        .await
        .unwrap();
    manager
        .install(
            package_for(&directory.0, BAD, "1.0.0", "bad_identity"),
            false,
        )
        .await
        .unwrap();
    manager
        .install(
            package_for(&directory.0, DISABLED, "1.0.0", "normal"),
            false,
        )
        .await
        .unwrap();
    // Simulate previously enabled inventory whose worker cannot handshake on
    // this startup. Keep the signed archive intact and exercise a real process.
    let mut desired = manager.read_state().unwrap();
    desired.plugins.get_mut(BAD).unwrap().enabled = true;
    manager.write_state(&desired).unwrap();
    manager.close().await.unwrap();
    let manager = PackageManager::open(
        directory.0.join("store"),
        host_target(),
        trust_for(&[PLUGIN, BAD, DISABLED]),
        host.clone(),
    )
    .await
    .unwrap();
    assert!(host.snapshots().is_empty());
    let results = manager
        .restore_enabled_in_epoch(host.authority_epoch())
        .await
        .unwrap();
    assert_eq!(results.len(), 2);
    assert!(results
        .iter()
        .find(|item| item.plugin_id == BAD)
        .unwrap()
        .error
        .is_some());
    assert!(results
        .iter()
        .find(|item| item.plugin_id == PLUGIN)
        .unwrap()
        .error
        .is_none());
    wait_running(&host).await;
    assert_eq!(host.snapshots().len(), 1);
    let pid = host
        .action(PLUGIN, "echo", json!({}), Duration::from_secs(2))
        .await
        .unwrap()["pid"]
        .clone();
    manager
        .restore_enabled_in_epoch(host.authority_epoch())
        .await
        .unwrap();
    assert_eq!(
        host.action(PLUGIN, "echo", json!({}), Duration::from_secs(2))
            .await
            .unwrap()["pid"],
        pid
    );
    assert_eq!(manager.read_state().unwrap().plugins, desired.plugins);
    manager.close().await.unwrap();
}

#[tokio::test]
async fn queued_disable_wins_over_a_later_identity_in_a_restore_pass() {
    const FIRST: &str = "test.first";
    let directory = TestDirectory::new();
    let host = PluginHost::default();
    let manager = PackageManager::open(
        directory.0.join("store"),
        host_target(),
        trust_for(&[PLUGIN, FIRST]),
        host.clone(),
    )
    .await
    .unwrap();
    manager
        .install(package_for(&directory.0, FIRST, "1.0.0", "no_hello"), false)
        .await
        .unwrap();
    manager
        .install(package(&directory.0, "1.0.0", "normal"), false)
        .await
        .unwrap();
    let mut desired = manager.read_state().unwrap();
    for record in desired.plugins.values_mut() {
        record.enabled = true;
    }
    manager.write_state(&desired).unwrap();
    let epoch = host.authority_epoch();
    let restoring = manager.clone();
    let restore = tokio::spawn(async move { restoring.restore_enabled_in_epoch(epoch).await });
    time::timeout(Duration::from_secs(5), async {
        while !host.snapshots().iter().any(|item| item.plugin_id == FIRST) {
            time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    manager.disable_in_epoch(PLUGIN, epoch).await.unwrap();
    let results = restore.await.unwrap().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].plugin_id, FIRST);
    assert!(results[0].error.is_some());
    assert!(!manager.read_state().unwrap().plugins[PLUGIN].enabled);
    assert!(host.snapshots().is_empty());
    manager.close().await.unwrap();
}

#[tokio::test]
async fn revocation_during_restoration_never_revives_a_worker_in_a_new_session() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    manager
        .install(package(&directory.0, "1.0.0", "no_hello"), false)
        .await
        .unwrap();
    let mut desired = manager.read_state().unwrap();
    desired.plugins.get_mut(PLUGIN).unwrap().enabled = true;
    manager.write_state(&desired).unwrap();
    let epoch = host.authority_epoch();
    let restoring = manager.clone();
    let restore = tokio::spawn(async move { restoring.restore_enabled_in_epoch(epoch).await });
    time::timeout(Duration::from_secs(5), async {
        while host.snapshots().is_empty() {
            time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    host.revoke();
    host.resume();
    assert!(restore.await.unwrap().is_err());
    assert!(host.snapshots().is_empty());
    assert_eq!(manager.read_state().unwrap().plugins, desired.plugins);
    manager.close().await.unwrap();
}

#[test]
fn store_lock_probe() {
    let Some(path) = std::env::var_os("INVERTER_INSTALLER_LOCK_PROBE") else {
        return;
    };
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    assert!(
        file.try_lock().is_err(),
        "a separate process must not obtain the live store lease"
    );
}

#[tokio::test]
async fn store_lease_is_exclusive_across_real_processes() {
    let directory = TestDirectory::new();
    let (manager, _) = manager(&directory).await;
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("store_lock_probe")
        .arg("--nocapture")
        .env(
            "INVERTER_INSTALLER_LOCK_PROBE",
            manager.0.root.join("store.lock"),
        )
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    manager.close().await.unwrap();
}

#[tokio::test]
async fn dropping_manager_keeps_lease_until_its_worker_is_reaped() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    manager
        .install(package(&directory.0, "1.0.0", "stalled"), true)
        .await
        .unwrap();
    #[cfg(unix)]
    let inherited = manager
        .0
        .lease
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .try_clone()
        .unwrap();
    drop(manager);
    assert!(PackageManager::open(
        directory.0.join("store"),
        host_target(),
        trust(),
        host.clone()
    )
    .await
    .is_err());
    let reopened = time::timeout(Duration::from_secs(4), async {
        loop {
            match PackageManager::open(
                directory.0.join("store"),
                host_target(),
                trust(),
                host.clone(),
            )
            .await
            {
                Ok(manager) => break manager,
                Err(_) => time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .unwrap();
    assert!(host.snapshots().is_empty());
    reopened.close().await.unwrap();
    #[cfg(unix)]
    drop(inherited);
}

#[tokio::test]
async fn recovery_never_collects_content_still_owned_by_a_worker() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    let record = manager
        .install(package(&directory.0, "1.0.0", "normal"), true)
        .await
        .unwrap();
    let active = manager.version_path(PLUGIN, &record.active);
    // An interrupted candidate has the same invariant: its worker is owned,
    // while the committed inventory does not yet reference its package bytes.
    assert!(manager.recover(&StoreState::default()).is_err());
    assert!(active.exists());
    wait_running(&host).await;
    manager.close().await.unwrap();
}

#[tokio::test]
async fn update_refuses_to_retain_corrupt_previous_active_as_rollback() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    let original = manager
        .install(package(&directory.0, "1.0.0", "normal"), true)
        .await
        .unwrap();
    wait_running(&host).await;
    let generation = host.snapshots()[0].generation;
    let archive = manager
        .version_path(PLUGIN, &original.active)
        .join("archive.idplugin");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&archive, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::write(archive, b"corrupt retained archive").unwrap();
    assert!(manager
        .install(package(&directory.0, "1.1.0", "normal"), true)
        .await
        .is_err());
    assert_eq!(manager.list().await.unwrap(), vec![original]);
    assert_eq!(host.snapshots()[0].generation, generation);
    assert_eq!(
        host.action(PLUGIN, "echo", json!({}), Duration::from_secs(2))
            .await
            .unwrap()["ok"],
        true
    );
    manager.close().await.unwrap();
}

#[tokio::test]
async fn entry_quota_refuses_install_before_staging_or_inventory_changes() {
    let directory = TestDirectory::new();
    let (manager, host) = manager(&directory).await;
    let archive = package(&directory.0, "1.0.0", "normal");
    let filler = manager.0.root.join("quota-fixture");
    create_private_directory(&filler).unwrap();
    let (_, initial_count) = tree_usage(&manager.0.root).unwrap();
    for index in initial_count..MAX_STORE_FILES - 2 {
        File::create(filler.join(format!("entry-{index}"))).unwrap();
    }
    assert_eq!(tree_usage(&manager.0.root).unwrap().1, MAX_STORE_FILES - 2);
    let inventory = fs::read(manager.0.root.join("state.json")).unwrap();
    let error = manager.install(archive, false).await.unwrap_err();
    assert!(error.contains("file quota"), "{error}");
    assert_eq!(
        fs::read(manager.0.root.join("state.json")).unwrap(),
        inventory
    );
    assert!(manager.list().await.unwrap().is_empty());
    assert!(fs::read_dir(manager.0.root.join("staging"))
        .unwrap()
        .next()
        .is_none());
    assert!(fs::read_dir(manager.0.root.join("content"))
        .unwrap()
        .next()
        .is_none());
    assert!(host.snapshots().is_empty());
    manager.close().await.unwrap();
    let reopened = PackageManager::open(directory.0.join("store"), host_target(), trust(), host)
        .await
        .unwrap();
    reopened.close().await.unwrap();
    remove_owned_tree(&filler).unwrap();
}

#[tokio::test]
async fn signed_deep_inventory_cannot_create_excessive_implicit_directories() {
    use super::super::package::{canonical_manifest_bytes, manifest_signing_payload};
    use super::super::protocol::{InventoryEntry, SignatureMetadata};
    use ed25519_dalek::Signer;
    use std::io::Cursor;
    use zip::write::SimpleFileOptions;

    let directory = TestDirectory::new();
    let (manager, _) = manager(&directory).await;
    let inventory: Vec<_> = (0..128)
        .map(|index| InventoryEntry {
            path: format!("p{index:03}/{}payload", "d/".repeat(65)),
            size: 1,
            sha256: sha256_hex(b"x"),
        })
        .collect();
    let mut manifest = PluginManifest {
        group: None,
        live_view: None,
        schema_version: 1,
        plugin_id: PLUGIN.into(),
        version: "1.0.0".into(),
        host_api: "^1.0".into(),
        target: host_target(),
        entrypoint: inventory[0].path.clone(),
        config_schema: json!({"type":"object"}),
        permissions: Vec::new(),
        http_video: None,
        inventory,
        signature: None,
    };
    let signature = key().sign(&manifest_signing_payload(&manifest).unwrap());
    manifest.signature = Some(SignatureMetadata {
        algorithm: "ed25519".into(),
        key_id: KEY_ID.into(),
        signature: signature
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    });
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o755);
    zip.start_file("manifest.json", options).unwrap();
    zip.write_all(&canonical_manifest_bytes(&manifest).unwrap())
        .unwrap();
    for item in &manifest.inventory {
        zip.start_file(&item.path, options).unwrap();
        zip.write_all(b"x").unwrap();
    }
    let bytes = zip.finish().unwrap().into_inner();
    let verified = verify_archive_bytes(bytes.clone(), &trust(), &host_target()).unwrap();
    assert!(projected_package_entries(&verified).is_err());
    let archive = directory.0.join("deep.idplugin");
    fs::write(&archive, bytes).unwrap();
    let error = manager.install(archive, false).await.unwrap_err();
    assert!(error.contains("file quota"), "{error}");
    assert!(manager.list().await.unwrap().is_empty());
    assert!(fs::read_dir(manager.0.root.join("staging"))
        .unwrap()
        .next()
        .is_none());
    assert!(fs::read_dir(manager.0.root.join("content"))
        .unwrap()
        .next()
        .is_none());
    manager.close().await.unwrap();
}

#[tokio::test]
async fn ownership_sentinel_is_atomic_directory_and_rejects_partial_files() {
    let directory = TestDirectory::new();
    let (manager, _) = manager(&directory).await;
    let sentinel = manager.0.root.join(STORE_SENTINEL);
    assert!(sentinel.is_dir());
    assert!(fs::read_dir(&sentinel).unwrap().next().is_none());
    manager.close().await.unwrap();
    fs::remove_dir(&sentinel).unwrap();
    fs::write(&sentinel, b"partial marker").unwrap();
    assert!(PackageManager::open(
        directory.0.join("store"),
        host_target(),
        trust(),
        PluginHost::default()
    )
    .await
    .is_err());
    assert_eq!(fs::read(&sentinel).unwrap(), b"partial marker");
}

#[tokio::test]
async fn http_video_permission_and_origin_come_from_verified_package_and_its_startup_configuration()
{
    let directory = TestDirectory::new();
    let host = PluginHost::default();
    let configuration = Arc::new(Mutex::new(WorkerConfiguration {
        revision: "video-settings".into(),
        values: json!({"server":"https://video.test/base/"}),
        secrets: Default::default(),
    }));
    let current = configuration.clone();
    let manager = PackageManager::open_with_configuration(
        directory.0.join("store"),
        host_target(),
        trust(),
        host.clone(),
        Arc::new(move |_, _epoch| Ok(current.lock().unwrap().clone())),
    )
    .await
    .unwrap();
    manager
        .install(
            package(&directory.0, "1.0.0", "configuration_video_authorized"),
            true,
        )
        .await
        .unwrap();
    let original = host
        .action(PLUGIN, "echo", json!({}), Duration::from_secs(2))
        .await
        .unwrap();
    let request = host.take_http_video_requests().pop().unwrap();
    assert!(request.lease.is_active());
    assert!(request
        .grant
        .validate_url("https://video.test/base/api/events/other/clip.mp4")
        .is_ok());
    configuration.lock().unwrap().values =
        json!({"server":"https://username:secret@video.test/base/"});
    assert!(manager
        .install(
            package(&directory.0, "1.1.0", "configuration_video_authorized"),
            true
        )
        .await
        .is_err());
    assert!(
        request.lease.is_active(),
        "invalid candidate grant must not stop the existing worker"
    );
    assert_eq!(
        host.action(PLUGIN, "echo", json!({}), Duration::from_secs(2))
            .await
            .unwrap()["pid"],
        original["pid"]
    );
    manager.disable(PLUGIN).await.unwrap();
    assert!(!request.lease.is_active());
    manager.close().await.unwrap();
}
