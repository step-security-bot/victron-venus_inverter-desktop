//! Serialized native package lifecycle. No executable path or trust policy comes from IPC.
//!
//! The store lease is exclusive for its entire lifetime, including worker cleanup.
//! Installed archives and extracted payloads are reverified before every launch.
//! Native workers are not an OS sandbox; same-account filesystem tampering during
//! execution remains outside this package integrity boundary.

use super::package::{
    read_regular_file, verify_archive_bytes, verify_pinned_archive_bytes, TrustStore,
    VerifiedPackage, MAX_ARCHIVE_BYTES,
};
use super::protocol::{
    validate_plugin_id, PluginManifest, PluginPermission, WorkerConfiguration, DESKTOP_TARGETS,
};
use super::runtime::{PluginError, PluginHost, WorkerSpec, WorkerState};
use super::settings_store::{SettingsStore, MAX_SETTINGS_FILE_BYTES};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::{self, Instant};

const STORE_SCHEMA: u32 = 1;
const MAX_PLUGINS: usize = 8;
const MAX_STATE_BYTES: usize = 64 * 1024;
const MAX_STORE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_STORE_FILES: usize = 8192;
const STARTUP_BUDGET: Duration = Duration::from_secs(5);
const STORE_SENTINEL: &str = "store-v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageVersion {
    pub version: String,
    pub sha256: String,
    /// This exact artifact was authorized by native application configuration.
    /// Local configuration and inventory are in the same-user trust boundary;
    /// the retained digest still verifies every archive and extracted payload.
    #[serde(default, skip_serializing_if = "is_false")]
    pub archive_pin: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl PackageVersion {
    fn same_content(&self, other: &Self) -> bool {
        self.version == other.version && self.sha256 == other.sha256
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledPlugin {
    pub plugin_id: String,
    pub active: PackageVersion,
    pub rollback: Option<PackageVersion>,
    /// Desired state. Opening a store does not automatically launch workers.
    pub enabled: bool,
}

/// Metadata remains visible when installed bytes no longer verify. A manifest
/// is exposed only after its archive and extracted inventory pass verification.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct InstalledPluginDetails {
    pub record: InstalledPlugin,
    pub manifest: Option<PluginManifest>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PluginRestoreResult {
    pub plugin_id: String,
    pub error: Option<String>,
}

pub(crate) type ConfigurationLoader =
    Arc<dyn Fn(&PluginManifest, u64) -> Result<WorkerConfiguration, String> + Send + Sync>;
pub(crate) type SettingsCommit = Box<dyn FnOnce() -> Result<(), String> + Send>;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SettingsApplyResult {
    pub restart_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreState {
    schema_version: u32,
    plugins: BTreeMap<String, InstalledPlugin>,
}

impl Default for StoreState {
    fn default() -> Self {
        Self {
            schema_version: STORE_SCHEMA,
            plugins: BTreeMap::new(),
        }
    }
}

struct ManagerInner {
    root: PathBuf,
    target: String,
    trust: TrustStore,
    host: PluginHost,
    operation: AsyncMutex<()>,
    lease: Mutex<Option<File>>,
    owned: Mutex<BTreeMap<String, String>>,
    closed: AtomicBool,
    configuration: Option<ConfigurationLoader>,
    #[cfg(test)]
    verifications: std::sync::atomic::AtomicUsize,
}

struct CleanupLease(Option<File>);

impl CleanupLease {
    fn release(mut self) {
        // A fork can briefly retain this descriptor before exec closes it.
        // Explicitly release the shared lock only after cleanup has succeeded.
        if self.0.as_ref().is_some_and(|lease| lease.unlock().is_err()) {
            // Drop retains the lease when its release could not be confirmed.
            return;
        }
        self.0.take();
    }
}

impl Drop for CleanupLease {
    fn drop(&mut self) {
        if let Some(lease) = self.0.take() {
            // Dropping/cancelling a cleanup future is not proof that its child
            // processes were reaped. Keep the OS lock until this process exits.
            std::mem::forget(lease);
        }
    }
}

impl Drop for ManagerInner {
    fn drop(&mut self) {
        let Some(lease) = self
            .lease
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            return;
        };
        let owned = self
            .owned
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let lease = CleanupLease(Some(lease));
        if owned.is_empty() {
            lease.release();
            return;
        }
        let host = self.host.clone();
        // Keep the lease alive while asynchronous cleanup finishes. A dropped
        // caller cannot make a second process replace a still-running package.
        let cleanup = async move {
            let mut reaped = true;
            for id in owned.into_keys() {
                if let Err(error) = host.remove(&id).await {
                    reaped &= error == PluginError::UnknownPlugin;
                }
            }
            if reaped {
                lease.release();
            }
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(cleanup);
        } else {
            std::thread::spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime.block_on(cleanup),
                    Err(_) => drop(cleanup),
                }
            });
        }
    }
}

/// A native-only manager. Clones serialize operations; other instances/processes
/// cannot open this store until its workers have stopped and its lease is released.
#[derive(Clone)]
pub struct PackageManager(Arc<ManagerInner>);

impl PackageManager {
    pub async fn open(
        root: PathBuf,
        target: String,
        trust: TrustStore,
        host: PluginHost,
    ) -> Result<Self, String> {
        Self::open_inner(root, target, trust, host, None).await
    }

    pub(crate) async fn open_with_configuration(
        root: PathBuf,
        target: String,
        trust: TrustStore,
        host: PluginHost,
        configuration: ConfigurationLoader,
    ) -> Result<Self, String> {
        Self::open_inner(root, target, trust, host, Some(configuration)).await
    }

    async fn open_inner(
        root: PathBuf,
        target: String,
        trust: TrustStore,
        host: PluginHost,
        configuration: Option<ConfigurationLoader>,
    ) -> Result<Self, String> {
        if !DESKTOP_TARGETS.contains(&target.as_str()) {
            return Err("package store requires a supported desktop target".into());
        }
        let (root, lease) = open_store(&root)?;
        let manager = Self(Arc::new(ManagerInner {
            root,
            target,
            trust,
            host,
            operation: AsyncMutex::new(()),
            lease: Mutex::new(Some(lease)),
            owned: Mutex::new(BTreeMap::new()),
            closed: AtomicBool::new(false),
            configuration,
            #[cfg(test)]
            verifications: std::sync::atomic::AtomicUsize::new(0),
        }));
        let state = manager.read_state()?;
        manager.recover(&state)?;
        Ok(manager)
    }

