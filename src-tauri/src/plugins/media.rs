//! Native ownership of completed plugin media files and their exact windows.
//!
//! Initialization and shutdown run while the package-manager lifetime lease is
//! held. The directory is a private sibling of the package store, never payload
//! content or an addition to the global asset-protocol scope.

use super::generation::GenerationLease;
use super::http_video::{
    media_byte_limit, media_content_type, media_extension, TransferPolicy, VideoError,
    VideoTransfer, MAX_CLIP_BYTES,
};
use super::protocol::HttpMediaKind;
use super::runtime::{MediaCameraId, QueuedHttpVideo};
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, Metadata, OpenOptions};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch, Notify, Semaphore};
use tokio::task::JoinHandle;
use uuid::Uuid;

const MAX_QUEUED: usize = 8;
const MAX_WINDOWS: usize = 8;
const MAX_TRANSFERS: usize = 2;
const MAX_MEDIA_BYTES: u64 = 512 * 1024 * 1024;
const MAX_RANGE_BYTES: usize = 1024 * 1024;
const MAX_RANGE_READS: usize = 4;
const EVENT_CAPACITY: usize = (MAX_QUEUED + MAX_WINDOWS) * 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MediaError {
    Unavailable,
    Busy,
    InvalidRequest,
    RangeRequired,
    NotFound,
    Storage,
    Transfer(VideoError),
}

impl MediaError {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Unavailable => "media_unavailable",
            Self::Busy => "media_busy",
            Self::InvalidRequest => "media_invalid_request",
            Self::RangeRequired => "media_range_required",
            Self::NotFound => "media_not_found",
            Self::Storage => "media_storage_failed",
            Self::Transfer(VideoError::Cancelled) => "media_cancelled",
            Self::Transfer(VideoError::Deadline) => "media_deadline",
            Self::Transfer(VideoError::Network) => "media_network_failed",
            Self::Transfer(VideoError::Http) => "media_http_failed",
            Self::Transfer(VideoError::Empty) => "media_empty",
            Self::Transfer(VideoError::Oversized) => "media_oversized",
            Self::Transfer(VideoError::InvalidMedia) => "media_invalid_format",
            Self::Transfer(VideoError::Storage) => "media_storage_failed",
        }
    }
}

impl std::fmt::Display for MediaError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code())
    }
}

/// Native adapter input. Preview URLs are disclosed only to their owning local viewer.
pub(crate) struct ReadyMedia {
    pub plugin_id: String,
    pub live_url: Option<reqwest::Url>,
    pub media_id: String,
    pub window_label: String,
    pub title: String,
    pub media_kind: HttpMediaKind,
    pub error: Option<MediaError>,
}

pub(crate) enum MediaEvent {
    Ready(ReadyMedia),
    Close { window_label: String },
}

pub(crate) struct MediaRange {
    pub status: u16,
    pub bytes: Vec<u8>,
    pub content_length: u64,
    pub content_range: Option<String>,
    pub content_type: &'static str,
    _permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Queued,
    Downloading,
    Ready,
    Closing,
}

struct Item {
    camera_id: Option<MediaCameraId>,
    lease: GenerationLease,
    label: String,
    phase: Phase,
    media_kind: HttpMediaKind,
    bytes: u64,
    file: Option<Arc<MediaFile>>,
    live_url: Option<reqwest::Url>,
    retired: watch::Sender<bool>,
    cleanup_failed: bool,
}

#[derive(Default)]
struct State {
    root: Option<PathBuf>,
    transfer: Option<VideoTransfer>,
    items: HashMap<String, Item>,
}

struct Inner {
    state: Mutex<State>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    events: mpsc::Sender<MediaEvent>,
    changed: Notify,
    shutdown: watch::Sender<bool>,
    closing: AtomicBool,
    cleanup_failed: Arc<AtomicBool>,
    native_failure: watch::Sender<u64>,
    reads: Arc<Semaphore>,
    initialize: tokio::sync::Mutex<()>,
    drain: tokio::sync::Mutex<()>,
}

#[derive(Clone)]
pub(crate) struct MediaService(Arc<Inner>);

