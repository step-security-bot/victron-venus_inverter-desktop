//! App session and window authority around the desktop worker host.

use super::application::{
    remove_configured_declaration, ManagerSnapshot, PackageApplication, PackagePreview,
    PluginDesiredChange, RetainedPluginData, SettingsSaveResult,
};
use super::media::MediaService;
use super::publishers::embedded_trust;
use super::runtime::{PluginHost, PluginSnapshot, WorkerState};
use super::settings::PluginSettingsView;
use crate::auth;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{Emitter, Manager, State};

pub(crate) struct DesktopPlugins {
    host: PluginHost,
    packages: PackageApplication,
    media: MediaService,
    session_gate: Mutex<()>,
    exit: ExitGate,
    #[cfg(feature = "native-media-smoke")]
    native_media_smoke: bool,
}

#[derive(Default)]
struct ExitGate {
    started: AtomicBool,
    finished: AtomicBool,
}

impl ExitGate {
    /// Returns whether to prevent exit and whether this request owns cleanup.
    fn request(&self) -> (bool, bool) {
        if self.finished.load(Ordering::Acquire) {
            return (false, false);
        }
        (true, !self.started.swap(true, Ordering::AcqRel))
    }
}

fn trusted_window(label: &str) -> bool {
    matches!(label, "main" | "config")
}

fn require_access(
    app: &tauri::AppHandle,
    window: &tauri::WebviewWindow,
    state: &DesktopPlugins,
) -> Result<(), String> {
    if !trusted_window(window.label()) || state.exit.started.load(Ordering::Acquire) {
        return Err("Plugin access is unavailable".into());
    }
    auth::require_session(app).inspect_err(|_| authentication_changed(app))
}

fn management_window(label: &str) -> bool {
    label == "config"
}

fn management_epoch(
    app: &tauri::AppHandle,
    window: &tauri::WebviewWindow,
    state: &DesktopPlugins,
) -> Result<u64, String> {
    let epoch = state.host.authority_epoch();
    require_access(app, window, state)?;
    if !management_window(window.label()) {
        return Err("Packages can only be managed from the settings window".into());
    }
    if !state.host.is_authorized_epoch(epoch) {
        return Err("Plugin session changed".into());
    }
    Ok(epoch)
}

fn finish_management(
    app: &tauri::AppHandle,
    window: &tauri::WebviewWindow,
    state: &DesktopPlugins,
    epoch: u64,
) -> Result<(), String> {
    if management_epoch(app, window, state)? != epoch {
        return Err("Plugin session changed".into());
    }
    Ok(())
}

#[tauri::command]
pub(crate) async fn get_plugin_manager_snapshot(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, DesktopPlugins>,
) -> Result<ManagerSnapshot, String> {
    let epoch = management_epoch(&app, &window, &state)?;
    let snapshot = state.packages.snapshot(epoch).await?;
    finish_management(&app, &window, &state, epoch)?;
    Ok(snapshot)
}

pub(crate) async fn portable_modules(
    app: &tauri::AppHandle,
) -> Result<crate::module_config::ModuleNamespaces, String> {
    auth::require_session(app)?;
    let state = app.state::<DesktopPlugins>();
    let epoch = state.host.authority_epoch();
    let modules = state.packages.export_modules(epoch).await?;
    auth::require_session(app)?;
    if !state.host.is_authorized_epoch(epoch) {
        return Err("Plugin session changed".into());
    }
    Ok(modules)
}

/// Restore against one verified, immutable SettingsStore snapshot. Core config
/// locking happens only inside the synchronous package-operation callback.
pub(crate) async fn restore_configuration(
    app: &tauri::AppHandle,
    window: &tauri::WebviewWindow,
    content: String,
) -> Result<(), String> {
    let state = app.state::<DesktopPlugins>();
    let epoch = management_epoch(app, window, &state)?;
    let packages = state.packages.clone();
    let owned_app = app.clone();
    let owned_window = window.clone();
    let changed =
        packages
            .with_portable_modules(epoch, move |installed| {
                let _update = crate::CONFIG_UPDATE_GATE
                    .lock()
                    .map_err(|_| "Config update lock failed")?;
                let state = owned_app.state::<DesktopPlugins>();
                finish_management(&owned_app, &owned_window, &state, epoch)?;
                let previous = crate::load_config(&owned_app)?;
                let next =
                    crate::module_config::restore_with_installed(&content, &previous, &installed)?;
                super::download::validate_declarations(&next.desktop_plugins)?;
                state
                    .host
                    .commit_in_epoch(epoch, || crate::save_config_encrypted(&owned_app, &next))?;
                Ok(previous.desktop_plugins != next.desktop_plugins
                    || previous.modules != next.modules)
            })
            .await?;
    if changed {
        configuration_changed(app);
    }
    Ok(())
}

