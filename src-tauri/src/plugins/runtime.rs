//! App-wide, desktop-only supervision for explicitly trusted worker executables.
//!
//! This is a process boundary, not an OS sandbox. Package discovery and executable
//! authorization belong to the signed package layer; the webview cannot start
//! an executable. Workers receive no inherited environment or core service handles.

use super::generation::{GenerationLease, RevokeOnDrop};
use super::protocol::{
    encode_host_frame, parse_worker_frame, validate_handshake, validate_host_message,
    validate_plugin_id, DashboardContribution, HostMessage, HttpMediaKind, HttpVideoGrant,
    NumberInputGrant, WorkerConfiguration, WorkerMessage, HOST_API_VERSION, MAX_ACTION_DEADLINE_MS,
    MAX_FRAME_BYTES, PROTOCOL_VERSION,
};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::future::poll_fn;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{self, Instant};

const MAX_WORKERS: usize = 8;
const MAX_IN_FLIGHT: usize = 16;
const CONTROL_QUEUE_CAPACITY: usize = 32;
const PIPE_QUEUE_CAPACITY: usize = 16;
const MAX_RESTARTS: u32 = 2;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(3);
const REAP_TIMEOUT: Duration = Duration::from_secs(2);
const STOP_GRACE: Duration = Duration::from_millis(200);
const RESTART_BACKOFF: Duration = Duration::from_millis(100);
const MAX_FRAMES_PER_SECOND: usize = 120;
const MAX_CONTRIBUTIONS_PER_SECOND: usize = 10;
const DEADLINE_TICK: Duration = Duration::from_millis(10);
const NOTIFICATION_QUEUE_CAPACITY: usize = 16;
const NOTIFICATIONS_PER_WORKER_PER_MINUTE: usize = 30;
const NOTIFICATIONS_GLOBAL_PER_MINUTE: usize = 120;
const NOTIFICATION_SEEN_CAPACITY: usize = 512;
const NOTIFICATION_DEDUP_TTL: Duration = Duration::from_secs(10 * 60);
const NOTIFICATION_DELIVERY_TTL: Duration = Duration::from_secs(30);

const HTTP_VIDEO_QUEUE_CAPACITY: usize = 4;

/// Stable camera identity for native admission only, never public snapshots.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum MediaCameraId {
    Explicit(String),
    HttpPreview(String),
}

#[derive(PartialEq, Eq)]
enum MediaCooldownId {
    Camera(MediaCameraId),
    LegacyTitle(String),
}

/// Native ownership only: URLs/titles never enter public worker snapshots.
pub(crate) struct QueuedHttpVideo {
    pub camera_id: Option<MediaCameraId>,
    pub live_preview: bool,
    pub lease: GenerationLease,
    pub grant: HttpVideoGrant,
    pub id: String,
    pub url: String,
    pub title: String,
    pub media_kind: HttpMediaKind,
}

#[derive(Default)]
struct HttpVideoState {
    pending: VecDeque<(QueuedHttpVideo, Instant)>,
    seen: VecDeque<(String, Instant)>,
    cooldowns: VecDeque<(MediaCooldownId, Instant)>,
    rate: NotificationRate,
}

struct MediaAdmission {
    kind: HttpMediaKind,
    cooldown_id: Option<String>,
    live_preview: bool,
}

type ChangeCallback = Arc<dyn Fn() + Send + Sync>;
type ActionReply = oneshot::Sender<Result<Value, PluginError>>;

/// Native delivery only. Never serialize notification content into app snapshots.
pub(crate) struct DesktopNotification {
    pub plugin_id: String,
    pub id: String,
    pub title: String,
    pub body: String,
    pub live_view: Option<LiveViewRequest>,
}

#[derive(Clone)]
pub(crate) struct LiveViewRequest {
    pub lease: GenerationLease,
    pub id: String,
    pub url: reqwest::Url,
}

struct QueuedNotification {
    notification: DesktopNotification,
    generation: u64,
    created: Instant,
}

struct NotificationRate {
    window: Instant,
    count: usize,
}

impl Default for NotificationRate {
    fn default() -> Self {
        Self {
            window: Instant::now(),
            count: 0,
        }
    }
}

impl NotificationRate {
    fn refresh(&mut self) {
        if self.window.elapsed() >= Duration::from_secs(60) {
            self.window = Instant::now();
            self.count = 0;
        }
    }
}

