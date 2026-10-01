//! App-wide unlock session shared by trusted local windows. Every custom IPC
//! command is checked centrally; an overlay alone is not an authorization boundary.
use super::{load_config, FullConfig};
use serde::Serialize;
#[cfg(desktop)]
use std::sync::MutexGuard;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use tauri::Emitter;

const SESSION_TTL: Duration = Duration::from_secs(15 * 60);
static SESSION: LazyLock<Mutex<Option<Session>>> = LazyLock::new(|| Mutex::new(None));

struct Session {
    token: String,
    created: Instant,
}

impl Session {
    fn valid_at(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.created) < SESSION_TTL
    }
}

pub(super) fn public_command(command: &str) -> bool {
    matches!(
        command,
        "auth_status"
            | "get_release_info"
            | "auth_login"
            | "auth_check"
            | "auth_logout"
            | "auth_biometric_available"
            | "auth_biometric"
            | "close_config_window"
    )
}

fn unlocked() -> Result<bool, String> {
    let mut session = SESSION.lock().map_err(|e| e.to_string())?;
    Ok(session_unlocked_at(&mut session, Instant::now()))
}

fn session_unlocked_at(session: &mut Option<Session>, now: Instant) -> bool {
    if session.as_ref().is_some_and(|s| !s.valid_at(now)) {
        *session = None;
    }
    session.is_some()
}

/// Keep logout from changing the checked session before a synchronous resume.
#[cfg(desktop)]
pub(super) struct SessionAuthority<'a> {
    _session: MutexGuard<'a, Option<Session>>,
}

#[cfg(desktop)]
fn session_authority_at<'a>(
    config: &FullConfig,
    sessions: &'a Mutex<Option<Session>>,
    now: Instant,
) -> Result<SessionAuthority<'a>, String> {
    let mut session = sessions.lock().map_err(|e| e.to_string())?;
    if !session_unlocked_at(&mut session, now) && config.auth_enabled.unwrap_or(false) {
        return Err("Authentication required".into());
    }
    Ok(SessionAuthority { _session: session })
}

#[cfg(desktop)]
pub(super) fn session_authority(config: &FullConfig) -> Result<SessionAuthority<'static>, String> {
    session_authority_at(config, &SESSION, Instant::now())
}

pub(super) fn require_session(app: &tauri::AppHandle) -> Result<(), String> {
    if !load_config(app)?.auth_enabled.unwrap_or(false) || unlocked()? {
        Ok(())
    } else {
        Err("Authentication required".into())
    }
}

pub(super) fn validate_policy(config: &FullConfig) -> Result<(), String> {
    if config.auth_enabled.unwrap_or(false)
        && (config
            .auth_username
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
            || config.auth_password.as_deref().unwrap_or("").is_empty())
    {
        return Err("A username and password are required when authentication is enabled".into());
    }
    Ok(())
}

fn policy_changed(before: &FullConfig, after: &FullConfig) -> bool {
    before.auth_enabled != after.auth_enabled
        || before.auth_username != after.auth_username
        || before.auth_password != after.auth_password
        || before.auth_biometric != after.auth_biometric
}

fn notify_session_changed(app: &tauri::AppHandle) {
    #[cfg(desktop)]
    crate::plugins::bridge::authentication_changed(app);
    let _ = app.emit("auth-state-changed", ());
}

pub(super) fn revoke_if_policy_changed(
    app: &tauri::AppHandle,
    before: &FullConfig,
    after: &FullConfig,
) -> Result<(), String> {
    if policy_changed(before, after) {
        *SESSION.lock().map_err(|e| e.to_string())? = None;
        notify_session_changed(app);
    }
    Ok(())
}

fn start_session(app: &tauri::AppHandle) -> Result<String, String> {
    let token = uuid::Uuid::new_v4().to_string();
    *SESSION.lock().map_err(|e| e.to_string())? = Some(Session {
        token: token.clone(),
        created: Instant::now(),
    });
    notify_session_changed(app);
    Ok(token)
}

#[derive(Serialize)]
pub(crate) struct AuthStatus {
    enabled: bool,
    unlocked: bool,
}

#[tauri::command]
pub(crate) fn auth_status(app: tauri::AppHandle) -> Result<AuthStatus, String> {
    let enabled = load_config(&app)?.auth_enabled.unwrap_or(false);
    let status = AuthStatus {
        enabled,
        unlocked: !enabled || unlocked()?,
    };
    #[cfg(desktop)]
    if status.unlocked {
        crate::plugins::bridge::recover_session_if_needed(&app);
    }
    Ok(status)
}

#[tauri::command]
pub(crate) fn auth_login(
    username: String,
    password: String,
    app: tauri::AppHandle,
) -> Result<String, String> {
    let config = load_config(&app)?;
    if !config.auth_enabled.unwrap_or(false) {
        return Ok("disabled".into());
    }
    if username != config.auth_username.as_deref().unwrap_or("")
        || password != config.auth_password.as_deref().unwrap_or("")
    {
        return Err("Invalid credentials".into());
    }
    start_session(&app)
}

#[tauri::command]
pub(crate) fn auth_check(token: String) -> Result<bool, String> {
    let _ = unlocked()?;
    Ok(SESSION
        .lock()
        .map_err(|e| e.to_string())?
        .as_ref()
        .is_some_and(|s| s.token == token))
}