#[tauri::command]
pub(crate) fn get_plugin_settings_choices(
    plugin_id: String,
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, DesktopPlugins>,
) -> Result<super::settings_choices::Snapshot, String> {
    let epoch = management_epoch(&app, &window, &state)?;
    let result = state
        .host
        .settings_choices(&plugin_id)
        .map_err(|error| error.to_string())?;
    finish_management(&app, &window, &state, epoch)?;
    Ok(result)
}

#[tauri::command]
pub(crate) async fn retry_configured_plugins(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, DesktopPlugins>,
) -> Result<(), String> {
    let epoch = management_epoch(&app, &window, &state)?;
    state.packages.restore(epoch).await?;
    finish_management(&app, &window, &state, epoch)
}

/// Declaring native code is restricted to the authenticated configuration window.
/// Ordinary core settings saves, including mobile and first-run setup, need no
/// plugin authority when they merely preserve the existing declarations.
pub(crate) fn save_configuration(
    app: &tauri::AppHandle,
    window: &tauri::WebviewWindow,
    previous: &crate::FullConfig,
    next: &crate::FullConfig,
) -> Result<(), String> {
    if previous.desktop_plugins == next.desktop_plugins {
        return crate::save_config_encrypted(app, next);
    }
    let state = app.state::<DesktopPlugins>();
    let epoch = management_epoch(app, window, &state)?;
    super::download::validate_declarations(&next.desktop_plugins)?;
    state
        .host
        .commit_in_epoch(epoch, || crate::save_config_encrypted(app, next))
}

pub(crate) fn configuration_changed(app: &tauri::AppHandle) {
    authentication_changed(app);
}

#[tauri::command]
pub(crate) async fn preview_plugin_package(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, DesktopPlugins>,
) -> Result<Option<PackagePreview>, String> {
    use tauri_plugin_dialog::{DialogExt, FilePath};
    let epoch = management_epoch(&app, &window, &state)?;
    let token = state.packages.begin_selection(window.label(), epoch)?;
    let result = async {
        let (selected, result) = tokio::sync::oneshot::channel();
        app.dialog()
            .file()
            .set_parent(&window)
            .set_title("Select a signed desktop plugin package")
            .add_filter("Inverter Desktop plugin", &["idplugin"])
            .pick_file(move |file| {
                let _ = selected.send(file);
            });
        let file = result
            .await
            .map_err(|_| "Package file selection was interrupted")?;
        finish_management(&app, &window, &state, epoch)?;
        match file {
            None => Ok(None),
            Some(FilePath::Path(path)) => state
                .packages
                .finish_selection(&token, window.label(), epoch, path)
                .await
                .map(Some),
            Some(_) => Err("Select a local plugin package file".into()),
        }
    }
    .await;
    if !matches!(result, Ok(Some(_))) {
        state.packages.discard(&token, window.label(), epoch);
    }
    let preview = result?;
    if let Err(error) = finish_management(&app, &window, &state, epoch) {
        state.packages.discard(&token, window.label(), epoch);
        return Err(error);
    }
    Ok(preview)
}

#[tauri::command]
pub(crate) fn discard_plugin_package(
    token: String,
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, DesktopPlugins>,
) -> Result<(), String> {
    let epoch = management_epoch(&app, &window, &state)?;
    state.packages.discard(&token, window.label(), epoch);
    Ok(())
}

#[tauri::command]
pub(crate) async fn install_plugin_package(
    token: String,
    enable: bool,
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, DesktopPlugins>,
) -> Result<(), String> {
    let epoch = management_epoch(&app, &window, &state)?;
    state
        .packages
        .install_review(&token, window.label(), epoch, enable)
        .await?;
    finish_management(&app, &window, &state, epoch)
}

#[tauri::command]
pub(crate) async fn set_plugin_enabled(
    plugin_id: String,
    enabled: bool,
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, DesktopPlugins>,
) -> Result<(), String> {
    let epoch = management_epoch(&app, &window, &state)?;
    let persist_app = app.clone();
    let persist_window = window.clone();
    state
        .packages
        .set_enabled_with_config(&plugin_id, enabled, epoch, move |id, change, packages| {
            persist_plugin_change(&persist_app, &persist_window, id, change, packages, epoch)
        })
        .await?;
    finish_management(&app, &window, &state, epoch)
}