impl MediaService {
    pub(crate) fn new() -> (Self, mpsc::Receiver<MediaEvent>) {
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let (shutdown, _) = watch::channel(false);
        (
            Self(Arc::new(Inner {
                state: Mutex::new(State::default()),
                tasks: Mutex::new(Vec::new()),
                events,
                changed: Notify::new(),
                shutdown,
                closing: AtomicBool::new(false),
                cleanup_failed: Arc::new(AtomicBool::new(false)),
                native_failure: watch::channel(0).0,
                reads: Arc::new(Semaphore::new(MAX_RANGE_READS)),
                initialize: tokio::sync::Mutex::new(()),
                drain: tokio::sync::Mutex::new(()),
            })),
            receiver,
        )
    }

    pub(crate) async fn initialize(&self, root: PathBuf) -> Result<(), String> {
        self.initialize_with_policy(root, TransferPolicy::default())
            .await
    }

    async fn initialize_with_policy(
        &self,
        root: PathBuf,
        policy: TransferPolicy,
    ) -> Result<(), String> {
        let _initialization = self.0.initialize.lock().await;
        if self.0.closing.load(Ordering::Acquire)
            || self
                .0
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .root
                .is_some()
        {
            return Err("Plugin media is already initialized or closed".into());
        }
        let directory = root.clone();
        tokio::task::spawn_blocking(move || recover_directory(&directory))
            .await
            .map_err(|_| "Plugin media initialization task failed")?
            .map_err(|_| "Plugin media directory is unavailable or unsafe")?;
        let transfer =
            VideoTransfer::new(policy).map_err(|error| MediaError::Transfer(error).to_string())?;
        if self.0.closing.load(Ordering::Acquire) {
            return Err("Plugin media is closed".into());
        }
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.root = Some(root);
        state.transfer = Some(transfer);
        Ok(())
    }

    /// Only bounded metadata and task admission occur here; never file or HTTP I/O.
    pub(crate) fn try_submit(&self, request: QueuedHttpVideo) -> Result<(), MediaError> {
        let (url, preview_duration) = if request.live_preview {
            if request.media_kind != HttpMediaKind::Video {
                return Err(MediaError::InvalidRequest);
            }
            (
                request.grant.validate_preview_url(&request.url),
                Some(
                    request
                        .grant
                        .preview_duration()
                        .ok_or(MediaError::InvalidRequest)?,
                ),
            )
        } else {
            (request.grant.validate_url(&request.url), None)
        };
        let url = url.map_err(|_| MediaError::InvalidRequest)?;
        let mut tasks = self.0.tasks.lock().unwrap_or_else(|e| e.into_inner());
        if self.0.closing.load(Ordering::Acquire) || self.0.cleanup_failed.load(Ordering::Acquire) {
            return Err(MediaError::Unavailable);
        }
        let id = Uuid::new_v4().to_string();
        let label = if request.live_preview {
            format!("plugin-preview-{id}")
        } else {
            format!("plugin-video-{id}")
        };
        let (retired, receiver) = watch::channel(false);
        let admitted = request
            .lease
            .commit_if_active(|| {
                let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.root.is_none() {
                    return Err(MediaError::Unavailable);
                }
                // Keep camera ownership through queued, hidden, visible and
                // closing phases, including revoked generations whose native
                // window has not yet acknowledged destruction. Never replay a
                // duplicate later when this item finally releases its slot.
                if request.camera_id.as_ref().is_some_and(|camera| {
                    state.items.values().any(|item| {
                        item.lease.plugin_id() == request.lease.plugin_id()
                            && item.camera_id.as_ref() == Some(camera)
                    })
                }) {
                    return Ok(false);
                }
                if state
                    .items
                    .values()
                    .filter(|item| item.phase == Phase::Queued)
                    .count()
                    >= MAX_QUEUED
                {
                    return Err(MediaError::Busy);
                }
                state.items.insert(
                    id.clone(),
                    Item {
                        camera_id: request.camera_id.clone(),
                        lease: request.lease.clone(),
                        label,
                        phase: Phase::Queued,
                        media_kind: request.media_kind,
                        bytes: 0,
                        file: None,
                        live_url: request.live_preview.then(|| url.clone()),
                        retired,
                        cleanup_failed: false,
                    },
                );
                Ok(true)
            })
            .map_err(|_| MediaError::Unavailable)??;
        if !admitted {
            return Ok(());
        }
        tasks.retain(|task| !task.is_finished());
        let service = self.clone();
        tasks.push(tokio::spawn(async move {
            if let Some(duration) = preview_duration {
                service
                    .own_live(id, request.lease, request.title, url, receiver, duration)
                    .await;
            } else {
                service
                    .own_item(
                        id,
                        request.lease,
                        request.title,
                        request.media_kind,
                        (url, request.grant.bearer_token().map(str::to_owned)),
                        receiver,
                    )
                    .await;
            }
        }));
        Ok(())
    }

