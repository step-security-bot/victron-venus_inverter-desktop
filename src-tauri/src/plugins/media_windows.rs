//! Native windows and a window-bound opaque media route for desktop plugins.

use super::bridge::media_access;
use super::media::{MediaError, MediaEvent, MediaService, ReadyMedia};
use super::protocol::HttpMediaKind;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tauri::utils::config::{Csp, CspDirectiveSources};
use tauri::{http, Manager};
use tokio::sync::{mpsc, watch, Semaphore};

const LABEL_PREFIX: &str = "plugin-video-";
const HIDDEN_WINDOW_TIMEOUT: Duration = Duration::from_secs(30);
static RANGE_REQUESTS: OnceLock<Arc<Semaphore>> = OnceLock::new();

pub(crate) fn media_id_for_label(label: &str) -> Option<String> {
    canonical_media_id(
        label
            .strip_prefix(LABEL_PREFIX)
            .or_else(|| label.strip_prefix("plugin-preview-"))?,
    )
}

fn canonical_media_id(value: &str) -> Option<String> {
    let id = uuid::Uuid::parse_str(value).ok()?;
    (id.to_string() == value).then(|| value.to_owned())
}

pub(crate) fn is_plugin_preview_label(label: &str) -> bool {
    label
        .strip_prefix("plugin-preview-")
        .and_then(canonical_media_id)
        .is_some()
}

pub(crate) fn is_plugin_video_label(label: &str) -> bool {
    label
        .strip_prefix(LABEL_PREFIX)
        .and_then(canonical_media_id)
        .is_some()
}

/// AuthGate reads session status before mounting the viewer. All other authority
/// belongs to these exact native-owned controls, never global application IPC.
pub(crate) fn preview_command_allowed(command: &str) -> bool {
    matches!(
        command,
        "auth_status"
            | "get_live_preview_url"
            | "reveal_plugin_video_window"
            | "close_plugin_video_window"
            | "drag_plugin_video_window"
    )
}

pub(crate) fn register(builder: tauri::Builder<tauri::Wry>) -> tauri::Builder<tauri::Wry> {
    builder.register_asynchronous_uri_scheme_protocol(
        "plugin-media",
        |context, request, responder| {
            let Ok(permit) = RANGE_REQUESTS
                .get_or_init(|| Arc::new(Semaphore::new(4)))
                .clone()
                .try_acquire_owned()
            else {
                responder.respond(empty_response(503));
                return;
            };
            let app = context.app_handle().clone();
            let label = context.webview_label().to_owned();
            tauri::async_runtime::spawn(async move {
                let _permit = permit;
                let Some(id) = request_media_id(&label, request.uri()) else {
                    responder.respond(empty_response(404));
                    return;
                };
                let head = request.method() == http::Method::HEAD;
                if !head && request.method() != http::Method::GET {
                    responder.respond(empty_response(405));
                    return;
                }
                let access_app = app.clone();
                let media = tauri::async_runtime::spawn_blocking(move || media_access(&access_app))
                    .await
                    .ok()
                    .flatten();
                let Some(media) = media else {
                    responder.respond(empty_response(404));
                    return;
                };
                let range = match request.headers().get(http::header::RANGE) {
                    Some(value) => match value.to_str() {
                        Ok(value) => Some(value),
                        Err(_) => {
                            responder.respond(empty_response(400));
                            return;
                        }
                    },
                    None => None,
                };
                let mut result = match media.read_range(&id, &label, range, head).await {
                    Ok(result) => result,
                    Err(error) => {
                        responder.respond(empty_response(error_status(error)));
                        return;
                    }
                };
                // Recheck live policy after asynchronous disk work. The service also
                // rechecks the exact generation, retirement, and requesting window.
                let authorized =
                    tauri::async_runtime::spawn_blocking(move || media_access(&app).is_some())
                        .await
                        .unwrap_or(false);
                if !authorized || !media.is_window_active(&id, &label) {
                    responder.respond(empty_response(404));
                    return;
                }
                let mut response = http::Response::builder()
                    .status(result.status)
                    .header(http::header::CONTENT_TYPE, result.content_type)
                    .header(http::header::CONTENT_LENGTH, result.content_length)
                    .header(http::header::ACCEPT_RANGES, "bytes")
                    .header(http::header::CACHE_CONTROL, "no-store")
                    .header("X-Content-Type-Options", "nosniff");
                if let Some(value) = &result.content_range {
                    response = response.header(http::header::CONTENT_RANGE, value);
                }
                let response = response
                    .body(std::mem::take(&mut result.bytes))
                    .unwrap_or_else(|_| empty_response(500));
                // Retain the bounded read permit through the native handoff. Once
                // submitted, buffering and display timing belong to the webview.
                responder.respond(response);
            });
        },
    )
}