fn persist_plugin_change(
    app: &tauri::AppHandle,
    window: &tauri::WebviewWindow,
    id: &str,
    change: PluginDesiredChange,
    packages: &PackageApplication,
    epoch: u64,
) -> Result<(), String> {
    // Match core saves' lock order and reload their latest unrelated fields.
    // The application retains its reconciliation gate; never await here.
    let _save = crate::CONFIG_UPDATE_GATE
        .lock()
        .map_err(|_| "Config update lock failed")?;
    let state = app.state::<DesktopPlugins>();
    finish_management(app, window, &state, epoch)?;
    let mut config = crate::load_config(app)?;
    let changed = change.apply(&mut config.desktop_plugins, id);
    state.host.commit_in_epoch(epoch, || {
        if changed {
            crate::save_config_encrypted(app, &config)?;
        }
        packages.plugin_desired_changed(id, change);
        if changed {
            let _ = app.emit(
                "plugin-configuration-changed",
                serde_json::json!({"desktop_plugins": config.desktop_plugins}),
            );
        }
        Ok(())
    })
}

/// Group controls are dashboard operations and are also available in settings.
fn group_epoch(
    app: &tauri::AppHandle,
    window: &tauri::WebviewWindow,
    state: &DesktopPlugins,
) -> Result<u64, String> {
    let epoch = state.host.authority_epoch();
    require_access(app, window, state)?;
    if !state.host.is_authorized_epoch(epoch) {
        return Err("Plugin session changed".into());
    }
    Ok(epoch)
}

#[tauri::command]
pub(crate) async fn get_plugin_groups(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, DesktopPlugins>,
) -> Result<Vec<super::application::PluginGroupSnapshot>, String> {
    let epoch = group_epoch(&app, &window, &state)?;
    let groups = state.packages.groups(epoch).await?;
    if group_epoch(&app, &window, &state)? != epoch {
        return Err("Plugin session changed".into());
    }
    Ok(groups)
}

#[tauri::command]
pub(crate) async fn set_plugin_group_enabled(
    group_id: String,
    enabled: bool,
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, DesktopPlugins>,
) -> Result<(), String> {
    let epoch = group_epoch(&app, &window, &state)?;
    let persist_app = app.clone();
    let persist_window = window.clone();
    state
        .packages
        .set_group_enabled(group_id, enabled, epoch, move |members, packages| {
            // Match save_config's lock ordering; never await with this native guard.
            let _save = crate::CONFIG_UPDATE_GATE
                .lock()
                .map_err(|_| "Config update lock failed")?;
            let state = persist_app.state::<DesktopPlugins>();
            if group_epoch(&persist_app, &persist_window, &state)? != epoch {
                return Err("Plugin session changed".into());
            }
            let mut config = crate::load_config(&persist_app)?;
            for declaration in &mut config.desktop_plugins {
                if members.contains(&declaration.plugin_id) {
                    declaration.enabled = enabled;
                }
            }
            state.host.commit_in_epoch(epoch, || {
                crate::save_config_encrypted(&persist_app, &config)?;
                packages.group_desired_changed(members, enabled);
                let _ = persist_app.emit(
                    "plugin-configuration-changed",
                    serde_json::json!({"desktop_plugins":config.desktop_plugins}),
                );
                Ok(())
            })
        })
        .await?;
    if group_epoch(&app, &window, &state)? != epoch {
        return Err("Plugin session changed".into());
    }
    Ok(())
}

#[tauri::command]
pub(crate) async fn rollback_plugin_package(
    plugin_id: String,
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, DesktopPlugins>,
) -> Result<(), String> {
    let epoch = management_epoch(&app, &window, &state)?;
    state.packages.rollback(&plugin_id, epoch).await?;
    finish_management(&app, &window, &state, epoch)
}

#[tauri::command]
pub(crate) async fn remove_configured_plugin(
    plugin_id: String,
    expected_declaration_revision: String,
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, DesktopPlugins>,
) -> Result<(), String> {
    let epoch = management_epoch(&app, &window, &state)?;
    let persist_app = app.clone();
    let persist_window = window.clone();
    state
        .packages
        .remove_configured_with_config(
            &plugin_id,
            &expected_declaration_revision,
            epoch,
            move |id, expected_revision, packages| {
                // Keep the same operation -> config -> authority order as
                // managed uninstall. No asynchronous work holds the save gate.
                let _save = crate::CONFIG_UPDATE_GATE
                    .lock()
                    .map_err(|_| "Config update lock failed")?;
                let state = persist_app.state::<DesktopPlugins>();
                finish_management(&persist_app, &persist_window, &state, epoch)?;
                let mut config = crate::load_config(&persist_app)?;
                remove_configured_declaration(&mut config.desktop_plugins, id, expected_revision)?;
                state.host.commit_in_epoch(epoch, || {
                    crate::save_config_encrypted(&persist_app, &config)?;
                    packages.plugin_desired_changed(id, PluginDesiredChange::Removed);
                    let _ = persist_app.emit(
                        "plugin-configuration-changed",
                        serde_json::json!({"desktop_plugins": config.desktop_plugins}),
                    );
                    Ok(())
                })
            },
        )
        .await?;
    finish_management(&app, &window, &state, epoch)
}