#[derive(Default)]
struct NotificationState {
    pending: VecDeque<QueuedNotification>,
    seen: VecDeque<(String, Instant)>,
    rate: NotificationRate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    Starting,
    Running,
    Restarting,
    Stopped,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub struct PluginSnapshot {
    pub plugin_id: String,
    pub state: WorkerState,
    pub generation: u64,
    /// Opaque identity of the actual process, including after reinstall.
    pub instance_id: Option<String>,
    pub restart_count: u32,
    pub contributions: Vec<DashboardContribution>,
    pub presentation: Vec<super::presentation::Presentation>,
    /// Host-owned diagnostic codes only. Worker stderr and response text never enter this field.
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginError {
    InvalidWorker,
    AlreadyRegistered,
    Capacity,
    HostStopped,
    UnknownPlugin,
    Unavailable,
    UnknownAction,
    Busy,
    InvalidRequest,
    DeadlineExceeded,
    Cancelled,
    WorkerError,
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

/// Constructed only by trusted native code. Never deserialize this from IPC.
#[derive(Clone)]
pub struct WorkerSpec {
    pub plugin_id: String,
    pub executable: PathBuf,
    pub args: Vec<OsString>,
    pub configuration: Option<WorkerConfiguration>,
    /// Granted only from the installed package's freshly verified manifest.
    pub desktop_notifications: bool,
    pub http_video: Option<HttpVideoGrant>,
    pub live_view: Option<super::protocol::LiveViewGrant>,
}

struct WorkerEntry {
    snapshot: Mutex<PluginSnapshot>,
    numeric: Arc<Mutex<HashMap<String, Arc<NumericLease>>>>,
    commands: mpsc::Sender<Control>,
    stop: watch::Sender<bool>,
    task: Mutex<Option<JoinHandle<()>>>,
    authority: Arc<Mutex<Authority>>,
    epoch: u64,
    done: watch::Receiver<bool>,
    reaped: AtomicBool,
    changed: ChangeCallback,
    notifications: Mutex<NotificationState>,
    notification_rate: Arc<Mutex<NotificationRate>>,
    generation_lease: Mutex<Option<GenerationLease>>,
    http_videos: Mutex<HttpVideoState>,
    settings_choices: Mutex<super::settings_choices::Catalog>,
}

impl WorkerEntry {
    fn revoke_generation(&self) {
        *self
            .settings_choices
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = super::settings_choices::Catalog::default();
        for (_, lease) in self
            .numeric
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
        {
            lease.revoked.send_replace(true);
        }
        if let Some(lease) = self
            .generation_lease
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            lease.revoke();
        }
        self.snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .instance_id = None;
        self.http_videos
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .clear();
    }

    /// Called under authority; this only validates and enqueues native data.
    fn queue_http_video(
        &self,
        grant: &HttpVideoGrant,
        id: String,
        url: String,
        title: String,
        admission: MediaAdmission,
    ) -> bool {
        let camera_id = if let Some(id) = &admission.cooldown_id {
            Some(MediaCameraId::Explicit(id.clone()))
        } else if admission.live_preview {
            let Ok(mut source) = grant.validate_preview_url(&url) else {
                return false;
            };
            // Frigate's validated endpoint has one stable path per camera.
            // Query credentials and fragments never define camera identity.
            source.set_query(None);
            source.set_fragment(None);
            Some(MediaCameraId::HttpPreview(source.into()))
        } else {
            None
        };
        let snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        if snapshot.state != WorkerState::Running
            || *self.stop.borrow()
            || self.reaped.load(Ordering::Acquire)
        {
            return false;
        }
        let lease = self
            .generation_lease
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(lease) = lease.filter(GenerationLease::is_active) else {
            return false;
        };
        let mut state = self.http_videos.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        state
            .pending
            .retain(|(_, created)| now.duration_since(*created) < NOTIFICATION_DELIVERY_TTL);
        state
            .seen
            .retain(|(_, created)| now.duration_since(*created) < NOTIFICATION_DEDUP_TTL);
        state
            .cooldowns
            .retain(|(_, created)| now.duration_since(*created) < grant.cooldown());
        let cooldown_id = camera_id
            .clone()
            .map(MediaCooldownId::Camera)
            .unwrap_or_else(|| MediaCooldownId::LegacyTitle(title.clone()));
        state.rate.refresh();
        if state.pending.len() >= HTTP_VIDEO_QUEUE_CAPACITY
            || state.seen.len() >= NOTIFICATION_SEEN_CAPACITY
            || state.seen.iter().any(|(seen, _)| seen == &id)
            || state.cooldowns.iter().any(|(seen, _)| seen == &cooldown_id)
            || camera_id.as_ref().is_some_and(|camera| {
                state
                    .pending
                    .iter()
                    .any(|(request, _)| request.camera_id.as_ref() == Some(camera))
            })
            || state.rate.count >= NOTIFICATIONS_PER_WORKER_PER_MINUTE
        {
            return false;
        }
        state.rate.count += 1;
        let request = QueuedHttpVideo {
            camera_id,
            live_preview: admission.live_preview,
            lease,
            grant: grant.clone(),
            id,
            url,
            title,
            media_kind: admission.kind,
        };
        state.seen.push_back((request.id.clone(), now));
        state.cooldowns.push_back((cooldown_id, now));
        state.pending.push_back((request, now));
        true
    }

    fn authorized(&self) -> bool {
        let authority = self.authority.lock().unwrap_or_else(|e| e.into_inner());
        authority.enabled && authority.epoch == self.epoch
    }

    fn snapshot(&self) -> PluginSnapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Called under authority, like snapshot replacement and action admission.
    fn replace_contributions(
        &self,
        items: Vec<DashboardContribution>,
        presentation: Vec<super::presentation::Presentation>,
    ) {
        let mut registry = self.numeric.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = HashMap::new();
        for grant in items
            .iter()
            .filter_map(DashboardContribution::number_input_grant)
        {
            let previous = registry.remove(&grant.action_id);
            let lease = match previous {
                Some(previous) if previous.grant == grant => previous,
                previous => {
                    if let Some(previous) = previous {
                        previous.revoked.send_replace(true);
                    }
                    Arc::new(NumericLease {
                        grant,
                        revoked: watch::channel(false).0,
                    })
                }
            };
            next.insert(lease.grant.action_id.clone(), lease);
        }
        for previous in registry.values() {
            previous.revoked.send_replace(true);
        }
        *registry = next;
        let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        snapshot.contributions = items;
        snapshot.presentation = presentation;
    }

    fn update(&self, update: impl FnOnce(&mut PluginSnapshot)) {
        {
            let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
            update(&mut snapshot);
            if snapshot.state != WorkerState::Running {
                self.notifications
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .pending
                    .clear();
            }
        }
        (self.changed)();
    }

    /// Called with the authority guard held. Bounds and deduplication are local
    /// to this worker registration; automatic restarts retain its recent IDs.
    fn queue_notification(&self, notification: DesktopNotification) -> bool {
        let snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        if snapshot.state != WorkerState::Running
            || *self.stop.borrow()
            || self.reaped.load(Ordering::Acquire)
        {
            return false;
        }
        let mut state = self.notifications.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        state
            .seen
            .retain(|(_, created)| now.duration_since(*created) < NOTIFICATION_DEDUP_TTL);
        state
            .pending
            .retain(|queued| now.duration_since(queued.created) < NOTIFICATION_DELIVERY_TTL);
        state.rate.refresh();
        if state.seen.iter().any(|(id, _)| id == &notification.id)
            || state.pending.len() >= NOTIFICATION_QUEUE_CAPACITY
            || state.rate.count >= NOTIFICATIONS_PER_WORKER_PER_MINUTE
        {
            return false;
        }
        let mut global = self
            .notification_rate
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        global.refresh();
        if global.count >= NOTIFICATIONS_GLOBAL_PER_MINUTE {
            return false;
        }
        global.count += 1;
        state.rate.count += 1;
        if state.seen.len() >= NOTIFICATION_SEEN_CAPACITY {
            state.seen.pop_front();
        }
        state.seen.push_back((notification.id.clone(), now));
        state.pending.push_back(QueuedNotification {
            notification,
            generation: snapshot.generation,
            created: now,
        });
        true
    }
}

struct Authority {
    enabled: bool,
    epoch: u64,
}

struct HostInner {
    authority: Arc<Mutex<Authority>>,
    entries: Mutex<HashMap<String, Arc<WorkerEntry>>>,
    stopped: AtomicBool,
    next_request: AtomicU64,
    changed: ChangeCallback,
    notification_rate: Arc<Mutex<NotificationRate>>,
}

impl Drop for HostInner {
    fn drop(&mut self) {
        // A separate watch channel makes stop independent of a saturated action queue.
        for entry in self
            .entries
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            entry.revoke_generation();
            let _ = entry.stop.send(true);
        }
    }
}

/// One instance is managed by Tauri and shared by every window. Clones share the registry.
#[derive(Clone)]
pub struct PluginHost(Arc<HostInner>);

impl Default for PluginHost {
    fn default() -> Self {
        Self::new(Arc::new(|| {}))
    }
}

impl PluginHost {
    pub fn new(changed: ChangeCallback) -> Self {
        Self(Arc::new(HostInner {
            authority: Arc::new(Mutex::new(Authority {
                enabled: true,
                epoch: 0,
            })),
            entries: Mutex::new(HashMap::new()),
            stopped: AtomicBool::new(false),
            next_request: AtomicU64::new(1),
            changed,
            notification_rate: Arc::new(Mutex::new(NotificationRate::default())),
        }))
    }

    pub fn authority_epoch(&self) -> u64 {
        self.0
            .authority
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .epoch
    }

    /// Check the authority captured before an asynchronous package operation.
    pub fn is_authorized_epoch(&self, epoch: u64) -> bool {
        let authority = self.0.authority.lock().unwrap_or_else(|e| e.into_inner());
        !self.0.stopped.load(Ordering::Acquire) && authority.enabled && authority.epoch == epoch
    }

    /// Linearize a small synchronous package-state commit with session revocation.
    /// The callback must not call the host or wait for asynchronous work.
    pub(crate) fn commit_in_epoch<T>(
        &self,
        epoch: u64,
        commit: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let authority = self.0.authority.lock().unwrap_or_else(|e| e.into_inner());
        if self.0.stopped.load(Ordering::Acquire) || !authority.enabled || authority.epoch != epoch
        {
            return Err("Plugin operation authorization changed".into());
        }
        commit()
    }

    pub fn snapshots(&self) -> Vec<PluginSnapshot> {
        let mut snapshots: Vec<_> = self
            .0
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|entry| entry.snapshot())
            .collect();
        snapshots.sort_by(|a, b| a.plugin_id.cmp(&b.plugin_id));
        snapshots
    }

