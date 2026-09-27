//! Opt-in native media acceptance. This entry is absent from ordinary builds and
//! is never called by the production app, even when the feature is enabled.

use super::generation::{GenerationLease, RevokeOnDrop};
use super::media::MediaService;
use super::protocol::{HttpMediaKind, HttpVideoGrant, PluginManifest, WorkerConfiguration};
use super::runtime::{MediaCameraId, QueuedHttpVideo};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::Manager;
use tokio::time::{sleep, timeout};

const PLUGIN: &str = "native.media-smoke";
const PEER_TITLE: &str = "Smoke independent peer";
const STAGES: [&str; 3] = ["Smoke close", "Smoke revoke", "Smoke ended"];
const WAIT: Duration = Duration::from_secs(45);

struct Options {
    always_on_top: bool,
    live: bool,
    url: String,
    output: PathBuf,
    hold: Duration,
    minimum_loading: Duration,
    expected_live_outcome: String,
    diagnose_hidden: bool,
}

fn options(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    let mut args = args.into_iter();
    let mut url = None;
    let mut live = false;
    let mut output = None;
    let mut hold = Duration::ZERO;
    let mut minimum_loading = Duration::ZERO;
    let mut expected_live_outcome = "ready".to_owned();
    let mut diagnose_hidden = false;
    let mut always_on_top = false;
    while let Some(argument) = args.next() {
        let value = args.next().ok_or("Every option requires a value")?;
        match argument.as_str() {
            "--always-on-top" => {
                always_on_top = value
                    .parse()
                    .map_err(|_| "Invalid always-on-top preference")?
            }
            "--fixture-url" if url.is_none() => url = Some(value),
            "--live-fixture-url" if url.is_none() => {
                url = Some(value);
                live = true;
            }
            "--evidence" if output.is_none() => output = Some(PathBuf::from(value)),
            "--hold-seconds" => {
                let seconds: u64 = value.parse().map_err(|_| "Invalid hold duration")?;
                if seconds > 30 {
                    return Err("Hold duration must be at most 30 seconds".into());
                }
                hold = Duration::from_secs(seconds);
            }
            "--minimum-loading-ms" => {
                let millis: u64 = value.parse().map_err(|_| "Invalid loading duration")?;
                if millis > 12_000 {
                    return Err("Minimum loading duration must be at most 12000 ms".into());
                }
                minimum_loading = Duration::from_millis(millis);
            }
            "--expected-live-outcome"
                if ["ready", "still", "error", "timeout", "revoke-loading"]
                    .contains(&value.as_str()) =>
            {
                expected_live_outcome = value;
            }
            "--diagnose-hidden" if value == "true" => diagnose_hidden = true,
            _ => return Err("Unknown or repeated native media smoke option".into()),
        }
    }
    let result = Options {
        always_on_top,
        live,
        url: url
            .ok_or("--fixture-url or --live-fixture-url is required (explicit loopback media)")?,
        output: output.ok_or("--evidence is required (new JSON file)")?,
        hold,
        minimum_loading,
        expected_live_outcome,
        diagnose_hidden,
    };
    if !live && (minimum_loading != Duration::ZERO || result.expected_live_outcome != "ready") {
        return Err("Loading expectations require --live-fixture-url".into());
    }
    if live {
        fixture_live_grant(&result.url)?;
    } else {
        fixture_grant(&result.url)?;
    }
    if !result.output.is_absolute() || result.output.file_name().is_none() {
        return Err("Evidence path must be an absolute new file path".into());
    }
    Ok(result)
}

fn fixture_grant(value: &str) -> Result<HttpVideoGrant, String> {
    fixture_media_grant(value, false)
}

fn fixture_live_grant(value: &str) -> Result<HttpVideoGrant, String> {
    fixture_media_grant(value, true)
}

fn fixture_media_grant(value: &str, preview: bool) -> Result<HttpVideoGrant, String> {
    let url = reqwest::Url::parse(value).map_err(|_| "Invalid fixture URL")?;
    let loopback = url
        .host_str()
        .and_then(|host| {
            host.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .ok()
        })
        .is_some_and(|address| address.is_loopback());
    if !loopback || url.scheme() != "http" {
        return Err("Fixture must use plain HTTP at an explicit loopback IP".into());
    }
    let base = url.join(".").map_err(|_| "Invalid fixture base")?;
    let mut declaration = json!({
        "schema_version":1,"plugin_id":PLUGIN,"version":"0.0.0",
        "host_api":"^1.8","target":env!("INVERTER_DESKTOP_TARGET"),
        "entrypoint":"fixture", "permissions":["plugin_configuration","http_video"],
        "http_video":{"base_url_setting":"fixture_base"},
        "config_schema":{"type":"object","properties":{"fixture_base":{"type":"string"}}},
        "inventory":[],"signature":null
    });
    if preview {
        declaration["http_video"]["live_preview"] = json!({
            "query":"fps=2&height=360", "max_duration_seconds":15
        });
    }
    let manifest: PluginManifest = serde_json::from_value(declaration)
        .map_err(|_| "Cannot form native fixture declaration")?;
    let configuration = WorkerConfiguration {
        revision: "native-smoke".into(),
        values: json!({"fixture_base":base.as_str()}),
        secrets: BTreeMap::new(),
    };
    let grant = HttpVideoGrant::from_manifest_configuration(&manifest, Some(&configuration))?
        .ok_or("Fixture media grant unavailable")?;
    if preview {
        grant.validate_preview_url(value)?;
    } else {
        grant.validate_url(value)?;
    }
    Ok(grant)
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    kind: String,
    time: Option<f64>,
    duration: Option<f64>,
    width: u32,
    height: u32,
    ready_state: u8,
    error_code: Option<u8>,
}

#[derive(Serialize)]
struct ObservedVideo {
    stage: String,
    label: String,
    observation: Observation,
}

#[derive(Serialize)]
struct Evidence {
    scope: &'static str,
    passed: bool,
    failure: Option<String>,
    checks: BTreeMap<String, Value>,
    observations: Vec<ObservedVideo>,
}

