use super::*;
use serde_json::json;
use std::io::{self, BufReader, Cursor};
use std::time::Duration;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    token: String,
}

#[test]
fn framing_is_bounded_and_rejects_truncated_or_unknown_commands() {
    let mut reader = BufReader::with_capacity(3, Cursor::new(b"{\"type\":\"shutdown\"}\r\n"));
    assert!(matches!(
        read_frame::<_, Configuration>(&mut reader).unwrap(),
        Some(HostFrame::Shutdown {})
    ));
    assert!(read_frame::<_, Configuration>(&mut reader)
        .unwrap()
        .is_none());
    for bytes in [
        b"{\"type\":\"shutdown\"}".to_vec(),
        b"\n".to_vec(),
        b"{\"type\":\"shutdown\",\"extra\":1}\n".to_vec(),
        b"{\"type\":\"action\"}\n".to_vec(),
        vec![b' '; MAX_FRAME_BYTES + 1],
    ] {
        assert!(read_frame::<_, Configuration>(&mut Cursor::new(bytes)).is_err());
    }
    let mut exact = vec![b' '; MAX_FRAME_BYTES - b"{\"type\":\"shutdown\"}\n".len()];
    exact.extend_from_slice(b"{\"type\":\"shutdown\"}\n");
    assert!(read_frame::<_, Configuration>(&mut Cursor::new(exact)).is_ok());
}

