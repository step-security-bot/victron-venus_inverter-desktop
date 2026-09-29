//! Configuration-owned package downloads, separate from worker settings/secrets.

use super::super::download::{download_archive, validate_declarations};
use super::super::package::verify_pinned_archive_bytes;
use super::super::runtime::WorkerState;
use super::management::declaration_revision;
use super::{PackageApplication, PackageManager};
use crate::plugin_config::DesktopPluginConfig;
use serde::Serialize;
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ConfiguredStatus {
    plugin_id: String,
    version: String,
    enabled: bool,
    local: bool,
    declaration_revision: String,
    state: String,
    error: Option<String>,
}

#[derive(Default)]
struct Desired {
    declarations: Vec<DesktopPluginConfig>,
    statuses: BTreeMap<String, ConfiguredStatus>,
    error: Option<String>,
}

#[derive(Default)]
pub(super) struct Reconciliation {
    desired: Mutex<Desired>,
    pub(super) operation: tokio::sync::Mutex<()>,
}

impl Reconciliation {
    pub(super) fn statuses(&self) -> Vec<ConfiguredStatus> {
        self.desired
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .statuses
            .values()
            .cloned()
            .collect()
    }

    pub(super) fn error(&self) -> Option<String> {
        self.desired
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .error
            .clone()
    }

    pub(super) fn contains(&self, id: &str) -> bool {
        self.desired
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .statuses
            .contains_key(id)
    }
}

impl PackageApplication {
    /// The bridge revokes the previous epoch before replacing these declarations.
    /// Configuration data comes only from the native encrypted config store.
    pub(crate) fn configure(&self, configuration: Result<Vec<DesktopPluginConfig>, String>) {
        let configuration = configuration.and_then(|declarations| {
            validate_declarations(&declarations)?;
            Ok(declarations)
        });
        let mut desired = Desired::default();
        match configuration {
            Ok(declarations) => {
                for declaration in &declarations {
                    desired.statuses.insert(
                        declaration.plugin_id.clone(),
                        ConfiguredStatus {
                            plugin_id: declaration.plugin_id.clone(),
                            version: declaration.version.clone(),
                            enabled: declaration.enabled,
                            local: false,
                            declaration_revision: declaration_revision(declaration),
                            state: "pending".into(),
                            error: None,
                        },
                    );
                }
                desired.declarations = declarations;
            }
            Err(error) => desired.error = Some(error),
        }
        *self
            .0
            .reconciliation
            .desired
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = desired;
        (self.0.changed)();
    }

    /// Called under the native save gate and current host epoch after persistence.
    pub(crate) fn group_desired_changed(&self, members: &[String], enabled: bool) {
        let mut desired = self
            .0
            .reconciliation
            .desired
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Desired {
            declarations,
            statuses,
            ..
        } = &mut *desired;
        for declaration in declarations {
            if members.contains(&declaration.plugin_id) {
                declaration.enabled = enabled;
                if let Some(status) = statuses.get_mut(&declaration.plugin_id) {
                    status.enabled = enabled;
                    status.declaration_revision = declaration_revision(declaration);
                    status.state = "pending".into();
                    status.error = None;
                }
            }
        }
    }

    /// Called with persisted configuration under the host's current epoch.
    pub(crate) fn plugin_desired_changed(&self, id: &str, change: super::PluginDesiredChange) {
        if let super::PluginDesiredChange::Enabled(enabled) = change {
            self.group_desired_changed(&[id.to_owned()], enabled);
            return;
        }
        let mut desired = self
            .0
            .reconciliation
            .desired
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        change.apply(&mut desired.declarations, id);
        desired.statuses.remove(id);
    }

    pub(super) fn require_unmanaged(&self, id: &str) -> Result<(), String> {
        if self.0.reconciliation.contains(id) {
            Err("This plugin is managed by desktop_plugins in the application configuration".into())
        } else {
            Ok(())
        }
    }

    pub(super) fn configured_status(
        &self,
        id: &str,
        epoch: u64,
        state: &str,
        error: Option<String>,
    ) {
        let _ = self.0.host.commit_in_epoch(epoch, || {
            if let Some(status) = self
                .0
                .reconciliation
                .desired
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .statuses
                .get_mut(id)
            {
                status.state = state.into();
                status.error = error;
            }
            Ok(())
        });
        (self.0.changed)();
    }

    pub(super) async fn reconcile(
        &self,
        manager: PackageManager,
        epoch: u64,
    ) -> Result<(), String> {
        self.reconcile_with(
            manager,
            epoch,
            |url| async move { download_archive(&url).await },
        )
        .await
    }