struct SmokeState {
    always_on_top: bool,
    evidence: Mutex<Evidence>,
    file: Mutex<File>,
    leases: Mutex<Vec<GenerationLease>>,
    sources: Mutex<BTreeMap<String, tokio::time::Instant>>,
    bootstrapped: Mutex<BTreeSet<String>>,
    minimum_loading: Duration,
    expected_live_outcome: String,
}

impl SmokeState {
    fn check(&self, key: &str, value: Value) {
        self.evidence
            .lock()
            .unwrap()
            .checks
            .insert(key.into(), value);
    }

    fn persist(&self) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(&*self.evidence.lock().unwrap())
            .map_err(|_| "Cannot encode native evidence")?;
        let mut file = self.file.lock().unwrap();
        file.seek(SeekFrom::Start(0))
            .and_then(|_| file.write_all(&bytes))
            .and_then(|_| file.set_len(bytes.len() as u64))
            .and_then(|_| file.sync_all())
            .map_err(|_| "Cannot persist native evidence".into())
    }

    fn revoke(&self) {
        for lease in self.leases.lock().unwrap().iter() {
            lease.revoke();
        }
    }

    fn saw(&self, stage: &str, kind: &str) -> bool {
        self.evidence
            .lock()
            .unwrap()
            .observations
            .iter()
            .any(|item| item.stage == stage && item.observation.kind == kind)
    }

    fn no_playback_error(&self) -> Result<(), String> {
        if self
            .evidence
            .lock()
            .unwrap()
            .observations
            .iter()
            .any(|item| item.observation.kind == "error")
        {
            Err("Native video decoder reported an error".into())
        } else {
            Ok(())
        }
    }
}

/// This harness command never calls the normal auth/config/keychain functions.
#[tauri::command]
async fn auth_status(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, Arc<SmokeState>>,
) -> Result<Value, String> {
    if !state
        .bootstrapped
        .lock()
        .unwrap()
        .insert(window.label().into())
    {
        return Ok(json!({"enabled":false,"unlocked":true}));
    }
    let before = window
        .is_visible()
        .map_err(|_| "Cannot inspect bootstrap visibility")?;
    // Hold the real AuthGate boundary long enough for a visible bootstrap flash
    // to be observed. This belongs only to this isolated executable.
    sleep(Duration::from_millis(750)).await;
    let after = window
        .is_visible()
        .map_err(|_| "Cannot inspect bootstrap visibility")?;
    state.check(
        &format!("hidden_bootstrap:{}", window.label()),
        json!({"delay_ms":750,"visible_before":before,"visible_after":after}),
    );
    state.persist()?;
    if before || after {
        return Err("Camera window became visible during AuthGate bootstrap".into());
    }
    Ok(json!({"enabled":false,"unlocked":true}))
}

#[tauri::command]
fn get_live_preview_url(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, Arc<SmokeState>>,
) -> Result<String, String> {
    state.check(
        &format!("source_command_entered:{}", window.label()),
        json!(true),
    );
    state.persist()?;
    state
        .sources
        .lock()
        .unwrap()
        .insert(window.label().into(), tokio::time::Instant::now());
    let label = window.label().to_owned();
    let result = super::bridge::get_live_preview_url(window);
    state.check(
        &format!("source_command_succeeded:{label}"),
        json!(result.is_ok()),
    );
    state.persist()?;
    result
}

/// Observe the real DOM while it is still hidden, then use the production show
/// operation unchanged. The observer never sets a source or starts playback.
#[tauri::command]
async fn reveal_plugin_video_window(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, Arc<SmokeState>>,
) -> Result<(), String> {
    // The independent peer belongs to another plugin whose preference is absent.
    // It must remain unpinned even while the primary plugin's windows are pinned.
    let expected_on_top = state.always_on_top
        && window
            .title()
            .map_err(|_| "Cannot inspect fixture identity")?
            != PEER_TITLE;
    let always_on_top = window
        .is_always_on_top()
        .map_err(|_| "Cannot inspect video window level")?;
    state.check(
        &format!("always_on_top:{}", window.label()),
        json!({"expected":expected_on_top,"actual":always_on_top}),
    );
    if always_on_top != expected_on_top {
        return Err("Native video window ignored its saved always-on-top preference".into());
    }
    state.check(
        &format!("reveal_command_entered:{}", window.label()),
        json!(true),
    );
    state.persist()?;
    let visible = window
        .is_visible()
        .map_err(|_| "Cannot inspect pre-reveal visibility")?;
    let sample = evaluate(&window, READINESS_SAMPLE)
        .await?
        .ok_or("Cannot inspect native media before reveal")?;
    let live = super::media_windows::is_plugin_preview_label(window.label());
    let elapsed = state
        .sources
        .lock()
        .unwrap()
        .get(window.label())
        .map(tokio::time::Instant::elapsed);
    let expected_error =
        live && matches!(state.expected_live_outcome.as_str(), "error" | "timeout");
    let valid = if expected_error {
        let message = sample["message"].as_str().unwrap_or_default();
        !sample["decoded"].as_bool().unwrap_or(false)
            && if state.expected_live_outcome == "timeout" {
                message.contains("Camera media did not become ready in time.")
            } else {
                message.contains("Failed to display live camera preview.")
            }
    } else {
        sample["decoded"] == true
            && sample["patternReady"] == true
            && (!live
                || sample["pixel"]
                    .as_array()
                    .is_some_and(|pixel| pixel.len() == 4 && pixel[3] == 255))
    };
    let loading_waited = !live || elapsed.is_some_and(|duration| duration >= state.minimum_loading);
    state.check(&format!("readiness_before_reveal:{}", window.label()), json!({
        "visible":visible,"sample":sample,"valid":valid,"minimum_loading_satisfied":loading_waited,
        "loading_seconds":elapsed.map(|duration| duration.as_secs_f64()),
    }));
    state.persist()?;
    if visible || !valid || !loading_waited {
        return Err(
            "Native reveal occurred before hidden, decoded media or the expected terminal error"
                .into(),
        );
    }
    let app = window.app_handle().clone();
    let label = window.label().to_owned();
    let anchor = app
        .get_webview_window("smoke-anchor")
        .ok_or("Missing focus anchor")?;
    let anchor_before = anchor.is_focused().unwrap_or(false);
    tauri::async_runtime::spawn_blocking(move || super::bridge::reveal_plugin_video_window(window))
        .await
        .map_err(|_| "Native reveal task failed".to_owned())??;
    let anchor_after = anchor.is_focused().unwrap_or(false);
    let revealed = app
        .get_webview_window(&label)
        .ok_or("Revealed preview disappeared")?;
    let preview_focused = revealed.is_focused().unwrap_or(true);
    let revealed_on_top = revealed
        .is_always_on_top()
        .map_err(|_| "Cannot inspect revealed window level")?;
    state.check(
        &format!("always_on_top_after_reveal:{label}"),
        json!(revealed_on_top),
    );
    if revealed_on_top != expected_on_top {
        return Err("Native reveal changed the video window level".into());
    }
    state.check(&format!("focus_at_reveal:{label}"), json!({
        "anchor_before":anchor_before,"anchor_after":anchor_after,"preview_focused":preview_focused,
    }));
    state.persist()?;
    if preview_focused || (anchor_before && !anchor_after) {
        return Err("Native reveal changed the focus owner".into());
    }
    Ok(())
}