    pub async fn list(&self) -> Result<Vec<InstalledPlugin>, String> {
        let _operation = self.0.operation.lock().await;
        self.check_open()?;
        Ok(self.read_state()?.plugins.into_values().collect())
    }

    pub(crate) async fn list_details_in_epoch(
        &self,
        epoch: u64,
    ) -> Result<Vec<InstalledPluginDetails>, String> {
        self.inspect_details_in_epoch(None, epoch).await
    }

    /// Reconciliation needs one package, not another verification of every
    /// installed archive and extracted payload for each configured plugin.
    pub(crate) async fn details_in_epoch(
        &self,
        id: &str,
        epoch: u64,
    ) -> Result<Option<InstalledPluginDetails>, String> {
        validate_plugin_id(id)?;
        Ok(self
            .inspect_details_in_epoch(Some(id.to_owned()), epoch)
            .await?
            .pop())
    }

    async fn inspect_details_in_epoch(
        &self,
        id: Option<String>,
        epoch: u64,
    ) -> Result<Vec<InstalledPluginDetails>, String> {
        let manager = self.clone();
        tokio::spawn(async move {
            let _operation = manager.0.operation.lock().await;
            manager.check_epoch(epoch)?;
            let mut details = Vec::new();
            for record in manager.read_state()?.plugins.into_values() {
                if id.as_ref().is_some_and(|id| *id != record.plugin_id) {
                    continue;
                }
                let (manifest, error) =
                    match manager.verify_installed(&record.plugin_id, &record.active) {
                        Ok(package) => (Some(package.manifest().clone()), None),
                        Err(error) => (None, Some(error)),
                    };
                details.push(InstalledPluginDetails {
                    record,
                    manifest,
                    error,
                });
            }
            manager.check_epoch(epoch)?;
            Ok(details)
        })
        .await
        .map_err(|_| "package inspection task failed".to_string())?
    }

    /// Own the selected archive bytes for review. Installing this value never
    /// reopens the selected path, even if its contents are replaced meanwhile.
    pub(crate) async fn inspect_archive(
        &self,
        archive: PathBuf,
        epoch: u64,
    ) -> Result<VerifiedPackage, String> {
        let manager = self.clone();
        tokio::spawn(async move {
            let _operation = manager.0.operation.lock().await;
            manager.check_epoch(epoch)?;
            let package = verify_archive_bytes(
                read_regular_file(&archive, MAX_ARCHIVE_BYTES)?,
                &manager.0.trust,
                &manager.0.target,
            )?;
            manager.check_epoch(epoch)?;
            Ok(package)
        })
        .await
        .map_err(|_| "package inspection task failed".to_string())?
    }

    pub(crate) async fn install_verified_in_epoch(
        &self,
        package: VerifiedPackage,
        enable: bool,
        epoch: u64,
    ) -> Result<InstalledPlugin, String> {
        let manager = self.clone();
        tokio::spawn(async move {
            let _operation = manager.0.operation.lock().await;
            manager.check_epoch(epoch)?;
            // Signed packages still require this manager's publisher policy;
            // explicit native pins retain their exact immutable authorization.
            let package = package.reverify(&manager.0.trust, &manager.0.target)?;
            manager.install_package_locked(package, enable, epoch).await
        })
        .await
        .map_err(|_| "package installation task failed".to_string())?
    }

    /// Verification happens before the working version is stopped. Installation
    /// can remain disabled; activation always requires a successful handshake.
    pub async fn install(&self, archive: PathBuf, enable: bool) -> Result<InstalledPlugin, String> {
        let manager = self.clone();
        let epoch = self.0.host.authority_epoch();
        tokio::spawn(async move { manager.install_inner(archive, enable, epoch).await })
            .await
            .map_err(|_| "package installation task failed".to_string())?
    }

    pub async fn enable(&self, plugin_id: &str) -> Result<InstalledPlugin, String> {
        self.enable_in_epoch(plugin_id, self.0.host.authority_epoch())
            .await
    }

    pub(crate) async fn enable_in_epoch(
        &self,
        plugin_id: &str,
        epoch: u64,
    ) -> Result<InstalledPlugin, String> {
        let manager = self.clone();
        let id = plugin_id.to_owned();
        tokio::spawn(async move { manager.select_inner(&id, false, epoch).await })
            .await
            .map_err(|_| "package activation task failed".to_string())?
    }

    pub async fn rollback(&self, plugin_id: &str) -> Result<InstalledPlugin, String> {
        self.rollback_in_epoch(plugin_id, self.0.host.authority_epoch())
            .await
    }

    pub(crate) async fn rollback_in_epoch(
        &self,
        plugin_id: &str,
        epoch: u64,
    ) -> Result<InstalledPlugin, String> {
        let manager = self.clone();
        let id = plugin_id.to_owned();
        tokio::spawn(async move { manager.select_inner(&id, true, epoch).await })
            .await
            .map_err(|_| "package rollback task failed".to_string())?
    }

    pub async fn disable(&self, plugin_id: &str) -> Result<InstalledPlugin, String> {
        self.disable_in_epoch(plugin_id, self.0.host.authority_epoch())
            .await
    }

    pub(crate) async fn disable_in_epoch(
        &self,
        plugin_id: &str,
        epoch: u64,
    ) -> Result<InstalledPlugin, String> {
        let manager = self.clone();
        let id = plugin_id.to_owned();
        tokio::spawn(async move {
            let _operation = manager.0.operation.lock().await;
            manager.check_epoch(epoch)?;
            let mut state = manager.read_state()?;
            let mut record = state
                .plugins
                .get(&id)
                .cloned()
                .ok_or("plugin is not installed")?;
            manager.stop_owned(&id).await?;
            record.enabled = false;
            state.plugins.insert(id, record.clone());
            manager
                .0
                .host
                .commit_in_epoch(epoch, || manager.write_state(&state))?;
            Ok(record)
        })
        .await
        .map_err(|_| "package disable task failed".to_string())?
    }

    /// Removes package records/content while retaining the plugin settings.
    /// An explicit native cleanup callback can remove only that plugin's settings.
    pub async fn remove(&self, plugin_id: &str) -> Result<(), String> {
        self.remove_in_epoch(plugin_id, self.0.host.authority_epoch())
            .await
    }

    pub(crate) async fn remove_in_epoch(&self, plugin_id: &str, epoch: u64) -> Result<(), String> {
        self.remove_with_settings_in_epoch(plugin_id, epoch, None)
            .await
    }

