use super::*;
use serde_json::json;
use std::pin::Pin;
use std::sync::OnceLock;

const TEST_PLUGIN: &str = "test.fixture";

fn number_input() -> DashboardContribution {
    serde_json::from_value(json!({
        "kind":"number_input","id":"numeric-input","title":"Temperature",
        "action_id":"set-number","label":"Set temperature","unit":"°C",
        "input_revision":"input-1","value_scaled":-15,"min_scaled":-25,
        "max_scaled":25,"step_scaled":5,"decimal_places":1
    }))
    .unwrap()
}

fn number_params() -> Value {
    json!({"input_revision":"input-1","value_scaled":5})
}

fn replace_inputs(entry: &WorkerEntry, items: Vec<DashboardContribution>) {
    super::super::protocol::validate_contributions(&items).unwrap();
    let _authority = entry.authority.lock().unwrap();
    entry.replace_contributions(items, Vec::new());
}

fn card_state(id: &str, text: &str) -> DashboardContribution {
    DashboardContribution::Text {
        id: id.into(),
        title: "Entity".into(),
        text: text.into(),
    }
}

#[test]
fn entity_card_grouping_never_creates_or_changes_static_action_authority() {
    let control: DashboardContribution = serde_json::from_value(json!({
        "kind":"action","id":"control","title":"Entity","action_id":"turn-on",
        "label":"Turn on","params":{"value":true},"state_id":"state"
    }))
    .unwrap();
    let mut items = vec![card_state("state", "On"), control];
    super::super::protocol::validate_contributions(&items).unwrap();
    assert!(advertises(&items, "turn-on", &json!({"value":true})));
    assert!(!advertises(&items, "turn-on", &json!({"value":false})));
    assert!(!advertises(&items, "state", &json!({})));
    assert!(!advertises(&items, "control", &json!({"value":true})));
    if let DashboardContribution::Action { state_id, .. } = &mut items[1] {
        *state_id = None;
    }
    assert!(advertises(&items, "turn-on", &json!({"value":true})));
    items.pop();
    assert!(!advertises(&items, "turn-on", &json!({"value":true})));
}

fn fake_pipes() -> (WorkerPipes, mpsc::Receiver<Outgoing>) {
    let (writer, outgoing) = mpsc::channel(PIPE_QUEUE_CAPACITY);
    let (_incoming, frames) = mpsc::channel(1);
    let (rejection_sender, rejected) = mpsc::channel(MAX_IN_FLIGHT);
    (
        WorkerPipes {
            writer,
            frames,
            rejected,
            rejection_sender,
            tasks: vec![],
        },
        outgoing,
    )
}

#[tokio::test]
async fn numeric_actions_use_current_revision_and_exact_scaled_params_through_real_pipes() {
    let host = PluginHost::default();
    host.start(spec("numeric")).await.unwrap();
    let snapshot = ready(&host).await;
    let instance = snapshot.instance_id.unwrap();
    let epoch = host.authority_epoch();
    for params in [
        json!({}),
        json!({"input_revision":"old","value_scaled":5}),
        json!({"input_revision":"input-1","value_scaled":6}),
        json!({"input_revision":"input-1","value_scaled":30}),
        json!({"input_revision":"input-1","value_scaled":5.0}),
        json!({"input_revision":"input-1","value_scaled":5,"position":5}),
    ] {
        assert_eq!(
            host.action_in_epoch(
                TEST_PLUGIN,
                &instance,
                "set-number",
                params,
                Duration::from_secs(2),
                epoch
            )
            .await
            .unwrap_err(),
            PluginError::UnknownAction
        );
    }
    assert_eq!(action(&host, "numeric-control").await.unwrap(), 0);
    let changed = wait_for(&host, |snapshot| {
        snapshot.contributions.iter().any(|item| {
            matches!(
                item,
                DashboardContribution::NumberInput {
                    value_scaled: -10,
                    ..
                }
            )
        })
    })
    .await;
    assert_eq!(changed.instance_id.as_deref(), Some(instance.as_str()));
    let response = host
        .action_in_epoch(
            TEST_PLUGIN,
            &instance,
            "set-number",
            number_params(),
            Duration::from_secs(2),
            epoch,
        )
        .await
        .unwrap();
    assert_eq!(
        response,
        json!({"input_revision":"input-1","value_scaled":5,"writes":1})
    );
    assert_eq!(action(&host, "numeric-control").await.unwrap(), 1);
    wait_for(&host, |snapshot| snapshot.contributions.iter().any(|item| {
        matches!(item, DashboardContribution::NumberInput { input_revision, .. } if input_revision == "input-2")
    })).await;
    assert_eq!(
        host.action_in_epoch(
            TEST_PLUGIN,
            &instance,
            "set-number",
            number_params(),
            Duration::from_secs(2),
            epoch
        )
        .await
        .unwrap_err(),
        PluginError::UnknownAction
    );
    assert_eq!(action(&host, "numeric-control").await.unwrap(), 1);
    wait_for(&host, |snapshot| snapshot.contributions.len() == 1).await;
    assert_eq!(
        host.action_in_epoch(
            TEST_PLUGIN,
            &instance,
            "set-number",
            json!({"input_revision":"input-2","value_scaled":5}),
            Duration::from_secs(2),
            epoch
        )
        .await
        .unwrap_err(),
        PluginError::UnknownAction
    );
    assert_eq!(action(&host, "numeric-control").await.unwrap(), 1);
    wait_for(&host, |snapshot| snapshot.contributions.iter().any(|item| {
        matches!(item, DashboardContribution::NumberInput { input_revision, .. } if input_revision == "input-3")
    })).await;
    assert_eq!(
        host.action_in_epoch(
            TEST_PLUGIN,
            "stale-instance",
            "set-number",
            json!({"input_revision":"input-3","value_scaled":5}),
            Duration::from_secs(2),
            epoch
        )
        .await
        .unwrap_err(),
        PluginError::Unavailable
    );
    host.shutdown().await;
}

#[tokio::test]
async fn entity_card_regrouping_preserves_queued_numeric_grants_but_not_removed_controls() {
    let host = PluginHost::default();
    host.start(spec("numeric")).await.unwrap();
    ready(&host).await;
    let entry = host.entry(TEST_PLUGIN).unwrap();
    let generation = entry.snapshot().generation;
    replace_inputs(&entry, vec![number_input()]);
    let lease = entry
        .numeric
        .lock()
        .unwrap()
        .get("set-number")
        .unwrap()
        .clone();
    for (index, state_id) in [Some("first"), Some("second"), None]
        .into_iter()
        .enumerate()
    {
        let mut control = number_input();
        if let DashboardContribution::NumberInput {
            state_id: target, ..
        } = &mut control
        {
            *target = state_id.map(str::to_owned);
        }
        let items = vec![
            card_state("first", &format!("Observed {index}")),
            card_state("second", "Replacement display"),
            control,
        ];
        assert!(!advertises(&items, "first", &number_params()));
        replace_inputs(&entry, items);
        let current = entry
            .numeric
            .lock()
            .unwrap()
            .get("set-number")
            .unwrap()
            .clone();
        assert!(Arc::ptr_eq(&lease, &current));
        assert!(!*lease.revoked.borrow());
        let (pipes, mut outgoing) = fake_pipes();
        let (reply, response) = oneshot::channel();
        let (_cancel, cancellation) = watch::channel(false);
        let mut pending = HashMap::new();
        handle_control(
            Control::Action {
                request_id: format!("regroup-{index}"),
                generation,
                action_id: "set-number".into(),
                params: number_params(),
                numeric: Some(lease.clone()),
                cancellation,
                deadline: Instant::now() + Duration::from_secs(2),
                reply,
            },
            &entry,
            generation,
            true,
            &pipes,
            &mut pending,
        );
        assert_eq!(pending.len(), 1);
        assert!(
            matches!(outgoing.try_recv().unwrap(), Outgoing::Action { numeric: Some(guard), .. }
            if Arc::ptr_eq(&guard.lease, &lease))
        );
        drop(response);
    }
    replace_inputs(&entry, vec![card_state("first", "Still visible")]);
    assert!(*lease.revoked.borrow());
    assert!(!advertises(
        &entry.snapshot().contributions,
        "set-number",
        &number_params()
    ));
    host.shutdown().await;
}

#[tokio::test]
async fn queued_numeric_controls_recheck_full_grant_and_continuity_not_only_worker_revision() {
    let host = PluginHost::default();
    host.start(spec("numeric")).await.unwrap();
    ready(&host).await;
    let entry = host.entry(TEST_PLUGIN).unwrap();
    let generation = entry.snapshot().generation;
    for change in [
        "withdraw-restore",
        "constraints-restore",
        "min",
        "max",
        "step",
        "precision",
        "unit",
        "revision",
        "id",
        "value",
        "title",
        "label",
    ] {
        replace_inputs(&entry, vec![number_input()]);
        let lease = entry
            .numeric
            .lock()
            .unwrap()
            .get("set-number")
            .unwrap()
            .clone();
        let mut changed = number_input();
        if let DashboardContribution::NumberInput {
            min_scaled,
            max_scaled,
            step_scaled,
            decimal_places,
            unit,
            input_revision,
            id,
            value_scaled,
            title,
            label,
            ..
        } = &mut changed
        {
            match change {
                "min" | "constraints-restore" => *min_scaled = -30,
                "max" => *max_scaled = 30,
                "step" => *step_scaled = 10,
                "precision" => *decimal_places = 2,
                "unit" => *unit = Some("°F".into()),
                "revision" => *input_revision = "input-2".into(),
                "id" => *id = "replacement-input".into(),
                "value" => *value_scaled = -10,
                "title" => *title = "Renamed temperature".into(),
                "label" => *label = "Apply temperature".into(),
                _ => {}
            }
        }
        if change == "withdraw-restore" {
            replace_inputs(&entry, vec![]);
        }
        replace_inputs(&entry, vec![changed]);
        if change == "constraints-restore" {
            replace_inputs(&entry, vec![number_input()]);
        }
        let (pipes, mut outgoing) = fake_pipes();
        let (reply, response) = oneshot::channel();
        let (_cancel, cancellation) = watch::channel(false);
        let mut pending = HashMap::new();
        handle_control(
            Control::Action {
                request_id: change.into(),
                generation,
                action_id: "set-number".into(),
                params: number_params(),
                numeric: Some(lease.clone()),
                cancellation,
                deadline: Instant::now() + Duration::from_secs(2),
                reply,
            },
            &entry,
            generation,
            true,
            &pipes,
            &mut pending,
        );
        if matches!(change, "value" | "title" | "label") {
            assert_eq!(pending.len(), 1);
            assert!(
                matches!(outgoing.try_recv().unwrap(), Outgoing::Action { numeric: Some(guard), .. }
                if Arc::ptr_eq(&guard.lease, &lease))
            );
            drop(response);
        } else {
            assert_eq!(
                response.await.unwrap(),
                Err(PluginError::UnknownAction),
                "{change}"
            );
            assert!(pending.is_empty());
            assert!(outgoing.try_recv().is_err());
        }
    }
    // A fixed preset cannot turn into a dynamic grant while queued.
    let (pipes, mut outgoing) = fake_pipes();
    replace_inputs(&entry, vec![number_input()]);
    let (reply, response) = oneshot::channel();
    let (_cancel, cancellation) = watch::channel(false);
    handle_control(
        Control::Action {
            request_id: "old-static".into(),
            generation,
            action_id: "set-number".into(),
            params: number_params(),
            numeric: None,
            cancellation,
            deadline: Instant::now() + Duration::from_secs(2),
            reply,
        },
        &entry,
        generation,
        true,
        &pipes,
        &mut HashMap::new(),
    );
    assert_eq!(response.await.unwrap(), Err(PluginError::UnknownAction));
    assert!(outgoing.try_recv().is_err());
    host.shutdown().await;
}