fn request_media_id(label: &str, uri: &http::Uri) -> Option<String> {
    if !is_plugin_video_label(label) || uri.query().is_some() {
        return None;
    }
    let id = canonical_media_id(uri.path().strip_prefix('/')?)?;
    (media_id_for_label(label).as_deref() == Some(id.as_str())).then_some(id)
}

fn error_status(error: MediaError) -> u16 {
    match error {
        MediaError::Unavailable | MediaError::NotFound => 404,
        MediaError::Busy => 503,
        MediaError::InvalidRequest | MediaError::RangeRequired => 400,
        _ => 500,
    }
}

fn empty_response(status: u16) -> http::Response<Vec<u8>> {
    let mut response = http::Response::new(Vec::new());
    *response.status_mut() =
        http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR);
    response.headers_mut().insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
    response
}

pub(crate) fn forward_events(
    app: tauri::AppHandle,
    media: MediaService,
    mut events: mpsc::Receiver<MediaEvent>,
) {
    // Keep receiving through shutdown: the media service waits for actual
    // Destroyed acknowledgements before releasing its files and lifetime lease.
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            let event_app = app.clone();
            let event_media = media.clone();
            match event {
                MediaEvent::Ready(ready) => {
                    let media_id = ready.media_id.clone();
                    let window_label = ready.window_label.clone();
                    let failed = tauri::async_runtime::spawn_blocking(move || {
                        open_window(&event_app, &event_media, ready)
                    })
                    .await;
                    if !matches!(failed, Ok(Ok(()))) {
                        let cleanup_app = app.clone();
                        let cleanup_media = media.clone();
                        let _ = tauri::async_runtime::spawn_blocking(move || {
                            if let Some(window) = cleanup_app.get_webview_window(&window_label) {
                                if window.destroy().is_err() {
                                    cleanup_media.window_cleanup_failed(&window_label);
                                }
                            } else {
                                cleanup_media.window_failed(&media_id);
                            }
                        })
                        .await;
                    }
                }
                MediaEvent::Close { window_label } => {
                    let _ = tauri::async_runtime::spawn_blocking(move || {
                        if let Some(window) = event_app.get_webview_window(&window_label) {
                            if window.destroy().is_err() {
                                log::warn!("Plugin video window cleanup could not complete");
                                event_media.window_cleanup_failed(&window_label);
                            }
                        } else {
                            event_media.window_destroyed(&window_label);
                        }
                    })
                    .await;
                }
            }
        }
    });
}