#[tauri::command]
pub(crate) async fn uninstall_plugin_package(
    plugin_id: String,
    delete_settings: Option<bool>,
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, DesktopPlugins>,
) -> Result<(), String> {
    let epoch = management_epoch(&app, &window, &state)?;
    let persist_app = app.clone();
    let persist_window = window.clone();
    state
        .packages
        .uninstall_with_config(
            &plugin_id,
            delete_settings.unwrap_or(false),
            epoch,
            move |id, change, packages| {
                persist_plugin_change(&persist_app, &persist_window, id, change, packages, epoch)
            },
        )
        .await?;
    finish_management(&app, &window, &state, epoch)
}

#[tauri::command]
pub(crate) async fn get_plugin_settings(
    plugin_id: String,
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, DesktopPlugins>,
) -> Result<PluginSettingsView, String> {
    let epoch = management_epoch(&app, &window, &state)?;
    let settings = state.packages.get_settings(&plugin_id, epoch).await?;
    finish_management(&app, &window, &state, epoch)?;
    Ok(settings)
}

#[tauri::command]
pub(crate) async fn get_retained_plugin_data(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, DesktopPlugins>,
) -> Result<RetainedPluginData, String> {
    let epoch = management_epoch(&app, &window, &state)?;
    let data = state.packages.retained_data(epoch).await?;
    finish_management(&app, &window, &state, epoch)?;
    Ok(data)
}

#[tauri::command]
pub(crate) async fn delete_retained_plugin_data(
    record_id: String,
    revision: String,
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, DesktopPlugins>,
) -> Result<(), String> {
    let epoch = management_epoch(&app, &window, &state)?;
    state
        .packages
        .delete_retained_data(&record_id, &revision, epoch)
        .await?;
    finish_management(&app, &window, &state, epoch)
}

#[tauri::command]
pub(crate) async fn save_plugin_settings(
    plugin_id: String,
    revision: String,
    values: BTreeMap<String, Value>,
    secret_changes: BTreeMap<String, Option<String>>,
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, DesktopPlugins>,
) -> Result<SettingsSaveResult, String> {
    let epoch = management_epoch(&app, &window, &state)?;
    let result = state
        .packages
        .save_settings(&plugin_id, epoch, revision, values, secret_changes)
        .await?;
    finish_management(&app, &window, &state, epoch)?;
    let packages = state.packages.clone();
    tauri::async_runtime::spawn(async move {
        let _ = packages.restore(epoch).await;
    });
    Ok(result)
}

#[tauri::command]
pub(crate) fn get_plugin_snapshot(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, DesktopPlugins>,
) -> Result<Vec<PluginSnapshot>, String> {
    require_access(&app, &window, &state)?;
    Ok(state.host.snapshots())
}

#[tauri::command]
pub(crate) async fn plugin_action(
    plugin_id: String,
    instance_id: String,
    action_id: String,
    params: Value,
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, DesktopPlugins>,
) -> Result<Value, String> {
    let epoch = state.host.authority_epoch();
    require_access(&app, &window, &state)?;
    let result = state
        .host
        .action_in_epoch(
            &plugin_id,
            &instance_id,
            &action_id,
            params,
            Duration::from_secs(5),
            epoch,
        )
        .await
        .map_err(|error| error.to_string())?;
    require_access(&app, &window, &state)?;
    if state.host.authority_epoch() != epoch {
        return Err("Plugin session changed".into());
    }
    Ok(result)
}