    pub(crate) fn has_owned_work(&self) -> bool {
        !self
            .0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .items
            .is_empty()
    }

    pub(crate) fn is_window_active(&self, id: &str, label: &str) -> bool {
        self.window_lease(id, label)
            .is_some_and(|lease| lease.is_active())
    }

    /// The exact active viewer can bootstrap its image without publishing the
    /// private source in routes, plugin snapshots, or caller-selected arguments.
    pub(crate) fn live_preview_url(&self, id: &str, label: &str) -> Option<reqwest::Url> {
        let lease = self.window_lease(id, label)?;
        lease
            .commit_if_active(|| {
                let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
                let item = state.items.get(id)?;
                (item.label == label && item.phase == Phase::Ready && !*item.retired.borrow())
                    .then(|| item.live_url.clone())
                    .flatten()
            })
            .ok()
            .flatten()
    }

    /// Recheck ownership after hidden construction or before native display.
    pub(crate) fn window_ready(&self, id: &str, label: &str) -> bool {
        self.is_window_active(id, label)
    }

    /// Revoke display authority before closing an unresponsive hidden viewer.
    /// The file and slot remain owned until native destruction is acknowledged.
    pub(crate) fn window_closing(&self, id: &str, label: &str) -> bool {
        let Some(lease) = self.window_lease(id, label) else {
            return false;
        };
        lease
            .commit_if_active(|| {
                let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
                let Some(item) = state.items.get_mut(id) else {
                    return false;
                };
                if item.label != label || item.phase != Phase::Ready || *item.retired.borrow() {
                    return false;
                }
                item.phase = Phase::Closing;
                true
            })
            .unwrap_or(false)
    }

    fn window_lease(&self, id: &str, label: &str) -> Option<GenerationLease> {
        if self.0.closing.load(Ordering::Acquire) {
            return None;
        }
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        let item = state.items.get(id)?;
        (item.label == label && item.phase == Phase::Ready && !*item.retired.borrow())
            .then(|| item.lease.clone())
    }

    pub(crate) fn window_failed(&self, id: &str) {
        if let Some(item) = self
            .0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .items
            .get(id)
        {
            item.retired.send_replace(true);
        }
        self.0.changed.notify_waiters();
    }

    /// Also acknowledge a Close event when the native window is already absent.
    pub(crate) fn window_destroyed(&self, label: &str) {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        for item in state.items.values().filter(|item| item.label == label) {
            item.retired.send_replace(true);
        }
        self.0.changed.notify_waiters();
    }