fn open_window(app: &tauri::AppHandle, media: &MediaService, ready: ReadyMedia) -> Result<(), ()> {
    if media_access(app).is_none() || !media.is_window_active(&ready.media_id, &ready.window_label)
    {
        return Err(());
    }
    let live = ready.live_url.is_some();
    let always_on_top =
        super::bridge::video_window_always_on_top(app, &ready.plugin_id).map_err(|_| ())?;
    if media_id_for_label(&ready.window_label).as_deref() != Some(ready.media_id.as_str())
        || live != is_plugin_preview_label(&ready.window_label)
    {
        return Err(());
    }
    let route = format!(
        "camera-video?pluginMedia={}&pluginMediaKind={}&name={}&error={}",
        ready.media_id,
        if live {
            "live"
        } else if ready.media_kind == HttpMediaKind::Video {
            "video"
        } else {
            "image"
        },
        crate::percent_encode_query(&ready.title),
        if ready.error.is_some() {
            "download"
        } else {
            ""
        },
    );
    let development_url = tauri::is_dev()
        .then(|| {
            app.config()
                .build
                .dev_url
                .as_ref()
                .and_then(|url| url.join("camera-video").ok())
        })
        .flatten();
    let live_url = ready.live_url.clone();
    let snapshot = ready.media_kind != HttpMediaKind::Video;
    let builder = tauri::WebviewWindowBuilder::new(
        app,
        &ready.window_label,
        tauri::WebviewUrl::App(route.into()),
    )
    .title(&ready.title)
    .inner_size(crate::CAMERA_VIDEO_WINDOW_W, crate::CAMERA_VIDEO_WINDOW_H)
    .resizable(true)
    .visible(false)
    .decorations(false)
    .focused(false)
    .always_on_top(always_on_top)
    .incognito(live)
    .on_navigation(move |url| navigation_allowed(url, development_url.as_ref()))
    .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny)
    .on_web_resource_request(move |request, response| {
        if !reqwest::Url::parse(&request.uri().to_string())
            .is_ok_and(|url| navigation_allowed(&url, None))
        {
            return;
        }
        if let Some(csp) = response
            .headers_mut()
            .get_mut(http::header::CONTENT_SECURITY_POLICY)
        {
            let updated = if let Some(url) = live_url.as_ref() {
                live_image_csp(csp, url)
            } else if snapshot {
                image_sources_csp(
                    csp,
                    &[
                        "plugin-media:",
                        "http://plugin-media.localhost",
                        "https://plugin-media.localhost",
                    ],
                )
            } else {
                None
            };
            if let Some(updated) = updated {
                *csp = updated;
            }
        }
    });
    #[cfg(target_os = "macos")]
    let builder = builder
        .hidden_title(true)
        .title_bar_style(tauri::TitleBarStyle::Transparent)
        // WebKit must process auth, image and timer callbacks while the window
        // waits hidden for its first frame. The lease and bootstrap watchdog
        // still bound the lifetime of this otherwise inactive webview.
        .background_throttling(tauri::utils::config::BackgroundThrottlingPolicy::Disabled);
    // Live views and the explicit harness never reuse a persistent browser store.
    #[cfg(feature = "native-media-smoke")]
    let builder = builder.incognito(live || super::bridge::native_media_smoke_session(app));
    let window = builder.build().map_err(|_| ())?;
    let event_app = app.clone();
    let (destroyed, destruction) = watch::channel(false);
    window.on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Destroyed) {
            destroyed.send_replace(true);
            crate::reflow_camera_video_windows(&event_app);
        }
    });
    crate::apply_camera_video_window_defaults(app, &window);
    if media_access(app).is_none() || !media.window_ready(&ready.media_id, &ready.window_label) {
        return Err(());
    }
    expire_hidden_window(app, media, ready.media_id, ready.window_label, destruction);
    // The viewer reveals its own window only after displaying media or an error.
    Ok(())
}

pub(crate) fn reveal_window(window: &tauri::WebviewWindow) -> Result<(), ()> {
    let media_id = media_id_for_label(window.label()).ok_or(())?;
    let media = media_access(window.app_handle()).ok_or(())?;
    show_without_focus(window, &media, &media_id)
}

fn expire_hidden_window(
    app: &tauri::AppHandle,
    media: &MediaService,
    media_id: String,
    window_label: String,
    mut destruction: watch::Receiver<bool>,
) {
    let app = app.clone();
    let media = media.clone();
    tauri::async_runtime::spawn(async move {
        tokio::select! {
            _ = destruction.wait_for(|destroyed| *destroyed) => return,
            _ = tokio::time::sleep(HIDDEN_WINDOW_TIMEOUT) => {},
        }
        let event_app = app.clone();
        let _ = app.run_on_main_thread(move || {
            let Some(window) = event_app.get_webview_window(&window_label) else {
                return;
            };
            // Serialize expiry with reveal. Retire visibility authority before
            // destruction so a delayed readiness callback cannot revive it.
            if window.is_visible().unwrap_or(false)
                || !media.window_closing(&media_id, &window_label)
            {
                return;
            }
            if window.destroy().is_err() {
                media.window_cleanup_failed(&window_label);
            }
        });
    });
}

fn live_image_csp(csp: &http::HeaderValue, url: &reqwest::Url) -> Option<http::HeaderValue> {
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    // Only a parsed origin can become an image source. Private query tokens,
    // paths and MQTT content cannot become directives or script authority.
    let origin = url.origin().ascii_serialization();
    if origin.bytes().any(|byte| {
        byte.is_ascii_whitespace() || matches!(byte, b';' | b',' | b'\'' | b'"' | b'*' | b'\\')
    }) {
        return None;
    }
    image_sources_csp(csp, &[&origin])
}

fn image_sources_csp(csp: &http::HeaderValue, sources: &[&str]) -> Option<http::HeaderValue> {
    let mut policy: HashMap<String, CspDirectiveSources> =
        Csp::Policy(csp.to_str().ok()?.to_owned()).into();
    let mut images: Vec<String> = policy
        .get("img-src")
        .or_else(|| policy.get("default-src"))
        .cloned()
        .unwrap_or_default()
        .into();
    images.retain(|source| source != "'none'");
    for source in sources {
        if !images.iter().any(|image| image == source) {
            images.push((*source).to_owned());
        }
    }
    policy.insert("img-src".into(), CspDirectiveSources::List(images));
    http::HeaderValue::from_str(&Csp::from(policy).to_string()).ok()
}