pub(crate) fn install(app: &tauri::AppHandle) {
    let changes = Arc::new(tokio::sync::Notify::new());
    let notify = changes.clone();
    let host = PluginHost::new(Arc::new(move || {
        notify.notify_one();
    }));
    let trust = embedded_trust();
    let notify = changes.clone();
    let (media, media_events) = MediaService::new();
    let packages = PackageApplication::new_with_media(
        host.clone(),
        env!("INVERTER_DESKTOP_TARGET").into(),
        trust.as_ref().is_ok_and(|trust| !trust.is_empty()),
        Arc::new(move || notify.notify_one()),
        Some(media.clone()),
    );
    let root = app
        .path()
        .app_local_data_dir()
        .map(|path| path.join("plugins"))
        .map_err(|error| error.to_string());
    app.manage(DesktopPlugins {
        host,
        packages: packages.clone(),
        media: media.clone(),
        session_gate: Mutex::new(()),
        exit: ExitGate::default(),
        #[cfg(feature = "native-media-smoke")]
        native_media_smoke: false,
    });
    super::media_windows::forward_events(app.clone(), media, media_events);
    let key_app = app.clone();
    let seed = super::migration::provider(app.clone());
    tauri::async_runtime::spawn(async move {
        packages
            .initialize_with_seed(
                root,
                trust,
                Arc::new(move || crate::config_store::plugin_settings_key(&key_app)),
                Some(seed),
            )
            .await;
    });
    forward_changes(app.clone(), changes);
    authentication_changed(app);
    watch_session_expiry(app.clone());
}

/// Only explicit native harness setup can mark this process-local state.
#[cfg(feature = "native-media-smoke")]
pub(crate) fn native_media_smoke_session(app: &tauri::AppHandle) -> bool {
    // Browser persistence belongs to this immutable process identity, even if
    // exit starts while a rejected window is still being constructed.
    app.try_state::<DesktopPlugins>()
        .is_some_and(|state| state.native_media_smoke)
}

/// Explicitly isolated harness installation. No normal auth/config/keychain
/// paths or notification/session watchers are installed by this entry.
#[cfg(feature = "native-media-smoke")]
pub(crate) async fn install_native_media_smoke(
    app: &tauri::AppHandle,
    root: std::path::PathBuf,
    plugin_id: &str,
    always_on_top: bool,
) -> Result<MediaService, String> {
    // The isolated fixture uses its own disposable key, never installation data.
    use rand::RngExt;
    let key: [u8; 32] = rand::rng().random();
    let key_provider: super::settings_store::SettingsKeyProvider =
        Arc::new(move || Ok(key.to_vec()));
    let settings = super::settings_store::SettingsStore::new(root.clone(), key_provider.clone());
    let host = PluginHost::default();
    let (media, events) = MediaService::new();
    let packages = PackageApplication::new_with_media(
        host.clone(),
        env!("INVERTER_DESKTOP_TARGET").into(),
        false,
        Arc::new(|| {}),
        Some(media.clone()),
    );
    app.manage(DesktopPlugins {
        host,
        packages: packages.clone(),
        media: media.clone(),
        session_gate: Mutex::new(()),
        exit: ExitGate::default(),
        native_media_smoke: true,
    });
    super::media_windows::forward_events(app.clone(), media.clone(), events);
    // Keep initialization owned even if the harness deadline cancels its caller.
    // Shutdown waits for the resulting Ready/Failed publication before cleanup.
    let initializing = packages.clone();
    tauri::async_runtime::spawn(async move {
        initializing
            .initialize_with_key(
                Ok(root),
                super::package::TrustStore::new(Vec::new()),
                key_provider,
            )
            .await;
    })
    .await
    .map_err(|_| "Native media smoke initialization task failed")?;
    let epoch = packages
        .session_changed(true)
        .ok_or("Native media smoke session unavailable")?;
    let snapshot = packages.snapshot(epoch).await?;
    if !snapshot.ready || snapshot.error.is_some() {
        return Err("Native media smoke package initialization failed".into());
    }
    if always_on_top {
        let mut data = super::settings_store::SettingsData::default();
        data.values.insert(
            super::settings::VIDEO_ALWAYS_ON_TOP.into(),
            Value::Bool(true),
        );
        settings.prepare_write(plugin_id, &data)?.commit()?;
    }
    Ok(media)
}

const NOTIFICATION_SIGNAL_CAPACITY: usize = 1;

fn forward_changes(app: tauri::AppHandle, changes: Arc<tokio::sync::Notify>) {
    let (notifications, requests) = tokio::sync::mpsc::channel(NOTIFICATION_SIGNAL_CAPACITY);
    let notification_app = app.clone();
    tauri::async_runtime::spawn(drain_notification_requests(requests, move || {
        let app = notification_app.clone();
        async move {
            // One awaited blocking dispatcher keeps native calls off the async
            // executor. Its own exit/auth checks run only once the task starts.
            let _ = tauri::async_runtime::spawn_blocking(move || {
                forward_notifications(&app);
            })
            .await;
        }
    }));
    tauri::async_runtime::spawn(async move {
        loop {
            changes.notified().await;
            // Coalesce all workers into at most 20 UI refresh signals per second.
            // Notify retains one pending permit instead of an unbounded event queue.
            tokio::time::sleep(Duration::from_millis(50)).await;
            if app
                .state::<DesktopPlugins>()
                .exit
                .started
                .load(Ordering::Acquire)
            {
                break;
            }
            // Admit automatic previews before waking the OS notification
            // dispatcher. It also yields between submissions to later media.
            forward_http_videos(&app);
            if app
                .state::<DesktopPlugins>()
                .host
                .has_pending_notifications()
            {
                // A single pending signal coalesces bursts while the separate
                // dispatcher waits for the OS; UI refreshes never await it.
                let _ = notifications.try_send(());
            }
            // The fixed event carries no worker data; each window rechecks its session.
            let _ = app.emit("plugin-host-update", ());
        }
    });
}