    pub(crate) async fn remove_with_settings_in_epoch(
        &self,
        plugin_id: &str,
        epoch: u64,
        cleanup: Option<SettingsCommit>,
    ) -> Result<(), String> {
        let manager = self.clone();
        let id = plugin_id.to_owned();
        tokio::spawn(async move {
            let _operation = manager.0.operation.lock().await;
            manager.check_epoch(epoch)?;
            let mut state = manager.read_state()?;
            if !state.plugins.contains_key(&id) {
                return Err("plugin is not installed".into());
            }
            manager.stop_owned(&id).await?;
            state.plugins.remove(&id);
            manager.0.host.commit_in_epoch(epoch, || {
                // Keep the installed record visible if explicit settings deletion
                // fails. A later inventory I/O failure may leave settings cleared
                // on that visible record; these are separate filesystem commits.
                if let Some(cleanup) = cleanup {
                    cleanup()?;
                }
                manager.write_state(&state)
            })?;
            manager.recover(&state)?;
            manager.check_epoch(epoch)
        })
        .await
        .map_err(|_| "package removal task failed".to_string())?
    }

    /// Identify retained data against the current installed inventory, including
    /// disabled packages and packages whose payload no longer verifies.
    pub(crate) async fn read_retained_data_in_epoch<R, F>(
        &self,
        epoch: u64,
        read: F,
    ) -> Result<R, String>
    where
        F: FnOnce(&BTreeSet<String>) -> Result<R, String> + Send + 'static,
        R: Send + 'static,
    {
        let manager = self.clone();
        tokio::spawn(async move {
            let _operation = manager.0.operation.lock().await;
            manager.check_epoch(epoch)?;
            let installed = manager.read_state()?.plugins.into_keys().collect();
            let result = read(&installed)?;
            manager.check_epoch(epoch)?;
            Ok(result)
        })
        .await
        .map_err(|_| "retained plugin data read task failed".to_string())?
    }