#[test]
fn configuration_deserialization_remains_owned_by_each_worker() {
    let valid = b"{\"type\":\"configuration\",\"configuration\":{\"token\":\"fixture-secret\"}}\n";
    let frame = read_frame::<_, Configuration>(&mut Cursor::new(valid)).unwrap();
    match frame {
        Some(HostFrame::Configuration { configuration }) => {
            assert_eq!(configuration.token, "fixture-secret")
        }
        _ => panic!("expected typed configuration"),
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OtherConfiguration {
        enabled: bool,
    }
    assert!(matches!(
        read_frame::<_, OtherConfiguration>(&mut Cursor::new(valid)),
        Err("invalid host frame")
    ));
    let other = b"{\"type\":\"configuration\",\"configuration\":{\"enabled\":true}}\n";
    match read_frame::<_, OtherConfiguration>(&mut Cursor::new(other)).unwrap() {
        Some(HostFrame::Configuration { configuration }) => assert!(configuration.enabled),
        _ => panic!("expected worker-specific configuration"),
    }
    let unknown = b"{\"type\":\"configuration\",\"configuration\":{\"token\":\"fixture-secret\",\"extra\":true}}\n";
    assert!(matches!(
        read_frame::<_, Configuration>(&mut Cursor::new(unknown)),
        Err("invalid host frame")
    ));
}

#[test]
fn action_and_cancel_are_strict_bounded_and_stamp_original_receipt() {
    let before = std::time::Instant::now();
    let valid = json!({"type":"action","request_id":"request-1","action_id":"ha-action-0","params":{},"deadline_ms":5000});
    let decode = |value: Value| {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        read_frame::<_, Configuration>(&mut Cursor::new(bytes))
    };
    match decode(valid.clone()).unwrap() {
        Some(HostFrame::Action {
            request_id,
            action_id,
            params,
            deadline_ms,
            received_at,
        }) => {
            assert_eq!(request_id, "request-1");
            assert_eq!(action_id, "ha-action-0");
            assert_eq!(params, json!({}));
            assert_eq!(deadline_ms, 5000);
            assert!(received_at >= before);
            assert!(received_at <= std::time::Instant::now());
        }
        _ => panic!("expected action"),
    }
    for (field, value) in [
        ("request_id", json!("../bad")),
        ("request_id", json!("a".repeat(129))),
        ("action_id", json!("")),
        ("params", json!(null)),
        ("params", json!([])),
        ("params", json!({"secret":"a".repeat(4096)})),
        ("params", json!({"nested":[[[[[[[[[]]]]]]]]]})),
        ("params", json!({"nodes":vec![0; 513]})),
        ("deadline_ms", json!(0)),
        ("deadline_ms", json!(30001)),
        ("deadline_ms", json!(-1)),
        ("deadline_ms", json!(0.5)),
        ("received_at", json!(0)),
        ("extra", json!(true)),
    ] {
        let mut frame = valid.clone();
        frame[field] = value;
        assert!(decode(frame).is_err(), "must reject {field}");
    }
    for deadline in [1, 30000] {
        let mut frame = valid.clone();
        frame["deadline_ms"] = json!(deadline);
        assert!(decode(frame).is_ok());
    }
    assert!(
        matches!(decode(json!({"type":"cancel","request_id":"request-1"})).unwrap(), Some(HostFrame::Cancel { request_id }) if request_id == "request-1")
    );
    for frame in [
        json!({"type":"cancel","request_id":""}),
        json!({"type":"cancel","request_id":"request-1","extra":true}),
        json!({"type":"cancel"}),
    ] {
        assert!(decode(frame).is_err());
    }
}

#[test]
fn action_pressure_cannot_hide_shutdown_or_eof() {
    let action = b"{\"type\":\"action\",\"request_id\":\"1\",\"action_id\":\"ha-action-0\",\"params\":{},\"deadline_ms\":5000}\n";
    for (suffix, expected) in [
        (
            b"{\"type\":\"shutdown\"}\n".as_slice(),
            StopReason::Shutdown,
        ),
        (b"".as_slice(), StopReason::Eof),
    ] {
        let mut bytes = action.repeat(QUEUE_CAPACITY);
        bytes.extend_from_slice(suffix);
        let (sender, incoming) = mpsc::channel::<HostFrame<Configuration>>(QUEUE_CAPACITY);
        let (stop, stopped) = watch::channel(StopReason::Running);
        read_frames(Cursor::new(bytes), sender, stop);
        assert!(*stopped.borrow() == expected);
        assert_eq!(incoming.len(), QUEUE_CAPACITY);
    }
}

#[test]
fn reader_shutdown_bypasses_a_full_bounded_command_queue() {
    let hello = b"{\"type\":\"hello\",\"protocol_version\":1,\"host_api_version\":\"1.3.0\",\"plugin_id\":\"any.worker\"}\n";
    let mut input = hello.repeat(QUEUE_CAPACITY);
    input.extend_from_slice(b"{\"type\":\"shutdown\"}\n");
    let (sender, incoming) = mpsc::channel::<HostFrame<Configuration>>(QUEUE_CAPACITY);
    let (stop, stopped) = watch::channel(StopReason::Running);
    read_frames(Cursor::new(input), sender, stop);
    assert!(matches!(*stopped.borrow(), StopReason::Shutdown));
    assert_eq!(incoming.len(), QUEUE_CAPACITY);

    let (sender, incoming) = mpsc::channel::<HostFrame<Configuration>>(QUEUE_CAPACITY);
    let (stop, stopped) = watch::channel(StopReason::Running);
    read_frames(Cursor::new(hello.repeat(QUEUE_CAPACITY + 1)), sender, stop);
    assert!(matches!(*stopped.borrow(), StopReason::Invalid));
    assert_eq!(incoming.len(), QUEUE_CAPACITY);
}

#[test]
fn reader_distinguishes_eof_and_invalid_input_without_secret_diagnostics() {
    for (input, expected) in [
        (b"".as_slice(), StopReason::Eof),
        (b"{\"type\":\"shutdown\"}".as_slice(), StopReason::Invalid),
        (
            b"{\"type\":\"shutdown\",\"token\":\"fixture-secret\"}\n".as_slice(),
            StopReason::Invalid,
        ),
    ] {
        let (sender, mut incoming) = mpsc::channel::<HostFrame<Configuration>>(QUEUE_CAPACITY);
        let (stop, stopped) = watch::channel(StopReason::Running);
        read_frames(Cursor::new(input), sender, stop);
        assert!(*stopped.borrow() == expected);
        assert!(matches!(
            incoming.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }
}

#[test]
fn cloned_outputs_share_the_same_bounded_queue_and_encoding_limit() {
    let (sender, mut outgoing) = mpsc::channel(QUEUE_CAPACITY);
    let output = Output { sender };
    let clone = output.clone();
    let frame = json!({"type":"contributions","contributions":[]});
    assert!(output.try_send(frame.clone()).unwrap());
    assert!(clone.try_send(frame.clone()).unwrap());
    assert!(!output.try_send(frame.clone()).unwrap());
    let queued = outgoing.try_recv().unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&queued.bytes).unwrap(),
        frame
    );
    assert!(clone.try_send(frame.clone()).unwrap());
    let exact = encode_frame(Value::String("x".repeat(MAX_FRAME_BYTES - 3))).unwrap();
    assert_eq!(exact.len(), MAX_FRAME_BYTES);
    assert_eq!(exact.last(), Some(&b'\n'));
    assert!(encode_frame(Value::String("x".repeat(MAX_FRAME_BYTES - 2))).is_err());
    drop(outgoing);
    assert!(clone.try_send(frame).is_err());
}

struct PendingFlush {
    bytes: Vec<u8>,
    release: std::sync::mpsc::SyncSender<io::Result<()>>,
}

struct ControlledWriter {
    bytes: Vec<u8>,
    flushed: mpsc::Sender<PendingFlush>,
}

impl Write for ControlledWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        let (release, released) = std::sync::mpsc::sync_channel(1);
        self.flushed
            .blocking_send(PendingFlush {
                bytes: std::mem::take(&mut self.bytes),
                release,
            })
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        released
            .recv()
            .unwrap_or_else(|_| Err(io::ErrorKind::BrokenPipe.into()))
    }
}