/// Live session policy is checked outside runtime and generation guards.
pub(crate) fn media_access(app: &tauri::AppHandle) -> Option<MediaService> {
    let state = app.try_state::<DesktopPlugins>()?;
    if state.exit.started.load(Ordering::Acquire) {
        return None;
    }
    #[cfg(feature = "native-media-smoke")]
    if state.native_media_smoke {
        return Some(state.media.clone());
    }
    if auth::require_session(app).is_err() {
        authentication_changed(app);
        let _ = app.emit("auth-state-changed", ());
        return None;
    }
    Some(state.media.clone())
}

/// Native-only preference lookup; the plugin identity comes from its media lease.
pub(crate) fn video_window_always_on_top(
    app: &tauri::AppHandle,
    plugin_id: &str,
) -> Result<bool, String> {
    app.try_state::<DesktopPlugins>()
        .ok_or("Plugin store is unavailable")?
        .packages
        .video_window_always_on_top(plugin_id)
}

fn forward_http_videos(app: &tauri::AppHandle) {
    let state = app.state::<DesktopPlugins>();
    if !state.host.has_pending_http_videos() {
        return;
    }
    let Some(media) = media_access(app) else {
        return;
    };
    for request in state.host.take_http_video_requests() {
        // Admission is bounded and never waits for HTTP, disk, or native windows.
        let _ = media.try_submit(request);
    }
}

#[tauri::command]
pub(crate) fn get_live_preview_url(window: tauri::WebviewWindow) -> Result<String, String> {
    let label = window.label();
    if !super::media_windows::is_plugin_preview_label(label) {
        return Err("Only an owned preview can request its stream".into());
    }
    let id = super::media_windows::media_id_for_label(label)
        .ok_or("Only an owned preview can request its stream")?;
    media_access(window.app_handle())
        .and_then(|media| media.live_preview_url(&id, label))
        .map(|url| url.to_string())
        .ok_or_else(|| "Live preview is no longer active".into())
}

#[tauri::command]
pub(crate) fn reveal_plugin_video_window(window: tauri::WebviewWindow) -> Result<(), String> {
    super::media_windows::reveal_window(&window)
        .map_err(|_| "Video window is no longer available".into())
}

#[tauri::command]
pub(crate) fn close_plugin_video_window(
    window: tauri::WebviewWindow,
    state: State<'_, DesktopPlugins>,
) -> Result<(), String> {
    check_owned_video(&state.media, window.label())?;
    window
        .close()
        .map_err(|_| "Cannot close video window".into())
}

#[tauri::command]
pub(crate) fn drag_plugin_video_window(
    window: tauri::WebviewWindow,
    state: State<'_, DesktopPlugins>,
) -> Result<(), String> {
    check_owned_video(&state.media, window.label())?;
    window
        .start_dragging()
        .map_err(|_| "Cannot drag video window".into())
}

fn check_owned_video(media: &MediaService, label: &str) -> Result<(), String> {
    let id = super::media_windows::media_id_for_label(label)
        .ok_or("Only an owned video window can use this command")?;
    if !media.is_window_active(&id, label) {
        return Err("Video window is no longer active".into());
    }
    Ok(())
}

async fn drain_notification_requests<F, Submission>(
    mut requests: tokio::sync::mpsc::Receiver<()>,
    mut submit: F,
) where
    F: FnMut() -> Submission,
    Submission: std::future::Future<Output = ()>,
{
    // There is one receiver for the app lifetime, at most one pending signal,
    // and no overlapping submission tasks. Dropping the producer on shutdown
    // closes this loop after its already-owned dispatch finishes.
    while requests.recv().await.is_some() {
        submit().await;
    }
}