    /// A cheap scheduling hint only; dispatch still checks authority and state.
    pub(crate) fn has_pending_notifications(&self) -> bool {
        self.0
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|entry| {
                !entry
                    .notifications
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .pending
                    .is_empty()
            })
    }

    pub(crate) fn settings_choices(
        &self,
        plugin_id: &str,
    ) -> Result<super::settings_choices::Snapshot, PluginError> {
        let authority = self.0.authority.lock().unwrap_or_else(|e| e.into_inner());
        if !authority.enabled || self.0.stopped.load(Ordering::Acquire) {
            return Err(PluginError::HostStopped);
        }
        let entries = self.0.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entries.get(plugin_id).ok_or(PluginError::UnknownPlugin)?;
        let snapshot = entry.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        if entry.epoch != authority.epoch
            || snapshot.state != WorkerState::Running
            || *entry.stop.borrow()
        {
            return Err(PluginError::Unavailable);
        }
        let snapshot = entry
            .settings_choices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot();
        Ok(snapshot)
    }

    /// Deliver at most sixteen queued notifications per registered worker (128
    /// globally), yielding to pending media before every submission. The callback
    /// runs synchronously under authority and worker state guards: it must not
    /// reenter this host/auth or defer delivery. An OS submission already entered
    /// keeps its existing authorization and cannot be preempted here.
    pub(crate) fn dispatch_notifications(
        &self,
        mut deliver: impl FnMut(&DesktopNotification),
    ) -> usize {
        let entries: Vec<_> = self
            .0
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        let mut delivered = 0;
        for entry in entries {
            for _ in 0..NOTIFICATION_QUEUE_CAPACITY {
                // Release authority between submissions so a slow native
                // notification service does not turn a batch into one lock hold.
                let authority = self.0.authority.lock().unwrap_or_else(|e| e.into_inner());
                // Check before taking an entry snapshot: media inspection takes
                // the entries lock, which precedes snapshots in host lock order.
                if self.has_pending_http_videos() {
                    return delivered;
                }
                let snapshot = entry.snapshot.lock().unwrap_or_else(|e| e.into_inner());
                let Some(queued) = entry
                    .notifications
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .pending
                    .pop_front()
                else {
                    break;
                };
                if self.0.stopped.load(Ordering::Acquire)
                    || !authority.enabled
                    || authority.epoch != entry.epoch
                    || *entry.stop.borrow()
                    || entry.reaped.load(Ordering::Acquire)
                    || snapshot.state != WorkerState::Running
                    || snapshot.generation != queued.generation
                    || queued.created.elapsed() >= NOTIFICATION_DELIVERY_TTL
                {
                    continue;
                }
                deliver(&queued.notification);
                delivered += 1;
            }
        }
        delivered
    }

    /// Cheap scheduling hint, without configuration/auth disk reads.
    pub(crate) fn has_pending_http_videos(&self) -> bool {
        self.0
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|entry| {
                !entry
                    .http_videos
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .pending
                    .is_empty()
            })
    }

    /// Drain at most four items per registered worker. Callers perform live app
    /// authentication first and retain/recheck the returned lease throughout I/O.
    pub(crate) fn take_http_video_requests(&self) -> Vec<QueuedHttpVideo> {
        let authority = self.0.authority.lock().unwrap_or_else(|e| e.into_inner());
        let mut requests = Vec::new();
        let entries = self.0.entries.lock().unwrap_or_else(|e| e.into_inner());
        for entry in entries.values() {
            let snapshot = entry.snapshot();
            let instance = entry
                .generation_lease
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(GenerationLease::instance_id);
            let mut state = entry.http_videos.lock().unwrap_or_else(|e| e.into_inner());
            while let Some((request, created)) = state.pending.pop_front() {
                if !self.0.stopped.load(Ordering::Acquire)
                    && authority.enabled
                    && authority.epoch == entry.epoch
                    && !*entry.stop.borrow()
                    && !entry.reaped.load(Ordering::Acquire)
                    && snapshot.state == WorkerState::Running
                    && request.lease.is_active()
                    && request.lease.generation() == snapshot.generation
                    && request.lease.epoch() == authority.epoch
                    && request.lease.plugin_id() == snapshot.plugin_id
                    && Some(request.lease.instance_id()) == instance
                    && created.elapsed() < NOTIFICATION_DELIVERY_TTL
                {
                    requests.push(request);
                }
            }
        }
        requests
    }

    /// Register a worker exactly once and begin its bounded startup/restart lifecycle.
    /// The returned snapshot is `starting`; readiness is signalled through snapshots.
    pub async fn start(&self, spec: WorkerSpec) -> Result<PluginSnapshot, PluginError> {
        self.register(spec, None)
    }

    /// Register only in the session that authorized a package operation.
    /// The epoch check and registration share the same authority lock.
    pub async fn start_in_epoch(
        &self,
        spec: WorkerSpec,
        epoch: u64,
    ) -> Result<PluginSnapshot, PluginError> {
        self.register(spec, Some(epoch))
    }

    fn register(
        &self,
        spec: WorkerSpec,
        expected_epoch: Option<u64>,
    ) -> Result<PluginSnapshot, PluginError> {
        if self.0.stopped.load(Ordering::Acquire) {
            return Err(PluginError::HostStopped);
        }
        if !spec.executable.is_absolute() || validate_plugin_id(&spec.plugin_id).is_err() {
            return Err(PluginError::InvalidWorker);
        }
        if spec.http_video.is_some() && spec.configuration.is_none() {
            return Err(PluginError::InvalidWorker);
        }
        if spec
            .configuration
            .as_ref()
            .is_some_and(|configuration| configuration.validate().is_err())
        {
            return Err(PluginError::InvalidWorker);
        }
        let authority = self.0.authority.lock().unwrap_or_else(|e| e.into_inner());
        if !authority.enabled || expected_epoch.is_some_and(|epoch| epoch != authority.epoch) {
            return Err(PluginError::Unavailable);
        }
        let (commands, receiver) = mpsc::channel(CONTROL_QUEUE_CAPACITY);
        let (stop, stop_receiver) = watch::channel(false);
        let (finished, done) = watch::channel(false);
        let entry = Arc::new(WorkerEntry {
            numeric: Arc::new(Mutex::new(HashMap::new())),
            snapshot: Mutex::new(PluginSnapshot {
                plugin_id: spec.plugin_id.clone(),
                state: WorkerState::Starting,
                generation: 0,
                instance_id: None,
                restart_count: 0,
                contributions: Vec::new(),
                presentation: Vec::new(),
                last_error: None,
            }),
            commands,
            stop,
            task: Mutex::new(None),
            changed: self.0.changed.clone(),
            authority: self.0.authority.clone(),
            epoch: authority.epoch,
            done,
            reaped: AtomicBool::new(true),
            notifications: Mutex::new(NotificationState::default()),
            generation_lease: Mutex::new(None),
            http_videos: Mutex::new(HttpVideoState::default()),
            settings_choices: Mutex::new(super::settings_choices::Catalog::default()),
            notification_rate: self.0.notification_rate.clone(),
        });
        {
            let mut entries = self.0.entries.lock().unwrap_or_else(|e| e.into_inner());
            if self.0.stopped.load(Ordering::Acquire) {
                return Err(PluginError::HostStopped);
            }
            if let Some(previous) = entries.get(&spec.plugin_id) {
                if !matches!(
                    previous.snapshot().state,
                    WorkerState::Stopped | WorkerState::Failed
                ) || !*previous.done.borrow()
                    || !previous.reaped.load(Ordering::Acquire)
                {
                    return Err(PluginError::AlreadyRegistered);
                }
                entry
                    .snapshot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .generation = previous.snapshot().generation;
            }
            if !entries.contains_key(&spec.plugin_id) && entries.len() >= MAX_WORKERS {
                return Err(PluginError::Capacity);
            }
            entries.insert(spec.plugin_id.clone(), entry.clone());
            let task_entry = entry.clone();
            let task = tokio::spawn(async move {
                supervise(spec, task_entry, receiver, stop_receiver).await;
                let _ = finished.send(true);
            });
            *entry.task.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
        }
        drop(authority);
        (self.0.changed)();
        Ok(entry.snapshot())
    }

    pub async fn action(
        &self,
        plugin_id: &str,
        action_id: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, PluginError> {
        let epoch = self.authority_epoch();
        let instance = self
            .entry(plugin_id)?
            .snapshot()
            .instance_id
            .ok_or(PluginError::Unavailable)?;
        self.action_in_epoch(plugin_id, &instance, action_id, params, timeout, epoch)
            .await
    }

    /// Enqueue only for the actual worker instance and session shown to the
    /// caller. Reinstall can reuse generation counters but never this identity.
    pub async fn action_in_epoch(
        &self,
        plugin_id: &str,
        instance_id: &str,
        action_id: &str,
        params: Value,
        timeout: Duration,
        epoch: u64,
    ) -> Result<Value, PluginError> {
        if self.0.stopped.load(Ordering::Acquire) {
            return Err(PluginError::HostStopped);
        }
        if timeout.is_zero() || timeout > Duration::from_millis(MAX_ACTION_DEADLINE_MS) {
            return Err(PluginError::InvalidRequest);
        }
        let entry = self.entry(plugin_id)?;
        let snapshot = entry.snapshot();
        if snapshot.state != WorkerState::Running
            || snapshot.instance_id.as_deref() != Some(instance_id)
        {
            return Err(PluginError::Unavailable);
        }
        if !advertises(&snapshot.contributions, action_id, &params) {
            return Err(PluginError::UnknownAction);
        }
        let request_id = self
            .0
            .next_request
            .fetch_add(1, Ordering::Relaxed)
            .to_string();
        let message = HostMessage::Action {
            request_id: request_id.clone(),
            action_id: action_id.to_owned(),
            params: params.clone(),
            deadline_ms: timeout.as_millis().max(1) as u64,
        };
        validate_host_message(&message).map_err(|_| PluginError::InvalidRequest)?;
        let (reply, response) = oneshot::channel();
        let (cancel, cancellation) = watch::channel(false);
        let deadline = Instant::now() + timeout;
        {
            let authority = entry.authority.lock().unwrap_or_else(|e| e.into_inner());
            if self.0.stopped.load(Ordering::Acquire) {
                return Err(PluginError::HostStopped);
            }
            if !authority.enabled || authority.epoch != epoch || entry.epoch != epoch {
                return Err(PluginError::Unavailable);
            }
            let current = entry.snapshot();
            if current.state != WorkerState::Running
                || current.instance_id.as_deref() != Some(instance_id)
            {
                return Err(PluginError::Unavailable);
            }
            if !advertises(&current.contributions, action_id, &params) {
                return Err(PluginError::UnknownAction);
            }
            let numeric = entry
                .numeric
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(action_id)
                .cloned();
            entry
                .commands
                .try_send(Control::Action {
                    request_id: request_id.clone(),
                    generation: current.generation,
                    action_id: action_id.to_owned(),
                    params,
                    numeric,
                    cancellation,
                    deadline,
                    reply,
                })
                .map_err(|error| match error {
                    mpsc::error::TrySendError::Full(_) => PluginError::Busy,
                    mpsc::error::TrySendError::Closed(_) => PluginError::Unavailable,
                })?;
        }
        let _cancel_on_drop = CancelOnDrop {
            commands: entry.commands.clone(),
            request_id,
            cancel,
        };
        let result = time::timeout_at(deadline, response).await;
        if !entry.authorized() {
            return Err(PluginError::Unavailable);
        }
        match result {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(PluginError::Unavailable),
            Err(_) => Err(PluginError::DeadlineExceeded),
        }
    }

    pub async fn stop(&self, plugin_id: &str) -> Result<(), PluginError> {
        let entry = self.entry(plugin_id)?;
        {
            let _authority = self.0.authority.lock().unwrap_or_else(|e| e.into_inner());
            entry.revoke_generation();
            let _ = entry.stop.send(true);
        }
        wait_stopped(&entry).await;
        if !entry.reaped.load(Ordering::Acquire) {
            return Err(PluginError::WorkerError);
        }
        Ok(())
    }

    /// Stop and reap before releasing a plugin's registry slot and contributions.
    /// A concurrent replacement must never be removed on behalf of an old entry.
    pub async fn remove(&self, plugin_id: &str) -> Result<(), PluginError> {
        let entry = self.entry(plugin_id)?;
        {
            let _authority = self.0.authority.lock().unwrap_or_else(|e| e.into_inner());
            entry.revoke_generation();
            let _ = entry.stop.send(true);
        }
        wait_stopped(&entry).await;
        if !entry.reaped.load(Ordering::Acquire) {
            return Err(PluginError::WorkerError);
        }
        {
            let mut entries = self.0.entries.lock().unwrap_or_else(|e| e.into_inner());
            match entries.get(plugin_id) {
                Some(current) if Arc::ptr_eq(current, &entry) => {
                    entries.remove(plugin_id);
                }
                Some(_) => return Err(PluginError::AlreadyRegistered),
                None => return Ok(()),
            }
        }
        (self.0.changed)();
        Ok(())
    }

    /// Revoke synchronously before publishing an application logout or policy change.
    /// Old generations stay unauthorized even if a later login resumes the host.
    pub fn revoke(&self) {
        self.revoke_epoch();
    }

    /// Return this revocation's exact epoch, even if another session changes
    /// authority before this caller can perform its follow-up work.
    pub(crate) fn revoke_epoch(&self) -> u64 {
        let mut authority = self.0.authority.lock().unwrap_or_else(|e| e.into_inner());
        authority.enabled = false;
        authority.epoch = authority.epoch.wrapping_add(1);
        let epoch = authority.epoch;
        let entries: Vec<_> = self
            .0
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for entry in entries {
            entry.revoke_generation();
            let _ = entry.stop.send(true);
            let mut snapshot = entry.snapshot.lock().unwrap_or_else(|e| e.into_inner());
            snapshot.state = WorkerState::Stopped;
            snapshot.contributions.clear();
            snapshot.presentation.clear();
            entry
                .notifications
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pending
                .clear();
        }
        drop(authority);
        (self.0.changed)();
        epoch
    }

    /// Resume native registration after a newly authorized session, without restarting workers.
    pub fn resume(&self) {
        let mut authority = self.0.authority.lock().unwrap_or_else(|e| e.into_inner());
        if !self.0.stopped.load(Ordering::Acquire) {
            authority.enabled = true;
        }
    }

    /// Resume only the session transition that captured `epoch`; an older
    /// unlock continuation must never undo a newer logout or terminal shutdown.
    pub(crate) fn resume_in_epoch(&self, epoch: u64) -> bool {
        let mut authority = self.0.authority.lock().unwrap_or_else(|e| e.into_inner());
        if self.0.stopped.load(Ordering::Acquire) || authority.epoch != epoch {
            return false;
        }
        authority.enabled = true;
        true
    }

    /// Stop and reap registered children; later explicit starts remain possible after resume.
    pub async fn stop_all(&self) {
        self.revoke();
        let entries: Vec<_> = self
            .0
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for entry in entries {
            wait_stopped(&entry).await;
        }
    }

    /// Wait only for revoked generations, without changing a newer authenticated session.
    pub async fn wait_revoked(&self) {
        let entries: Vec<_> = {
            let authority = self.0.authority.lock().unwrap_or_else(|e| e.into_inner());
            self.0
                .entries
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .filter(|entry| !authority.enabled || entry.epoch != authority.epoch)
                .cloned()
                .collect()
        };
        for entry in entries {
            wait_stopped(&entry).await;
        }
    }

    /// Terminal shutdown. New registrations and actions are rejected thereafter.
    pub async fn shutdown(&self) {
        self.0.stopped.store(true, Ordering::Release);
        self.stop_all().await;
    }

    fn entry(&self, id: &str) -> Result<Arc<WorkerEntry>, PluginError> {
        self.0
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
            .ok_or(PluginError::UnknownPlugin)
    }
}

