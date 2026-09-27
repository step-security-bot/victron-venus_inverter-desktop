//! Application-owned package store and review consent, independent of Tauri UI.

use super::installer::{InstalledPluginDetails, PackageManager};
use super::package::{TrustStore, VerifiedPackage};
use super::protocol::{validate_plugin_id, PluginPermission};
use super::runtime::{PluginHost, PluginSnapshot};
#[path = "groups.rs"]
mod groups;
pub(crate) use groups::PluginGroupSnapshot;
#[path = "management.rs"]
mod management;
pub(crate) use management::{remove_configured_declaration, PluginDesiredChange};
#[path = "reconciliation.rs"]
mod reconciliation;
use super::settings::{PluginSettingsView, SettingsSchema};
use super::settings_store::{SettingsData, SettingsKeyProvider, SettingsStore};
use reconciliation::{ConfiguredStatus, Reconciliation};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

pub(crate) type SettingsSeedProvider = Arc<
    dyn Fn(&super::protocol::PluginManifest, &SettingsData) -> Result<SettingsData, String>
        + Send
        + Sync,
>;

const PREVIEW_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_FAILURES: usize = 8;

#[derive(Serialize)]
pub(crate) struct SettingsSaveResult {
    settings: PluginSettingsView,
    restart_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RetainedPluginData {
    records: Vec<RetainedPluginRecord>,
    total_bytes: u64,
    max_records: usize,
    max_bytes: u64,
}

#[derive(Debug, Serialize)]
struct RetainedPluginRecord {
    record_id: String,
    revision: String,
    bytes: u64,
    plugin_id: Option<String>,
}

struct SessionActivity(Arc<ApplicationInner>);

impl Drop for SessionActivity {
    fn drop(&mut self) {
        self.0.authorized_work.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Serialize)]
pub(crate) struct ManagerSnapshot {
    pub ready: bool,
    pub error: Option<String>,
    pub installation_available: bool,
    pub target: String,
    pub data_revision: String,
    pub plugins: Vec<ManagedPlugin>,
    pub configured: Vec<ConfiguredStatus>,
    pub groups: Vec<PluginGroupSnapshot>,
    pub configuration_error: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct ManagedPlugin {
    configuration_managed: bool,
    plugin_id: String,
    version: String,
    rollback_version: Option<String>,
    enabled: bool,
    permissions: Vec<PluginPermission>,
    runtime: Option<PluginSnapshot>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct PackagePreview {
    pub token: String,
    pub plugin_id: String,
    version: String,
    current_version: Option<String>,
    permissions: Vec<PluginPermission>,
    target: String,
    host_api: String,
    publisher_key_id: String,
    restart_required: bool,
}

struct PendingReview {
    token: String,
    owner: String,
    epoch: u64,
    created: Instant,
    package: Option<VerifiedPackage>,
}

impl PendingReview {
    fn matches(&self, token: &str, owner: &str, epoch: u64) -> bool {
        self.token == token
            && self.owner == owner
            && self.epoch == epoch
            && self.created.elapsed() < PREVIEW_TTL
    }
}

enum StoreStatus {
    Opening,
    Ready(PackageManager, SettingsStore),
    Failed(String),
}

struct ApplicationInner {
    reconciliation: Reconciliation,
    local_build_root: Mutex<Option<PathBuf>>,
    settings_seed: Mutex<Option<SettingsSeedProvider>>,
    migration_waiting: AtomicBool,
    host: PluginHost,
    media: Option<super::media::MediaService>,
    target: String,
    installation_available: bool,
    store: Mutex<StoreStatus>,
    initialized: Notify,
    pending: Mutex<Option<PendingReview>>,
    failures: Mutex<BTreeMap<String, String>>,
    closing: AtomicBool,
    authorized_work: AtomicUsize,
    metadata_revision: AtomicU64,
    metadata: tokio::sync::Mutex<Option<(u64, Vec<InstalledPluginDetails>)>>,
    changed: Arc<dyn Fn() + Send + Sync>,
}

#[derive(Clone)]
pub(crate) struct PackageApplication(Arc<ApplicationInner>);

impl PackageApplication {
    #[cfg(test)]
    pub(crate) fn new(
        host: PluginHost,
        target: String,
        installation_available: bool,
        changed: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self::new_with_media(host, target, installation_available, changed, None)
    }

    pub(crate) fn new_with_media(
        host: PluginHost,
        target: String,
        installation_available: bool,
        changed: Arc<dyn Fn() + Send + Sync>,
        media: Option<super::media::MediaService>,
    ) -> Self {
        Self(Arc::new(ApplicationInner {
            reconciliation: Reconciliation::default(),
            local_build_root: Mutex::new(None),
            settings_seed: Mutex::new(None),
            migration_waiting: AtomicBool::new(false),
            host,
            media,
            target,
            installation_available,
            store: Mutex::new(StoreStatus::Opening),
            initialized: Notify::new(),
            pending: Mutex::new(None),
            failures: Mutex::new(BTreeMap::new()),
            closing: AtomicBool::new(false),
            authorized_work: AtomicUsize::new(0),
            metadata_revision: AtomicU64::new(0),
            metadata: tokio::sync::Mutex::new(None),
            changed,
        }))
    }

    /// Called once by application setup. Metadata recovery never executes a worker.
    #[cfg(test)]
    pub(crate) async fn initialize(
        &self,
        root: Result<PathBuf, String>,
        trust: Result<TrustStore, String>,
    ) {
        self.initialize_with_key(
            root,
            trust,
            Arc::new(|| Err("Test key provider is not configured".into())),
        )
        .await;
    }

    #[cfg(any(test, feature = "native-media-smoke"))]
    pub(crate) async fn initialize_with_key(
        &self,
        root: Result<PathBuf, String>,
        trust: Result<TrustStore, String>,
        key: SettingsKeyProvider,
    ) {
        self.initialize_with_seed(root, trust, key, None).await;
    }

    pub(crate) async fn initialize_with_seed(
        &self,
        root: Result<PathBuf, String>,
        trust: Result<TrustStore, String>,
        key: SettingsKeyProvider,
        seed: Option<SettingsSeedProvider>,
    ) {
        *self
            .0
            .settings_seed
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = seed;
        let result = async {
            let root = root?;
            let trust = trust?;
            let parent = root.parent().ok_or("Invalid plugin store parent")?;
            *self
                .0
                .local_build_root
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(parent.to_owned());
            let media_root = parent.join("desktop-plugin-media");
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            let settings = SettingsStore::new(root.clone(), key);
            let worker_settings = settings.clone();
            let service = self.clone();
            let manager = PackageManager::open_with_configuration(
                root,
                self.0.target.clone(),
                trust,
                self.0.host.clone(),
                Arc::new(move |manifest, epoch| {
                    let schema = SettingsSchema::compile(manifest)?;
                    let data = service.load_settings(&worker_settings, manifest, epoch)?;
                    schema.configuration(&data)
                }),
            )
            .await?;
            settings.recover()?;
            if let Some(media) = &self.0.media {
                // The package-manager lease owns this sibling store's lifetime.
                // It must be drained before that lease is released during close.
                media.initialize(media_root).await?;
            }
            Ok::<_, String>((manager, settings))
        }
        .await;
        *self
            .0
            .store
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = match result {
            Ok((manager, settings)) => StoreStatus::Ready(manager, settings),
            Err(error) => StoreStatus::Failed(error),
        };
        self.0.initialized.notify_waiters();
        (self.0.changed)();
    }

    fn check_epoch(&self, epoch: u64) -> Result<(), String> {
        if self.0.closing.load(Ordering::Acquire) || !self.0.host.is_authorized_epoch(epoch) {
            return Err("Plugin session changed or the application is closing".into());
        }
        Ok(())
    }

    fn manager(&self) -> Result<PackageManager, String> {
        match &*self
            .0
            .store
            .lock()
            .unwrap_or_else(|error| error.into_inner())
        {
            StoreStatus::Ready(manager, _) => Ok(manager.clone()),
            StoreStatus::Opening => Err("Plugin store is still opening".into()),
            StoreStatus::Failed(error) => Err(error.clone()),
        }
    }

    fn settings_store(&self) -> Result<SettingsStore, String> {
        match &*self
            .0
            .store
            .lock()
            .unwrap_or_else(|error| error.into_inner())
        {
            StoreStatus::Ready(_, settings) => Ok(settings.clone()),
            StoreStatus::Opening => Err("Plugin store is still opening".into()),
            StoreStatus::Failed(error) => Err(error.clone()),
        }
    }

    pub(crate) fn video_window_always_on_top(&self, id: &str) -> Result<bool, String> {
        super::settings::video_always_on_top(&self.settings_store()?.read(id)?)
    }

    /// Called under the package operation lock with the verified manifest.
    /// The seed is committed once, before any worker receives its configuration.
    fn load_settings(
        &self,
        store: &SettingsStore,
        manifest: &super::protocol::PluginManifest,
        epoch: u64,
    ) -> Result<SettingsData, String> {
        self.check_epoch(epoch)?;
        let current = store.read(&manifest.plugin_id)?;
        let seed = self
            .0
            .settings_seed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(seed) = seed else {
            return Ok(current);
        };
        let next = seed(manifest, &current).inspect_err(|error| {
            if error == "legacy_migration_waiting_for_daemon" {
                self.0.migration_waiting.store(true, Ordering::Release);
            }
        })?;
        if next == current {
            return Ok(current);
        }
        SettingsSchema::compile(manifest)?.seed_configuration(&next)?;
        let prepared = store.prepare_write(&manifest.plugin_id, &next)?;
        self.0.host.commit_in_epoch(epoch, || prepared.commit())?;
        Ok(next)
    }

    /// Export the current isolated values, never an obsolete configuration shadow.
    pub(crate) async fn export_modules(
        &self,
        epoch: u64,
    ) -> Result<crate::module_config::ModuleNamespaces, String> {
        self.with_portable_modules(epoch, Ok).await
    }

    /// The consumer runs before any package/settings operation can change this
    /// snapshot. It must perform its final persistence synchronously and bind it
    /// to the original host epoch; no configuration lock is held while awaiting.
    pub(crate) async fn with_portable_modules<R, F>(
        &self,
        epoch: u64,
        consume: F,
    ) -> Result<R, String>
    where
        F: FnOnce(crate::module_config::ModuleNamespaces) -> Result<R, String> + Send + 'static,
        R: Send + 'static,
    {
        let activity = self.begin_activity();
        self.check_epoch(epoch)?;
        let manager = self.manager()?;
        let store = self.settings_store()?;
        let service = self.clone();
        run_owned(activity, async move {
            manager
                .read_all_settings_in_epoch(epoch, move |packages| {
                    let mut modules = crate::module_config::ModuleNamespaces::new();
                    for (manifest, digest) in packages {
                        let schema = SettingsSchema::compile(manifest)?;
                        let data = store.read(&manifest.plugin_id)?;
                        schema.view(manifest, digest, &data)?;
                        modules.insert(
                            manifest.plugin_id.clone(),
                            crate::module_config::ModuleNamespace {
                                schema_version: std::num::NonZeroU32::new(1).unwrap(),
                                values: data
                                    .values
                                    .into_iter()
                                    .filter(|(key, _)| !data.secret_fields.contains(key))
                                    .collect(),
                                secrets: BTreeMap::new(),
                            },
                        );
                    }
                    service.check_epoch(epoch)?;
                    consume(modules)
                })
                .await
        })
        .await
    }

    pub(crate) async fn get_settings(
        &self,
        id: &str,
        epoch: u64,
    ) -> Result<PluginSettingsView, String> {
        let activity = self.begin_activity();
        self.check_epoch(epoch)?;
        let manager = self.manager()?;
        let store = self.settings_store()?;
        let id = id.to_owned();
        let service = self.clone();
        run_owned(activity, async move {
            let loader = service.clone();
            let view = manager
                .read_settings_in_epoch(&id, epoch, move |manifest, digest| {
                    let schema = SettingsSchema::compile(manifest)?;
                    // A migration conflict must not prevent opening the editor to fix it.
                    let data = loader
                        .load_settings(&store, manifest, epoch)
                        .or_else(|_| store.read(&manifest.plugin_id))?;
                    schema.view(manifest, digest, &data)
                })
                .await?;
            service.check_epoch(epoch)?;
            Ok(view)
        })
        .await
    }

    pub(crate) async fn save_settings(
        &self,
        id: &str,
        epoch: u64,
        revision: String,
        values: BTreeMap<String, Value>,
        changes: BTreeMap<String, Option<String>>,
    ) -> Result<SettingsSaveResult, String> {
        let activity = self.begin_activity();
        self.prepare_mutation(id, epoch)?;
        let manager = self.manager()?;
        let store = self.settings_store()?;
        let id = id.to_owned();
        let service = self.clone();
        run_owned(activity, async move {
            let result = manager
                .apply_settings_in_epoch(&id, epoch, move |manifest, digest| {
                    let schema = SettingsSchema::compile(manifest)?;
                    let current = store.read(&manifest.plugin_id)?;
                    let mut next = schema.merge(digest, &current, &revision, values, changes)?;
                    if next.legacy_migration_version == 0 {
                        next.legacy_migration_version = 1;
                        next.revision = uuid::Uuid::new_v4().to_string();
                    }
                    // Enforce the worker envelope bound even while the plugin is disabled.
                    schema.configuration(&next)?;
                    let view = schema.view(manifest, digest, &next)?;
                    let prepared = store.prepare_write(&manifest.plugin_id, &next)?;
                    Ok((
                        view,
                        Box::new(move || prepared.commit())
                            as Box<dyn FnOnce() -> Result<(), String> + Send>,
                    ))
                })
                .await;
            let status = match &result {
                Ok((_, applied)) => applied.restart_error.clone().map_or(Ok(()), Err),
                Err(error) => Err(error.clone()),
            };
            service.record_result(&id, epoch, &status);
            let (settings, applied) = result?;
            service.check_epoch(epoch)?;
            Ok(SettingsSaveResult {
                settings,
                restart_error: applied.restart_error,
            })
        })
        .await
    }

    pub(crate) async fn retained_data(&self, epoch: u64) -> Result<RetainedPluginData, String> {
        let activity = self.begin_activity();
        self.check_epoch(epoch)?;
        let manager = self.manager()?;
        let store = self.settings_store()?;
        let service = self.clone();
        run_owned(activity, async move {
            let view = manager
                .read_retained_data_in_epoch(epoch, move |installed| {
                    let identities = installed
                        .iter()
                        .map(|id| SettingsStore::record_id(id).map(|record| (record, id.clone())))
                        .collect::<Result<BTreeMap<_, _>, _>>()?;
                    let inventory = store.inventory()?;
                    Ok(RetainedPluginData {
                        records: inventory
                            .records
                            .into_iter()
                            .map(|record| RetainedPluginRecord {
                                plugin_id: identities.get(&record.record_id).cloned(),
                                record_id: record.record_id,
                                revision: record.revision,
                                bytes: record.bytes,
                            })
                            .collect(),
                        total_bytes: inventory.total_bytes,
                        max_records: inventory.max_records,
                        max_bytes: inventory.max_bytes,
                    })
                })
                .await?;
            service.check_epoch(epoch)?;
            Ok(view)
        })
        .await
    }

    pub(crate) async fn delete_retained_data(
        &self,
        record_id: &str,
        revision: &str,
        epoch: u64,
    ) -> Result<(), String> {
        let activity = self.begin_activity();
        self.check_epoch(epoch)?;
        let manager = self.manager()?;
        let store = self.settings_store()?;
        let record_id = record_id.to_owned();
        let expected_record = record_id.clone();
        let revision = revision.to_owned();
        let service = self.clone();
        self.0.host.commit_in_epoch(epoch, || {
            self.clear_preview();
            Ok(())
        })?;
        run_owned(activity, async move {
            let result = manager
                .remove_retained_data_in_epoch(
                    &record_id,
                    epoch,
                    Box::new(move || store.remove_record(&expected_record, &revision)),
                )
                .await;
            if result.is_ok() {
                service.0.metadata_revision.fetch_add(1, Ordering::AcqRel);
                (service.0.changed)();
            }
            result?;
            service.check_epoch(epoch)
        })
        .await
    }

    async fn wait_manager(&self) -> Result<PackageManager, String> {
        loop {
            let ready = self.0.initialized.notified();
            {
                let store = self
                    .0
                    .store
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                match &*store {
                    StoreStatus::Ready(manager, _) => return Ok(manager.clone()),
                    StoreStatus::Failed(error) => return Err(error.clone()),
                    StoreStatus::Opening => {}
                }
            }
            ready.await;
        }
    }

    pub(crate) async fn snapshot(&self, epoch: u64) -> Result<ManagerSnapshot, String> {
        self.check_epoch(epoch)?;
        self.expire_preview();
        let mut snapshot = ManagerSnapshot {
            ready: false,
            error: None,
            installation_available: self.0.installation_available,
            target: self.0.target.clone(),
            data_revision: self.0.metadata_revision.load(Ordering::Acquire).to_string(),
            plugins: Vec::new(),
            configured: self.0.reconciliation.statuses(),
            groups: Vec::new(),
            configuration_error: self.0.reconciliation.error(),
        };
        let manager = match self.manager() {
            Ok(manager) => manager,
            Err(error) => {
                snapshot.error = Some(error);
                return Ok(snapshot);
            }
        };
        // Worker contribution events may arrive at 20 Hz. Verify UI metadata
        // once per inventory/session revision, not on every telemetry refresh.
        // The installer still re-verifies archive and payload before each launch.
        let revision = self.0.metadata_revision.load(Ordering::Acquire);
        snapshot.data_revision = revision.to_string();
        let details = {
            let mut cache = self.0.metadata.lock().await;
            if let Some((cached_revision, details)) = &*cache {
                if *cached_revision == revision {
                    details.clone()
                } else {
                    let details = manager.list_details_in_epoch(epoch).await?;
                    *cache = Some((revision, details.clone()));
                    details
                }
            } else {
                let details = manager.list_details_in_epoch(epoch).await?;
                *cache = Some((revision, details.clone()));
                details
            }
        };
        self.check_epoch(epoch)?;
        let runtimes = self.0.host.snapshots();
        let failures = self
            .0
            .failures
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        snapshot.ready = true;
        snapshot.groups = groups::project(&details);
        snapshot.plugins = details
            .into_iter()
            .map(|details| {
                let managed = self.0.reconciliation.contains(&details.record.plugin_id);
                let mut plugin = managed_plugin(details, &runtimes, &failures);
                plugin.configuration_managed = managed;
                plugin
            })
            .collect();
        Ok(snapshot)
    }

    pub(crate) fn begin_selection(&self, owner: &str, epoch: u64) -> Result<String, String> {
        self.check_epoch(epoch)?;
        self.manager()?;
        if owner != "config" || !self.0.installation_available {
            return Err("Package installation is unavailable in this window or build".into());
        }
        self.0.host.commit_in_epoch(epoch, || {
            let mut pending = self
                .0
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if pending.as_ref().is_some_and(|review| {
                review.package.is_none() && review.created.elapsed() < PREVIEW_TTL
            }) {
                return Err("A package file selection is already in progress".into());
            }
            let token = uuid::Uuid::new_v4().to_string();
            *pending = Some(PendingReview {
                token: token.clone(),
                owner: owner.into(),
                epoch,
                created: Instant::now(),
                package: None,
            });
            Ok(token)
        })
    }

    fn check_selection(&self, token: &str, owner: &str, epoch: u64) -> Result<(), String> {
        self.check_epoch(epoch)?;
        if self
            .0
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .is_some_and(|review| review.matches(token, owner, epoch))
        {
            Ok(())
        } else {
            Err("Package review expired or was cancelled; select the package again".into())
        }
    }

    /// Only a native file-dialog result reaches this method. The webview has no path input.
    pub(crate) async fn finish_selection(
        &self,
        token: &str,
        owner: &str,
        epoch: u64,
        path: PathBuf,
    ) -> Result<PackagePreview, String> {
        self.check_selection(token, owner, epoch)?;
        if self
            .0
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .is_some_and(|review| review.package.is_some())
        {
            return Err("Package selection has already been reviewed".into());
        }
        let manager = self.manager()?;
        let package = manager.inspect_archive(path, epoch).await?;
        self.check_selection(token, owner, epoch)?;
        let current_version = manager
            .list()
            .await?
            .into_iter()
            .find(|record| record.plugin_id == package.manifest().plugin_id)
            .map(|record| record.active.version);
        self.check_selection(token, owner, epoch)?;
        self.0.host.commit_in_epoch(epoch, || {
            let mut pending = self
                .0
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let review = pending
                .as_mut()
                .filter(|review| review.matches(token, owner, epoch) && review.package.is_none())
                .ok_or("Package review expired or was cancelled")?;
            let manifest = package.manifest();
            let preview = PackagePreview {
                token: token.into(),
                plugin_id: manifest.plugin_id.clone(),
                version: manifest.version.clone(),
                current_version,
                permissions: manifest.permissions.clone(),
                target: manifest.target.clone(),
                host_api: manifest.host_api.clone(),
                publisher_key_id: manifest
                    .signature
                    .as_ref()
                    .ok_or("Verified package is missing its publisher")?
                    .key_id
                    .clone(),
                restart_required: false,
            };
            review.package = Some(package);
            Ok(preview)
        })
    }

    pub(crate) fn discard(&self, token: &str, owner: &str, epoch: u64) {
        let mut pending = self
            .0
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if pending.as_ref().is_some_and(|review| {
            review.token == token && review.owner == owner && review.epoch == epoch
        }) {
            pending.take();
        }
    }

    pub(crate) fn window_closed(&self, owner: &str) {
        let mut pending = self
            .0
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if pending.as_ref().is_some_and(|review| review.owner == owner) {
            pending.take();
        }
    }

    pub(crate) fn expire_preview(&self) {
        let mut pending = self
            .0
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if pending
            .as_ref()
            .is_some_and(|review| review.created.elapsed() >= PREVIEW_TTL)
        {
            pending.take();
        }
    }

    pub(crate) fn has_session_work(&self) -> bool {
        self.0.authorized_work.load(Ordering::Acquire) != 0
            || self
                .0
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_some()
    }

    fn clear_preview(&self) {
        self.0
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }

    pub(crate) async fn install_review(
        &self,
        token: &str,
        owner: &str,
        epoch: u64,
        enable: bool,
    ) -> Result<(), String> {
        // Keep expiry checks active before consuming the last pending preview.
        let activity = self.begin_activity();
        self.check_selection(token, owner, epoch)?;
        let manager = self.manager()?;
        let package = self.0.host.commit_in_epoch(epoch, || {
            let mut pending = self
                .0
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !pending.as_ref().is_some_and(|review| {
                review.matches(token, owner, epoch) && review.package.is_some()
            }) {
                return Err("Package review is unavailable; select the package again".into());
            }
            pending
                .take()
                .and_then(|review| review.package)
                .ok_or_else(|| "Package review is unavailable".into())
        })?;
        let id = package.manifest().plugin_id.clone();
        self.require_unmanaged(&id)?;
        self.run_operation(id, epoch, activity, async move {
            manager
                .install_verified_in_epoch(package, enable, epoch)
                .await
                .map(|_| ())
        })
        .await
    }

    #[cfg(test)]
    pub(crate) async fn set_enabled(
        &self,
        id: &str,
        enabled: bool,
        epoch: u64,
    ) -> Result<(), String> {
        self.set_enabled_with_config(id, enabled, epoch, |id, _, service| {
            service.require_unmanaged(id)
        })
        .await
    }

    pub(crate) async fn rollback(&self, id: &str, epoch: u64) -> Result<(), String> {
        self.require_unmanaged(id)?;
        let activity = self.begin_activity();
        self.prepare_mutation(id, epoch)?;
        let manager = self.manager()?;
        let id = id.to_owned();
        self.run_operation(id.clone(), epoch, activity, async move {
            manager.rollback_in_epoch(&id, epoch).await.map(|_| ())
        })
        .await
    }

    #[cfg(test)]
    pub(crate) async fn uninstall(&self, id: &str, epoch: u64) -> Result<(), String> {
        self.uninstall_with_settings(id, false, epoch).await
    }

    #[cfg(test)]
    pub(crate) async fn uninstall_with_settings(
        &self,
        id: &str,
        delete_settings: bool,
        epoch: u64,
    ) -> Result<(), String> {
        self.uninstall_with_config(id, delete_settings, epoch, |id, _, service| {
            service.require_unmanaged(id)
        })
        .await
    }

    fn begin_activity(&self) -> SessionActivity {
        self.0.authorized_work.fetch_add(1, Ordering::AcqRel);
        SessionActivity(self.0.clone())
    }

    async fn run_operation<F>(
        &self,
        id: String,
        epoch: u64,
        activity: SessionActivity,
        operation: F,
    ) -> Result<(), String>
    where
        F: Future<Output = Result<(), String>> + Send + 'static,
    {
        let service = self.clone();
        run_owned(activity, async move {
            let _operation = service.0.reconciliation.operation.lock().await;
            service.check_epoch(epoch)?;
            service.require_unmanaged(&id)?;
            let result = operation.await;
            service.record_result(&id, epoch, &result);
            result
        })
        .await
    }

    fn prepare_mutation(&self, id: &str, epoch: u64) -> Result<(), String> {
        self.check_epoch(epoch)?;
        validate_plugin_id(id)?;
        self.0.host.commit_in_epoch(epoch, || {
            self.clear_preview();
            Ok(())
        })
    }

    fn record_result(&self, id: &str, epoch: u64, result: &Result<(), String>) {
        self.0.metadata_revision.fetch_add(1, Ordering::AcqRel);
        let _ = self.0.host.commit_in_epoch(epoch, || {
            let mut failures = self
                .0
                .failures
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Err(error) = result {
                if failures.len() < MAX_FAILURES || failures.contains_key(id) {
                    failures.insert(id.into(), error.clone());
                }
            } else {
                failures.remove(id);
            }
            Ok(())
        });
        (self.0.changed)();
    }

    /// Session revocation is synchronous; restoration is a separate, epoch-bound task.
    #[cfg(any(test, feature = "native-media-smoke"))]
    pub(crate) fn session_changed(&self, unlocked: bool) -> Option<u64> {
        self.transition_session(unlocked, None)
    }

    pub(crate) fn session_configured(
        &self,
        unlocked: bool,
        configuration: Result<Vec<crate::plugin_config::DesktopPluginConfig>, String>,
    ) -> Option<u64> {
        self.transition_session(unlocked, Some(configuration))
    }

    fn transition_session(
        &self,
        unlocked: bool,
        configuration: Option<Result<Vec<crate::plugin_config::DesktopPluginConfig>, String>>,
    ) -> Option<u64> {
        let epoch = self.0.host.revoke_epoch();
        self.0.metadata_revision.fetch_add(1, Ordering::AcqRel);
        self.clear_preview();
        self.0
            .failures
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        // Do not publish a new authorized epoch with the previous declarations.
        if let Some(configuration) = configuration {
            self.configure(configuration);
        }
        if unlocked && !self.0.closing.load(Ordering::Acquire) && self.0.host.resume_in_epoch(epoch)
        {
            Some(epoch)
        } else {
            None
        }
    }

    pub(crate) fn take_waiting_migration(&self) -> bool {
        self.0.migration_waiting.swap(false, Ordering::AcqRel)
    }

    pub(crate) async fn restore(&self, epoch: u64) -> Result<(), String> {
        let activity = self.begin_activity();
        let service = self.clone();
        run_owned(activity, async move {
            let manager = service.wait_manager().await?;
            service.check_epoch(epoch)?;
            service.reconcile(manager, epoch).await?;
            service.check_epoch(epoch)
        })
        .await
    }

    pub(crate) fn begin_shutdown(&self) {
        self.0.closing.store(true, Ordering::Release);
        self.0.host.revoke();
        self.clear_preview();
    }

    pub(crate) async fn close(&self) -> Result<(), String> {
        self.begin_shutdown();
        // Failed initialization owns no store lease. Successful initialization
        // must finish before close can release its lease and allow app exit.
        if let Ok(manager) = self.wait_manager().await {
            if let Some(media) = &self.0.media {
                media.shutdown().await?;
            }
            manager.close().await?;
        } else if let Some(media) = &self.0.media {
            media.shutdown().await?;
        }
        self.0.host.shutdown().await;
        Ok(())
    }
}

/// Installer transactions outlive a cancelled IPC caller. Their expiry-watch
/// activity and completion notification must have the same owned lifetime.
async fn run_owned<T, F>(activity: SessionActivity, operation: F) -> Result<T, String>
where
    T: Send + 'static,
    F: Future<Output = Result<T, String>> + Send + 'static,
{
    tokio::spawn(async move {
        let _activity = activity;
        operation.await
    })
    .await
    .map_err(|error| format!("Plugin operation task failed: {error}"))?
}

fn managed_plugin(
    details: InstalledPluginDetails,
    runtimes: &[PluginSnapshot],
    failures: &BTreeMap<String, String>,
) -> ManagedPlugin {
    let id = details.record.plugin_id;
    let runtime = runtimes
        .iter()
        .find(|worker| worker.plugin_id == id)
        .cloned();
    let error = details
        .error
        .or_else(|| failures.get(&id).cloned())
        .or_else(|| {
            runtime
                .as_ref()
                .and_then(|worker| worker.last_error.clone())
        });
    ManagedPlugin {
        configuration_managed: false,
        plugin_id: id,
        version: details.record.active.version,
        rollback_version: details.record.rollback.map(|version| version.version),
        enabled: details.record.enabled,
        permissions: details
            .manifest
            .map(|manifest| manifest.permissions)
            .unwrap_or_default(),
        runtime,
        error,
    }
}

#[cfg(test)]
#[path = "application_tests.rs"]
mod tests;