fn forward_notifications(app: &tauri::AppHandle) {
    let state = app.state::<DesktopPlugins>();
    if state.exit.started.load(Ordering::Acquire) || !state.host.has_pending_notifications() {
        return;
    }
    // Validate before entering the host authority guard; auth transitions reenter
    // the host. The dispatcher then checks each generation and stop signal.
    if auth::require_session(app).is_err() {
        authentication_changed(app);
        let _ = app.emit("auth-state-changed", ());
        return;
    }
    state.host.dispatch_notifications(|message| {
        if state.exit.started.load(Ordering::Acquire) {
            return;
        }
        // Tauri's notification wrapper schedules another task. Submit directly
        // through the same native backend while this generation is authorized.
        // Display timing and already submitted notifications belong to the OS.
        if super::native_notifications::submit(app, message).is_err() {
            log::warn!(
                "Desktop notification delivery failed for plugin {}",
                message.plugin_id
            );
        }
    });
}

fn watch_session_expiry(app: tauri::AppHandle) {
    // Session expiry must stop background workers even when no webview is making
    // requests. Empty/stopped hosts do not read configuration on each tick.
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let state = app.state::<DesktopPlugins>();
            if state.exit.started.load(Ordering::Acquire) {
                break;
            }
            state.packages.expire_preview();
            if super::migration::daemon_ready(&app)
                && state.host.is_authorized_epoch(state.host.authority_epoch())
                && auth::require_session(&app).is_ok()
                && state.packages.take_waiting_migration()
            {
                let epoch = state.host.authority_epoch();
                let packages = state.packages.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = packages.restore(epoch).await;
                });
            }
            let active = state.host.snapshots().iter().any(|worker| {
                matches!(
                    worker.state,
                    WorkerState::Starting | WorkerState::Running | WorkerState::Restarting
                )
            });
            if (active || state.packages.has_session_work() || state.media.has_owned_work())
                && auth::require_session(&app).is_err()
            {
                authentication_changed(&app);
                let _ = app.emit("auth-state-changed", ());
            }
        }
    });
}

/// Successful auth polling can recover a startup configuration-access failure.
/// Healthy sessions retain their epoch and running workers unchanged.
pub(crate) fn recover_session_if_needed(app: &tauri::AppHandle) {
    let Some(state) = app.try_state::<DesktopPlugins>() else {
        return;
    };
    let epoch = recover_session(
        &state.host,
        &state.packages,
        &crate::CONFIG_UPDATE_GATE,
        &state.session_gate,
        &state.exit.started,
        || {
            let config = crate::load_config(app)?;
            let authority = auth::session_authority(&config)?;
            Ok((config.desktop_plugins, authority))
        },
    );
    restore_session(&state.packages, epoch);
}

pub(super) fn recover_session<A>(
    host: &PluginHost,
    packages: &PackageApplication,
    config_gate: &Mutex<()>,
    session_gate: &Mutex<()>,
    exiting: &AtomicBool,
    configuration: impl FnOnce() -> Result<(Vec<crate::plugin_config::DesktopPluginConfig>, A), String>,
) -> Option<u64> {
    // Match config saves' lock order. A concurrent policy write must finish
    // before recovery samples it, or wait until this resume has completed.
    let _configuration = config_gate.lock().ok()?;
    let _transition = session_gate
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if exiting.load(Ordering::Acquire) || host.is_authorized_epoch(host.authority_epoch()) {
        return None;
    }
    // Read the current policy and declarations only after acquiring the gate.
    // The authority guard stays held through resume; no earlier UI result is
    // authority to unlock, and a failed read leaves the host revoked.
    let (declarations, _authority) = configuration().ok()?;
    if exiting.load(Ordering::Acquire) {
        return None;
    }
    packages.session_configured(true, Ok(declarations))
}

fn restore_session(packages: &PackageApplication, epoch: Option<u64>) {
    if let Some(epoch) = epoch {
        let packages = packages.clone();
        tauri::async_runtime::spawn(async move {
            // Errors remain visible in the manager snapshot. A revoked
            // restoration is expected when logout or quit wins the race.
            let _ = packages.restore(epoch).await;
        });
    }
}

pub(crate) fn authentication_changed(app: &tauri::AppHandle) {
    if let Some(state) = app.try_state::<DesktopPlugins>() {
        // Sample the live policy/session under the same gate as the transition.
        // A delayed login notification cannot replay an old `unlocked` value
        // after a completed logout notification.
        let _transition = state
            .session_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // Revoke synchronously before the frontend receives auth-state-changed.
        // Resume never revives the previous epoch's processes or queued actions.
        let unlocked =
            !state.exit.started.load(Ordering::Acquire) && auth::require_session(app).is_ok();
        let epoch = state.packages.session_configured(
            unlocked,
            crate::load_config(app)
                .map(|config| config.desktop_plugins)
                .map_err(|_| "Plugin declarations could not be loaded".into()),
        );
        restore_session(&state.packages, epoch);
    }
}