#[tokio::test(flavor = "current_thread")]
async fn send_acknowledges_only_a_successful_completed_flush() {
    let (flushed, mut pending) = mpsc::channel(2);
    let output = Output::with_writer(ControlledWriter {
        bytes: Vec::new(),
        flushed,
    });
    let sender = output.clone();
    let send = tokio::spawn(async move {
        sender
            .send(json!({"type":"configuration_ready","revision":"fixture"}))
            .await
    });
    let flushing = tokio::time::timeout(Duration::from_secs(2), pending.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&flushing.bytes).unwrap(),
        json!({"type":"configuration_ready","revision":"fixture"})
    );
    assert!(
        !send.is_finished(),
        "accepted bytes do not imply flushed acknowledgement"
    );
    flushing.release.send(Ok(())).unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(2), send)
        .await
        .unwrap()
        .unwrap()
        .is_ok());

    let sender = output.clone();
    let send = tokio::spawn(async move { sender.send(json!({"type":"ready"})).await });
    let flushing = tokio::time::timeout(Duration::from_secs(2), pending.recv())
        .await
        .unwrap()
        .unwrap();
    flushing
        .release
        .send(Err(io::Error::other("fixture-secret")))
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), send)
            .await
            .unwrap()
            .unwrap(),
        Err("host output closed")
    );
    assert_eq!(
        output.send(json!({"type":"ready"})).await,
        Err("host output closed")
    );
}

#[test]
fn write_failure_is_reported_without_the_underlying_error_contents() {
    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("fixture-secret"))
        }
        fn flush(&mut self) -> io::Result<()> {
            panic!("flush must not run after a failed write")
        }
    }
    let (sender, outgoing) = mpsc::channel(QUEUE_CAPACITY);
    let (written, result) = oneshot::channel();
    sender
        .try_send(Outbound {
            bytes: b"{}\n".to_vec(),
            written,
            expires_at: None,
        })
        .ok()
        .unwrap();
    write_frames(FailingWriter, outgoing);
    assert_eq!(result.blocking_recv().unwrap(), Err("host output closed"));
}

#[tokio::test(flavor = "current_thread")]
async fn expired_preview_behind_blocked_output_is_discarded_without_expiring_other_traffic() {
    for kind in ["live_view", "mqtt_live", "http_live"] {
        let (flushed, mut pending) = mpsc::channel(2);
        let output = Output::with_writer(ControlledWriter {
            bytes: Vec::new(),
            flushed,
        });
        // Block a prior frame's flush, leaving both following frames in the
        // bounded queue. An already-expired deadline avoids wall-clock sleeps.
        assert!(output.try_send(json!({"type":"contributions"})).unwrap());
        let blocked = tokio::time::timeout(Duration::from_secs(2), pending.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(output
            .try_send_before(
                json!({"type":kind,"id":"expired","url":"https://fixture.invalid/?token=private"}),
                Instant::now(),
            )
            .unwrap());
        let clip = json!({"type":"http_video","id":"clip"});
        assert!(output.try_send(clip.clone()).unwrap());
        assert!(!output.try_send(json!({"type":"notification"})).unwrap());
        blocked.release.send(Ok(())).unwrap();
        let next = tokio::time::timeout(Duration::from_secs(2), pending.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&next.bytes).unwrap(), clip);
        next.release.send(Ok(())).unwrap();
        let fresh = json!({"type":kind,"id":"fresh"});
        assert!(output
            .try_send_before(fresh.clone(), Instant::now() + Duration::from_secs(60))
            .unwrap());
        let next = tokio::time::timeout(Duration::from_secs(2), pending.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&next.bytes).unwrap(), fresh);
        next.release.send(Ok(())).unwrap();
    }
}

#[test]
fn reserved_numeric_object_keys_are_rejected_before_frame_values_can_be_coerced() {
    for key in [
        "$serde_json::private::Number",
        r"\u0024serde_json::private::Number",
        r"$serde_json::private::\u004eumber",
    ] {
        let frame = format!(
            r#"{{"type":"action","request_id":"numeric-1","action_id":"ha-number-0-set","params":{{"input_revision":"r1","value_scaled":{{"{key}":"1"}}}},"deadline_ms":5000}}"#
        ) + "\n";
        assert!(read_frame::<_, Configuration>(&mut Cursor::new(frame.as_bytes())).is_err());
    }
    for bytes in [
        br#"{"value":"$serde_json::private::Number"}"#.as_slice(),
        br#"{"escaped":"\"$serde_json::private::Number\":123","value":1.25e-1}"#.as_slice(),
        br#"{"$serde_json::private::Numberx":1}"#.as_slice(),
    ] {
        assert!(reject_reserved_number_keys(bytes).is_ok());
    }
    let frame = br#"{"type":"action","request_id":"numeric-1","action_id":"ha-number-0-set","params":{"input_revision":"$serde_json::private::Number","value_scaled":1},"deadline_ms":5000}
"#;
    assert!(read_frame::<_, Configuration>(&mut Cursor::new(frame)).is_ok());
    assert!(reject_reserved_number_keys(br#"{"unterminated\"#).is_err());
}