    /// Native failure is not proof of absence. Keep the file, slot and task so
    /// a later shutdown can retry closing the same exact window.
    pub(crate) fn window_cleanup_failed(&self, label: &str) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut failed = false;
        for item in state.items.values_mut().filter(|item| item.label == label) {
            if !*item.retired.borrow() {
                item.cleanup_failed = true;
                failed = true;
            }
        }
        if failed {
            self.0
                .native_failure
                .send_modify(|revision| *revision = revision.wrapping_add(1));
        }
    }

    async fn reserve(
        &self,
        id: &str,
        lease: &GenerationLease,
        retired: &mut watch::Receiver<bool>,
        preview: bool,
    ) -> bool {
        let mut shutdown = self.0.shutdown.subscribe();
        loop {
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !lease.is_active()
                || *retired.borrow()
                || *shutdown.borrow()
                || self.0.cleanup_failed.load(Ordering::Acquire)
            {
                return false;
            }
            let reserved = lease
                .commit_if_active(|| {
                    let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
                    let transfers = state
                        .items
                        .values()
                        .filter(|item| item.phase == Phase::Downloading)
                        .count();
                    let windows = state
                        .items
                        .values()
                        .filter(|item| item.phase != Phase::Queued)
                        .count();
                    let bytes: u64 = state.items.values().map(|item| item.bytes).sum();
                    let Some(item) = state.items.get_mut(id) else {
                        return false;
                    };
                    let reservation = if preview {
                        0
                    } else {
                        media_byte_limit(item.media_kind)
                    };
                    if windows >= MAX_WINDOWS
                        || (!preview
                            && (transfers >= MAX_TRANSFERS
                                || bytes + reservation > MAX_MEDIA_BYTES))
                    {
                        return false;
                    }
                    item.phase = if preview {
                        Phase::Ready
                    } else {
                        Phase::Downloading
                    };
                    item.bytes = reservation;
                    true
                })
                .unwrap_or(false);
            if reserved {
                return true;
            }
            tokio::select! {
                biased;
                _ = lease.cancelled() => return false,
                _ = retired.wait_for(|done| *done) => return false,
                _ = shutdown.wait_for(|done| *done) => return false,
                _ = &mut changed => {},
            }
        }
    }

    async fn own_item(
        &self,
        id: String,
        lease: GenerationLease,
        title: String,
        media_kind: HttpMediaKind,
        source: (reqwest::Url, Option<String>),
        mut retired: watch::Receiver<bool>,
    ) {
        if !self.reserve(&id, &lease, &mut retired, false).await {
            self.remove_item(&id).await;
            return;
        }
        let (root, transfer) = {
            let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            (
                state.root.clone().expect("initialized media root"),
                state.transfer.clone().expect("initialized media client"),
            )
        };
        let path = root.join(format!("clip-{id}.{}", media_extension(media_kind)));
        let cleanup_failed = self.0.cleanup_failed.clone();
        let created =
            tokio::task::spawn_blocking(move || MediaFile::create(path, cleanup_failed)).await;
        let (file, result) = match created {
            Ok(Ok(file)) => {
                let file = Arc::new(file);
                let result = transfer
                    .with_bearer(source.1)
                    .download(
                        source.0,
                        media_kind,
                        file.clone(),
                        &lease,
                        self.0.shutdown.subscribe(),
                    )
                    .await;
                (Some(file), result)
            }
            _ => (None, Err(VideoError::Storage)),
        };
        let mut file = file;
        if result.is_err() {
            let discarded = file.take();
            let _ = tokio::task::spawn_blocking(move || drop(discarded)).await;
        }
        let committed = lease
            .commit_if_active(|| {
                let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
                let item = state.items.get_mut(&id)?;
                if *item.retired.borrow() || self.0.closing.load(Ordering::Acquire) {
                    return None;
                }
                item.phase = Phase::Ready;
                item.bytes = result.unwrap_or(0);
                item.file = file.clone();
                Some(item.label.clone())
            })
            .ok()
            .flatten();
        self.0.changed.notify_waiters();
        let Some(label) = committed else {
            let _ = tokio::task::spawn_blocking(move || drop(file)).await;
            self.remove_item(&id).await;
            return;
        };
        // The registry now owns the file. Dropping this extra reference does no I/O.
        drop(file);
        let ready = MediaEvent::Ready(ReadyMedia {
            plugin_id: lease.plugin_id().to_owned(),
            live_url: None,
            media_id: id.clone(),
            window_label: label.clone(),
            title,
            media_kind,
            error: result.err().map(MediaError::Transfer),
        });
        self.present_until_retired(id, lease, label, retired, ready, None)
            .await;
    }

    async fn own_live(
        &self,
        id: String,
        lease: GenerationLease,
        title: String,
        url: reqwest::Url,
        mut retired: watch::Receiver<bool>,
        duration: std::time::Duration,
    ) {
        // Motion expires while waiting for a shared window. A preview reserves
        // no transfer capacity, file, or disk bytes; the webview owns its stream.
        if !tokio::time::timeout(
            std::time::Duration::from_secs(3),
            self.reserve(&id, &lease, &mut retired, true),
        )
        .await
        .unwrap_or(false)
        {
            self.remove_item(&id).await;
            return;
        }
        let label = {
            let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            state
                .items
                .get(&id)
                .expect("reserved preview")
                .label
                .clone()
        };
        let ready = MediaEvent::Ready(ReadyMedia {
            plugin_id: lease.plugin_id().to_owned(),
            live_url: Some(url),
            media_id: id.clone(),
            window_label: label.clone(),
            title,
            media_kind: HttpMediaKind::Video,
            error: None,
        });
        self.present_until_retired(id, lease, label, retired, ready, Some(duration))
            .await;
    }

    async fn present_until_retired(
        &self,
        id: String,
        lease: GenerationLease,
        label: String,
        mut retired: watch::Receiver<bool>,
        ready: MediaEvent,
        lifetime: Option<std::time::Duration>,
    ) {
        let mut shutdown = self.0.shutdown.subscribe();
        let delivered = tokio::select! {
            biased;
            _ = lease.cancelled() => false,
            _ = shutdown.wait_for(|done| *done) => false,
            result = self.0.events.send(ready) => result.is_ok(),
        };
        if delivered {
            tokio::select! {
                biased;
                _ = retired.wait_for(|done| *done) => {},
                _ = lease.cancelled() => {},
                _ = shutdown.wait_for(|done| *done) => {},
                _ = async {
                    if let Some(duration) = lifetime { tokio::time::sleep(duration).await; }
                    else { std::future::pending::<()>().await; }
                } => {},
            }
            if lifetime.is_some() {
                if let Some(item) = self
                    .0
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .items
                    .get_mut(&id)
                {
                    // Deny display before Close; retain the slot until Destroyed
                    // acknowledges that the native stream no longer exists.
                    item.phase = Phase::Closing;
                }
            }
            if !*retired.borrow()
                && self
                    .0
                    .events
                    .send(MediaEvent::Close {
                        window_label: label,
                    })
                    .await
                    .is_ok()
            {
                // Keep the slot until the adapter acknowledges native absence.
                let _ = retired.wait_for(|done| *done).await;
            }
        }
        self.remove_item(&id).await;
    }

    async fn remove_item(&self, id: &str) {
        let file = {
            let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            state.items.get_mut(id).and_then(|item| {
                item.retired.send_replace(true);
                item.file.take()
            })
        };
        if let Some(file) = file {
            loop {
                let changed = file.activity.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if file.activity.readers.load(Ordering::Acquire) == 0 {
                    break;
                }
                changed.await;
            }
            let _ = tokio::task::spawn_blocking(move || drop(file)).await;
        }
        // Keep the reservation until all handles have closed and unlink finished.
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .items
            .remove(id);
        self.0.changed.notify_waiters();
    }

    pub(crate) async fn read_range(
        &self,
        id: &str,
        label: &str,
        range: Option<&str>,
        head: bool,
    ) -> Result<MediaRange, MediaError> {
        let lease = self.window_lease(id, label).ok_or(MediaError::NotFound)?;
        let (file, size, kind) = lease
            .commit_if_active(|| {
                let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
                let item = state.items.get(id)?;
                if item.label != label || item.phase != Phase::Ready || *item.retired.borrow() {
                    return None;
                }
                Some((
                    ReadOwner::new(item.file.clone()?),
                    item.bytes,
                    item.media_kind,
                ))
            })
            .ok()
            .flatten()
            .ok_or(MediaError::NotFound)?;
        if head {
            return Ok(MediaRange {
                status: 200,
                bytes: Vec::new(),
                content_length: size,
                content_range: None,
                content_type: media_content_type(kind),
                _permit: None,
            });
        }
        // Images need a complete GET for <img>; the byte cap and shared read
        // semaphore bound these responses without changing video range behavior.
        let full_limit = if kind == HttpMediaKind::Video {
            MAX_RANGE_BYTES as u64
        } else {
            media_byte_limit(kind)
        };
        let selected = select_range(range, size, full_limit)?;
        let Some((start, end, partial)) = selected else {
            return Ok(MediaRange {
                status: 416,
                bytes: Vec::new(),
                content_length: 0,
                content_range: Some(format!("bytes */{size}")),
                content_type: media_content_type(kind),
                _permit: None,
            });
        };
        let permit = self
            .0
            .reads
            .clone()
            .try_acquire_owned()
            .map_err(|_| MediaError::Busy)?;
        let (bytes, permit) = tokio::task::spawn_blocking(move || {
            let bytes = file
                .file
                .as_ref()
                .ok_or(MediaError::Storage)?
                .read(start, (end - start + 1) as usize)?;
            Ok::<_, MediaError>((bytes, permit))
        })
        .await
        .map_err(|_| MediaError::Storage)??;
        if !lease.is_active() || !self.is_window_active(id, label) {
            return Err(MediaError::NotFound);
        }
        Ok(MediaRange {
            status: if partial { 206 } else { 200 },
            content_length: bytes.len() as u64,
            bytes,
            content_range: partial.then(|| format!("bytes {start}-{end}/{size}")),
            content_type: media_content_type(kind),
            _permit: Some(permit),
        })
    }

    pub(crate) async fn shutdown(&self) -> Result<(), String> {
        let _drain = self.0.drain.lock().await;
        let mut failures = self.0.native_failure.subscribe();
        self.0.closing.store(true, Ordering::Release);
        self.0.shutdown.send_replace(true);
        let retry = {
            let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            state
                .items
                .values_mut()
                .filter_map(|item| {
                    if !item.cleanup_failed || *item.retired.borrow() {
                        return None;
                    }
                    item.cleanup_failed = false;
                    Some(item.label.clone())
                })
                .collect::<Vec<_>>()
        };
        for window_label in retry {
            let _ = self.0.events.send(MediaEvent::Close { window_label }).await;
        }
        // Admission and task publication hold this same short mutex.
        let tasks = std::mem::take(&mut *self.0.tasks.lock().unwrap_or_else(|e| e.into_inner()));
        let mut draining = TaskDrain {
            service: self.clone(),
            pending: tasks.into(),
            current: None,
        };
        while let Some(task) = draining.pending.pop_front() {
            draining.current = Some(task);
            let joined = tokio::select! {
                biased;
                _ = failures.changed() => return Err("Plugin media native window cleanup failed".into()),
                joined = draining.current.as_mut().expect("owned media task") => joined,
            };
            draining.current = None;
            if joined.is_err() {
                self.0.cleanup_failed.store(true, Ordering::Release);
            }
        }
        // Even a cancelled range caller leaves its permit in the blocking task.
        let _reads = self
            .0
            .reads
            .acquire_many(MAX_RANGE_READS as u32)
            .await
            .map_err(|_| "Plugin media reads could not drain")?;
        if self.0.cleanup_failed.load(Ordering::Acquire) || self.has_owned_work() {
            return Err("Plugin media cleanup could not complete".into());
        }
        Ok(())
    }
}