const READINESS_SAMPLE: &str = r#"(() => {
  const video = document.querySelector('video');
  const image = document.images[0];
  const width = video ? video.videoWidth : image ? image.naturalWidth : 0;
  const height = video ? video.videoHeight : image ? image.naturalHeight : 0;
  const decoded = width > 0 && height > 0 && (!video || (video.readyState >= 2 && !video.paused && !video.ended));
  let pixel = null;
  let patternReady = false;
  if (decoded) {
    try {
      const canvas = document.createElement('canvas'); canvas.width = 1; canvas.height = 1;
      const context = canvas.getContext('2d');
      patternReady = context.createPattern(image || video, 'no-repeat') !== null;
      if (image) {
        context.drawImage(image, 0, 0, 1, 1);
        pixel = Array.from(context.getImageData(0,0,1,1).data);
      }
    } catch (_) {}
  }
  return {decoded, width, height, kind:video?'video':image?'image':'none',
    readyState:video?video.readyState:null, time:video?video.currentTime:null,
    decodedFrames:video?(video.webkitDecodedFrameCount ?? null):null,
    presentedFrames:video?(video.getVideoPlaybackQuality?.().totalVideoFrames ?? null):null,
    imageComplete:image?image.complete:null, imageLoadEvents:window.__nativeSmokeImageLoads || 0,
    pixel, patternReady,
    toolbar:!!document.querySelector('[aria-label="Camera window controls"]'),
    closeButton:!!document.querySelector('button[aria-label="Close"]'),
    imageSourceAssigned:!!image?.getAttribute('src'), visibility:document.visibilityState,
    message:document.querySelector('p')?.textContent.trim() || ''};
})()"#;

#[tauri::command]
fn observe_native_media_smoke(
    window: tauri::WebviewWindow,
    observation: Observation,
    state: tauri::State<'_, Arc<SmokeState>>,
) -> Result<(), String> {
    let stage = window.title().map_err(|_| "Cannot identify smoke window")?;
    if !super::media_windows::is_plugin_video_label(window.label())
        || !STAGES.contains(&stage.as_str())
        || !matches!(
            observation.kind.as_str(),
            "metadata" | "progress" | "ended" | "error"
        )
        || observation.width > 16384
        || observation.height > 16384
        || observation.ready_state > 4
        || observation
            .time
            .is_some_and(|n| !n.is_finite() || !(0.0..=600.0).contains(&n))
        || observation
            .duration
            .is_some_and(|n| !n.is_finite() || !(0.0..=600.0).contains(&n))
    {
        return Err("Invalid native smoke observation".into());
    }
    let mut evidence = state.evidence.lock().unwrap();
    if evidence.observations.len() >= 64 {
        return Err("Native smoke observation bound reached".into());
    }
    evidence.observations.push(ObservedVideo {
        stage,
        label: window.label().into(),
        observation,
    });
    Ok(())
}

// Observe native video events without replacing its source, playback, or controls.
const OBSERVER: &str = r#"(() => {
  if (window.__nativeSmokeObserver) return;
  window.__nativeSmokeObserver = true;
  window.__nativeSmokeImageLoads = 0;
  // Fixture-only CORS opt-in lets this observer read pixels from the actual
  // production image, rather than proving a second independently loaded image.
  // Install before AuthGate resolves and Vue creates the media element.
  const createElement = Document.prototype.createElement;
  Document.prototype.createElement = function(...args) {
    const element = createElement.apply(this, args);
    if (element instanceof HTMLImageElement) element.crossOrigin = 'anonymous';
    return element;
  };
  document.addEventListener('load', event => {
    if (event.target instanceof HTMLImageElement && event.target.isConnected) window.__nativeSmokeImageLoads++;
  }, true);
  const sent = new Set();
  function report(video, kind) {
    if (!(video instanceof HTMLVideoElement) || !window.__TAURI_INTERNALS__ || sent.has(kind)) return;
    sent.add(kind);
    void window.__TAURI_INTERNALS__.invoke('observe_native_media_smoke', {observation: {
      kind, time: Number.isFinite(video.currentTime) ? video.currentTime : null,
      duration: Number.isFinite(video.duration) ? video.duration : null,
      width: video.videoWidth, height: video.videoHeight, ready_state: video.readyState,
      error_code: video.error ? video.error.code : null
    }}).catch(() => {});
  }
  function inspect(video) {
    if (!(video instanceof HTMLVideoElement)) return;
    if (video.readyState >= 1) report(video, 'metadata');
    if (video.currentTime >= 1.2 && video.readyState >= 2) report(video, 'progress');
    if (video.error) report(video, 'error');
    if (video.ended) report(video, 'ended');
  }
  for (const type of ['loadedmetadata', 'timeupdate', 'error', 'ended']) {
    document.addEventListener(type, event => {
      if (type === 'ended') report(event.target, 'ended');
      else inspect(event.target);
    }, true);
  }
  new MutationObserver(() => inspect(document.querySelector('video')))
    .observe(document, {subtree:true, childList:true});
  inspect(document.querySelector('video'));
})()"#;