    /// Delete only data that has no installed owner. The native callback checks
    /// the reviewed ciphertext revision and removes its exact bounded record.
    pub(crate) async fn remove_retained_data_in_epoch(
        &self,
        record_id: &str,
        epoch: u64,
        cleanup: SettingsCommit,
    ) -> Result<(), String> {
        let manager = self.clone();
        let record_id = record_id.to_owned();
        tokio::spawn(async move {
            let _operation = manager.0.operation.lock().await;
            manager.check_epoch(epoch)?;
            if record_id.len() != 64
                || !record_id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err("invalid retained plugin data identity".into());
            }
            // Package verification must not weaken ownership protection: a
            // disabled or corrupt installed package still owns its settings.
            for id in manager.read_state()?.plugins.keys() {
                if SettingsStore::record_id(id)? == record_id {
                    return Err("plugin data belongs to an installed package".into());
                }
            }
            manager.0.host.commit_in_epoch(epoch, cleanup)?;
            manager.check_epoch(epoch)
        })
        .await
        .map_err(|_| "retained plugin data removal task failed".to_string())?
    }

    /// Keep a coherent settings snapshot and its synchronous consumer under the
    /// same operation lock as installation, settings writes, and removal.
    pub(crate) async fn read_all_settings_in_epoch<R, F>(
        &self,
        epoch: u64,
        read: F,
    ) -> Result<R, String>
    where
        F: FnOnce(&[(PluginManifest, String)]) -> Result<R, String> + Send + 'static,
        R: Send + 'static,
    {
        let manager = self.clone();
        tokio::spawn(async move {
            let _operation = manager.0.operation.lock().await;
            manager.check_epoch(epoch)?;
            let mut packages = Vec::new();
            for record in manager.read_state()?.plugins.into_values() {
                let package = manager.verify_installed(&record.plugin_id, &record.active)?;
                if package
                    .manifest()
                    .permissions
                    .contains(&PluginPermission::PluginConfiguration)
                {
                    packages.push((package.manifest().clone(), record.active.sha256));
                }
            }
            let result = read(&packages)?;
            manager.check_epoch(epoch)?;
            Ok(result)
        })
        .await
        .map_err(|_| "plugin settings snapshot task failed".to_string())?
    }

    pub(crate) async fn read_settings_in_epoch<R, F>(
        &self,
        plugin_id: &str,
        epoch: u64,
        read: F,
    ) -> Result<R, String>
    where
        F: FnOnce(&PluginManifest, &str) -> Result<R, String> + Send + 'static,
        R: Send + 'static,
    {
        let manager = self.clone();
        let id = plugin_id.to_owned();
        tokio::spawn(async move {
            let _operation = manager.0.operation.lock().await;
            manager.check_epoch(epoch)?;
            let state = manager.read_state()?;
            let record = state.plugins.get(&id).ok_or("plugin is not installed")?;
            let package = manager.configuration_package(record)?;
            let result = read(package.manifest(), &record.active.sha256)?;
            manager.check_epoch(epoch)?;
            Ok(result)
        })
        .await
        .map_err(|_| "plugin settings read task failed".to_string())?
    }

    pub(crate) async fn apply_settings_in_epoch<R, F>(
        &self,
        plugin_id: &str,
        epoch: u64,
        prepare: F,
    ) -> Result<(R, SettingsApplyResult), String>
    where
        F: FnOnce(&PluginManifest, &str) -> Result<(R, SettingsCommit), String> + Send + 'static,
        R: Send + 'static,
    {
        let manager = self.clone();
        let id = plugin_id.to_owned();
        tokio::spawn(async move {
            let _operation = manager.0.operation.lock().await;
            manager.check_epoch(epoch)?;
            let state = manager.read_state()?;
            let record = state.plugins.get(&id).ok_or("plugin is not installed")?;
            let package = manager.configuration_package(record)?;
            let (bytes, entries) = tree_usage(&manager.0.root)?;
            // Reserve staging before prepare has filesystem effects: one bounded
            // ciphertext, the settings directory, and pending/final file entries.
            if bytes + MAX_SETTINGS_FILE_BYTES as u64 > MAX_STORE_BYTES
                || entries + 3 > MAX_STORE_FILES
            {
                return Err("package store quota leaves no room for plugin settings".into());
            }
            let (result, commit) = prepare(package.manifest(), &record.active.sha256)?;
            manager.0.host.commit_in_epoch(epoch, commit)?;
            let restart_error = if record.enabled {
                match manager.stop_owned(&id).await {
                    Ok(()) => match manager.launch(&id, &record.active, epoch).await {
                        Ok(()) => None,
                        Err(error) => {
                            let cleanup = manager.stop_owned(&id).await;
                            Some(match cleanup {
                                Ok(()) => error,
                                Err(cleanup) => format!("{error}; {cleanup}"),
                            })
                        }
                    },
                    Err(error) => Some(error),
                }
            } else {
                None
            };
            manager.check_epoch(epoch)?;
            Ok((result, SettingsApplyResult { restart_error }))
        })
        .await
        .map_err(|_| "plugin settings update task failed".to_string())?
    }

    fn configuration_package(&self, record: &InstalledPlugin) -> Result<VerifiedPackage, String> {
        let package = self.verify_installed(&record.plugin_id, &record.active)?;
        if !package
            .manifest()
            .permissions
            .contains(&PluginPermission::PluginConfiguration)
        {
            return Err("plugin does not have configuration permission".into());
        }
        Ok(package)
    }

    /// Explicit authenticated restoration; opening the store still never starts
    /// a worker. Release the operation lock between identities so queued user
    /// changes take precedence over the remainder of this bounded restore pass.
    pub(crate) async fn restore_enabled_in_epoch(
        &self,
        epoch: u64,
    ) -> Result<Vec<PluginRestoreResult>, String> {
        let manager = self.clone();
        tokio::spawn(async move {
            let ids: Vec<_> = {
                let _operation = manager.0.operation.lock().await;
                manager.check_epoch(epoch)?;
                manager
                    .read_state()?
                    .plugins
                    .into_values()
                    .filter(|record| record.enabled)
                    .map(|record| record.plugin_id)
                    .collect()
            };
            let mut results = Vec::new();
            for id in ids {
                let _operation = manager.0.operation.lock().await;
                manager.check_epoch(epoch)?;
                let Some(record) = manager.read_state()?.plugins.remove(&id) else {
                    continue;
                };
                if !record.enabled {
                    continue;
                }
                let error = manager.restore_one_locked(&record, epoch).await.err();
                manager.check_epoch(epoch)?;
                results.push(PluginRestoreResult {
                    plugin_id: id,
                    error,
                });
            }
            manager.check_epoch(epoch)?;
            Ok(results)
        })
        .await
        .map_err(|_| "package restoration task failed".to_string())?
    }

    /// Explicitly stop owned workers and release the cross-process store lease.
    /// Failed cleanup or unlock keeps the manager and lease available for another attempt.
    pub async fn close(&self) -> Result<(), String> {
        let manager = self.clone();
        tokio::spawn(async move {
            let _operation = manager.0.operation.lock().await;
            if manager.0.closed.load(Ordering::Acquire) {
                return Ok(());
            }
            let owned = manager
                .0
                .owned
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            for id in owned.into_keys() {
                manager.stop_owned(&id).await?;
            }
            let mut lease = manager.0.lease.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(file) = lease.as_ref() {
                // Close alone waits for any descriptors inherited by an unrelated
                // concurrent fork. Reaping owned workers makes explicit unlock safe.
                file.unlock().map_err(io_error)?;
            }
            lease.take();
            manager.0.closed.store(true, Ordering::Release);
            Ok(())
        })
        .await
        .map_err(|_| "package cleanup task failed".to_string())?
    }

    fn check_open(&self) -> Result<(), String> {
        if self.0.closed.load(Ordering::Acquire) {
            return Err("package store is closed".into());
        }
        check_directory(&self.0.root, true)
    }

    fn check_epoch(&self, epoch: u64) -> Result<(), String> {
        self.check_open()?;
        if !self.0.host.is_authorized_epoch(epoch) {
            return Err("package operation authorization changed".into());
        }
        Ok(())
    }

    async fn install_inner(
        &self,
        archive: PathBuf,
        enable: bool,
        epoch: u64,
    ) -> Result<InstalledPlugin, String> {
        let _operation = self.0.operation.lock().await;
        self.check_epoch(epoch)?;
        let bytes = read_regular_file(&archive, MAX_ARCHIVE_BYTES)?;
        let verified = verify_archive_bytes(bytes, &self.0.trust, &self.0.target)?;
        self.install_package_locked(verified, enable, epoch).await
    }

    async fn install_package_locked(
        &self,
        verified: VerifiedPackage,
        enable: bool,
        epoch: u64,
    ) -> Result<InstalledPlugin, String> {
        self.check_epoch(epoch)?;
        let original = self.read_state()?;
        self.recover(&original)?;
        let id = verified.manifest().plugin_id.clone();
        let next = PackageVersion {
            version: verified.manifest().version.clone(),
            sha256: verified.archive_sha256().into(),
            archive_pin: verified.archive_pin(),
        };
        let previous = original.plugins.get(&id).cloned();
        if previous.is_none() && original.plugins.len() >= MAX_PLUGINS {
            return Err("installed plugin limit reached".into());
        }
        if let Some(previous) = &previous {
            // Only retain a rollback whose current installed bytes still pass
            // the publisher, identity, inventory, and payload checks.
            self.verify_installed(&id, &previous.active)?;
            let current_version =
                Version::parse(&previous.active.version).map_err(|_| "invalid stored version")?;
            let next_version =
                Version::parse(&next.version).map_err(|_| "invalid package version")?;
            // Explicit native configuration authorizes the exact pinned bytes,
            // including rebuilt versions or an intentional downgrade. Manual
            // publisher-signed installs retain the newer immutable version rule.
            if !verified.archive_pin()
                && !next.same_content(&previous.active)
                && next_version <= current_version
            {
                return Err(
                    "updates require a newer immutable version; use rollback for retained versions"
                        .into(),
                );
            }
        } else if self
            .0
            .host
            .snapshots()
            .iter()
            .any(|snapshot| snapshot.plugin_id == id)
        {
            return Err("worker identity is already registered outside this package store".into());
        }
        self.stage_package(&verified)?;
        self.check_epoch(epoch)?;
        let rollback = previous.as_ref().and_then(|record| {
            if record.active.same_content(&next) {
                record.rollback.clone()
            } else {
                Some(record.active.clone())
            }
        });
        let record = InstalledPlugin {
            plugin_id: id.clone(),
            active: next,
            rollback,
            enabled: enable,
        };
        self.activate_and_commit(original, record, epoch).await
    }

    async fn restore_one_locked(&self, record: &InstalledPlugin, epoch: u64) -> Result<(), String> {
        let id = &record.plugin_id;
        self.verify_installed(id, &record.active)?;
        self.check_epoch(epoch)?;
        let owns_version = self
            .0
            .owned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            == Some(&record.active.sha256);
        if owns_version
            && self
                .0
                .host
                .snapshots()
                .iter()
                .any(|snapshot| snapshot.plugin_id == *id && snapshot.state == WorkerState::Running)
        {
            // A user may have enabled this identity after the restore pass
            // captured its list. Never replace that already healthy generation.
            return Ok(());
        }
        self.stop_owned(id).await?;
        if let Err(error) = self.launch(id, &record.active, epoch).await {
            if let Err(cleanup_error) = self.stop_owned(id).await {
                return Err(format!("{error}; {cleanup_error}"));
            }
            return Err(error);
        }
        Ok(())
    }

    async fn select_inner(
        &self,
        id: &str,
        rollback: bool,
        epoch: u64,
    ) -> Result<InstalledPlugin, String> {
        let _operation = self.0.operation.lock().await;
        self.check_epoch(epoch)?;
        let original = self.read_state()?;
        let mut record = original
            .plugins
            .get(id)
            .cloned()
            .ok_or("plugin is not installed")?;
        if rollback {
            let previous = record
                .rollback
                .take()
                .ok_or("no rollback version is retained")?;
            record.rollback = Some(std::mem::replace(&mut record.active, previous));
        } else {
            record.enabled = true;
        }
        // Verify selected bytes even for a disabled rollback, before changing state.
        self.verify_installed(id, &record.active)?;
        self.activate_and_commit(original, record, epoch).await
    }

    async fn activate_and_commit(
        &self,
        original: StoreState,
        record: InstalledPlugin,
        epoch: u64,
    ) -> Result<InstalledPlugin, String> {
        let id = &record.plugin_id;
        let was_running = self.0.host.snapshots().iter().any(|snapshot| {
            snapshot.plugin_id == *id
                && matches!(
                    snapshot.state,
                    WorkerState::Starting | WorkerState::Running | WorkerState::Restarting
                )
        });
        // Reverification precedes stopping the existing worker, including archive,
        // signature, exact scoped publisher, and every extracted payload byte.
        let configuration = {
            let package = self.verify_installed(id, &record.active)?;
            if record.enabled {
                self.load_configuration(package.manifest(), epoch)?
            } else {
                None
            }
        };
        self.check_epoch(epoch)?;
        self.stop_owned(id).await?;
        let activation = async {
            if record.enabled {
                self.launch_with_configuration(id, &record.active, epoch, configuration)
                    .await?;
            }
            self.check_epoch(epoch)?;
            let mut next = original.clone();
            next.plugins.insert(id.clone(), record.clone());
            self.0
                .host
                .commit_in_epoch(epoch, || self.write_state(&next))?;
            Ok::<_, String>(next)
        }
        .await;
        match activation {
            Ok(next) => {
                // A committed active version is valid even if orphan cleanup is
                // interrupted; the next open/transaction repeats recovery.
                if let Err(error) = self.recover(&next) {
                    log::warn!("Plugin package orphan cleanup deferred: {error}");
                }
                Ok(record)
            }
            Err(error) => {
                self.stop_owned(id).await?;
                self.write_state(&original)?;
                if was_running && self.0.host.is_authorized_epoch(epoch) {
                    if let Some(previous) = original.plugins.get(id) {
                        if let Err(restore_error) = self.launch(id, &previous.active, epoch).await {
                            return Err(format!(
                                "{error}; previous worker could not restart: {restore_error}"
                            ));
                        }
                    }
                }
                Err(error)
            }
        }
    }

    async fn launch(&self, id: &str, version: &PackageVersion, epoch: u64) -> Result<(), String> {
        self.check_epoch(epoch)?;
        let configuration = {
            let package = self.verify_installed(id, version)?;
            self.load_configuration(package.manifest(), epoch)?
        };
        self.launch_with_configuration(id, version, epoch, configuration)
            .await
    }

    fn load_configuration(
        &self,
        manifest: &PluginManifest,
        epoch: u64,
    ) -> Result<Option<WorkerConfiguration>, String> {
        self.check_epoch(epoch)?;
        let configuration = if manifest
            .permissions
            .contains(&PluginPermission::PluginConfiguration)
        {
            let load = self
                .0
                .configuration
                .as_ref()
                .ok_or("plugin configuration provider is unavailable")?;
            let configuration = load(manifest, epoch)?;
            configuration.validate()?;
            Some(configuration)
        } else {
            None
        };
        super::protocol::HttpVideoGrant::from_manifest_configuration(
            manifest,
            configuration.as_ref(),
        )?;
        super::protocol::LiveViewGrant::from_manifest_configuration(
            manifest,
            configuration.as_ref(),
        )?;
        self.check_epoch(epoch)?;
        Ok(configuration)
    }

    async fn launch_with_configuration(
        &self,
        id: &str,
        version: &PackageVersion,
        epoch: u64,
        configuration: Option<WorkerConfiguration>,
    ) -> Result<(), String> {
        self.check_epoch(epoch)?;
        // The prepared settings belong to this operation's verified immutable
        // package. Reverify its actual files immediately before execution.
        let package = self.verify_installed(id, version)?;
        if configuration.is_some()
            != package
                .manifest()
                .permissions
                .contains(&PluginPermission::PluginConfiguration)
        {
            return Err("plugin configuration permission changed".into());
        }
        let http_video = super::protocol::HttpVideoGrant::from_manifest_configuration(
            package.manifest(),
            configuration.as_ref(),
        )?;
        let live_view = super::protocol::LiveViewGrant::from_manifest_configuration(
            package.manifest(),
            configuration.as_ref(),
        )?;
        let executable = self
            .version_path(id, version)
            .join("payload")
            .join(&package.manifest().entrypoint);
        self.0
            .host
            .start_in_epoch(
                WorkerSpec {
                    plugin_id: id.into(),
                    executable,
                    args: Vec::new(),
                    configuration,
                    http_video,
                    live_view,
                    desktop_notifications: package
                        .manifest()
                        .permissions
                        .contains(&PluginPermission::DesktopNotifications),
                },
                epoch,
            )
            .await
            .map_err(|error| format!("cannot start package worker: {error}"))?;
        self.0
            .owned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.into(), version.sha256.clone());
        let deadline = Instant::now() + STARTUP_BUDGET;
        loop {
            self.check_epoch(epoch)?;
            if let Some(snapshot) = self
                .0
                .host
                .snapshots()
                .into_iter()
                .find(|snapshot| snapshot.plugin_id == id)
            {
                match snapshot.state {
                    WorkerState::Running => return Ok(()),
                    WorkerState::Failed | WorkerState::Stopped => {
                        #[cfg(test)]
                        eprintln!(
                            "Package worker startup failure: state={:?}, code={:?}",
                            snapshot.state, snapshot.last_error
                        );
                        return Err("package worker handshake failed".into());
                    }
                    _ => {}
                }
            }
            if Instant::now() >= deadline {
                return Err("package worker handshake deadline exceeded".into());
            }
            time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn stop_owned(&self, id: &str) -> Result<(), String> {
        if !self
            .0
            .owned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(id)
        {
            if self
                .0
                .host
                .snapshots()
                .iter()
                .any(|snapshot| snapshot.plugin_id == id)
            {
                return Err("worker identity is not owned by this package manager".into());
            }
            return Ok(());
        }
        match self.0.host.remove(id).await {
            Ok(()) | Err(PluginError::UnknownPlugin) => {
                self.0
                    .owned
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(id);
                Ok(())
            }
            Err(error) => Err(format!("package worker could not be reaped: {error}")),
        }
    }

    fn version_path(&self, id: &str, version: &PackageVersion) -> PathBuf {
        self.0.root.join("content").join(id).join(&version.sha256)
    }

    fn stage_package(&self, package: &VerifiedPackage) -> Result<(), String> {
        let version = PackageVersion {
            version: package.manifest().version.clone(),
            sha256: package.archive_sha256().into(),
            archive_pin: package.archive_pin(),
        };
        let destination = self.version_path(&package.manifest().plugin_id, &version);
        if destination.try_exists().map_err(io_error)? {
            self.verify_installed(&package.manifest().plugin_id, &version)?;
            return Ok(());
        }
        let bytes_needed = package.archive_bytes().len() as u64
            + package
                .files()
                .map(|(_, bytes)| bytes.len() as u64)
                .sum::<u64>()
            + MAX_STATE_BYTES as u64;
        let (current_bytes, current_entries) = tree_usage(&self.0.root)?;
        if current_bytes + bytes_needed > MAX_STORE_BYTES {
            return Err("package store disk quota exceeded".into());
        }
        let plugin_directory = destination.parent().ok_or("invalid package directory")?;
        let new_plugin_directory = usize::from(!plugin_directory.try_exists().map_err(io_error)?);
        if current_entries + projected_package_entries(package)? + new_plugin_directory
            > MAX_STORE_FILES
        {
            return Err("package store file quota exceeded".into());
        }
        let staging = self
            .0
            .root
            .join("staging")
            .join(format!("stage-{}", uuid::Uuid::new_v4()));
        create_private_directory(&staging)?;
        let result = (|| {
            write_new_file(
                &staging.join("archive.idplugin"),
                package.archive_bytes(),
                false,
            )?;
            let payload = staging.join("payload");
            create_private_directory(&payload)?;
            for (path, bytes) in package.files() {
                let output = payload.join(path);
                create_payload_parents(&payload, output.parent().ok_or("invalid payload parent")?)?;
                write_new_file(&output, bytes, path == package.manifest().entrypoint)?;
            }
            seal_directories(&payload)?;
            let plugin_directory = destination.parent().ok_or("invalid package directory")?;
            if !plugin_directory.try_exists().map_err(io_error)? {
                create_private_directory(plugin_directory)?;
            }
            check_directory(plugin_directory, true)?;
            fs::rename(&staging, &destination)
                .map_err(|error| format!("cannot publish staged package: {error}"))?;
            // Moving a directory between parents can require write permission
            // on the directory itself. Seal it only at its immutable location.
            seal_directory(&destination)?;
            sync_directory(plugin_directory)?;
            Ok(())
        })();
        if result.is_err() && staging.try_exists().unwrap_or(false) {
            let _ = remove_owned_tree(&staging);
        }
        result
    }

    fn verify_installed(
        &self,
        id: &str,
        version: &PackageVersion,
    ) -> Result<VerifiedPackage, String> {
        #[cfg(test)]
        self.0.verifications.fetch_add(1, Ordering::Relaxed);
        validate_plugin_id(id)?;
        validate_version(version)?;
        let content = self.0.root.join("content");
        check_directory(&content, true)?;
        let plugin = content.join(id);
        check_directory(&plugin, true)?;
        let directory = self.version_path(id, version);
        check_directory(&directory, true)?;
        let bytes = read_regular_file(&directory.join("archive.idplugin"), MAX_ARCHIVE_BYTES)?;
        let package = if version.archive_pin {
            verify_pinned_archive_bytes(
                bytes,
                id,
                &version.version,
                &self.0.target,
                &version.sha256,
            )?
        } else {
            verify_archive_bytes(bytes, &self.0.trust, &self.0.target)?
        };
        if package.archive_sha256() != version.sha256
            || package.manifest().version != version.version
            || package.manifest().plugin_id != id
        {
            return Err("installed package identity or archive digest changed".into());
        }
        let entries: BTreeSet<_> = fs::read_dir(&directory)
            .map_err(io_error)?
            .map(|entry| entry.map(|entry| entry.file_name()).map_err(io_error))
            .collect::<Result<_, _>>()?;
        if entries
            != ["archive.idplugin".into(), "payload".into()]
                .into_iter()
                .collect()
        {
            return Err("unexpected installed package files".into());
        }
        let payload = directory.join("payload");
        check_directory(&payload, true)?;
        let mut actual = BTreeSet::new();
        collect_files(&payload, &payload, &mut actual, &mut 0)?;
        let expected: BTreeSet<_> = package.files().map(|(path, _)| path.to_owned()).collect();
        if actual != expected {
            return Err("installed payload inventory changed".into());
        }
        for (path, bytes) in package.files() {
            if read_regular_file(&payload.join(path), bytes.len())? != bytes {
                return Err("installed payload content changed".into());
            }
        }
        Ok(package)
    }

    fn read_state(&self) -> Result<StoreState, String> {
        self.check_open()?;
        let state: StoreState = serde_json::from_slice(&read_regular_file(
            &self.0.root.join("state.json"),
            MAX_STATE_BYTES,
        )?)
        .map_err(|_| "invalid package store inventory")?;
        if state.schema_version != STORE_SCHEMA || state.plugins.len() > MAX_PLUGINS {
            return Err("unsupported or oversized package store inventory".into());
        }
        for (id, record) in &state.plugins {
            validate_plugin_id(id)?;
            if *id != record.plugin_id {
                return Err("package store identity mismatch".into());
            }
            validate_version(&record.active)?;
            if let Some(rollback) = &record.rollback {
                validate_version(rollback)?;
                if rollback.same_content(&record.active) {
                    return Err("duplicate rollback version".into());
                }
            }
        }
        Ok(state)
    }

    fn write_state(&self, state: &StoreState) -> Result<(), String> {
        self.check_open()?;
        let bytes = serde_json::to_vec(state).map_err(|_| "cannot serialize package inventory")?;
        if bytes.len() > MAX_STATE_BYTES {
            return Err("package inventory is oversized".into());
        }
        atomic_state(&self.0.root, &bytes)
    }

    fn recover(&self, state: &StoreState) -> Result<(), String> {
        // A candidate that could not be reaped can outlive a failed activation.
        // Its uncommitted bytes are not garbage until cleanup proves it stopped.
        for (id, digest) in self
            .0
            .owned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            let referenced = state.plugins.get(id).is_some_and(|record| {
                record.active.sha256 == *digest
                    || record
                        .rollback
                        .as_ref()
                        .is_some_and(|version| version.sha256 == *digest)
            });
            if !referenced {
                return Err(
                    "uncommitted worker still owns package content; cleanup is required".into(),
                );
            }
        }
        tree_size(&self.0.root)?;
        let staging = self.0.root.join("staging");
        check_directory(&staging, true)?;
        for entry in fs::read_dir(&staging).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "invalid staging name")?;
            if !name.starts_with("stage-") || uuid::Uuid::parse_str(&name[6..]).is_err() {
                return Err("unexpected package staging entry".into());
            }
            remove_owned_tree(&entry.path())?;
        }
        for entry in fs::read_dir(&self.0.root).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "invalid store filename")?;
            if let Some(suffix) = name.strip_prefix("state-pending-") {
                uuid::Uuid::parse_str(suffix).map_err(|_| "invalid pending inventory filename")?;
                read_regular_file(&entry.path(), MAX_STATE_BYTES)?;
                fs::remove_file(entry.path()).map_err(io_error)?;
            }
        }
        let content = self.0.root.join("content");
        check_directory(&content, true)?;
        for entry in fs::read_dir(&content).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let id = entry
                .file_name()
                .into_string()
                .map_err(|_| "invalid package identity directory")?;
            validate_plugin_id(&id)?;
            check_directory(&entry.path(), true)?;
            let keep: BTreeSet<_> = state
                .plugins
                .get(&id)
                .into_iter()
                .flat_map(|record| {
                    std::iter::once(record.active.sha256.clone()).chain(
                        record
                            .rollback
                            .as_ref()
                            .map(|version| version.sha256.clone()),
                    )
                })
                .collect();
            for version in fs::read_dir(entry.path()).map_err(io_error)? {
                let version = version.map_err(io_error)?;
                let hash = version
                    .file_name()
                    .into_string()
                    .map_err(|_| "invalid package version directory")?;
                if !valid_digest(&hash) {
                    return Err("invalid package version directory".into());
                }
                check_directory(&version.path(), true)?;
                if !keep.contains(&hash) {
                    remove_owned_tree(&version.path())?;
                }
            }
            if fs::read_dir(entry.path())
                .map_err(io_error)?
                .next()
                .is_none()
            {
                fs::remove_dir(entry.path()).map_err(io_error)?;
            }
        }
        for (id, record) in &state.plugins {
            for version in std::iter::once(&record.active).chain(record.rollback.as_ref()) {
                check_directory(&self.version_path(id, version), true)?;
            }
        }
        tree_size(&self.0.root)?;
        sync_directory(&self.0.root)
    }
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_version(version: &PackageVersion) -> Result<(), String> {
    if version.version.len() > 128
        || Version::parse(&version.version).is_err()
        || !valid_digest(&version.sha256)
    {
        return Err("invalid stored package version".into());
    }
    Ok(())
}