/// An early failure or cancellation restores every unjoined task to the service.
/// JoinHandle drop must never detach unfinished work from later shutdown retries.
struct TaskDrain {
    service: MediaService,
    pending: VecDeque<JoinHandle<()>>,
    current: Option<JoinHandle<()>>,
}

impl Drop for TaskDrain {
    fn drop(&mut self) {
        let mut tasks = self
            .service
            .0
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        tasks.extend(self.current.take());
        tasks.extend(self.pending.drain(..));
    }
}

fn select_range(
    range: Option<&str>,
    size: u64,
    full_limit: u64,
) -> Result<Option<(u64, u64, bool)>, MediaError> {
    if size == 0 {
        return Ok(None);
    }
    let Some(range) = range else {
        return if size <= full_limit {
            Ok(Some((0, size - 1, false)))
        } else {
            Err(MediaError::RangeRequired)
        };
    };
    if range.len() > 100 || !range.starts_with("bytes=") || range.contains(',') {
        return Ok(None);
    }
    let Some((start, end)) = range[6..].split_once('-') else {
        return Ok(None);
    };
    let parse = |value: &str| {
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            None
        } else {
            value.parse::<u64>().ok()
        }
    };
    let (start, end) = if start.is_empty() {
        let Some(suffix) = parse(end).filter(|suffix| *suffix != 0) else {
            return Ok(None);
        };
        (size.saturating_sub(suffix), size - 1)
    } else {
        let Some(start) = parse(start).filter(|start| *start < size) else {
            return Ok(None);
        };
        let end = if end.is_empty() {
            size - 1
        } else {
            let Some(end) = parse(end).filter(|end| *end >= start) else {
                return Ok(None);
            };
            end.min(size - 1)
        };
        (start, end)
    };
    Ok(Some((
        start,
        end.min(start + MAX_RANGE_BYTES as u64 - 1),
        true,
    )))
}