async fn wait_stopped(entry: &WorkerEntry) {
    let mut done = entry.done.clone();
    let _ = done.wait_for(|finished| *finished).await;
    let task = entry.task.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(task) = task {
        let _ = task.await;
    }
}

fn advertises(items: &[DashboardContribution], action_id: &str, params: &Value) -> bool {
    items.iter().any(|item| match item {
        DashboardContribution::Action {
            action_id: id,
            params: advertised,
            ..
        } => id == action_id && advertised == params,
        _ => item
            .number_input_grant()
            .is_some_and(|grant| grant.action_id == action_id && grant.accepts(params)),
    })
}

struct NumericLease {
    grant: NumberInputGrant,
    revoked: watch::Sender<bool>,
}

struct NumericWriteGuard {
    lease: Arc<NumericLease>,
    registry: Arc<Mutex<HashMap<String, Arc<NumericLease>>>>,
    rejected: mpsc::Sender<String>,
}

fn current_numeric(
    registry: &HashMap<String, Arc<NumericLease>>,
    lease: &Arc<NumericLease>,
    params: &Value,
) -> bool {
    registry
        .get(&lease.grant.action_id)
        .is_some_and(|current| Arc::ptr_eq(current, lease))
        && lease.grant.accepts(params)
}

struct CancelOnDrop {
    commands: mpsc::Sender<Control>,
    request_id: String,
    cancel: watch::Sender<bool>,
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
        // Expiration remains enforced by the supervisor even when cancellation meets a full queue.
        let _ = self
            .commands
            .try_send(Control::Cancel(self.request_id.clone()));
    }
}