fn io_error(error: std::io::Error) -> String {
    format!("package store I/O: {error}")
}

fn is_link(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn check_directory(path: &Path, private: bool) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if !metadata.is_dir() || is_link(&metadata) {
        return Err("package store directory is a link or non-directory".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if private && metadata.permissions().mode() & 0o077 != 0 {
            return Err("package store directory is not private".into());
        }
    }
    #[cfg(not(unix))]
    let _ = private;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err("reparse point in package store".into());
        }
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<(), String> {
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    builder.create(path).map_err(io_error)?;
    sync_directory(path.parent().ok_or("invalid directory parent")?)
}

fn open_store(root: &Path) -> Result<(PathBuf, File), String> {
    if !root.is_absolute() || root.parent().is_none() {
        return Err("package store needs a dedicated absolute directory".into());
    }
    if !root.try_exists().map_err(io_error)? {
        create_private_directory(root)?;
    }
    check_directory(root, true)?;
    let root = fs::canonicalize(root).map_err(io_error)?;
    let marker = root.join(STORE_SENTINEL);
    if !marker.try_exists().map_err(io_error)? {
        // An empty root or its single empty lock file is the only pre-marker
        // initialization state. Never reinterpret unrelated data as our store.
        for entry in fs::read_dir(&root).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            if entry.file_name() != "store.lock" || !read_regular_file(&entry.path(), 0)?.is_empty()
            {
                return Err(
                    "refusing to adopt a nonempty unowned directory as a package store".into(),
                );
            }
        }
    }
    let lock_path = root.join("store.lock");
    if let Ok(metadata) = fs::symlink_metadata(&lock_path) {
        if !metadata.is_file() || is_link(&metadata) {
            return Err("invalid package store lock file".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() != 1 {
                return Err("linked package store lock file".into());
            }
        }
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000);
    }
    let lease = options.open(&lock_path).map_err(io_error)?;
    let opened = lease.metadata().map_err(io_error)?;
    if !opened.is_file() {
        return Err("invalid opened store lease".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.nlink() != 1 {
            return Err("linked opened store lease".into());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if opened.file_attributes() & 0x400 != 0 {
            return Err("reparse point in opened store lease".into());
        }
    }
    lease
        .try_lock()
        .map_err(|_| "package store is already open by another manager or process")?;
    if marker.try_exists().map_err(io_error)? {
        check_directory(&marker, true)?;
        if fs::read_dir(&marker).map_err(io_error)?.next().is_some() {
            return Err("invalid package store ownership sentinel".into());
        }
    } else {
        // Directory creation is atomic: interruption cannot publish a partial
        // ownership string that permanently strands an otherwise empty store.
        create_private_directory(&marker)?;
    }
    if !root.join("state.json").try_exists().map_err(io_error)? {
        // Resume interrupted initial creation only while no package data exists.
        // A missing inventory beside existing content is corruption, never an
        // invitation to erase packages or infer an active version.
        for name in ["content", "staging"] {
            let directory = root.join(name);
            if directory.try_exists().map_err(io_error)? {
                check_directory(&directory, true)?;
                if fs::read_dir(&directory).map_err(io_error)?.next().is_some() {
                    return Err("missing inventory beside nonempty package data".into());
                }
            } else {
                create_private_directory(&directory)?;
            }
        }
        atomic_state(
            &root,
            &serde_json::to_vec(&StoreState::default())
                .map_err(|_| "cannot initialize package inventory")?,
        )?;
    }
    Ok((root, lease))
}

fn write_new_file(path: &Path, bytes: &[u8], executable: bool) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if executable { 0o500 } else { 0o400 });
    }
    #[cfg(not(unix))]
    let _ = executable;
    let mut file = options.open(path).map_err(io_error)?;
    file.write_all(bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)
}