async fn until<T>(mut read: impl FnMut() -> Result<Option<T>, String>) -> Result<T, String> {
    timeout(WAIT, async {
        loop {
            if let Some(value) = read()? {
                break Ok(value);
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| "Timed out waiting for real native media evidence")?
}

async fn playing_window(
    app: &tauri::AppHandle,
    state: &SmokeState,
    stage: &str,
) -> Result<tauri::WebviewWindow, String> {
    until(|| {
        state.no_playback_error()?;
        if !state.saw(stage, "metadata") || !state.saw(stage, "progress") {
            return Ok(None);
        }
        let decoded = state
            .evidence
            .lock()
            .unwrap()
            .observations
            .iter()
            .any(|item| {
                item.stage == stage
                    && item.observation.kind == "progress"
                    && item.observation.width > 0
                    && item.observation.height > 0
                    && item.observation.ready_state >= 2
                    && item.observation.time.is_some_and(|time| time >= 1.2)
                    && item
                        .observation
                        .duration
                        .is_some_and(|duration| duration >= 1.2)
                    && item.observation.error_code.is_none()
            });
        if !decoded {
            return Err(
                "Native progress has no decoded video dimensions or time advancement".into(),
            );
        }
        Ok(app.webview_windows().into_values().find(|window| {
            window.title().is_ok_and(|title| title == stage) && window.is_visible().unwrap_or(false)
        }))
    })
    .await
}

fn submit(
    media: &MediaService,
    state: &SmokeState,
    options: &Options,
    sequence: usize,
) -> Result<GenerationLease, String> {
    let lease = GenerationLease::new(PLUGIN.into(), 1, sequence as u64 + 1);
    state.leases.lock().unwrap().push(lease.clone());
    media
        .try_submit(QueuedHttpVideo {
            camera_id: Some(MediaCameraId::Explicit(format!("clip-{sequence}"))),
            live_preview: false,
            media_kind: HttpMediaKind::Video,
            lease: lease.clone(),
            grant: fixture_grant(&options.url)?,
            id: format!("native-smoke-{sequence}"),
            url: options.url.clone(),
            title: STAGES[sequence].into(),
        })
        .map_err(|_| "Native media admission failed")?;
    Ok(lease)
}

fn geometry(window: &tauri::WebviewWindow) -> Result<Value, String> {
    let position = window
        .outer_position()
        .map_err(|_| "Window position unavailable")?;
    let size = window.outer_size().map_err(|_| "Window size unavailable")?;
    let monitor = window
        .current_monitor()
        .map_err(|_| "Monitor unavailable")?
        .ok_or("Window has no monitor")?;
    let work_area = monitor.work_area();
    let origin = &work_area.position;
    let bounds = &work_area.size;
    let contained = position.x >= origin.x
        && position.y >= origin.y
        && i64::from(position.x) + i64::from(size.width)
            <= i64::from(origin.x) + i64::from(bounds.width)
        && i64::from(position.y) + i64::from(size.height)
            <= i64::from(origin.y) + i64::from(bounds.height);
    if !contained || size.width == 0 || size.height == 0 {
        return Err("Native media window is outside its monitor bounds".into());
    }
    Ok(
        json!({"x":position.x,"y":position.y,"width":size.width,"height":size.height,
        "visible":window.is_visible().unwrap_or(false),"focused":window.is_focused().unwrap_or(false),
        "contained":contained}),
    )
}

/// Native evaluation returns structured observations directly, without granting
/// remote pages an IPC capability or adding a production frontend route.
async fn live_sample(window: &tauri::WebviewWindow) -> Result<Option<Value>, String> {
    let value = evaluate(window, r#"(() => {
      try {
        const image = document.images[0];
        if (!image || !image.naturalWidth || !image.naturalHeight) return null;
        const canvas = document.createElement('canvas'); canvas.width = 1; canvas.height = 1;
        const context = canvas.getContext('2d'); context.drawImage(image, 0, 0, 1, 1);
        return {toolbar:!!document.querySelector('[aria-label="Camera window controls"]'),closeButton:!!document.querySelector('button[aria-label="Close"]'),width:image.naturalWidth,height:image.naturalHeight,pixel:Array.from(context.getImageData(0,0,1,1).data)};
      } catch (_) { return null; }
    })()"#).await?;
    Ok(value.filter(|value| {
        value["width"].as_u64().is_some_and(|n| n > 0 && n <= 16384)
            && value["height"]
                .as_u64()
                .is_some_and(|n| n > 0 && n <= 16384)
            && value["pixel"].as_array().is_some_and(|p| p.len() == 4)
    }))
}

async fn evaluate(window: &tauri::WebviewWindow, script: &str) -> Result<Option<Value>, String> {
    let (sent, received) = tokio::sync::oneshot::channel();
    let sent = Mutex::new(Some(sent));
    window
        .eval_with_callback(script, move |value| {
            if let Some(sent) = sent.lock().unwrap().take() {
                let _ = sent.send(value);
            }
        })
        .map_err(|_| "Cannot inspect native live image")?;
    // Wry queues pre-navigation JS strings but drops their callback until
    // didCommitNavigation. Treat that initial cancellation as not loaded yet;
    // the caller still enforces its overall bounded decoding deadline.
    let value = match timeout(Duration::from_secs(2), received).await {
        Ok(Ok(value)) if !value.is_empty() => value,
        _ => return Ok(None),
    };
    let value: Value =
        serde_json::from_str(&value).map_err(|_| "Invalid native live observation")?;
    Ok((!value.is_null()).then_some(value))
}

async fn exercise_live(
    app: &tauri::AppHandle,
    state: &SmokeState,
    media: &MediaService,
    anchor: &tauri::WebviewWindow,
    options: &Options,
) -> Result<(), String> {
    let scenarios = if matches!(options.expected_live_outcome.as_str(), "ready" | "still") {
        vec![(10, false), (11, true)]
    } else {
        vec![(10, false)]
    };
    for (sequence, revoke) in scenarios {
        // Each independent scenario needs a fresh focus precondition. The user
        // may have activated another app during the preceding 15-second wait.
        anchor
            .set_focus()
            .map_err(|_| "Cannot focus live scenario anchor")?;
        until(|| Ok(anchor.is_focused().unwrap_or(false).then_some(()))).await?;
        let lease = GenerationLease::new(PLUGIN.into(), 1, sequence);
        state.leases.lock().unwrap().push(lease.clone());
        let _owner = RevokeOnDrop(lease.clone());
        let title = if revoke {
            "Smoke live revoke"
        } else {
            "Smoke live expiry"
        };
        let started = tokio::time::Instant::now();
        media
            .try_submit(QueuedHttpVideo {
                camera_id: Some(MediaCameraId::Explicit("front".into())),
                live_preview: true,
                media_kind: HttpMediaKind::Video,
                lease: lease.clone(),
                grant: fixture_live_grant(&options.url)?,
                id: format!("native-live-{sequence}"),
                url: options.url.clone(),
                title: title.into(),
            })
            .map_err(|_| "Cannot admit native live fixture")?;
        let expected_title = title;
        let window = until(|| {
            Ok(app
                .webview_windows()
                .into_values()
                .find(|window| window.title().is_ok_and(|title| title == expected_title)))
        })
        .await?;
        if window.is_visible().unwrap_or(true) {
            return Err("Live preview was already visible during native creation".into());
        }
        let duplicate_during_loading = !revoke && options.expected_live_outcome == "ready";
        if duplicate_during_loading {
            media
                .try_submit(QueuedHttpVideo {
                    camera_id: Some(MediaCameraId::Explicit("front".into())),
                    live_preview: true,
                    media_kind: HttpMediaKind::Video,
                    lease: lease.clone(),
                    grant: fixture_live_grant(&options.url)?,
                    id: "native-live-duplicate".into(),
                    url: options.url.clone(),
                    title: "Unexpected duplicate camera".into(),
                })
                .map_err(|_| "Duplicate fixture should be harmlessly coalesced")?;
        }
        if options.expected_live_outcome == "revoke-loading" {
            until(|| {
                if window.is_visible().unwrap_or(true) {
                    return Err("Undecoded revoked fixture became visible".into());
                }
                Ok(state
                    .sources
                    .lock()
                    .unwrap()
                    .get(window.label())
                    .is_some_and(|start| {
                        start.elapsed() >= options.minimum_loading.max(Duration::from_secs(1))
                    })
                    .then_some(()))
            })
            .await?;
            let reveal_requested = state
                .evidence
                .lock()
                .unwrap()
                .checks
                .contains_key(&format!("reveal_command_entered:{}", window.label()));
            if reveal_requested {
                return Err("Stalled fixture requested reveal before revocation".into());
            }
            lease.revoke();
            until(|| {
                Ok(app
                    .get_webview_window(window.label())
                    .is_none()
                    .then_some(()))
            })
            .await?;
            until(|| Ok((!media.has_owned_work()).then_some(()))).await?;
            state.check(
                "revoked_while_loading_stayed_hidden",
                json!({"reveal_requested":reveal_requested,"visible_before_revocation":false,"elapsed_seconds":started.elapsed().as_secs_f64()}),
            );
            return Ok(());
        }
        let mut diagnostics = [false; 2];
        timeout(WAIT, async {
            loop {
                let media_window_count = app
                    .webview_windows()
                    .values()
                    .filter(|candidate| {
                        super::media_windows::media_id_for_label(candidate.label()).is_some()
                    })
                    .count();
                if duplicate_during_loading && media_window_count > 1 {
                    state.check("duplicate_window_count", json!(media_window_count));
                    return Err("Same camera opened a second native window while loading".into());
                }
                if app.get_webview_window(window.label()).is_none() {
                    return Err(
                        "Live preview expired without revealing media or its terminal error".into(),
                    );
                }
                if window.is_visible().unwrap_or(false) {
                    return Ok::<(), String>(());
                }
                for (index, seconds) in [3, 11].into_iter().enumerate() {
                    if options.diagnose_hidden
                        && !diagnostics[index]
                        && started.elapsed() >= Duration::from_secs(seconds)
                    {
                        diagnostics[index] = true;
                        let sample = evaluate(&window, READINESS_SAMPLE).await?;
                        state.check(
                            &format!("hidden_loading_at_{seconds}s:{}", window.label()),
                            json!({
                                "sample":sample,"media_window_count":media_window_count,
                                "visible":window.is_visible().unwrap_or(false),
                            }),
                        );
                        state.persist()?;
                    }
                }
                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .map_err(|_| "Timed out waiting for native live reveal")??;
        if duplicate_during_loading {
            state.check(
                "same_camera_duplicate_while_loading_suppressed",
                json!(true),
            );
        }
        let window_geometry = geometry(&window)?;
        let scale = window
            .scale_factor()
            .map_err(|_| "Window scale unavailable")?;
        if window.is_decorated().unwrap_or(true)
            || window_geometry["width"].as_u64()
                != Some((crate::CAMERA_VIDEO_WINDOW_W * scale).round() as u64)
            || window_geometry["height"].as_u64()
                != Some((crate::CAMERA_VIDEO_WINDOW_H * scale).round() as u64)
        {
            return Err("Live preview must match the frameless clip size".into());
        }
        if !matches!(options.expected_live_outcome.as_str(), "ready" | "still") {
            let focused =
                anchor.is_focused().unwrap_or(false) && !window.is_focused().unwrap_or(true);
            let sample = evaluate(&window, READINESS_SAMPLE)
                .await?
                .ok_or("Cannot inspect terminal live fixture")?;
            if window.is_focused().unwrap_or(true)
                || sample["toolbar"] != true
                || sample["closeButton"] != true
            {
                return Err("Terminal live preview lost controls or anchor focus".into());
            }
            state.check(
                "terminal_live_error_visible_without_focus",
                json!({"sample":sample,"focus_preserved":focused,"geometry":window_geometry}),
            );
            until(|| {
                Ok(app
                    .get_webview_window(window.label())
                    .is_none()
                    .then_some(()))
            })
            .await?;
            let seconds = started.elapsed().as_secs_f64();
            if !(14.0..=25.0).contains(&seconds) {
                return Err("Terminal error extended the live preview grant lifetime".into());
            }
            until(|| Ok((!media.has_owned_work()).then_some(()))).await?;
            state.check(
                "terminal_error_kept_15_second_expiry",
                json!({"elapsed_seconds":seconds}),
            );
            return Ok(());
        }
        let first = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(sample) = live_sample(&window).await? {
                    break Ok::<_, String>(sample);
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| "MJPEG fixture never decoded an image")??;
        let second = if options.expected_live_outcome == "still" {
            Value::Null
        } else {
            timeout(Duration::from_secs(5), async {
                loop {
                    sleep(Duration::from_millis(200)).await;
                    if let Some(sample) = live_sample(&window).await? {
                        if sample["pixel"] != first["pixel"] {
                            break Ok::<_, String>(sample);
                        }
                    }
                }
            })
            .await
            .map_err(|_| "MJPEG fixture did not advance between distinct frames")??
        };
        if first["toolbar"] != true || first["closeButton"] != true {
            return Err("Live preview is missing the shared camera controls".into());
        }
        let anchor_focused = anchor.is_focused().unwrap_or(false);
        let preview_focused = window.is_focused().unwrap_or(true);
        let focused = anchor_focused && !preview_focused;
        state.check(
            title,
            json!({"geometry":window_geometry,"first_decoded_frame":first,
            "second_decoded_frame":second,"focus_preserved":focused,
            "anchor_focused":anchor_focused,"preview_focused":preview_focused}),
        );
        state.persist()?;
        if preview_focused {
            return Err("Live preview stole focus from the native anchor".into());
        }
        if !revoke {
            // Independently leased previews share the exact same owned viewer and stack.
            let peer_lease = GenerationLease::new("native.camera-smoke".into(), 1, 12);
            state.leases.lock().unwrap().push(peer_lease.clone());
            let _peer_owner = RevokeOnDrop(peer_lease.clone());
            media
                .try_submit(QueuedHttpVideo {
                    camera_id: Some(MediaCameraId::Explicit("peer".into())),
                    live_preview: true,
                    media_kind: HttpMediaKind::Video,
                    lease: peer_lease.clone(),
                    grant: fixture_live_grant(&options.url)?,
                    id: "native-live-peer".into(),
                    url: options.url.clone(),
                    title: PEER_TITLE.into(),
                })
                .map_err(|_| "Cannot admit native camera peer")?;
            let peer = until(|| {
                Ok(app.webview_windows().into_values().find(|candidate| {
                    candidate.title().is_ok_and(|title| title == PEER_TITLE)
                        && candidate.is_visible().unwrap_or(false)
                }))
            })
            .await?;
            let peer_geometry = geometry(&peer)?;
            let stacked = peer_geometry["x"] == window_geometry["x"]
                && peer_geometry["y"].as_i64()
                    == window_geometry["y"]
                        .as_i64()
                        .zip(window_geometry["height"].as_i64())
                        .map(|(y, h)| y + h)
                && peer_geometry["width"] == window_geometry["width"]
                && peer_geometry["height"] == window_geometry["height"];
            let peer_focus =
                anchor.is_focused().unwrap_or(false) && !peer.is_focused().unwrap_or(true);
            state.check("independent_previews_shared_stack", json!({"first":window_geometry,"second":peer_geometry,"exact_order_and_size":stacked,"focus_preserved":peer_focus}));
            if !stacked || peer.is_focused().unwrap_or(true) {
                return Err("Independent previews did not share the same ordered stack".into());
            }
            // Exercise the shared Vue Close button with a live owner, not just
            // the downloaded-media path or native destruction from the harness.
            peer.eval("document.querySelector('button[aria-label=\"Close\"]')?.click()")
                .map_err(|_| "Cannot activate live preview close control")?;
            until(|| Ok(app.get_webview_window(peer.label()).is_none().then_some(()))).await?;
            state.check("live_real_close_control_destroyed_window", json!(true));
        }
        if revoke {
            lease.revoke();
        }
        until(|| {
            Ok(app
                .get_webview_window(window.label())
                .is_none()
                .then_some(()))
        })
        .await?;
        let seconds = started.elapsed().as_secs_f64();
        if !revoke && !(14.0..=25.0).contains(&seconds) {
            return Err("Live preview lifetime was not bounded to 15 seconds".into());
        }
        state.check(
            if revoke {
                "live_revocation_destroyed_window"
            } else {
                "live_15_second_expiry"
            },
            json!({"elapsed_seconds":seconds}),
        );
        until(|| Ok((!media.has_owned_work()).then_some(()))).await?;
    }
    Ok(())
}

async fn exercise(
    app: tauri::AppHandle,
    state: Arc<SmokeState>,
    profile: PathBuf,
    options: Options,
) -> Result<(), String> {
    let media = super::bridge::install_native_media_smoke(
        &app,
        profile.join("plugins"),
        PLUGIN,
        options.always_on_top,
    )
    .await?;
    state.check("isolated_ready", json!(true));
    state.check("anchor_focused", json!(false));
    state.persist()?;
    let anchor = app
        .get_webview_window("smoke-anchor")
        .ok_or("Focus anchor missing")?;
    anchor
        .set_focus()
        .map_err(|_| "Cannot focus smoke anchor")?;
    until(|| {
        Ok(anchor
            .is_focused()
            .map_err(|_| "Anchor focus unavailable")?
            .then_some(()))
    })
    .await
    .map_err(|_| "Focus precondition failed: activate the Native Media Smoke anchor window before testing focus preservation")?;
    state.check("anchor_focused", json!(true));
    state.persist()?;
    if options.live {
        return exercise_live(&app, &state, &media, &anchor, &options).await;
    }

    let first_lease = submit(&media, &state, &options, 0)?;
    let _first_owner = RevokeOnDrop(first_lease.clone());
    let first = playing_window(&app, &state, STAGES[0]).await?;
    let second_lease = submit(&media, &state, &options, 1)?;
    let _second_owner = RevokeOnDrop(second_lease.clone());
    let second = playing_window(&app, &state, STAGES[1]).await?;
    let first_geometry = geometry(&first)?;
    let second_geometry = geometry(&second)?;
    let bounds = |value: &Value| {
        let x = value["x"].as_i64().expect("native x");
        let y = value["y"].as_i64().expect("native y");
        let width = value["width"].as_i64().expect("native width");
        let height = value["height"].as_i64().expect("native height");
        (x, y, x + width, y + height)
    };
    let (ax, ay, ar, ab) = bounds(&first_geometry);
    let (bx, by, br, bb) = bounds(&second_geometry);
    let separated = ar <= bx || br <= ax || ab <= by || bb <= ay;
    let focus_preserved = anchor.is_focused().unwrap_or(false)
        && !first.is_focused().unwrap_or(true)
        && !second.is_focused().unwrap_or(true);
    state.check(
        "two_native_windows",
        json!({"first":first_geometry,"second":second_geometry,
        "non_overlapping":separated,"anchor_focus_preserved":focus_preserved}),
    );
    if !separated || first.is_focused().unwrap_or(true) || second.is_focused().unwrap_or(true) {
        return Err("Native stacking or focus preservation failed".into());
    }
    let first_id = super::media_windows::media_id_for_label(first.label())
        .ok_or("Missing first media identity")?;
    let second_id = super::media_windows::media_id_for_label(second.label())
        .ok_or("Missing second media identity")?;
    let head = media
        .read_range(&first_id, first.label(), None, true)
        .await
        .map_err(|_| "Native media HEAD failed")?;
    let total = head.content_length;
    if total <= 1024 * 1024 {
        return Err("MP4 fixture must be larger than one MiB for range proof".into());
    }
    drop(head);
    let range = media
        .read_range(&first_id, first.label(), Some("bytes=0-"), false)
        .await
        .map_err(|_| "Native media range read failed")?;
    let bounded = range.status == 206
        && range.bytes.len() == 1024 * 1024
        && range.content_range == Some(format!("bytes 0-1048575/{total}"));
    state.check(
        "backend_range",
        json!({"status":range.status,"bytes":range.bytes.len(),
        "content_range":range.content_range,"whole_file_bytes":total,"bounded":bounded}),
    );
    drop(range);
    let wrong_window_denied = media
        .read_range(&first_id, second.label(), Some("bytes=0-1"), false)
        .await
        .is_err();
    state.check("backend_wrong_window_denied", json!(wrong_window_denied));
    if !bounded || !wrong_window_denied {
        return Err("Native range ownership check failed".into());
    }
    state.persist()?;
    sleep(options.hold).await;
    if app.get_webview_window(first.label()).is_none()
        || app.get_webview_window(second.label()).is_none()
    {
        return Err("Fixture ended before the requested observation hold completed".into());
    }
    // Exercise the production Vue button and its owned-close IPC, not destroy().
    first
        .eval("document.querySelector('button[aria-label=\"Close\"]')?.click()")
        .map_err(|_| "Cannot activate real video close control")?;
    until(|| {
        Ok(app
            .get_webview_window(first.label())
            .is_none()
            .then_some(()))
    })
    .await?;
    state.check("real_close_control_destroyed_window", json!(true));
    if media.is_window_active(&first_id, first.label()) {
        return Err("User-closed media remains accessible".into());
    }
    second_lease.revoke();
    let revoked = !media.is_window_active(&second_id, second.label());
    state.check("revoked_access_before_native_close", json!(revoked));
    if !revoked {
        return Err("Revoked media remained accessible".into());
    }
    until(|| {
        Ok(app
            .get_webview_window(second.label())
            .is_none()
            .then_some(()))
    })
    .await?;
    until(|| Ok((!media.has_owned_work()).then_some(()))).await?;
    state.check("revocation_destroyed_window", json!(true));

    let final_lease = submit(&media, &state, &options, 2)?;
    let _final_owner = RevokeOnDrop(final_lease);
    let final_window = playing_window(&app, &state, STAGES[2]).await?;
    until(|| {
        state.no_playback_error()?;
        Ok(state.saw(STAGES[2], "ended").then_some(()))
    })
    .await?;
    until(|| {
        Ok(app
            .get_webview_window(final_window.label())
            .is_none()
            .then_some(()))
    })
    .await?;
    until(|| Ok((!media.has_owned_work()).then_some(()))).await?;
    state.check("ended_event_and_automatic_close", json!(true));
    let empty = std::fs::read_dir(profile.join("desktop-plugin-media"))
        .map_err(|_| "Cannot inspect owned media cleanup")?
        .next()
        .is_none();
    state.check("owned_media_directory_empty", json!(empty));
    if !empty {
        return Err("Owned media files remain after window cleanup".into());
    }
    Ok(())
}

/// Launch the isolated native harness from its required-feature example only.
/// Arguments: --fixture-url http://127.0.0.1:PORT/prefix/clip.mp4
/// --evidence /absolute/new-evidence.json [--hold-seconds 0..30].
pub fn run() -> Result<(), String> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments == ["--help"] {
        println!("plugin-media-smoke --fixture-url http://127.0.0.1:PORT/prefix/clip.mp4 --evidence /absolute/new-evidence.json [--hold-seconds 0..30]");
        println!("Use a decodable video MP4 larger than one MiB, lasting 8–40 seconds (at least hold + 6 seconds). No production services or settings are opened.");
        println!("Live mode: --live-fixture-url 'http://127.0.0.1:PORT/prefix/api/front?fps=2&height=360' --evidence /absolute/new.json. Serve MJPEG with alternating distinct frames for native decoding and 15-second expiry proof.");
        println!("Live readiness proof: [--minimum-loading-ms 0..12000] [--expected-live-outcome ready|still|error|timeout|revoke-loading]. Every window must remain hidden during an isolated 750 ms AuthGate bootstrap and until its decoded media or expected terminal error.");
        println!("Diagnostic only: --diagnose-hidden true evaluates the hidden DOM at 3s/11s and may wake a throttled webview; do not use this option as hidden-readiness acceptance.");
        println!("Window preference: [--always-on-top true|false] seeds isolated encrypted plugin settings and verifies the native window level before and after reveal (default false).");
        return Ok(());
    }
    let options = options(arguments)?;
    let mut open = OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    let output = open
        .open(&options.output)
        .map_err(|_| "Evidence file must be new and writable")?;
    let profile = tempfile::Builder::new()
        .prefix("inverter-native-media-smoke-")
        .tempdir()
        .map_err(|_| "Cannot create private smoke profile")?;
    let state = Arc::new(SmokeState {
        always_on_top: options.always_on_top,
        evidence: Mutex::new(Evidence { scope:"Native fixture lease; actual media service, route, Vue player and OS windows. Signed worker/MQTT are proved separately.",
            passed:false,failure:None,checks:BTreeMap::new(),observations:Vec::new() }),
        file: Mutex::new(output), leases:Mutex::new(Vec::new()),
        sources: Mutex::new(BTreeMap::new()), minimum_loading:options.minimum_loading,
        bootstrapped: Mutex::new(BTreeSet::new()),
        expected_live_outcome:options.expected_live_outcome.clone(),
    });
    let mut context = tauri::generate_context!();
    context.config_mut().identifier = format!(
        "com.alvit.native-media-smoke.{}",
        uuid::Uuid::new_v4().simple()
    );
    context.config_mut().product_name = Some("Native Media Smoke".into());
    context.config_mut().app.windows.clear();
    context.config_mut().build.dev_url = None;
    context.config_mut().app.security.asset_protocol.enable = false;
    context.package_info_mut().name = "Native Media Smoke".into();
    let setup_state = state.clone();
    let root = profile
        .path()
        .canonicalize()
        .map_err(|_| "Cannot canonicalize private smoke profile")?;
    let builder = super::media_windows::register(tauri::Builder::default())
        .append_invoke_initialization_script(format!(";{OBSERVER};"))
        .manage(state.clone())
        .invoke_handler(|invoke| {
            if super::media_windows::media_id_for_label(invoke.message.webview().label()).is_some()
                && !super::media_windows::preview_command_allowed(invoke.message.command())
                // The observer is harness-only and validates its exact fixture owner.
                && invoke.message.command() != "observe_native_media_smoke"
            {
                invoke
                    .resolver
                    .reject("Media windows can only access their own media controls");
                return true;
            }
            let handler: fn(tauri::ipc::Invoke) -> bool = tauri::generate_handler![
                auth_status,
                observe_native_media_smoke,
                get_live_preview_url,
                reveal_plugin_video_window,
                super::bridge::close_plugin_video_window,
                super::bridge::drag_plugin_video_window
            ];
            handler(invoke)
        })
        .setup(move |app| {
            tauri::WebviewWindowBuilder::new(
                app,
                "smoke-anchor",
                tauri::WebviewUrl::External("about:blank".parse()?),
            )
            .incognito(true)
            .title("Native media smoke — focus anchor")
            .inner_size(460.0, 140.0)
            .focused(true)
            .build()?;
            let app = app.handle().clone();
            let run_state = setup_state.clone();
            tauri::async_runtime::spawn(async move {
                let result = timeout(
                    Duration::from_secs(180),
                    exercise(app.clone(), run_state.clone(), root, options),
                )
                .await
                .map_err(|_| "Native smoke overall deadline expired".to_owned())
                .and_then(|result| result);
                run_state.revoke();
                let failed = result.is_err();
                {
                    let mut evidence = run_state.evidence.lock().unwrap();
                    // A completed scenario is not a completed native shutdown.
                    evidence
                        .checks
                        .insert("scenario_completed".into(), json!(!failed));
                    evidence.failure = result.err();
                }
                let _ = run_state.persist();
                app.exit(i32::from(failed));
            });
            Ok(())
        });
    let app = match builder.build(context) {
        Ok(app) => app,
        Err(_) => {
            state.evidence.lock().unwrap().failure =
                Some("Cannot build isolated native application".into());
            state.persist()?;
            return Err("Cannot build isolated native application".into());
        }
    };
    let exit_state = state.clone();
    let exit_code = app.run_return(move |app, event| {
        if matches!(
            &event,
            tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit
        ) {
            exit_state.revoke();
        }
        super::bridge::on_run_event(app, event);
    });
    state.check("native_exit_code", json!(exit_code));
    let profile_closed = profile.close().is_ok();
    state.check("private_profile_removed", json!(profile_closed));
    {
        let mut evidence = state.evidence.lock().unwrap();
        evidence.passed = exit_code == 0
            && profile_closed
            && evidence.checks.get("scenario_completed") == Some(&json!(true))
            && evidence.failure.is_none();
        if !evidence.passed && evidence.failure.is_none() {
            evidence.failure =
                Some("Native exit or private profile cleanup did not complete successfully".into());
        }
    }
    state.persist()?;
    if !state.evidence.lock().unwrap().passed {
        return Err("Native smoke did not pass; inspect its structured evidence".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixture_input_is_explicit_loopback_without_url_ambiguity() {
        assert!(fixture_grant("http://127.0.0.1:9000/prefix/clip.mp4").is_ok());
        for value in [
            "http://example.com/clip.mp4",
            "http://localhost/clip.mp4",
            "http://127.0.0.1/clip.mp4?secret=value",
            "http://user@127.0.0.1/clip.mp4",
            "http://127.0.0.1/prefix/%2e%2e/clip.mp4",
        ] {
            assert!(fixture_grant(value).is_err());
        }
        assert!(options(Vec::new()).is_err());
        let relative = options(
            [
                "--fixture-url",
                "http://127.0.0.1:9000/clip.mp4",
                "--evidence",
                "relative.json",
            ]
            .map(str::to_owned),
        );
        assert_eq!(
            relative.err().as_deref(),
            Some("Evidence path must be an absolute new file path")
        );
        let absolute = std::env::temp_dir().join("native-smoke-options-only.json");
        assert!(options(vec![
            "--fixture-url".into(),
            "http://127.0.0.1:9000/clip.mp4".into(),
            "--evidence".into(),
            absolute.to_string_lossy().into_owned(),
        ])
        .is_ok());
    }
}