enum Control {
    Action {
        request_id: String,
        generation: u64,
        action_id: String,
        params: Value,
        numeric: Option<Arc<NumericLease>>,
        cancellation: watch::Receiver<bool>,
        deadline: Instant,
        reply: ActionReply,
    },
    Cancel(String),
}

struct Pending {
    deadline: Instant,
    reply: ActionReply,
}

enum Outgoing {
    Control(Vec<u8>),
    Configuration {
        encoded: Vec<u8>,
        deadline: Instant,
    },
    Action {
        request_id: String,
        action_id: String,
        params: Value,
        numeric: Option<NumericWriteGuard>,
        deadline: Instant,
        cancellation: watch::Receiver<bool>,
    },
}

async fn write_frames<W: AsyncWrite + Unpin>(
    mut stdin: W,
    mut outgoing: mpsc::Receiver<Outgoing>,
    mut stop: watch::Receiver<bool>,
    authority: Arc<Mutex<Authority>>,
    epoch: u64,
) -> Result<(), &'static str> {
    loop {
        let message = tokio::select! {
            biased;
            _ = stop.changed() => {
                // No partially written frame here. Bypass queued actions for cooperative shutdown.
                let shutdown = encode_host_frame(&HostMessage::Shutdown).map_err(|_| "host_frame_invalid")?;
                let _ = time::timeout(Duration::from_millis(20), stdin.write_all(&shutdown)).await;
                return Ok(());
            }
            message = outgoing.recv() => match message { Some(message) => message, None => return Ok(()) },
        };
        match message {
            Outgoing::Configuration { encoded, deadline } => {
                let authorized = {
                    let guard = authority.lock().unwrap_or_else(|e| e.into_inner());
                    guard.enabled && guard.epoch == epoch
                };
                if !authorized || *stop.borrow() {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err("worker_configuration_timeout");
                }
                tokio::select! {
                    biased;
                    _ = stop.changed() => return Ok(()),
                    _ = time::sleep_until(deadline) => return Err("worker_configuration_timeout"),
                    result = stdin.write_all(&encoded) => result.map_err(|_| "worker_write_failed")?,
                }
            }
            Outgoing::Control(bytes) => {
                tokio::select! {
                    biased;
                    _ = stop.changed() => return Ok(()),
                    result = time::timeout(Duration::from_secs(1), stdin.write_all(&bytes)) => {
                        result.map_err(|_| "worker_write_timeout")?.map_err(|_| "worker_write_failed")?;
                    }
                }
            }
            Outgoing::Action {
                request_id,
                action_id,
                params,
                numeric,
                deadline,
                mut cancellation,
            } => {
                let authorized = {
                    let guard = authority.lock().unwrap_or_else(|e| e.into_inner());
                    guard.enabled && guard.epoch == epoch
                };
                if !authorized
                    || *stop.borrow()
                    || *cancellation.borrow()
                    || Instant::now() >= deadline
                {
                    continue;
                }
                if let Some(numeric) = numeric {
                    enum FirstWrite {
                        Started(Vec<u8>, usize),
                        Rejected,
                        Expired,
                    }
                    let mut revoked = numeric.lease.revoked.subscribe();
                    let stop_check = stop.clone();
                    let cancel_check = cancellation.clone();
                    let first = tokio::select! {
                        biased;
                        _ = stop.changed() => return Ok(()),
                        _ = cancellation.changed() => return Err("worker_write_cancelled"),
                        _ = time::sleep_until(deadline) => return Err("worker_write_timeout"),
                        _ = revoked.changed() => FirstWrite::Rejected,
                        result = poll_fn(|context| {
                            // A Pending write has emitted no bytes. Recheck and
                            // re-encode on every poll, under the same locks used
                            // for revocation, until the first byte is accepted.
                            let authority = authority.lock().unwrap_or_else(|e| e.into_inner());
                            if !authority.enabled || authority.epoch != epoch
                                || *stop_check.borrow() || *cancel_check.borrow()
                            {
                                return Poll::Ready(Err("worker_write_cancelled"));
                            }
                            let registry = numeric.registry.lock().unwrap_or_else(|e| e.into_inner());
                            if !current_numeric(&registry, &numeric.lease, &params) {
                                return Poll::Ready(Ok(FirstWrite::Rejected));
                            }
                            let remaining = deadline.saturating_duration_since(Instant::now()).as_millis();
                            if remaining == 0 { return Poll::Ready(Ok(FirstWrite::Expired)); }
                            let encoded = match encode_host_frame(&HostMessage::Action {
                                request_id: request_id.clone(), action_id: action_id.clone(),
                                params: params.clone(), deadline_ms: remaining as u64,
                            }) {
                                Ok(encoded) => encoded,
                                Err(_) => return Poll::Ready(Err("host_frame_invalid")),
                            };
                            match std::pin::Pin::new(&mut stdin).poll_write(context, &encoded) {
                                Poll::Ready(Ok(0) | Err(_)) => Poll::Ready(Err("worker_write_failed")),
                                Poll::Ready(Ok(count)) => Poll::Ready(Ok(FirstWrite::Started(encoded, count))),
                                Poll::Pending => Poll::Pending,
                            }
                        }) => result?,
                    };
                    match first {
                        FirstWrite::Rejected => {
                            numeric
                                .rejected
                                .try_send(request_id)
                                .map_err(|_| "worker_write_queue_full")?;
                        }
                        FirstWrite::Expired => {}
                        FirstWrite::Started(encoded, count) => {
                            // A capability change cannot splice a new frame into
                            // partial JSON. The worker rechecks after receiving it.
                            tokio::select! {
                                biased;
                                _ = stop.changed() => return Ok(()),
                                _ = cancellation.changed() => return Err("worker_write_cancelled"),
                                _ = time::sleep_until(deadline) => return Err("worker_write_timeout"),
                                result = stdin.write_all(&encoded[count..]) => result.map_err(|_| "worker_write_failed")?,
                            }
                        }
                    }
                    continue;
                }
                // Forward only the original request's remaining budget. Queue
                // pressure must not give the worker a fresh full timeout.
                let deadline_ms = deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .try_into()
                    .map_err(|_| "host_frame_invalid")?;
                if deadline_ms == 0 {
                    continue;
                }
                let encoded = encode_host_frame(&HostMessage::Action {
                    request_id,
                    action_id,
                    params,
                    deadline_ms,
                })
                .map_err(|_| "host_frame_invalid")?;
                tokio::select! {
                    biased;
                    _ = stop.changed() => return Ok(()),
                    _ = cancellation.changed() => return Err("worker_write_cancelled"),
                    _ = time::sleep_until(deadline) => return Err("worker_write_timeout"),
                    result = stdin.write_all(&encoded) => result.map_err(|_| "worker_write_failed")?,
                }
                // A cancelled partial frame ends this writer/generation; queued frames never follow it.
            }
        }
    }
}