fn atomic_state(root: &Path, bytes: &[u8]) -> Result<(), String> {
    let pending = root.join(format!("state-pending-{}", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&pending).map_err(io_error)?;
    let result = (|| {
        file.write_all(bytes).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        drop(file);
        // rename replaces one file atomically; no delete-before-rename gap.
        fs::rename(&pending, root.join("state.json")).map_err(io_error)?;
        sync_directory(root)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&pending);
    }
    result
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        File::open(path)
            .map_err(io_error)?
            .sync_all()
            .map_err(io_error)?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn create_payload_parents(root: &Path, parent: &Path) -> Result<(), String> {
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| "payload parent escaped store")?;
    let mut current = root.to_owned();
    for component in relative.components() {
        current.push(component);
        if !current.try_exists().map_err(io_error)? {
            create_private_directory(&current)?;
        }
        check_directory(&current, true)?;
    }
    Ok(())
}

fn seal_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o500)).map_err(io_error)?;
    }
    sync_directory(path)
}

fn seal_directories(path: &Path) -> Result<(), String> {
    for entry in fs::read_dir(path).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(io_error)?;
        if is_link(&metadata) {
            return Err("link in staged package".into());
        }
        if metadata.is_dir() {
            seal_directories(&entry.path())?;
        }
    }
    seal_directory(path)
}