fn show_without_focus(
    window: &tauri::WebviewWindow,
    media: &MediaService,
    media_id: &str,
) -> Result<(), ()> {
    // Tao's macOS show() calls makeKeyAndOrderFront even when the builder used
    // focused(false). Suppress key eligibility only during that native call;
    // restore it immediately so later user clicks and dragging still work.
    // Keep these operations together on the main thread, without refocusing a
    // previous window (which could belong to another application by this point).
    let showing = window.clone();
    let media = media.clone();
    let media_id = media_id.to_owned();
    let (sent, received) = std::sync::mpsc::sync_channel(1);
    window
        .run_on_main_thread(move || {
            // A delayed UI callback cannot revive already-retired media.
            if media_access(showing.app_handle()).is_none()
                || !media.is_window_active(&media_id, showing.label())
            {
                let _ = sent.send(Err(()));
                return;
            }
            if showing.is_visible().unwrap_or(false) {
                let _ = sent.send(Ok(()));
                return;
            }
            // Hidden peers do not occupy stack slots. Choose the slot only at
            // reveal, and never reposition an already-visible, possibly dragged viewer.
            crate::position_camera_video_stacked(showing.app_handle(), &showing);
            #[cfg(target_os = "macos")]
            let shown = showing.set_focusable(false).and_then(|_| showing.show());
            #[cfg(target_os = "macos")]
            let restored = showing.set_focusable(true);
            #[cfg(target_os = "macos")]
            let _ = sent.send(shown.and(restored).map_err(|_| ()));
            #[cfg(not(target_os = "macos"))]
            let _ = sent.send(showing.show().map_err(|_| ()));
        })
        .map_err(|_| ())?;
    received.recv().map_err(|_| ())?
}