struct WorkerPipes {
    writer: mpsc::Sender<Outgoing>,
    rejected: mpsc::Receiver<String>,
    rejection_sender: mpsc::Sender<String>,
    frames: mpsc::Receiver<Result<WorkerMessage, &'static str>>,
    tasks: Vec<JoinHandle<()>>,
}

impl WorkerPipes {
    fn new(child: &mut Child, entry: &WorkerEntry, stop: watch::Receiver<bool>) -> Option<Self> {
        let stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        let mut stderr = child.stderr.take()?;
        let (writer, outgoing) = mpsc::channel(PIPE_QUEUE_CAPACITY);
        let (rejection_sender, rejected) = mpsc::channel(MAX_IN_FLIGHT);
        let (incoming, frames) = mpsc::channel(PIPE_QUEUE_CAPACITY);
        let writer_errors = incoming.clone();
        let authority = entry.authority.clone();
        let epoch = entry.epoch;
        let write_task = tokio::spawn(async move {
            if let Err(code) = write_frames(stdin, outgoing, stop, authority, epoch).await {
                let _ = writer_errors.send(Err(code)).await;
            }
        });
        let read_task = tokio::spawn(async move {
            let mut reader = BufReader::with_capacity(4096, stdout);
            loop {
                let result = read_frame(&mut reader).await;
                let failed = result.is_err();
                if incoming.send(result).await.is_err() || failed {
                    return;
                }
            }
        });
        let stderr_task = tokio::spawn(async move {
            // Drain with constant memory. Never log plugin-controlled text or inherited secrets.
            let mut buffer = [0_u8; 4096];
            while let Ok(count) = stderr.read(&mut buffer).await {
                if count == 0 {
                    break;
                }
            }
        });
        Some(Self {
            writer,
            rejected,
            rejection_sender,
            frames,
            tasks: vec![write_task, read_task, stderr_task],
        })
    }

    fn send(&self, message: &HostMessage) -> Result<(), &'static str> {
        if matches!(message, HostMessage::Configuration { .. }) {
            return Err("configuration_requires_authorized_writer");
        }
        let bytes = encode_host_frame(message).map_err(|_| "host_frame_invalid")?;
        self.writer
            .try_send(Outgoing::Control(bytes))
            .map_err(|_| "worker_write_queue_full")
    }

    fn configure(
        &self,
        configuration: &WorkerConfiguration,
        deadline: Instant,
    ) -> Result<(), &'static str> {
        let encoded = encode_host_frame(&HostMessage::Configuration {
            configuration: configuration.clone(),
        })
        .map_err(|_| "host_configuration_invalid")?;
        self.writer
            .try_send(Outgoing::Configuration { encoded, deadline })
            .map_err(|_| "worker_write_queue_full")
    }
}

impl Drop for WorkerPipes {
    fn drop(&mut self) {
        // Cancel blocked async pipe I/O, including inherited descriptors in a worker descendant.
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<WorkerMessage, &'static str> {
    let mut frame = Vec::new();
    loop {
        let buffer = reader.fill_buf().await.map_err(|_| "worker_read_failed")?;
        if buffer.is_empty() {
            return Err("worker_eof");
        }
        let newline = buffer.iter().position(|&byte| byte == b'\n');
        let count = newline.map_or(buffer.len(), |index| index + 1);
        if frame.len() + count > MAX_FRAME_BYTES {
            return Err("worker_frame_too_large");
        }
        frame.extend_from_slice(&buffer[..count]);
        reader.consume(count);
        if newline.is_some() {
            return parse_worker_frame(&frame).map_err(|_| "worker_frame_invalid");
        }
    }
}

fn spawn_generation(
    spec: &WorkerSpec,
    entry: &WorkerEntry,
    restart_count: u32,
) -> Result<Option<(Child, RevokeOnDrop)>, &'static str> {
    let authority = entry.authority.lock().unwrap_or_else(|e| e.into_inner());
    if !authority.enabled || authority.epoch != entry.epoch || *entry.stop.borrow() {
        return Ok(None);
    }
    {
        let mut snapshot = entry.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        snapshot.state = if restart_count == 0 {
            WorkerState::Starting
        } else {
            WorkerState::Restarting
        };
        snapshot.generation += 1;
        snapshot.instance_id = None;
        snapshot.restart_count = restart_count;
        snapshot.contributions.clear();
        snapshot.presentation.clear();
    }
    let child = Command::new(&spec.executable)
        .args(&spec.args)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "worker_spawn_failed")?;
    let lease = GenerationLease::new(
        spec.plugin_id.clone(),
        entry.epoch,
        entry.snapshot().generation,
    );
    let generation_guard = RevokeOnDrop(lease.clone());
    let instance_id = lease.instance_id().to_string();
    *entry
        .generation_lease
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(lease);
    entry
        .snapshot
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .instance_id = Some(instance_id);
    entry.reaped.store(false, Ordering::Release);
    drop(authority);
    (entry.changed)();
    Ok(Some((child, generation_guard)))
}

async fn supervise(
    spec: WorkerSpec,
    entry: Arc<WorkerEntry>,
    mut commands: mpsc::Receiver<Control>,
    mut stop: watch::Receiver<bool>,
) {
    let mut restart_count = 0;
    loop {
        if *stop.borrow() || !entry.authorized() {
            break;
        }
        let (mut child, _generation_guard) = match spawn_generation(&spec, &entry, restart_count) {
            Ok(Some(generation)) => generation,
            Ok(None) => break,
            Err(code) => {
                fail_entry(&entry, code);
                return;
            }
        };
        let Some(mut pipes) = WorkerPipes::new(&mut child, &entry, stop.clone()) else {
            entry.revoke_generation();
            entry
                .reaped
                .store(terminate(&mut child, None).await, Ordering::Release);
            fail_entry(&entry, "worker_pipe_failed");
            return;
        };
        let outcome = run_generation(
            &spec,
            &entry,
            &mut child,
            &mut pipes,
            &mut commands,
            &mut stop,
        )
        .await;
        entry.revoke_generation();
        // Remove stale contributions before awaiting grace, cancellation, or restart backoff.
        entry.update(|snapshot| {
            snapshot.contributions.clear();
            snapshot.presentation.clear();
            snapshot.state = if matches!(outcome, Outcome::Stopped) {
                WorkerState::Stopped
            } else {
                WorkerState::Restarting
            };
            snapshot.last_error = outcome.error().map(str::to_owned);
        });
        let reaped = terminate(&mut child, Some(&pipes)).await;
        entry.reaped.store(reaped, Ordering::Release);
        drop(pipes);
        if !reaped {
            fail_entry(&entry, "worker_reap_failed");
            return;
        }
        match outcome {
            Outcome::Stopped => break,
            Outcome::Failed(code) => {
                fail_entry(&entry, code);
                return;
            }
            Outcome::Exited(code) if restart_count >= MAX_RESTARTS => {
                fail_entry(&entry, code);
                return;
            }
            Outcome::Exited(_) => {}
        }
        restart_count += 1;
        let backoff = time::sleep(RESTART_BACKOFF * restart_count);
        tokio::pin!(backoff);
        loop {
            tokio::select! {
                biased;
                _ = stop.changed() => break,
                _ = &mut backoff => break,
                control = commands.recv() => reject_control(control),
            }
        }
    }
    entry.update(|snapshot| {
        snapshot.state = WorkerState::Stopped;
        snapshot.contributions.clear();
        snapshot.presentation.clear();
        snapshot.last_error = None;
    });
    while let Ok(control) = commands.try_recv() {
        reject_control(Some(control));
    }
}