    pub(super) async fn reconcile_with<F, Download>(
        &self,
        manager: PackageManager,
        epoch: u64,
        download: F,
    ) -> Result<(), String>
    where
        F: Fn(String) -> Download,
        Download: Future<Output = Result<Vec<u8>, String>>,
    {
        let _operation = self.0.reconciliation.operation.lock().await;
        self.check_epoch(epoch)?;
        let declarations = {
            let desired = self
                .0
                .reconciliation
                .desired
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(error) = &desired.error {
                return Err(error.clone());
            }
            desired.declarations.clone()
        };
        // Apply disabled intent before restoring any cached worker. Network
        // availability must never determine whether an unwanted worker starts.
        let installed = manager.list().await?;
        for declaration in &declarations {
            if !declaration.enabled
                && installed
                    .iter()
                    .any(|record| record.plugin_id == declaration.plugin_id && record.enabled)
            {
                self.check_epoch(epoch)?;
                let result = manager
                    .disable_in_epoch(&declaration.plugin_id, epoch)
                    .await
                    .map(|_| ());
                self.record_result(&declaration.plugin_id, epoch, &result);
                result?;
            }
        }
        // Healthy cached workers resume before any potentially slow download.
        for result in manager.restore_enabled_in_epoch(epoch).await? {
            self.record_result(&result.plugin_id, epoch, &result.error.map_or(Ok(()), Err));
        }
        for declaration in declarations {
            self.check_epoch(epoch)?;
            let result = self
                .reconcile_one(&manager, &declaration, epoch, &download)
                .await;
            self.check_epoch(epoch)?;
            self.configured_status(
                &declaration.plugin_id,
                epoch,
                if result.is_err() {
                    "failed"
                } else if declaration.enabled {
                    "ready"
                } else {
                    "disabled"
                },
                result.as_ref().err().cloned(),
            );
            self.record_result(&declaration.plugin_id, epoch, &result);
        }
        self.check_epoch(epoch)
    }

    async fn reconcile_one<F, Download>(
        &self,
        manager: &PackageManager,
        declaration: &DesktopPluginConfig,
        epoch: u64,
        download: &F,
    ) -> Result<(), String>
    where
        F: Fn(String) -> Download,
        Download: Future<Output = Result<Vec<u8>, String>>,
    {
        let id = &declaration.plugin_id;
        let artifact = declaration
            .artifacts
            .get(&self.0.target)
            .ok_or("No configured archive is available for this desktop platform")?;
        let details = manager.details_in_epoch(id, epoch).await?;
        self.check_epoch(epoch)?;
        if let Some(details) = &details {
            if let Some(error) = &details.error {
                return Err(format!("Installed plugin requires repair: {error}"));
            }
        }
        let local = if details.is_some() {
            let root = self
                .0
                .local_build_root
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            root.map(|root| {
                super::super::local_builds::selected(&root, &self.0.target, id, &artifact.sha256)
            })
            .transpose()?
            .flatten()
        } else {
            None
        };
        let version = local
            .as_ref()
            .map_or(&declaration.version, |archive| &archive.package.version);
        let digest = local
            .as_ref()
            .map_or(&artifact.sha256, |archive| &archive.package.sha256);
        self.0.host.commit_in_epoch(epoch, || {
            if let Some(status) = self
                .0
                .reconciliation
                .desired
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .statuses
                .get_mut(id)
            {
                status.local = local.is_some();
                status.version = version.clone();
            }
            Ok(())
        })?;
        if let Some(details) = &details {
            if details.record.active.version == *version && details.record.active.sha256 == *digest
            {
                if !declaration.enabled {
                    return Ok(());
                }
                if details.record.enabled {
                    return if self.0.host.snapshots().iter().any(|worker| {
                        worker.plugin_id == *id && worker.state == WorkerState::Running
                    }) {
                        Ok(())
                    } else {
                        Err("Configured plugin could not start; review its settings".into())
                    };
                }
                manager.enable_in_epoch(id, epoch).await?;
                return Ok(());
            }
        }
        self.configured_status(id, epoch, "downloading", None);
        // Dropping a revoked download cancels its response/body stream. No
        // installer lock or partially staged package exists during this wait.
        let bytes = match &local {
            Some(archive) => archive.read()?,
            None => {
                self.await_current(epoch, download(artifact.url.clone()))
                    .await?
            }
        };
        self.check_epoch(epoch)?;
        let expected_id = id.clone();
        let version = version.clone();
        let target = self.0.target.clone();
        let digest = digest.clone();
        let package = tokio::task::spawn_blocking(move || {
            verify_pinned_archive_bytes(bytes, &expected_id, &version, &target, &digest)
        })
        .await
        .map_err(|_| "Plugin archive validation task failed")??;
        self.check_epoch(epoch)?;
        self.configured_status(id, epoch, "installing", None);
        // Existing enabled versions use the installer's transactional activation
        // and rollback. First installs remain configurable if credentials are absent.
        let activate_transactionally = declaration.enabled
            && details
                .as_ref()
                .is_some_and(|details| details.record.enabled);
        manager
            .install_verified_in_epoch(package, activate_transactionally, epoch)
            .await?;
        self.check_epoch(epoch)?;
        if declaration.enabled && !activate_transactionally {
            manager.enable_in_epoch(id, epoch).await?;
        }
        Ok(())
    }

    async fn await_current<T>(
        &self,
        epoch: u64,
        future: impl Future<Output = Result<T, String>>,
    ) -> Result<T, String> {
        tokio::pin!(future);
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        loop {
            tokio::select! {
                biased;
                _ = tick.tick() => self.check_epoch(epoch)?,
                result = &mut future => {
                    self.check_epoch(epoch)?;
                    return result;
                }
            }
        }
    }
}