#[derive(Default)]
struct WriteGate {
    state: Mutex<(bool, usize, Option<std::task::Waker>, Vec<u8>)>,
}

impl WriteGate {
    fn open(&self) {
        let waker = {
            let mut state = self.state.lock().unwrap();
            state.0 = true;
            state.2.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    async fn polled(&self) {
        time::timeout(Duration::from_secs(2), async {
            while self.state.lock().unwrap().1 == 0 {
                time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
    }
}

struct GatedWriter(Arc<WriteGate>);

impl AsyncWrite for GatedWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut state = self.0.state.lock().unwrap();
        state.1 += 1;
        if !state.0 {
            state.2 = Some(context.waker().clone());
            return Poll::Pending;
        }
        state.3.extend_from_slice(bytes);
        Poll::Ready(Ok(bytes.len()))
    }
    fn poll_flush(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn numeric_writer_rechecks_grants_after_queueing_and_after_a_pending_first_write() {
    for queued in [false, true] {
        for change in ["withdraw", "restore", "same-revision", "unchanged"] {
            let gate = Arc::new(WriteGate::default());
            let lease = Arc::new(NumericLease {
                grant: number_input().number_input_grant().unwrap(),
                revoked: watch::channel(false).0,
            });
            let registry = Arc::new(Mutex::new(HashMap::from([(
                "set-number".into(),
                lease.clone(),
            )])));
            let (rejected, mut rejections) = mpsc::channel(4);
            let (outgoing, receiver) = mpsc::channel(4);
            let (_stop, stop_receiver) = watch::channel(false);
            let (_cancel, cancellation) = watch::channel(false);
            let authority = Arc::new(Mutex::new(Authority {
                enabled: true,
                epoch: 7,
            }));
            let control = b"earlier control frame\n";
            if queued {
                outgoing
                    .send(Outgoing::Control(control.to_vec()))
                    .await
                    .unwrap();
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            outgoing
                .send(Outgoing::Action {
                    request_id: "number".into(),
                    action_id: "set-number".into(),
                    params: number_params(),
                    numeric: Some(NumericWriteGuard {
                        lease: lease.clone(),
                        registry: registry.clone(),
                        rejected,
                    }),
                    deadline,
                    cancellation,
                })
                .await
                .unwrap();
            drop(outgoing);
            let task = tokio::spawn(write_frames(
                GatedWriter(gate.clone()),
                receiver,
                stop_receiver,
                authority.clone(),
                7,
            ));
            gate.polled().await;
            if change != "unchanged" {
                let _authority = authority.lock().unwrap();
                let mut registry = registry.lock().unwrap();
                registry.remove("set-number");
                if change != "same-revision" {
                    lease.revoked.send_replace(true);
                }
                if change != "withdraw" {
                    let mut grant = lease.grant.clone();
                    if change == "same-revision" {
                        grant.min_scaled = -30;
                    }
                    registry.insert(
                        "set-number".into(),
                        Arc::new(NumericLease {
                            grant,
                            revoked: watch::channel(false).0,
                        }),
                    );
                }
            }
            // Deliberately omit a wake notification for same-revision changes:
            // the actual first poll must check the full current grant too.
            time::sleep(Duration::from_millis(25)).await;
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .as_millis() as u64;
            gate.open();
            time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let bytes = gate.state.lock().unwrap().3.clone();
            let prefix = if queued { control.len() } else { 0 };
            if queued {
                assert_eq!(&bytes[..prefix], control);
            }
            if change == "unchanged" {
                let frame: HostMessage = serde_json::from_slice(&bytes[prefix..]).unwrap();
                assert!(
                    matches!(frame, HostMessage::Action { request_id, action_id, params, deadline_ms }
                    if request_id == "number" && action_id == "set-number" && params == number_params()
                        && (1..=remaining).contains(&deadline_ms))
                );
                assert!(rejections.try_recv().is_err());
            } else {
                assert_eq!(bytes.len(), prefix, "{change}, queued={queued}");
                assert_eq!(rejections.try_recv().unwrap(), "number");
            }
        }
    }
}

#[tokio::test]
async fn partial_numeric_frames_finish_on_capability_change_but_close_on_cancel_or_deadline() {
    for change in ["capability", "cancel", "deadline"] {
        let (writer, mut reader) = tokio::io::duplex(8);
        let lease = Arc::new(NumericLease {
            grant: number_input().number_input_grant().unwrap(),
            revoked: watch::channel(false).0,
        });
        let registry = Arc::new(Mutex::new(HashMap::from([(
            "set-number".into(),
            lease.clone(),
        )])));
        let (rejected, mut rejections) = mpsc::channel(4);
        let (outgoing, receiver) = mpsc::channel(4);
        let (_stop, stop_receiver) = watch::channel(false);
        let (cancel, cancellation) = watch::channel(false);
        let authority = Arc::new(Mutex::new(Authority {
            enabled: true,
            epoch: 7,
        }));
        outgoing
            .send(Outgoing::Action {
                request_id: "partial-number".into(),
                action_id: "set-number".into(),
                params: number_params(),
                numeric: Some(NumericWriteGuard {
                    lease: lease.clone(),
                    registry: registry.clone(),
                    rejected,
                }),
                deadline: Instant::now()
                    + if change == "deadline" {
                        Duration::from_millis(500)
                    } else {
                        Duration::from_secs(2)
                    },
                cancellation,
            })
            .await
            .unwrap();
        let following = b"following frame must never splice into partial JSON\n";
        outgoing
            .send(Outgoing::Control(following.to_vec()))
            .await
            .unwrap();
        drop(outgoing);
        let task = tokio::spawn(write_frames(
            writer,
            receiver,
            stop_receiver,
            authority.clone(),
            7,
        ));
        let mut first = [0; 8];
        time::timeout(Duration::from_secs(2), reader.read_exact(&mut first))
            .await
            .unwrap()
            .unwrap();
        let mut bytes = first.to_vec();
        if change == "capability" {
            {
                let _authority = authority.lock().unwrap();
                registry.lock().unwrap().clear();
                lease.revoked.send_replace(true);
            }
            time::timeout(Duration::from_secs(2), reader.read_to_end(&mut bytes))
                .await
                .unwrap()
                .unwrap();
            task.await.unwrap().unwrap();
            assert!(bytes.ends_with(following));
            let frame: HostMessage =
                serde_json::from_slice(&bytes[..bytes.len() - following.len()]).unwrap();
            assert!(
                matches!(frame, HostMessage::Action { params, .. } if params == number_params())
            );
        } else {
            if change == "cancel" {
                cancel.send(true).unwrap();
            }
            assert_eq!(
                time::timeout(Duration::from_secs(2), task)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap_err(),
                if change == "cancel" {
                    "worker_write_cancelled"
                } else {
                    "worker_write_timeout"
                }
            );
            reader.read_to_end(&mut bytes).await.unwrap();
            assert!(!bytes.contains(&b'\n'));
            assert!(!String::from_utf8_lossy(&bytes).contains("following"));
        }
        assert!(rejections.try_recv().is_err());
    }
}

fn fixture() -> PathBuf {
    static EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();
    EXECUTABLE
        .get_or_init(|| {
            let directory = std::env::temp_dir().join(format!(
                "inverter-plugin-worker-fixture-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&directory).unwrap();
            let path = directory.join(format!("worker{}", std::env::consts::EXE_SUFFIX));
            let source =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugin_worker.rs");
            let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
            let output = std::process::Command::new(rustc)
                .arg("--edition=2021")
                .arg(source)
                .arg("-o")
                .arg(&path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "fixture compile: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            path
        })
        .clone()
}

fn spec(mode: &str) -> WorkerSpec {
    WorkerSpec {
        live_view: None,
        plugin_id: TEST_PLUGIN.into(),
        executable: fixture(),
        args: vec![mode.into()],
        configuration: None,
        desktop_notifications: false,
        http_video: None,
    }
}

fn configured_spec(mode: &str) -> WorkerSpec {
    let mut worker = spec(mode);
    worker.configuration = Some(WorkerConfiguration {
        revision: "revision-1".into(),
        values: json!({"server":"https://plugin.example"}),
        secrets: [("token".into(), "fixture-secret".into())].into(),
    });
    worker
}

fn notification_spec(mode: &str) -> WorkerSpec {
    let mut worker = if mode.starts_with("configuration") {
        configured_spec(mode)
    } else {
        spec(mode)
    };
    worker.desktop_notifications = true;
    worker
}

#[tokio::test]
async fn notifications_deliver_once_after_readiness_without_exposing_snapshot_content() {
    let host = PluginHost::default();
    assert!(!host.has_pending_notifications());
    host.start(notification_spec("notifications"))
        .await
        .unwrap();
    ready(&host).await;
    action(&host, "echo").await.unwrap();
    assert!(host.has_pending_notifications());
    let public = serde_json::to_string(&host.snapshots()).unwrap();
    assert!(!public.contains("Private camera"));
    assert!(!public.contains("motion-1"));
    let mut delivered = Vec::new();
    assert_eq!(
        host.dispatch_notifications(|item| {
            delivered.push((
                item.plugin_id.clone(),
                item.id.clone(),
                item.title.clone(),
                item.body.clone(),
            ));
        }),
        2
    );
    assert_eq!(
        delivered[0],
        (
            TEST_PLUGIN.into(),
            "motion-1".into(),
            "Private camera title".into(),
            "Private camera body".into()
        )
    );
    assert_eq!(delivered[1].1, "motion-2");
    assert!(!host.has_pending_notifications());
    action(&host, "echo").await.unwrap();
    assert_eq!(
        host.dispatch_notifications(|_| panic!("duplicate delivery")),
        0
    );
    host.shutdown().await;

    let host = PluginHost::default();
    host.start(notification_spec("configuration_notifications"))
        .await
        .unwrap();
    ready(&host).await;
    action(&host, "echo").await.unwrap();
    assert_eq!(
        host.dispatch_notifications(|item| assert_eq!(item.id, "configured-motion")),
        1
    );
    host.shutdown().await;
}

#[tokio::test]
async fn unauthorized_premature_oversized_and_frame_flood_notifications_fail_closed() {
    for (mode, permitted, expected) in [
        ("notifications", false, "worker_notification_unauthorized"),
        (
            "notifications_before_ready",
            true,
            "worker_handshake_invalid",
        ),
        (
            "configuration_early_notification",
            true,
            "worker_configuration_ack_invalid",
        ),
        ("notifications_oversize", true, "worker_frame_invalid"),
        ("notifications_flood", true, "worker_rate_limit"),
    ] {
        let host = PluginHost::default();
        let mut worker = notification_spec(mode);
        worker.desktop_notifications = permitted;
        host.start(worker).await.unwrap();
        let failed = wait_for(&host, |snapshot| snapshot.state == WorkerState::Failed).await;
        assert_eq!(failed.last_error.as_deref(), Some(expected), "{mode}");
        assert!(failed.contributions.is_empty());
        assert_eq!(
            host.dispatch_notifications(|_| panic!("failed worker notification")),
            0
        );
        host.shutdown().await;
    }
}

#[tokio::test]
async fn valid_notification_bursts_drop_queue_and_rate_excess_without_failing_worker() {
    let host = PluginHost::default();
    host.start(notification_spec("notifications_actions"))
        .await
        .unwrap();
    ready(&host).await;
    for expected in [
        NOTIFICATION_QUEUE_CAPACITY,
        NOTIFICATIONS_PER_WORKER_PER_MINUTE - NOTIFICATION_QUEUE_CAPACITY,
        0,
    ] {
        action(&host, "echo").await.unwrap();
        assert_eq!(host.dispatch_notifications(|_| {}), expected);
        assert_eq!(host.snapshots()[0].state, WorkerState::Running);
    }
    let entry = host.entry(TEST_PLUGIN).unwrap();
    assert_eq!(
        entry.notifications.lock().unwrap().seen.len(),
        NOTIFICATIONS_PER_WORKER_PER_MINUTE
    );
    // Advancing only the rate windows avoids a minute of wall-clock waiting.
    entry.notifications.lock().unwrap().rate.window = Instant::now() - Duration::from_secs(61);
    host.0.notification_rate.lock().unwrap().window = Instant::now() - Duration::from_secs(61);
    action(&host, "echo").await.unwrap();
    assert_eq!(
        host.dispatch_notifications(|_| {}),
        NOTIFICATION_QUEUE_CAPACITY
    );
    host.shutdown().await;
}

#[tokio::test]
async fn global_notification_rate_bounds_multiple_real_workers() {
    let host = PluginHost::default();
    let mut total = 0;
    for index in 0..5 {
        let mut worker = notification_spec("notifications_actions");
        worker.plugin_id = format!("test.notifications-{index}");
        let id = worker.plugin_id.clone();
        host.start(worker).await.unwrap();
        time::timeout(Duration::from_secs(5), async {
            while !host.snapshots().iter().any(|snapshot| {
                snapshot.plugin_id == id
                    && snapshot.state == WorkerState::Running
                    && !snapshot.contributions.is_empty()
            }) {
                time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        for _ in 0..2 {
            host.action(&id, "echo", json!({}), Duration::from_secs(2))
                .await
                .unwrap();
            total += host.dispatch_notifications(|_| {});
        }
    }
    assert_eq!(total, NOTIFICATIONS_GLOBAL_PER_MINUTE);
    assert!(host
        .snapshots()
        .iter()
        .all(|snapshot| snapshot.state == WorkerState::Running));
    host.shutdown().await;
}

#[tokio::test]
async fn queued_notifications_cannot_cross_stop_removal_revocation_or_restart() {
    for transition in ["stop", "remove", "revoke", "restart"] {
        let host = PluginHost::default();
        host.start(notification_spec("notifications"))
            .await
            .unwrap();
        let first = ready(&host).await;
        action(&host, "echo").await.unwrap();
        assert_eq!(
            host.entry(TEST_PLUGIN)
                .unwrap()
                .notifications
                .lock()
                .unwrap()
                .pending
                .len(),
            2
        );
        match transition {
            "stop" => host.stop(TEST_PLUGIN).await.unwrap(),
            "remove" => host.remove(TEST_PLUGIN).await.unwrap(),
            "revoke" => {
                host.revoke();
                host.resume();
            }
            "restart" => {
                assert!(action(&host, "crash").await.is_err());
                wait_for(&host, |snapshot| {
                    snapshot.generation > first.generation
                        && snapshot.state == WorkerState::Running
                        && !snapshot.contributions.is_empty()
                })
                .await;
                action(&host, "echo").await.unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(
            host.dispatch_notifications(|_| panic!("stale notification after {transition}")),
            0
        );
        host.shutdown().await;
    }
}

#[tokio::test]
async fn queued_notification_expiry_and_duplicate_retention_are_bounded() {
    let host = PluginHost::default();
    host.start(notification_spec("notifications"))
        .await
        .unwrap();
    ready(&host).await;
    action(&host, "echo").await.unwrap();
    let entry = host.entry(TEST_PLUGIN).unwrap();
    {
        let mut state = entry.notifications.lock().unwrap();
        for queued in &mut state.pending {
            queued.created = Instant::now() - NOTIFICATION_DELIVERY_TTL;
        }
    }
    assert_eq!(
        host.dispatch_notifications(|_| panic!("expired notification")),
        0
    );
    action(&host, "echo").await.unwrap();
    assert_eq!(
        host.dispatch_notifications(|_| panic!("duplicate after queue expiry")),
        0
    );
    {
        let mut state = entry.notifications.lock().unwrap();
        for (_, created) in &mut state.seen {
            *created = Instant::now() - NOTIFICATION_DEDUP_TTL;
        }
    }
    action(&host, "echo").await.unwrap();
    assert_eq!(host.dispatch_notifications(|_| {}), 2);
    host.shutdown().await;
}

#[tokio::test]
async fn configuration_is_acknowledged_before_running_or_accepting_actions() {
    let host = PluginHost::default();
    host.start(configured_spec("configuration_delayed"))
        .await
        .unwrap();
    assert_eq!(
        action(&host, "echo").await.unwrap_err(),
        PluginError::Unavailable
    );
    let snapshot = ready(&host).await;
    let reply = action(&host, "echo").await.unwrap();
    assert!(!host.has_pending_notifications());
    assert_eq!(reply["configuration_revision"], "revision-1");
    assert_eq!(reply["configuration_secret_matches"], true);
    let public = serde_json::to_string(&snapshot).unwrap();
    assert!(!public.contains("fixture-secret"));
    assert!(!public.contains("plugin.example"));
    host.shutdown().await;
}

#[tokio::test]
async fn missing_wrong_early_and_duplicate_configuration_acknowledgements_fail() {
    for mode in [
        "configuration_wrong_ack",
        "configuration_early_data",
        "configuration_duplicate_ack",
        "configuration_no_ack",
    ] {
        let host = PluginHost::default();
        host.start(configured_spec(mode)).await.unwrap();
        let failed = wait_for(&host, |snapshot| snapshot.state == WorkerState::Failed).await;
        assert!(failed.contributions.is_empty(), "{mode}");
        assert!(
            failed.last_error.as_deref().is_some_and(|error| matches!(
                error,
                "worker_configuration_ack_invalid"
                    | "worker_duplicate_handshake"
                    | "worker_startup_timeout"
            )),
            "{mode}: {:?}",
            failed.last_error
        );
        assert_eq!(
            action(&host, "echo").await.unwrap_err(),
            PluginError::Unavailable
        );
        host.shutdown().await;
    }
}

#[tokio::test]
async fn configuration_writer_rejects_revoked_epoch_and_expired_deadline_before_write() {
    for expired in [false, true] {
        let (writer, mut reader) = tokio::io::duplex(8);
        let (outgoing, receiver) = mpsc::channel(2);
        let (_stop, stop_receiver) = watch::channel(false);
        let authority = Arc::new(Mutex::new(Authority {
            enabled: true,
            epoch: u64::from(!expired),
        }));
        outgoing
            .send(Outgoing::Configuration {
                encoded: b"secret must never be written\n".to_vec(),
                deadline: if expired {
                    Instant::now() - Duration::from_millis(1)
                } else {
                    Instant::now() + Duration::from_secs(1)
                },
            })
            .await
            .unwrap();
        drop(outgoing);
        let result = write_frames(writer, receiver, stop_receiver, authority, 0).await;
        if expired {
            assert_eq!(result.unwrap_err(), "worker_configuration_timeout");
        } else {
            result.unwrap();
        }
        let mut actual = Vec::new();
        reader.read_to_end(&mut actual).await.unwrap();
        assert!(actual.is_empty());
    }
}

#[tokio::test]
async fn interrupted_configuration_frame_never_allows_a_following_frame() {
    for revoked in [false, true] {
        let (writer, mut reader) = tokio::io::duplex(8);
        let (outgoing, receiver) = mpsc::channel(2);
        let (stop, stop_receiver) = watch::channel(false);
        let authority = Arc::new(Mutex::new(Authority {
            enabled: true,
            epoch: 0,
        }));
        outgoing
            .send(Outgoing::Configuration {
                encoded: b"configuration frame longer than the pipe\n".to_vec(),
                deadline: Instant::now()
                    + if revoked {
                        Duration::from_secs(1)
                    } else {
                        Duration::from_millis(20)
                    },
            })
            .await
            .unwrap();
        outgoing
            .send(Outgoing::Control(
                b"must never follow partial configuration\n".to_vec(),
            ))
            .await
            .unwrap();
        let task = tokio::spawn(write_frames(
            writer,
            receiver,
            stop_receiver,
            authority.clone(),
            0,
        ));
        time::sleep(Duration::from_millis(40)).await;
        if revoked {
            *authority.lock().unwrap() = Authority {
                enabled: false,
                epoch: 1,
            };
            stop.send(true).unwrap();
        }
        let result = time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        if revoked {
            result.unwrap();
        } else {
            assert_eq!(result.unwrap_err(), "worker_configuration_timeout");
        }
        let mut actual = Vec::new();
        reader.read_to_end(&mut actual).await.unwrap();
        assert_eq!(actual, &b"configuration"[..8]);
    }
}

async fn wait_for(
    host: &PluginHost,
    predicate: impl Fn(&PluginSnapshot) -> bool,
) -> PluginSnapshot {
    time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(snapshot) = host.snapshots().into_iter().find(&predicate) {
                return snapshot;
            }
            time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("worker reached expected state")
}

async fn ready(host: &PluginHost) -> PluginSnapshot {
    wait_for(host, |snapshot| {
        snapshot.state == WorkerState::Running && !snapshot.contributions.is_empty()
    })
    .await
}

async fn action(host: &PluginHost, id: &str) -> Result<Value, PluginError> {
    host.action(TEST_PLUGIN, id, json!({}), Duration::from_secs(2))
        .await
}

#[tokio::test]
async fn separate_executable_handshakes_contributes_and_serves_multiple_windows() {
    let host = PluginHost::default();
    host.start(spec("normal")).await.unwrap();
    let snapshot = ready(&host).await;
    assert_eq!(snapshot.generation, 1);
    assert!(snapshot.contributions.iter().any(|item| matches!(item, DashboardContribution::Text { text, .. } if text == "Separate executable")));
    let other_window = host.clone();
    assert_eq!(
        other_window.start(spec("normal")).await.unwrap_err(),
        PluginError::AlreadyRegistered
    );
    let response = action(&other_window, "echo").await.unwrap();
    assert_eq!(response["ok"], true);
    assert_eq!(response["inherited_environment"], false);
    assert_ne!(
        response["pid"].as_u64().unwrap(),
        u64::from(std::process::id())
    );
    assert_eq!(
        action(&host, "perform_action").await.unwrap_err(),
        PluginError::UnknownAction
    );
    assert_eq!(
        host.action(
            TEST_PLUGIN,
            "echo",
            json!({"unexpected":true}),
            Duration::from_secs(1)
        )
        .await
        .unwrap_err(),
        PluginError::UnknownAction
    );
    host.shutdown().await;
    let stopped = host.snapshots().remove(0);
    assert_eq!(stopped.state, WorkerState::Stopped);
    assert!(stopped.contributions.is_empty());
    assert!(host
        .entry(TEST_PLUGIN)
        .unwrap()
        .task
        .lock()
        .unwrap()
        .is_none());
    assert_eq!(
        host.start(spec("normal")).await.unwrap_err(),
        PluginError::HostStopped
    );
}

#[tokio::test]
async fn package_start_cannot_cross_logout_and_relogin() {
    let host = PluginHost::default();
    let original_epoch = host.authority_epoch();
    host.revoke();
    host.resume();
    assert!(!host.is_authorized_epoch(original_epoch));
    let committed = AtomicBool::new(false);
    assert!(host
        .commit_in_epoch(original_epoch, || {
            committed.store(true, Ordering::Release);
            Ok(())
        })
        .is_err());
    assert!(!committed.load(Ordering::Acquire));
    assert_eq!(
        host.start_in_epoch(spec("normal"), original_epoch)
            .await
            .unwrap_err(),
        PluginError::Unavailable
    );
    assert!(host.snapshots().is_empty());
    let current_epoch = host.authority_epoch();
    assert!(host.is_authorized_epoch(current_epoch));
    host.start_in_epoch(spec("normal"), current_epoch)
        .await
        .unwrap();
    ready(&host).await;
    host.shutdown().await;
    assert!(!host.is_authorized_epoch(current_epoch));
}

#[tokio::test]
async fn uninstall_reaps_workers_and_releases_registry_capacity() {
    let host = PluginHost::default();
    for index in 0..=MAX_WORKERS {
        let mut worker = spec("normal");
        worker.plugin_id = format!("test.package{index}");
        let id = worker.plugin_id.clone();
        host.start(worker).await.unwrap();
        ready(&host).await;
        let entry = host.entry(&id).unwrap();
        host.remove(&id).await.unwrap();
        assert!(entry.reaped.load(Ordering::Acquire));
        assert!(*entry.done.borrow());
        assert!(entry.task.lock().unwrap().is_none());
        assert!(host.snapshots().is_empty());
    }
    host.shutdown().await;
}

#[tokio::test]
async fn deadlines_and_dropped_callers_cancel_and_ignore_late_results() {
    let host = PluginHost::default();
    host.start(spec("normal")).await.unwrap();
    ready(&host).await;
    let timed_out = host
        .action(TEST_PLUGIN, "hold", json!({}), Duration::from_millis(40))
        .await;
    assert_eq!(timed_out.unwrap_err(), PluginError::DeadlineExceeded);
    time::sleep(Duration::from_millis(40)).await;
    assert_eq!(action(&host, "cancel_count").await.unwrap(), 1);
    let caller = host.clone();
    let task = tokio::spawn(async move { action(&caller, "hold").await });
    time::sleep(Duration::from_millis(40)).await;
    task.abort();
    let _ = task.await;
    time::sleep(Duration::from_millis(40)).await;
    assert_eq!(action(&host, "cancel_count").await.unwrap(), 2);
    assert_eq!(action(&host, "echo").await.unwrap()["ok"], true);
    host.shutdown().await;
}

#[tokio::test]
async fn crash_recovers_in_a_fresh_generation_and_stops_after_finite_retries() {
    let host = PluginHost::default();
    host.start(spec("normal")).await.unwrap();
    ready(&host).await;
    for expected_generation in 2..=3 {
        let result = action(&host, "crash").await;
        assert_eq!(result.unwrap_err(), PluginError::Unavailable);
        let recovered = wait_for(&host, |snapshot| {
            snapshot.state == WorkerState::Running
                && snapshot.generation == expected_generation
                && !snapshot.contributions.is_empty()
        })
        .await;
        assert_eq!(recovered.restart_count, (expected_generation - 1) as u32);
        assert_eq!(action(&host, "echo").await.unwrap()["ok"], true);
    }
    assert_eq!(
        action(&host, "crash").await.unwrap_err(),
        PluginError::Unavailable
    );
    let failed = wait_for(&host, |snapshot| snapshot.state == WorkerState::Failed).await;
    assert_eq!(failed.generation, 3);
    assert_eq!(failed.restart_count, MAX_RESTARTS);
    assert!(failed.contributions.is_empty());
    host.shutdown().await;
}

#[tokio::test]
async fn rejects_bad_identity_version_oversized_frames_and_event_floods() {
    for (mode, expected) in [
        ("bad_identity", "worker_handshake_invalid"),
        ("bad_version", "worker_frame_invalid"),
        ("oversize", "worker_frame_too_large"),
        ("flood", "worker_rate_limit"),
    ] {
        let host = PluginHost::default();
        host.start(spec(mode)).await.unwrap();
        let failed = wait_for(&host, |snapshot| snapshot.state == WorkerState::Failed).await;
        assert_eq!(failed.last_error.as_deref(), Some(expected), "{mode}");
        assert!(failed.contributions.is_empty());
        assert_eq!(failed.generation, 1, "protocol failures are not restarted");
        host.shutdown().await;
    }
}

#[tokio::test]
async fn drains_stderr_and_forces_a_stalled_child_to_stop_and_reap() {
    let host = PluginHost::default();
    host.start(spec("stderr_flood")).await.unwrap();
    ready(&host).await;
    assert_eq!(action(&host, "echo").await.unwrap()["ok"], true);
    host.stop(TEST_PLUGIN).await.unwrap();
    host.start(spec("stalled")).await.unwrap();
    ready(&host).await;
    let started = Instant::now();
    time::timeout(Duration::from_secs(3), host.stop(TEST_PLUGIN))
        .await
        .unwrap()
        .unwrap();
    assert!(started.elapsed() >= STOP_GRACE);
    assert!(*host.entry(TEST_PLUGIN).unwrap().done.borrow());
    assert!(host
        .entry(TEST_PLUGIN)
        .unwrap()
        .task
        .lock()
        .unwrap()
        .is_none());
    host.shutdown().await;
}

#[tokio::test]
async fn revocation_clears_immediately_and_new_login_cannot_revive_old_generation() {
    let host = PluginHost::default();
    host.start(spec("normal")).await.unwrap();
    ready(&host).await;
    let caller = host.clone();
    let pending = tokio::spawn(async move { action(&caller, "hold").await });
    time::sleep(Duration::from_millis(20)).await;
    host.revoke();
    assert!(host.snapshots()[0].contributions.is_empty());
    host.resume();
    assert_eq!(
        action(&host, "echo").await.unwrap_err(),
        PluginError::Unavailable
    );
    assert_eq!(
        pending.await.unwrap().unwrap_err(),
        PluginError::Unavailable
    );
    host.stop(TEST_PLUGIN).await.unwrap();
    host.start(spec("normal")).await.unwrap();
    ready(&host).await;
    assert_eq!(action(&host, "echo").await.unwrap()["ok"], true);
    host.shutdown().await;
}

#[tokio::test]
async fn in_flight_capacity_rejects_excess_work_without_breaking_core_host() {
    let host = PluginHost::default();
    host.start(spec("normal")).await.unwrap();
    ready(&host).await;
    let mut requests = Vec::new();
    for _ in 0..MAX_IN_FLIGHT {
        let caller = host.clone();
        requests.push(tokio::spawn(async move { action(&caller, "hold").await }));
    }
    time::sleep(Duration::from_millis(50)).await;
    assert_eq!(action(&host, "hold").await.unwrap_err(), PluginError::Busy);
    for request in requests {
        request.abort();
        let _ = request.await;
    }
    time::sleep(Duration::from_millis(50)).await;
    assert_eq!(action(&host, "echo").await.unwrap()["ok"], true);
    host.shutdown().await;
}

#[tokio::test]
async fn startup_timeout_and_concurrent_stop_are_bounded() {
    let host = PluginHost::default();
    host.start(spec("no_hello")).await.unwrap();
    let failed = wait_for(&host, |snapshot| snapshot.state == WorkerState::Failed).await;
    assert_eq!(failed.last_error.as_deref(), Some("worker_startup_timeout"));
    host.stop(TEST_PLUGIN).await.unwrap();
    host.start(spec("stalled")).await.unwrap();
    ready(&host).await;
    let second = host.clone();
    let (first_result, second_result) =
        tokio::join!(host.stop(TEST_PLUGIN), second.stop(TEST_PLUGIN));
    first_result.unwrap();
    second_result.unwrap();
    assert!(*host.entry(TEST_PLUGIN).unwrap().done.borrow());
    host.shutdown().await;
}

#[tokio::test]
async fn dropping_last_host_signals_a_running_worker_to_stop() {
    let host = PluginHost::default();
    host.start(spec("stalled")).await.unwrap();
    ready(&host).await;
    let entry = host.entry(TEST_PLUGIN).unwrap();
    drop(host);
    time::timeout(Duration::from_secs(3), wait_stopped(&entry))
        .await
        .unwrap();
    assert_eq!(entry.snapshot().state, WorkerState::Stopped);
}

#[tokio::test]
async fn queued_writes_recheck_deadline_cancellation_and_revoked_epoch() {
    for invalidation in ["deadline", "cancel", "revoke"] {
        // Eight bytes deterministically back-pressure the actual production writer,
        // independently of the operating system's subprocess pipe capacity.
        let (writer, mut reader) = tokio::io::duplex(8);
        let (outgoing, receiver) = mpsc::channel(4);
        let (stop, stop_receiver) = watch::channel(false);
        let authority = Arc::new(Mutex::new(Authority {
            enabled: true,
            epoch: 7,
        }));
        let (cancel, cancellation) = watch::channel(false);
        let first_frame = b"first control frame is longer than eight bytes\n".to_vec();
        outgoing
            .send(Outgoing::Control(first_frame.clone()))
            .await
            .unwrap();
        outgoing
            .send(Outgoing::Action {
                request_id: "expired-request".into(),
                numeric: None,
                action_id: "echo".into(),
                params: json!({"marker":"must never be written"}),
                deadline: Instant::now()
                    + if invalidation == "deadline" {
                        Duration::from_millis(20)
                    } else {
                        Duration::from_secs(2)
                    },
                cancellation,
            })
            .await
            .unwrap();
        let task = tokio::spawn(write_frames(
            writer,
            receiver,
            stop_receiver,
            authority.clone(),
            7,
        ));
        time::sleep(Duration::from_millis(40)).await;
        match invalidation {
            "cancel" => {
                cancel.send(true).unwrap();
            }
            "revoke" => {
                *authority.lock().unwrap() = Authority {
                    enabled: false,
                    epoch: 8,
                };
                stop.send(true).unwrap();
            }
            _ => {}
        }
        drop(outgoing);
        let mut actual = Vec::new();
        time::timeout(Duration::from_secs(2), reader.read_to_end(&mut actual))
            .await
            .unwrap()
            .unwrap();
        task.await.unwrap().unwrap();
        assert!(
            !String::from_utf8_lossy(&actual).contains("must never"),
            "{invalidation}"
        );
        if invalidation != "revoke" {
            assert_eq!(actual, first_frame);
        }
    }
}

#[tokio::test]
async fn a_queued_action_forwards_only_its_original_remaining_budget() {
    let (writer, mut reader) = tokio::io::duplex(8);
    let (outgoing, receiver) = mpsc::channel(4);
    let (_stop, stop_receiver) = watch::channel(false);
    let authority = Arc::new(Mutex::new(Authority {
        enabled: true,
        epoch: 7,
    }));
    let (_cancel, cancellation) = watch::channel(false);
    let first_frame = b"control frame blocks the action until the reader drains\n".to_vec();
    outgoing
        .send(Outgoing::Control(first_frame.clone()))
        .await
        .unwrap();
    let params =
        json!({"nested":{"literal":"button.fixed","values":[1,true,null]},"text":"unchanged"});
    let original_budget = Duration::from_secs(2);
    let deadline = Instant::now() + original_budget;
    outgoing
        .send(Outgoing::Action {
            request_id: "queued-request".into(),
            numeric: None,
            action_id: "fixed-action".into(),
            params: params.clone(),
            deadline,
            cancellation,
        })
        .await
        .unwrap();
    drop(outgoing);
    let task = tokio::spawn(write_frames(writer, receiver, stop_receiver, authority, 7));
    time::sleep(Duration::from_millis(100)).await;
    let remaining_at_release = deadline
        .saturating_duration_since(Instant::now())
        .as_millis() as u64;
    let mut actual = Vec::new();
    time::timeout(Duration::from_secs(2), reader.read_to_end(&mut actual))
        .await
        .unwrap()
        .unwrap();
    task.await.unwrap().unwrap();
    assert!(actual.starts_with(&first_frame));
    let action: HostMessage = serde_json::from_slice(&actual[first_frame.len()..]).unwrap();
    match action {
        HostMessage::Action {
            request_id,
            action_id,
            params: actual_params,
            deadline_ms,
        } => {
            assert_eq!(request_id, "queued-request");
            assert_eq!(action_id, "fixed-action");
            assert_eq!(actual_params, params);
            assert!((1..=remaining_at_release).contains(&deadline_ms));
            assert!(deadline_ms < original_budget.as_millis() as u64);
        }
        _ => panic!("expected the queued action"),
    }
}

#[tokio::test]
async fn an_action_with_less_than_one_millisecond_remaining_is_not_written() {
    let (writer, mut reader) = tokio::io::duplex(1024);
    let (outgoing, receiver) = mpsc::channel(1);
    let (_stop, stop_receiver) = watch::channel(false);
    let (_cancel, cancellation) = watch::channel(false);
    let authority = Arc::new(Mutex::new(Authority {
        enabled: true,
        epoch: 7,
    }));
    outgoing
        .send(Outgoing::Action {
            request_id: "submillisecond-request".into(),
            numeric: None,
            action_id: "echo".into(),
            params: json!({}),
            deadline: Instant::now() + Duration::from_micros(500),
            cancellation,
        })
        .await
        .unwrap();
    drop(outgoing);
    write_frames(writer, receiver, stop_receiver, authority, 7)
        .await
        .unwrap();
    let mut actual = Vec::new();
    reader.read_to_end(&mut actual).await.unwrap();
    assert!(actual.is_empty());
}

#[tokio::test]
async fn interruption_mid_action_frame_closes_writer_instead_of_writing_next_frame() {
    for invalidation in ["deadline", "cancel"] {
        let (writer, mut reader) = tokio::io::duplex(8);
        let (outgoing, receiver) = mpsc::channel(4);
        let (_stop, stop_receiver) = watch::channel(false);
        let authority = Arc::new(Mutex::new(Authority {
            enabled: true,
            epoch: 0,
        }));
        let (cancel, cancellation) = watch::channel(false);
        outgoing
            .send(Outgoing::Action {
                request_id: "partial-request".into(),
                numeric: None,
                action_id: "echo".into(),
                params: json!({"marker":"first action cannot complete while reader is stalled"}),
                deadline: Instant::now()
                    + if invalidation == "deadline" {
                        Duration::from_millis(20)
                    } else {
                        Duration::from_secs(2)
                    },
                cancellation,
            })
            .await
            .unwrap();
        outgoing
            .send(Outgoing::Control(
                b"second frame must not follow partial JSON\n".to_vec(),
            ))
            .await
            .unwrap();
        let task = tokio::spawn(write_frames(writer, receiver, stop_receiver, authority, 0));
        time::sleep(Duration::from_millis(40)).await;
        if invalidation == "cancel" {
            cancel.send(true).unwrap();
        }
        assert_eq!(
            time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err(),
            if invalidation == "cancel" {
                "worker_write_cancelled"
            } else {
                "worker_write_timeout"
            }
        );
        let mut actual = Vec::new();
        reader.read_to_end(&mut actual).await.unwrap();
        assert_eq!(actual, b"{\"type\":");
    }
}

#[tokio::test]
async fn worker_count_is_bounded_and_failed_registrations_do_not_spawn() {
    let host = PluginHost::default();
    for index in 0..MAX_WORKERS {
        let mut worker = spec("normal");
        worker.plugin_id = format!("test.worker-{index}");
        host.start(worker).await.unwrap();
    }
    let mut excess = spec("normal");
    excess.plugin_id = "test.excess".into();
    assert_eq!(host.start(excess).await.unwrap_err(), PluginError::Capacity);
    let mut relative = spec("normal");
    relative.executable = "relative-worker".into();
    assert_eq!(
        host.start(relative).await.unwrap_err(),
        PluginError::InvalidWorker
    );
    assert_eq!(host.snapshots().len(), MAX_WORKERS);
    host.shutdown().await;
    assert!(host
        .snapshots()
        .iter()
        .all(|entry| entry.state == WorkerState::Stopped && entry.contributions.is_empty()));
}

#[tokio::test]
async fn a_buffered_reply_cannot_cross_logout_and_a_new_login() {
    use std::future::Future;
    use std::task::Poll;

    let host = PluginHost::default();
    host.start(spec("normal")).await.unwrap();
    ready(&host).await;
    let mut request = Box::pin(action(&host, "echo"));
    // Enqueue once, then leave the caller unpolled while the real child replies.
    std::future::poll_fn(|context| {
        assert!(request.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
    time::sleep(Duration::from_millis(50)).await;
    let old_epoch = host.authority_epoch();
    host.revoke();
    host.resume();
    assert_ne!(host.authority_epoch(), old_epoch);
    assert_eq!(request.await.unwrap_err(), PluginError::Unavailable);
    host.wait_revoked().await;
    host.start(spec("normal")).await.unwrap();
    ready(&host).await;
    assert_eq!(action(&host, "echo").await.unwrap()["ok"], true);
    host.shutdown().await;
}

#[test]
fn an_older_concurrent_resume_cannot_undo_a_newer_revocation() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::Barrier;

    let first_revoked = Arc::new(Barrier::new(2));
    let continue_first = Arc::new(Barrier::new(2));
    let changed = Arc::new(AtomicUsize::new(0));
    let host = PluginHost::new(Arc::new({
        let first_revoked = first_revoked.clone();
        let continue_first = continue_first.clone();
        move || {
            if changed.fetch_add(1, Ordering::AcqRel) == 0 {
                // Pause after the authority lock is released but before the
                // first revoke_epoch returns to its unlock continuation.
                first_revoked.wait();
                continue_first.wait();
            }
        }
    }));
    let older_host = host.clone();
    let older = std::thread::spawn(move || {
        let epoch = older_host.revoke_epoch();
        (epoch, older_host.resume_in_epoch(epoch))
    });
    first_revoked.wait();
    let newer_epoch = host.revoke_epoch();
    continue_first.wait();
    let (older_epoch, resumed) = older.join().unwrap();
    assert_ne!(older_epoch, newer_epoch);
    assert!(!resumed);
    assert_eq!(host.authority_epoch(), newer_epoch);
    assert!(!host.is_authorized_epoch(newer_epoch));

    assert!(host.resume_in_epoch(newer_epoch));
    assert!(!host.resume_in_epoch(older_epoch));
    assert!(host.is_authorized_epoch(newer_epoch));
}

#[tokio::test]
async fn an_old_session_action_cannot_reach_a_replacement_worker() {
    let host = PluginHost::default();
    host.start(spec("normal")).await.unwrap();
    ready(&host).await;
    let old_epoch = host.authority_epoch();
    let old_instance = host.snapshots()[0].instance_id.clone().unwrap();
    // Keep this caller unpolled until logout and a new login have replaced the
    // worker. If dispatched, this advertised action would terminate that child.
    let old_request = host.action_in_epoch(
        TEST_PLUGIN,
        &old_instance,
        "crash",
        json!({}),
        Duration::from_secs(2),
        old_epoch,
    );
    let new_epoch = host.revoke_epoch();
    assert!(host.resume_in_epoch(new_epoch));
    host.wait_revoked().await;
    host.start_in_epoch(spec("normal"), new_epoch)
        .await
        .unwrap();
    let replacement = ready(&host).await;

    assert_eq!(old_request.await.unwrap_err(), PluginError::Unavailable);
    assert_eq!(
        host.action_in_epoch(
            TEST_PLUGIN,
            replacement.instance_id.as_deref().unwrap(),
            "echo",
            json!({}),
            Duration::from_secs(2),
            new_epoch,
        )
        .await
        .unwrap()["ok"],
        true
    );
    let after = host.snapshots().pop().unwrap();
    assert_eq!(after.generation, replacement.generation);
    assert_eq!(after.restart_count, replacement.restart_count);
    assert_eq!(after.state, WorkerState::Running);
    host.shutdown().await;
}

#[tokio::test]
async fn a_displayed_action_cannot_target_a_reinstalled_worker_with_reused_counters() {
    let host = PluginHost::default();
    host.start(spec("normal")).await.unwrap();
    let first = ready(&host).await;
    let epoch = host.authority_epoch();
    let first_instance = first.instance_id.as_deref().unwrap();
    // Delay polling until reinstall. Both workers advertise the same empty
    // preset, so parameter equality and the reused counter cannot reject it.
    let stale_click = host.action_in_epoch(
        TEST_PLUGIN,
        first_instance,
        "crash",
        json!({}),
        Duration::from_secs(2),
        epoch,
    );
    host.remove(TEST_PLUGIN).await.unwrap();
    host.start(spec("normal")).await.unwrap();
    let replacement = ready(&host).await;
    assert_eq!(replacement.generation, first.generation);
    assert_eq!(host.authority_epoch(), epoch);
    assert_ne!(replacement.instance_id, first.instance_id);
    assert_eq!(stale_click.await.unwrap_err(), PluginError::Unavailable);
    assert_eq!(
        host.action_in_epoch(
            TEST_PLUGIN,
            replacement.instance_id.as_deref().unwrap(),
            "echo",
            json!({}),
            Duration::from_secs(2),
            epoch,
        )
        .await
        .unwrap()["ok"],
        true
    );
    let after = host.snapshots().pop().unwrap();
    assert_eq!(after.instance_id, replacement.instance_id);
    assert_eq!(after.generation, replacement.generation);
    assert_eq!(after.restart_count, 0);
    assert_eq!(after.state, WorkerState::Running);
    host.shutdown().await;
    assert!(host
        .snapshots()
        .iter()
        .all(|entry| entry.instance_id.is_none()));
}

#[tokio::test]
async fn an_epoch_bound_resume_cannot_revive_a_shutdown_host() {
    let host = PluginHost::default();
    let epoch = host.revoke_epoch();
    assert!(host.resume_in_epoch(epoch));
    host.shutdown().await;
    let stopped_epoch = host.authority_epoch();
    assert!(!host.resume_in_epoch(stopped_epoch));
    assert!(!host.resume_in_epoch(epoch));
    assert!(!host.is_authorized_epoch(stopped_epoch));
}

fn video_spec(mode: &str) -> WorkerSpec {
    let mut worker = configured_spec(mode);
    worker.configuration.as_mut().unwrap().values = json!({"server":"https://video.test/base/"});
    let manifest = serde_json::from_value(json!({
        "schema_version":1,"plugin_id":TEST_PLUGIN,"version":"1.0.0","host_api":"^1.3",
        "target":"aarch64-apple-darwin","entrypoint":"worker","inventory":[],"signature":null,
        "config_schema":{"type":"object","properties":{"server":{"type":"string"}}},
        "permissions":["plugin_configuration","http_video"],"http_video":{"base_url_setting":"server"}
    })).unwrap();
    worker.http_video =
        HttpVideoGrant::from_manifest_configuration(&manifest, worker.configuration.as_ref())
            .unwrap();
    worker
}

#[tokio::test]
async fn pending_media_preempts_notification_batches_without_consuming_delivery() {
    let host = PluginHost::default();
    let media_worker = video_spec("configuration");
    let grant = media_worker.http_video.clone().unwrap();
    host.start(media_worker).await.unwrap();
    ready(&host).await;
    let media_entry = host.entry(TEST_PLUGIN).unwrap();

    let mut notifier = notification_spec("notifications");
    notifier.plugin_id = "test.notifier".into();
    host.start(notifier).await.unwrap();
    time::timeout(Duration::from_secs(5), async {
        while !host.snapshots().iter().any(|snapshot| {
            snapshot.plugin_id == "test.notifier"
                && snapshot.state == WorkerState::Running
                && !snapshot.contributions.is_empty()
        }) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    host.action("test.notifier", "echo", json!({}), Duration::from_secs(2))
        .await
        .unwrap();

    let enqueue = |id: &str| {
        media_entry.queue_http_video(
            &grant,
            id.into(),
            "https://video.test/base/fixture.mp4".into(),
            "Camera".into(),
            MediaAdmission {
                kind: HttpMediaKind::Video,
                cooldown_id: Some(id.into()),
                live_preview: false,
            },
        )
    };
    {
        let _authority = host.0.authority.lock().unwrap();
        assert!(enqueue("before-delivery"));
    }
    assert_eq!(
        host.dispatch_notifications(|_| panic!("media must be admitted first")),
        0
    );
    assert!(host.has_pending_notifications());
    assert_eq!(host.take_http_video_requests().len(), 1);

    let mut delivered = Vec::new();
    assert_eq!(
        host.dispatch_notifications(|message| {
            delivered.push(message.id.clone());
            // Deterministically put media from the other running worker at the
            // submission boundary, under the callback's existing authority guard.
            assert!(enqueue("between-deliveries"));
        }),
        1
    );
    assert!(
        host.has_pending_notifications(),
        "yielding must retain the next notification"
    );
    let requests = host.take_http_video_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].id, "between-deliveries");
    assert_eq!(
        host.dispatch_notifications(|message| delivered.push(message.id.clone())),
        1
    );
    assert_eq!(delivered, ["motion-1", "motion-2"]);
    assert!(!host.has_pending_notifications());
    host.shutdown().await;
    assert!(!requests[0].lease.is_active());
}

#[tokio::test]
async fn http_video_admission_is_private_deduplicated_and_notification_permission_scoped() {
    let host = PluginHost::default();
    let mut worker = video_spec("configuration_video");
    worker.desktop_notifications = true;
    host.start(worker).await.unwrap();
    ready(&host).await;
    action(&host, "echo").await.unwrap();
    let snapshot = serde_json::to_string(&host.snapshots()).unwrap();
    assert!(!snapshot.contains("video.test") && !snapshot.contains("Private camera"));
    let requests = host.take_http_video_requests();
    assert_eq!(
        requests.len(),
        1,
        "same ID and same title are independently suppressed"
    );
    assert_eq!(requests[0].id, "clip-1");
    assert_eq!(requests[0].lease.plugin_id(), TEST_PLUGIN);
    assert_eq!(requests[0].lease.epoch(), host.authority_epoch());
    let mut notifications = Vec::new();
    host.dispatch_notifications(|item| notifications.push(item.body.clone()));
    assert_eq!(notifications, ["Camera motion clip available"]);
    host.shutdown().await;
    assert!(!requests[0].lease.is_active());
}

#[tokio::test]
async fn http_video_rejects_missing_permission_wrong_origin_and_preconfiguration_requests() {
    for (mode, permission) in [
        ("configuration_video", false),
        ("configuration_video_bad_url", true),
        ("configuration_early_video", true),
        ("configuration_video_image", false),
        ("configuration_video_image_bad_url", true),
    ] {
        let host = PluginHost::default();
        let worker = if permission {
            video_spec(mode)
        } else {
            configured_spec(mode)
        };
        host.start(worker).await.unwrap();
        wait_for(&host, |snapshot| snapshot.state == WorkerState::Failed).await;
        assert!(host.take_http_video_requests().is_empty());
        assert!(!host.has_pending_notifications());
        host.shutdown().await;
    }
}

#[tokio::test]
async fn typed_snapshot_keeps_native_scope_privacy_and_generation_revocation() {
    let host = PluginHost::default();
    let mut worker = video_spec("configuration_video_image");
    worker.desktop_notifications = true;
    host.start(worker).await.unwrap();
    ready(&host).await;
    action(&host, "echo").await.unwrap();
    let requests = host.take_http_video_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].media_kind, HttpMediaKind::Jpeg);
    assert_eq!(requests[0].lease.plugin_id(), TEST_PLUGIN);
    assert!(requests[0].lease.is_active());
    let snapshots = serde_json::to_string(&host.snapshots()).unwrap();
    assert!(!snapshots.contains("video.test") && !snapshots.contains("Private snapshot"));
    let mut notifications = Vec::new();
    host.dispatch_notifications(|item| notifications.push(item.body.clone()));
    assert_eq!(notifications, ["Camera snapshot available"]);
    host.shutdown().await;
    assert!(!requests[0].lease.is_active());
}

#[tokio::test]
async fn http_video_queue_is_bounded_and_old_leases_cannot_survive_same_epoch_reregistration() {
    let host = PluginHost::default();
    host.start(video_spec("configuration_video_burst"))
        .await
        .unwrap();
    ready(&host).await;
    action(&host, "echo").await.unwrap();
    let requests = host.take_http_video_requests();
    assert_eq!(requests.len(), HTTP_VIDEO_QUEUE_CAPACITY);
    assert!(
        !host.has_pending_notifications(),
        "HttpVideo alone does not grant notifications"
    );
    let old = requests[0].lease.clone();
    host.remove(TEST_PLUGIN).await.unwrap();
    assert!(!old.is_active());
    host.start(video_spec("configuration_video")).await.unwrap();
    ready(&host).await;
    action(&host, "echo").await.unwrap();
    let fresh = host.take_http_video_requests().pop().unwrap().lease;
    assert_eq!(old.epoch(), fresh.epoch());
    assert_eq!(old.generation(), fresh.generation());
    assert_ne!(old.instance_id(), fresh.instance_id());
    assert!(old
        .commit_if_active(|| panic!("old instance revived"))
        .is_err());
    host.revoke();
    host.resume();
    assert!(!fresh.is_active());
    assert!(host.take_http_video_requests().is_empty());
    host.shutdown().await;
}

#[tokio::test]
async fn http_video_crash_revokes_before_restart_and_supervisor_drop_revokes_current_lease() {
    let host = PluginHost::default();
    host.start(video_spec("configuration_video")).await.unwrap();
    ready(&host).await;
    action(&host, "echo").await.unwrap();
    let old = host.take_http_video_requests().pop().unwrap().lease;
    let _ = action(&host, "crash").await;
    wait_for(&host, |snapshot| {
        snapshot.generation > old.generation() && snapshot.state == WorkerState::Running
    })
    .await;
    assert!(!old.is_active());
    let entry = host.entry(TEST_PLUGIN).unwrap();
    let fresh = entry.generation_lease.lock().unwrap().clone().unwrap();
    assert_ne!(old.instance_id(), fresh.instance_id());
    entry.task.lock().unwrap().as_ref().unwrap().abort();
    time::timeout(Duration::from_secs(2), fresh.cancelled())
        .await
        .unwrap();
    assert!(!fresh.is_active());
    host.shutdown().await;
}

#[tokio::test]
async fn generation_is_revoked_if_the_first_spawn_callback_panics() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = calls.clone();
    let host = PluginHost::new(Arc::new(move || {
        if observed.fetch_add(1, Ordering::SeqCst) == 1 {
            panic!("fixture spawn callback panic");
        }
    }));
    host.start(video_spec("configuration_video")).await.unwrap();
    let entry = host.entry(TEST_PLUGIN).unwrap();
    let lease = time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(lease) = entry.generation_lease.lock().unwrap().clone() {
                break lease;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    time::timeout(Duration::from_secs(2), lease.cancelled())
        .await
        .unwrap();
    assert!(!lease.is_active());
    host.shutdown().await;
}

#[tokio::test]
async fn explicit_media_cooldown_ids_separate_equal_camera_titles_and_keep_global_bounds() {
    let host = PluginHost::default();
    let worker = video_spec("configuration");
    let grant = worker.http_video.clone().unwrap();
    host.start(worker).await.unwrap();
    ready(&host).await;
    let entry = host.entry(TEST_PLUGIN).unwrap();
    {
        let _authority = entry.authority.lock().unwrap();
        let enqueue = |id: &str, bucket: &str| {
            entry.queue_http_video(
                &grant,
                id.into(),
                "https://video.test/base/snapshot".into(),
                "Front door".into(),
                MediaAdmission {
                    kind: HttpMediaKind::Png,
                    cooldown_id: Some(bucket.into()),
                    live_preview: false,
                },
            )
        };
        assert!(enqueue("motion-1", "front-motion"));
        assert!(enqueue("ding-1", "front-ding"));
        assert!(!enqueue("motion-2", "front-motion"));
        assert!(
            !enqueue("motion-1", "new-bucket"),
            "a different bucket cannot replay the same request"
        );
        assert_eq!(entry.http_videos.lock().unwrap().rate.count, 2);
    }
    assert_eq!(host.take_http_video_requests().len(), 2);
    host.shutdown().await;
}

fn live_spec(mode: &str) -> WorkerSpec {
    let mut worker = video_spec(mode);
    let manifest = serde_json::from_value(json!({
        "schema_version":1,"plugin_id":TEST_PLUGIN,"version":"1.0.0","host_api":"^1.8",
        "target":"aarch64-apple-darwin","entrypoint":"worker","inventory":[],"signature":null,
        "config_schema":{"type":"object","properties":{"server":{"type":"string"}}},
        "permissions":["plugin_configuration","http_video"],
        "http_video":{"base_url_setting":"server","live_preview":{"query":"fps=2&height=360","max_duration_seconds":15}}
    })).unwrap();
    worker.http_video =
        HttpVideoGrant::from_manifest_configuration(&manifest, worker.configuration.as_ref())
            .unwrap();
    worker
}

#[tokio::test]
async fn http_live_admission_is_private_revocable_and_never_overlays_a_notification() {
    let host = PluginHost::default();
    let mut worker = live_spec("configuration_live");
    worker.desktop_notifications = true;
    host.start(worker).await.unwrap();
    ready(&host).await;
    action(&host, "echo").await.unwrap();
    let mut requests = host.take_http_video_requests();
    assert_eq!(requests.len(), 1);
    let request = requests.pop().unwrap();
    assert!(request.live_preview);
    assert_eq!(request.media_kind, HttpMediaKind::Video);
    assert_eq!(request.id, "live-1");
    assert!(matches!(
        request.camera_id,
        Some(MediaCameraId::HttpPreview(ref source)) if source == "https://video.test/base/api/front"
    ));
    assert!(!serde_json::to_string(&host.snapshots())
        .unwrap()
        .contains("video.test"));
    assert!(!host.has_pending_notifications());
    assert_eq!(
        host.dispatch_notifications(|_| panic!("preview must not emit a duplicate toast")),
        0
    );
    host.revoke();
    assert!(!request.lease.is_active());
    host.shutdown().await;
}

#[tokio::test]
async fn http_live_camera_admission_uses_stable_paths_and_keeps_pending_ownership() {
    let host = PluginHost::default();
    let worker = live_spec("configuration");
    let grant = worker.http_video.clone().unwrap();
    host.start(worker).await.unwrap();
    ready(&host).await;
    let entry = host.entry(TEST_PLUGIN).unwrap();
    let enqueue = |id: &str, camera: &str, title: &str| {
        entry.queue_http_video(
            &grant,
            id.into(),
            format!("https://video.test/base/api/{camera}?fps=2&height=360"),
            title.into(),
            MediaAdmission {
                kind: HttpMediaKind::Video,
                cooldown_id: None,
                live_preview: true,
            },
        )
    };
    {
        let _authority = entry.authority.lock().unwrap();
        assert!(enqueue("front-first", "front", "Camera"));
        assert!(!enqueue("front-repeat", "front", "Changed title"));
        assert!(enqueue("rear-first", "rear", "Camera"));
        for (_, created) in &mut entry.http_videos.lock().unwrap().cooldowns {
            *created -= grant.cooldown();
        }
        assert!(
            !enqueue("front-pending", "front", "Another title"),
            "a pending camera stays reserved after its cooldown expires"
        );
        assert_eq!(entry.http_videos.lock().unwrap().rate.count, 2);
    }
    let requests = host.take_http_video_requests();
    assert_eq!(requests.len(), 2);
    for (request, camera) in requests.iter().zip(["front", "rear"]) {
        assert!(matches!(
            request.camera_id,
            Some(MediaCameraId::HttpPreview(ref source))
                if source == &format!("https://video.test/base/api/{camera}")
        ));
    }
    {
        let _authority = entry.authority.lock().unwrap();
        assert!(enqueue("front-next", "front", "Camera"));
    }
    assert_eq!(host.take_http_video_requests().len(), 1);
    {
        let _authority = entry.authority.lock().unwrap();
        assert!(
            !enqueue("front-too-soon", "front", "Renamed camera"),
            "draining the queue does not bypass the camera cooldown"
        );
    }
    host.shutdown().await;
}

#[tokio::test]
async fn http_live_requires_explicit_preview_policy_and_configured_origin() {
    for worker in [
        configured_spec("configuration_live"),
        video_spec("configuration_live"),
        live_spec("configuration_live_bad_url"),
    ] {
        let host = PluginHost::default();
        host.start(worker).await.unwrap();
        wait_for(&host, |snapshot| snapshot.state == WorkerState::Failed).await;
        assert!(host.take_http_video_requests().is_empty());
        assert!(!host.has_pending_notifications());
        host.shutdown().await;
    }
}

fn mapped_live_spec(preview: bool) -> WorkerSpec {
    let mut worker = configured_spec("configuration_mapped_live");
    let configuration = worker.configuration.as_mut().unwrap();
    configuration.secrets.insert(
        "destinations".into(),
        json!({
            "front":"https://private-camera.test/front?token=fixture#live",
            "rear":"https://private-camera.test/rear?token=fixture"
        })
        .to_string(),
    );
    let mut manifest: super::super::protocol::PluginManifest = serde_json::from_value(json!({
        "schema_version":1,"plugin_id":TEST_PLUGIN,"version":"1.0.0","host_api":"^1.8",
        "target":"aarch64-apple-darwin","entrypoint":"worker","inventory":[],"signature":null,
        "config_schema":{"type":"object","properties":{"destinations":{"type":"string","writeOnly":true}}},
        "permissions":["plugin_configuration","live_view"],
        "live_view":{"urls_setting":"destinations"}
    })).unwrap();
    if preview {
        manifest
            .live_view
            .as_mut()
            .unwrap()
            .preview_duration_seconds = Some(15);
    }
    worker.live_view = super::super::protocol::LiveViewGrant::from_manifest_configuration(
        &manifest,
        worker.configuration.as_ref(),
    )
    .unwrap();
    worker
}

#[tokio::test]
async fn mapped_live_preview_is_private_notification_independent_and_camera_scoped() {
    let host = PluginHost::default();
    let worker = mapped_live_spec(true);
    assert!(!worker.desktop_notifications);
    let mapping = worker.live_view.clone().unwrap();
    host.start(worker).await.unwrap();
    ready(&host).await;
    action(&host, "echo").await.unwrap();
    let requests = host.take_http_video_requests();
    assert_eq!(
        requests.len(),
        2,
        "equal titles are separate cameras; unknown IDs and repeated camera episodes are omitted"
    );
    for (request, id) in requests.iter().zip(["front", "rear"]) {
        assert!(request.live_preview);
        assert!(matches!(
            request.camera_id,
            Some(MediaCameraId::Explicit(ref camera)) if camera == id
        ));
        assert_eq!(request.url, mapping.resolve(id).unwrap().as_str());
        assert!(request.grant.validate_preview_url(&request.url).is_ok());
        assert!(request.grant.validate_url(&request.url).is_err());
        assert_eq!(request.grant.cooldown(), Duration::from_secs(15));
    }
    assert!(!host.has_pending_notifications());
    let public = serde_json::to_string(&host.snapshots()).unwrap();
    assert!(!public.contains("private-camera") && !public.contains("fixture#live"));
    let entry = host.entry(TEST_PLUGIN).unwrap();
    {
        let _authority = entry.authority.lock().unwrap();
        for (_, instant) in &mut entry.http_videos.lock().unwrap().cooldowns {
            *instant -= Duration::from_secs(15);
        }
        let (url, grant) = mapping.preview("front").unwrap().unwrap();
        let enqueue = |id: &str| {
            entry.queue_http_video(
                &grant,
                id.into(),
                url.to_string(),
                "Private camera".into(),
                MediaAdmission {
                    kind: HttpMediaKind::Video,
                    cooldown_id: Some("front".into()),
                    live_preview: true,
                },
            )
        };
        assert!(
            !enqueue("mapped-1"),
            "expired camera cooldown cannot replay an episode ID"
        );
        assert!(
            enqueue("mapped-next"),
            "a new episode after 15 seconds is admitted"
        );
    }
    host.revoke();
    assert!(host.take_http_video_requests().is_empty());
    assert!(requests.iter().all(|request| !request.lease.is_active()));
    host.shutdown().await;
}

#[tokio::test]
async fn mapped_live_preview_requires_explicit_permission_and_automatic_policy() {
    for worker in [
        configured_spec("configuration_mapped_live"),
        mapped_live_spec(false),
    ] {
        let host = PluginHost::default();
        host.start(worker).await.unwrap();
        wait_for(&host, |snapshot| snapshot.state == WorkerState::Failed).await;
        assert!(host.take_http_video_requests().is_empty());
        assert!(!host.has_pending_notifications());
        host.shutdown().await;
    }
}

fn mqtt_live_spec(mode: &str) -> WorkerSpec {
    let mut worker = configured_spec(mode);
    worker.configuration.as_mut().unwrap().values["mqtt_live_endpoints"] = json!(json!({
        "front":"https://ha.invalid/api/camera_proxy_stream/camera.front",
        "rear":"https://ha.invalid/api/camera_proxy_stream/camera.rear"
    })
    .to_string());
    let manifest: super::super::protocol::PluginManifest = serde_json::from_str(include_str!(
        "../../../scripts/plugins/kerberos-manifest.json"
    ))
    .unwrap();
    worker.live_view = super::super::protocol::LiveViewGrant::from_manifest_configuration(
        &manifest,
        worker.configuration.as_ref(),
    )
    .unwrap();
    worker
}

#[tokio::test]
async fn mqtt_live_is_native_scoped_with_camera_cooldown_and_no_toast_or_public_tokens() {
    let host = PluginHost::default();
    host.start(mqtt_live_spec("configuration_mqtt_live"))
        .await
        .unwrap();
    ready(&host).await;
    action(&host, "echo").await.unwrap();
    let requests = host.take_http_video_requests();
    assert_eq!(
        requests.len(),
        2,
        "rotating token must not bypass per-camera cooldown"
    );
    for (request, camera) in requests.iter().zip(["front", "rear"]) {
        assert!(request.live_preview);
        assert!(
            matches!(request.camera_id, Some(MediaCameraId::Explicit(ref id)) if id == &format!("mqtt:{camera}"))
        );
        assert_eq!(
            request.grant.preview_duration(),
            Some(Duration::from_secs(15))
        );
        assert!(request.grant.validate_preview_url(&request.url).is_ok());
        assert!(request.grant.validate_url(&request.url).is_err());
    }
    assert!(!host.has_pending_notifications());
    let public = serde_json::to_string(&host.snapshots()).unwrap();
    assert!(!public.contains("ha.invalid") && !public.contains("private_fixture"));
    host.revoke();
    assert!(requests.iter().all(|r| !r.lease.is_active()));
    host.shutdown().await;
    for worker in [
        configured_spec("configuration_mqtt_live"),
        mqtt_live_spec("configuration_mqtt_live_bad_url"),
    ] {
        let host = PluginHost::default();
        host.start(worker).await.unwrap();
        wait_for(&host, |s| s.state == WorkerState::Failed).await;
        assert!(host.take_http_video_requests().is_empty());
        assert!(!host.has_pending_notifications());
        let public = serde_json::to_string(&host.snapshots()).unwrap();
        assert!(!public.contains("ha.invalid") && !public.contains("private_fixture"));
        host.shutdown().await;
    }
}

#[tokio::test]
async fn delayed_media_dispatch_drops_old_motion_without_shortening_completed_clip_delivery() {
    let host = PluginHost::default();
    let worker = live_spec("configuration");
    let grant = worker.http_video.clone().unwrap();
    host.start(worker).await.unwrap();
    ready(&host).await;
    let entry = host.entry(TEST_PLUGIN).unwrap();
    {
        let _authority = entry.authority.lock().unwrap();
        for (id, live_preview) in [("stale-motion", true), ("completed-clip", false)] {
            assert!(entry.queue_http_video(
                &grant,
                id.into(),
                if live_preview {
                    "https://video.test/base/api/front?fps=2&height=360".into()
                } else {
                    "https://video.test/base/api/events/front/clip.mp4".into()
                },
                "Front camera".into(),
                MediaAdmission {
                    kind: HttpMediaKind::Video,
                    cooldown_id: Some(id.into()),
                    live_preview,
                },
            ));
        }
        for request in &mut entry.http_videos.lock().unwrap().pending {
            request.created -= Duration::from_secs(4);
        }
    }
    let requests = host.take_http_video_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].id, "completed-clip");
    assert!(!host.has_pending_http_videos());
    assert!(!host.has_pending_notifications());
    host.shutdown().await;
}

#[tokio::test]
async fn expired_motion_does_not_fill_the_runtime_queue_for_fresh_cameras() {
    let host = PluginHost::default();
    let worker = live_spec("configuration");
    let grant = worker.http_video.clone().unwrap();
    host.start(worker).await.unwrap();
    ready(&host).await;
    let entry = host.entry(TEST_PLUGIN).unwrap();
    {
        let _authority = entry.authority.lock().unwrap();
        let enqueue = |index| {
            entry.queue_http_video(
                &grant,
                format!("motion-{index}"),
                format!("https://video.test/base/api/camera-{index}?fps=2&height=360"),
                "Camera".into(),
                MediaAdmission {
                    kind: HttpMediaKind::Video,
                    cooldown_id: None,
                    live_preview: true,
                },
            )
        };
        for index in 0..HTTP_VIDEO_QUEUE_CAPACITY {
            assert!(enqueue(index));
        }
        assert!(!enqueue(HTTP_VIDEO_QUEUE_CAPACITY));
        for request in &mut entry.http_videos.lock().unwrap().pending {
            request.created -= LIVE_PREVIEW_DELIVERY_TTL;
        }
        assert!(enqueue(HTTP_VIDEO_QUEUE_CAPACITY));
    }
    let requests = host.take_http_video_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].id,
        format!("motion-{HTTP_VIDEO_QUEUE_CAPACITY}")
    );
    assert!(!host.has_pending_notifications());
    host.shutdown().await;
}