pub(crate) fn on_run_event(app: &tauri::AppHandle, event: tauri::RunEvent) {
    let Some(state) = app.try_state::<DesktopPlugins>() else {
        return;
    };
    if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
        let (prevent, cleanup) = state.exit.request();
        if prevent {
            api.prevent_exit();
        }
        if cleanup {
            state.packages.begin_shutdown();
            let packages = state.packages.clone();
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(error) = packages.close().await {
                    log::error!("Plugin cleanup could not complete: {error}");
                    // Keep the application and lease alive; a later quit may
                    // retry cleanup, but no new package work is admitted.
                    app.state::<DesktopPlugins>()
                        .exit
                        .started
                        .store(false, Ordering::Release);
                    return;
                }
                app.state::<DesktopPlugins>()
                    .exit
                    .finished
                    .store(true, Ordering::Release);
                app.exit(code.unwrap_or(0));
            });
        }
    } else if let tauri::RunEvent::WindowEvent {
        label,
        event: tauri::WindowEvent::Destroyed,
        ..
    } = event
    {
        state.packages.window_closed(&label);
        state.media.window_destroyed(&label);
    } else if matches!(event, tauri::RunEvent::Exit) {
        state.packages.begin_shutdown();
    }
}

#[cfg(test)]
#[path = "session_recovery_tests.rs"]
mod recovery_tests;

#[cfg(test)]
mod tests {
    use super::{
        drain_notification_requests, management_window, trusted_window, ExitGate, Ordering,
        NOTIFICATION_SIGNAL_CAPACITY,
    };

    #[tokio::test]
    async fn slow_notification_delivery_coalesces_without_blocking_or_overlapping() {
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        use std::sync::Arc;
        use std::time::Duration;

        let (signals, requests) = tokio::sync::mpsc::channel(NOTIFICATION_SIGNAL_CAPACITY);
        let (started, receiving) = tokio::sync::oneshot::channel();
        let (release, blocked) = tokio::sync::oneshot::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicBool::new(false));
        let mut first = Some((started, blocked));
        let count = calls.clone();
        let in_flight = active.clone();
        let drain = tokio::spawn(drain_notification_requests(requests, move || {
            let first = first.take();
            let count = count.clone();
            let in_flight = in_flight.clone();
            async move {
                assert!(!in_flight.swap(true, Ordering::AcqRel));
                count.fetch_add(1, Ordering::AcqRel);
                if let Some((started, blocked)) = first {
                    let _ = started.send(());
                    blocked.await.unwrap();
                }
                in_flight.store(false, Ordering::Release);
            }
        }));
        signals.try_send(()).unwrap();
        receiving.await.unwrap();

        // The producer remains free to forward UI events while the native
        // submission is blocked. A burst retains only one follow-up request.
        let mut accepted = 0;
        for _ in 0..100 {
            accepted += usize::from(signals.try_send(()).is_ok());
        }
        assert_eq!(accepted, 1);
        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert!(!drain.is_finished());
        drop(signals);
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), drain)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(calls.load(Ordering::Acquire), 2);
        assert!(!active.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn idle_notification_dispatcher_never_starts_native_or_auth_work() {
        let (signals, requests) = tokio::sync::mpsc::channel(NOTIFICATION_SIGNAL_CAPACITY);
        drop(signals);
        drain_notification_requests(requests, || async {
            panic!("an idle dispatcher must not inspect auth or submit notifications");
        })
        .await;
    }

    #[test]
    fn repeated_quit_waits_until_worker_cleanup_finishes() {
        let gate = ExitGate::default();
        assert_eq!(gate.request(), (true, true));
        assert_eq!(gate.request(), (true, false));
        gate.finished.store(true, Ordering::Release);
        assert_eq!(gate.request(), (false, false));
    }

    #[test]
    fn only_dashboard_and_settings_windows_can_access_workers() {
        assert!(trusted_window("main"));
        assert!(trusted_window("config"));
        for label in [
            "about",
            "camera-video-123",
            "plugin-custom",
            "",
            "main/other",
        ] {
            assert!(!trusted_window(label), "{label}");
        }
    }

    #[test]
    fn only_settings_can_manage_packages() {
        assert!(management_window("config"));
        for label in [
            "main",
            "about",
            "camera-video-123",
            "plugin-custom",
            "",
            "config/other",
        ] {
            assert!(!management_window(label));
        }
    }
}