fn collect_files(
    root: &Path,
    path: &Path,
    files: &mut BTreeSet<String>,
    count: &mut usize,
) -> Result<(), String> {
    check_directory(path, true)?;
    for entry in fs::read_dir(path).map_err(io_error)? {
        *count += 1;
        if *count > MAX_STORE_FILES {
            return Err("package tree has too many files".into());
        }
        let entry = entry.map_err(io_error)?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(io_error)?;
        if is_link(&metadata) {
            return Err("link in installed package".into());
        }
        if metadata.is_dir() {
            collect_files(root, &entry.path(), files, count)?;
        } else if metadata.is_file() {
            let relative = entry
                .path()
                .strip_prefix(root)
                .map_err(|_| "package path escaped payload")?
                .to_str()
                .ok_or("invalid package filename")?
                .replace('\\', "/");
            files.insert(relative);
        } else {
            return Err("special file in installed package".into());
        }
    }
    Ok(())
}

fn projected_package_entries(package: &VerifiedPackage) -> Result<usize, String> {
    // ZIP inventories list files, while extraction also creates every unique
    // parent directory. Count those prefixes before touching the filesystem.
    // Four more entries cover the staging/version root, archive, payload root,
    // and the temporary inventory file needed for the final atomic commit.
    const FIXED_ENTRIES: usize = 4;
    let mut paths = BTreeSet::new();
    for (path, _) in package.files() {
        paths.insert(path);
        for (separator, _) in path.match_indices('/') {
            paths.insert(&path[..separator]);
            if paths.len() + FIXED_ENTRIES > MAX_STORE_FILES {
                return Err("package store file quota exceeded".into());
            }
        }
    }
    Ok(paths.len() + FIXED_ENTRIES)
}

