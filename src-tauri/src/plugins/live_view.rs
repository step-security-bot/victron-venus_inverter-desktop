//! Isolated, generation-owned pages opened only by notification interaction.

#[cfg(target_os = "macos")]
use super::runtime::LiveViewRequest;
#[cfg(target_os = "macos")]
use tauri::Manager;

pub(crate) fn is_live_window(label: &str) -> bool {
    label.starts_with("plugin-live-")
}

#[cfg(target_os = "macos")]
fn label(request: &LiveViewRequest) -> String {
    format!(
        "plugin-live-{}-{}",
        super::package::sha256_hex(
            format!("{}\0{}", request.lease.plugin_id(), request.id).as_bytes()
        ),
        request.lease.instance_id()
    )
}

#[cfg(target_os = "macos")]
fn open(app: tauri::AppHandle, request: LiveViewRequest, title: String) {
    let Ok(always_on_top) =
        super::bridge::video_window_always_on_top(&app, request.lease.plugin_id())
    else {
        log::warn!("Cannot read plugin video window preference");
        return;
    };
    let owned = request.clone();
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        if !owned.lease.is_active() || crate::auth::require_session(&handle).is_err() {
            return;
        }
        let name = label(&owned);
        if let Some(window) = handle.get_webview_window(&name) {
            let _ = window.show();
            let _ = window.unminimize();
            let _ = window.set_focus();
            return;
        }
        let window = tauri::WebviewWindowBuilder::new(
            &handle,
            &name,
            tauri::WebviewUrl::External(owned.url.clone()),
        )
        .title(title)
        .inner_size(854.0, 480.0)
        .min_inner_size(320.0, 180.0)
        .resizable(true)
        .focused(true)
        .always_on_top(always_on_top)
        .on_navigation(|url| matches!(url.scheme(), "http" | "https"))
        .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny)
        .build();
        if let Ok(window) = window {
            // UI operations must not run under the host's generation mutex.
            // Close immediately if revocation raced the native window creation.
            if !owned.lease.is_active() {
                let _ = window.close();
                return;
            }
            tauri::async_runtime::spawn(async move {
                owned.lease.cancelled().await;
                let _ = window.close();
            });
        }
    });
}

#[cfg(target_os = "macos")]
async fn authorize() -> Result<(), ()> {
    use mac_usernotifications::AuthorizationStatus;
    static AUTHORIZATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = AUTHORIZATION.lock().await;
    let settings = mac_usernotifications::get_notification_settings()
        .await
        .map_err(|_| ())?;
    match settings.authorization_status {
        AuthorizationStatus::Authorized
        | AuthorizationStatus::Provisional
        | AuthorizationStatus::Ephemeral => Ok(()),
        AuthorizationStatus::NotDetermined
            if mac_usernotifications::request_auth()
                .await
                .map_err(|_| ())? =>
        {
            Ok(())
        }
        _ => Err(()),
    }
}

#[cfg(target_os = "macos")]
pub(super) fn submit(
    app: &tauri::AppHandle,
    message: &super::runtime::DesktopNotification,
) -> Result<(), ()> {
    use std::sync::{Arc, LazyLock};
    use std::time::Duration;
    static SLOTS: LazyLock<Arc<tokio::sync::Semaphore>> =
        LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(32)));
    let permit = SLOTS.clone().try_acquire_owned().map_err(|_| ())?;
    let request = message.live_view.clone().ok_or(())?;
    let app = app.clone();
    let title = message.title.clone();
    let body = message.body.clone();
    tauri::async_runtime::spawn(async move {
        let _permit = permit;
        let id = uuid::Uuid::new_v4().to_string();
        let mut submitted = false;
        let work = async {
            if !request.lease.is_active() || crate::auth::require_session(&app).is_err() {
                return;
            }
            if !matches!(
                tokio::time::timeout(Duration::from_secs(60), authorize()).await,
                Ok(Ok(()))
            ) {
                return;
            }
            if !request.lease.is_active() || crate::auth::require_session(&app).is_err() {
                return;
            }
            let notification = mac_usernotifications::Notification::new()
                .id(&id)
                .title(&title)
                .message(body)
                .thread_id(label(&request))
                .timeout(Duration::from_secs(300))
                .action(mac_usernotifications::Action::button("view", "View camera"));
            submitted = true;
            let Ok(Ok(handle)) =
                tokio::time::timeout(Duration::from_secs(10), notification.send()).await
            else {
                return;
            };
            if let Ok(response) = handle.response().await {
                if !response.is_dismiss_action()
                    && !response.is_timed_out()
                    && (response.is_default_action() || response.action_identifier == "view")
                    && request.lease.is_active()
                    && crate::auth::require_session(&app).is_ok()
                {
                    open(app.clone(), request.clone(), title.clone());
                }
            }
        };
        tokio::select! {
            biased;
            _ = request.lease.cancelled() => {},
            _ = tokio::time::timeout(Duration::from_secs(375), work) => {},
        }
        if submitted {
            let _ = tokio::time::timeout(Duration::from_secs(10), async {
                mac_usernotifications::cancel_pending(&id).await;
                mac_usernotifications::close_delivered(&id).await;
            })
            .await;
        }
    });
    Ok(())
}