/// Every disk task retains this owner; the last owner closes before unlinking.
pub(super) struct MediaFile {
    file: Option<File>,
    path: PathBuf,
    identity: (u64, u64),
    cleanup_failed: Arc<AtomicBool>,
    activity: Arc<FileActivity>,
}

#[derive(Default)]
struct FileActivity {
    readers: AtomicUsize,
    changed: Notify,
}

struct ReadOwner {
    file: Option<Arc<MediaFile>>,
    activity: Arc<FileActivity>,
}

impl ReadOwner {
    fn new(file: Arc<MediaFile>) -> Self {
        let activity = file.activity.clone();
        activity.readers.fetch_add(1, Ordering::AcqRel);
        Self {
            file: Some(file),
            activity,
        }
    }
}

impl Drop for ReadOwner {
    fn drop(&mut self) {
        drop(self.file.take());
        self.activity.readers.fetch_sub(1, Ordering::AcqRel);
        self.activity.changed.notify_waiters();
    }
}

impl MediaFile {
    fn create(path: PathBuf, cleanup_failed: Arc<AtomicBool>) -> Result<Self, MediaError> {
        check_directory(path.parent().ok_or(MediaError::Storage)?, true)?;
        let mut options = private_options();
        options.write(true).create_new(true);
        let file = options.open(&path).map_err(|_| MediaError::Storage)?;
        let identity = file_identity(&file).inspect_err(|_| {
            // Creation succeeded, but this path cannot be safely identified for
            // removal. Retain it and stop admitting unaccounted storage work.
            cleanup_failed.store(true, Ordering::Release);
        })?;
        Ok(Self {
            file: Some(file),
            path,
            identity,
            cleanup_failed,
            activity: Arc::default(),
        })
    }