fn tree_size(root: &Path) -> Result<u64, String> {
    tree_usage(root).map(|(bytes, _)| bytes)
}

fn tree_usage(root: &Path) -> Result<(u64, usize), String> {
    fn walk(path: &Path, count: &mut usize, size: &mut u64) -> Result<(), String> {
        let metadata = fs::symlink_metadata(path).map_err(io_error)?;
        *count += 1;
        if *count > MAX_STORE_FILES || is_link(&metadata) {
            return Err("package store contains a link or exceeds file limit".into());
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(path).map_err(io_error)? {
                walk(&entry.map_err(io_error)?.path(), count, size)?;
            }
        } else if metadata.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.nlink() != 1 {
                    return Err("hard link in package store".into());
                }
            }
            *size = size
                .checked_add(metadata.len())
                .ok_or("package store size overflow")?;
            if *size > MAX_STORE_BYTES {
                return Err("package store disk quota exceeded".into());
            }
        } else {
            return Err("special file in package store".into());
        }
        Ok(())
    }
    let mut size = 0;
    let mut count = 0;
    walk(root, &mut count, &mut size)?;
    Ok((size, count))
}

fn remove_owned_tree(root: &Path) -> Result<(), String> {
    // Inspect the entire tree before making any directory writable or deleting
    // entries. In particular, never traverse a symlink out of the owned store.
    tree_size(root)?;
    fn remove(path: &Path) -> Result<(), String> {
        let metadata = fs::symlink_metadata(path).map_err(io_error)?;
        if !metadata.is_dir() || is_link(&metadata) {
            return Err("invalid owned package directory".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io_error)?;
        }
        for entry in fs::read_dir(path).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let metadata = fs::symlink_metadata(entry.path()).map_err(io_error)?;
            if is_link(&metadata) {
                return Err("link in owned package directory".into());
            }
            if metadata.is_dir() {
                remove(&entry.path())?;
            } else if metadata.is_file() {
                fs::remove_file(entry.path()).map_err(io_error)?;
            } else {
                return Err("special file in owned package directory".into());
            }
        }
        fs::remove_dir(path).map_err(io_error)
    }
    remove(root)
}

#[cfg(test)]
#[path = "installer_tests.rs"]
mod tests;