#[tauri::command]
pub(crate) fn auth_logout(app: tauri::AppHandle) -> Result<(), String> {
    *SESSION.lock().map_err(|e| e.to_string())? = None;
    notify_session_changed(&app);
    Ok(())
}

fn biometric_enabled(config: &FullConfig) -> bool {
    config.auth_enabled.unwrap_or(false) && config.auth_biometric.unwrap_or(false)
}

#[tauri::command]
pub(crate) fn auth_biometric_available(app: tauri::AppHandle) -> Result<bool, String> {
    if !biometric_enabled(&load_config(&app)?) {
        return Ok(false);
    }
    #[cfg(target_os = "macos")]
    {
        Ok(unsafe { super::biometric_available() })
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(false)
    }
}

#[tauri::command]
pub(crate) async fn auth_biometric(app: tauri::AppHandle) -> Result<String, String> {
    let policy = load_config(&app)?;
    if !biometric_enabled(&policy) {
        return Err("Biometric authentication is disabled".into());
    }
    #[cfg(target_os = "macos")]
    {
        let ok = tauri::async_runtime::spawn_blocking(|| {
            let reason = std::ffi::CString::new("Authenticate to access Inverter Desktop")
                .expect("static reason");
            unsafe { super::biometric_authenticate(reason.as_ptr()) }
        })
        .await
        .map_err(|e| e.to_string())?;
        // The policy may have changed while the native prompt was open.
        if !ok || policy_changed(&policy, &load_config(&app)?) {
            return Err("Biometric authentication failed or was cancelled".into());
        }
        start_session(&app)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("Biometric authentication is only supported on macOS".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn privileged_commands_are_not_public() {
        for command in [
            "get_config",
            "save_config",
            "backup_config",
            "export_tariff",
            "restore_config",
            "perform_action",
            "get_setpoint_override",
            "set_setpoint_override",
            "get_plugin_snapshot",
            "plugin_action",
            "get_plugin_manager_snapshot",
            "get_plugin_settings",
            "get_plugin_settings_choices",
            "get_plugin_groups",
            "set_plugin_group_enabled",
            "save_plugin_settings",
            "get_retained_plugin_data",
            "delete_retained_plugin_data",
            "preview_plugin_package",
            "install_plugin_package",
            "discard_plugin_package",
            "set_plugin_enabled",
            "rollback_plugin_package",
            "uninstall_plugin_package",
            "connect_mqtt",
            "connect_gateway",
            "open_config_window",
            "auth_fake",
        ] {
            assert!(!public_command(command), "{command}");
        }
        assert!(public_command("auth_status"));
        assert!(public_command("auth_login"));
    }
    #[test]
    fn removed_provider_commands_have_no_public_authority() {
        for command in ["close_camera_video_window", "disconnect_ha_mqtt"] {
            assert!(!public_command(command));
        }
    }
    #[test]
    fn session_expires_at_deadline() {
        let now = Instant::now();
        let session = Session {
            token: "test".into(),
            created: now,
        };
        assert!(session.valid_at(now + SESSION_TTL - Duration::from_secs(1)));
        assert!(!session.valid_at(now + SESSION_TTL));
    }
    #[cfg(desktop)]
    #[test]
    fn recovery_authority_requires_current_policy_and_unexpired_session() {
        let now = Instant::now();
        let sessions = Mutex::new(None);
        let mut config = FullConfig::default();
        // Disabled authentication permits startup recovery without a login.
        let authority = session_authority_at(&config, &sessions, now).unwrap();
        assert!(sessions.try_lock().is_err());
        drop(authority);
        config.auth_enabled = Some(true);
        assert!(session_authority_at(&config, &sessions, now).is_err());
        *sessions.lock().unwrap() = Some(Session {
            token: "local-session".into(),
            created: now,
        });
        let authority = session_authority_at(
            &config,
            &sessions,
            now + SESSION_TTL - Duration::from_secs(1),
        )
        .unwrap();
        // Logout cannot replace the checked session until resume releases it.
        assert!(sessions.try_lock().is_err());
        drop(authority);
        assert!(session_authority_at(&config, &sessions, now + SESSION_TTL).is_err());
        assert!(sessions.lock().unwrap().is_none());
    }
    #[test]
    fn policy_changes_revoke_but_display_changes_do_not() {
        let before = FullConfig::default();
        let mut after = before.clone();
        after.color_scheme = Some("light".into());
        assert!(!policy_changed(&before, &after));
        after.auth_password = Some("new-password".into());
        assert!(policy_changed(&before, &after));
        after = before.clone();
        after.auth_biometric = Some(true);
        assert!(policy_changed(&before, &after));
    }
    #[test]
    fn biometric_requires_both_policy_flags() {
        let mut config = FullConfig {
            auth_enabled: Some(true),
            ..Default::default()
        };
        assert!(!biometric_enabled(&config));
        assert!(validate_policy(&config).is_err());
        config.auth_biometric = Some(true);
        assert!(biometric_enabled(&config));
        config.auth_enabled = Some(false);
        assert!(!biometric_enabled(&config));
    }
}
