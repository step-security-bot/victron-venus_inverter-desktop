//! Per-client cancellation and telemetry coalescing. Camera and inverter clients
//! never share emission state; queued work cannot revive a stopped session.
use super::{note_state_emit, InverterState};
use crate::notification_session::SessionAppHandle;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const EMIT_INTERVAL: Duration = Duration::from_millis(500);

pub(super) struct Shutdown(tokio::sync::watch::Sender<bool>, Mutex<()>);
impl Shutdown {
    pub(super) fn new() -> Self {
        Self(tokio::sync::watch::channel(false).0, Mutex::new(()))
    }
    pub(super) fn stop(&self) {
        let _guard = self.1.lock().unwrap_or_else(|e| e.into_inner());
        self.0.send_replace(true);
    }
    pub(super) fn is_stopped(&self) -> bool {
        *self.0.borrow()
    }
    pub(super) fn while_running(&self, action: impl FnOnce()) {
        let _guard = self.1.lock().unwrap_or_else(|e| e.into_inner());
        if !self.is_stopped() {
            action();
        }
    }
    pub(super) async fn cancelled(&self) {
        let mut receiver = self.0.subscribe();
        let _ = receiver.wait_for(|stopped| *stopped).await;
    }
}

pub(super) struct CancelOnDrop(pub(super) Arc<Shutdown>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.stop();
    }
}

struct PendingState {
    active: bool,
    last_emit: Option<Instant>,
    pending: Option<InverterState>,
    flush_scheduled: bool,
}
pub(super) struct StateEmitter(Mutex<PendingState>);
impl StateEmitter {
    pub(super) fn new(active: bool) -> Self {
        Self(Mutex::new(PendingState {
            active,
            last_emit: None,
            pending: None,
            flush_scheduled: false,
        }))
    }
    pub(super) fn stop(&self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.active = false;
        state.pending = None;
    }
    fn dispatch(&self, send: impl FnOnce()) {
        // Serialize the final delivery with stop: no trailing delivery after
        // stop returns, including already-awoken timer tasks.
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.active {
            send();
        }
    }
    fn queue(
        &self,
        snapshot: &InverterState,
        force: bool,
        now: Instant,
    ) -> (bool, Option<Duration>) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if !state.active {
            return (false, None);
        }
        let elapsed = state.last_emit.map(|last| now.duration_since(last));
        if !force && elapsed.is_some_and(|elapsed| elapsed < EMIT_INTERVAL) {
            state.pending = Some(snapshot.clone());
            if !state.flush_scheduled {
                state.flush_scheduled = true;
                return (
                    false,
                    Some(EMIT_INTERVAL.saturating_sub(elapsed.unwrap_or_default())),
                );
            }
            return (false, None);
        }
        state.last_emit = Some(now);
        state.pending = None;
        (true, None)
    }
    fn take_pending(&self) -> Option<InverterState> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.flush_scheduled = false;
        if !state.active {
            return None;
        }
        let pending = state.pending.take();
        if pending.is_some() {
            state.last_emit = Some(Instant::now());
        }
        pending
    }
    pub(super) fn emit(
        self: &Arc<Self>,
        app: &Option<SessionAppHandle>,
        snapshot: &InverterState,
        force: bool,
    ) {
        if crate::app_visibility::WINDOW_HIDDEN.load(std::sync::atomic::Ordering::Relaxed) {
            self.0.lock().unwrap_or_else(|e| e.into_inner()).pending = None;
            return;
        }
        let Some(app) = app else {
            return;
        };
        let (now, delay) = self.queue(snapshot, force, Instant::now());
        if now {
            self.dispatch(|| {
                let _ = app.emit("mqtt-state-update", snapshot);
                note_state_emit("now");
            });
        }
        if let Some(delay) = delay {
            let emitter = self.clone();
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(delay).await;
                if let Some(snapshot) = emitter.take_pending() {
                    if !crate::app_visibility::WINDOW_HIDDEN
                        .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        emitter.dispatch(|| {
                            let _ = app.emit("mqtt-state-update", snapshot);
                            note_state_emit("flush");
                        });
                    }
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stopping_camera_does_not_disable_inverter_or_deliver_old_snapshot() {
        let inverter = StateEmitter::new(true);
        let camera = StateEmitter::new(false);
        let now = Instant::now();
        let snapshot = InverterState::default();
        assert!(inverter.queue(&snapshot, false, now).0);
        assert!(inverter.queue(&snapshot, false, now).1.is_some());
        camera.stop();
        assert!(inverter.take_pending().is_some());
        inverter.queue(&snapshot, false, now + Duration::from_secs(1));
        inverter.queue(&snapshot, false, now + Duration::from_secs(1));
        inverter.stop();
        assert!(inverter.take_pending().is_none());
        inverter.dispatch(|| panic!("stopped session emitted"));
        let replacement = StateEmitter::new(true);
        assert!(replacement.queue(&snapshot, true, now).0);
        assert!(!inverter.queue(&snapshot, true, now).0);
    }
    #[test]
    fn bursts_keep_only_the_latest_snapshot_and_schedule_one_flush() {
        let emitter = StateEmitter::new(true);
        let now = Instant::now();
        let mut snapshot = InverterState::default();
        assert!(emitter.queue(&snapshot, false, now).0);
        snapshot.solar_total = Some(10.0);
        assert!(emitter.queue(&snapshot, false, now).1.is_some());
        snapshot.solar_total = Some(20.0);
        assert_eq!(emitter.queue(&snapshot, false, now), (false, None));
        assert_eq!(emitter.take_pending().unwrap().solar_total, Some(20.0));
        assert!(emitter.take_pending().is_none());
    }
    #[tokio::test]
    async fn cancellation_is_observed_even_when_stop_precedes_subscription() {
        let shutdown = Shutdown::new();
        shutdown.stop();
        tokio::time::timeout(Duration::from_secs(1), shutdown.cancelled())
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn dropping_connection_cancels_children_without_stopping_session() {
        let session = Shutdown::new();
        let connection = Arc::new(Shutdown::new());
        let guard = CancelOnDrop(connection.clone());
        drop(guard);
        tokio::time::timeout(Duration::from_secs(1), connection.cancelled())
            .await
            .unwrap();
        assert!(!session.is_stopped());
    }
}