    pub(super) fn reset(&self) -> Result<(), VideoError> {
        self.file
            .as_ref()
            .ok_or(VideoError::Storage)?
            .set_len(0)
            .map_err(|_| VideoError::Storage)
    }

    pub(super) fn write(&self, mut offset: u64, mut bytes: &[u8]) -> Result<(), VideoError> {
        let file = self.file.as_ref().ok_or(VideoError::Storage)?;
        while !bytes.is_empty() {
            #[cfg(unix)]
            let written = {
                use std::os::unix::fs::FileExt;
                file.write_at(bytes, offset)
            };
            #[cfg(windows)]
            let written = {
                use std::os::windows::fs::FileExt;
                file.seek_write(bytes, offset)
            };
            let written = written.map_err(|_| VideoError::Storage)?;
            if written == 0 {
                return Err(VideoError::Storage);
            }
            offset += written as u64;
            bytes = &bytes[written..];
        }
        Ok(())
    }

    pub(super) fn flush(&self) -> Result<(), VideoError> {
        self.file
            .as_ref()
            .ok_or(VideoError::Storage)?
            .sync_all()
            .map_err(|_| VideoError::Storage)
    }

    fn read(&self, mut offset: u64, length: usize) -> Result<Vec<u8>, MediaError> {
        let file = self.file.as_ref().ok_or(MediaError::Storage)?;
        if file_identity(file)? != self.identity {
            return Err(MediaError::Storage);
        }
        let mut bytes = vec![0; length];
        let mut remaining = bytes.as_mut_slice();
        while !remaining.is_empty() {
            #[cfg(unix)]
            let read = {
                use std::os::unix::fs::FileExt;
                file.read_at(remaining, offset)
            };
            #[cfg(windows)]
            let read = {
                use std::os::windows::fs::FileExt;
                file.seek_read(remaining, offset)
            };
            let read = read.map_err(|_| MediaError::Storage)?;
            if read == 0 {
                return Err(MediaError::Storage);
            }
            offset += read as u64;
            remaining = &mut remaining[read..];
        }
        Ok(bytes)
    }
}