enum Outcome {
    Stopped,
    Exited(&'static str),
    Failed(&'static str),
}
impl Outcome {
    fn error(&self) -> Option<&'static str> {
        match self {
            Self::Stopped => None,
            Self::Exited(code) | Self::Failed(code) => Some(code),
        }
    }
}

fn fail_entry(entry: &WorkerEntry, code: &str) {
    entry.revoke_generation();
    entry.update(|snapshot| {
        snapshot.state = WorkerState::Failed;
        snapshot.last_error = Some(code.to_owned());
        snapshot.contributions.clear();
        snapshot.presentation.clear();
    });
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StartupPhase {
    Identity,
    Configuration,
    Running,
}

async fn run_generation(
    spec: &WorkerSpec,
    entry: &WorkerEntry,
    child: &mut Child,
    pipes: &mut WorkerPipes,
    commands: &mut mpsc::Receiver<Control>,
    stop: &mut watch::Receiver<bool>,
) -> Outcome {
    let hello = HostMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        host_api_version: HOST_API_VERSION.to_owned(),
        plugin_id: spec.plugin_id.clone(),
    };
    if pipes.send(&hello).is_err() {
        return Outcome::Failed("host_hello_failed");
    }
    let generation = entry.snapshot().generation;
    let startup_deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut startup = StartupPhase::Identity;
    let mut rate_window = Instant::now();
    let mut frame_count = 0;
    let mut contribution_count = 0;
    let mut pending = HashMap::new();
    let mut ticks = time::interval(DEADLINE_TICK);
    ticks.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let outcome = loop {
        tokio::select! {
            biased;
            _ = stop.changed() => break Outcome::Stopped,
            exit = child.wait() => {
                break Outcome::Exited(if exit.is_ok() { "worker_exited" } else { "worker_wait_failed" });
            }
            _ = ticks.tick() => {
                if startup != StartupPhase::Running && Instant::now() >= startup_deadline { break Outcome::Failed("worker_startup_timeout"); }
                expire_pending(&mut pending, pipes);
            }
            Some(id) = pipes.rejected.recv() => {
                if let Some(request) = pending.remove(&id) {
                    let _ = request.reply.send(Err(PluginError::UnknownAction));
                }
            }
            frame = pipes.frames.recv() => {
                if !entry.authorized() { break Outcome::Stopped; }
                if rate_window.elapsed() >= Duration::from_secs(1) {
                    rate_window = Instant::now(); frame_count = 0; contribution_count = 0;
                }
                frame_count += 1;
                if matches!(&frame, Some(Ok(WorkerMessage::Contributions { .. }))) { contribution_count += 1; }
                if frame_count > MAX_FRAMES_PER_SECOND || contribution_count > MAX_CONTRIBUTIONS_PER_SECOND {
                    break Outcome::Failed("worker_rate_limit");
                }
                match handle_frame(frame, spec, entry, &mut startup, &mut pending, pipes, startup_deadline) {
                    Ok(()) => {},
                    Err(outcome) => break outcome,
                }
            }
            control = commands.recv() => {
                match control {
                    Some(control) => handle_control(control, entry, generation, startup == StartupPhase::Running, pipes, &mut pending),
                    None => break Outcome::Stopped,
                }
            }
        }
    };
    for (_, request) in pending {
        let _ = request.reply.send(Err(PluginError::Unavailable));
    }
    outcome
}

fn handle_frame(
    frame: Option<Result<WorkerMessage, &'static str>>,
    spec: &WorkerSpec,
    entry: &WorkerEntry,
    startup: &mut StartupPhase,
    pending: &mut HashMap<String, Pending>,
    pipes: &WorkerPipes,
    startup_deadline: Instant,
) -> Result<(), Outcome> {
    let message = match frame {
        Some(Ok(message)) => message,
        Some(Err("worker_eof" | "worker_write_failed")) | None => {
            return Err(Outcome::Exited("worker_pipe_closed"))
        }
        Some(Err(code)) => return Err(Outcome::Failed(code)),
    };
    if *startup != StartupPhase::Running {
        if Instant::now() >= startup_deadline {
            return Err(Outcome::Failed("worker_startup_timeout"));
        }
        match *startup {
            StartupPhase::Identity => validate_handshake(&spec.plugin_id, &message)
                .map_err(|_| Outcome::Failed("worker_handshake_invalid"))?,
            StartupPhase::Configuration => {
                if !matches!(&message, WorkerMessage::ConfigurationReady { revision }
                    if spec.configuration.as_ref().is_some_and(|configuration| &configuration.revision == revision))
                {
                    return Err(Outcome::Failed("worker_configuration_ack_invalid"));
                }
            }
            StartupPhase::Running => unreachable!(),
        }
        let authority = entry.authority.lock().unwrap_or_else(|e| e.into_inner());
        if !authority.enabled || authority.epoch != entry.epoch {
            return Err(Outcome::Stopped);
        }
        if *startup == StartupPhase::Identity {
            if let Some(configuration) = &spec.configuration {
                pipes
                    .configure(configuration, startup_deadline)
                    .map_err(Outcome::Failed)?;
                *startup = StartupPhase::Configuration;
                return Ok(());
            }
        }
        *startup = StartupPhase::Running;
        {
            let mut snapshot = entry.snapshot.lock().unwrap_or_else(|e| e.into_inner());
            snapshot.state = WorkerState::Running;
            snapshot.last_error = None;
        }
        drop(authority);
        (entry.changed)();
        return Ok(());
    }
    let authority = entry.authority.lock().unwrap_or_else(|e| e.into_inner());
    if !authority.enabled || authority.epoch != entry.epoch {
        return Err(Outcome::Stopped);
    }
    let mut notify = false;
    match message {
        WorkerMessage::HttpVideo {
            id,
            url,
            title,
            media_kind,
            cooldown_id,
        } => {
            let grant = spec
                .http_video
                .as_ref()
                .ok_or(Outcome::Failed("worker_http_video_unauthorized"))?;
            grant
                .validate_url(&url)
                .map_err(|_| Outcome::Failed("worker_http_video_url_invalid"))?;
            notify = entry.queue_http_video(
                grant,
                id.clone(),
                url,
                title.clone(),
                MediaAdmission {
                    kind: media_kind,
                    cooldown_id,
                    live_preview: false,
                },
            );
            if notify && spec.desktop_notifications {
                entry.queue_notification(DesktopNotification {
                    plugin_id: spec.plugin_id.clone(),
                    id,
                    title,
                    body: if media_kind.is_video() {
                        "Camera motion clip available"
                    } else {
                        "Camera snapshot available"
                    }
                    .into(),
                    live_view: None,
                });
            }
        }
        WorkerMessage::HttpLive { id, url, title } => {
            let grant = spec
                .http_video
                .as_ref()
                .ok_or(Outcome::Failed("worker_http_video_unauthorized"))?;
            grant
                .validate_preview_url(&url)
                .map_err(|_| Outcome::Failed("worker_http_live_url_invalid"))?;
            notify = entry.queue_http_video(
                grant,
                id,
                url,
                title,
                MediaAdmission {
                    kind: HttpMediaKind::Video,
                    cooldown_id: None,
                    live_preview: true,
                },
            );
            // The automatic preview already announces motion. A native toast
            // for the same event would cover the camera window.
        }
        WorkerMessage::MqttLive {
            id,
            title,
            camera_id,
            url,
        } => {
            let mapping = spec
                .live_view
                .as_ref()
                .ok_or(Outcome::Failed("worker_mqtt_live_unauthorized"))?;
            let (url, grant) = mapping
                .mqtt_preview(&camera_id, &url)
                .map_err(|_| Outcome::Failed("worker_mqtt_live_url_invalid"))?;
            notify = entry.queue_http_video(
                &grant,
                id,
                url.into(),
                title,
                MediaAdmission {
                    kind: HttpMediaKind::Video,
                    cooldown_id: Some(format!("mqtt:{camera_id}")),
                    live_preview: true,
                },
            );
        }
        WorkerMessage::LiveView {
            id,
            title,
            live_view_id,
        } => {
            let mapping = spec
                .live_view
                .as_ref()
                .ok_or(Outcome::Failed("worker_live_view_unauthorized"))?;
            let preview = mapping
                .preview(&live_view_id)
                .map_err(|_| Outcome::Failed("worker_live_view_preview_unauthorized"))?;
            if let Some((url, grant)) = preview {
                notify = entry.queue_http_video(
                    &grant,
                    id,
                    url.into(),
                    title,
                    MediaAdmission {
                        kind: HttpMediaKind::Video,
                        cooldown_id: Some(live_view_id),
                        live_preview: true,
                    },
                );
            }
        }
        WorkerMessage::Notification {
            id,
            title,
            body,
            live_view_id,
        } => {
            if !spec.desktop_notifications {
                return Err(Outcome::Failed("worker_notification_unauthorized"));
            }
            let live_view = if let Some(id) = live_view_id {
                let grant = spec
                    .live_view
                    .as_ref()
                    .ok_or(Outcome::Failed("worker_live_view_unauthorized"))?;
                let lease = entry
                    .generation_lease
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .ok_or(Outcome::Stopped)?;
                grant
                    .resolve(&id)
                    .map(|url| LiveViewRequest { lease, id, url })
            } else {
                None
            };
            notify = entry.queue_notification(DesktopNotification {
                plugin_id: spec.plugin_id.clone(),
                id,
                title,
                body,
                live_view,
            });
        }
        WorkerMessage::Contributions {
            items,
            presentation,
        } => {
            entry.replace_contributions(items, presentation);
            notify = true;
        }
        WorkerMessage::ActionResult { request_id, value } => {
            if let Some(request) = pending.remove(&request_id) {
                let result = if Instant::now() >= request.deadline {
                    Err(PluginError::DeadlineExceeded)
                } else {
                    Ok(value)
                };
                let _ = request.reply.send(result);
            }
        }
        WorkerMessage::ActionError { request_id, .. } => {
            if let Some(request) = pending.remove(&request_id) {
                let _ = request.reply.send(Err(PluginError::WorkerError));
            }
        }
        WorkerMessage::Event { name, data } => {
            if name == "settings_choices" {
                entry
                    .settings_choices
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .accept(data)
                    .map_err(|_| Outcome::Failed("worker_settings_choices_invalid"))?;
            }
        }
        WorkerMessage::Ready { .. } | WorkerMessage::ConfigurationReady { .. } => {
            return Err(Outcome::Failed("worker_duplicate_handshake"));
        }
    }
    drop(authority);
    if notify {
        (entry.changed)();
    }
    Ok(())
}

fn handle_control(
    control: Control,
    entry: &WorkerEntry,
    generation: u64,
    ready: bool,
    pipes: &WorkerPipes,
    pending: &mut HashMap<String, Pending>,
) {
    match control {
        Control::Cancel(id) => {
            if let Some(request) = pending.remove(&id) {
                let _ = pipes.send(&HostMessage::Cancel { request_id: id });
                let _ = request.reply.send(Err(PluginError::Cancelled));
            }
        }
        Control::Action {
            request_id,
            generation: expected,
            action_id,
            params,
            numeric,
            cancellation,
            deadline,
            reply,
        } => {
            let authority = entry.authority.lock().unwrap_or_else(|e| e.into_inner());
            let error = if !authority.enabled
                || authority.epoch != entry.epoch
                || !ready
                || expected != generation
            {
                Some(PluginError::Unavailable)
            } else if Instant::now() >= deadline {
                Some(PluginError::DeadlineExceeded)
            } else if !advertises(&entry.snapshot().contributions, &action_id, &params)
                || numeric.as_ref().is_some_and(|lease| {
                    !current_numeric(
                        &entry.numeric.lock().unwrap_or_else(|e| e.into_inner()),
                        lease,
                        &params,
                    )
                })
                || (numeric.is_none()
                    && entry
                        .numeric
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .contains_key(&action_id))
            {
                Some(PluginError::UnknownAction)
            } else if pending.len() >= MAX_IN_FLIGHT {
                Some(PluginError::Busy)
            } else {
                None
            };
            if let Some(error) = error {
                let _ = reply.send(Err(error));
                return;
            }
            if pipes
                .writer
                .try_send(Outgoing::Action {
                    request_id: request_id.clone(),
                    action_id,
                    params,
                    numeric: numeric.map(|lease| NumericWriteGuard {
                        lease,
                        registry: entry.numeric.clone(),
                        rejected: pipes.rejection_sender.clone(),
                    }),
                    deadline,
                    cancellation,
                })
                .is_err()
            {
                let _ = reply.send(Err(PluginError::Busy));
                return;
            }
            pending.insert(request_id, Pending { deadline, reply });
        }
    }
}

fn expire_pending(pending: &mut HashMap<String, Pending>, pipes: &WorkerPipes) {
    let expired: Vec<_> = pending
        .iter()
        .filter(|(_, request)| request.reply.is_closed() || Instant::now() >= request.deadline)
        .map(|(id, _)| id.clone())
        .collect();
    for id in expired {
        if let Some(request) = pending.remove(&id) {
            let _ = pipes.send(&HostMessage::Cancel { request_id: id });
            let _ = request.reply.send(Err(PluginError::DeadlineExceeded));
        }
    }
}

fn reject_control(control: Option<Control>) {
    if let Some(Control::Action { reply, .. }) = control {
        let _ = reply.send(Err(PluginError::Unavailable));
    }
}

async fn terminate(child: &mut Child, pipes: Option<&WorkerPipes>) -> bool {
    if let Some(pipes) = pipes {
        let _ = pipes.send(&HostMessage::Shutdown);
    }
    if matches!(time::timeout(STOP_GRACE, child.wait()).await, Ok(Ok(_))) {
        return true;
    }
    let _ = child.start_kill();
    // A kernel-delayed reap is reported as failed; that registration cannot be replaced.
    // kill_on_drop keeps Tokio's fallback kill/reap behavior on this exceptional path.
    matches!(time::timeout(REAP_TIMEOUT, child.wait()).await, Ok(Ok(_)))
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