fn navigation_allowed(url: &reqwest::Url, development_url: Option<&reqwest::Url>) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    let packaged = url.port().is_none()
        && url.path() == "/camera-video"
        && ((url.scheme() == "tauri" && url.host_str() == Some("localhost"))
            || (matches!(url.scheme(), "http" | "https")
                && url.host_str() == Some("tauri.localhost")));
    packaged
        || development_url.is_some_and(|expected| {
            url.origin() == expected.origin() && url.path() == expected.path()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_commands_allow_only_auth_bootstrap_and_owned_controls() {
        for allowed in [
            "auth_status",
            "get_live_preview_url",
            "reveal_plugin_video_window",
            "close_plugin_video_window",
            "drag_plugin_video_window",
        ] {
            assert!(preview_command_allowed(allowed));
        }
        for forbidden in [
            "get_config",
            "save_config",
            "auth_login",
            "auth_logout",
            "plugin_action",
            "get_plugin_snapshot",
            "observe_native_media_smoke",
        ] {
            assert!(!preview_command_allowed(forbidden));
        }
    }

    #[test]
    fn owned_snapshot_sources_do_not_broaden_other_document_authority() {
        let original = http::HeaderValue::from_static(
            "default-src 'self'; img-src 'none'; script-src 'self'; frame-src 'none'",
        );
        let changed = image_sources_csp(
            &original,
            &[
                "plugin-media:",
                "http://plugin-media.localhost",
                "https://plugin-media.localhost",
            ],
        )
        .unwrap();
        let policy: HashMap<String, CspDirectiveSources> =
            Csp::Policy(changed.to_str().unwrap().into()).into();
        assert_eq!(
            Vec::<String>::from(policy["img-src"].clone()),
            [
                "plugin-media:",
                "http://plugin-media.localhost",
                "https://plugin-media.localhost"
            ]
        );
        assert_eq!(
            Vec::<String>::from(policy["script-src"].clone()),
            ["'self'"]
        );
        assert_eq!(Vec::<String>::from(policy["frame-src"].clone()), ["'none'"]);
    }

    #[test]
    fn media_routes_require_the_exact_owning_window_and_canonical_id() {
        let id = "acdb88b6-453a-4f88-8d52-5b902441fe4c";
        let label = format!("plugin-video-{id}");
        for url in [
            format!("plugin-media://localhost/{id}"),
            format!("http://plugin-media.localhost/{id}"),
        ] {
            assert_eq!(
                request_media_id(&label, &url.parse().unwrap()).as_deref(),
                Some(id)
            );
        }
        for label in ["main", "config", "camera-video-1", "plugin-video-other"] {
            assert!(request_media_id(label, &format!("/{id}").parse().unwrap()).is_none());
        }
        for path in [
            format!("/{id}?path=other"),
            format!("/{id}/other"),
            "/../clip.mp4".into(),
            format!("/{}", id.to_uppercase()),
        ] {
            assert!(request_media_id(&label, &path.parse().unwrap()).is_none());
        }
    }

    #[test]
    fn preview_identity_has_no_opaque_media_route_or_clickable_live_namespace() {
        let id = "acdb88b6-453a-4f88-8d52-5b902441fe4c";
        let label = format!("plugin-preview-{id}");
        assert!(is_plugin_preview_label(&label));
        assert!(!is_plugin_video_label(&label));
        assert_eq!(media_id_for_label(&label).as_deref(), Some(id));
        assert!(request_media_id(&label, &format!("/{id}").parse().unwrap()).is_none());
        for rejected in [
            format!("plugin-live-{id}"),
            format!("plugin-preview-{}", id.to_uppercase()),
            "plugin-preview-other".into(),
        ] {
            assert!(!is_plugin_preview_label(&rejected));
        }
    }

    #[test]
    fn live_image_csp_adds_only_the_exact_origin_and_preserves_other_directives() {
        let original = http::HeaderValue::from_static(
            "default-src 'self'; img-src 'self' data: blob:; script-src 'self' 'nonce-test'; object-src 'none'; connect-src ipc: http://ipc.localhost",
        );
        let url = reqwest::Url::parse(
            "http://camera.local:5005/prefix/api/front?token=private;script-src%20*#fragment",
        )
        .unwrap();
        let changed = live_image_csp(&original, &url).unwrap();
        let before: HashMap<String, CspDirectiveSources> =
            Csp::Policy(original.to_str().unwrap().into()).into();
        let after: HashMap<String, CspDirectiveSources> =
            Csp::Policy(changed.to_str().unwrap().into()).into();
        assert_eq!(before.len(), after.len());
        for (directive, sources) in &before {
            if directive != "img-src" {
                assert_eq!(after.get(directive), Some(sources));
            }
        }
        assert_eq!(
            Vec::<String>::from(after["img-src"].clone()),
            ["'self'", "data:", "blob:", "http://camera.local:5005"]
        );
        assert!(!changed.to_str().unwrap().contains("private"));
        assert!(!changed.to_str().unwrap().contains("/prefix"));
        let repeated = live_image_csp(&changed, &url).unwrap();
        let repeated: HashMap<String, CspDirectiveSources> =
            Csp::Policy(repeated.to_str().unwrap().into()).into();
        assert_eq!(repeated, after, "one origin cannot broaden on repeated use");
    }

    #[test]
    fn live_image_csp_preserves_inherited_defaults_and_rejects_unsafe_sources() {
        let original = http::HeaderValue::from_static("default-src 'self'; script-src 'none'");
        let url = reqwest::Url::parse("http://[::1]:5005/api/front").unwrap();
        let changed = live_image_csp(&original, &url).unwrap();
        let policy: HashMap<String, CspDirectiveSources> =
            Csp::Policy(changed.to_str().unwrap().into()).into();
        assert_eq!(
            Vec::<String>::from(policy["img-src"].clone()),
            ["'self'", "http://[::1]:5005"]
        );
        assert_eq!(
            Vec::<String>::from(policy["default-src"].clone()),
            ["'self'"]
        );
        assert_eq!(
            Vec::<String>::from(policy["script-src"].clone()),
            ["'none'"]
        );
        for value in [
            "file:///tmp/frame.jpg",
            "javascript:alert(1)",
            "https://user:password@camera.local/",
            "https://*.camera.local/",
            "https://camera.local;img-src/",
        ] {
            assert!(live_image_csp(&original, &reqwest::Url::parse(value).unwrap()).is_none());
        }
    }

    #[test]
    fn video_navigation_allows_only_packaged_or_active_development_view() {
        let dev = reqwest::Url::parse("http://localhost:1420/camera-video").unwrap();
        assert!(!navigation_allowed(&dev, None));
        assert!(navigation_allowed(&dev, Some(&dev)));
        for value in [
            "tauri://localhost/camera-video",
            "http://tauri.localhost/camera-video",
            "https://tauri.localhost/camera-video?pluginMedia=id",
        ] {
            assert!(navigation_allowed(&value.parse().unwrap(), None));
        }
        for value in [
            "tauri://other/camera-video",
            "ftp://tauri.localhost/camera-video",
            "https://tauri.localhost:9000/camera-video",
            "http://localhost:1421/camera-video",
            "http://localhost:1420/config",
            "https://remote.invalid/camera-video",
            "https://user@tauri.localhost/camera-video",
        ] {
            assert!(!navigation_allowed(&value.parse().unwrap(), Some(&dev)));
        }
    }
}