impl Drop for MediaFile {
    fn drop(&mut self) {
        let result = (|| {
            check_directory(self.path.parent().ok_or(MediaError::Storage)?, true)?;
            let inspected = private_options()
                .open(&self.path)
                .map_err(|_| MediaError::Storage)?;
            if file_identity(&inspected)? != self.identity {
                return Err(MediaError::Storage);
            }
            drop(inspected);
            drop(self.file.take());
            fs::remove_file(&self.path).map_err(|_| MediaError::Storage)
        })();
        if result.is_err() {
            self.cleanup_failed.store(true, Ordering::Release);
        }
    }
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true);
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
        options.custom_flags(0x0020_0000);
    }
    options
}

fn file_identity(file: &File) -> Result<(u64, u64), MediaError> {
    let metadata = file.metadata().map_err(|_| MediaError::Storage)?;
    if !metadata.is_file() || is_link(&metadata) || metadata.len() > MAX_CLIP_BYTES {
        return Err(MediaError::Storage);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1
            || metadata.mode() & 0o077 != 0
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(MediaError::Storage);
        }
        Ok((metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: the live file owns the handle and information is writable API storage.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0
            || information.nNumberOfLinks != 1
        {
            return Err(MediaError::Storage);
        }
        Ok((
            information.dwVolumeSerialNumber as u64,
            ((information.nFileIndexHigh as u64) << 32) | information.nFileIndexLow as u64,
        ))
    }
}

fn is_link(metadata: &Metadata) -> bool {
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

fn check_directory(path: &Path, private: bool) -> Result<(), MediaError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(MediaError::Storage);
    }
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).map_err(|_| MediaError::Storage)?;
        if !metadata.is_dir() || is_link(&metadata) {
            return Err(MediaError::Storage);
        }
        #[cfg(unix)]
        if ancestor == path && private {
            use std::os::unix::fs::MetadataExt;
            if metadata.mode() & 0o077 != 0 || metadata.uid() != unsafe { libc::geteuid() } {
                return Err(MediaError::Storage);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = private;
    Ok(())
}

fn recover_directory(root: &Path) -> Result<(), MediaError> {
    check_directory(root.parent().ok_or(MediaError::Storage)?, false)?;
    if !root.try_exists().map_err(|_| MediaError::Storage)? {
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(root).map_err(|_| MediaError::Storage)?;
    }
    check_directory(root, true)?;
    let mut paths = Vec::new();
    let mut bytes = 0u64;
    for entry in fs::read_dir(root).map_err(|_| MediaError::Storage)? {
        let entry = entry.map_err(|_| MediaError::Storage)?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(MediaError::Storage)?;
        let id = name
            .strip_prefix("clip-")
            .and_then(|name| {
                let (id, extension) = name.rsplit_once('.')?;
                matches!(extension, "mp4" | "jpg" | "png" | "webp").then_some(id)
            })
            .ok_or(MediaError::Storage)?;
        if Uuid::parse_str(id)
            .ok()
            .is_none_or(|uuid| uuid.to_string() != id)
            || paths.len() >= MAX_QUEUED + MAX_WINDOWS
        {
            return Err(MediaError::Storage);
        }
        let file = private_options()
            .open(entry.path())
            .map_err(|_| MediaError::Storage)?;
        file_identity(&file)?;
        bytes += file.metadata().map_err(|_| MediaError::Storage)?.len();
        if bytes > MAX_MEDIA_BYTES {
            return Err(MediaError::Storage);
        }
        paths.push(entry.path());
    }
    // Validate the complete bounded inventory before removing any orphan.
    for path in paths {
        fs::remove_file(path).map_err(|_| MediaError::Storage)?;
    }
    #[cfg(unix)]
    File::open(root)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| MediaError::Storage)?;
    Ok(())
}

#[cfg(test)]
#[path = "media_tests.rs"]
mod tests;
